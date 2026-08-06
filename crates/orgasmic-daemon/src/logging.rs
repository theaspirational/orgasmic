//! Best-effort tracing sinks so a dead stdout/stderr pipe never kills the
//! daemon or fails an HTTP request (TASK-FZF2D).
//!
//! Size-triggered rotation: TASK-ZBYH3. Stdout mirror gated on a terminal when
//! a durable sink is present (TASK-ZBYH3.1) — inode equality conflicts with a
//! separate `StandardOutPath`.
//!
//! orgasmic:TASK-FZF2D,TASK-ZBYH3,TASK-ZBYH3.1

use std::fs::{File, OpenOptions};
use std::io::{self, IsTerminal, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use tracing_subscriber::fmt::MakeWriter;
use tracing_subscriber::EnvFilter;

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

/// Install the global tracing subscriber once. Later calls are no-ops.
///
/// When `durable_log` is set, logs append to that path (created if needed, never
/// truncated). `mirror` is best-effort: failures are counted and swallowed so
/// they cannot propagate into request handling. When a durable sink is present,
/// a [`LogMirror::Stdout`] mirror is kept only if stdout is a terminal — so
/// launchd/systemd/service redirects never double-write, regardless of which
/// file `StandardOutPath` names. Without a durable sink (non-`serve` CLI), the
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
            record_drop();
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
            record_drop();
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
/// Production (`LogMirror::Stdout`): keep the mirror only on a real terminal.
/// Service managers redirect stdout to a file that is never a tty, so this
/// suppresses the double-write under any `StandardOutPath` (and on Windows,
/// where inode equality cannot be checked). When `durable_path` is `None`
/// (non-`serve` CLI via [`init_tracing`]), the mirror is returned unchanged so
/// piped/redirected CLI tracing still works — orgasmic:TASK-ZBYH3.1.
fn resolve_mirror(mirror: LogMirror, durable_path: Option<&Path>) -> LogMirror {
    let Some(path) = durable_path else {
        return mirror;
    };
    match mirror {
        LogMirror::Stdout => {
            if io::stdout().is_terminal() {
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
    /// Test-only: simulate EMFILE/ENOSPC on durable reopen (TASK-ZBYH3.1 F3).
    #[cfg(test)]
    reject_durable_open: bool,
}

enum MirrorState {
    Stdout,
    File(File),
    None,
}

impl BestEffortMakeWriter {
    fn new(durable: Option<(PathBuf, File)>, mirror: LogMirror, rotation: LogRotation) -> Self {
        let durable_path = durable.as_ref().map(|(path, _)| path.clone());
        let mirror = resolve_mirror(mirror, durable_path.as_deref());
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
                #[cfg(test)]
                reject_durable_open: false,
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
                bytes_written: rotation.max_bytes.saturating_add(1),
                rotation,
                mirror: MirrorState::None,
                reject_durable_open: true,
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
        if self.reject_durable_open {
            record_drop();
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
                true
            }
            None => false,
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
    if state.durable.is_none() && state.durable_path.is_some() {
        // Retry a failed post-roll reopen before giving up on this line
        // (TASK-ZBYH3.1 F3).
        let _ = state.try_open_durable();
    }
    if let Some(file) = state.durable.as_mut() {
        if file.write_all(buf).is_err() {
            record_drop();
        } else {
            state.bytes_written = state.bytes_written.saturating_add(buf.len() as u64);
        }
    } else if state.durable_path.is_some() {
        // Still no handle. Count the drop (so `dropped_log_writes` keeps
        // moving) and advance the byte counter so `maybe_rotate` retries.
        record_drop();
        state.bytes_written = state.bytes_written.saturating_add(buf.len() as u64);
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
    maybe_rotate(&mut state);
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
fn maybe_rotate(state: &mut SinkState) {
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
                    state.bytes_written = state.rotation.max_bytes.saturating_add(1);
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
            // Keep writing the current file when it still exists; if reopen
            // fails, stay past the threshold so a later write retries (F3).
            if !state.try_open_durable() {
                state.bytes_written = state.rotation.max_bytes.saturating_add(1);
            }
            return;
        }
    }

    if state.try_open_durable() {
        state.bytes_written = 0;
    } else {
        // Stay past the rotation threshold so the next write retries (F3).
        state.bytes_written = state.rotation.max_bytes.saturating_add(1);
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

    /// orgasmic:TASK-ZBYH3.1 F3 — a failed reopen after a successful roll must
    /// move `dropped_log_writes` and retry on a later write.
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
        assert!(
            !path.exists(),
            "open was rejected; durable path must not have been created yet"
        );

        // Allow reopen and write again.
        {
            let mut state = sink.inner.lock().unwrap_or_else(|p| p.into_inner());
            state.reject_durable_open = false;
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
