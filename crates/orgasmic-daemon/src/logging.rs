//! Best-effort tracing sinks so a dead stdout/stderr pipe never kills the
//! daemon or fails an HTTP request (TASK-FZF2D).
//!
//! Size-triggered rotation: TASK-ZBYH3. Stdout mirror: service definitions
//! orgasmic writes set `ORGASMIC_LOG_MIRROR=off` (an older binary ignores the
//! unknown env and degrades); `--no-log-mirror` remains the interactive
//! override. `is_terminal()` is the fallback for supervisors orgasmic did not
//! write (TASK-ZBYH3.1, TASK-G64ZH, TASK-G64ZH.1). Whether a stdout mirror
//! would double-write is decided from the live durable handle at write time
//! (TASK-G64ZH.1.1), not from a construction-time path/tty/flag proxy.
//! Reopen after a failed durable open — including boot — is backoff-bounded
//! (TASK-G64ZH); so is a retry after a failed rotation (TASK-CGJM7). A failed
//! durable *write* drops the handle so the same reopen backoff owns recovery
//! and the line falls back to the stdout mirror (TASK-0KP3T).
//!
//! orgasmic:TASK-FZF2D,TASK-ZBYH3,TASK-ZBYH3.1,TASK-G64ZH,TASK-G64ZH.1,TASK-G64ZH.1.1,TASK-CGJM7,TASK-0KP3T

use std::fs::{File, OpenOptions};
use std::io::{self, IsTerminal, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use tracing_subscriber::fmt::MakeWriter;
use tracing_subscriber::EnvFilter;

/// Initial wait before retrying a failed durable reopen (TASK-G64ZH F1) or a
/// failed rotation (TASK-CGJM7).
const REOPEN_BACKOFF_INITIAL: Duration = Duration::from_secs(1);
/// Cap for the reopen/rotation backoff (TASK-G64ZH F1, TASK-CGJM7).
const REOPEN_BACKOFF_CAP: Duration = Duration::from_secs(60);

/// Env var that suppresses the stdout tracing mirror when set to `off` / `0` /
/// `false` (case-insensitive). Service definitions orgasmic writes set this
/// so a runtime rollback cannot hand an older binary an unknown CLI flag.
/// orgasmic:TASK-G64ZH.1
pub const LOG_MIRROR_ENV: &str = "ORGASMIC_LOG_MIRROR";

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

/// Process-wide count of sink I/O failures. One meaning across every path:
/// one per line that never landed in a sink it was routed to (durable
/// open/write failure, mirror write failure) plus one per failed rotation
/// attempt — never one per syscall, and rotation attempts are backoff-bounded
/// (TASK-G64ZH F1, TASK-CGJM7). Cheap to read; never consulted on the request
/// success path.
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
/// 2. else [`LOG_MIRROR_ENV`]=`off` (also `0` / `false`, case-insensitive) →
///    [`LogMirror::None`]
/// 3. else [`LogMirror::Stdout`], and [`resolve_mirror`] turns a non-tty
///    durable-intended launch into [`MirrorState::StdoutWhenNoDurable`]
///
/// Explicit off always wins over a tty at the request layer. An old LaunchAgent
/// plist without the env still avoids steady-state double-write under launchd
/// because stdout is not a terminal — but a missing durable handle falls back
/// to stdout at write time (TASK-G64ZH.1.1).
pub fn requested_log_mirror(no_log_mirror: bool) -> LogMirror {
    if no_log_mirror || env_log_mirror_off() {
        LogMirror::None
    } else {
        LogMirror::Stdout
    }
}

/// True when [`LOG_MIRROR_ENV`] requests mirror suppression.
pub fn env_log_mirror_off() -> bool {
    match std::env::var(LOG_MIRROR_ENV) {
        Ok(value) => {
            let value = value.trim();
            value.eq_ignore_ascii_case("off") || value == "0" || value.eq_ignore_ascii_case("false")
        }
        Err(_) => false,
    }
}

#[derive(Debug)]
enum DurableOpenError {
    CreateDir { parent: PathBuf, err: io::Error },
    Open { err: io::Error },
}

impl DurableOpenError {
    fn kind(&self) -> io::ErrorKind {
        match self {
            Self::CreateDir { err, .. } | Self::Open { err } => err.kind(),
        }
    }
}

/// Open the durable log for append, creating parent dirs as needed.
/// Does not write to stderr — callers own the diagnostic (and its throttle).
fn open_durable_log(path: &Path) -> Result<File, DurableOpenError> {
    if let Some(parent) = path.parent() {
        if let Err(err) = std::fs::create_dir_all(parent) {
            return Err(DurableOpenError::CreateDir {
                parent: parent.to_path_buf(),
                err,
            });
        }
    }
    match OpenOptions::new().create(true).append(true).open(path) {
        Ok(file) => Ok(file),
        Err(err) => Err(DurableOpenError::Open { err }),
    }
}

fn report_durable_open_failure(path: &Path, err: &DurableOpenError) {
    match err {
        DurableOpenError::CreateDir { parent, err } => {
            let _ = writeln!(
                io::stderr(),
                "orgasmic: failed to create log dir {}: {err}",
                parent.display()
            );
        }
        DurableOpenError::Open { err } => {
            let _ = writeln!(
                io::stderr(),
                "orgasmic: failed to open log file {}: {err}",
                path.display()
            );
        }
    }
}

fn report_durable_reopened(path: &Path) {
    let _ = writeln!(
        io::stderr(),
        "orgasmic: reopened log file {}",
        path.display()
    );
}

/// Map a requested durable path to `(path, open_result)`.
///
/// Keeps the path when the boot open fails so [`SinkState::maybe_reopen_durable`]
/// owns the 1s→60s retry and [`record_drop`] fires per dropped line
/// (TASK-G64ZH.1 F-A). Collapsing with `and_then` discarded the path and made
/// construction resolve the mirror to permanent silence — with the reopen
/// backoff unreachable for the whole daemon lifetime.
///
/// The `Result` carries the boot error so construction can print once and seed
/// stderr throttling for reopen retries (TASK-G64ZH.1.1 R-4).
fn resolve_durable_open(
    durable_log: Option<&Path>,
) -> Option<(PathBuf, Result<File, DurableOpenError>)> {
    durable_log.map(|path| (path.to_path_buf(), open_durable_log(path)))
}

/// Install the global tracing subscriber once. Later calls are no-ops.
///
/// When `durable_log` is set, logs append to that path (created if needed, never
/// truncated). A failed boot open still retains the path so reopen backoff can
/// recover. `mirror` is best-effort: failures are counted and swallowed so they
/// cannot propagate into request handling. When a durable sink is *intended*
/// and stdout is not a terminal (or the caller passed [`LogMirror::None`]),
/// stdout is suppressed only while a live durable handle exists — if the handle
/// is missing, lines fall back to stdout so a failure window is not total
/// silence (TASK-G64ZH.1.1). Without a durable path (non-`serve` CLI), the
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
    let durable = resolve_durable_open(durable_log);
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

/// Resolve a requested mirror into a write-time policy.
///
/// Production (`LogMirror::Stdout`) with an intended durable sink on a
/// non-terminal stdout, and explicit [`LogMirror::None`] with a durable path,
/// both become [`MirrorState::StdoutWhenNoDurable`]: suppress stdout only while
/// a live durable handle exists. That is the "do not double-write" contract
/// evaluated at write time — not a construction-time path/tty/flag proxy
/// (TASK-G64ZH.1.1). When `durable_path` is `None` (non-`serve` CLI via
/// [`init_tracing`]), a Stdout request stays always-on so piped/redirected CLI
/// tracing still works — orgasmic:TASK-ZBYH3.1,TASK-G64ZH,TASK-G64ZH.1,TASK-G64ZH.1.1.
fn resolve_mirror(
    mirror: LogMirror,
    durable_path: Option<&Path>,
    stdout_is_terminal: bool,
) -> MirrorState {
    match mirror {
        LogMirror::Stdout => {
            if durable_path.is_none() || stdout_is_terminal {
                MirrorState::Stdout
            } else {
                MirrorState::StdoutWhenNoDurable
            }
        }
        LogMirror::Writer(file) => {
            if let Some(path) = durable_path {
                if same_file_as_path(&file, path) {
                    // Same inode as the durable file: suppress while the handle
                    // is live; fall back to stdout if it is not.
                    MirrorState::StdoutWhenNoDurable
                } else {
                    MirrorState::File(file)
                }
            } else {
                MirrorState::File(file)
            }
        }
        LogMirror::None => {
            if durable_path.is_some() {
                MirrorState::StdoutWhenNoDurable
            } else {
                MirrorState::None
            }
        }
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
    /// Earliest Instant at which a failed rotation may be retried. Without it
    /// a persistent rename failure re-ran the whole rename loop on every line
    /// (TASK-CGJM7).
    next_rotate_attempt: Instant,
    /// Current rotation backoff; same 1s→60s shape as `reopen_backoff`.
    rotate_backoff: Duration,
    /// Last durable open/write error kind reported to stderr. Same-kind
    /// retries stay quiet; a kind change or the first success after a streak
    /// re-emits (TASK-G64ZH.1.1 R-4).
    last_open_err_kind: Option<io::ErrorKind>,
    /// Test-only: simulate EMFILE/ENOSPC on durable reopen (TASK-ZBYH3.1 F3).
    #[cfg(test)]
    reject_durable_open: bool,
    /// Test-only: count reopen attempts (TASK-G64ZH F1 bound).
    #[cfg(test)]
    open_attempts: u64,
    /// Test-only: count rotation attempts that passed every gate, including
    /// the rotation backoff (TASK-G64ZH F1, TASK-CGJM7).
    #[cfg(test)]
    rotate_attempts: u64,
    /// Test-only: lines that never landed in the durable file (TASK-G64ZH F1).
    #[cfg(test)]
    lines_dropped: u64,
    /// Test-only clock for backoff liveness (TASK-G64ZH.1 F-E). When set,
    /// used instead of [`Instant::now`] so a second attempt can be asserted
    /// without sleeping.
    #[cfg(test)]
    clock: Option<Instant>,
}

enum MirrorState {
    /// Always mirror to stdout (interactive tty, or no durable path).
    Stdout,
    /// Suppress stdout while a durable handle is live; write to stdout when it
    /// is not. The construction-time "suppressed because a durable sink exists"
    /// marker — evaluated against the live handle at write time
    /// (TASK-G64ZH.1.1).
    StdoutWhenNoDurable,
    File(File),
    /// Never mirror (explicit [`LogMirror::None`] with no durable path).
    None,
}

impl BestEffortMakeWriter {
    /// `durable` is `(path, open_result)`. Pass `Err` when the boot open failed
    /// so the path is retained for reopen backoff (TASK-G64ZH.1 F-A) and the
    /// error seeds stderr throttling (TASK-G64ZH.1.1 R-4).
    fn new(
        durable: Option<(PathBuf, Result<File, DurableOpenError>)>,
        mirror: LogMirror,
        rotation: LogRotation,
    ) -> Self {
        Self::new_with_terminal_gate(durable, mirror, rotation, io::stdout().is_terminal())
    }

    /// Like [`Self::new`], but with an injectable terminal predicate so both
    /// mirror-gate branches can be asserted (TASK-G64ZH F3).
    fn new_with_terminal_gate(
        durable: Option<(PathBuf, Result<File, DurableOpenError>)>,
        mirror: LogMirror,
        rotation: LogRotation,
        stdout_is_terminal: bool,
    ) -> Self {
        // Path presence means a durable sink is *intended* — the live-handle
        // question is answered at write time (TASK-G64ZH.1.1).
        let durable_path = durable.as_ref().map(|(path, _)| path.clone());
        let mirror = resolve_mirror(mirror, durable_path.as_deref(), stdout_is_terminal);
        let (durable_path, durable, bytes_written, last_open_err_kind) = match durable {
            Some((path, Ok(file))) => {
                let len = file.metadata().map(|m| m.len()).unwrap_or(0);
                (Some(path), Some(file), len, None)
            }
            Some((path, Err(err))) => {
                report_durable_open_failure(&path, &err);
                (Some(path), None, 0, Some(err.kind()))
            }
            None => (None, None, 0, None),
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
                next_rotate_attempt: Instant::now(),
                rotate_backoff: REOPEN_BACKOFF_INITIAL,
                last_open_err_kind,
                #[cfg(test)]
                reject_durable_open: false,
                #[cfg(test)]
                open_attempts: 0,
                #[cfg(test)]
                rotate_attempts: 0,
                #[cfg(test)]
                lines_dropped: 0,
                #[cfg(test)]
                clock: None,
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
                // Fall back to stdout while the handle is missing (TASK-G64ZH.1.1 R-2).
                mirror: MirrorState::StdoutWhenNoDurable,
                next_reopen_attempt: Instant::now(),
                reopen_backoff: REOPEN_BACKOFF_INITIAL,
                next_rotate_attempt: Instant::now(),
                rotate_backoff: REOPEN_BACKOFF_INITIAL,
                // Pretend the failure was already noted so injected retries stay quiet.
                last_open_err_kind: Some(io::ErrorKind::Other),
                reject_durable_open: true,
                open_attempts: 0,
                rotate_attempts: 0,
                lines_dropped: 0,
                clock: None,
            })),
        }
    }
}

impl SinkState {
    fn now(&self) -> Instant {
        #[cfg(test)]
        if let Some(t) = self.clock {
            return t;
        }
        Instant::now()
    }

    fn note_open_failure(&mut self, path: &Path, err: &DurableOpenError) {
        let kind = err.kind();
        if self.last_open_err_kind == Some(kind) {
            return;
        }
        report_durable_open_failure(path, err);
        self.last_open_err_kind = Some(kind);
    }

    fn note_open_success(&mut self, path: &Path) {
        if self.last_open_err_kind.take().is_some() {
            report_durable_reopened(path);
        }
    }

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
            let err = DurableOpenError::Open {
                err: io::Error::other("injected open failure"),
            };
            self.note_open_failure(&path, &err);
            return false;
        }
        match open_durable_log(&path) {
            Ok(file) => {
                self.bytes_written = file.metadata().map(|m| m.len()).unwrap_or(0);
                self.durable = Some(file);
                self.reopen_backoff = REOPEN_BACKOFF_INITIAL;
                self.next_reopen_attempt = self.now();
                self.note_open_success(&path);
                true
            }
            Err(err) => {
                self.note_open_failure(&path, &err);
                false
            }
        }
    }

    fn schedule_reopen_retry(&mut self) {
        let now = self.now();
        schedule_retry(&mut self.next_reopen_attempt, &mut self.reopen_backoff, now);
    }

    /// Same 1s→60s shape as the reopen retry, for a failed rotation (TASK-CGJM7).
    fn schedule_rotate_retry(&mut self) {
        let now = self.now();
        schedule_retry(&mut self.next_rotate_attempt, &mut self.rotate_backoff, now);
    }

    /// A live handle whose write failed (ENOSPC/EDQUOT/EIO): drop it so
    /// [`Self::maybe_reopen_durable`] owns recovery behind the backoff, and
    /// let the write-time mirror fallback take the line (TASK-0KP3T). One
    /// stderr line per error kind, throttled like open failures.
    fn note_write_failure(&mut self, err: &io::Error) {
        let kind = err.kind();
        if self.last_open_err_kind != Some(kind) {
            if let Some(path) = &self.durable_path {
                let _ = writeln!(
                    io::stderr(),
                    "orgasmic: failed to write log file {}: {err}",
                    path.display()
                );
            }
            self.last_open_err_kind = Some(kind);
        }
        self.durable = None;
        self.schedule_reopen_retry();
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
        if self.now() < self.next_reopen_attempt {
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
    // "Durable took the line" is decided from the write result, not from
    // handle presence (TASK-0KP3T).
    let mut landed = false;
    if let Some(file) = state.durable.as_mut() {
        match file.write_all(buf) {
            Ok(()) => {
                state.bytes_written = state.bytes_written.saturating_add(buf.len() as u64);
                landed = true;
            }
            Err(err) => state.note_write_failure(&err),
        }
    }
    if !landed && state.durable_path.is_some() {
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
        MirrorState::StdoutWhenNoDurable => {
            // A successful durable write means the durable sink already has
            // the line — do not double-write. Otherwise fall back to stdout so
            // a failure window is not total silence (TASK-G64ZH.1.1, TASK-0KP3T).
            if !landed && io::stdout().write_all(buf).is_err() {
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
        MirrorState::StdoutWhenNoDurable => {
            if state.durable.is_none() && io::stdout().flush().is_err() {
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

/// Double `backoff` up to [`REOPEN_BACKOFF_CAP`] and park `next` behind it.
fn schedule_retry(next: &mut Instant, backoff: &mut Duration, now: Instant) {
    *next = now + *backoff;
    *backoff = backoff.saturating_mul(2).min(REOPEN_BACKOFF_CAP);
}

/// Roll the durable file when the tracked byte count exceeds the threshold.
/// Failures never propagate — keep writing the current handle, count ONE drop
/// per failed attempt, and retry only behind `next_rotate_attempt` so a
/// persistent rename failure costs a bounded number of syscalls and stderr
/// lines per unit time, not per tracing line (TASK-CGJM7).
/// Must not be called while `state.durable` is `None` (the write path owns
/// reopen retries via [`SinkState::maybe_reopen_durable`]).
fn maybe_rotate(state: &mut SinkState) {
    if state.durable.is_none() {
        return;
    }
    if state.rotation.max_bytes == 0 {
        return;
    }
    if state.bytes_written <= state.rotation.max_bytes {
        return;
    }
    if state.now() < state.next_rotate_attempt {
        return;
    }
    let Some(path) = state.durable_path.clone() else {
        return;
    };
    #[cfg(test)]
    {
        state.rotate_attempts = state.rotate_attempts.saturating_add(1);
    }
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
                state.schedule_rotate_retry();
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
        // Roll .N -> .(N+1) … .1 -> .2, then current -> .1. Remember the first
        // failure; ONE drop and ONE stderr line per attempt, not per rename.
        let mut failed: Option<(PathBuf, io::Error)> = None;
        for i in (1..=highest).rev() {
            let from = rolled_path(&path, i);
            let to = rolled_path(&path, i + 1);
            match std::fs::rename(&from, &to) {
                Ok(()) => {}
                Err(err) if err.kind() == io::ErrorKind::NotFound => {}
                Err(err) => {
                    failed.get_or_insert((from, err));
                }
            }
        }
        // Drop the handle before renaming so the path is free; the open fd
        // would otherwise keep writing the renamed inode after we reopen.
        state.durable = None;
        let current_err = std::fs::rename(&path, rolled_path(&path, 1)).err();
        let current_failed = current_err.is_some();
        if let Some(err) = current_err {
            failed = Some((path.clone(), err));
        }
        if let Some((from, err)) = &failed {
            record_drop();
            let _ = writeln!(
                io::stderr(),
                "orgasmic: log rotation rename failed for {}: {err}",
                from.display()
            );
        }
        if current_failed {
            // Current file was not rolled: keep writing it when reopen succeeds
            // (bytes_written stays above the threshold) and retry the roll only
            // behind the rotation backoff (TASK-CGJM7). A failed reopen is the
            // write-path backoff's to retry (TASK-G64ZH F1).
            state.schedule_rotate_retry();
            if !state.try_open_durable() {
                state.schedule_reopen_retry();
            }
            return;
        }
    }

    state.rotate_backoff = REOPEN_BACKOFF_INITIAL;
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
            Some((path.clone(), Ok(durable))),
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
            Some((path.clone(), Ok(durable))),
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
            Some((durable_path.clone(), Ok(durable))),
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
            BestEffortMakeWriter::new(Some((path.clone(), Ok(durable))), LogMirror::None, rotation);
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

    /// orgasmic:TASK-G64ZH F3 / TASK-G64ZH.1.1 — both branches of the production
    /// mirror gate. Non-tty with an intended durable sink is
    /// `StdoutWhenNoDurable` (suppress only while the handle is live), not a
    /// permanent `None`. A refactor that gated purely on `durable_path.is_some()`
    /// into permanent silence would keep every non-tty test green and break
    /// the tty branch.
    #[test]
    fn mirror_gate_keeps_stdout_on_terminal_and_suppresses_off_terminal() {
        let path = PathBuf::from("/tmp/orgasmic-g64zh-unused.log");
        assert!(
            matches!(
                resolve_mirror(LogMirror::Stdout, Some(&path), true),
                MirrorState::Stdout
            ),
            "tty branch must keep always-on Stdout when durable is intended"
        );
        assert!(
            matches!(
                resolve_mirror(LogMirror::Stdout, Some(&path), false),
                MirrorState::StdoutWhenNoDurable
            ),
            "non-tty branch must suppress Stdout only while a durable handle is live"
        );
        // Without a durable sink the mirror is always-on either way (CLI path).
        assert!(matches!(
            resolve_mirror(LogMirror::Stdout, None, false),
            MirrorState::Stdout
        ));
        assert!(
            matches!(
                resolve_mirror(LogMirror::None, Some(&path), false),
                MirrorState::StdoutWhenNoDurable
            ),
            "explicit suppression with an intended durable sink still falls back              when the handle is missing"
        );
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
            Some((durable_path.clone(), Ok(durable))),
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
            BestEffortMakeWriter::new(Some((path.clone(), Ok(durable))), LogMirror::None, rotation);
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
            BestEffortMakeWriter::new(Some((path.clone(), Ok(durable))), LogMirror::None, rotation);
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

    /// orgasmic:TASK-CGJM7 — the mode-555 reproducer: renames fail EACCES
    /// while appends to the existing file keep succeeding. A persistent rename
    /// failure must cost ONE rotation attempt per backoff window, not one
    /// rename loop (and ~32 stderr lines) per tracing line; every line still
    /// lands; the retry does happen once the window expires.
    #[test]
    fn persistent_rotation_rename_failure_bounds_attempts_not_per_line() {
        use std::os::unix::fs::PermissionsExt;

        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("daemon.out.log");
        std::fs::write(rolled_path(&path, 1), "retained-gen\n").unwrap();
        let durable = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)
            .unwrap();
        let mut perms = std::fs::metadata(dir.path()).unwrap().permissions();
        let original_mode = perms.mode();
        perms.set_mode(0o555);
        std::fs::set_permissions(dir.path(), perms).unwrap();
        struct RestoreMode(PathBuf, u32);
        impl Drop for RestoreMode {
            fn drop(&mut self) {
                let mut perms = std::fs::metadata(&self.0).unwrap().permissions();
                perms.set_mode(self.1);
                let _ = std::fs::set_permissions(&self.0, perms);
            }
        }
        let _restore = RestoreMode(dir.path().to_path_buf(), original_mode);

        let rotation = LogRotation {
            max_bytes: 16,
            keep: 32,
        };
        let sink =
            BestEffortMakeWriter::new(Some((path.clone(), Ok(durable))), LogMirror::None, rotation);
        const LINES: u64 = 40;
        {
            let mut writer = sink.make_writer();
            for i in 0..LINES {
                writer
                    .write_all(format!("rotation-stuck-line-{i:02}\n").as_bytes())
                    .unwrap();
            }
            writer.flush().unwrap();
        }
        {
            let state = sink.inner.lock().unwrap_or_else(|p| p.into_inner());
            assert_eq!(
                state.rotate_attempts, 1,
                "40 over-threshold lines inside the initial 1s rotation backoff \
                 window must cost one rotation attempt, not one per line (got {})",
                state.rotate_attempts
            );
            assert_eq!(
                state.open_attempts, 1,
                "one reopen for the one failed rotation (got {})",
                state.open_attempts
            );
            assert_eq!(
                state.lines_dropped, 0,
                "every line landed in the current file; a failed roll is not a \
                 dropped line (got {})",
                state.lines_dropped
            );
            assert!(
                state.next_rotate_attempt > state.now(),
                "failed rotation must park the next attempt behind the backoff"
            );
        }
        let contents = std::fs::read_to_string(&path).unwrap();
        assert!(
            contents.contains("rotation-stuck-line-00")
                && contents.contains("rotation-stuck-line-39"),
            "daemon must keep logging the current file across a failed rotation: {contents:?}"
        );

        // Liveness: once the window expires the roll is retried.
        {
            let mut state = sink.inner.lock().unwrap_or_else(|p| p.into_inner());
            state.clock = Some(state.now() + REOPEN_BACKOFF_CAP + Duration::from_millis(1));
        }
        {
            let mut writer = sink.make_writer();
            writer.write_all(b"rotation-retry-window\n").unwrap();
        }
        let state = sink.inner.lock().unwrap_or_else(|p| p.into_inner());
        assert_eq!(
            state.rotate_attempts, 2,
            "after the rotation backoff expires a second attempt must run (got {})",
            state.rotate_attempts
        );
    }

    /// orgasmic:TASK-0KP3T — a live handle whose write fails (ENOSPC/EIO
    /// class; here a read-only fd) must not lose the line and must not wedge
    /// the sink: the line reaches the stdout fallback, the handle is dropped so
    /// the reopen backoff owns recovery, and a later successful reopen resumes
    /// durable writes without double-writing.
    #[test]
    fn failed_durable_write_falls_back_to_stdout_and_recovers_on_reopen() {
        use std::os::unix::io::AsRawFd;

        let _lock = STDOUT_REDIRECT_LOCK
            .lock()
            .unwrap_or_else(|p| p.into_inner());
        let dir = tempfile::tempdir().unwrap();
        let durable_path = dir.path().join("daemon.out.log");
        let stdout_path = dir.path().join("daemon.stdout.log");
        std::fs::write(&durable_path, "").unwrap();
        let stdout_file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&stdout_path)
            .unwrap();
        let saved_fd = unsafe { libc::dup(libc::STDOUT_FILENO) };
        assert!(saved_fd >= 0, "dup stdout");
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

        // Open succeeds, every write fails (EBADF): the handle-present /
        // write-failed shape no other test drives.
        let read_only = File::open(&durable_path).unwrap();
        let sink = BestEffortMakeWriter::new_with_terminal_gate(
            Some((durable_path.clone(), Ok(read_only))),
            LogMirror::Stdout,
            LogRotation::default(),
            false,
        );
        let before = dropped_log_writes();
        {
            let mut writer = sink.make_writer();
            writer.write_all(b"0kp3t-write-failed-1\n").unwrap();
            writer.write_all(b"0kp3t-write-failed-2\n").unwrap();
            writer.flush().unwrap();
        }
        let _ = io::stdout().flush();
        assert!(
            dropped_log_writes() > before,
            "failed durable write must move dropped_log_writes"
        );
        {
            let state = sink.inner.lock().unwrap_or_else(|p| p.into_inner());
            assert!(
                state.durable.is_none(),
                "a handle whose write failed must be dropped so reopen owns recovery"
            );
            assert_eq!(
                state.lines_dropped, 2,
                "each line that never landed durably counts once (got {})",
                state.lines_dropped
            );
            assert_eq!(
                state.open_attempts, 0,
                "the second line must sit inside the reopen backoff, not reopen \
                 per line (got {})",
                state.open_attempts
            );
        }
        let stdout_contents = std::fs::read_to_string(&stdout_path).unwrap();
        assert!(
            stdout_contents.contains("0kp3t-write-failed-1")
                && stdout_contents.contains("0kp3t-write-failed-2"),
            "lines the durable handle rejected must reach the stdout fallback: \
             {stdout_contents:?}"
        );

        // Expire the backoff: reopen (append mode) succeeds and durable resumes.
        {
            let mut state = sink.inner.lock().unwrap_or_else(|p| p.into_inner());
            state.next_reopen_attempt = state.now();
        }
        {
            let mut writer = sink.make_writer();
            writer.write_all(b"0kp3t-recovered\n").unwrap();
            writer.flush().unwrap();
        }
        let _ = io::stdout().flush();
        let durable_contents = std::fs::read_to_string(&durable_path).unwrap();
        assert!(
            durable_contents.contains("0kp3t-recovered"),
            "after a successful reopen durable writes must resume: {durable_contents:?}"
        );
        let stdout_contents = std::fs::read_to_string(&stdout_path).unwrap();
        assert!(
            !stdout_contents.contains("0kp3t-recovered"),
            "a line the durable sink took must not also hit stdout: {stdout_contents:?}"
        );
    }

    /// orgasmic:TASK-G64ZH.1 F-A — boot-time durable-open failure under a
    /// mirror-suppressed launch must keep the path. Pre-fix, `and_then`
    /// discarded it with the handle and construction resolved the mirror to
    /// permanent silence, with the reopen backoff unreachable for the daemon's
    /// whole life.
    #[test]
    fn boot_open_failure_with_mirror_suppressed_keeps_path_counts_drops_and_retries() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("daemon.out.log");
        // EISDIR: OpenOptions::open fails deterministically, no permission games.
        std::fs::create_dir(&path).unwrap();

        // Same construction `init_tracing_to` uses after a failed boot open.
        let durable = resolve_durable_open(Some(&path));
        let sink = BestEffortMakeWriter::new(durable, LogMirror::None, LogRotation::default());

        // FIRST assertion — verify/TASK-G64ZH.1 pins this message under injection.
        {
            let state = sink.inner.lock().unwrap_or_else(|p| p.into_inner());
            assert!(
                state.durable_path.is_some(),
                "boot-time open failure must keep durable_path so reopen backoff and \
                 record_drop remain reachable (TASK-G64ZH.1 F-A)"
            );
            assert!(
                state.durable.is_none(),
                "boot open failed; durable handle must be absent"
            );
            assert!(
                matches!(state.mirror, MirrorState::StdoutWhenNoDurable),
                "suppressed launch with an intended durable sink must fall back to                  stdout while the handle is missing"
            );
        }

        let before = dropped_log_writes();
        {
            let mut writer = sink.make_writer();
            writer.write_all(b"boot-fail-silenced\n").unwrap();
            writer.write_all(b"boot-fail-still-silenced\n").unwrap();
        }
        let mid = dropped_log_writes();
        assert!(
            mid > before,
            "mirror-suppressed boot open failure must move dropped_log_writes \
             ({before} -> {mid})"
        );
        {
            let state = sink.inner.lock().unwrap_or_else(|p| p.into_inner());
            assert_eq!(
                state.open_attempts, 1,
                "first writes share one reopen attempt inside the backoff window \
                 (got {})",
                state.open_attempts
            );
            assert_eq!(
                state.lines_dropped, 2,
                "each dropped line must count once (got {})",
                state.lines_dropped
            );
        }

        // Make the path openable and expire the backoff so reopen recovers.
        std::fs::remove_dir(&path).unwrap();
        {
            let mut state = sink.inner.lock().unwrap_or_else(|p| p.into_inner());
            state.next_reopen_attempt = state.now();
        }
        {
            let mut writer = sink.make_writer();
            writer.write_all(b"recovered-after-boot-fail\n").unwrap();
            writer.flush().unwrap();
        }
        let contents = std::fs::read_to_string(&path).unwrap_or_default();
        assert!(
            contents.contains("recovered-after-boot-fail"),
            "1s->60s reopen backoff must retry a boot-time open failure once \
             the path is openable again: {contents:?}"
        );
    }

    /// orgasmic:TASK-G64ZH.1.1 R-1 — failed boot open with NO suppression and a
    /// non-tty stdout must still write every line to stdout (the eaa88da^
    /// behaviour). Pre-fix (round 2) path-keyed `resolve_mirror` saw
    /// `durable_path: Some` and silenced every non-terminal launch.
    #[test]
    fn failed_boot_open_with_stdout_mirror_non_tty_still_writes_stdout() {
        use std::os::unix::io::AsRawFd;

        let _lock = STDOUT_REDIRECT_LOCK
            .lock()
            .unwrap_or_else(|p| p.into_inner());
        let dir = tempfile::tempdir().unwrap();
        let durable_path = dir.path().join("daemon.out.log");
        let stdout_path = dir.path().join("capture.log");
        // EISDIR: boot open fails; path is retained.
        std::fs::create_dir(&durable_path).unwrap();
        let stdout_file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&stdout_path)
            .unwrap();

        let saved_fd = unsafe { libc::dup(libc::STDOUT_FILENO) };
        assert!(saved_fd >= 0, "dup stdout");
        assert!(
            unsafe { libc::dup2(stdout_file.as_raw_fd(), libc::STDOUT_FILENO) } >= 0,
            "dup2 stdout -> capture.log"
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

        let durable = resolve_durable_open(Some(&durable_path));
        // FIRST assertion under verify/TASK-G64ZH.1.1 injection: path-keyed
        // silence makes this go red with this exact message.
        let sink = BestEffortMakeWriter::new_with_terminal_gate(
            durable,
            LogMirror::Stdout,
            LogRotation::default(),
            false,
        );
        {
            let state = sink.inner.lock().unwrap_or_else(|p| p.into_inner());
            assert!(
                matches!(state.mirror, MirrorState::StdoutWhenNoDurable),
                "failed boot open with LogMirror::Stdout and non-tty stdout must \
                 keep a write-time fallback, not resolve to permanent silence \
                 (TASK-G64ZH.1.1 R-1)"
            );
        }
        {
            let mut writer = sink.make_writer();
            writer.write_all(b"r1-non-tty-boot-fail\n").unwrap();
            writer.write_all(b"r1-non-tty-boot-fail-2\n").unwrap();
            writer.flush().unwrap();
        }
        let _ = io::stdout().flush();
        let contents = std::fs::read_to_string(&stdout_path).unwrap();
        assert!(
            contents.contains("r1-non-tty-boot-fail"),
            "failed boot open with LogMirror::Stdout and non-tty stdout must still \
             write every line to stdout (TASK-G64ZH.1.1 R-1); got {contents:?}"
        );
        assert!(
            contents.contains("r1-non-tty-boot-fail-2"),
            "second line must also reach stdout under failed boot open: {contents:?}"
        );
    }

    /// orgasmic:TASK-G64ZH.1.1 R-2 — under suppression with a permanently
    /// unopenable path, tracing must still reach stdout (failure-window
    /// growth into daemon.stdout.log is accepted; steady-state double-write
    /// is not).
    #[test]
    fn permanent_unopenable_under_suppression_falls_back_to_stdout() {
        use std::os::unix::io::AsRawFd;

        let _lock = STDOUT_REDIRECT_LOCK
            .lock()
            .unwrap_or_else(|p| p.into_inner());
        let dir = tempfile::tempdir().unwrap();
        let durable_path = dir.path().join("daemon.out.log");
        let stdout_path = dir.path().join("daemon.stdout.log");
        std::fs::create_dir(&durable_path).unwrap();
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

        let durable = resolve_durable_open(Some(&durable_path));
        let sink = BestEffortMakeWriter::new(durable, LogMirror::None, LogRotation::default());
        let before = dropped_log_writes();
        {
            let mut writer = sink.make_writer();
            writer.write_all(b"r2-suppressed-permanent\n").unwrap();
            writer.flush().unwrap();
        }
        let _ = io::stdout().flush();
        let after = dropped_log_writes();
        assert!(
            after > before,
            "permanent unopenable under suppression must still move \
             dropped_log_writes ({before} -> {after})"
        );
        let contents = std::fs::read_to_string(&stdout_path).unwrap();
        assert!(
            contents.contains("r2-suppressed-permanent"),
            "under suppression with a permanently unopenable path, tracing must \
             still reach stdout (TASK-G64ZH.1.1 R-2); got {contents:?}"
        );
    }

    /// orgasmic:TASK-G64ZH.1.1 R-4 — reopen stderr is throttled to first failure
    /// / kind transition / first success, not one line per retry attempt.
    #[test]
    fn reopen_stderr_throttled_across_same_kind_retries() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("daemon.out.log");
        let sink = BestEffortMakeWriter::with_missing_durable(
            path,
            LogRotation {
                max_bytes: 0,
                keep: 2,
            },
        );
        // Seed says a failure was already noted (Other). Two more Other failures
        // must not clear the throttle — last_open_err_kind stays Some(Other).
        {
            let mut writer = sink.make_writer();
            writer.write_all(b"throttle-1\n").unwrap();
        }
        {
            let mut state = sink.inner.lock().unwrap_or_else(|p| p.into_inner());
            assert_eq!(state.open_attempts, 1);
            assert_eq!(state.last_open_err_kind, Some(io::ErrorKind::Other));
            // Expire backoff for a second attempt of the same kind.
            state.next_reopen_attempt = state.now();
            state.clock = Some(state.now());
        }
        {
            let mut writer = sink.make_writer();
            writer.write_all(b"throttle-2\n").unwrap();
        }
        let state = sink.inner.lock().unwrap_or_else(|p| p.into_inner());
        assert_eq!(state.open_attempts, 2);
        assert_eq!(
            state.last_open_err_kind,
            Some(io::ErrorKind::Other),
            "same-kind retry must keep the throttle marker so stderr is not \
             re-emitted per attempt"
        );
    }

    /// orgasmic:TASK-G64ZH.1 F-D — the TRUE side of the production terminal
    /// predicate, through the production constructor. Hardcoding
    /// `new_with_terminal_gate(..., false)` at `new` keeps every other test
    /// green and silences interactive `orgasmic serve`.
    #[test]
    fn production_new_keeps_stdout_mirror_when_stdout_is_pty() {
        let _lock = STDOUT_REDIRECT_LOCK
            .lock()
            .unwrap_or_else(|p| p.into_inner());

        let mut master = 0;
        let mut slave = 0;
        assert_eq!(
            unsafe {
                libc::openpty(
                    &mut master,
                    &mut slave,
                    std::ptr::null_mut(),
                    std::ptr::null_mut(),
                    std::ptr::null_mut(),
                )
            },
            0,
            "openpty"
        );

        struct RestorePty {
            saved: i32,
            master: i32,
            slave: i32,
        }
        impl Drop for RestorePty {
            fn drop(&mut self) {
                unsafe {
                    libc::dup2(self.saved, libc::STDOUT_FILENO);
                    libc::close(self.saved);
                    libc::close(self.master);
                    libc::close(self.slave);
                }
            }
        }
        let saved = unsafe { libc::dup(libc::STDOUT_FILENO) };
        assert!(saved >= 0, "dup stdout");
        assert!(
            unsafe { libc::dup2(slave, libc::STDOUT_FILENO) } >= 0,
            "dup2 pty slave onto stdout"
        );
        let _restore = RestorePty {
            saved,
            master,
            slave,
        };
        assert!(
            io::stdout().is_terminal(),
            "pty slave on fd 1 must make is_terminal() true"
        );

        let dir = tempfile::tempdir().unwrap();
        let durable_path = dir.path().join("daemon.out.log");
        let durable = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&durable_path)
            .unwrap();
        // PRODUCTION constructor — not new_with_terminal_gate with a literal.
        let sink = BestEffortMakeWriter::new(
            Some((durable_path, Ok(durable))),
            LogMirror::Stdout,
            LogRotation::default(),
        );
        let state = sink.inner.lock().unwrap_or_else(|p| p.into_inner());
        assert!(
            matches!(state.mirror, MirrorState::Stdout),
            "production new() must keep Stdout mirror when fd 1 is a pty; \
             hardcoding terminal=false at new() would silence interactive serve"
        );
    }

    /// orgasmic:TASK-G64ZH.1 F-E / TASK-G64ZH.1.1 R-3 — the backoff floor: after
    /// the window expires, a second reopen attempt must occur. The deadline is
    /// computed independently of state — reading `next_reopen_attempt` back and
    /// advancing the clock to it left a permanent `now()+86400s` backoff green.
    #[test]
    fn reopen_backoff_expires_and_allows_a_second_attempt() {
        assert_eq!(
            REOPEN_BACKOFF_CAP,
            Duration::from_secs(60),
            "REOPEN_BACKOFF_CAP must stay 60s"
        );
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("daemon.out.log");
        let sink = BestEffortMakeWriter::with_missing_durable(
            path,
            LogRotation {
                max_bytes: 0,
                keep: 2,
            },
        );
        let start = {
            let state = sink.inner.lock().unwrap_or_else(|p| p.into_inner());
            state.now()
        };
        {
            let mut writer = sink.make_writer();
            writer.write_all(b"first-window\n").unwrap();
        }
        {
            let state = sink.inner.lock().unwrap_or_else(|p| p.into_inner());
            assert_eq!(state.open_attempts, 1);
            assert!(
                state.next_reopen_attempt > start,
                "failed open must schedule a future retry"
            );
            assert!(
                state.next_reopen_attempt <= start + REOPEN_BACKOFF_CAP,
                "scheduled retry must be bounded by REOPEN_BACKOFF_CAP \
                 (got {:?} after start; a now()+86400s mutation must fail here)",
                state.next_reopen_attempt.saturating_duration_since(start)
            );
            assert_eq!(
                state.reopen_backoff,
                REOPEN_BACKOFF_INITIAL.saturating_mul(2),
                "backoff must double after the first failure"
            );
        }
        // Advance past the cap — independent of whatever state scheduled —
        // so a permanent now()+86400s backoff cannot sneak through.
        {
            let mut state = sink.inner.lock().unwrap_or_else(|p| p.into_inner());
            state.clock = Some(start + REOPEN_BACKOFF_CAP + Duration::from_millis(1));
        }
        {
            let mut writer = sink.make_writer();
            writer.write_all(b"second-window\n").unwrap();
        }
        let state = sink.inner.lock().unwrap_or_else(|p| p.into_inner());
        assert_eq!(
            state.open_attempts, 2,
            "after the backoff window expires a second reopen attempt must run \
             (got {}); a permanent backoff would leave this at 1",
            state.open_attempts
        );
    }

    /// Process-wide env mutation must not race other tests that touch
    /// [`LOG_MIRROR_ENV`].
    static LOG_MIRROR_ENV_LOCK: Mutex<()> = Mutex::new(());

    /// orgasmic:TASK-G64ZH.1 F-F — `requested_log_mirror` / `env_log_mirror_off`
    /// including the accepted value set. Load-bearing once service defs rely
    /// on the env form.
    #[test]
    fn requested_log_mirror_and_env_log_mirror_off_cover_accepted_values() {
        let _guard = LOG_MIRROR_ENV_LOCK
            .lock()
            .unwrap_or_else(|p| p.into_inner());
        let saved = std::env::var_os(LOG_MIRROR_ENV);
        struct Restore(Option<std::ffi::OsString>);
        impl Drop for Restore {
            fn drop(&mut self) {
                match &self.0 {
                    Some(v) => std::env::set_var(LOG_MIRROR_ENV, v),
                    None => std::env::remove_var(LOG_MIRROR_ENV),
                }
            }
        }
        let _restore = Restore(saved);

        std::env::remove_var(LOG_MIRROR_ENV);
        assert!(
            !env_log_mirror_off(),
            "unset {LOG_MIRROR_ENV} must not suppress"
        );
        assert!(
            matches!(requested_log_mirror(false), LogMirror::Stdout),
            "default request is Stdout"
        );
        assert!(
            matches!(requested_log_mirror(true), LogMirror::None),
            "CLI flag must suppress even when env is unset"
        );

        for value in ["off", "OFF", "0", "false", "False", " false "] {
            std::env::set_var(LOG_MIRROR_ENV, value);
            assert!(
                env_log_mirror_off(),
                "{LOG_MIRROR_ENV}={value:?} must suppress"
            );
            assert!(
                matches!(requested_log_mirror(false), LogMirror::None),
                "{LOG_MIRROR_ENV}={value:?} must yield LogMirror::None"
            );
        }
        for value in ["on", "1", "true", "stdout", ""] {
            std::env::set_var(LOG_MIRROR_ENV, value);
            assert!(
                !env_log_mirror_off(),
                "{LOG_MIRROR_ENV}={value:?} must not suppress"
            );
        }
        // Flag still wins when env would not suppress.
        std::env::set_var(LOG_MIRROR_ENV, "on");
        assert!(matches!(requested_log_mirror(true), LogMirror::None));
    }
}
