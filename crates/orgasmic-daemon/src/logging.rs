//! Best-effort tracing sinks so a dead stdout/stderr pipe never kills the
//! daemon or fails an HTTP request (TASK-FZF2D).
//!
//! orgasmic:TASK-FZF2D

use std::fs::{File, OpenOptions};
use std::io::{self, Write};
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use tracing_subscriber::fmt::MakeWriter;
use tracing_subscriber::EnvFilter;

/// Default durable daemon log under `$ORGASMIC_HOME/logs/`.
pub const DAEMON_OUT_LOG: &str = "daemon.out.log";

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

/// Install the global tracing subscriber once. Later calls are no-ops.
///
/// When `durable_log` is set, logs append to that path (created if needed, never
/// truncated). `mirror` is best-effort: failures are counted and swallowed so
/// they cannot propagate into request handling.
///
/// Returns `true` when this call installed the subscriber.
pub fn init_tracing_to(
    default_filter: &str,
    durable_log: Option<&Path>,
    mirror: LogMirror,
) -> bool {
    ignore_sigpipe();
    let filter = EnvFilter::try_from_default_env()
        .or_else(|_| EnvFilter::try_new(default_filter))
        .unwrap_or_else(|_| EnvFilter::new("info"));
    let durable = durable_log.and_then(open_durable_log);
    let sink = BestEffortMakeWriter::new(durable, mirror);
    tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_writer(sink)
        .try_init()
        .is_ok()
}

/// Stdout-only best-effort tracing (non-`serve` CLI commands).
pub fn init_tracing(default_filter: &str) -> bool {
    init_tracing_to(default_filter, None, LogMirror::Stdout)
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

#[derive(Clone)]
struct BestEffortMakeWriter {
    inner: Arc<Mutex<SinkState>>,
}

struct SinkState {
    durable: Option<File>,
    mirror: MirrorState,
}

enum MirrorState {
    Stdout,
    File(File),
    None,
}

impl BestEffortMakeWriter {
    fn new(durable: Option<File>, mirror: LogMirror) -> Self {
        let mirror = match mirror {
            LogMirror::Stdout => MirrorState::Stdout,
            LogMirror::Writer(file) => MirrorState::File(file),
            LogMirror::None => MirrorState::None,
        };
        Self {
            inner: Arc::new(Mutex::new(SinkState { durable, mirror })),
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
        let sink = BestEffortMakeWriter::new(Some(durable), LogMirror::Writer(mirror));
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
}
