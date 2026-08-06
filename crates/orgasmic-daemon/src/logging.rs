//! Best-effort tracing sinks so a dead stdout/stderr pipe never kills the
//! daemon or fails an HTTP request (TASK-FZF2D).
//!
//! Size-triggered rotation and same-inode mirror suppression: TASK-ZBYH3.
//!
//! orgasmic:TASK-FZF2D,TASK-ZBYH3

use std::fs::{File, OpenOptions};
use std::io::{self, Write};
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
/// they cannot propagate into request handling. When the mirror resolves to the
/// same device+inode as the durable file (launchd `StandardOutPath` pointing at
/// the durable log), the mirror is suppressed so each line lands once.
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
/// false on non-Unix — we cannot detect the launchd double-open there.
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

#[cfg(unix)]
fn stdout_same_as_path(path: &Path) -> bool {
    use std::os::unix::io::FromRawFd;
    // Dup stdout so we can fstat without taking ownership of fd 1.
    let fd = unsafe { libc::dup(libc::STDOUT_FILENO) };
    if fd < 0 {
        return false;
    }
    let file = unsafe { File::from_raw_fd(fd) };
    same_file_as_path(&file, path)
}

#[cfg(not(unix))]
fn stdout_same_as_path(_path: &Path) -> bool {
    false
}

/// Resolve a requested mirror against the durable path. When they name the
/// same inode, return [`LogMirror::None`] so tracing does not double-write
/// (TASK-ZBYH3 defect 1 under launchd).
fn resolve_mirror(mirror: LogMirror, durable_path: Option<&Path>) -> LogMirror {
    let Some(path) = durable_path else {
        return mirror;
    };
    match mirror {
        LogMirror::Stdout => {
            if stdout_same_as_path(path) {
                LogMirror::None
            } else {
                LogMirror::Stdout
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
            })),
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
    if let Some(file) = state.durable.as_mut() {
        if file.write_all(buf).is_err() {
            record_drop();
        } else {
            state.bytes_written = state.bytes_written.saturating_add(buf.len() as u64);
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

    let keep = state.rotation.keep;
    if keep == 0 {
        // No rolled retain: remove the current path and reopen empty.
        state.durable = None;
        if let Err(err) = std::fs::remove_file(&path) {
            if err.kind() != io::ErrorKind::NotFound {
                record_drop();
                state.durable = open_durable_log(&path);
                state.bytes_written = state
                    .durable
                    .as_ref()
                    .and_then(|f| f.metadata().ok())
                    .map(|m| m.len())
                    .unwrap_or(0);
                return;
            }
        }
    } else {
        // Roll .N-1 -> .N … .1 -> .2, then current -> .1.
        for i in (1..keep).rev() {
            let from = rolled_path(&path, i);
            let to = rolled_path(&path, i + 1);
            match std::fs::rename(&from, &to) {
                Ok(()) => {}
                Err(err) if err.kind() == io::ErrorKind::NotFound => {}
                Err(_) => {}
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
            // Keep writing the current file — reopen the original path (it
            // still exists because rename failed).
            state.durable = open_durable_log(&path);
            state.bytes_written = state
                .durable
                .as_ref()
                .and_then(|f| f.metadata().ok())
                .map(|m| m.len())
                .unwrap_or(0);
            return;
        }
    }

    match open_durable_log(&path) {
        Some(file) => {
            state.bytes_written = 0;
            state.durable = Some(file);
        }
        None => {
            // open_durable_log already recorded a drop. Nothing to write to.
            state.bytes_written = 0;
            state.durable = None;
        }
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
            writer.write_all(b"third-content-line-003\n").unwrap(); // .2 dropped (keep=2), …
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
        assert!(
            rolled_path(&path, 1).exists() && rolled_path(&path, 2).exists(),
            "keep=2 must retain .1 and .2 after three threshold crossings"
        );
        // The oldest line (early) must have been pushed out of keep=2, or still
        // sit in .2 if the third roll promoted it there — either way a rolled
        // file must retain content written before the latest current file.
        let rolled_blob = format!("{rolled1}{rolled2}");
        assert!(
            rolled_blob.contains("second-content") || rolled_blob.contains("early-content"),
            "rolled files must retain earlier content; .1={rolled1:?} .2={rolled2:?}"
        );
        assert!(
            current.contains("third-content") || rolled1.contains("third-content"),
            "latest writes must be retained; current={current:?} .1={rolled1:?}"
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
