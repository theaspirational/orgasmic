//! Best-effort tracing sinks so a dead stdout/stderr pipe never kills the
//! daemon or fails an HTTP request (TASK-FZF2D).
//!
//! Size-triggered rotation: TASK-ZBYH3. Stdout mirror: explicit
//! `--no-log-mirror` / `ORGASMIC_LOG_MIRROR=off` by construction in service
//! definitions orgasmic writes, with `is_terminal()` as the fallback for
//! supervisors it did not write (TASK-ZBYH3.1, TASK-G64ZH). Reopen after a
//! failed durable open is backoff-bounded (TASK-G64ZH).
//!
//! orgasmic:TASK-FZF2D,TASK-ZBYH3,TASK-ZBYH3.1,TASK-G64ZH

use std::fs::{File, OpenOptions};
use std::io::{self, IsTerminal, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use tracing_subscriber::fmt::MakeWriter;
use tracing_subscriber::EnvFilter;

/// Initial wait before retrying a failed durable reopen (TASK-G64ZH F1).
const REOPEN_BACKOFF_INITIAL: Duration = Duration::from_secs(1);
/// Cap for the reopen backoff (TASK-G64ZH F1).
const REOPEN_BACKOFF_CAP: Duration = Duration::from_secs(60);

/// Default durable daemon log under `$ORGASMIC_HOME/logs/`.
pub const DAEMON_OUT_LOG: &str = "daemon.out.log";

/// Default size threshold before the durable log rolls (10 MiB).
pub const DEFAULT_LOG_MAX_BYTES: u64 = 10 * 1024 * 1024;

/// Default number of rolled durable logs to keep (`daemon.out.log.1` .. `.N`).
pub const DEFAULT_LOG_KEEP: u32 = 3;

/// Upper bound for `log.keep`. Each roll renames under the sink mutex; an
/// unclamped typo would turn every rotation into tens of thousands of syscalls.
/// orgasmic:TASK-ZBYH3.1
pub const MAX_LOG_KEEP: u32 = 32;

static DROPPED_LOG_WRITES: AtomicU64 = AtomicU64::new(0);

/// Process-wide count of sink write failures (BrokenPipe/EPIPE and other I/O
/// errors). Cheap to read; never consulted on the request success path.
pub fn dropped_log_writes() -> u64 {
    DROPPED_LOG_WRITES.load(Ordering::Relaxed)
}

fn record_drop() {
    DROPPED_LOG_WRITES.fetch_add(1, Ordering::Relaxed);
}

/// Ignore SIGPIPE so writes to a closed pipe return EPIPE instead of terminating
/// the process. No-op on non-Unix targets.
pub fn ignore_sigpipe() {
    #[cfg(unix)]
    unsafe {
        libc::signal(libc::SIGPIPE, libc::SIG_IGN);
    }
}

/// Where best-effort mirrored log lines go in addition to the durable file.
#[derive(Debug)]
pub enum LogMirror {
    /// Mirror to process stdout (production default for interactive `serve`).
    Stdout,
    /// Mirror to an explicit writer (tests inject a closed pipe here).
    Writer(File),
    /// Durable sink only.
    None,
}

/// Size-triggered roll settings for the durable sink.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LogRotation {
    /// Roll when bytes written since open exceed this. `0` disables rotation.
    pub max_bytes: u64,
    /// Keep this many rolled files (`path.1` .. `path.N`). `0` disables keep
    /// (current file is removed on roll).
    pub keep: u32,
}

impl Default for LogRotation {
    fn default() -> Self {
        Self {
            max_bytes: DEFAULT_LOG_MAX_BYTES,
            keep: DEFAULT_LOG_KEEP,
        }
    }
}

/// Resolve the mirror request before install.
///
/// Precedence (TASK-G64ZH F2):
/// 1. `no_log_mirror` CLI flag (`serve --no-log-mirror`) → [`LogMirror::None`]
/// 2. else `ORGASMIC_LOG_MIRROR=off` (also `0` / `false`, case-insensitive) →
///    [`LogMirror::None`]
/// 3. else [`LogMirror::Stdout`], and [`resolve_mirror`] applies the
///    `is_terminal()` fallback when a durable sink is present
///
/// Explicit off always wins over a tty. An old LaunchAgent plist without the
/// flag still suppresses under launchd because stdout is not a terminal.
pub fn requested_log_mirror(no_log_mirror: bool) -> LogMirror {
    if no_log_mirror || env_log_mirror_off() {
        LogMirror::None
    } else {
        LogMirror::Stdout
    }
}

fn env_log_mirror_off() -> bool {
    match std::env::var("ORGASMIC_LOG_MIRROR") {
        Ok(value) => {
            let value = value.trim();
            value.eq_ignore_ascii_case("off") || value == "0" || value.eq_ignore_ascii_case("false")
        }
        Err(_) => false,
    }
}

/// Install the global tracing subscriber once. Later calls are no-ops.
///
/// When `durable_log` is set, logs append to that path (created if needed, never
/// truncated). `mirror` is best-effort: failures are counted and swallowed so
/// they cannot propagate into request handling. When a durable sink is present,
/// a [`LogMirror::Stdout`] mirror is kept only if stdout is a terminal — unless
/// the caller already passed [`LogMirror::None`] (explicit `--no-log-mirror` /
/// `ORGASMIC_LOG_MIRROR=off`). Without a durable sink (non-`serve` CLI), the
/// stdout mirror is left alone so piped/redirected CLI tracing still works.
///
/// Returns `true` when this call installed the subscriber.
pub fn init_tracing_to(
    default_filter: &str,
    durable_log: Option<&Path>,
    mirror: LogMirror,
    rotation: LogRotation,
) -> bool {
    ignore_sigpipe();
    let filter = EnvFilter::try_from_default_env()
        .or_else(|_| EnvFilter::try_new(default_filter))
        .unwrap_or_else(|_| EnvFilter::new("info"));
    let durable =
        durable_log.and_then(|path| open_durable_log(path).map(|file| (path.to_path_buf(), file)));
    let sink = BestEffortMakeWriter::new(durable, mirror, rotation);
    tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_writer(sink)
        .try_init()
        .is_ok()
}

/// Stdout-only best-effort tracing (non-`serve` CLI commands).
pub fn init_tracing(default_filter: &str) -> bool {
    init_tracing_to(
        default_filter,
        None,
        LogMirror::Stdout,
        LogRotation::default(),
    )
}

fn open_durable_log(path: &Path) -> Option<File> {
    if let Some(parent) = path.parent() {
        if let Err(err) = std::fs::create_dir_all(parent) {
            let _ = writeln!(
                io::stderr(),
                "orgasmic: failed to create log dir {}: {err}",
                parent.display()
            );
            return None;
        }
    }
    match OpenOptions::new().create(true).append(true).open(path) {
        Ok(file) => Some(file),
        Err(err) => {
            let _ = writeln!(
                io::stderr(),
                "orgasmic: failed to open log file {}: {err}",
                path.display()
            );
            None
        }
    }
}

/// True when `path` and the open `file` share device+inode (Unix). Always
/// false on non-Unix. Used only for [`LogMirror::Writer`] test injectors.
#[cfg(unix)]
fn same_file_as_path(file: &File, path: &Path) -> bool {
    use std::os::unix::fs::MetadataExt;
    let Ok(file_meta) = file.metadata() else {
        return false;
    };
    let Ok(path_meta) = std::fs::metadata(path) else {
        return false;
    };
    file_meta.dev() == path_meta.dev() && file_meta.ino() == path_meta.ino()
}

#[cfg(not(unix))]
fn same_file_as_path(_file: &File, _path: &Path) -> bool {
    false
}

/// Resolve a requested mirror when a durable sink is present.
///
/// Production (`LogMirror::Stdout`): keep the mirror only when
/// `stdout_is_terminal` is true. Service managers orgasmic writes pass
/// `--no-log-mirror` so the caller supplies [`LogMirror::None`] already;
/// `is_terminal()` remains the fallback for supervisors orgasmic did not
/// write (and for already-installed plists that lack the flag). When
/// `durable_path` is `None` (non-`serve` CLI via [`init_tracing`]), the mirror
/// is returned unchanged so piped/redirected CLI tracing still works —
/// orgasmic:TASK-ZBYH3.1,TASK-G64ZH.
fn resolve_mirror(
    mirror: LogMirror,
    durable_path: Option<&Path>,
    stdout_is_terminal: bool,
) -> LogMirror {
    let Some(path) = durable_path else {
        return mirror;
    };
    match mirror {
        LogMirror::Stdout => {
            if stdout_is_terminal {
                LogMirror::Stdout
            } else {
                LogMirror::None
            }
        }
        LogMirror::Writer(file) => {
            if same_file_as_path(&file, path) {
                LogMirror::None
            } else {
                LogMirror::Writer(file)
            }
        }
        LogMirror::None => LogMirror::None,
    }
}

#[derive(Clone)]
struct BestEffortMakeWriter {
    inner: Arc<Mutex<SinkState>>,
}

struct SinkState {
    durable: Option<File>,
    durable_path: Option<PathBuf>,
    bytes_written: u64,
    rotation: LogRotation,
    mirror: MirrorState,
    /// Earliest Instant at which a failed durable reopen may be retried.
    /// orgasmic:TASK-G64ZH
    next_reopen_attempt: Instant,
    /// Current reopen backoff; doubles after each failed attempt up to
    /// [`REOPEN_BACKOFF_CAP`].
    reopen_backoff: Duration,
    /// Test-only: simulate EMFILE/ENOSPC on durable reopen (TASK-ZBYH3.1 F3).
    #[cfg(test)]
    reject_durable_open: bool,
    /// Test-only: count reopen attempts (TASK-G64ZH F1 bound).
    #[cfg(test)]
    open_attempts: u64,
    /// Test-only: count `maybe_rotate` entries (TASK-G64ZH F1).
    #[cfg(test)]
    rotate_attempts: u64,
    /// Test-only: lines that never landed in the durable file (TASK-G64ZH F1).
    #[cfg(test)]
    lines_dropped: u64,
}

enum MirrorState {
    Stdout,
    File(File),
    None,
}

impl BestEffortMakeWriter {
    fn new(durable: Option<(PathBuf, File)>, mirror: LogMirror, rotation: LogRotation) -> Self {
        Self::new_with_terminal_gate(durable, mirror, rotation, io::stdout().is_terminal())
    }

    /// Like [`Self::new`], but with an injectable terminal predicate so both
    /// mirror-gate branches can be asserted (TASK-G64ZH F3).
    fn new_with_terminal_gate(
        durable: Option<(PathBuf, File)>,
        mirror: LogMirror,
        rotation: LogRotation,
        stdout_is_terminal: bool,
    ) -> Self {
        let durable_path = durable.as_ref().map(|(path, _)| path.clone());
        let mirror = resolve_mirror(mirror, durable_path.as_deref(), stdout_is_terminal);
        let mirror = match mirror {
            LogMirror::Stdout => MirrorState::Stdout,
            LogMirror::Writer(file) => MirrorState::File(file),
            LogMirror::None => MirrorState::None,
        };
        let (durable_path, durable, bytes_written) = match durable {
            Some((path, file)) => {
                let len = file.metadata().map(|m| m.len()).unwrap_or(0);
                (Some(path), Some(file), len)
            }
            None => (None, None, 0),
        };
        Self {
            inner: Arc::new(Mutex::new(SinkState {
                durable,
                durable_path,
                bytes_written,
                rotation,
                mirror,
                next_reopen_attempt: Instant::now(),
                reopen_backoff: REOPEN_BACKOFF_INITIAL,
                #[cfg(test)]
                reject_durable_open: false,
                #[cfg(test)]
                open_attempts: 0,
                #[cfg(test)]
                rotate_attempts: 0,
                #[cfg(test)]
                lines_dropped: 0,
            })),
        }
    }

    /// Test helper: durable path known but handle missing — the post-roll
    /// reopen-failed state (TASK-ZBYH3.1 F3).
    #[cfg(test)]
    fn with_missing_durable(path: PathBuf, rotation: LogRotation) -> Self {
        Self {
            inner: Arc::new(Mutex::new(SinkState {
                durable: None,
                durable_path: Some(path),
                bytes_written: 0,
                rotation,
                mirror: MirrorState::None,
                next_reopen_attempt: Instant::now(),
                reopen_backoff: REOPEN_BACKOFF_INITIAL,
                reject_durable_open: true,
                open_attempts: 0,
                rotate_attempts: 0,
                lines_dropped: 0,
            })),
        }
    }
}

impl SinkState {
    fn try_open_durable(&mut self) -> bool {
        let Some(path) = self.durable_path.clone() else {
            return false;
        };
        #[cfg(test)]
        {
            self.open_attempts = self.open_attempts.saturating_add(1);
        }
        #[cfg(test)]
        if self.reject_durable_open {
            let _ = writeln!(
                io::stderr(),
                "orgasmic: failed to open log file {}: injected open failure",
                path.display()
            );
            return false;
        }
        match open_durable_log(&path) {
            Some(file) => {
                self.bytes_written = file.metadata().map(|m| m.len()).unwrap_or(0);
                self.durable = Some(file);
                self.reopen_backoff = REOPEN_BACKOFF_INITIAL;
                self.next_reopen_attempt = Instant::now();
                true
            }
            None => false,
        }
    }

    fn schedule_reopen_retry(&mut self) {
        self.next_reopen_attempt = Instant::now() + self.reopen_backoff;
        self.reopen_backoff = self
            .reopen_backoff
            .saturating_mul(2)
            .min(REOPEN_BACKOFF_CAP);
    }

    /// Attempt a durable reopen only when the backoff window allows it.
    /// Returns true when a handle is present afterwards.
    fn maybe_reopen_durable(&mut self) -> bool {
        if self.durable.is_some() {
            return true;
        }
        if self.durable_path.is_none() {
            return false;
        }
        if Instant::now() < self.next_reopen_attempt {
            return false;
        }
        if self.try_open_durable() {
            true
        } else {
            self.schedule_reopen_retry();
            false
        }
    }
}

impl<'a> MakeWriter<'a> for BestEffortMakeWriter {
    type Writer = BestEffortWriter;

    fn make_writer(&'a self) -> Self::Writer {
        BestEffortWriter {
            inner: Arc::clone(&self.inner),
        }
    }
}

struct BestEffortWriter {
    inner: Arc<Mutex<SinkState>>,
}

impl Write for BestEffortWriter {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        // Always report success to tracing so a dead sink cannot fail a request.
        write_all_best_effort(&self.inner, buf);
        Ok(buf.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        flush_best_effort(&self.inner);
        Ok(())
    }
}

fn write_all_best_effort(inner: &Mutex<SinkState>, buf: &[u8]) {
    let mut state = match inner.lock() {
        Ok(guard) => guard,
        Err(poisoned) => poisoned.into_inner(),
    };
    // Reopen owns the retry behind a backoff; maybe_rotate does not run while
    // the handle is missing (TASK-G64ZH F1).
    let _ = state.maybe_reopen_durable();
    if let Some(file) = state.durable.as_mut() {
        if file.write_all(buf).is_err() {
            record_drop();
        } else {
            state.bytes_written = state.bytes_written.saturating_add(buf.len() as u64);
        }
    } else if state.durable_path.is_some() {
        // One drop per line that never landed in the durable file — not one
        // per syscall on the reopen/rotate paths (TASK-G64ZH F1).
        record_drop();
        #[cfg(test)]
        {
            state.lines_dropped = state.lines_dropped.saturating_add(1);
        }
    }
    match &mut state.mirror {
        MirrorState::Stdout => {
            if io::stdout().write_all(buf).is_err() {
                record_drop();
            }
        }
        MirrorState::File(file) => {
            if file.write_all(buf).is_err() {
                record_drop();
            }
        }
        MirrorState::None => {}
    }
    if state.durable.is_some() {
        maybe_rotate(&mut state);
    }
}

fn flush_best_effort(inner: &Mutex<SinkState>) {
    let mut state = match inner.lock() {
        Ok(guard) => guard,
        Err(poisoned) => poisoned.into_inner(),
    };
    if let Some(file) = state.durable.as_mut() {
        if file.flush().is_err() {
            record_drop();
        }
    }
    match &mut state.mirror {
        MirrorState::Stdout => {
            if io::stdout().flush().is_err() {
                record_drop();
            }
        }
        MirrorState::File(file) => {
            if file.flush().is_err() {
                record_drop();
            }
        }
        MirrorState::None => {}
    }
}

/// Roll the durable file when the tracked byte count exceeds the threshold.
/// Failures never propagate — keep writing the current handle and count a drop.
/// Must not be called while `state.durable` is `None` (the write path owns
/// reopen retries via [`SinkState::maybe_reopen_durable`]).
fn maybe_rotate(state: &mut SinkState) {
    #[cfg(test)]
    {
        state.rotate_attempts = state.rotate_attempts.saturating_add(1);
    }
    if state.durable.is_none() {
        return;
    }
    if state.rotation.max_bytes == 0 {
        return;
    }
    if state.bytes_written <= state.rotation.max_bytes {
        return;
    }
    let Some(path) = state.durable_path.clone() else {
        return;
    };
    if let Some(file) = state.durable.as_mut() {
        let _ = file.flush();
    }

    let keep = state.rotation.keep.min(MAX_LOG_KEEP);
    if keep == 0 {
        // No rolled retain: remove the current path and reopen empty.
        state.durable = None;
        if let Err(err) = std::fs::remove_file(&path) {
            if err.kind() != io::ErrorKind::NotFound {
                record_drop();
                if !state.try_open_durable() {
                    state.schedule_reopen_retry();
                }
                return;
            }
        }
    } else {
        // Discover the highest contiguous rolled generation, then rename that
        // range only — stop at the first missing source so a large `keep` does
        // not issue `keep-1` syscalls under the sink mutex (TASK-ZBYH3.1).
        let mut highest = 0u32;
        for i in 1..keep {
            if rolled_path(&path, i).exists() {
                highest = i;
            } else {
                break;
            }
        }
        // Roll .N -> .(N+1) … .1 -> .2, then current -> .1.
        for i in (1..=highest).rev() {
            let from = rolled_path(&path, i);
            let to = rolled_path(&path, i + 1);
            match std::fs::rename(&from, &to) {
                Ok(()) => {}
                Err(err) if err.kind() == io::ErrorKind::NotFound => {}
                Err(err) => {
                    record_drop();
                    let _ = writeln!(
                        io::stderr(),
                        "orgasmic: log rotation rename failed for {}: {err}",
                        from.display()
                    );
                }
            }
        }
        // Drop the handle before renaming so the path is free; the open fd
        // would otherwise keep writing the renamed inode after we reopen.
        state.durable = None;
        if let Err(err) = std::fs::rename(&path, rolled_path(&path, 1)) {
            record_drop();
            let _ = writeln!(
                io::stderr(),
                "orgasmic: log rotation rename failed for {}: {err}",
                path.display()
            );
            // Keep writing the current file when reopen succeeds; otherwise the
            // write-path backoff owns the retry (TASK-G64ZH F1).
            if !state.try_open_durable() {
                state.schedule_reopen_retry();
            }
            return;
        }
    }

    if state.try_open_durable() {
        state.bytes_written = 0;
    } else {
        // Write-path backoff owns the retry; do not park past the rotation
        // threshold so maybe_rotate re-enters on every subsequent line.
        state.schedule_reopen_retry();
    }
}

fn rolled_path(path: &Path, n: u32) -> PathBuf {
    let mut os = path.as_os_str().to_owned();
    os.push(format!(".{n}"));
    PathBuf::from(os)
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use std::io::Read;
    use std::os::unix::io::FromRawFd;

    fn closed_pipe_writer() -> File {
        let mut fds = [0; 2];
        assert_eq!(unsafe { libc::pipe(fds.as_mut_ptr()) }, 0);
        unsafe {
            libc::close(fds[0]);
            File::from_raw_fd(fds[1])
        }
    }

    #[test]
    fn closed_mirror_increments_drop_counter_and_keeps_durable_writable() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("daemon.out.log");
        let durable = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)
            .unwrap();
        let mirror = closed_pipe_writer();
        let sink = BestEffortMakeWriter::new(
            Some((path.clone(), durable)),
            LogMirror::Writer(mirror),
            LogRotation::default(),
        );
        let before = dropped_log_writes();
        {
            let mut writer = sink.make_writer();
            writer.write_all(b"durable-line\n").unwrap();
            writer.flush().unwrap();
        }
        let after = dropped_log_writes();
        assert!(
            after > before,
            "expected dropped_log_writes to increase ({before} -> {after})"
        );
        let mut contents = String::new();
        File::open(&path)
            .unwrap()
            .read_to_string(&mut contents)
            .unwrap();
        assert!(
            contents.contains("durable-line"),
            "durable sink missing line: {contents:?}"
        );
    }

    #[test]
    fn ignore_sigpipe_is_callable() {
        ignore_sigpipe();
        // Second call must remain safe (idempotent install).
        ignore_sigpipe();
    }

    /// orgasmic:TASK-ZBYH3 — launchd's StandardOutPath is the durable file, so
    /// a Stdout/Writer mirror that resolves to the same inode must not double
    /// the line. Pre-fix this counted 2; the operator measured ~half of a
    /// 13 MB log as exact duplication.
    #[test]
    fn same_inode_mirror_writes_each_line_once() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("daemon.out.log");
        let durable = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)
            .unwrap();
        let mirror = OpenOptions::new().append(true).open(&path).unwrap();
        let sink = BestEffortMakeWriter::new(
            Some((path.clone(), durable)),
            LogMirror::Writer(mirror),
            LogRotation::default(),
        );
        {
            let mut writer = sink.make_writer();
            writer.write_all(b"unique-marker-zbyh3\n").unwrap();
            writer.flush().unwrap();
        }
        let contents = std::fs::read_to_string(&path).unwrap();
        let count = contents.matches("unique-marker-zbyh3").count();
        assert_eq!(
            count, 1,
            "line logged once must appear once in durable file when mirror \
             resolves to the same inode (got {count}): {contents:?}"
        );
    }

    /// Process-wide stdout redirect must not race other tests.
    static STDOUT_REDIRECT_LOCK: Mutex<()> = Mutex::new(());

    /// orgasmic:TASK-ZBYH3.1 — production path is `LogMirror::Stdout`, not
    /// `Writer`. Under a launchd-shaped redirect (stdout → any file, here a
    /// distinct `daemon.stdout.log`), one logged line must appear once across
    /// both files combined. Pre-fix (inode gate + ruling A) counted 2.
    #[test]
    fn launchd_shaped_stdout_mirror_writes_each_line_once_across_both_files() {
        use std::os::unix::io::AsRawFd;

        let _lock = STDOUT_REDIRECT_LOCK
            .lock()
            .unwrap_or_else(|p| p.into_inner());
        let dir = tempfile::tempdir().unwrap();
        let durable_path = dir.path().join("daemon.out.log");
        let stdout_path = dir.path().join("daemon.stdout.log");
        let stdout_file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&stdout_path)
            .unwrap();
        let durable = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&durable_path)
            .unwrap();

        let saved_fd = unsafe { libc::dup(libc::STDOUT_FILENO) };
        assert!(saved_fd >= 0, "dup stdout");
        assert!(
            unsafe { libc::dup2(stdout_file.as_raw_fd(), libc::STDOUT_FILENO) } >= 0,
            "dup2 stdout -> daemon.stdout.log"
        );
        // fd 1 now owns a dup of the file; drop our handle so later reads see
        // a quiescent file (and restore happens in the guard below).
        drop(stdout_file);

        struct RestoreStdout(i32);
        impl Drop for RestoreStdout {
            fn drop(&mut self) {
                unsafe {
                    libc::dup2(self.0, libc::STDOUT_FILENO);
                    libc::close(self.0);
                }
            }
        }
        let _restore = RestoreStdout(saved_fd);

        let sink = BestEffortMakeWriter::new(
            Some((durable_path.clone(), durable)),
            LogMirror::Stdout,
            LogRotation::default(),
        );
        {
            let mut writer = sink.make_writer();
            writer.write_all(b"unique-marker-zbyh3-1\n").unwrap();
            writer.flush().unwrap();
        }
        // Flush stdio so the mirror's write is visible before we read the file.
        let _ = io::stdout().flush();

        let durable_contents = std::fs::read_to_string(&durable_path).unwrap();
        let stdout_contents = std::fs::read_to_string(&stdout_path).unwrap();
        let combined = format!("{durable_contents}{stdout_contents}");
        let count = combined.matches("unique-marker-zbyh3-1").count();
        assert_eq!(
            count, 1,
            "line logged once must appear once across daemon.out.log and \
             daemon.stdout.log combined under a launchd-shaped (non-tty) stdout \
             redirect (got {count}): durable={durable_contents:?} \
             stdout={stdout_contents:?}"
        );
    }

    #[test]
    fn durable_log_rolls_at_threshold_and_honours_keep() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("daemon.out.log");
        let durable = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)
            .unwrap();
        let rotation = LogRotation {
            max_bytes: 20,
            keep: 2,
        };
        let sink =
            BestEffortMakeWriter::new(Some((path.clone(), durable)), LogMirror::None, rotation);
        {
            let mut writer = sink.make_writer();
            // Each write exceeds the threshold so every line triggers a roll.
            writer.write_all(b"early-content-line-01\n").unwrap(); // -> .1
            writer.write_all(b"second-content-line-02\n").unwrap(); // .1->.2, current->.1
            writer.write_all(b"third-content-line-003\n").unwrap(); // early evicted (keep=2)
            writer.flush().unwrap();
        }
        let current = std::fs::read_to_string(&path).unwrap_or_default();
        let rolled1 = std::fs::read_to_string(rolled_path(&path, 1)).unwrap_or_default();
        let rolled2 = std::fs::read_to_string(rolled_path(&path, 2)).unwrap_or_default();
        assert!(
            !rolled_path(&path, 3).exists(),
            "keep=2 must not retain {}.3",
            path.display()
        );
        // Exact end state (TASK-ZBYH3.1 F5): .1 = third, .2 = second, current
        // empty, early evicted. Disjunctive asserts would pass a broken roll
        // that overwrote .1 before promoting it to .2.
        assert_eq!(
            rolled1, "third-content-line-003\n",
            "exact .1 after three rolls; current={current:?} .2={rolled2:?}"
        );
        assert_eq!(
            rolled2, "second-content-line-02\n",
            "exact .2 after three rolls; current={current:?} .1={rolled1:?}"
        );
        assert_eq!(current, "", "current must be empty after the third roll");
        assert!(
            !rolled1.contains("early-content")
                && !rolled2.contains("early-content")
                && !current.contains("early-content"),
            "early generation must be evicted at keep=2"
        );
    }

    /// orgasmic:TASK-ZBYH3.1 — without a durable sink, a redirected stdout
    /// mirror must still receive lines (non-`serve` CLI via `init_tracing`).
    #[test]
    fn stdout_mirror_without_durable_still_writes_when_redirected() {
        use std::os::unix::io::AsRawFd;

        let _lock = STDOUT_REDIRECT_LOCK
            .lock()
            .unwrap_or_else(|p| p.into_inner());
        let dir = tempfile::tempdir().unwrap();
        let stdout_path = dir.path().join("cli-redirected.log");
        let stdout_file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&stdout_path)
            .unwrap();
        let saved_fd = unsafe { libc::dup(libc::STDOUT_FILENO) };
        assert!(saved_fd >= 0);
        assert!(unsafe { libc::dup2(stdout_file.as_raw_fd(), libc::STDOUT_FILENO) } >= 0);
        drop(stdout_file);
        struct RestoreStdout(i32);
        impl Drop for RestoreStdout {
            fn drop(&mut self) {
                unsafe {
                    libc::dup2(self.0, libc::STDOUT_FILENO);
                    libc::close(self.0);
                }
            }
        }
        let _restore = RestoreStdout(saved_fd);

        let sink = BestEffortMakeWriter::new(None, LogMirror::Stdout, LogRotation::default());
        {
            let mut writer = sink.make_writer();
            writer.write_all(b"cli-piped-marker-zbyh3-1\n").unwrap();
            writer.flush().unwrap();
        }
        let _ = io::stdout().flush();
        let contents = std::fs::read_to_string(&stdout_path).unwrap();
        assert!(
            contents.contains("cli-piped-marker-zbyh3-1"),
            "non-serve CLI tracing must still reach redirected stdout when \
             durable_log is None: {contents:?}"
        );
    }

    /// orgasmic:TASK-ZBYH3.1 F3 / TASK-G64ZH F1 — a failed reopen after a
    /// successful roll must move `dropped_log_writes` once per dropped line
    /// and retry only behind the backoff (not per line).
    #[test]
    fn failed_reopen_after_roll_counts_drops_and_retries() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("daemon.out.log");
        // Rotation disabled so a successful reopen is not immediately rolled.
        let rotation = LogRotation {
            max_bytes: 0,
            keep: 2,
        };
        // Post-roll state: path known, handle gone, opens rejected.
        let sink = BestEffortMakeWriter::with_missing_durable(path.clone(), rotation);

        let before = dropped_log_writes();
        {
            let mut writer = sink.make_writer();
            writer.write_all(b"while-silenced\n").unwrap();
            writer.write_all(b"still-silenced\n").unwrap();
        }
        let mid = dropped_log_writes();
        assert!(
            mid > before,
            "failed reopen must move dropped_log_writes ({before} -> {mid})"
        );
        {
            let state = sink.inner.lock().unwrap_or_else(|p| p.into_inner());
            assert_eq!(
                state.lines_dropped, 2,
                "each dropped line must count once (got {})",
                state.lines_dropped
            );
            assert_eq!(
                state.open_attempts, 1,
                "two rapid writes must share one reopen attempt inside the \
                 backoff window (got {})",
                state.open_attempts
            );
            assert_eq!(
                state.rotate_attempts, 0,
                "maybe_rotate must not run while the durable handle is missing \
                 (got {})",
                state.rotate_attempts
            );
        }
        assert!(
            !path.exists(),
            "open was rejected; durable path must not have been created yet"
        );

        // Allow reopen and write again.
        {
            let mut state = sink.inner.lock().unwrap_or_else(|p| p.into_inner());
            state.reject_durable_open = false;
            // Expire the backoff so the next write retries immediately.
            state.next_reopen_attempt = Instant::now();
        }
        {
            let mut writer = sink.make_writer();
            writer.write_all(b"recovered-after-reopen\n").unwrap();
            writer.flush().unwrap();
        }
        let contents = std::fs::read_to_string(&path).unwrap_or_default();
        assert!(
            contents.contains("recovered-after-reopen"),
            "reopen must be retried once opens succeed again: {contents:?}"
        );
    }

    /// orgasmic:TASK-G64ZH F1 — a persistent open failure costs a bounded
    /// number of reopen attempts per unit time, not one per log line. With
    /// rotation armed (the compound path the heading names), maybe_rotate
    /// must not re-enter while the handle is missing.
    #[test]
    fn persistent_open_failure_bounds_reopen_attempts_not_per_line() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("daemon.out.log");
        let rotation = LogRotation {
            max_bytes: 16,
            keep: 32,
        };
        let sink = BestEffortMakeWriter::with_missing_durable(path.clone(), rotation);
        let before = dropped_log_writes();
        const LINES: u64 = 40;
        {
            let mut writer = sink.make_writer();
            for i in 0..LINES {
                writer
                    .write_all(format!("stuck-line-{i:02}\n").as_bytes())
                    .unwrap();
            }
        }
        let after = dropped_log_writes();
        assert!(
            after > before,
            "persistent open failure must move dropped_log_writes ({before} -> {after})"
        );
        let state = sink.inner.lock().unwrap_or_else(|p| p.into_inner());
        assert_eq!(
            state.lines_dropped, LINES,
            "dropped lines must count once each, not ~4x (got {})",
            state.lines_dropped
        );
        assert_eq!(
            state.open_attempts, 1,
            "40 lines inside the initial 1s backoff window must cost one reopen \
             attempt, not one per line (got {})",
            state.open_attempts
        );
        assert_eq!(
            state.rotate_attempts, 0,
            "maybe_rotate must not run while durable is missing (got {})",
            state.rotate_attempts
        );
    }

    /// orgasmic:TASK-G64ZH F3 — both branches of the production mirror gate.
    /// A refactor that gated purely on `durable_path.is_some()` would silence
    /// the tty branch and keep every non-tty test green.
    #[test]
    fn mirror_gate_keeps_stdout_on_terminal_and_suppresses_off_terminal() {
        let path = PathBuf::from("/tmp/orgasmic-g64zh-unused.log");
        assert!(
            matches!(
                resolve_mirror(LogMirror::Stdout, Some(&path), true),
                LogMirror::Stdout
            ),
            "tty branch must keep LogMirror::Stdout when durable is present"
        );
        assert!(
            matches!(
                resolve_mirror(LogMirror::Stdout, Some(&path), false),
                LogMirror::None
            ),
            "non-tty branch must suppress LogMirror::Stdout when durable is present"
        );
        // Without a durable sink the mirror is unchanged either way (CLI path).
        assert!(matches!(
            resolve_mirror(LogMirror::Stdout, None, false),
            LogMirror::Stdout
        ));
    }

    /// orgasmic:TASK-G64ZH F3 — end-to-end: an injected terminal gate keeps the
    /// Stdout mirror alive even when fd 1 is redirected to a file. Inverting
    /// the predicate (or gating only on durable_path) makes this go red.
    #[test]
    fn interactive_terminal_gate_keeps_stdout_mirror_under_redirect() {
        use std::os::unix::io::AsRawFd;

        let _lock = STDOUT_REDIRECT_LOCK
            .lock()
            .unwrap_or_else(|p| p.into_inner());
        let dir = tempfile::tempdir().unwrap();
        let durable_path = dir.path().join("daemon.out.log");
        let stdout_path = dir.path().join("daemon.stdout.log");
        let stdout_file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&stdout_path)
            .unwrap();
        let durable = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&durable_path)
            .unwrap();

        let saved_fd = unsafe { libc::dup(libc::STDOUT_FILENO) };
        assert!(saved_fd >= 0, "dup stdout");
        assert!(
            unsafe { libc::dup2(stdout_file.as_raw_fd(), libc::STDOUT_FILENO) } >= 0,
            "dup2 stdout -> daemon.stdout.log"
        );
        drop(stdout_file);

        struct RestoreStdout(i32);
        impl Drop for RestoreStdout {
            fn drop(&mut self) {
                unsafe {
                    libc::dup2(self.0, libc::STDOUT_FILENO);
                    libc::close(self.0);
                }
            }
        }
        let _restore = RestoreStdout(saved_fd);

        // Inject terminal=true: the production interactive-serve branch.
        let sink = BestEffortMakeWriter::new_with_terminal_gate(
            Some((durable_path.clone(), durable)),
            LogMirror::Stdout,
            LogRotation::default(),
            true,
        );
        {
            let mut writer = sink.make_writer();
            writer.write_all(b"interactive-mirror-g64zh\n").unwrap();
            writer.flush().unwrap();
        }
        let _ = io::stdout().flush();

        let durable_contents = std::fs::read_to_string(&durable_path).unwrap();
        let stdout_contents = std::fs::read_to_string(&stdout_path).unwrap();
        assert!(
            durable_contents.contains("interactive-mirror-g64zh"),
            "durable must receive the line: {durable_contents:?}"
        );
        assert!(
            stdout_contents.contains("interactive-mirror-g64zh"),
            "tty-gated mirror must still write to stdout: {stdout_contents:?}"
        );
    }

    /// orgasmic:TASK-ZBYH3.1 F4 — a failed intermediate rename must be counted
    /// and noted (same as the final rename path).
    #[test]
    fn failed_intermediate_rename_counts_a_drop() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("daemon.out.log");
        // Seed .1 and a non-empty .2 directory so `.1 -> .2` fails with
        // ENOTEMPTY while the final `current -> .1` can still proceed.
        std::fs::write(rolled_path(&path, 1), "retained-gen\n").unwrap();
        let blocker = rolled_path(&path, 2);
        std::fs::create_dir(&blocker).unwrap();
        std::fs::write(blocker.join("pin"), "x").unwrap();
        let durable = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)
            .unwrap();
        let rotation = LogRotation {
            max_bytes: 16,
            keep: 2,
        };
        let sink =
            BestEffortMakeWriter::new(Some((path.clone(), durable)), LogMirror::None, rotation);
        let before = dropped_log_writes();
        {
            let mut writer = sink.make_writer();
            writer.write_all(b"aaaaaaaaaaaaaaaaaaaa\n").unwrap();
            writer.flush().unwrap();
        }
        let after = dropped_log_writes();
        assert!(
            after > before,
            "failed intermediate rename must move dropped_log_writes ({before} -> {after})"
        );
    }

    #[test]
    fn rotation_failure_does_not_propagate_and_counts_a_drop() {
        use std::os::unix::fs::PermissionsExt;

        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("daemon.out.log");
        let durable = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)
            .unwrap();
        // Block renames in the log directory so rotation's rename fails with
        // EACCES. Restored before tempdir cleanup.
        let mut perms = std::fs::metadata(dir.path()).unwrap().permissions();
        let original_mode = perms.mode();
        perms.set_mode(0o555);
        std::fs::set_permissions(dir.path(), perms).unwrap();
        let rotation = LogRotation {
            max_bytes: 16,
            keep: 2,
        };
        let sink =
            BestEffortMakeWriter::new(Some((path.clone(), durable)), LogMirror::None, rotation);
        let before = dropped_log_writes();
        {
            let mut writer = sink.make_writer();
            writer.write_all(b"aaaaaaaaaaaaaaaaaaaa\n").unwrap();
            writer
                .write_all(b"still-logging-after-failed-rotation\n")
                .unwrap();
            writer.flush().unwrap();
        }
        let after = dropped_log_writes();
        // Restore writability so tempfile can clean up and so we can read.
        let mut perms = std::fs::metadata(dir.path()).unwrap().permissions();
        perms.set_mode(original_mode);
        std::fs::set_permissions(dir.path(), perms).unwrap();
        assert!(
            after > before,
            "rotation failure must move dropped_log_writes ({before} -> {after})"
        );
        let contents = std::fs::read_to_string(&path).unwrap();
        assert!(
            contents.contains("still-logging-after-failed-rotation"),
            "daemon must keep logging the current file after a failed rotation: {contents:?}"
        );
    }
}
