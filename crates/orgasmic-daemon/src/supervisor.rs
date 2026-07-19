// arch: arch_A53QX.1, arch_A53QX.5, arch_Z3Z3V.1, arch_Z3Z3V.2
// orgasmic:arch_A53QX, arch_Z3Z3V, dec_CSKBD
//! Run supervisor — owns the live driver session map and the
//! `(task_id, kind)` lease table.
//!
//! Invariants the supervisor enforces:
//!
//! - **AC #2**: at most one active run per `(task_id, RunKind)` tuple. A
//!   second `acquire` for the same key while a run is live returns
//!   [`SupervisorError::LeaseHeld`].
//! - **AC #3**: every driver event lands in a per-run JSONL through the
//!   serialized [`crate::writer::WriterHandle`], plus a `Lifecycle::Acquire`
//!   on start and a `Lifecycle::Release` on stop.
//! - **AC #4**: runtime ownership is the `(run_id, runtime_id, boot_id)`
//!   tuple (`arch_010`). The supervisor refuses to mutate state on a run
//!   whose caller identity tuple doesn't match the current run record (e.g.
//!   a stale handle left behind by a previous boot or replacement runtime).
//! - **AC #5**: babysitter runs always live in
//!   `sessions/<run-id>.babysitter.jsonl`, the supervisor coalesces
//!   implementer events into `BabysitterSummaryChunk` envelopes before the
//!   babysitter sees them, and the babysitter driver enforces the closed
//!   tool set (`BabysitterTool::ALL`).
//! - **AC #6**: `acquire_continuation` attaches the prior session JSONL
//!   path, the current worktree diff summary, and the original acceptance
//!   criteria into the new run's [`Lifecycle::Continuation`] envelope, and
//!   passes the same context to the driver.

use std::collections::HashMap;
use std::future::Future;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

use chrono::Utc;
use orgasmic_core::{
    compute_all, compute_eligibility, read_session_file, BabysitterSummaryChunk, BabysitterTool,
    DriverEvent, Lifecycle, ReleaseOutcome, RunSubState, RuntimeIdentity, SessionEnvelope,
    SessionEventKind, TaskConstraints, Worker, WorkerEligibility,
};
use orgasmic_drivers::{
    driver_for_mode_harness, AttachOutcome, ContinuationContext, DriverConfig, DriverContext,
    DriverControl, NativeRuntimeMeta, RunKind, RuntimeOptionsRequest, TransitionRequest,
    UserInputRequest, WorkerDriver,
};
use serde::{Deserialize, Serialize};
use thiserror::Error;
use tokio::sync::Mutex;
use tokio::time::Instant;
use tracing::warn;
use uuid::Uuid;

use crate::runtime::BootIdentity;
use crate::writer::{SessionAppend, WriterHandle};

static BABYSITTER_SPAWN_ATTEMPTS: AtomicU64 = AtomicU64::new(0);
static WATCHER_EVENTS_HANDLED: AtomicU64 = AtomicU64::new(0);
static SPAWN_PIPELINE_POLLS: AtomicU64 = AtomicU64::new(0);
static ARTIFACTOR_LIFECYCLE_TOKEN: AtomicU64 = AtomicU64::new(1);

const BABYSITTER_AUTO_SPAWN_MAX_RETRIES: u32 = 10;
const BABYSITTER_AUTO_SPAWN_INITIAL_BACKOFF: Duration = Duration::from_secs(1);
const BABYSITTER_AUTO_SPAWN_MAX_BACKOFF: Duration = Duration::from_secs(60);
const BABYSITTER_SUMMARY_EVENT_THRESHOLD: usize = 50;
const BABYSITTER_SUMMARY_INTERVAL: Duration = Duration::from_secs(60);
const DEFAULT_STALL_TIMEOUT: Duration = Duration::from_secs(600);
const DEFAULT_MAX_RUN_DURATION: Duration = Duration::from_secs(14_400);
/// Default idle window for persistent (hot-session) artifactor runs: 15
/// minutes of no accepted `send_input` before self-release. Long enough to
/// survive an operator reading a diff or drafting review feedback between
/// grilling/regenerate rounds; short enough that an abandoned hot session
/// doesn't hold its `artifact.generate:{id}` lease and pane indefinitely.
pub(crate) const DEFAULT_IDLE_TIMEOUT_SECS: u32 = 900;

/// Task-id prefix for interactive manager sessions (see
/// `post_manager_launch`). Manager runs are operator-paced — they idle at a
/// prompt waiting for a human — so the stall detector and run ceiling never
/// apply to them.
const MANAGER_TASK_PREFIX: &str = "manager.launch:";

pub(crate) fn is_interactive_manager_task(task_id: &str) -> bool {
    task_id.starts_with(MANAGER_TASK_PREFIX)
}

fn initial_working_sub_state(role: &str) -> Option<RunSubState> {
    RunSubState::new(format!("{}.working", role.trim())).ok()
}

const RUN_TIMEOUT_CHECK_INTERVAL: Duration = Duration::from_millis(50);

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SupervisorMetrics {
    pub babysitter_spawn_attempts: u64,
    pub watcher_events_handled: u64,
    pub spawn_pipeline_polls: u64,
}

pub fn supervisor_metrics() -> SupervisorMetrics {
    SupervisorMetrics {
        babysitter_spawn_attempts: BABYSITTER_SPAWN_ATTEMPTS.load(Ordering::Relaxed),
        watcher_events_handled: WATCHER_EVENTS_HANDLED.load(Ordering::Relaxed),
        spawn_pipeline_polls: SPAWN_PIPELINE_POLLS.load(Ordering::Relaxed),
    }
}

pub fn record_watcher_event_handled() {
    WATCHER_EVENTS_HANDLED.fetch_add(1, Ordering::Relaxed);
}

pub fn record_spawn_pipeline_poll() {
    SPAWN_PIPELINE_POLLS.fetch_add(1, Ordering::Relaxed);
}

fn record_babysitter_spawn_attempt() {
    BABYSITTER_SPAWN_ATTEMPTS.fetch_add(1, Ordering::Relaxed);
}

/// What a caller hands the supervisor to start a run.
#[derive(Debug, Clone)]
pub struct AcquireRequest {
    pub task_id: String,
    pub kind: RunKind,
    pub worker_id: String,
    /// The resolved worker's kind ("implementer", "reviewer", "babysitter",
    /// "manager", …) — the live role surfaced on [`RunSummary`]. RunKind only
    /// distinguishes worker from babysitter supervision; this names who is
    /// actually working.
    pub role: String,
    pub project_id: Option<String>,
    pub worktree: Option<PathBuf>,
    /// Dispatch artifact paths (`orgasmic dispatch` CLI-derived), carried
    /// through to the `RunMeta` lifecycle event so a boot reattach can
    /// respawn the dispatch completion watcher. `None` for non-dispatch
    /// acquires (manager launch, recovery, stage launch, babysitter).
    pub last_path: Option<PathBuf>,
    pub stdout_path: Option<PathBuf>,
    /// Full UUID attempt token minted by the CLI for this dispatch. Fences
    /// delayed cleanup against a newer live attempt (TASK-ZGT1X).
    pub dispatch_attempt_token: Option<String>,
    /// Where the per-run JSONL lives. The supervisor opens this through
    /// the daemon writer so concurrent runs don't race on the file
    /// descriptor.
    pub session_path: PathBuf,
    /// Driver-specific configuration. Forwarded to [`WorkerDriver::acquire`].
    pub driver_config: DriverConfig,
    /// Babysitter-only: the run this babysitter is observing. Must be
    /// `None` for `RunKind::Worker`.
    pub babysitter_target: Option<String>,
    /// Optional worker-level no-driver-event threshold in seconds.
    /// `Some(0)` disables the stall detector entirely — used for interactive,
    /// operator-paced runs (the manager) that legitimately idle at a prompt.
    pub stall_timeout_secs: Option<u32>,
    /// Optional worker-level absolute run ceiling in seconds. `Some(0)`
    /// disables the ceiling (interactive manager sessions outlive any sane
    /// worker bound).
    pub max_run_duration_secs: Option<u32>,
    /// Opt-in idle release window in seconds for persistent (hot-session)
    /// runs: if no `send_input` is accepted for this long, the run is
    /// released. Unlike `stall_timeout_secs`/`max_run_duration_secs`, this is
    /// disabled by default — `None` or `Some(0)` means no idle release.
    /// Only the persistent artifactor spawn path sets an explicit value;
    /// every other caller (one-shot dispatch, manager, reviewer, babysitter)
    /// must leave this `None`.
    pub idle_timeout_secs: Option<u32>,
    /// When set on an implementer acquire, spawn this babysitter worker after
    /// the implementer run is live.
    pub babysitter: Option<BabysitterAutoSpawn>,
}

/// Companion worker to spawn automatically after implementer acquire.
#[derive(Debug, Clone)]
pub struct BabysitterAutoSpawn {
    pub worker_id: String,
    pub mode: String,
    pub harness: String,
    pub driver_config: DriverConfig,
    pub stall_timeout_secs: Option<u32>,
    pub max_run_duration_secs: Option<u32>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AcquireResponse {
    pub run_id: String,
    pub identity: RuntimeIdentity,
    pub pid: Option<u32>,
}

/// Continuation seed: the prior run JSONL + diff + acceptance criteria.
#[derive(Debug, Clone)]
pub struct ContinuationSeed {
    pub previous_run: String,
    pub previous_session_path: PathBuf,
    pub diff_summary: String,
    pub acceptance_criteria: Vec<String>,
}

#[derive(Debug, Error)]
pub enum SupervisorError {
    #[error("lease held: task={task_id} kind={kind:?} run={run_id}")]
    LeaseHeld {
        task_id: String,
        kind: RunKind,
        run_id: String,
    },
    #[error("run not found: {0}")]
    RunNotFound(String),
    #[error(
        "runtime ownership mismatch: field={field} expected={expected} got={got} run_id={run_id}"
    )]
    OwnershipMismatch {
        run_id: String,
        field: &'static str,
        expected: String,
        got: String,
    },
    #[error("babysitter target invalid: {0}")]
    BabysitterTargetInvalid(String),
    #[error("reattach blocked: task={task_id} kind={kind:?} held by run={active_run_id}")]
    ReattachLeaseConflict {
        task_id: String,
        kind: RunKind,
        active_run_id: String,
    },
    #[error("run {run_id} cannot be reattached: {reason}")]
    NotReattachable { run_id: String, reason: String },
    #[error("driver: {0}")]
    Driver(#[from] orgasmic_drivers::DriverError),
    #[error("session write: {0}")]
    Session(#[from] anyhow::Error),
    #[error("run acquisition is paused for controlled restart")]
    AcquisitionPaused,
    /// Release deferred while an artifactor writer/regenerate acknowledgment
    /// is in flight — the record stays live until commit/abort/rollback
    /// resolves it (TASK-ARZGD / TASK-S52X9 round 3).
    #[error("release deferred while artifactor in-flight: {0}")]
    DeferredWhileInFlight(String),
    #[error("artifactor lifecycle busy: {0}")]
    ArtifactorLifecycleBusy(String),
    #[error("dispatch cleanup in progress for task={task_id} kind={kind:?} worktree={worktree}")]
    CleanupInProgress {
        task_id: String,
        kind: RunKind,
        worktree: String,
    },
}

#[derive(Clone)]
pub struct Supervisor {
    inner: Arc<Mutex<Inner>>,
    writer: WriterHandle,
    boot: Arc<BootIdentity>,
    /// Resolves the worktree diff summary for continuation runs. Injected
    /// so tests can stub it without invoking real `git`.
    diff_summarizer: Arc<dyn DiffSummarizer>,
}

struct Inner {
    acquisition_paused: bool,
    /// `(task_id, RunKind)` → run_id. Single-entry guard for AC #2.
    leases: HashMap<(String, RunKind), String>,
    /// run_id → live run record. Holds the driver control handle so the
    /// supervisor can call `release` later.
    runs: HashMap<String, RunRecord>,
    /// task_id → retry state after babysitter auto-spawn hits a stale
    /// `(task_id, Babysitter)` lease. Prevents dispatch churn from turning
    /// one stale babysitter lease into an immediate retry loop.
    babysitter_auto_spawn_backoff: HashMap<String, BabysitterAutoSpawnBackoff>,
    /// Active dispatch cleanup reservations held through filesystem mutation
    /// (TASK-1FV1N). Blocks reuse of the same default worktree path.
    cleanup_reservations: HashMap<CleanupReservationKey, DispatchCleanupReservation>,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct CleanupReservationKey {
    task_id: String,
    kind: RunKind,
    worktree_key: PathBuf,
}

#[derive(Debug, Clone)]
struct DispatchCleanupReservation {
    branch: String,
    worktree_path: PathBuf,
    dispatch_attempt_token: Option<String>,
    last_path: Option<PathBuf>,
    stdout_path: Option<PathBuf>,
}

/// Identity bundle for dispatch cleanup authorization (TASK-NW4WV).
#[derive(Debug, Clone)]
pub struct DispatchCleanupParams {
    pub task_id: String,
    pub kind: RunKind,
    pub branch: String,
    pub worktree_path: PathBuf,
    pub dispatch_attempt_token: Option<String>,
    pub last_path: Option<PathBuf>,
    pub stdout_path: Option<PathBuf>,
}

/// Durable dispatch attempt owner recovered from persisted session JSONL.
#[derive(Debug, Clone)]
struct DurableDispatchOwner {
    dispatch_attempt_token: Option<String>,
    last_path: Option<PathBuf>,
    stdout_path: Option<PathBuf>,
    worktree: Option<PathBuf>,
    recorded_at: chrono::DateTime<Utc>,
}

enum CleanupIdentityAuth {
    NoOwner,
    ExactOwner,
    IdentityMismatch,
    AuthorityUnavailable,
}

enum DurableScanError {
    UnreadableSessionsDir,
    UnreadableSessionFile(PathBuf),
}

/// Owns a lease reservation until the corresponding [`RunRecord`] is live.
///
/// Most acquisition failures can clean up explicitly, but cancellation can
/// drop an acquire future at any `.await`. Drop therefore removes the lease
/// synchronously when the supervisor lock is free (the normal case), or queues
/// the same conditional cleanup on the current runtime. The run-id check keeps
/// a stale guard from removing a newer holder's lease.
struct LeaseReservation {
    inner: Arc<Mutex<Inner>>,
    key: Option<(String, RunKind)>,
    run_id: String,
}

impl LeaseReservation {
    fn new(inner: Arc<Mutex<Inner>>, key: (String, RunKind), run_id: String) -> Self {
        Self {
            inner,
            key: Some(key),
            run_id,
        }
    }

    fn commit(&mut self) {
        self.key = None;
    }

    fn remove_if_unowned(inner: &mut Inner, key: &(String, RunKind), run_id: &str) {
        let reserved_by_this_run = inner.leases.get(key).is_some_and(|held| held == run_id);
        if reserved_by_this_run && !inner.runs.contains_key(run_id) {
            inner.leases.remove(key);
        }
    }
}

impl Drop for LeaseReservation {
    fn drop(&mut self) {
        let Some(key) = self.key.take() else {
            return;
        };
        if let Ok(mut inner) = self.inner.try_lock() {
            Self::remove_if_unowned(&mut inner, &key, &self.run_id);
            return;
        }

        let inner = Arc::clone(&self.inner);
        let run_id = self.run_id.clone();
        if let Ok(runtime) = tokio::runtime::Handle::try_current() {
            runtime.spawn(async move {
                let mut inner = inner.lock().await;
                Self::remove_if_unowned(&mut inner, &key, &run_id);
            });
        }
    }
}

struct RunRecord {
    task_id: String,
    kind: RunKind,
    worker_id: String,
    role: String,
    transport: String,
    harness: Option<String>,
    project_id: Option<String>,
    /// The dispatched worktree root, when known (CLI dispatch acquire/reattach;
    /// `None` for manager/recovery/stage/babysitter runs). Exposed on
    /// [`RunSummary`] so `orgasmic dispatch finalize --commit` can refuse to
    /// commit a git root that isn't the dispatched worktree (TASK-QKQ3R).
    worktree: Option<PathBuf>,
    sub_state: Option<RunSubState>,
    identity: RuntimeIdentity,
    session_path: PathBuf,
    babysitter_target: Option<String>,
    /// Dispatch artifact paths (`orgasmic dispatch` CLI-derived), mirroring
    /// [`AcquireRequest::last_path`]/[`AcquireRequest::stdout_path`]. Exposed
    /// on [`RunSummary`] so `orgasmic dispatch finalize` can resolve the
    /// exact report path for the current run without scanning `.orgasmic/tx`
    /// (which a worker's worktree checkout cannot see live daemon writes to).
    /// `None` for non-dispatch runs and for reattached runs (no `AcquireRequest`
    /// carries them across a reattach; `set_dispatch_artifact_paths` backfills
    /// them for boot reattach when the persisted `RunMeta` event has them).
    last_path: Option<PathBuf>,
    stdout_path: Option<PathBuf>,
    dispatch_attempt_token: Option<String>,
    /// When true, this run must end with an explicit worker-declared terminal
    /// call writing the finalize tombstone (`finalized_by_worker` + reason).
    /// Protocol-end alone must not count as success (dec_WDR5K item 6 /
    /// TASK-S52X9). Set at acquire for every shape that declares termination:
    /// dispatch workers + stage grill/plan (via `dispatch finalize`),
    /// artifactor (`artifact submit`), manager (`manager release`).
    requires_worker_finalize: bool,
    /// Monotonic artifactor regenerate counter. A terminal declaration is
    /// valid only when its `round` matches this value (TASK-S52X9 / TASK-TZJFF).
    terminal_round: u64,
    /// Set when a shape's terminal verb has declared success without yet
    /// releasing the lease (hot-session artifactor submit). Stream-end,
    /// idle release, and TUI terminal events promote this into the finalize
    /// tombstone when `round == terminal_round`.
    terminal_declaration: Option<TerminalDeclaration>,
    /// Exactly one artifactor lifecycle transaction may be active at a time.
    /// Submit covers the durable writer transaction; regenerate covers the
    /// driver acknowledgement window (TASK-Y5K2C).
    artifactor_lifecycle: ArtifactorLifecycle,
    /// Stream-end, TUI terminal event, or timeout deferred while
    /// `artifactor_lifecycle` is not idle — resolved only
    /// after the writer/ack outcome.
    pending_terminal_drain: bool,
    /// Operator cancel deferred while an artifactor writer/regenerate is
    /// in flight — after commit/abort/rollback, release as Cancelled
    /// (TASK-ARZGD OQ2).
    pending_cancel: bool,
    /// On implementer runs only: companion babysitter run_id set by auto-spawn.
    babysitter_run_id: Option<String>,
    last_driver_event_at: Instant,
    run_started_at: Instant,
    /// Instant of the last `send_input` accepted by the driver. Reset on
    /// every accepted `send_input` — unlike `last_driver_event_at`, which
    /// resets on any driver event. Initialized to `run_started_at`.
    last_input_at: Instant,
    /// `None` = stall detection disabled (interactive manager runs).
    stall_timeout: Option<Duration>,
    /// `None` = no absolute ceiling (interactive manager runs).
    max_run_duration: Option<Duration>,
    /// `None` = idle-release disabled (default for everything except
    /// persistent artifactor runs).
    idle_timeout: Option<Duration>,
    next_event_seq: u64,
    terminal_outcome: Option<ReleaseOutcome>,
    control: Box<dyn DriverControl>,
    event_drain: tokio::task::JoinHandle<()>,
    /// Babysitter coalescing buffer (None on implementer runs).
    babysitter_summary: Option<BabysitterSummaryBuffer>,
}

struct BabysitterAutoSpawnBackoff {
    attempts: u32,
    next_retry: Instant,
    gave_up_logged: bool,
}

struct ReleasedRun {
    kind: RunKind,
    babysitter_run_id: Option<String>,
}

#[derive(Clone, Copy, Debug)]
struct TerminalDeclaration {
    reason: &'static str,
    round: u64,
}

#[derive(Clone, Copy, Debug)]
struct SubmitInFlight {
    round: u64,
    token: u64,
}

#[derive(Clone, Copy, Debug)]
enum ArtifactorLifecycle {
    Idle,
    Submit(SubmitInFlight),
    Regenerate(ArtifactorRegenerateCheckpoint),
}

/// Snapshot taken before advancing an artifactor regenerate round so a
/// rejected follow-up can restore the prior declaration (TASK-99W9C).
#[derive(Clone, Copy, Debug)]
pub struct ArtifactorRegenerateCheckpoint {
    terminal_round: u64,
    terminal_declaration: Option<TerminalDeclaration>,
    token: u64,
}

struct ResolvedTerminalRelease {
    reason: String,
    outcome: ReleaseOutcome,
    finalized_by_worker: bool,
}

enum TerminalReleaseSource {
    StreamEnd,
    TerminalEvent,
}

struct TerminalRelease {
    run_id: String,
    session_path: PathBuf,
    identity: RuntimeIdentity,
    reason: String,
    outcome: ReleaseOutcome,
    finalized_by_worker: bool,
    control: Box<dyn DriverControl>,
}

struct RunTimeoutCandidate {
    run_id: String,
    reason: &'static str,
    threshold: Duration,
    elapsed: Duration,
    deadline: Instant,
}

struct PendingBabysitterSummary {
    run_id: String,
    session_path: PathBuf,
    identity: RuntimeIdentity,
    chunk: BabysitterSummaryChunk,
}

#[derive(Default)]
struct BabysitterSummaryBuffer {
    window_started_at: Option<Instant>,
    window_start_seq: u64,
    window_end_seq: u64,
    count: usize,
    headline: String,
    last_text: String,
    tool_calls: Vec<String>,
}

pub trait DiffSummarizer: Send + Sync {
    fn summarize(&self, worktree: Option<&Path>) -> String;
}

/// Default summarizer: runs `git diff --stat HEAD` in the worktree. If git
/// is missing or returns nonzero, emits an empty summary.
pub struct GitDiffSummarizer;

impl DiffSummarizer for GitDiffSummarizer {
    fn summarize(&self, worktree: Option<&Path>) -> String {
        let cwd = worktree.unwrap_or_else(|| Path::new("."));
        let out = Command::new("git")
            .arg("diff")
            .arg("--stat")
            .arg("HEAD")
            .current_dir(cwd)
            .output();
        match out {
            Ok(o) if o.status.success() => String::from_utf8_lossy(&o.stdout).trim().to_string(),
            _ => String::new(),
        }
    }
}

/// Test-only summarizer that returns a fixed string.
pub struct StaticDiffSummarizer(pub String);

impl DiffSummarizer for StaticDiffSummarizer {
    fn summarize(&self, _worktree: Option<&Path>) -> String {
        self.0.clone()
    }
}

pub fn find_eligible_workers(
    task: &TaskConstraints<'_>,
    workers: &[Worker<'_>],
) -> Vec<WorkerEligibility> {
    compute_all(task, workers)
}

pub fn auto_select_worker<'a>(
    task: &TaskConstraints<'_>,
    workers: &'a [Worker<'a>],
) -> Option<&'a Worker<'a>> {
    let mut first_eligible = None;
    for worker in workers {
        if !compute_eligibility(task, worker).eligible {
            continue;
        }
        if worker.default_provider.is_some()
            && worker.default_model.is_some()
            && worker.default_effort.is_some()
        {
            return Some(worker);
        }
        if first_eligible.is_none() {
            first_eligible = Some(worker);
        }
    }
    first_eligible
}

impl Supervisor {
    pub fn new(writer: WriterHandle, boot: Arc<BootIdentity>) -> Self {
        Self::with_summarizer(writer, boot, Arc::new(GitDiffSummarizer))
    }

    pub fn with_summarizer(
        writer: WriterHandle,
        boot: Arc<BootIdentity>,
        diff_summarizer: Arc<dyn DiffSummarizer>,
    ) -> Self {
        let supervisor = Self {
            inner: Arc::new(Mutex::new(Inner {
                acquisition_paused: false,
                leases: HashMap::new(),
                runs: HashMap::new(),
                babysitter_auto_spawn_backoff: HashMap::new(),
                cleanup_reservations: HashMap::new(),
            })),
            writer,
            boot,
            diff_summarizer,
        };
        spawn_run_timeout_monitor(supervisor.clone());
        supervisor
    }

    /// Acquire a new run.
    ///
    /// AC #2 lease check is exclusive: a second `acquire` for the same
    /// `(task_id, kind)` returns [`SupervisorError::LeaseHeld`] until the
    /// first run releases. The run_id of the live holder is in the error
    /// payload so the caller can attach instead of retrying.
    pub async fn acquire(
        &self,
        driver: &dyn WorkerDriver,
        req: AcquireRequest,
    ) -> Result<AcquireResponse, SupervisorError> {
        let babysitter = req.babysitter.clone();
        let task_id_for_babysitter = req.task_id.clone();
        let auto_spawn_babysitter = req.kind == RunKind::Worker;
        let sessions_dir = req
            .session_path
            .parent()
            .map(Path::to_path_buf)
            .unwrap_or_else(|| PathBuf::from("."));
        let resp = self.acquire_impl(driver, req).await?;
        if auto_spawn_babysitter {
            if let Some(bs) = babysitter {
                if !self
                    .should_attempt_babysitter_auto_spawn(&task_id_for_babysitter)
                    .await
                {
                    return Ok(resp);
                }
                if let Some(bs_driver) = driver_for_mode_harness(&bs.mode, &bs.harness) {
                    record_babysitter_spawn_attempt();
                    match self
                        .spawn_babysitter(
                            bs_driver.as_ref(),
                            &resp.run_id,
                            &sessions_dir,
                            &bs.worker_id,
                            bs.driver_config,
                            (bs.stall_timeout_secs, bs.max_run_duration_secs),
                        )
                        .await
                    {
                        Ok(bs_resp) => {
                            self.clear_babysitter_auto_spawn_backoff(&task_id_for_babysitter)
                                .await;
                            let mut g = self.inner.lock().await;
                            if let Some(rec) = g.runs.get_mut(&resp.run_id) {
                                rec.babysitter_run_id = Some(bs_resp.run_id);
                            }
                        }
                        Err(e) => {
                            if matches!(e, SupervisorError::LeaseHeld { .. }) {
                                self.record_babysitter_auto_spawn_lease_held(
                                    &task_id_for_babysitter,
                                    &resp.run_id,
                                    &e,
                                )
                                .await;
                            } else {
                                warn!(error = %e, run_id = %resp.run_id, "babysitter auto-spawn failed");
                            }
                        }
                    }
                } else {
                    warn!(
                        run_id = %resp.run_id,
                        mode = %bs.mode,
                        harness = %bs.harness,
                        "babysitter auto-spawn skipped: unsupported driver/harness pair"
                    );
                }
            }
        }
        Ok(resp)
    }

    async fn should_attempt_babysitter_auto_spawn(&self, task_id: &str) -> bool {
        let now = Instant::now();
        let mut g = self.inner.lock().await;
        let Some(state) = g.babysitter_auto_spawn_backoff.get_mut(task_id) else {
            return true;
        };
        if state.attempts >= BABYSITTER_AUTO_SPAWN_MAX_RETRIES {
            if !state.gave_up_logged {
                state.gave_up_logged = true;
                warn!(
                    task_id,
                    attempts = state.attempts,
                    "babysitter auto-spawn paused after repeated lease-held failures; will resume after babysitter lease release"
                );
            }
            return false;
        }
        now >= state.next_retry
    }

    async fn clear_babysitter_auto_spawn_backoff(&self, task_id: &str) {
        self.inner
            .lock()
            .await
            .babysitter_auto_spawn_backoff
            .remove(task_id);
    }

    #[cfg(test)]
    async fn babysitter_auto_spawn_attempts_for_test(&self, task_id: &str) -> u32 {
        self.inner
            .lock()
            .await
            .babysitter_auto_spawn_backoff
            .get(task_id)
            .map(|state| state.attempts)
            .unwrap_or(0)
    }

    #[cfg(test)]
    async fn force_babysitter_auto_spawn_retry_for_test(&self, task_id: &str) {
        if let Some(state) = self
            .inner
            .lock()
            .await
            .babysitter_auto_spawn_backoff
            .get_mut(task_id)
        {
            state.next_retry = Instant::now();
        }
    }

    async fn record_babysitter_auto_spawn_lease_held(
        &self,
        task_id: &str,
        run_id: &str,
        error: &SupervisorError,
    ) {
        let mut g = self.inner.lock().await;
        let state = g
            .babysitter_auto_spawn_backoff
            .entry(task_id.to_string())
            .or_insert_with(|| BabysitterAutoSpawnBackoff {
                attempts: 0,
                next_retry: Instant::now(),
                gave_up_logged: false,
            });
        state.attempts += 1;
        let delay = babysitter_auto_spawn_backoff_delay(state.attempts);
        state.next_retry = Instant::now() + delay;
        if state.attempts >= BABYSITTER_AUTO_SPAWN_MAX_RETRIES {
            if !state.gave_up_logged {
                state.gave_up_logged = true;
                warn!(
                    task_id,
                    run_id,
                    attempts = state.attempts,
                    error = %error,
                    "babysitter auto-spawn paused after repeated lease-held failures; will resume after babysitter lease release"
                );
            }
        } else {
            tracing::debug!(
                task_id,
                run_id,
                attempts = state.attempts,
                retry_after_ms = delay.as_millis(),
                error = %error,
                "babysitter auto-spawn backed off after lease-held failure"
            );
        }
    }

    async fn acquire_impl(
        &self,
        driver: &dyn WorkerDriver,
        req: AcquireRequest,
    ) -> Result<AcquireResponse, SupervisorError> {
        if req.kind == RunKind::Worker && req.babysitter_target.is_some() {
            return Err(SupervisorError::BabysitterTargetInvalid(
                "worker runs cannot carry babysitter_target".into(),
            ));
        }
        if req.kind == RunKind::Babysitter && req.babysitter_target.is_none() {
            return Err(SupervisorError::BabysitterTargetInvalid(
                "babysitter runs require babysitter_target".into(),
            ));
        }

        // Lease enforcement (AC #2). We hold the lock only long enough to
        // reserve the slot — the actual driver spawn is awaited without
        // the lock so a slow driver doesn't block other runs.
        let run_id = make_run_id(&req.kind);
        let lease_key = (req.task_id.clone(), req.kind);
        {
            let mut guard = self.inner.lock().await;
            if guard.acquisition_paused {
                return Err(SupervisorError::AcquisitionPaused);
            }
            if let Some(existing) = guard.leases.get(&lease_key) {
                return Err(SupervisorError::LeaseHeld {
                    task_id: req.task_id.clone(),
                    kind: req.kind,
                    run_id: existing.clone(),
                });
            }
            if let Some(worktree) = req.worktree.as_ref() {
                let reservation_key = CleanupReservationKey {
                    task_id: req.task_id.clone(),
                    kind: req.kind,
                    worktree_key: normalize_cleanup_worktree(worktree),
                };
                if guard.cleanup_reservations.contains_key(&reservation_key) {
                    return Err(SupervisorError::CleanupInProgress {
                        task_id: req.task_id.clone(),
                        kind: req.kind,
                        worktree: worktree.display().to_string(),
                    });
                }
            }
            guard.leases.insert(lease_key.clone(), run_id.clone());
        }
        let mut lease = LeaseReservation::new(self.inner.clone(), lease_key, run_id.clone());

        // Build the driver context and spawn. If the driver fails, release
        // the lease before returning.
        let identity = RuntimeIdentity::new(&run_id, &self.boot.boot_id);
        let ctx = DriverContext {
            identity: identity.clone(),
            run_kind: req.kind,
            task_id: req.task_id.clone(),
            worker_id: req.worker_id.clone(),
            project_id: req.project_id.clone(),
            worktree: req.worktree.clone(),
            babysitter_target: req.babysitter_target.clone(),
            continuation: None,
        };
        let transport = driver.transport().to_string();
        let harness = driver.harness().map(str::to_string);
        let session = match driver.acquire(ctx, req.driver_config.clone()).await {
            Ok(s) => s,
            Err(e) => return Err(SupervisorError::Driver(e)),
        };
        let pid = session.pid;

        // AC #3: write the Acquire lifecycle envelope before any driver
        // event so the JSONL stream starts with a known marker.
        let acquire_evt = Lifecycle::Acquire {
            task_id: req.task_id.clone(),
            kind: match req.kind {
                RunKind::Worker => "worker".to_string(),
                RunKind::Babysitter => "babysitter".to_string(),
            },
            worker_id: req.worker_id.clone(),
        };
        self.writer
            .append_session(SessionAppend {
                run_id: run_id.clone(),
                session_path: req.session_path.clone(),
                identity: identity.clone(),
                kind: SessionEventKind::Lifecycle,
                event: serde_json::to_value(&acquire_evt).map_err(into_anyhow)?,
            })
            .await
            .map_err(SupervisorError::Session)?;

        // Persist reattach metadata so a future daemon boot can rehydrate this
        // run against its still-live mux session (boot auto-reattach).
        self.write_run_meta(
            &run_id,
            &req.session_path,
            &identity,
            &transport,
            harness.clone(),
            req.project_id.clone(),
            req.worktree.clone(),
            req.last_path.clone(),
            req.stdout_path.clone(),
            req.dispatch_attempt_token.clone(),
            req.role.clone(),
            run_requires_worker_finalize(&req.last_path, &req.role),
            req.driver_config.clone(),
        )
        .await?;

        // Record harness-aware native runtime identity (dec_052) when the
        // driver knows it, so recovery can later resume/fork the native
        // session deterministically.
        if let Some(native) = session.native_runtime.clone() {
            self.write_native_runtime(&run_id, &req.session_path, &identity, native)
                .await?;
        }

        // Drain driver events into the JSONL session in a background task.
        let writer = self.writer.clone();
        let session_path = req.session_path.clone();
        let inner_for_drain = self.inner.clone();
        let run_id_for_drain = run_id.clone();
        let identity_for_drain = identity.clone();
        let kind = req.kind;
        let drain = tokio::spawn(async move {
            let mut events = session.events;
            while let Some(evt) = events.recv().await {
                let payload = match serde_json::to_value(&evt) {
                    Ok(v) => v,
                    Err(e) => {
                        warn!(error = %e, "driver event serialize failed");
                        continue;
                    }
                };
                let terminal_outcome = terminal_outcome_for_event(&evt);
                let event_at = Instant::now();
                {
                    let mut g = inner_for_drain.lock().await;
                    if let Some(rec) = g.runs.get_mut(&run_id_for_drain) {
                        apply_driver_event_to_record(rec, &evt, event_at, terminal_outcome);
                    }
                }
                if let Err(e) = writer
                    .append_session(SessionAppend {
                        run_id: run_id_for_drain.clone(),
                        session_path: session_path.clone(),
                        identity: identity_for_drain.clone(),
                        kind: SessionEventKind::DriverEvent,
                        event: payload,
                    })
                    .await
                {
                    warn!(error = %e, run_id = %run_id_for_drain, "session append failed");
                }
                // Bump the per-run sequence cursor and update the babysitter
                // summary buffer if applicable.
                let (pending_babysitter_summary, terminal_release) = {
                    let mut g = inner_for_drain.lock().await;
                    let mut flush_to_babysitter: Option<String> = None;
                    if let Some(rec) = g.runs.get_mut(&run_id_for_drain) {
                        let seq = rec.next_event_seq;
                        rec.next_event_seq += 1;
                        if rec.kind == RunKind::Worker {
                            if let Some(buf) = rec.babysitter_summary.as_mut() {
                                update_babysitter_buffer(buf, &evt, seq, event_at);
                                if should_flush_babysitter_buffer(buf, event_at) {
                                    flush_to_babysitter = rec.babysitter_run_id.clone();
                                }
                            }
                        }
                    }
                    let pending_summary =
                        flush_to_babysitter.as_deref().and_then(|babysitter_run| {
                            match take_babysitter_summary_locked(
                                &mut g,
                                &run_id_for_drain,
                                babysitter_run,
                            ) {
                                Ok(summary) => summary,
                                Err(e) => {
                                    warn!(
                                        error = %e,
                                        run_id = %run_id_for_drain,
                                        babysitter_run,
                                        "babysitter summary flush failed"
                                    );
                                    None
                                }
                            }
                        });
                    let terminal_release = if terminal_outcome.is_some() {
                        take_driver_terminal_release(&mut g, &run_id_for_drain)
                    } else {
                        None
                    };
                    (pending_summary, terminal_release)
                };
                if let Some(summary) = pending_babysitter_summary {
                    if let Err(e) = writer
                        .append_session(SessionAppend {
                            run_id: summary.run_id,
                            session_path: summary.session_path,
                            identity: summary.identity,
                            kind: SessionEventKind::BabysitterSummary,
                            event: serde_json::to_value(&summary.chunk)
                                .unwrap_or(serde_json::Value::Null),
                        })
                        .await
                    {
                        warn!(error = %e, "babysitter summary append failed");
                    }
                }
                if let Some(release) = terminal_release {
                    finish_driver_terminal_release(&writer, release).await;
                }
            }
            // Stream end: driver dropped its sender without an explicit
            // finalize/release. Claim the run atomically under the lock so a
            // concurrent `release_with_finalization` (worker finalize,
            // TASK-P4MGK / dec_WDR5K) cannot interleave a second lease
            // release or a second Lifecycle::Release write.
            finish_stream_end_terminal_drain(&writer, &inner_for_drain, &run_id_for_drain).await;
        });

        let run_started_at = Instant::now();
        let record = RunRecord {
            task_id: req.task_id.clone(),
            kind,
            worker_id: req.worker_id.clone(),
            role: req.role.clone(),
            transport,
            harness,
            project_id: req.project_id.clone(),
            worktree: req.worktree.clone(),
            sub_state: initial_working_sub_state(&req.role),
            identity: identity.clone(),
            session_path: req.session_path.clone(),
            babysitter_target: req.babysitter_target.clone(),
            last_path: req.last_path.clone(),
            stdout_path: req.stdout_path.clone(),
            dispatch_attempt_token: req.dispatch_attempt_token.clone(),
            requires_worker_finalize: run_requires_worker_finalize(&req.last_path, &req.role),
            terminal_round: 0,
            terminal_declaration: None,
            artifactor_lifecycle: ArtifactorLifecycle::Idle,
            pending_terminal_drain: false,
            pending_cancel: false,
            babysitter_run_id: None,
            last_driver_event_at: run_started_at,
            run_started_at,
            last_input_at: run_started_at,
            stall_timeout: resolve_timeout_secs(req.stall_timeout_secs, DEFAULT_STALL_TIMEOUT),
            max_run_duration: resolve_timeout_secs(
                req.max_run_duration_secs,
                DEFAULT_MAX_RUN_DURATION,
            ),
            idle_timeout: resolve_idle_timeout_secs(req.idle_timeout_secs),
            next_event_seq: 0,
            terminal_outcome: None,
            control: session.control,
            event_drain: drain,
            babysitter_summary: if kind == RunKind::Worker {
                Some(BabysitterSummaryBuffer::default())
            } else {
                None
            },
        };
        let mut g = self.inner.lock().await;
        g.runs.insert(run_id.clone(), record);
        lease.commit();
        if let Some(pid) = pid {
            spawn_early_exit_watcher(self.clone(), run_id.clone(), pid);
        }

        Ok(AcquireResponse {
            run_id,
            identity,
            pid,
        })
    }

    /// Convenience wrapper for continuation runs (AC #6). Writes the
    /// continuation lifecycle envelope after acquire and seeds the driver
    /// context.
    pub async fn acquire_continuation(
        &self,
        driver: &dyn WorkerDriver,
        mut req: AcquireRequest,
        seed: ContinuationSeed,
    ) -> Result<AcquireResponse, SupervisorError> {
        let diff = if seed.diff_summary.is_empty() {
            self.diff_summarizer.summarize(req.worktree.as_deref())
        } else {
            seed.diff_summary.clone()
        };
        let cont = ContinuationContext {
            previous_run: seed.previous_run.clone(),
            previous_session_path: seed.previous_session_path.clone(),
            diff_summary: diff.clone(),
            acceptance_criteria: seed.acceptance_criteria.clone(),
        };

        // Inject continuation into driver config via DriverContext.continuation
        // by going through a shimmed acquire path. We can't piggy-back on the
        // public acquire because it builds its own DriverContext, so re-do
        // the spawn here keeping all other invariants.
        if req.kind == RunKind::Worker && req.babysitter_target.is_some() {
            return Err(SupervisorError::BabysitterTargetInvalid(
                "worker continuation cannot carry babysitter_target".into(),
            ));
        }
        let run_id = make_run_id(&req.kind);
        let lease_key = (req.task_id.clone(), req.kind);
        {
            let mut guard = self.inner.lock().await;
            if guard.acquisition_paused {
                return Err(SupervisorError::AcquisitionPaused);
            }
            if let Some(existing) = guard.leases.get(&lease_key) {
                return Err(SupervisorError::LeaseHeld {
                    task_id: req.task_id.clone(),
                    kind: req.kind,
                    run_id: existing.clone(),
                });
            }
            guard.leases.insert(lease_key.clone(), run_id.clone());
        }
        let mut lease = LeaseReservation::new(self.inner.clone(), lease_key, run_id.clone());
        let identity = RuntimeIdentity::new(&run_id, &self.boot.boot_id);
        let ctx = DriverContext {
            identity: identity.clone(),
            run_kind: req.kind,
            task_id: req.task_id.clone(),
            worker_id: req.worker_id.clone(),
            project_id: req.project_id.clone(),
            worktree: req.worktree.clone(),
            babysitter_target: req.babysitter_target.clone(),
            continuation: Some(cont),
        };
        let transport = driver.transport().to_string();
        let harness = driver.harness().map(str::to_string);
        let session = match driver.acquire(ctx, req.driver_config.clone()).await {
            Ok(s) => s,
            Err(e) => return Err(SupervisorError::Driver(e)),
        };
        let pid = session.pid;

        // Acquire + Continuation envelopes.
        let acquire_evt = Lifecycle::Acquire {
            task_id: req.task_id.clone(),
            kind: match req.kind {
                RunKind::Worker => "worker".into(),
                RunKind::Babysitter => "babysitter".into(),
            },
            worker_id: req.worker_id.clone(),
        };
        self.writer
            .append_session(SessionAppend {
                run_id: run_id.clone(),
                session_path: req.session_path.clone(),
                identity: identity.clone(),
                kind: SessionEventKind::Lifecycle,
                event: serde_json::to_value(&acquire_evt).map_err(into_anyhow)?,
            })
            .await
            .map_err(SupervisorError::Session)?;
        let cont_evt = Lifecycle::Continuation {
            previous_run: seed.previous_run.clone(),
            previous_session_path: seed.previous_session_path.clone(),
            diff_summary: diff,
            acceptance_criteria: seed.acceptance_criteria.clone(),
        };
        self.writer
            .append_session(SessionAppend {
                run_id: run_id.clone(),
                session_path: req.session_path.clone(),
                identity: identity.clone(),
                kind: SessionEventKind::Lifecycle,
                event: serde_json::to_value(&cont_evt).map_err(into_anyhow)?,
            })
            .await
            .map_err(SupervisorError::Session)?;

        // Persist reattach metadata (boot auto-reattach), as in `acquire_impl`.
        self.write_run_meta(
            &run_id,
            &req.session_path,
            &identity,
            &transport,
            harness.clone(),
            req.project_id.clone(),
            req.worktree.clone(),
            req.last_path.clone(),
            req.stdout_path.clone(),
            req.dispatch_attempt_token.clone(),
            req.role.clone(),
            run_requires_worker_finalize(&req.last_path, &req.role),
            req.driver_config.clone(),
        )
        .await?;

        if let Some(native) = session.native_runtime.clone() {
            self.write_native_runtime(&run_id, &req.session_path, &identity, native)
                .await?;
        }

        let writer = self.writer.clone();
        let session_path = req.session_path.clone();
        let inner_for_drain = self.inner.clone();
        let run_id_for_drain = run_id.clone();
        let identity_for_drain = identity.clone();
        let kind = req.kind;
        let drain = tokio::spawn(async move {
            let mut events = session.events;
            while let Some(evt) = events.recv().await {
                let payload = match serde_json::to_value(&evt) {
                    Ok(v) => v,
                    Err(e) => {
                        warn!(error = %e, "driver event serialize failed");
                        continue;
                    }
                };
                let terminal_outcome = terminal_outcome_for_event(&evt);
                let event_at = Instant::now();
                {
                    let mut g = inner_for_drain.lock().await;
                    if let Some(rec) = g.runs.get_mut(&run_id_for_drain) {
                        apply_driver_event_to_record(rec, &evt, event_at, terminal_outcome);
                    }
                }
                if let Err(e) = writer
                    .append_session(SessionAppend {
                        run_id: run_id_for_drain.clone(),
                        session_path: session_path.clone(),
                        identity: identity_for_drain.clone(),
                        kind: SessionEventKind::DriverEvent,
                        event: payload,
                    })
                    .await
                {
                    warn!(error = %e, run_id = %run_id_for_drain, "session append failed");
                }
                let terminal_release = {
                    let mut g = inner_for_drain.lock().await;
                    if let Some(rec) = g.runs.get_mut(&run_id_for_drain) {
                        rec.next_event_seq += 1;
                    }
                    if terminal_outcome.is_some() {
                        take_driver_terminal_release(&mut g, &run_id_for_drain)
                    } else {
                        None
                    }
                };
                if let Some(release) = terminal_release {
                    finish_driver_terminal_release(&writer, release).await;
                }
            }
            finish_stream_end_terminal_drain(&writer, &inner_for_drain, &run_id_for_drain).await;
        });

        let run_started_at = Instant::now();
        let record = RunRecord {
            task_id: req.task_id.clone(),
            kind,
            worker_id: req.worker_id.clone(),
            role: req.role.clone(),
            transport,
            harness,
            project_id: req.project_id.clone(),
            worktree: req.worktree.clone(),
            sub_state: initial_working_sub_state(&req.role),
            identity: identity.clone(),
            session_path: std::mem::take(&mut req.session_path),
            babysitter_target: req.babysitter_target.clone(),
            last_path: req.last_path.clone(),
            stdout_path: req.stdout_path.clone(),
            dispatch_attempt_token: req.dispatch_attempt_token.clone(),
            requires_worker_finalize: run_requires_worker_finalize(&req.last_path, &req.role),
            terminal_round: 0,
            terminal_declaration: None,
            artifactor_lifecycle: ArtifactorLifecycle::Idle,
            pending_terminal_drain: false,
            pending_cancel: false,
            babysitter_run_id: None,
            last_driver_event_at: run_started_at,
            run_started_at,
            last_input_at: run_started_at,
            stall_timeout: resolve_timeout_secs(req.stall_timeout_secs, DEFAULT_STALL_TIMEOUT),
            max_run_duration: resolve_timeout_secs(
                req.max_run_duration_secs,
                DEFAULT_MAX_RUN_DURATION,
            ),
            idle_timeout: resolve_idle_timeout_secs(req.idle_timeout_secs),
            next_event_seq: 0,
            terminal_outcome: None,
            control: session.control,
            event_drain: drain,
            babysitter_summary: None,
        };
        let mut g = self.inner.lock().await;
        g.runs.insert(run_id.clone(), record);
        lease.commit();
        if let Some(pid) = pid {
            spawn_early_exit_watcher(self.clone(), run_id.clone(), pid);
        }
        Ok(AcquireResponse {
            run_id,
            identity,
            pid,
        })
    }

    /// Write a typed `Lifecycle::NativeRuntime` event into a run's session
    /// JSONL (dec_052).
    async fn write_native_runtime(
        &self,
        run_id: &str,
        session_path: &Path,
        identity: &RuntimeIdentity,
        native: NativeRuntimeMeta,
    ) -> Result<(), SupervisorError> {
        let evt = Lifecycle::NativeRuntime {
            provider: native.provider,
            session_id: native.session_id,
            session_path: native.session_path,
            launch_argv: native.launch_argv,
            resume_argv: native.resume_argv,
        };
        self.writer
            .append_session(SessionAppend {
                run_id: run_id.to_string(),
                session_path: session_path.to_path_buf(),
                identity: identity.clone(),
                kind: SessionEventKind::Lifecycle,
                event: serde_json::to_value(&evt).map_err(into_anyhow)?,
            })
            .await
            .map_err(SupervisorError::Session)?;
        Ok(())
    }

    /// Write a `Lifecycle::RunMeta` event carrying the reattach material so a
    /// future daemon boot can rehydrate this run against its live mux session.
    #[allow(clippy::too_many_arguments)]
    async fn write_run_meta(
        &self,
        run_id: &str,
        session_path: &Path,
        identity: &RuntimeIdentity,
        transport: &str,
        harness: Option<String>,
        project_id: Option<String>,
        worktree: Option<PathBuf>,
        last_path: Option<PathBuf>,
        stdout_path: Option<PathBuf>,
        dispatch_attempt_token: Option<String>,
        role: String,
        requires_worker_finalize: bool,
        driver_config: DriverConfig,
    ) -> Result<(), SupervisorError> {
        let evt = Lifecycle::RunMeta {
            transport: transport.to_string(),
            harness,
            project_id,
            worktree,
            last_path,
            stdout_path,
            dispatch_attempt_token,
            role: Some(role),
            requires_worker_finalize: Some(requires_worker_finalize),
            driver_config: driver_config.0,
        };
        self.writer
            .append_session(SessionAppend {
                run_id: run_id.to_string(),
                session_path: session_path.to_path_buf(),
                identity: identity.clone(),
                kind: SessionEventKind::Lifecycle,
                event: serde_json::to_value(&evt).map_err(into_anyhow)?,
            })
            .await
            .map_err(SupervisorError::Session)?;
        Ok(())
    }

    /// Append a durable, operator-authored composer send to a *live* run's
    /// session JSONL (dec_052). The shared recording path for Run Dock sends.
    pub async fn record_composer_send(
        &self,
        run_id: &str,
        text: &str,
    ) -> Result<(), SupervisorError> {
        let (session_path, identity) = {
            let g = self.inner.lock().await;
            let rec = g
                .runs
                .get(run_id)
                .ok_or_else(|| SupervisorError::RunNotFound(run_id.into()))?;
            (rec.session_path.clone(), rec.identity.clone())
        };
        let evt = Lifecycle::ComposerSend {
            text: text.to_string(),
        };
        self.writer
            .append_session(SessionAppend {
                run_id: run_id.to_string(),
                session_path,
                identity,
                kind: SessionEventKind::Lifecycle,
                event: serde_json::to_value(&evt).map_err(into_anyhow)?,
            })
            .await
            .map_err(SupervisorError::Session)?;
        Ok(())
    }

    /// Append a pending recovery prompt draft to a run's session JSONL
    /// (dec_052). The operator must send it manually from the UI.
    pub async fn append_prompt_draft(
        &self,
        run_id: &str,
        session_path: &Path,
        identity: &RuntimeIdentity,
        text: &str,
    ) -> Result<(), SupervisorError> {
        let evt = Lifecycle::PromptDraft {
            text: text.to_string(),
            sent: false,
        };
        self.writer
            .append_session(SessionAppend {
                run_id: run_id.to_string(),
                session_path: session_path.to_path_buf(),
                identity: identity.clone(),
                kind: SessionEventKind::Lifecycle,
                event: serde_json::to_value(&evt).map_err(into_anyhow)?,
            })
            .await
            .map_err(SupervisorError::Session)?;
        Ok(())
    }

    /// Rehydrate a still-live runtime from a prior daemon boot into supervisor
    /// state, preserving the original `run_id` and `runtime_id` (dec_052). A
    /// `reattach` lifecycle event recording the current boot is appended to
    /// the *original* session JSONL and event drain resumes into that file.
    ///
    /// A held `(task_id, kind)` lease blocks automatic reattach with a
    /// structured [`SupervisorError::ReattachLeaseConflict`].
    #[allow(clippy::too_many_arguments)]
    pub async fn reattach(
        &self,
        driver: &dyn WorkerDriver,
        identity: RuntimeIdentity,
        kind: RunKind,
        task_id: String,
        worker_id: String,
        role: String,
        requires_worker_finalize: bool,
        project_id: Option<String>,
        worktree: Option<PathBuf>,
        session_path: PathBuf,
        driver_config: DriverConfig,
    ) -> Result<AcquireResponse, SupervisorError> {
        let run_id = identity.run_id.clone();
        // Lease conflict guard: do not steal an occupied lease.
        let lease_key = (task_id.clone(), kind);
        {
            let mut guard = self.inner.lock().await;
            if guard.acquisition_paused {
                return Err(SupervisorError::AcquisitionPaused);
            }
            if guard.runs.contains_key(&run_id) {
                // Already live in this boot; nothing to rehydrate.
                let existing = &guard.runs[&run_id];
                return Ok(AcquireResponse {
                    run_id: run_id.clone(),
                    identity: existing.identity.clone(),
                    pid: None,
                });
            }
            if let Some(active) = guard.leases.get(&lease_key) {
                if active != &run_id {
                    return Err(SupervisorError::ReattachLeaseConflict {
                        task_id,
                        kind,
                        active_run_id: active.clone(),
                    });
                }
            }
            guard.leases.insert(lease_key.clone(), run_id.clone());
        }
        let mut lease = LeaseReservation::new(self.inner.clone(), lease_key, run_id.clone());

        let ctx = DriverContext {
            identity: identity.clone(),
            run_kind: kind,
            task_id: task_id.clone(),
            worker_id: worker_id.clone(),
            project_id: project_id.clone(),
            worktree: worktree.clone(),
            babysitter_target: None,
            continuation: None,
        };
        let transport = driver.transport().to_string();
        let harness = driver.harness().map(str::to_string);
        let attached = match driver.attach(ctx, driver_config).await {
            Ok(AttachOutcome::Attached(attached)) => attached,
            Ok(AttachOutcome::NotReattachable) => {
                return Err(SupervisorError::NotReattachable {
                    run_id,
                    reason: "driver could not prove a live runtime handle".into(),
                });
            }
            Err(e) => return Err(SupervisorError::Driver(e)),
        };
        let session = *attached.session;

        // Append the reattach lifecycle marker to the ORIGINAL session JSONL,
        // recording the new boot that rehydrated the run.
        let reattach_evt = Lifecycle::Reattach {
            reattached_boot: self.boot.boot_id.clone(),
            transport: transport.clone(),
        };
        self.writer
            .append_session(SessionAppend {
                run_id: run_id.clone(),
                session_path: session_path.clone(),
                identity: identity.clone(),
                kind: SessionEventKind::Lifecycle,
                event: serde_json::to_value(&reattach_evt).map_err(into_anyhow)?,
            })
            .await
            .map_err(SupervisorError::Session)?;

        // Resume event drain into the same file.
        let writer = self.writer.clone();
        let drain_path = session_path.clone();
        let inner_for_drain = self.inner.clone();
        let run_id_for_drain = run_id.clone();
        let identity_for_drain = identity.clone();
        let drain = tokio::spawn(async move {
            let mut events = session.events;
            while let Some(evt) = events.recv().await {
                let payload = match serde_json::to_value(&evt) {
                    Ok(v) => v,
                    Err(e) => {
                        warn!(error = %e, "driver event serialize failed");
                        continue;
                    }
                };
                let terminal_outcome = terminal_outcome_for_event(&evt);
                let event_at = Instant::now();
                {
                    let mut g = inner_for_drain.lock().await;
                    if let Some(rec) = g.runs.get_mut(&run_id_for_drain) {
                        apply_driver_event_to_record(rec, &evt, event_at, terminal_outcome);
                    }
                }
                if let Err(e) = writer
                    .append_session(SessionAppend {
                        run_id: run_id_for_drain.clone(),
                        session_path: drain_path.clone(),
                        identity: identity_for_drain.clone(),
                        kind: SessionEventKind::DriverEvent,
                        event: payload,
                    })
                    .await
                {
                    warn!(error = %e, run_id = %run_id_for_drain, "session append failed");
                }
                let terminal_release = {
                    let mut g = inner_for_drain.lock().await;
                    if let Some(rec) = g.runs.get_mut(&run_id_for_drain) {
                        rec.next_event_seq += 1;
                    }
                    if terminal_outcome.is_some() {
                        take_driver_terminal_release(&mut g, &run_id_for_drain)
                    } else {
                        None
                    }
                };
                if let Some(release) = terminal_release {
                    finish_driver_terminal_release(&writer, release).await;
                }
            }
            finish_stream_end_terminal_drain(&writer, &inner_for_drain, &run_id_for_drain).await;
        });

        let run_started_at = Instant::now();
        let record = RunRecord {
            task_id: task_id.clone(),
            kind,
            worker_id: worker_id.clone(),
            role: role.clone(),
            transport,
            harness,
            project_id,
            worktree: worktree.clone(),
            sub_state: initial_working_sub_state(&role),
            identity: identity.clone(),
            session_path,
            babysitter_target: None,
            // Reattach carries no `AcquireRequest`, so these start empty; the
            // boot auto-reattach caller backfills them via
            // `set_dispatch_artifact_paths` when the persisted `RunMeta` event
            // has dispatch artifact paths.
            last_path: None,
            stdout_path: None,
            dispatch_attempt_token: None,
            requires_worker_finalize,
            terminal_round: 0,
            terminal_declaration: None,
            artifactor_lifecycle: ArtifactorLifecycle::Idle,
            pending_terminal_drain: false,
            pending_cancel: false,
            babysitter_run_id: None,
            last_driver_event_at: run_started_at,
            run_started_at,
            last_input_at: run_started_at,
            // Reattach has no caller-provided thresholds; managers stay
            // exempt across daemon restarts.
            stall_timeout: (!is_interactive_manager_task(&task_id))
                .then_some(DEFAULT_STALL_TIMEOUT),
            max_run_duration: (!is_interactive_manager_task(&task_id))
                .then_some(DEFAULT_MAX_RUN_DURATION),
            // Reattach carries no persistence signal (no AcquireRequest is
            // threaded through it), so idle release stays disabled — a
            // reattached run is never idle-released even if it was
            // originally a persistent artifactor session.
            idle_timeout: None,
            next_event_seq: 0,
            terminal_outcome: None,
            control: session.control,
            event_drain: drain,
            babysitter_summary: if kind == RunKind::Worker {
                Some(BabysitterSummaryBuffer::default())
            } else {
                None
            },
        };
        let mut g = self.inner.lock().await;
        g.runs.insert(run_id.clone(), record);
        lease.commit();
        Ok(AcquireResponse {
            run_id,
            identity,
            pid: None,
        })
    }

    /// Spawn a babysitter watching `target_run`. AC #5: separate JSONL
    /// (`<target_run>.babysitter.jsonl`), fixed tool set, summarized
    /// implementer events as the only input the babysitter sees from the
    /// implementer side.
    pub async fn spawn_babysitter(
        &self,
        driver: &dyn WorkerDriver,
        target_run: &str,
        sessions_dir: &Path,
        worker_id: &str,
        driver_config: DriverConfig,
        run_timeouts: (Option<u32>, Option<u32>),
    ) -> Result<AcquireResponse, SupervisorError> {
        let (task_id, project_id) = {
            let g = self.inner.lock().await;
            let rec = g
                .runs
                .get(target_run)
                .ok_or_else(|| SupervisorError::RunNotFound(target_run.into()))?;
            (rec.task_id.clone(), None::<String>)
        };
        let bs_path = sessions_dir.join(format!("{}.babysitter.jsonl", target_run));
        let req = AcquireRequest {
            task_id,
            kind: RunKind::Babysitter,
            worker_id: worker_id.into(),
            role: "babysitter".into(),
            project_id,
            worktree: None,
            last_path: None,
            stdout_path: None,
            dispatch_attempt_token: None,
            session_path: bs_path,
            driver_config,
            babysitter_target: Some(target_run.into()),
            stall_timeout_secs: run_timeouts.0,
            max_run_duration_secs: run_timeouts.1,
            idle_timeout_secs: None,
            babysitter: None,
        };
        let resp = self.acquire_impl(driver, req).await?;
        // Emit a BabysitterSpawned envelope into the target run's session
        // so the implementer's JSONL records that a watcher attached.
        if let Some(target_rec) = self.inner.lock().await.runs.get(target_run) {
            let evt = Lifecycle::BabysitterSpawned {
                target_run: target_run.into(),
                babysitter_run: resp.run_id.clone(),
            };
            let _ = self
                .writer
                .append_session(SessionAppend {
                    run_id: target_run.into(),
                    session_path: target_rec.session_path.clone(),
                    identity: target_rec.identity.clone(),
                    kind: SessionEventKind::Lifecycle,
                    event: serde_json::to_value(&evt).unwrap_or(serde_json::Value::Null),
                })
                .await;
        }
        Ok(resp)
    }

    /// Hand the latest summarized chunk of the implementer's event stream
    /// to the babysitter's JSONL. The supervisor coalesces driver events as
    /// they arrive (see [`update_babysitter_buffer`]) and this method
    /// flushes the current window.
    pub async fn flush_babysitter_summary(
        &self,
        target_run: &str,
        babysitter_run: &str,
    ) -> Result<Option<BabysitterSummaryChunk>, SupervisorError> {
        let pending = {
            let mut g = self.inner.lock().await;
            take_babysitter_summary_locked(&mut g, target_run, babysitter_run)?
        };
        let Some(pending) = pending else {
            return Ok(None);
        };
        self.writer
            .append_session(SessionAppend {
                run_id: pending.run_id,
                session_path: pending.session_path,
                identity: pending.identity,
                kind: SessionEventKind::BabysitterSummary,
                event: serde_json::to_value(&pending.chunk).map_err(into_anyhow)?,
            })
            .await
            .map_err(SupervisorError::Session)?;
        Ok(Some(pending.chunk))
    }

    pub async fn transition_state(
        &self,
        run_id: &str,
        req: TransitionRequest,
        caller_identity: &RuntimeIdentity,
    ) -> Result<orgasmic_drivers::TransitionAck, SupervisorError> {
        let mut g = self.inner.lock().await;
        let rec = g
            .runs
            .get_mut(run_id)
            .ok_or_else(|| SupervisorError::RunNotFound(run_id.into()))?;
        self.check_ownership(rec, caller_identity)?;
        Ok(rec.control.transition_state(req).await?)
    }

    pub async fn babysitter_action(
        &self,
        babysitter_run: &str,
        tool: BabysitterTool,
        payload: serde_json::Value,
        caller_identity: &RuntimeIdentity,
    ) -> Result<orgasmic_drivers::BabysitterAck, SupervisorError> {
        let mut g = self.inner.lock().await;
        let rec = g
            .runs
            .get_mut(babysitter_run)
            .ok_or_else(|| SupervisorError::RunNotFound(babysitter_run.into()))?;
        self.check_ownership(rec, caller_identity)?;
        let target = rec
            .babysitter_target
            .clone()
            .ok_or_else(|| SupervisorError::BabysitterTargetInvalid("missing target".into()))?;
        let req = orgasmic_drivers::BabysitterRequest {
            tool,
            target_run: target,
            payload,
        };
        Ok(rec.control.babysitter_action(req).await?)
    }

    pub async fn send_input(
        &self,
        run_id: &str,
        input: String,
        caller_identity: &RuntimeIdentity,
    ) -> Result<orgasmic_drivers::UserInputAck, SupervisorError> {
        let ack = {
            let mut g = self.inner.lock().await;
            let rec = g
                .runs
                .get_mut(run_id)
                .ok_or_else(|| SupervisorError::RunNotFound(run_id.into()))?;
            self.check_ownership(rec, caller_identity)?;
            let ack = rec
                .control
                .send_input(UserInputRequest {
                    input: input.clone(),
                })
                .await?;
            // Idle timer resets on every accepted send_input — independent
            // of last_driver_event_at (stall), which resets on driver
            // events instead. Reset while still holding the guard that
            // fetched `rec` so this can't race a concurrent idle sweep.
            if ack.accepted {
                rec.last_input_at = Instant::now();
            }
            ack
        };
        // Shared recording path (TASK-102 / dec_052): a durable composer_send
        // lifecycle event for every accepted operator send. Best-effort — a
        // recording failure must not mask a delivered input.
        if ack.accepted {
            if let Err(e) = self.record_composer_send(run_id, &input).await {
                warn!(error = %e, run_id, "composer_send recording failed");
            }
        }
        Ok(ack)
    }

    pub async fn switch_runtime_options(
        &self,
        run_id: &str,
        req: RuntimeOptionsRequest,
        caller_identity: &RuntimeIdentity,
    ) -> Result<orgasmic_drivers::RuntimeOptionsAck, SupervisorError> {
        let mut g = self.inner.lock().await;
        let rec = g
            .runs
            .get_mut(run_id)
            .ok_or_else(|| SupervisorError::RunNotFound(run_id.into()))?;
        self.check_ownership(rec, caller_identity)?;
        Ok(rec.control.switch_runtime_options(req).await?)
    }

    pub async fn runtime_options_catalog(
        &self,
        run_id: &str,
        caller_identity: &RuntimeIdentity,
    ) -> Result<orgasmic_drivers::RuntimeOptionsCatalog, SupervisorError> {
        let mut g = self.inner.lock().await;
        let rec = g
            .runs
            .get_mut(run_id)
            .ok_or_else(|| SupervisorError::RunNotFound(run_id.into()))?;
        self.check_ownership(rec, caller_identity)?;
        Ok(rec.control.runtime_options_catalog().await?)
    }

    pub async fn release(
        &self,
        run_id: &str,
        reason: &str,
        outcome: ReleaseOutcome,
    ) -> Result<(), SupervisorError> {
        self.release_with_finalization(run_id, reason, outcome, false, None)
            .await
    }

    /// Same as [`Supervisor::release`], but lets the caller record that the
    /// worker itself declared completion (`orgasmic dispatch finalize`,
    /// dec_3M7M0) before this release, over the same daemon channel used for
    /// every other write. Persisted on the `Lifecycle::Release` event so the
    /// dispatch completion watcher — which may not observe the release until
    /// after this call returns — can tell a worker-declared completion apart
    /// from every other release path (stall timeout, manual cancel, driver
    /// terminal event) and skip its scrollback-scrape fallback.
    ///
    /// `caller_identity` (TASK-DWJVH, review #4 residual): when present, the
    /// same self-consistency guard `send_input`/`transition_state` already
    /// apply — the live run's identity must match before it is released.
    /// This is not a new trust boundary (the localhost+bearer model is
    /// unchanged); it defends against a stale/reattached identity or a
    /// different run having reclaimed this run_id between the caller
    /// resolving the run and this call landing. `None` preserves today's
    /// unauthenticated release for the human manager path
    /// (dispatch-close/lease-release).
    pub async fn release_with_finalization(
        &self,
        run_id: &str,
        reason: &str,
        outcome: ReleaseOutcome,
        finalized_by_worker: bool,
        caller_identity: Option<&RuntimeIdentity>,
    ) -> Result<(), SupervisorError> {
        let released = self
            .release_one(
                run_id,
                reason,
                outcome,
                finalized_by_worker,
                caller_identity,
            )
            .await?;
        if released.kind == RunKind::Worker {
            if let Some(bs_run_id) = released.babysitter_run_id {
                let cascade_reason = format!("cascade from implementer {run_id}");
                // The babysitter itself never finalizes; only the implementer
                // run it observes does, so the cascade release carries no
                // caller identity of its own.
                if let Err(e) = self
                    .release_one(&bs_run_id, &cascade_reason, outcome, false, None)
                    .await
                {
                    if !matches!(e, SupervisorError::RunNotFound(_)) {
                        warn!(
                            error = %e,
                            run_id,
                            babysitter_run_id = %bs_run_id,
                            "babysitter cascade release failed"
                        );
                    }
                }
            }
        }
        Ok(())
    }

    async fn release_one(
        &self,
        run_id: &str,
        reason: &str,
        outcome: ReleaseOutcome,
        finalized_by_worker: bool,
        caller_identity: Option<&RuntimeIdentity>,
    ) -> Result<ReleasedRun, SupervisorError> {
        let (
            task_id,
            kind,
            babysitter_run_id,
            session_path,
            identity,
            final_outcome,
            mut control,
            drain,
            cleared_babysitter_backoff,
        ) = {
            let mut g = self.inner.lock().await;
            // Ownership check happens before the remove, under the same lock
            // guard, so there is no window between "checked" and "removed"
            // for a different run to reclaim `run_id`.
            if let Some(caller) = caller_identity {
                let rec = g
                    .runs
                    .get(run_id)
                    .ok_or_else(|| SupervisorError::RunNotFound(run_id.into()))?;
                self.check_ownership(rec, caller)?;
            }
            // orgasmic:TASK-ARZGD — no path may remove the record while an
            // artifactor writer/regenerate ack is in flight. Cancel waits for
            // the writer outcome then records Cancelled; timeout/drain defer.
            if let Some(rec) = g.runs.get_mut(run_id) {
                if artifactor_lifecycle_in_flight(rec) {
                    if !finalized_by_worker && matches!(outcome, ReleaseOutcome::Cancelled) {
                        rec.pending_cancel = true;
                    } else {
                        rec.pending_terminal_drain = true;
                    }
                    return Err(SupervisorError::DeferredWhileInFlight(run_id.into()));
                }
            }
            let rec = g
                .runs
                .remove(run_id)
                .ok_or_else(|| SupervisorError::RunNotFound(run_id.into()))?;
            g.leases.remove(&(rec.task_id.clone(), rec.kind));
            let cleared_babysitter_backoff = if rec.kind == RunKind::Babysitter {
                g.babysitter_auto_spawn_backoff
                    .remove(&rec.task_id)
                    .is_some()
            } else {
                false
            };
            let task_id = rec.task_id.clone();
            let kind = rec.kind;
            let babysitter_run_id = rec.babysitter_run_id.clone();
            let path = rec.session_path.clone();
            let identity = rec.identity.clone();
            let final_outcome = rec.terminal_outcome.unwrap_or(outcome);
            let drain = rec.event_drain;
            (
                task_id,
                kind,
                babysitter_run_id,
                path,
                identity,
                final_outcome,
                rec.control,
                drain,
                cleared_babysitter_backoff,
            )
        };
        if cleared_babysitter_backoff {
            warn!(
                task_id = %task_id,
                run_id,
                "babysitter auto-spawn resumed after babysitter lease release"
            );
        }
        let _ = control.release(reason).await;
        drop(control);
        // Let the drain flush driver events (e.g. synthetic run_complete) after
        // release ack; aborting immediately races and drops terminal events.
        let _ = tokio::time::timeout(Duration::from_secs(5), drain).await;
        let evt = Lifecycle::Release {
            reason: reason.into(),
            outcome: final_outcome,
            finalized_by_worker,
        };
        self.writer
            .append_session(SessionAppend {
                run_id: run_id.into(),
                session_path,
                identity,
                kind: SessionEventKind::Lifecycle,
                event: serde_json::to_value(&evt).map_err(into_anyhow)?,
            })
            .await
            .map_err(SupervisorError::Session)?;
        Ok(ReleasedRun {
            kind,
            babysitter_run_id,
        })
    }

    pub async fn snapshot(&self) -> SupervisorSnapshot {
        let g = self.inner.lock().await;
        let runs = g
            .runs
            .iter()
            .map(|(id, rec)| RunSummary {
                run_id: id.clone(),
                task_id: rec.task_id.clone(),
                kind: rec.role.clone(),
                run_kind: rec.kind,
                worker_id: rec.worker_id.clone(),
                role: rec.role.clone(),
                driver: rec.transport.clone(),
                harness: rec.harness.clone(),
                project_id: rec.project_id.clone(),
                worktree: rec.worktree.clone(),
                sub_state: rec.sub_state.clone(),
                identity: rec.identity.clone(),
                session_path: rec.session_path.clone(),
                babysitter_target: rec.babysitter_target.clone(),
                event_count: rec.next_event_seq,
                last_path: rec.last_path.clone(),
                stdout_path: rec.stdout_path.clone(),
                dispatch_attempt_token: rec.dispatch_attempt_token.clone(),
            })
            .collect();
        SupervisorSnapshot {
            acquisition_paused: g.acquisition_paused,
            runs,
        }
    }

    /// Backfill dispatch artifact paths onto an already-live `RunRecord`.
    /// `AcquireRequest` populates these at acquire time, but `reattach` takes
    /// no `AcquireRequest`; boot auto-reattach calls this once it has read
    /// them back out of the persisted `RunMeta` lifecycle event, so a
    /// reattached dispatch run is still resolvable by `orgasmic dispatch
    /// finalize`. A no-op if the run is no longer live.
    pub async fn set_dispatch_artifact_paths(
        &self,
        run_id: &str,
        last_path: PathBuf,
        stdout_path: PathBuf,
        dispatch_attempt_token: Option<String>,
    ) {
        let mut g = self.inner.lock().await;
        if let Some(rec) = g.runs.get_mut(run_id) {
            rec.last_path = Some(last_path);
            rec.stdout_path = Some(stdout_path);
            if dispatch_attempt_token.is_some() {
                rec.dispatch_attempt_token = dispatch_attempt_token;
            }
            rec.requires_worker_finalize = run_requires_worker_finalize(&rec.last_path, &rec.role);
        }
    }

    /// Restore the persisted terminal contract after boot reattach when artifact
    /// paths alone cannot reconstruct it (manager, artifactor, stage shapes).
    pub async fn restore_terminal_contract(
        &self,
        run_id: &str,
        role: String,
        requires_worker_finalize: bool,
    ) {
        let mut g = self.inner.lock().await;
        if let Some(rec) = g.runs.get_mut(run_id) {
            rec.role = role;
            rec.requires_worker_finalize = requires_worker_finalize;
        }
    }

    pub async fn pause_acquisition(&self) {
        self.inner.lock().await.acquisition_paused = true;
    }

    pub async fn resume_acquisition(&self) {
        self.inner.lock().await.acquisition_paused = false;
    }

    async fn release_first_timed_out_run(&self) {
        self.release_first_timed_out_run_after_candidate(|| async {})
            .await;
    }

    async fn release_first_timed_out_run_after_candidate<F, Fut>(&self, after_candidate: F)
    where
        F: FnOnce() -> Fut,
        Fut: Future<Output = ()>,
    {
        let now = Instant::now();
        let candidate = {
            let g = self.inner.lock().await;
            g.runs
                .iter()
                .filter_map(|(run_id, rec)| timed_out_run(run_id, rec, now))
                .min_by_key(|candidate| candidate.deadline)
        };
        let Some(candidate) = candidate else {
            return;
        };
        after_candidate().await;
        let revalidated = {
            let now = Instant::now();
            let g = self.inner.lock().await;
            let Some(rec) = g.runs.get(&candidate.run_id) else {
                return;
            };
            let Some(revalidated) = timed_out_run(&candidate.run_id, rec, now) else {
                return;
            };
            if revalidated.reason != candidate.reason {
                return;
            }
            revalidated
        };
        warn!(
            run_id = %revalidated.run_id,
            reason = revalidated.reason,
            threshold_secs = revalidated.threshold.as_secs(),
            elapsed_secs = revalidated.elapsed.as_secs(),
            "supervisor run timeout exceeded"
        );
        // orgasmic:TASK-S52X9 — hot-session artifactor that already submitted
        // ends idle with the finalize tombstone (Completed), not Failed.
        let (reason, outcome, finalized) = {
            let g = self.inner.lock().await;
            match g.runs.get(&revalidated.run_id).and_then(|rec| {
                rec.terminal_declaration
                    .filter(|decl| decl.round == rec.terminal_round)
                    .map(|decl| decl.reason)
            }) {
                Some(declared) => (declared, ReleaseOutcome::Completed, true),
                None => (revalidated.reason, ReleaseOutcome::Failed, false),
            }
        };
        if let Err(e) = self
            .release_with_finalization(&revalidated.run_id, reason, outcome, finalized, None)
            .await
        {
            if matches!(e, SupervisorError::DeferredWhileInFlight(_)) {
                // In-flight artifactor writer/regenerate — defer; never a
                // false Failed timeout tombstone (TASK-ARZGD).
                return;
            }
            if !matches!(e, SupervisorError::RunNotFound(_)) {
                warn!(
                    error = %e,
                    run_id = %revalidated.run_id,
                    reason,
                    "supervisor timeout release failed"
                );
            }
        }
    }

    /// Record a worker-declared terminal verb without releasing the lease
    /// (hot-session artifactor submit). Cleared on a new regenerate round.
    // orgasmic:TASK-S52X9
    pub async fn mark_terminal_declaration(
        &self,
        run_id: &str,
        reason: &'static str,
    ) -> Result<(), SupervisorError> {
        let mut g = self.inner.lock().await;
        let rec = g
            .runs
            .get_mut(run_id)
            .ok_or_else(|| SupervisorError::RunNotFound(run_id.into()))?;
        let round = rec.terminal_round;
        rec.terminal_declaration = Some(TerminalDeclaration { reason, round });
        Ok(())
    }

    /// Atomically find the live artifactor run for `task_id` and install the
    /// submit-in-flight token under one supervisor lock (TASK-ARZGD P1).
    /// Carry the returned run_id through commit/abort — never re-lookup.
    // orgasmic:TASK-ARZGD
    pub async fn begin_artifactor_submit_for_task(
        &self,
        task_id: &str,
    ) -> Result<(String, u64), SupervisorError> {
        let token = ARTIFACTOR_LIFECYCLE_TOKEN.fetch_add(1, Ordering::Relaxed);
        let mut g = self.inner.lock().await;
        let run_id = g
            .runs
            .iter()
            .find(|(_, rec)| rec.task_id == task_id)
            .map(|(id, _)| id.clone())
            .ok_or_else(|| SupervisorError::RunNotFound(task_id.into()))?;
        let rec = g
            .runs
            .get_mut(&run_id)
            .ok_or_else(|| SupervisorError::RunNotFound(run_id.clone()))?;
        if !matches!(rec.artifactor_lifecycle, ArtifactorLifecycle::Idle) {
            return Err(SupervisorError::ArtifactorLifecycleBusy(run_id));
        }
        rec.artifactor_lifecycle = ArtifactorLifecycle::Submit(SubmitInFlight {
            round: rec.terminal_round,
            token,
        });
        Ok((run_id, token))
    }

    /// Mark an in-flight artifactor submit before the durable writer transaction.
    /// Does not install a terminal declaration — only defers terminal resolution
    /// until commit or abort (TASK-99W9C). Prefer
    /// [`Self::begin_artifactor_submit_for_task`] for the production path.
    pub async fn prepare_artifactor_submit_in_flight(
        &self,
        run_id: &str,
    ) -> Result<u64, SupervisorError> {
        let token = ARTIFACTOR_LIFECYCLE_TOKEN.fetch_add(1, Ordering::Relaxed);
        let mut g = self.inner.lock().await;
        let rec = g
            .runs
            .get_mut(run_id)
            .ok_or_else(|| SupervisorError::RunNotFound(run_id.into()))?;
        if !matches!(rec.artifactor_lifecycle, ArtifactorLifecycle::Idle) {
            return Err(SupervisorError::ArtifactorLifecycleBusy(run_id.into()));
        }
        rec.artifactor_lifecycle = ArtifactorLifecycle::Submit(SubmitInFlight {
            round: rec.terminal_round,
            token,
        });
        Ok(token)
    }

    /// Promote an in-flight submit to a durable declaration after the writer
    /// transaction commits. Resolves deferred cancel/drain outcomes.
    pub async fn commit_artifactor_submit_in_flight(
        &self,
        run_id: &str,
        token: u64,
    ) -> Result<(), SupervisorError> {
        let outcome = {
            let mut g = self.inner.lock().await;
            let rec = g
                .runs
                .get_mut(run_id)
                .ok_or_else(|| SupervisorError::RunNotFound(run_id.into()))?;
            let ArtifactorLifecycle::Submit(in_flight) = rec.artifactor_lifecycle else {
                return Ok(());
            };
            if in_flight.token != token {
                return Ok(());
            }
            rec.artifactor_lifecycle = ArtifactorLifecycle::Idle;
            rec.terminal_declaration = Some(TerminalDeclaration {
                reason: "artifact_submitted",
                round: in_flight.round,
            });
            take_deferred_artifactor_release(&mut g, run_id)
        };
        finish_deferred_artifactor_release(&self.writer, outcome).await;
        Ok(())
    }

    /// Clear an in-flight submit after writer failure. Resolves deferred
    /// cancel/drain — never a false Completed.
    pub async fn abort_artifactor_submit_in_flight(
        &self,
        run_id: &str,
        token: u64,
    ) -> Result<(), SupervisorError> {
        let outcome = {
            let mut g = self.inner.lock().await;
            let Some(rec) = g.runs.get_mut(run_id) else {
                return Ok(());
            };
            let ArtifactorLifecycle::Submit(in_flight) = rec.artifactor_lifecycle else {
                return Ok(());
            };
            if in_flight.token != token {
                return Ok(());
            }
            rec.artifactor_lifecycle = ArtifactorLifecycle::Idle;
            rec.terminal_declaration = None;
            take_deferred_artifactor_release(&mut g, run_id)
        };
        match outcome {
            DeferredArtifactorRelease::Cancel(rec) => {
                append_terminal_release(
                    &self.writer,
                    rec,
                    ResolvedTerminalRelease {
                        reason: "cancelled".into(),
                        outcome: ReleaseOutcome::Cancelled,
                        finalized_by_worker: false,
                    },
                )
                .await;
            }
            DeferredArtifactorRelease::Drain(rec) => {
                append_terminal_release(
                    &self.writer,
                    rec,
                    ResolvedTerminalRelease {
                        reason: "artifact_submit_failed".into(),
                        outcome: ReleaseOutcome::Failed,
                        finalized_by_worker: false,
                    },
                )
                .await;
            }
            DeferredArtifactorRelease::None => {}
        }
        Ok(())
    }

    /// Clear a prior terminal declaration and bump the artifactor round when a
    /// new regenerate round starts — that round needs its own submit. Holds a
    /// `regenerate_in_flight` checkpoint so terminal drains defer until
    /// commit/rollback (TASK-ARZGD P3).
    // orgasmic:TASK-S52X9,TASK-ARZGD
    pub async fn begin_artifactor_regenerate_round(
        &self,
        run_id: &str,
    ) -> Result<ArtifactorRegenerateCheckpoint, SupervisorError> {
        let token = ARTIFACTOR_LIFECYCLE_TOKEN.fetch_add(1, Ordering::Relaxed);
        let mut g = self.inner.lock().await;
        let rec = g
            .runs
            .get_mut(run_id)
            .ok_or_else(|| SupervisorError::RunNotFound(run_id.into()))?;
        if !matches!(rec.artifactor_lifecycle, ArtifactorLifecycle::Idle) {
            return Err(SupervisorError::ArtifactorLifecycleBusy(run_id.into()));
        }
        let checkpoint = ArtifactorRegenerateCheckpoint {
            terminal_round: rec.terminal_round,
            terminal_declaration: rec.terminal_declaration,
            token,
        };
        rec.terminal_round = rec.terminal_round.saturating_add(1);
        rec.terminal_declaration = None;
        rec.artifactor_lifecycle = ArtifactorLifecycle::Regenerate(checkpoint);
        Ok(checkpoint)
    }

    /// Clear regenerate-in-flight after an accepted follow-up. Resolves any
    /// deferred drain against the new (undeclared) round.
    // orgasmic:TASK-ARZGD
    pub async fn commit_artifactor_regenerate_round(
        &self,
        run_id: &str,
        checkpoint: ArtifactorRegenerateCheckpoint,
    ) -> Result<(), SupervisorError> {
        let outcome = {
            let mut g = self.inner.lock().await;
            let rec = g
                .runs
                .get_mut(run_id)
                .ok_or_else(|| SupervisorError::RunNotFound(run_id.into()))?;
            let ArtifactorLifecycle::Regenerate(active) = rec.artifactor_lifecycle else {
                return Ok(());
            };
            if active.token != checkpoint.token {
                return Ok(());
            }
            rec.artifactor_lifecycle = ArtifactorLifecycle::Idle;
            take_deferred_artifactor_release(&mut g, run_id)
        };
        finish_deferred_artifactor_release(&self.writer, outcome).await;
        Ok(())
    }

    /// Restore the artifactor round/declaration after a rejected regenerate
    /// follow-up (TASK-99W9C / TASK-ARZGD). Resolves deferred cancel/drain
    /// against the restored declaration.
    pub async fn rollback_artifactor_regenerate_round(
        &self,
        run_id: &str,
        checkpoint: ArtifactorRegenerateCheckpoint,
    ) -> Result<(), SupervisorError> {
        let outcome = {
            let mut g = self.inner.lock().await;
            let rec = g
                .runs
                .get_mut(run_id)
                .ok_or_else(|| SupervisorError::RunNotFound(run_id.into()))?;
            let ArtifactorLifecycle::Regenerate(active) = rec.artifactor_lifecycle else {
                return Ok(());
            };
            if active.token != checkpoint.token {
                return Ok(());
            }
            rec.terminal_round = active.terminal_round;
            rec.terminal_declaration = active.terminal_declaration;
            rec.artifactor_lifecycle = ArtifactorLifecycle::Idle;
            take_deferred_artifactor_release(&mut g, run_id)
        };
        finish_deferred_artifactor_release(&self.writer, outcome).await;
        Ok(())
    }

    fn check_ownership(
        &self,
        rec: &RunRecord,
        caller: &RuntimeIdentity,
    ) -> Result<(), SupervisorError> {
        if rec.identity.run_id != caller.run_id {
            return Err(SupervisorError::OwnershipMismatch {
                run_id: caller.run_id.clone(),
                field: "run_id",
                expected: rec.identity.run_id.clone(),
                got: caller.run_id.clone(),
            });
        }
        if rec.identity.runtime_id != caller.runtime_id {
            return Err(SupervisorError::OwnershipMismatch {
                run_id: caller.run_id.clone(),
                field: "runtime_id",
                expected: rec.identity.runtime_id.clone(),
                got: caller.runtime_id.clone(),
            });
        }
        if rec.identity.boot_id != caller.boot_id {
            return Err(SupervisorError::OwnershipMismatch {
                run_id: caller.run_id.clone(),
                field: "boot_id",
                expected: rec.identity.boot_id.clone(),
                got: caller.boot_id.clone(),
            });
        }
        Ok(())
    }

    /// Stop any live dispatch worker before daemon-side worktree/branch cleanup.
    /// Releases the worker only when task, kind, attempt token, worktree, and
    /// artifact pair all match exactly (TASK-ZGT1X).
    pub async fn release_dispatch_worker_for_cleanup(
        &self,
        task_id: &str,
        kind: RunKind,
        dispatch_attempt_token: Option<&str>,
        worktree_path: &Path,
        last_path: Option<&Path>,
        stdout_path: Option<&Path>,
    ) -> Result<Option<String>, SupervisorError> {
        let run_id = {
            let g = self.inner.lock().await;
            g.leases.get(&(task_id.to_string(), kind)).cloned()
        };
        let Some(run_id) = run_id else {
            return Ok(None);
        };
        let live = {
            let g = self.inner.lock().await;
            g.runs.contains_key(&run_id)
        };
        // Never steal a lease in the lease→RunRecord gap during cleanup (TASK-NW4WV).
        if !live {
            return Ok(None);
        }
        let matches = {
            let g = self.inner.lock().await;
            g.runs.get(&run_id).is_some_and(|rec| {
                dispatch_cleanup_identity_matches(
                    rec,
                    dispatch_attempt_token,
                    worktree_path,
                    last_path,
                    stdout_path,
                )
            })
        };
        if !matches {
            return Ok(None);
        }
        self.release(
            &run_id,
            "dispatch failure cleanup",
            ReleaseOutcome::Interrupted,
        )
        .await?;
        Ok(Some(run_id))
    }

    /// Authorize and optionally release a dispatch worker for cleanup. Returns
    /// `Conflict` when the live run, a durable session owner, a newer tokened
    /// attempt, or an in-flight cleanup reservation owns the same worktree/
    /// artifacts so filesystem cleanup must not proceed (TASK-KE0JW, TASK-1FV1N,
    /// TASK-NW4WV).
    pub async fn prepare_dispatch_cleanup(
        &self,
        sessions_dir: &Path,
        params: &DispatchCleanupParams,
    ) -> Result<DispatchCleanupOutcome, SupervisorError> {
        let worktree_key = normalize_cleanup_worktree(&params.worktree_path);
        let reservation_key = CleanupReservationKey {
            task_id: params.task_id.clone(),
            kind: params.kind,
            worktree_key: worktree_key.clone(),
        };
        let reservation = DispatchCleanupReservation {
            branch: params.branch.clone(),
            worktree_path: params.worktree_path.clone(),
            dispatch_attempt_token: params.dispatch_attempt_token.clone(),
            last_path: params.last_path.clone(),
            stdout_path: params.stdout_path.clone(),
        };

        if params.worktree_path.is_dir() {
            if let Some(checked_out) = dispatch_worktree_checked_out_branch(&params.worktree_path) {
                if checked_out != params.branch {
                    return Ok(DispatchCleanupOutcome::Conflict);
                }
            }
        }

        // Install the cleanup fence atomically before any durable scan or release
        // window can admit a new acquire (TASK-NW4WV).
        {
            let mut g = self.inner.lock().await;
            if g.cleanup_reservations.contains_key(&reservation_key) {
                return Ok(DispatchCleanupOutcome::Conflict);
            }
            for rec in g.runs.values() {
                if rec.task_id != params.task_id || rec.kind != params.kind {
                    continue;
                }
                if rec.worktree.as_ref().map(|p| normalize_cleanup_worktree(p))
                    != Some(worktree_key.clone())
                {
                    continue;
                }
                if !dispatch_cleanup_identity_matches(
                    rec,
                    params.dispatch_attempt_token.as_deref(),
                    &params.worktree_path,
                    params.last_path.as_deref(),
                    params.stdout_path.as_deref(),
                ) {
                    return Ok(DispatchCleanupOutcome::Conflict);
                }
            }
            if params.dispatch_attempt_token.is_none() {
                let tokened_owner = g.runs.values().any(|rec| {
                    rec.worktree.as_ref().map(|p| normalize_cleanup_worktree(p))
                        == Some(worktree_key.clone())
                        && rec.dispatch_attempt_token.is_some()
                });
                if tokened_owner {
                    return Ok(DispatchCleanupOutcome::Conflict);
                }
            }
            g.cleanup_reservations
                .insert(reservation_key.clone(), reservation);
        }

        let durable_owner =
            match scan_durable_dispatch_owner(sessions_dir, &params.task_id, &worktree_key) {
                Ok(owner) => owner,
                Err(_) => {
                    self.clear_dispatch_cleanup_reservation(
                        &params.task_id,
                        params.kind,
                        &params.worktree_path,
                    )
                    .await;
                    return Ok(DispatchCleanupOutcome::Conflict);
                }
            };
        match authorize_cleanup_identity(
            params.dispatch_attempt_token.as_deref(),
            &params.worktree_path,
            params.last_path.as_deref(),
            params.stdout_path.as_deref(),
            durable_owner.as_ref(),
        ) {
            CleanupIdentityAuth::IdentityMismatch | CleanupIdentityAuth::AuthorityUnavailable => {
                self.clear_dispatch_cleanup_reservation(
                    &params.task_id,
                    params.kind,
                    &params.worktree_path,
                )
                .await;
                return Ok(DispatchCleanupOutcome::Conflict);
            }
            CleanupIdentityAuth::NoOwner | CleanupIdentityAuth::ExactOwner => {}
        }

        let released_run_id = self
            .release_dispatch_worker_for_cleanup(
                &params.task_id,
                params.kind,
                params.dispatch_attempt_token.as_deref(),
                &params.worktree_path,
                params.last_path.as_deref(),
                params.stdout_path.as_deref(),
            )
            .await?;

        Ok(DispatchCleanupOutcome::Proceed { released_run_id })
    }

    async fn clear_dispatch_cleanup_reservation(
        &self,
        task_id: &str,
        kind: RunKind,
        worktree_path: &Path,
    ) {
        let worktree_key = normalize_cleanup_worktree(worktree_path);
        let reservation_key = CleanupReservationKey {
            task_id: task_id.to_string(),
            kind,
            worktree_key,
        };
        self.inner
            .lock()
            .await
            .cleanup_reservations
            .remove(&reservation_key);
    }

    /// Release a dispatch cleanup reservation after filesystem mutation finishes.
    pub async fn finish_dispatch_cleanup(
        &self,
        task_id: &str,
        kind: RunKind,
        worktree_path: &Path,
        branch: Option<&str>,
        dispatch_attempt_token: Option<&str>,
    ) {
        let worktree_key = normalize_cleanup_worktree(worktree_path);
        let reservation_key = CleanupReservationKey {
            task_id: task_id.to_string(),
            kind,
            worktree_key,
        };
        let mut g = self.inner.lock().await;
        let Some(reservation) = g.cleanup_reservations.get(&reservation_key) else {
            return;
        };
        if let Some(expected_branch) = branch {
            if reservation.branch != expected_branch {
                return;
            }
        }
        if reservation.dispatch_attempt_token.as_deref() != dispatch_attempt_token {
            return;
        }
        if reservation.worktree_path != worktree_path {
            return;
        }
        g.cleanup_reservations.remove(&reservation_key);
    }

    /// Clear an *orphaned* lease: one held for `(task_id, kind)` whose run
    /// record no longer exists (e.g. the CLI timed out while the daemon
    /// completed the acquire, then the run died without releasing). Returns
    /// what happened so the caller can report it honestly. A lease backed by
    /// a live run is NOT cleared — release the run instead.
    pub async fn release_orphaned_lease(
        &self,
        task_id: &str,
        kind: RunKind,
    ) -> OrphanedLeaseOutcome {
        let mut g = self.inner.lock().await;
        let key = (task_id.to_string(), kind);
        let Some(run_id) = g.leases.get(&key).cloned() else {
            return OrphanedLeaseOutcome::NoLease;
        };
        if g.runs.contains_key(&run_id) {
            return OrphanedLeaseOutcome::HeldByLiveRun { run_id };
        }
        g.leases.remove(&key);
        OrphanedLeaseOutcome::Released { run_id }
    }
}

/// Whether a dispatch cleanup request may proceed with filesystem mutation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DispatchCleanupOutcome {
    /// Identity matches; optional released run id when a live worker was stopped.
    Proceed { released_run_id: Option<String> },
    /// A live or newer tokened attempt owns the same worktree/artifacts.
    Conflict,
}

/// Result of [`Supervisor::release_orphaned_lease`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum OrphanedLeaseOutcome {
    /// No lease was held for the key; nothing to clear.
    NoLease,
    /// The lease is backed by a live run record — refuse to steal it.
    HeldByLiveRun { run_id: String },
    /// The lease was orphaned (no run record) and has been cleared.
    Released { run_id: String },
}

/// When a subprocess driver exits before emitting any work envelope (only
/// `Lifecycle::Acquire` + `DriverEvent::Ready`), the event drain can stay
/// blocked and leave the lease stuck. TASK-072 closed the no-terminal hung
/// watcher for longer runs; this closes the early-exit case.
/// Subprocess drivers such as cursor-agent fork a long-lived worker child and
/// may exit their CLI wrapper quickly. Poll briefly for a direct child so
/// the dispatch watch hint (DispatchResponse.pid) tracks the real worker PID.
/// The early-exit watcher (spawn_early_exit_watcher, TASK-074) remains on the
/// wrapper PID by design — it must track the original spawn target to detect
/// genuine early-exit failures rather than intermediate-child shenanigans.
pub(crate) async fn resolve_dispatch_watch_pid(wrapper_pid: Option<u32>) -> Option<u32> {
    let wrapper_pid = wrapper_pid?;
    if wrapper_pid == 0 {
        return Some(0);
    }
    Some(
        poll_direct_child_pid(wrapper_pid)
            .await
            .unwrap_or(wrapper_pid),
    )
}

async fn poll_direct_child_pid(parent_pid: u32) -> Option<u32> {
    let wait_for_worker_server = wrapper_looks_like_cursor_agent(parent_pid);
    let accept_any_child_after = Instant::now() + Duration::from_millis(500);
    let deadline = Instant::now() + Duration::from_secs(5);
    while Instant::now() < deadline {
        if let Some(child) = prefer_worker_server_child(parent_pid) {
            return Some(child);
        }
        if !wait_for_worker_server && Instant::now() >= accept_any_child_after {
            if let Some(child) = live_direct_child_pid(parent_pid) {
                return Some(child);
            }
        }
        if subprocess_exited(parent_pid) {
            return None;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    prefer_worker_server_child(parent_pid).or_else(|| live_direct_child_pid(parent_pid))
}

fn prefer_worker_server_child(parent_pid: u32) -> Option<u32> {
    live_direct_child_pids(parent_pid).into_iter().find(|pid| {
        process_command(*pid)
            .map(|command| command.contains("worker-server"))
            .unwrap_or(false)
    })
}

fn live_direct_child_pids(parent_pid: u32) -> Vec<u32> {
    let output = match Command::new("pgrep")
        .args(["-P", &parent_pid.to_string()])
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .output()
    {
        Ok(output) if output.status.success() => output,
        _ => return ps_direct_child_pids(parent_pid),
    };
    String::from_utf8_lossy(&output.stdout)
        .lines()
        .filter_map(|line| line.trim().parse::<u32>().ok())
        .filter(|pid| !process_is_zombie(*pid))
        .collect::<Vec<_>>()
        .into_iter()
        .chain(ps_direct_child_pids(parent_pid))
        .collect::<std::collections::BTreeSet<_>>()
        .into_iter()
        .collect()
}

fn ps_direct_child_pids(parent_pid: u32) -> Vec<u32> {
    let output = match Command::new("ps")
        .args(["ax", "-o", "pid=,ppid="])
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .output()
    {
        Ok(output) if output.status.success() => output,
        _ => return Vec::new(),
    };
    String::from_utf8_lossy(&output.stdout)
        .lines()
        .filter_map(|line| {
            let mut parts = line.split_whitespace();
            let pid = parts.next()?.parse::<u32>().ok()?;
            let ppid = parts.next()?.parse::<u32>().ok()?;
            (ppid == parent_pid).then_some(pid)
        })
        .filter(|pid| !process_is_zombie(*pid))
        .collect()
}

fn live_direct_child_pid(parent_pid: u32) -> Option<u32> {
    live_direct_child_pids(parent_pid).into_iter().next()
}

fn wrapper_looks_like_cursor_agent(parent_pid: u32) -> bool {
    process_command(parent_pid)
        .map(|command| command.contains("cursor-agent"))
        .unwrap_or(false)
}

fn process_command(pid: u32) -> Option<String> {
    let output = Command::new("ps")
        .args(["-p", pid.to_string().as_str(), "-o", "command="])
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let command = String::from_utf8_lossy(&output.stdout).trim().to_string();
    if command.is_empty() {
        None
    } else {
        Some(command)
    }
}

fn process_is_zombie(pid: u32) -> bool {
    let output = Command::new("ps")
        .args(["-p", pid.to_string().as_str(), "-o", "stat="])
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .output()
        .ok();
    match output {
        Some(output) if output.status.success() => String::from_utf8_lossy(&output.stdout)
            .chars()
            .next()
            .is_some_and(|stat| stat == 'Z'),
        _ => false,
    }
}

fn spawn_early_exit_watcher(supervisor: Supervisor, run_id: String, pid: u32) {
    tokio::spawn(async move {
        let safety = Instant::now() + Duration::from_secs(30);
        let mut exit_observed: Option<Instant> = None;
        loop {
            if Instant::now() >= safety {
                return;
            }
            let session_path = {
                let guard = supervisor.inner.lock().await;
                guard
                    .runs
                    .get(&run_id)
                    .map(|record| record.session_path.clone())
            };
            let Some(session_path) = session_path else {
                return;
            };
            if !subprocess_exited(pid) {
                exit_observed = None;
                tokio::time::sleep(Duration::from_millis(250)).await;
                continue;
            }
            let exit_at = *exit_observed.get_or_insert_with(Instant::now);
            if exit_at.elapsed() < Duration::from_millis(500) {
                tokio::time::sleep(Duration::from_millis(250)).await;
                continue;
            }
            let Ok(envelopes) = read_session_file(&session_path) else {
                if exit_at.elapsed() >= Duration::from_secs(10) {
                    return;
                }
                tokio::time::sleep(Duration::from_millis(250)).await;
                continue;
            };
            if session_has_work_envelope(&envelopes) {
                return;
            }
            if session_is_early_exit_no_work(&envelopes) {
                tracing::info!(
                    run_id = %run_id,
                    pid,
                    "auto-releasing early-exit subprocess run with no work envelopes"
                );
                let _ = supervisor
                    .release(
                        &run_id,
                        "early-exit subprocess with no work envelopes",
                        ReleaseOutcome::Interrupted,
                    )
                    .await;
                return;
            }
            if exit_at.elapsed() >= Duration::from_secs(10) {
                return;
            }
            tokio::time::sleep(Duration::from_millis(250)).await;
        }
    });
}

#[cfg(unix)]
pub(crate) fn subprocess_exited(pid: u32) -> bool {
    if pid == 0 {
        tracing::warn!(pid, "refusing to probe invalid process id");
        return true;
    }
    let Ok(pid) = libc::pid_t::try_from(pid) else {
        tracing::warn!(
            pid,
            "process id does not fit platform pid_t; treating as exited"
        );
        return true;
    };
    let result = if unsafe { libc::kill(pid, 0) } == 0 {
        Ok(())
    } else {
        Err(std::io::Error::last_os_error().raw_os_error())
    };
    process_probe_reports_exited(pid, result)
}

#[cfg(not(unix))]
pub(crate) fn subprocess_exited(_pid: u32) -> bool {
    // There is no portable non-Unix equivalent of kill(pid, 0). External
    // manager registration normalizes every supplied PID away on these
    // targets, so it uses the tokenized TTL fallback instead.
    false
}

#[cfg(unix)]
fn process_probe_reports_exited(pid: libc::pid_t, result: Result<(), Option<i32>>) -> bool {
    match result {
        Ok(()) | Err(Some(libc::EPERM)) => false,
        Err(Some(libc::ESRCH)) => true,
        Err(errno) => {
            tracing::warn!(
                pid,
                ?errno,
                "unexpected kill(pid, 0) result; keeping run alive"
            );
            false
        }
    }
}

fn session_has_work_envelope(envelopes: &[SessionEnvelope]) -> bool {
    envelopes.iter().any(|envelope| {
        if envelope.kind != SessionEventKind::DriverEvent {
            return false;
        }
        match envelope.event.get("type").and_then(|value| value.as_str()) {
            Some("ready") => false,
            // TASK-076 option 1: a driver_error means the harness failed before
            // useful work, so it must not block early-exit auto-release.
            Some("driver_error") => false,
            Some(_) => true,
            None => false,
        }
    })
}

fn session_has_terminal_event(envelopes: &[SessionEnvelope]) -> bool {
    envelopes.iter().any(|envelope| {
        if envelope.kind != SessionEventKind::DriverEvent {
            return false;
        }
        matches!(
            envelope.event.get("type").and_then(|value| value.as_str()),
            Some("run_complete") | Some("run_fail") | Some("run_error")
        )
    })
}

fn session_is_early_exit_no_work(envelopes: &[SessionEnvelope]) -> bool {
    if envelopes.is_empty()
        || session_has_work_envelope(envelopes)
        || session_has_terminal_event(envelopes)
    {
        return false;
    }
    envelopes.iter().any(|envelope| {
        if envelope.kind != SessionEventKind::Lifecycle {
            return false;
        }
        matches!(
            serde_json::from_value::<Lifecycle>(envelope.event.clone()),
            Ok(Lifecycle::Acquire { .. })
        )
    })
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SupervisorSnapshot {
    pub acquisition_paused: bool,
    pub runs: Vec<RunSummary>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RunSummary {
    pub run_id: String,
    pub task_id: String,
    /// The dispatched worker role — who is working right now ("implementer",
    /// "reviewer", "babysitter", "manager", …). This is intentionally the
    /// public `run.kind` field; `run_kind` carries the supervisor lease axis.
    pub kind: String,
    pub run_kind: RunKind,
    pub worker_id: String,
    /// Duplicate of `kind` retained for existing UI code while consumers move
    /// away from session-filename role inference.
    pub role: String,
    pub driver: String,
    pub harness: Option<String>,
    pub project_id: Option<String>,
    /// The dispatched worktree root, when known. `orgasmic dispatch finalize
    /// --commit` cross-checks this against the resolved git toplevel before
    /// committing, refusing to commit a root that isn't the dispatched
    /// worktree (TASK-QKQ3R).
    #[serde(default)]
    pub worktree: Option<PathBuf>,
    pub sub_state: Option<RunSubState>,
    pub identity: RuntimeIdentity,
    pub session_path: PathBuf,
    pub babysitter_target: Option<String>,
    pub event_count: u64,
    /// Dispatch/stage artifact path when the run advertises finalize
    /// (`None` for manager/recovery/artifactor). `orgasmic dispatch finalize`
    /// resolves the report path for the current run from this field rather
    /// than scanning `.orgasmic/tx`, which a worker's own worktree checkout
    /// cannot see live daemon writes to.
    #[serde(default)]
    pub last_path: Option<PathBuf>,
    #[serde(default)]
    pub stdout_path: Option<PathBuf>,
    #[serde(default)]
    pub dispatch_attempt_token: Option<String>,
}

fn make_run_id(kind: &RunKind) -> String {
    let prefix = match kind {
        RunKind::Worker => "run",
        RunKind::Babysitter => "bs",
    };
    format!(
        "{prefix}-{}-{}",
        Utc::now().format("%Y%m%dT%H%M%S"),
        Uuid::new_v4().simple()
    )
}

fn babysitter_auto_spawn_backoff_delay(attempts: u32) -> Duration {
    let exponent = attempts.saturating_sub(1).min(6);
    let secs = BABYSITTER_AUTO_SPAWN_INITIAL_BACKOFF.as_secs() * (1_u64 << exponent);
    Duration::from_secs(secs.min(BABYSITTER_AUTO_SPAWN_MAX_BACKOFF.as_secs()))
}

fn into_anyhow(e: serde_json::Error) -> anyhow::Error {
    anyhow::anyhow!("serialize session envelope: {e}")
}

/// `None` = use the default; `Some(0)` = disable the timeout entirely
/// (interactive runs); any other value is an explicit threshold.
fn resolve_timeout_secs(value: Option<u32>, default: Duration) -> Option<Duration> {
    match value {
        Some(0) => None,
        Some(secs) => Some(Duration::from_secs(u64::from(secs))),
        None => Some(default),
    }
}

/// Idle-release resolution — deliberately the inverse default of
/// [`resolve_timeout_secs`]: `None` means idle detection stays OFF (every
/// caller except the persistent artifactor spawn path), not "apply the
/// default". `Some(0)` also disables it, matching the stall/max `Some(0)`
/// convention; only an explicit positive value enables it.
fn resolve_idle_timeout_secs(value: Option<u32>) -> Option<Duration> {
    match value {
        None | Some(0) => None,
        Some(secs) => Some(Duration::from_secs(u64::from(secs))),
    }
}

fn spawn_run_timeout_monitor(supervisor: Supervisor) {
    let Ok(handle) = tokio::runtime::Handle::try_current() else {
        return;
    };
    std::mem::drop(handle.spawn(async move {
        let mut tick = tokio::time::interval(RUN_TIMEOUT_CHECK_INTERVAL);
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        loop {
            tick.tick().await;
            supervisor.release_first_timed_out_run().await;
        }
    }));
}

fn timed_out_run(run_id: &str, rec: &RunRecord, now: Instant) -> Option<RunTimeoutCandidate> {
    if rec.terminal_outcome.is_some() {
        return None;
    }

    let stall = rec.stall_timeout.and_then(|threshold| {
        let elapsed = now.saturating_duration_since(rec.last_driver_event_at);
        (elapsed > threshold).then(|| RunTimeoutCandidate {
            run_id: run_id.to_string(),
            reason: "stall_timeout_exceeded",
            threshold,
            elapsed,
            deadline: rec.last_driver_event_at + threshold,
        })
    });
    let max = rec.max_run_duration.and_then(|threshold| {
        let elapsed = now.saturating_duration_since(rec.run_started_at);
        (elapsed > threshold).then(|| RunTimeoutCandidate {
            run_id: run_id.to_string(),
            reason: "max_run_duration_exceeded",
            threshold,
            elapsed,
            deadline: rec.run_started_at + threshold,
        })
    });
    // Idle is a THIRD, independent timeout keyed on the more recent of the
    // last accepted `send_input` and the last driver event, so a run that is
    // actively streaming driver output is never idle-released even if no
    // input has arrived — only true inactivity on BOTH clocks counts.
    let idle = rec.idle_timeout.and_then(|threshold| {
        let last_activity_at = rec.last_input_at.max(rec.last_driver_event_at);
        let elapsed = now.saturating_duration_since(last_activity_at);
        (elapsed > threshold).then(|| RunTimeoutCandidate {
            run_id: run_id.to_string(),
            reason: "idle_timeout_exceeded",
            threshold,
            elapsed,
            deadline: last_activity_at + threshold,
        })
    });
    // Earliest deadline wins; ties fall to whichever candidate is first in
    // this list (stall, then max, then idle), matching the pre-existing
    // stall-wins-ties behavior.
    [stall, max, idle]
        .into_iter()
        .flatten()
        .min_by_key(|candidate| candidate.deadline)
}

fn terminal_outcome_for_event(evt: &DriverEvent) -> Option<ReleaseOutcome> {
    match evt {
        DriverEvent::RunComplete { .. } => Some(ReleaseOutcome::Completed),
        DriverEvent::RunFail { .. } => Some(ReleaseOutcome::Failed),
        DriverEvent::DriverError { fatal: true, .. } => Some(ReleaseOutcome::Failed),
        _ => None,
    }
}

fn apply_driver_event_to_record(
    rec: &mut RunRecord,
    evt: &DriverEvent,
    event_at: Instant,
    terminal_outcome: Option<ReleaseOutcome>,
) {
    rec.last_driver_event_at = event_at;
    if let Some(outcome) = terminal_outcome {
        rec.terminal_outcome = Some(outcome);
    }
    if let DriverEvent::TransitionState { to, .. } = evt {
        if let Ok(sub_state) = RunSubState::new(to.clone()) {
            rec.sub_state = Some(sub_state);
        }
    }
}

/// TUI transports emit a terminal driver event on pane/process end
/// (TASK-AFE5Q — no marker fallback). ACP / subprocess modes do not — their
/// stream-end path handles protocol end.
// orgasmic:TASK-AFE5Q
fn terminal_event_releases_transport(transport: &str) -> bool {
    matches!(transport, "tmux" | "tmux-tui" | "rmux")
}

/// Whether a live run record matches the cleanup request's attempt identity.
fn dispatch_cleanup_identity_matches(
    rec: &RunRecord,
    dispatch_attempt_token: Option<&str>,
    worktree_path: &Path,
    last_path: Option<&Path>,
    stdout_path: Option<&Path>,
) -> bool {
    fn path_eq(left: Option<&PathBuf>, right: Option<&Path>) -> bool {
        match (left, right) {
            (Some(a), Some(b)) => a == b,
            (None, None) => true,
            _ => false,
        }
    }
    rec.worktree.as_deref() == Some(worktree_path)
        && path_eq(rec.last_path.as_ref(), last_path)
        && path_eq(rec.stdout_path.as_ref(), stdout_path)
        && rec.dispatch_attempt_token.as_deref() == dispatch_attempt_token
}

fn durable_owner_identity_matches(
    owner: &DurableDispatchOwner,
    dispatch_attempt_token: Option<&str>,
    worktree_path: &Path,
    last_path: Option<&Path>,
    stdout_path: Option<&Path>,
) -> bool {
    fn path_eq(left: Option<&PathBuf>, right: Option<&Path>) -> bool {
        match (left, right) {
            (Some(a), Some(b)) => a == b,
            (None, None) => true,
            _ => false,
        }
    }
    owner.worktree.as_deref() == Some(worktree_path)
        && path_eq(owner.last_path.as_ref(), last_path)
        && path_eq(owner.stdout_path.as_ref(), stdout_path)
        && owner.dispatch_attempt_token.as_deref() == dispatch_attempt_token
}

fn authorize_cleanup_identity(
    dispatch_attempt_token: Option<&str>,
    worktree_path: &Path,
    last_path: Option<&Path>,
    stdout_path: Option<&Path>,
    durable_owner: Option<&DurableDispatchOwner>,
) -> CleanupIdentityAuth {
    let Some(owner) = durable_owner else {
        return CleanupIdentityAuth::NoOwner;
    };
    if durable_owner_identity_matches(
        owner,
        dispatch_attempt_token,
        worktree_path,
        last_path,
        stdout_path,
    ) {
        CleanupIdentityAuth::ExactOwner
    } else if owner.dispatch_attempt_token.is_some() || dispatch_attempt_token.is_some() {
        CleanupIdentityAuth::IdentityMismatch
    } else {
        CleanupIdentityAuth::NoOwner
    }
}

fn scan_durable_dispatch_owner(
    sessions_dir: &Path,
    task_id: &str,
    worktree_key: &Path,
) -> Result<Option<DurableDispatchOwner>, DurableScanError> {
    if !sessions_dir.exists() {
        return Ok(None);
    }
    let entries =
        std::fs::read_dir(sessions_dir).map_err(|_| DurableScanError::UnreadableSessionsDir)?;
    let mut latest: Option<DurableDispatchOwner> = None;
    for entry in entries {
        let entry = entry.map_err(|_| DurableScanError::UnreadableSessionsDir)?;
        let path = entry.path();
        if path.extension().and_then(|ext| ext.to_str()) != Some("jsonl") {
            continue;
        }
        let envelopes = read_session_file(&path)
            .map_err(|_| DurableScanError::UnreadableSessionFile(path.clone()))?;
        let mut current_task: Option<String> = None;
        for envelope in &envelopes {
            if envelope.kind != SessionEventKind::Lifecycle {
                continue;
            }
            match serde_json::from_value::<Lifecycle>(envelope.event.clone()) {
                Ok(Lifecycle::Acquire {
                    task_id: acquired_task,
                    ..
                }) => current_task = Some(acquired_task),
                Ok(Lifecycle::RunMeta {
                    worktree,
                    last_path,
                    stdout_path,
                    dispatch_attempt_token,
                    ..
                }) => {
                    let Some(acquired_task) = current_task.as_deref() else {
                        continue;
                    };
                    if acquired_task != task_id {
                        continue;
                    }
                    let Some(wt) = worktree.as_ref() else {
                        continue;
                    };
                    if normalize_cleanup_worktree(wt) != *worktree_key {
                        continue;
                    }
                    let candidate = DurableDispatchOwner {
                        dispatch_attempt_token: dispatch_attempt_token.clone(),
                        last_path: last_path.clone(),
                        stdout_path: stdout_path.clone(),
                        worktree: Some(wt.clone()),
                        recorded_at: envelope.time,
                    };
                    if let Some(existing) = latest.as_ref() {
                        if candidate.recorded_at == existing.recorded_at
                            && (candidate.dispatch_attempt_token != existing.dispatch_attempt_token
                                || candidate.last_path != existing.last_path
                                || candidate.stdout_path != existing.stdout_path)
                        {
                            return Err(DurableScanError::UnreadableSessionFile(path.clone()));
                        }
                    }
                    if latest
                        .as_ref()
                        .is_none_or(|existing| candidate.recorded_at > existing.recorded_at)
                    {
                        latest = Some(candidate);
                    }
                }
                Err(_) => {
                    return Err(DurableScanError::UnreadableSessionFile(path.clone()));
                }
                _ => {}
            }
        }
    }
    Ok(latest)
}

fn normalize_cleanup_worktree(path: &Path) -> PathBuf {
    path.canonicalize().unwrap_or_else(|_| path.to_path_buf())
}

fn dispatch_worktree_checked_out_branch(worktree: &Path) -> Option<String> {
    let output = Command::new("git")
        .args(["rev-parse", "--abbrev-ref", "HEAD"])
        .current_dir(worktree)
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let branch = String::from_utf8_lossy(&output.stdout).trim().to_string();
    if branch.is_empty() || branch == "HEAD" {
        None
    } else {
        Some(branch)
    }
}

/// Whether this run requires an explicit worker-declared terminal call
/// (dec_WDR5K item 6 / TASK-S52X9). Dispatch and stage grill/plan/architect
/// advertise the contract when they carry a `last_path`; artifactor and
/// manager always do (their terminal verbs are submit / release, not
/// `dispatch finalize`). Custom bare terminals (`terminal`) and babysitters
/// are exempt (dec_WDR5K item 6 seventh amendment / TASK-TZJFF). Unknown
/// historical agent roles fail closed (TASK-ARZGD).
// orgasmic:TASK-S52X9,TASK-ARZGD,dec_WDR5K
pub(crate) fn run_requires_worker_finalize(last_path: &Option<PathBuf>, role: &str) -> bool {
    match role {
        "implementer" | "reviewer" | "architector" | "griller" | "planner" => last_path.is_some(),
        "artifactor" | "manager" => true,
        "terminal" | "babysitter" => false,
        // Fail closed: unknown non-terminal historical agent roles require
        // a declaration; protocol-end without one is Failed.
        _ => true,
    }
}

fn artifactor_lifecycle_in_flight(rec: &RunRecord) -> bool {
    !matches!(rec.artifactor_lifecycle, ArtifactorLifecycle::Idle)
}

enum DeferredArtifactorRelease {
    None,
    Cancel(RunRecord),
    Drain(RunRecord),
}

fn take_deferred_artifactor_release(inner: &mut Inner, run_id: &str) -> DeferredArtifactorRelease {
    let Some(rec) = inner.runs.get_mut(run_id) else {
        return DeferredArtifactorRelease::None;
    };
    // A deferred release belongs to the lifecycle transaction that observed
    // it. Never extract the run until that transaction has resolved.
    if artifactor_lifecycle_in_flight(rec) {
        return DeferredArtifactorRelease::None;
    }
    if rec.pending_cancel {
        let Some(rec) = inner.runs.remove(run_id) else {
            return DeferredArtifactorRelease::None;
        };
        inner.leases.remove(&(rec.task_id.clone(), rec.kind));
        return DeferredArtifactorRelease::Cancel(rec);
    }
    if rec.pending_terminal_drain {
        let Some(rec) = inner.runs.remove(run_id) else {
            return DeferredArtifactorRelease::None;
        };
        inner.leases.remove(&(rec.task_id.clone(), rec.kind));
        return DeferredArtifactorRelease::Drain(rec);
    }
    DeferredArtifactorRelease::None
}

async fn finish_deferred_artifactor_release(
    writer: &WriterHandle,
    outcome: DeferredArtifactorRelease,
) {
    match outcome {
        DeferredArtifactorRelease::None => {}
        DeferredArtifactorRelease::Cancel(rec) => {
            append_terminal_release(
                writer,
                rec,
                ResolvedTerminalRelease {
                    reason: "cancelled".into(),
                    outcome: ReleaseOutcome::Cancelled,
                    finalized_by_worker: false,
                },
            )
            .await;
        }
        DeferredArtifactorRelease::Drain(rec) => {
            write_terminal_release_from_record(writer, rec, TerminalReleaseSource::StreamEnd).await;
        }
    }
}

/// Resolve the terminal release tombstone for stream-end and TUI terminal
/// events. One state machine for both paths (TASK-TZJFF / TASK-S52X9).
fn resolve_terminal_release(
    rec: &RunRecord,
    source: TerminalReleaseSource,
) -> ResolvedTerminalRelease {
    if artifactor_lifecycle_in_flight(rec) {
        return ResolvedTerminalRelease {
            reason: "artifact_submit_in_flight".into(),
            outcome: ReleaseOutcome::Failed,
            finalized_by_worker: false,
        };
    }
    if let Some(decl) = rec.terminal_declaration {
        if decl.round == rec.terminal_round {
            return ResolvedTerminalRelease {
                reason: decl.reason.to_string(),
                outcome: ReleaseOutcome::Completed,
                finalized_by_worker: true,
            };
        }
    }
    if rec.requires_worker_finalize {
        return ResolvedTerminalRelease {
            reason: "protocol_end_without_finalize".into(),
            outcome: ReleaseOutcome::Failed,
            finalized_by_worker: false,
        };
    }
    let reason = match source {
        TerminalReleaseSource::StreamEnd => "driver stream closed",
        TerminalReleaseSource::TerminalEvent => "driver terminal event",
    };
    match rec.terminal_outcome {
        Some(outcome) => ResolvedTerminalRelease {
            reason: reason.into(),
            outcome,
            finalized_by_worker: false,
        },
        None => ResolvedTerminalRelease {
            reason: reason.into(),
            outcome: ReleaseOutcome::Interrupted,
            finalized_by_worker: false,
        },
    }
}

/// Decide the stream-end Lifecycle::Release when the driver event channel
/// closes and the run was still live (no prior finalize / terminal
/// release won the race). Thin wrapper kept for unit tests.
fn stream_end_release_for_transport(
    _transport: &str,
    terminal_outcome: Option<ReleaseOutcome>,
    requires_worker_finalize: bool,
) -> (&'static str, ReleaseOutcome) {
    if requires_worker_finalize {
        return ("protocol_end_without_finalize", ReleaseOutcome::Failed);
    }
    match terminal_outcome {
        Some(outcome) => ("driver stream closed", outcome),
        None => ("driver stream closed", ReleaseOutcome::Interrupted),
    }
}

fn take_driver_terminal_release(inner: &mut Inner, run_id: &str) -> Option<TerminalRelease> {
    let should_release = inner
        .runs
        .get(run_id)
        .map(|rec| terminal_event_releases_transport(&rec.transport))
        .unwrap_or(false);
    if !should_release {
        return None;
    }
    if inner
        .runs
        .get(run_id)
        .and_then(|rec| rec.terminal_outcome)
        .is_none()
    {
        return None;
    }
    if inner
        .runs
        .get(run_id)
        .is_some_and(artifactor_lifecycle_in_flight)
    {
        if let Some(rec) = inner.runs.get_mut(run_id) {
            rec.pending_terminal_drain = true;
        }
        return None;
    }

    let rec = inner.runs.remove(run_id)?;
    inner.leases.remove(&(rec.task_id.clone(), rec.kind));
    let resolved = resolve_terminal_release(&rec, TerminalReleaseSource::TerminalEvent);
    Some(TerminalRelease {
        run_id: run_id.to_string(),
        session_path: rec.session_path,
        identity: rec.identity,
        reason: resolved.reason,
        outcome: resolved.outcome,
        finalized_by_worker: resolved.finalized_by_worker,
        control: rec.control,
    })
}

fn claim_run_on_stream_end(inner: &mut Inner, run_id: &str) -> Option<RunRecord> {
    let mut rec = inner.runs.remove(run_id)?;
    inner.leases.remove(&(rec.task_id.clone(), rec.kind));
    if artifactor_lifecycle_in_flight(&rec) {
        rec.pending_terminal_drain = true;
        inner.runs.insert(run_id.to_string(), rec);
        return None;
    }
    Some(rec)
}

async fn append_terminal_release(
    writer: &WriterHandle,
    rec: RunRecord,
    resolved: ResolvedTerminalRelease,
) {
    let evt = Lifecycle::Release {
        reason: resolved.reason,
        outcome: resolved.outcome,
        finalized_by_worker: resolved.finalized_by_worker,
    };
    let _ = writer
        .append_session(SessionAppend {
            run_id: rec.identity.run_id.clone(),
            session_path: rec.session_path,
            identity: rec.identity,
            kind: SessionEventKind::Lifecycle,
            event: serde_json::to_value(&evt).unwrap_or(serde_json::Value::Null),
        })
        .await;
    drop(rec.control);
}

async fn write_terminal_release_from_record(
    writer: &WriterHandle,
    rec: RunRecord,
    source: TerminalReleaseSource,
) {
    let resolved = resolve_terminal_release(&rec, source);
    append_terminal_release(writer, rec, resolved).await;
}

async fn finish_stream_end_terminal_drain(
    writer: &WriterHandle,
    inner: &tokio::sync::Mutex<Inner>,
    run_id: &str,
) {
    let rec = {
        let mut g = inner.lock().await;
        claim_run_on_stream_end(&mut g, run_id)
    };
    if let Some(rec) = rec {
        write_terminal_release_from_record(writer, rec, TerminalReleaseSource::StreamEnd).await;
    }
}

async fn finish_driver_terminal_release(writer: &WriterHandle, release: TerminalRelease) {
    let mut control = release.control;
    let _ = control.release("driver terminal event").await;
    drop(control);
    let evt = Lifecycle::Release {
        reason: release.reason,
        outcome: release.outcome,
        finalized_by_worker: release.finalized_by_worker,
    };
    let _ = writer
        .append_session(SessionAppend {
            run_id: release.run_id,
            session_path: release.session_path,
            identity: release.identity,
            kind: SessionEventKind::Lifecycle,
            event: serde_json::to_value(&evt).unwrap_or(serde_json::Value::Null),
        })
        .await;
}

fn take_babysitter_summary_locked(
    g: &mut Inner,
    target_run: &str,
    babysitter_run: &str,
) -> Result<Option<PendingBabysitterSummary>, SupervisorError> {
    let chunk = {
        let rec = g
            .runs
            .get_mut(target_run)
            .ok_or_else(|| SupervisorError::RunNotFound(target_run.into()))?;
        let Some(buf) = rec.babysitter_summary.as_mut() else {
            return Ok(None);
        };
        if buf.count == 0 {
            return Ok(None);
        }
        let chunk = BabysitterSummaryChunk {
            window_start_seq: buf.window_start_seq,
            window_end_seq: buf.window_end_seq,
            event_count: buf.count,
            headline: std::mem::take(&mut buf.headline),
            last_text: std::mem::take(&mut buf.last_text),
            tool_calls: std::mem::take(&mut buf.tool_calls),
        };
        buf.window_started_at = None;
        buf.count = 0;
        buf.window_start_seq = buf.window_end_seq;
        chunk
    };
    let rec = g
        .runs
        .get(babysitter_run)
        .ok_or_else(|| SupervisorError::RunNotFound(babysitter_run.into()))?;
    Ok(Some(PendingBabysitterSummary {
        run_id: babysitter_run.into(),
        session_path: rec.session_path.clone(),
        identity: rec.identity.clone(),
        chunk,
    }))
}

fn should_flush_babysitter_buffer(buf: &BabysitterSummaryBuffer, now: Instant) -> bool {
    if buf.count == 0 {
        return false;
    }
    buf.count >= BABYSITTER_SUMMARY_EVENT_THRESHOLD
        || buf
            .window_started_at
            .map(|started_at| now.duration_since(started_at) >= BABYSITTER_SUMMARY_INTERVAL)
            .unwrap_or(false)
}

fn update_babysitter_buffer(
    buf: &mut BabysitterSummaryBuffer,
    evt: &DriverEvent,
    seq: u64,
    event_at: Instant,
) {
    // Heartbeats are pure liveness signals with no substantive content; they
    // must not inflate the summary window/count fed to the babysitter.
    if matches!(evt, DriverEvent::Heartbeat { .. }) {
        return;
    }
    if buf.count == 0 {
        buf.window_started_at = Some(event_at);
        buf.window_start_seq = seq;
    }
    buf.window_end_seq = seq;
    buf.count += 1;
    match evt {
        // Unreachable: filtered above, but the match must stay exhaustive.
        DriverEvent::Heartbeat { .. } => {}
        DriverEvent::TextChunk { chunk, .. } => {
            buf.last_text = truncate(chunk, 4096);
            if buf.headline.is_empty() {
                buf.headline = "text".into();
            }
        }
        DriverEvent::ToolCall { name, .. } => {
            buf.tool_calls.push(name.clone());
            buf.headline = format!("tool:{name}");
        }
        DriverEvent::ToolResult { ok, .. } => {
            buf.headline = if *ok {
                "tool_result_ok".into()
            } else {
                "tool_result_fail".into()
            };
        }
        DriverEvent::TransitionState { to, .. } => {
            buf.headline = format!("transition:{to}");
        }
        DriverEvent::RunComplete { .. } => {
            buf.headline = "run_complete".into();
        }
        DriverEvent::RunFail { error_code, .. } => {
            buf.headline = format!("run_fail:{error_code}");
        }
        DriverEvent::DriverError { fatal, message } => {
            buf.headline = if *fatal {
                format!("driver_error_fatal:{message}")
            } else {
                format!("driver_error:{message}")
            };
        }
        DriverEvent::Ready { .. } => {
            if buf.headline.is_empty() {
                buf.headline = "ready".into();
            }
        }
    }
}

fn truncate(s: &str, max: usize) -> String {
    if s.len() <= max {
        s.to_string()
    } else {
        let mut end = max;
        while !s.is_char_boundary(end) && end > 0 {
            end -= 1;
        }
        format!("{}…", &s[..end])
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::events::EventBus;
    use crate::writer::spawn as spawn_writer;
    use orgasmic_core::WorkerKind;
    use orgasmic_drivers::{
        modes::tmux, BabysitterAck, BabysitterRequest, ClaudeAcpDriver, DriverError, DriverSession,
        TmuxTuiDriver, TransitionAck, UserInputAck,
    };
    use serde_json::json;

    /// Serialize real-tmux/rmux tests across ALL test binaries: they spawn real
    /// mux daemons and contend under `cargo test --workspace` (TASK-X0ZVE). An
    /// advisory flock on a shared temp path lets at most one run at a time,
    /// cross-process. Held for the whole test via the returned guard.
    fn live_session_guard() -> LiveSessionGuard {
        let path = std::env::temp_dir().join("orgasmic-live-session-tests.lock");
        let file = std::fs::OpenOptions::new()
            .create(true)
            .truncate(false)
            .write(true)
            .open(&path)
            .expect("open live-session lock file");
        // MSRV 1.87: call fs2 explicitly — std's File::lock_exclusive (1.89) shadows it.
        fs2::FileExt::lock_exclusive(&file).expect("flock live-session lock");
        LiveSessionGuard(file)
    }

    /// Minimal in-process driver whose control always accepts `send_input` —
    /// none of the real drivers used elsewhere in these tests (`TmuxTuiDriver`)
    /// implement `send_input` (it's tmux-tui's unimplemented trait default),
    /// so idle-reset tests need a control that actually accepts input to
    /// exercise `Supervisor::send_input`'s accepted branch.
    struct AcceptingInputDriver;

    #[async_trait::async_trait]
    impl WorkerDriver for AcceptingInputDriver {
        fn transport(&self) -> &'static str {
            "test-accepting-input"
        }

        async fn acquire(
            &self,
            ctx: DriverContext,
            _config: DriverConfig,
        ) -> Result<DriverSession, orgasmic_drivers::DriverError> {
            let (tx, rx) = tokio::sync::mpsc::channel(1);
            Ok(DriverSession {
                identity: ctx.identity,
                pid: None,
                events: rx,
                // The sender must outlive acquire() — dropping it here would
                // close the channel and make the drain task see stream-end
                // immediately, auto-releasing the run before the idle sweep
                // ever runs. Keeping it on the control ties its lifetime to
                // the run's control handle instead.
                control: Box::new(AcceptingInputControl { _events: tx }),
                native_runtime: None,
            })
        }
    }

    struct AcceptingInputControl {
        _events: tokio::sync::mpsc::Sender<DriverEvent>,
    }

    #[async_trait::async_trait]
    impl DriverControl for AcceptingInputControl {
        async fn transition_state(
            &mut self,
            _req: TransitionRequest,
        ) -> Result<TransitionAck, DriverError> {
            Ok(TransitionAck {
                accepted: true,
                message: None,
            })
        }

        async fn babysitter_action(
            &mut self,
            _req: BabysitterRequest,
        ) -> Result<BabysitterAck, DriverError> {
            Err(DriverError::Unsupported("babysitter_action"))
        }

        async fn send_input(
            &mut self,
            _req: UserInputRequest,
        ) -> Result<UserInputAck, DriverError> {
            Ok(UserInputAck {
                accepted: true,
                message: None,
            })
        }

        async fn release(&mut self, _reason: &str) -> Result<(), DriverError> {
            Ok(())
        }
    }

    /// ACP-shaped test driver: emits Ready + RunComplete then drops the
    /// event sender so the supervisor stream-end path runs. Transport is
    /// `acp-stdio` so protocol-end must NOT auto-release as Completed
    /// success (TASK-P4MGK).
    struct ProtocolEndAcpDriver;

    #[async_trait::async_trait]
    impl WorkerDriver for ProtocolEndAcpDriver {
        fn transport(&self) -> &'static str {
            "acp-stdio"
        }

        async fn acquire(
            &self,
            ctx: DriverContext,
            _config: DriverConfig,
        ) -> Result<DriverSession, orgasmic_drivers::DriverError> {
            let (tx, rx) = tokio::sync::mpsc::channel(8);
            tokio::spawn(async move {
                let _ = tx
                    .send(DriverEvent::Ready {
                        protocol_version: "test-acp/1".into(),
                        capabilities: json!({"simulated": true}),
                    })
                    .await;
                let _ = tx
                    .send(DriverEvent::RunComplete {
                        summary: Some("protocol turn completed".into()),
                    })
                    .await;
                // Dropping tx closes the stream → supervisor stream-end.
            });
            Ok(DriverSession {
                identity: ctx.identity,
                pid: None,
                events: rx,
                control: Box::new(ProtocolEndAcpControl),
                native_runtime: None,
            })
        }
    }

    struct ProtocolEndAcpControl;

    /// Holds the driver stream open until the test signals `gate`, so in-flight
    /// submit can be prepared before protocol-end (TASK-99W9C).
    struct GatedProtocolEndDriver {
        gate: Arc<tokio::sync::Notify>,
    }

    #[async_trait::async_trait]
    impl WorkerDriver for GatedProtocolEndDriver {
        fn transport(&self) -> &'static str {
            "acp-stdio"
        }

        async fn acquire(
            &self,
            ctx: DriverContext,
            _config: DriverConfig,
        ) -> Result<DriverSession, orgasmic_drivers::DriverError> {
            let gate = Arc::clone(&self.gate);
            let (tx, rx) = tokio::sync::mpsc::channel(8);
            tokio::spawn(async move {
                let _ = tx
                    .send(DriverEvent::Ready {
                        protocol_version: "test-acp/1".into(),
                        capabilities: json!({"simulated": true}),
                    })
                    .await;
                gate.notified().await;
                let _ = tx
                    .send(DriverEvent::RunComplete {
                        summary: Some("protocol turn completed".into()),
                    })
                    .await;
            });
            Ok(DriverSession {
                identity: ctx.identity,
                pid: None,
                events: rx,
                control: Box::new(ProtocolEndAcpControl),
                native_runtime: None,
            })
        }
    }

    /// TUI-shaped test driver: same as [`ProtocolEndAcpDriver`] but transport
    /// is `tmux-tui` so terminal events (not stream-end) claim release.
    struct ProtocolEndTuiDriver;

    #[async_trait::async_trait]
    impl WorkerDriver for ProtocolEndTuiDriver {
        fn transport(&self) -> &'static str {
            "tmux-tui"
        }

        async fn acquire(
            &self,
            ctx: DriverContext,
            _config: DriverConfig,
        ) -> Result<DriverSession, orgasmic_drivers::DriverError> {
            let (tx, rx) = tokio::sync::mpsc::channel(8);
            tokio::spawn(async move {
                let _ = tx
                    .send(DriverEvent::Ready {
                        protocol_version: "test-tui/1".into(),
                        capabilities: json!({"simulated": true}),
                    })
                    .await;
                let _ = tx
                    .send(DriverEvent::RunComplete {
                        summary: Some("protocol turn completed".into()),
                    })
                    .await;
            });
            Ok(DriverSession {
                identity: ctx.identity,
                pid: None,
                events: rx,
                control: Box::new(ProtocolEndAcpControl),
                native_runtime: None,
            })
        }
    }

    #[async_trait::async_trait]
    impl DriverControl for ProtocolEndAcpControl {
        async fn transition_state(
            &mut self,
            _req: TransitionRequest,
        ) -> Result<TransitionAck, DriverError> {
            Ok(TransitionAck {
                accepted: true,
                message: None,
            })
        }

        async fn babysitter_action(
            &mut self,
            _req: BabysitterRequest,
        ) -> Result<BabysitterAck, DriverError> {
            Err(DriverError::Unsupported("babysitter_action"))
        }

        async fn release(&mut self, _reason: &str) -> Result<(), DriverError> {
            Ok(())
        }
    }

    /// Holds the event channel open until `release`, then emits RunComplete
    /// and drops — models finalize-then-protocol-end for ACP modes.
    struct FinalizeThenProtocolEndDriver;

    #[async_trait::async_trait]
    impl WorkerDriver for FinalizeThenProtocolEndDriver {
        fn transport(&self) -> &'static str {
            "acp-stdio"
        }

        async fn acquire(
            &self,
            ctx: DriverContext,
            _config: DriverConfig,
        ) -> Result<DriverSession, orgasmic_drivers::DriverError> {
            let (tx, rx) = tokio::sync::mpsc::channel(8);
            let _ = tx
                .send(DriverEvent::Ready {
                    protocol_version: "test-acp/1".into(),
                    capabilities: json!({"simulated": true}),
                })
                .await;
            Ok(DriverSession {
                identity: ctx.identity,
                pid: None,
                events: rx,
                control: Box::new(FinalizeThenProtocolEndControl { events: Some(tx) }),
                native_runtime: None,
            })
        }
    }

    struct FinalizeThenProtocolEndControl {
        events: Option<tokio::sync::mpsc::Sender<DriverEvent>>,
    }

    #[async_trait::async_trait]
    impl DriverControl for FinalizeThenProtocolEndControl {
        async fn transition_state(
            &mut self,
            _req: TransitionRequest,
        ) -> Result<TransitionAck, DriverError> {
            Ok(TransitionAck {
                accepted: true,
                message: None,
            })
        }

        async fn babysitter_action(
            &mut self,
            _req: BabysitterRequest,
        ) -> Result<BabysitterAck, DriverError> {
            Err(DriverError::Unsupported("babysitter_action"))
        }

        async fn release(&mut self, _reason: &str) -> Result<(), DriverError> {
            if let Some(tx) = self.events.take() {
                let _ = tx
                    .send(DriverEvent::RunComplete {
                        summary: Some("protocol end after finalize".into()),
                    })
                    .await;
                // tx drop closes the stream after finalize already released.
            }
            Ok(())
        }
    }

    struct LiveSessionGuard(std::fs::File);
    impl Drop for LiveSessionGuard {
        fn drop(&mut self) {
            let _ = fs2::FileExt::unlock(&self.0);
        }
    }

    fn make_supervisor() -> (Supervisor, tempfile::TempDir, WriterHandle) {
        let dir = tempfile::tempdir().unwrap();
        let writer = spawn_writer(EventBus::new());
        let boot = Arc::new(BootIdentity::new());
        let sup = Supervisor::with_summarizer(
            writer.clone(),
            boot,
            Arc::new(StaticDiffSummarizer(
                "M crates/orgasmic-drivers/src/lib.rs".into(),
            )),
        );
        (sup, dir, writer)
    }

    #[cfg(unix)]
    #[test]
    fn process_probe_distinguishes_esrch_eperm_and_unexpected_errors() {
        assert!(process_probe_reports_exited(4242, Err(Some(libc::ESRCH))));
        assert!(!process_probe_reports_exited(4242, Err(Some(libc::EPERM))));
        assert!(!process_probe_reports_exited(4242, Err(Some(libc::EIO))));
        assert!(!process_probe_reports_exited(4242, Ok(())));
        assert!(subprocess_exited(0));
    }

    #[tokio::test]
    async fn failed_session_write_releases_manager_lease_for_immediate_reacquire() {
        let (sup, dir, _writer) = make_supervisor();
        let driver = AcceptingInputDriver;
        let task_id = "manager.launch:writer-failure";

        // A file where the sessions directory should be makes SessionWriter
        // fail after the supervisor has reserved the manager lease.
        let blocked = dir.path().join("blocked-sessions");
        std::fs::write(&blocked, "not a directory").unwrap();
        let mut broken = impl_req(task_id, dir.path());
        broken.role = "manager".into();
        broken.worker_id = "manager".into();
        broken.session_path = blocked.join("manager.jsonl");
        let error = sup.acquire(&driver, broken).await.unwrap_err();
        assert!(matches!(error, SupervisorError::Session(_)), "{error}");

        // Mirrors the app manager's stable task id + Worker lease. The failed
        // external registration must not leave the slot wedged.
        let mut retry = impl_req(task_id, dir.path());
        retry.role = "manager".into();
        retry.worker_id = "manager".into();
        retry.session_path = dir.path().join("manager-app-retry.jsonl");
        sup.acquire(&driver, retry)
            .await
            .expect("manager lease should be immediately reusable");
    }

    #[test]
    fn heartbeat_is_non_terminal_so_drain_never_releases_on_it() {
        // The event drain resets last_driver_event_at for every drained event
        // (variant-agnostic) but only releases the lease on a terminal outcome.
        // A heartbeat must reset-but-not-release: it carries no terminal
        // outcome. (TASK-100.3)
        assert!(terminal_outcome_for_event(&DriverEvent::Heartbeat { seq: 0 }).is_none());
    }

    #[test]
    fn heartbeat_does_not_pollute_babysitter_summary_window() {
        // Heartbeats are pure liveness; they must not advance the window or
        // count fed to the babysitter (otherwise a long quiet turn would look
        // like a flurry of activity to the babysitter). (TASK-100.3)
        let mut buf = BabysitterSummaryBuffer::default();
        let now = Instant::now();
        update_babysitter_buffer(&mut buf, &DriverEvent::Heartbeat { seq: 0 }, 7, now);
        update_babysitter_buffer(&mut buf, &DriverEvent::Heartbeat { seq: 1 }, 8, now);
        assert_eq!(buf.count, 0);
        assert_eq!(buf.window_end_seq, 0);
        assert!(buf.headline.is_empty());

        // A real event after heartbeats still records cleanly.
        update_babysitter_buffer(
            &mut buf,
            &DriverEvent::TextChunk {
                stream: orgasmic_core::TextStream::Assistant,
                chunk: "working".into(),
                seq: 0,
            },
            9,
            now,
        );
        assert_eq!(buf.count, 1);
        assert_eq!(buf.window_start_seq, 9);
        assert_eq!(buf.window_end_seq, 9);
    }

    fn impl_req(task: &str, dir: &Path) -> AcquireRequest {
        AcquireRequest {
            task_id: task.into(),
            kind: RunKind::Worker,
            worker_id: "implementer-claude-acp".into(),
            role: "implementer".into(),
            project_id: Some("orgasmic".into()),
            worktree: None,
            last_path: None,
            stdout_path: None,
            dispatch_attempt_token: None,
            session_path: dir.join(format!("{task}.jsonl")),
            driver_config: tmux::inert_config(),
            babysitter_target: None,
            stall_timeout_secs: None,
            max_run_duration_secs: None,
            idle_timeout_secs: None,
            babysitter: None,
        }
    }

    /// CLI-dispatch-shaped acquire: artifact paths present so the run
    /// advertises the worker-finalize completion contract.
    fn dispatch_impl_req(task: &str, dir: &Path) -> AcquireRequest {
        let mut req = impl_req(task, dir);
        req.last_path = Some(dir.join(format!("{task}.last.txt")));
        req.stdout_path = Some(dir.join(format!("{task}.stdout.log")));
        req
    }

    /// Stage grill-shaped acquire: last_path present so the universal
    /// finalize contract applies (TASK-S52X9).
    fn stage_grill_req(task: &str, dir: &Path) -> AcquireRequest {
        let mut req = impl_req(task, dir);
        req.role = "griller".into();
        req.worker_id = "griller".into();
        req.last_path = Some(dir.join(format!("{task}.last.txt")));
        req.stall_timeout_secs = Some(0);
        req.max_run_duration_secs = Some(0);
        req
    }

    fn artifactor_req(task: &str, dir: &Path) -> AcquireRequest {
        let mut req = impl_req(task, dir);
        req.role = "artifactor".into();
        req.worker_id = "artifactor".into();
        req.last_path = None;
        req.stdout_path = None;
        req.stall_timeout_secs = Some(0);
        req.max_run_duration_secs = Some(0);
        req
    }

    fn manager_req(task: &str, dir: &Path) -> AcquireRequest {
        let mut req = impl_req(task, dir);
        req.role = "manager".into();
        req.worker_id = "manager".into();
        req.last_path = None;
        req.stdout_path = None;
        req.stall_timeout_secs = Some(0);
        req.max_run_duration_secs = Some(0);
        req
    }

    fn manual_req(
        task: &str,
        dir: &Path,
        stall_timeout_secs: Option<u32>,
        max_run_duration_secs: Option<u32>,
    ) -> AcquireRequest {
        let mut req = impl_req(task, dir);
        req.stall_timeout_secs = stall_timeout_secs;
        req.max_run_duration_secs = max_run_duration_secs;
        req
    }

    fn idle_req(task: &str, dir: &Path, idle_timeout_secs: Option<u32>) -> AcquireRequest {
        let mut req = impl_req(task, dir);
        req.idle_timeout_secs = idle_timeout_secs;
        req
    }

    /// Mirrors the exact shape `spawn_worker_run` (api.rs) now produces for
    /// an Artifactor && rmux dispatch (TASK-NZ3C9): stall disabled via
    /// `Some(0)`, idle enabled via `DEFAULT_IDLE_TIMEOUT_SECS`.
    fn persistent_artifactor_req(task: &str, dir: &Path) -> AcquireRequest {
        let mut req = impl_req(task, dir);
        req.stall_timeout_secs = Some(0);
        req.idle_timeout_secs = Some(DEFAULT_IDLE_TIMEOUT_SECS);
        req
    }

    #[tokio::test]
    async fn snapshot_includes_driver_harness() {
        let (sup, dir, _w) = make_supervisor();
        let driver = tmux::driver();
        let resp = sup
            .acquire(&driver, impl_req("TASK-HARNESS", dir.path()))
            .await
            .unwrap();

        let snapshot = sup.snapshot().await;
        let run = snapshot
            .runs
            .iter()
            .find(|run| run.run_id == resp.run_id)
            .expect("live run");
        assert_eq!(run.driver, "tmux");
        assert_eq!(run.harness.as_deref(), Some("claude"));
    }

    #[tokio::test]
    async fn acquire_sets_working_sub_state_from_role_before_heartbeat() {
        let (sup, dir, _w) = make_supervisor();
        let driver = tmux::driver();
        let mut req = impl_req("TASK-ACQUIRE-SUBSTATE", dir.path());
        req.worker_id = "reviewer-claude-rmux".into();
        req.role = "reviewer".into();
        let resp = sup.acquire(&driver, req).await.unwrap();

        let snapshot = sup.snapshot().await;
        let run = snapshot
            .runs
            .iter()
            .find(|run| run.run_id == resp.run_id)
            .expect("live run");
        assert_eq!(run.kind, "reviewer");
        assert_eq!(run.run_kind, RunKind::Worker);
        assert_eq!(
            run.sub_state.as_ref().map(RunSubState::as_str),
            Some("reviewer.working")
        );
    }

    #[tokio::test]
    async fn transition_state_updates_snapshot_sub_state() {
        let (sup, dir, _w) = make_supervisor();
        let driver = TmuxTuiDriver;
        let resp = sup
            .acquire(&driver, impl_req("TASK-SUBSTATE", dir.path()))
            .await
            .unwrap();
        let before = event_count(&sup, &resp.run_id).await;
        sup.transition_state(
            &resp.run_id,
            TransitionRequest {
                from: "implementer.working".into(),
                to: "reviewer.approved".into(),
                reason: "fake transition".into(),
            },
            &resp.identity,
        )
        .await
        .unwrap();
        wait_for_event_count(&sup, &resp.run_id, before + 1).await;

        let snapshot = sup.snapshot().await;
        let run = snapshot
            .runs
            .iter()
            .find(|run| run.run_id == resp.run_id)
            .expect("live run");
        assert_eq!(
            run.sub_state.as_ref().map(RunSubState::as_str),
            Some("reviewer.approved")
        );
        let encoded = serde_json::to_value(&snapshot).unwrap();
        assert_eq!(
            encoded["runs"][0]["sub_state"].as_str(),
            Some("reviewer.approved")
        );
    }

    async fn age_run(
        sup: &Supervisor,
        run_id: &str,
        last_driver_event_age: Option<Duration>,
        run_age: Option<Duration>,
    ) {
        let now = Instant::now();
        let mut g = sup.inner.lock().await;
        let rec = g.runs.get_mut(run_id).expect("run exists");
        if let Some(age) = last_driver_event_age {
            rec.last_driver_event_at = now - age;
        }
        if let Some(age) = run_age {
            rec.run_started_at = now - age;
        }
    }

    async fn age_input(sup: &Supervisor, run_id: &str, last_input_age: Duration) {
        let now = Instant::now();
        let mut g = sup.inner.lock().await;
        let rec = g.runs.get_mut(run_id).expect("run exists");
        rec.last_input_at = now - last_input_age;
    }

    async fn run_is_live(sup: &Supervisor, run_id: &str) -> bool {
        sup.snapshot()
            .await
            .runs
            .iter()
            .any(|run| run.run_id == run_id)
    }

    async fn event_count(sup: &Supervisor, run_id: &str) -> u64 {
        sup.snapshot()
            .await
            .runs
            .iter()
            .find(|run| run.run_id == run_id)
            .map(|run| run.event_count)
            .unwrap_or(0)
    }

    async fn wait_for_event_count(sup: &Supervisor, run_id: &str, count: u64) {
        // Deadline-based, not iteration-based: under full-suite parallel load
        // a fixed yield budget expires before the event task gets scheduled.
        // (Under start_paused runtimes the sleep auto-advances virtual time.)
        let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
        loop {
            let seen = sup
                .snapshot()
                .await
                .runs
                .iter()
                .find(|run| run.run_id == run_id)
                .map(|run| run.event_count)
                .unwrap_or(0);
            if seen >= count {
                return;
            }
            if tokio::time::Instant::now() >= deadline {
                panic!("run {run_id} reached event_count {seen}, wanted {count}");
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    }

    fn assert_release_reason(path: &Path, reason: &str) {
        let release = session_events(path)
            .into_iter()
            .find(|envelope| {
                envelope.kind == SessionEventKind::Lifecycle
                    && envelope.event.get("phase").and_then(|phase| phase.as_str())
                        == Some("release")
            })
            .expect("release lifecycle event");
        assert_eq!(
            release.event.get("reason").and_then(|value| value.as_str()),
            Some(reason)
        );
        assert_eq!(
            release
                .event
                .get("outcome")
                .and_then(|value| value.as_str()),
            Some("failed")
        );
    }

    fn has_release_reason(path: &Path, reason: &str) -> bool {
        session_events(path).into_iter().any(|envelope| {
            envelope.kind == SessionEventKind::Lifecycle
                && envelope.event.get("phase").and_then(|phase| phase.as_str()) == Some("release")
                && envelope
                    .event
                    .get("reason")
                    .and_then(|value| value.as_str())
                    == Some(reason)
        })
    }

    #[tokio::test]
    async fn stall_detector_releases_after_no_driver_events() {
        let (sup, dir, _w) = make_supervisor();
        let driver = TmuxTuiDriver;
        let req = manual_req("TASK-STALL", dir.path(), Some(1), None);
        let session_path = req.session_path.clone();
        let resp = sup.acquire(&driver, req).await.unwrap();

        age_run(&sup, &resp.run_id, Some(Duration::from_millis(1_001)), None).await;
        sup.release_first_timed_out_run().await;

        assert!(!run_is_live(&sup, &resp.run_id).await);
        assert_release_reason(&session_path, "stall_timeout_exceeded");
    }

    /// `Some(0)` disables both detectors: an interactive (manager) run parked
    /// at its prompt for far longer than every default threshold must survive
    /// the timeout sweep instead of being reaped as "stalled".
    #[tokio::test]
    async fn zero_timeouts_disable_stall_and_ceiling() {
        let (sup, dir, _w) = make_supervisor();
        let driver = TmuxTuiDriver;
        let req = manual_req("TASK-INTERACTIVE", dir.path(), Some(0), Some(0));
        let resp = sup.acquire(&driver, req).await.unwrap();

        let week = Duration::from_secs(7 * 24 * 3600);
        age_run(&sup, &resp.run_id, Some(week), Some(week)).await;
        sup.release_first_timed_out_run().await;

        assert!(
            run_is_live(&sup, &resp.run_id).await,
            "0-second timeouts must disable the sweep, not fire immediately"
        );
    }

    /// Mixed config: stall disabled but ceiling kept — the ceiling still fires.
    #[tokio::test]
    async fn zero_stall_timeout_keeps_explicit_ceiling() {
        let (sup, dir, _w) = make_supervisor();
        let driver = TmuxTuiDriver;
        let req = manual_req("TASK-CEILING-ONLY", dir.path(), Some(0), Some(1));
        let session_path = req.session_path.clone();
        let resp = sup.acquire(&driver, req).await.unwrap();

        age_run(
            &sup,
            &resp.run_id,
            Some(Duration::from_secs(3600)),
            Some(Duration::from_millis(1_001)),
        )
        .await;
        sup.release_first_timed_out_run().await;

        assert!(!run_is_live(&sup, &resp.run_id).await);
        assert_release_reason(&session_path, "max_run_duration_exceeded");
    }

    /// D3 of arch_045Q0.2 (TASK-F9N5F): a persistent artifactor run idle past
    /// its window is released, freeing the `artifact.generate:{id}` lease so
    /// the next regenerate cold-spawns instead of hitting `LeaseHeld`.
    #[tokio::test]
    async fn idle_timeout_releases_persistent_run_and_frees_lease() {
        let (sup, dir, _w) = make_supervisor();
        let driver = AcceptingInputDriver;
        let req = idle_req("TASK-IDLE", dir.path(), Some(1));
        let session_path = req.session_path.clone();
        let resp = sup.acquire(&driver, req).await.unwrap();

        // Age both clocks: idle now requires last_input_at AND
        // last_driver_event_at to be stale (TASK-NZ3C9 RULING 2).
        age_input(&sup, &resp.run_id, Duration::from_millis(1_001)).await;
        age_run(&sup, &resp.run_id, Some(Duration::from_millis(1_001)), None).await;
        sup.release_first_timed_out_run().await;

        assert!(!run_is_live(&sup, &resp.run_id).await);
        assert_release_reason(&session_path, "idle_timeout_exceeded");

        let reacquired = sup
            .acquire(&driver, idle_req("TASK-IDLE", dir.path(), Some(1)))
            .await
            .expect("lease must be freed after idle release so regenerate cold-spawns");
        assert_ne!(reacquired.run_id, resp.run_id);
    }

    /// The idle timer resets on every ACCEPTED send_input, independent of
    /// last_driver_event_at (stall) or run_started_at (max). Without the
    /// reset, this run would already be past its deadline when the sweep
    /// runs and would be incorrectly released.
    #[tokio::test]
    async fn idle_timeout_resets_on_accepted_send_input() {
        let (sup, dir, _w) = make_supervisor();
        let driver = AcceptingInputDriver;
        let req = idle_req("TASK-IDLE-RESET", dir.path(), Some(1));
        let session_path = req.session_path.clone();
        let resp = sup.acquire(&driver, req).await.unwrap();

        // Hold the driver-event clock stale for the whole test (no driver
        // output at all) so only the input-clock reset behavior under test
        // can keep the run alive — otherwise (TASK-NZ3C9 RULING 2) a fresh
        // last_driver_event_at would mask input staleness on its own.
        age_run(&sup, &resp.run_id, Some(Duration::from_secs(10)), None).await;

        age_input(&sup, &resp.run_id, Duration::from_millis(1_001)).await;
        let ack = sup
            .send_input(&resp.run_id, "keep going".into(), &resp.identity)
            .await
            .unwrap();
        assert!(ack.accepted);
        sup.release_first_timed_out_run().await;
        assert!(
            run_is_live(&sup, &resp.run_id).await,
            "accepted send_input must reset the idle clock, pushing the deadline out"
        );

        // Advance past the window again from the reset baseline: now it fires.
        age_input(&sup, &resp.run_id, Duration::from_millis(1_001)).await;
        sup.release_first_timed_out_run().await;
        assert!(!run_is_live(&sup, &resp.run_id).await);
        assert_release_reason(&session_path, "idle_timeout_exceeded");
    }

    /// One-shot/non-artifactor runs (`idle_timeout_secs: None`) must never be
    /// idle-released, no matter how long input has been silent — only the
    /// persistent artifactor spawn path opts in.
    #[tokio::test]
    async fn non_persistent_run_never_idle_released() {
        let (sup, dir, _w) = make_supervisor();
        let driver = TmuxTuiDriver;
        let req = manual_req("TASK-NOT-PERSISTENT", dir.path(), Some(0), Some(0));
        let resp = sup.acquire(&driver, req).await.unwrap();

        age_input(&sup, &resp.run_id, Duration::from_secs(7 * 24 * 3600)).await;
        sup.release_first_timed_out_run().await;

        assert!(
            run_is_live(&sup, &resp.run_id).await,
            "idle_timeout_secs: None must keep idle detection disabled"
        );
    }

    /// TASK-NZ3C9 (F9N5F reviewer HIGH-1): the 600s stall timeout used to
    /// pre-empt the 900s idle window, killing a persistent artifactor run
    /// long before idle ever got a chance to fire. With `spawn_worker_run`
    /// now setting `stall_timeout_secs: Some(0)` for Artifactor && rmux, a
    /// run that has gone quiet on the driver-event clock past the OLD 600s
    /// stall point (with the idle clock still fresh) must survive the
    /// sweep. Reverting the `Some(0)` guard in api.rs — or `resolve_timeout_secs`
    /// treating `Some(0)` as anything but "disabled" — would make this fail.
    #[tokio::test]
    async fn persistent_artifactor_stall_disabled_survives_old_stall_point() {
        let (sup, dir, _w) = make_supervisor();
        let driver = AcceptingInputDriver;
        let req = persistent_artifactor_req("TASK-ARTIFACTOR-STALL", dir.path());
        let resp = sup.acquire(&driver, req).await.unwrap();

        // Only the driver-event clock goes stale, past the old 600s stall
        // threshold; last_input_at stays fresh so idle cannot be what saves
        // this run — only stall being disabled can.
        age_run(&sup, &resp.run_id, Some(Duration::from_secs(601)), None).await;
        sup.release_first_timed_out_run().await;

        assert!(
            run_is_live(&sup, &resp.run_id).await,
            "stall_timeout_secs: Some(0) must disable stall for persistent artifactor runs"
        );
    }

    /// TASK-NZ3C9 (F9N5F reviewer MEDIUM-1): idle release must require BOTH
    /// `last_input_at` and `last_driver_event_at` to be stale. A run that is
    /// actively streaming driver output (fresh `last_driver_event_at`) must
    /// never be idle-released even if input has gone quiet — only once both
    /// clocks fall silent does idle fire. This fails if the idle candidate
    /// reverts to keying on `last_input_at` alone.
    #[tokio::test]
    async fn idle_release_requires_both_clocks_stale() {
        let (sup, dir, _w) = make_supervisor();
        let driver = AcceptingInputDriver;
        let req = idle_req("TASK-IDLE-BOTH-CLOCKS", dir.path(), Some(1));
        let session_path = req.session_path.clone();
        let resp = sup.acquire(&driver, req).await.unwrap();

        // Age only last_input_at; last_driver_event_at stays fresh, as if the
        // driver were continuously streaming output. Must NOT release.
        age_input(&sup, &resp.run_id, Duration::from_millis(1_001)).await;
        sup.release_first_timed_out_run().await;
        assert!(
            run_is_live(&sup, &resp.run_id).await,
            "a run with fresh driver events must not be idle-released even if input is stale"
        );

        // Now age last_driver_event_at too, so BOTH clocks are stale.
        age_run(&sup, &resp.run_id, Some(Duration::from_millis(1_001)), None).await;
        sup.release_first_timed_out_run().await;

        assert!(!run_is_live(&sup, &resp.run_id).await);
        assert_release_reason(&session_path, "idle_timeout_exceeded");
    }

    /// TASK-NZ3C9 (F9N5F reviewer LOW-1): production-shaped regression —
    /// exercises the exact `AcquireRequest` shape `spawn_worker_run` now
    /// builds for a persistent artifactor (stall disabled, idle enabled),
    /// ages both clocks together past the idle window, and asserts both the
    /// release reason and that the lease is freed for the next dispatch.
    #[tokio::test]
    async fn persistent_artifactor_idle_release_frees_lease_production_shaped() {
        let (sup, dir, _w) = make_supervisor();
        let driver = AcceptingInputDriver;
        let req = persistent_artifactor_req("TASK-ARTIFACTOR-IDLE", dir.path());
        let session_path = req.session_path.clone();
        let resp = sup.acquire(&driver, req).await.unwrap();

        let past_idle_window = Duration::from_secs(u64::from(DEFAULT_IDLE_TIMEOUT_SECS) + 1);
        age_input(&sup, &resp.run_id, past_idle_window).await;
        age_run(&sup, &resp.run_id, Some(past_idle_window), None).await;
        sup.release_first_timed_out_run().await;

        assert!(!run_is_live(&sup, &resp.run_id).await);
        assert_release_reason(&session_path, "idle_timeout_exceeded");

        let reacquired = sup
            .acquire(
                &driver,
                persistent_artifactor_req("TASK-ARTIFACTOR-IDLE", dir.path()),
            )
            .await
            .expect("lease must be freed after idle release so regenerate cold-spawns");
        assert_ne!(reacquired.run_id, resp.run_id);
    }

    #[test]
    fn manager_task_ids_are_interactive() {
        assert!(is_interactive_manager_task("manager.launch:orgasmic"));
        assert!(!is_interactive_manager_task("TASK-103.1"));
    }

    #[tokio::test]
    async fn stall_detector_resets_on_driver_event() {
        let (sup, dir, _w) = make_supervisor();
        let driver = TmuxTuiDriver;
        let req = manual_req("TASK-STALL-RESET", dir.path(), Some(1), None);
        let session_path = req.session_path.clone();
        let resp = sup.acquire(&driver, req).await.unwrap();

        let before = event_count(&sup, &resp.run_id).await;
        sup.transition_state(
            &resp.run_id,
            TransitionRequest {
                from: "ready".into(),
                to: "in_progress".into(),
                reason: "still active".into(),
            },
            &resp.identity,
        )
        .await
        .unwrap();
        wait_for_event_count(&sup, &resp.run_id, before + 1).await;
        sup.release_first_timed_out_run().await;
        assert!(
            run_is_live(&sup, &resp.run_id).await,
            "fresh driver_event should reset stall detector"
        );

        age_run(&sup, &resp.run_id, Some(Duration::from_millis(1_001)), None).await;
        sup.release_first_timed_out_run().await;

        assert!(!run_is_live(&sup, &resp.run_id).await);
        assert_release_reason(&session_path, "stall_timeout_exceeded");
    }

    #[tokio::test]
    async fn stall_detector_revalidates_after_driver_event_race() {
        let (sup, dir, _w) = make_supervisor();
        let driver = TmuxTuiDriver;
        let req = manual_req("TASK-STALL-RACE", dir.path(), Some(1), None);
        let session_path = req.session_path.clone();
        let resp = sup.acquire(&driver, req).await.unwrap();
        wait_for_event_count(&sup, &resp.run_id, 1).await;

        age_run(&sup, &resp.run_id, Some(Duration::from_millis(1_001)), None).await;
        let hook_sup = sup.clone();
        let hook_run_id = resp.run_id.clone();
        let hook_identity = resp.identity.clone();
        sup.release_first_timed_out_run_after_candidate(move || async move {
            let before = event_count(&hook_sup, &hook_run_id).await;
            hook_sup
                .transition_state(
                    &hook_run_id,
                    TransitionRequest {
                        from: "ready".into(),
                        to: "in_progress".into(),
                        reason: "driver event won the timeout race".into(),
                    },
                    &hook_identity,
                )
                .await
                .unwrap();
            wait_for_event_count(&hook_sup, &hook_run_id, before + 1).await;
        })
        .await;

        assert!(
            run_is_live(&sup, &resp.run_id).await,
            "fresh driver_event in the selection/release gap should abort timeout release"
        );
        assert!(
            !has_release_reason(&session_path, "stall_timeout_exceeded"),
            "stale timeout candidate must not write a release event"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn run_timeout_monitor_releases_via_spawned_task() {
        let (sup, dir, _w) = make_supervisor();
        tokio::task::yield_now().await;
        let driver = TmuxTuiDriver;
        let req = manual_req("TASK-MONITOR", dir.path(), Some(1), None);
        let session_path = req.session_path.clone();
        let resp = sup.acquire(&driver, req).await.unwrap();
        wait_for_event_count(&sup, &resp.run_id, 1).await;

        for _ in 0..30 {
            tokio::time::advance(Duration::from_millis(50)).await;
            tokio::task::yield_now().await;
            if !run_is_live(&sup, &resp.run_id).await {
                break;
            }
        }

        assert!(
            !run_is_live(&sup, &resp.run_id).await,
            "spawned monitor task should release the stalled run"
        );
        for _ in 0..20 {
            if has_release_reason(&session_path, "stall_timeout_exceeded") {
                return;
            }
            tokio::task::yield_now().await;
        }
        assert_release_reason(&session_path, "stall_timeout_exceeded");
    }

    #[tokio::test]
    async fn max_run_duration_releases_even_with_driver_events() {
        let (sup, dir, _w) = make_supervisor();
        let driver = TmuxTuiDriver;
        let req = manual_req("TASK-MAX-RUN", dir.path(), Some(10), Some(1));
        let session_path = req.session_path.clone();
        let resp = sup.acquire(&driver, req).await.unwrap();

        let before = event_count(&sup, &resp.run_id).await;
        sup.transition_state(
            &resp.run_id,
            TransitionRequest {
                from: "ready".into(),
                to: "in_progress".into(),
                reason: "active".into(),
            },
            &resp.identity,
        )
        .await
        .unwrap();
        wait_for_event_count(&sup, &resp.run_id, before + 1).await;
        age_run(
            &sup,
            &resp.run_id,
            Some(Duration::from_millis(1)),
            Some(Duration::from_millis(1_001)),
        )
        .await;
        sup.release_first_timed_out_run().await;

        assert!(!run_is_live(&sup, &resp.run_id).await);
        assert_release_reason(&session_path, "max_run_duration_exceeded");
    }

    fn command_available_for_test(command: &str) -> bool {
        Command::new("which")
            .arg(command)
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .map(|status| status.success())
            .unwrap_or(false)
    }

    async fn tmux_spawn_usable_for_test() -> bool {
        if !command_available_for_test("tmux") {
            return false;
        }
        let session = format!(
            "orgasmic-supervisor-probe-{}",
            Utc::now().timestamp_nanos_opt().unwrap_or(0)
        );
        let status = tokio::process::Command::new("tmux")
            .args(["new-session", "-d", "-s", &session, "--", "sleep", "1"])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .await;
        let ok = status.map(|status| status.success()).unwrap_or(false);
        if ok {
            let _ = tokio::process::Command::new("tmux")
                .args(["kill-session", "-t", &session])
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .status()
                .await;
        }
        ok
    }

    async fn tmux_has_session_for_test(session: &str) -> bool {
        tokio::process::Command::new("tmux")
            .args(["has-session", "-t", session])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .await
            .map(|status| status.success())
            .unwrap_or(false)
    }

    async fn wait_for_run_release(sup: &Supervisor, run_id: &str, timeout: Duration) {
        let start = Instant::now();
        loop {
            let snapshot = sup.snapshot().await;
            if snapshot.runs.iter().all(|run| run.run_id != run_id) {
                return;
            }
            assert!(
                start.elapsed() < timeout,
                "run {run_id} did not release within {timeout:?}"
            );
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    }

    fn session_events(path: &Path) -> Vec<SessionEnvelope> {
        read_session_file(path).unwrap_or_default()
    }

    fn eligibility_task() -> TaskConstraints<'static> {
        TaskConstraints {
            kind: WorkerKind::Implementer,
            provider: None,
            model: None,
            reasoning_effort: None,
        }
    }

    fn eligibility_worker(id: &'static str) -> Worker<'static> {
        Worker {
            id,
            kind: WorkerKind::Implementer,
            driver: "tmux",
            harness: "codex",
            providers: vec!["openai".to_string()],
            models: vec!["gpt-5".to_string()],
            reasoning_efforts: vec!["high".to_string()],
            default_provider: Some("openai".to_string()),
            default_model: Some("gpt-5".to_string()),
            default_effort: Some("high".to_string()),
            linked_skills: Vec::new(),
            applicable_states: Vec::new(),
            max_iterations: None,
            context_budget: None,
            stall_timeout_secs: None,
            max_run_duration_secs: None,
            babysitter_worker: None,
            sandbox_permissions: None,
            harness_args: Vec::new(),
            version: None,
            persona: None,
            operating_rules: None,
        }
    }

    fn assert_ownership_mismatch(
        err: SupervisorError,
        field: &'static str,
        expected: &str,
        got: &str,
        run_id: &str,
    ) {
        match err {
            SupervisorError::OwnershipMismatch {
                field: actual_field,
                expected: actual_expected,
                got: actual_got,
                run_id: actual_run_id,
            } => {
                assert_eq!(actual_field, field);
                assert_eq!(actual_expected, expected);
                assert_eq!(actual_got, got);
                assert_eq!(actual_run_id, run_id);
            }
            other => panic!("expected OwnershipMismatch, got {other:?}"),
        }
    }

    fn test_envelope(
        seq: u64,
        kind: SessionEventKind,
        event: serde_json::Value,
    ) -> SessionEnvelope {
        SessionEnvelope {
            seq,
            time: Utc::now(),
            run_id: "run-test".into(),
            runtime_id: "rt-test".into(),
            boot_id: "boot-test".into(),
            kind,
            event,
        }
    }

    fn lifecycle_acquire(seq: u64) -> SessionEnvelope {
        test_envelope(
            seq,
            SessionEventKind::Lifecycle,
            serde_json::to_value(Lifecycle::Acquire {
                task_id: "TASK-075.1".into(),
                kind: "implementer".into(),
                worker_id: "implementer-codex-stdio".into(),
            })
            .unwrap(),
        )
    }

    fn driver_event(seq: u64, event: DriverEvent) -> SessionEnvelope {
        test_envelope(
            seq,
            SessionEventKind::DriverEvent,
            serde_json::to_value(event).unwrap(),
        )
    }

    #[test]
    fn driver_error_only_session_is_early_exit_no_work() {
        let envelopes = vec![
            lifecycle_acquire(0),
            driver_event(
                1,
                DriverEvent::Ready {
                    protocol_version: "codex-appserver/1".into(),
                    capabilities: json!({}),
                },
            ),
            driver_event(
                2,
                DriverEvent::DriverError {
                    fatal: true,
                    message: "cursor-agent killed before work".into(),
                },
            ),
        ];

        assert!(session_is_early_exit_no_work(&envelopes));
    }

    #[test]
    fn auto_select_worker_returns_none_when_no_eligible() {
        let mut task = eligibility_task();
        task.provider = Some("anthropic");
        let workers = vec![eligibility_worker("w1")];

        let selected = auto_select_worker(&task, &workers);

        assert!(selected.is_none());
    }

    #[test]
    fn auto_select_worker_picks_first_eligible_in_input_order() {
        let workers = vec![eligibility_worker("w1"), eligibility_worker("w2")];

        let selected = auto_select_worker(&eligibility_task(), &workers).unwrap();

        assert_eq!(selected.id, "w1");
    }

    #[test]
    fn auto_select_worker_prefers_fully_specified() {
        let mut task = eligibility_task();
        task.model = Some("gpt-5");
        let mut partial = eligibility_worker("partial");
        partial.default_model = None;
        partial.default_effort = None;
        partial.reasoning_efforts = Vec::new();
        let full = eligibility_worker("full");
        let workers = vec![partial, full];

        let selected = auto_select_worker(&task, &workers).unwrap();

        assert_eq!(selected.id, "full");
    }

    #[tokio::test]
    async fn acquire_writes_acquire_lifecycle_and_driver_events() {
        let (sup, dir, _w) = make_supervisor();
        let driver = TmuxTuiDriver;
        let resp = sup
            .acquire(&driver, impl_req("TASK-001", dir.path()))
            .await
            .unwrap();
        // Allow the drain task to pick up the Ready event.
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        sup.release(&resp.run_id, "done", ReleaseOutcome::Completed)
            .await
            .unwrap();
        let env = orgasmic_core::read_session_file(dir.path().join("TASK-001.jsonl")).unwrap();
        // Expect: Lifecycle::Acquire, DriverEvent::Ready, DriverEvent::RunComplete (from release),
        // Lifecycle::Release. Order: Acquire (sync), then drain races with release write — both
        // synchronized through the same writer, so the lifecycle release comes after drain has
        // consumed Ready. We just assert the three categories are present.
        let kinds: Vec<_> = env.iter().map(|e| e.kind).collect();
        assert!(kinds.contains(&SessionEventKind::Lifecycle));
        assert!(kinds.contains(&SessionEventKind::DriverEvent));
        let lifecycle_count = kinds
            .iter()
            .filter(|k| **k == SessionEventKind::Lifecycle)
            .count();
        assert!(lifecycle_count >= 2, "acquire + release lifecycle");
    }

    #[tokio::test]
    async fn tmux_process_exit_releases_supervisor_run_and_session() {
        let _live_guard = live_session_guard();
        if !tmux_spawn_usable_for_test().await || !command_available_for_test("bash") {
            eprintln!(
                "skipping tmux_process_exit_releases_supervisor_run_and_session: tmux/bash unavailable"
            );
            return;
        }
        let (sup, dir, _w) = make_supervisor();
        let driver = TmuxTuiDriver;
        let mut req = dispatch_impl_req("TASK-TMUX-MOCK", dir.path());
        req.worker_id = "implementer-claude-tmux".into();
        // Dispatch-shaped acquire with last_path advertises finalize contract;
        // pane exit without finalize must tombstone Failed.
        req.driver_config = DriverConfig::from_value(json!({
            "command": "bash",
            "args": ["-lc", "printf 'mock output\\n'; exit 0"],
        }));
        let resp = sup.acquire(&driver, req).await.unwrap();
        let session = tmux::tmux_session_name(&resp.identity);
        wait_for_run_release(&sup, &resp.run_id, Duration::from_secs(8)).await;
        tokio::time::sleep(Duration::from_millis(200)).await;
        assert!(
            !tmux_has_session_for_test(&session).await,
            "tmux session should be killed after process-exit release"
        );

        let events = session_events(&dir.path().join("TASK-TMUX-MOCK.jsonl"));
        assert!(events.iter().any(|envelope| {
            envelope.kind == SessionEventKind::DriverEvent
                && matches!(
                    serde_json::from_value::<DriverEvent>(envelope.event.clone()),
                    Ok(DriverEvent::Ready { .. })
                )
        }));
        assert!(
            !events.iter().any(|envelope| {
                envelope.kind == SessionEventKind::DriverEvent
                    && matches!(
                        serde_json::from_value::<DriverEvent>(envelope.event.clone()),
                        Ok(DriverEvent::TextChunk { .. })
                    )
            }),
            "capture removal must not synthesize TextChunk from scrollback"
        );
        assert!(events.iter().any(|envelope| {
            envelope.kind == SessionEventKind::Lifecycle
                && matches!(
                    serde_json::from_value::<Lifecycle>(envelope.event.clone()),
                    Ok(Lifecycle::Release {
                        reason,
                        outcome: ReleaseOutcome::Failed,
                        finalized_by_worker: false,
                        ..
                    }) if reason == "protocol_end_without_finalize"
                )
        }));
    }

    #[tokio::test]
    async fn second_acquire_for_same_task_kind_is_rejected() {
        let (sup, dir, _w) = make_supervisor();
        let driver = TmuxTuiDriver;
        let _r1 = sup
            .acquire(&driver, impl_req("TASK-007", dir.path()))
            .await
            .unwrap();
        let err = sup
            .acquire(&driver, impl_req("TASK-007", dir.path()))
            .await
            .unwrap_err();
        assert!(matches!(err, SupervisorError::LeaseHeld { .. }));
    }

    #[tokio::test]
    async fn release_frees_the_lease() {
        let (sup, dir, _w) = make_supervisor();
        let driver = TmuxTuiDriver;
        let r1 = sup
            .acquire(&driver, impl_req("TASK-100", dir.path()))
            .await
            .unwrap();
        sup.release(&r1.run_id, "done", ReleaseOutcome::Completed)
            .await
            .unwrap();
        let r2 = sup
            .acquire(&driver, impl_req("TASK-100", dir.path()))
            .await
            .expect("can re-acquire after release");
        assert_ne!(r2.run_id, r1.run_id);
    }

    #[tokio::test]
    async fn orphaned_lease_release_semantics() {
        let (sup, dir, _w) = make_supervisor();
        let driver = TmuxTuiDriver;
        // No lease at all → nothing to clear.
        assert_eq!(
            sup.release_orphaned_lease("TASK-ORPHAN", RunKind::Worker)
                .await,
            OrphanedLeaseOutcome::NoLease
        );
        let r1 = sup
            .acquire(&driver, impl_req("TASK-ORPHAN", dir.path()))
            .await
            .unwrap();
        // A live run holds the lease → refuse to steal it.
        assert_eq!(
            sup.release_orphaned_lease("TASK-ORPHAN", RunKind::Worker)
                .await,
            OrphanedLeaseOutcome::HeldByLiveRun {
                run_id: r1.run_id.clone()
            }
        );
        // Orphan the lease: drop the run record while the lease stays behind
        // (the zombie-lease shape produced by a mid-acquire failure).
        {
            let mut g = sup.inner.lock().await;
            g.runs.remove(&r1.run_id);
        }
        assert_eq!(
            sup.release_orphaned_lease("TASK-ORPHAN", RunKind::Worker)
                .await,
            OrphanedLeaseOutcome::Released {
                run_id: r1.run_id.clone()
            }
        );
        // The task is dispatchable again without any restart.
        sup.acquire(&driver, impl_req("TASK-ORPHAN", dir.path()))
            .await
            .expect("lease cleared; fresh acquire succeeds");
    }

    #[tokio::test]
    async fn different_kinds_share_a_task_id() {
        let (sup, dir, _w) = make_supervisor();
        let driver = TmuxTuiDriver;
        let r1 = sup
            .acquire(&driver, impl_req("TASK-K", dir.path()))
            .await
            .unwrap();
        let bs_req = AcquireRequest {
            task_id: "TASK-K".into(),
            kind: RunKind::Babysitter,
            worker_id: "babysitter-stall-detector".into(),
            role: "babysitter".into(),
            project_id: None,
            worktree: None,
            last_path: None,
            stdout_path: None,
            dispatch_attempt_token: None,
            session_path: dir.path().join(format!("{}.babysitter.jsonl", r1.run_id)),
            driver_config: tmux::inert_config(),
            babysitter_target: Some(r1.run_id.clone()),
            stall_timeout_secs: None,
            max_run_duration_secs: None,
            idle_timeout_secs: None,
            babysitter: None,
        };
        let r2 = sup.acquire(&driver, bs_req).await.unwrap();
        assert_ne!(r1.run_id, r2.run_id);
    }

    #[tokio::test]
    async fn runtime_identity_carries_run_runtime_boot() {
        let (sup, dir, _w) = make_supervisor();
        let driver = TmuxTuiDriver;
        let resp = sup
            .acquire(&driver, impl_req("TASK-RT", dir.path()))
            .await
            .unwrap();
        assert!(!resp.identity.run_id.is_empty());
        assert!(!resp.identity.runtime_id.is_empty());
        assert!(!resp.identity.boot_id.is_empty());
        // Matches the supervisor's boot.
        assert_eq!(resp.identity.boot_id, sup.boot.boot_id);
    }

    #[tokio::test]
    async fn boot_id_ownership_mismatch_blocks_state_mutation() {
        let (sup, dir, _w) = make_supervisor();
        let driver = TmuxTuiDriver;
        let resp = sup
            .acquire(&driver, impl_req("TASK-O", dir.path()))
            .await
            .unwrap();
        let stale = RuntimeIdentity {
            run_id: resp.identity.run_id.clone(),
            runtime_id: resp.identity.runtime_id.clone(),
            boot_id: "different-boot".into(),
        };
        let err = sup
            .transition_state(
                &resp.run_id,
                TransitionRequest {
                    from: "ready".into(),
                    to: "in_progress".into(),
                    reason: "x".into(),
                },
                &stale,
            )
            .await
            .unwrap_err();
        assert_ownership_mismatch(
            err,
            "boot_id",
            &resp.identity.boot_id,
            &stale.boot_id,
            &stale.run_id,
        );
    }

    #[tokio::test]
    async fn run_id_ownership_mismatch_blocks_state_mutation() {
        let (sup, dir, _w) = make_supervisor();
        let driver = TmuxTuiDriver;
        let resp = sup
            .acquire(&driver, impl_req("TASK-O-RUN", dir.path()))
            .await
            .unwrap();
        let stale = RuntimeIdentity {
            run_id: "stale-run".into(),
            runtime_id: resp.identity.runtime_id.clone(),
            boot_id: resp.identity.boot_id.clone(),
        };
        let err = sup
            .transition_state(
                &resp.run_id,
                TransitionRequest {
                    from: "ready".into(),
                    to: "in_progress".into(),
                    reason: "x".into(),
                },
                &stale,
            )
            .await
            .unwrap_err();
        assert_ownership_mismatch(
            err,
            "run_id",
            &resp.identity.run_id,
            &stale.run_id,
            &stale.run_id,
        );
    }

    #[tokio::test]
    async fn runtime_id_ownership_mismatch_blocks_state_mutation() {
        let (sup, dir, _w) = make_supervisor();
        let driver = TmuxTuiDriver;
        let resp = sup
            .acquire(&driver, impl_req("TASK-O-RUNTIME", dir.path()))
            .await
            .unwrap();
        let stale = RuntimeIdentity {
            run_id: resp.identity.run_id.clone(),
            runtime_id: "stale-runtime".into(),
            boot_id: resp.identity.boot_id.clone(),
        };
        let err = sup
            .transition_state(
                &resp.run_id,
                TransitionRequest {
                    from: "ready".into(),
                    to: "in_progress".into(),
                    reason: "x".into(),
                },
                &stale,
            )
            .await
            .unwrap_err();
        assert_ownership_mismatch(
            err,
            "runtime_id",
            &resp.identity.runtime_id,
            &stale.runtime_id,
            &stale.run_id,
        );
    }

    // TASK-DWJVH item A: `release_with_finalization` gains the same
    // self-consistency `caller_identity` guard `transition_state`/`send_input`
    // already have (dec_3M7M0 residual, review #4).

    #[tokio::test]
    async fn release_with_matching_caller_identity_succeeds() {
        let (sup, dir, _w) = make_supervisor();
        let driver = TmuxTuiDriver;
        let resp = sup
            .acquire(&driver, impl_req("TASK-REL-MATCH", dir.path()))
            .await
            .unwrap();
        sup.release_with_finalization(
            &resp.run_id,
            "worker finalize",
            ReleaseOutcome::Completed,
            true,
            Some(&resp.identity),
        )
        .await
        .unwrap();
        let snapshot = sup.snapshot().await;
        assert!(
            snapshot.runs.iter().all(|run| run.run_id != resp.run_id),
            "run should be released"
        );
    }

    #[tokio::test]
    async fn release_with_mismatched_caller_identity_is_rejected() {
        let (sup, dir, _w) = make_supervisor();
        let driver = TmuxTuiDriver;
        let resp = sup
            .acquire(&driver, impl_req("TASK-REL-MISMATCH", dir.path()))
            .await
            .unwrap();
        let stale = RuntimeIdentity {
            run_id: resp.run_id.clone(),
            runtime_id: "stale-runtime".into(),
            boot_id: resp.identity.boot_id.clone(),
        };
        let err = sup
            .release_with_finalization(
                &resp.run_id,
                "worker finalize",
                ReleaseOutcome::Completed,
                true,
                Some(&stale),
            )
            .await
            .unwrap_err();
        assert_ownership_mismatch(
            err,
            "runtime_id",
            &resp.identity.runtime_id,
            &stale.runtime_id,
            &stale.run_id,
        );
        // Rejected release must not have removed the run.
        let snapshot = sup.snapshot().await;
        assert!(
            snapshot.runs.iter().any(|run| run.run_id == resp.run_id),
            "run must still be live after a rejected release"
        );
    }

    #[tokio::test]
    async fn release_with_no_caller_identity_still_releases() {
        // The human manager path (dispatch-close/lease-release) sends no
        // identity at all — must keep working exactly as before.
        let (sup, dir, _w) = make_supervisor();
        let driver = TmuxTuiDriver;
        let resp = sup
            .acquire(&driver, impl_req("TASK-REL-NONE", dir.path()))
            .await
            .unwrap();
        sup.release_with_finalization(
            &resp.run_id,
            "manager release",
            ReleaseOutcome::Completed,
            false,
            None,
        )
        .await
        .unwrap();
        let snapshot = sup.snapshot().await;
        assert!(
            snapshot.runs.iter().all(|run| run.run_id != resp.run_id),
            "run should be released"
        );
    }

    // TASK-DWJVH item B: the finalize-vs-stall-sweep race. The stall sweep
    // releases the run (Failed/stall_timeout_exceeded) in the window between
    // the worker resolving the run and the worker's own finalize release
    // landing. At the supervisor layer this must stay a plain, well-formed
    // `RunNotFound` — no panic, no corrupted lease/run state — so the CLI
    // layer (`cmd_dispatch_finalize`) can treat "already released" as
    // success-with-warning instead of a hard error (review #5 residual).
    #[tokio::test]
    async fn release_after_stall_sweep_already_released_is_clean_run_not_found() {
        let (sup, dir, _w) = make_supervisor();
        let driver = TmuxTuiDriver;
        let resp = sup
            .acquire(&driver, impl_req("TASK-REL-RACE", dir.path()))
            .await
            .unwrap();
        // Simulate the stall sweep: it releases with no caller identity and
        // `finalized_by_worker: false`, exactly like `release_first_timed_out_run`.
        sup.release(
            &resp.run_id,
            "stall_timeout_exceeded",
            ReleaseOutcome::Failed,
        )
        .await
        .unwrap();
        assert_release_reason(
            &dir.path().join("TASK-REL-RACE.jsonl"),
            "stall_timeout_exceeded",
        );

        // The worker's own finalize call — its commit + last.txt write are
        // already durable by this point — now finds the run gone.
        let err = sup
            .release_with_finalization(
                &resp.run_id,
                "worker finalize for TASK-REL-RACE",
                ReleaseOutcome::Completed,
                true,
                Some(&resp.identity),
            )
            .await
            .unwrap_err();
        let err_display = err.to_string();
        assert!(
            matches!(err, SupervisorError::RunNotFound(ref id) if *id == resp.run_id),
            "expected a plain RunNotFound for the already-released run, got {err_display:?}"
        );
        // No leftover lease or run state from the failed second release.
        let snapshot = sup.snapshot().await;
        assert!(snapshot.runs.iter().all(|run| run.run_id != resp.run_id));
    }

    // TASK-P4MGK: ACP protocol-end vs worker finalize must not double-release
    // or race the lease. Cover finalize-then-protocol-end, protocol-end-then-
    // finalize, and finalize against an already-released run.

    #[tokio::test]
    async fn finalize_then_protocol_end_does_not_double_release() {
        // orgasmic:TASK-P4MGK
        let (sup, dir, _w) = make_supervisor();
        let driver = FinalizeThenProtocolEndDriver;
        let resp = sup
            .acquire(
                &driver,
                dispatch_impl_req("TASK-FIN-THEN-PROTO", dir.path()),
            )
            .await
            .unwrap();
        sup.release_with_finalization(
            &resp.run_id,
            "worker finalize for TASK-FIN-THEN-PROTO",
            ReleaseOutcome::Completed,
            true,
            Some(&resp.identity),
        )
        .await
        .unwrap();
        // Let the post-release drain observe RunComplete + stream close.
        tokio::time::sleep(Duration::from_millis(100)).await;

        let path = dir.path().join("TASK-FIN-THEN-PROTO.jsonl");
        let releases: Vec<_> = session_events(&path)
            .into_iter()
            .filter(|envelope| {
                envelope.kind == SessionEventKind::Lifecycle
                    && envelope.event.get("phase").and_then(|p| p.as_str()) == Some("release")
            })
            .collect();
        assert_eq!(
            releases.len(),
            1,
            "finalize then protocol-end must write exactly one Release, got {releases:?}"
        );
        assert_eq!(
            releases[0]
                .event
                .get("finalized_by_worker")
                .and_then(|v| v.as_bool()),
            Some(true)
        );
        assert!(
            !has_release_reason(&path, "protocol_end_without_finalize"),
            "stream-end must not write a second release after finalize claimed the run"
        );
        let snapshot = sup.snapshot().await;
        assert!(snapshot.runs.iter().all(|run| run.run_id != resp.run_id));
    }

    #[tokio::test]
    async fn protocol_end_then_finalize_is_clean_run_not_found() {
        // orgasmic:TASK-P4MGK
        let (sup, dir, _w) = make_supervisor();
        let driver = ProtocolEndAcpDriver;
        let resp = sup
            .acquire(
                &driver,
                dispatch_impl_req("TASK-PROTO-THEN-FIN", dir.path()),
            )
            .await
            .unwrap();
        let path = dir.path().join("TASK-PROTO-THEN-FIN.jsonl");
        let deadline = Instant::now() + Duration::from_secs(2);
        while Instant::now() < deadline {
            if has_release_reason(&path, "protocol_end_without_finalize") {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        assert_release_reason(&path, "protocol_end_without_finalize");

        let err = sup
            .release_with_finalization(
                &resp.run_id,
                "worker finalize for TASK-PROTO-THEN-FIN",
                ReleaseOutcome::Completed,
                true,
                Some(&resp.identity),
            )
            .await
            .unwrap_err();
        assert!(
            matches!(err, SupervisorError::RunNotFound(ref id) if *id == resp.run_id),
            "finalize after protocol-end must be clean RunNotFound, got {err}"
        );
        let releases: Vec<_> = session_events(&path)
            .into_iter()
            .filter(|envelope| {
                envelope.kind == SessionEventKind::Lifecycle
                    && envelope.event.get("phase").and_then(|p| p.as_str()) == Some("release")
            })
            .collect();
        assert_eq!(
            releases.len(),
            1,
            "protocol-end must not leave a double-release trail: {releases:?}"
        );
    }

    #[tokio::test]
    async fn finalize_after_already_released_acp_run_is_clean_run_not_found() {
        // orgasmic:TASK-P4MGK
        let (sup, dir, _w) = make_supervisor();
        let driver = FinalizeThenProtocolEndDriver;
        let resp = sup
            .acquire(&driver, dispatch_impl_req("TASK-ALREADY-REL", dir.path()))
            .await
            .unwrap();
        sup.release(
            &resp.run_id,
            "manual release before finalize",
            ReleaseOutcome::Interrupted,
        )
        .await
        .unwrap();
        let err = sup
            .release_with_finalization(
                &resp.run_id,
                "worker finalize for TASK-ALREADY-REL",
                ReleaseOutcome::Completed,
                true,
                Some(&resp.identity),
            )
            .await
            .unwrap_err();
        assert!(
            matches!(err, SupervisorError::RunNotFound(ref id) if *id == resp.run_id),
            "finalize on already-released ACP run must be clean RunNotFound, got {err}"
        );
    }

    #[test]
    fn stream_end_release_downgrades_dispatch_acp_protocol_complete_to_failed() {
        // orgasmic:TASK-P4MGK
        let (reason, outcome) =
            stream_end_release_for_transport("acp-stdio", Some(ReleaseOutcome::Completed), true);
        assert_eq!(reason, "protocol_end_without_finalize");
        assert_eq!(outcome, ReleaseOutcome::Failed);

        let (reason, outcome) = stream_end_release_for_transport(
            "subprocess-stream-json",
            Some(ReleaseOutcome::Completed),
            true,
        );
        assert_eq!(reason, "protocol_end_without_finalize");
        assert_eq!(outcome, ReleaseOutcome::Failed);

        // TUI transports obey the same declaration gate as stream-end
        // (TASK-TZJFF / TASK-S52X9).
        let (reason, outcome) =
            stream_end_release_for_transport("rmux", Some(ReleaseOutcome::Completed), true);
        assert_eq!(reason, "protocol_end_without_finalize");
        assert_eq!(outcome, ReleaseOutcome::Failed);
    }

    #[test]
    fn stream_end_release_keeps_protocol_complete_when_finalize_contract_absent() {
        // Runs that do not advertise the terminal-declaration contract
        // (babysitter, architect stage without last_path, etc.) still treat
        // protocol-end as success.
        let (reason, outcome) =
            stream_end_release_for_transport("acp-stdio", Some(ReleaseOutcome::Completed), false);
        assert_eq!(reason, "driver stream closed");
        assert_eq!(outcome, ReleaseOutcome::Completed);
    }

    #[test]
    fn run_requires_worker_finalize_is_universal_per_shape() {
        // orgasmic:TASK-S52X9
        let last = Some(PathBuf::from("/tmp/x.last.txt"));
        assert!(run_requires_worker_finalize(&last, "implementer"));
        assert!(run_requires_worker_finalize(&last, "griller"));
        assert!(run_requires_worker_finalize(&last, "planner"));
        assert!(run_requires_worker_finalize(&None, "artifactor"));
        assert!(run_requires_worker_finalize(&None, "manager"));
        assert!(!run_requires_worker_finalize(&None, "implementer"));
        assert!(!run_requires_worker_finalize(&None, "griller"));
        assert!(!run_requires_worker_finalize(&None, "babysitter"));
        assert!(!run_requires_worker_finalize(&None, "terminal"));
        // Fail closed for unknown historical agent roles (TASK-ARZGD).
        assert!(run_requires_worker_finalize(&None, "worker"));
    }

    #[tokio::test]
    async fn grill_tui_protocol_end_without_finalize_is_failed() {
        // orgasmic:TASK-TZJFF
        let (sup, dir, _w) = make_supervisor();
        let driver = ProtocolEndTuiDriver;
        let resp = sup
            .acquire(&driver, stage_grill_req("TASK-GRILL-TUI-PROTO", dir.path()))
            .await
            .unwrap();
        let path = dir.path().join("TASK-GRILL-TUI-PROTO.jsonl");
        let deadline = Instant::now() + Duration::from_secs(2);
        while Instant::now() < deadline {
            if has_release_reason(&path, "protocol_end_without_finalize") {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        assert_release_reason(&path, "protocol_end_without_finalize");
        let snapshot = sup.snapshot().await;
        assert!(snapshot.runs.iter().all(|run| run.run_id != resp.run_id));
    }

    #[tokio::test]
    async fn artifactor_stale_declaration_after_regenerate_round_fails_on_protocol_end() {
        // orgasmic:TASK-TZJFF
        let (sup, dir, _w) = make_supervisor();
        let driver = ProtocolEndTuiDriver;
        let resp = sup
            .acquire(
                &driver,
                artifactor_req("artifact.generate:ART-ROUND1", dir.path()),
            )
            .await
            .unwrap();
        sup.mark_terminal_declaration(&resp.run_id, "artifact_submitted")
            .await
            .unwrap();
        // Accepted followup clears regenerate_in_flight; the new round has no
        // declaration, so protocol-end must Fail (TASK-ARZGD / TASK-TZJFF).
        let _checkpoint = sup
            .begin_artifactor_regenerate_round(&resp.run_id)
            .await
            .unwrap();
        sup.commit_artifactor_regenerate_round(&resp.run_id, _checkpoint)
            .await
            .unwrap();
        let path = dir.path().join("artifact.generate:ART-ROUND1.jsonl");
        let deadline = Instant::now() + Duration::from_secs(2);
        while Instant::now() < deadline {
            if has_release_reason(&path, "protocol_end_without_finalize") {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        assert_release_reason(&path, "protocol_end_without_finalize");
    }

    #[tokio::test]
    async fn artifactor_in_flight_submit_defers_protocol_end_without_false_completed() {
        // orgasmic:TASK-99W9C — in-flight submit must never write Completed
        // before the durable writer transaction commits.
        let (sup, dir, _w) = make_supervisor();
        let gate = Arc::new(tokio::sync::Notify::new());
        let driver = GatedProtocolEndDriver {
            gate: Arc::clone(&gate),
        };
        let resp = sup
            .acquire(
                &driver,
                artifactor_req("artifact.generate:ART-INFLIGHT", dir.path()),
            )
            .await
            .unwrap();
        let token = sup
            .prepare_artifactor_submit_in_flight(&resp.run_id)
            .await
            .unwrap();
        gate.notify_one();
        let path = dir.path().join("artifact.generate:ART-INFLIGHT.jsonl");
        let deadline = Instant::now() + Duration::from_secs(2);
        while Instant::now() < deadline {
            if session_events(&path)
                .iter()
                .filter(|envelope| envelope.kind == SessionEventKind::DriverEvent)
                .count()
                >= 2
            {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        assert!(
            !session_events(&path).iter().any(|envelope| {
                envelope.kind == SessionEventKind::Lifecycle
                    && envelope.event.get("phase").and_then(|p| p.as_str()) == Some("release")
                    && envelope.event.get("reason").and_then(|v| v.as_str())
                        == Some("artifact_submitted")
                    && envelope.event.get("outcome").and_then(|v| v.as_str()) == Some("completed")
            }),
            "in-flight submit must not write a false Completed tombstone"
        );
        sup.abort_artifactor_submit_in_flight(&resp.run_id, token)
            .await
            .unwrap();
        let deadline = Instant::now() + Duration::from_secs(2);
        while Instant::now() < deadline {
            if has_release_reason(&path, "artifact_submit_failed") {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        assert_release_reason(&path, "artifact_submit_failed");
    }

    #[tokio::test]
    async fn artifactor_in_flight_submit_commit_resolves_deferred_protocol_end_completed() {
        // orgasmic:TASK-99W9C — deferred terminal resolves Completed only after
        // commit promotes the durable declaration.
        let (sup, dir, _w) = make_supervisor();
        let gate = Arc::new(tokio::sync::Notify::new());
        let driver = GatedProtocolEndDriver {
            gate: Arc::clone(&gate),
        };
        let resp = sup
            .acquire(
                &driver,
                artifactor_req("artifact.generate:ART-COMMIT", dir.path()),
            )
            .await
            .unwrap();
        let token = sup
            .prepare_artifactor_submit_in_flight(&resp.run_id)
            .await
            .unwrap();
        gate.notify_one();
        let path = dir.path().join("artifact.generate:ART-COMMIT.jsonl");
        let deadline = Instant::now() + Duration::from_secs(2);
        while Instant::now() < deadline {
            if session_events(&path)
                .iter()
                .filter(|envelope| envelope.kind == SessionEventKind::DriverEvent)
                .count()
                >= 2
            {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        sup.commit_artifactor_submit_in_flight(&resp.run_id, token)
            .await
            .unwrap();
        let deadline = Instant::now() + Duration::from_secs(2);
        while Instant::now() < deadline {
            if session_events(&path).iter().any(|envelope| {
                envelope.kind == SessionEventKind::Lifecycle
                    && envelope.event.get("reason").and_then(|v| v.as_str())
                        == Some("artifact_submitted")
                    && envelope.event.get("outcome").and_then(|v| v.as_str()) == Some("completed")
            }) {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        assert!(
            session_events(&path).iter().any(|envelope| {
                envelope.kind == SessionEventKind::Lifecycle
                    && envelope.event.get("reason").and_then(|v| v.as_str())
                        == Some("artifact_submitted")
                    && envelope.event.get("outcome").and_then(|v| v.as_str()) == Some("completed")
            }),
            "commit after deferred protocol-end must write artifact_submitted Completed"
        );
    }

    #[tokio::test]
    async fn manager_protocol_end_without_finalize_is_failed_after_contract_restore() {
        // orgasmic:TASK-99W9C — manager runs with the terminal contract must
        // fail closed on protocol-end without a declaration.
        let (sup, dir, _w) = make_supervisor();
        let driver = ProtocolEndTuiDriver;
        let resp = sup
            .acquire(&driver, manager_req("manager.launch:proj", dir.path()))
            .await
            .unwrap();
        let path = dir.path().join("manager.launch:proj.jsonl");
        let deadline = Instant::now() + Duration::from_secs(2);
        while Instant::now() < deadline {
            if has_release_reason(&path, "protocol_end_without_finalize") {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        assert_release_reason(&path, "protocol_end_without_finalize");
        let snapshot = sup.snapshot().await;
        assert!(snapshot.runs.iter().all(|run| run.run_id != resp.run_id));
    }

    #[tokio::test]
    async fn custom_terminal_protocol_end_is_exempt_from_finalize_contract() {
        let (sup, dir, _w) = make_supervisor();
        let driver = ProtocolEndAcpDriver;
        let mut req = manager_req("manager.launch:proj:custom", dir.path());
        req.role = "terminal".into();
        let _resp = sup.acquire(&driver, req).await.unwrap();
        let path = dir.path().join("manager.launch:proj:custom.jsonl");
        let deadline = Instant::now() + Duration::from_secs(2);
        while Instant::now() < deadline {
            if session_events(&path)
                .iter()
                .any(|envelope| envelope.kind == SessionEventKind::Lifecycle)
            {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        assert!(
            !has_release_reason(&path, "protocol_end_without_finalize"),
            "custom terminal must be exempt from the finalize contract"
        );
    }

    #[tokio::test]
    async fn continuation_protocol_end_without_finalize_is_failed() {
        // orgasmic:TASK-99W9C — continuation drains use the same terminal gate.
        let (sup, dir, _w) = make_supervisor();
        let driver = ProtocolEndAcpDriver;
        let mut req = dispatch_impl_req("TASK-CONT-PROTO", dir.path());
        req.role = "implementer".into();
        let seed = ContinuationSeed {
            previous_run: "run-prev".into(),
            previous_session_path: dir.path().join("prev.jsonl"),
            diff_summary: "no diff".into(),
            acceptance_criteria: vec!["ac1".into()],
        };
        let _resp = sup.acquire_continuation(&driver, req, seed).await.unwrap();
        let path = dir.path().join("TASK-CONT-PROTO.jsonl");
        let deadline = Instant::now() + Duration::from_secs(2);
        while Instant::now() < deadline {
            if has_release_reason(&path, "protocol_end_without_finalize") {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        assert_release_reason(&path, "protocol_end_without_finalize");
    }

    #[tokio::test]
    async fn artifactor_regenerate_rejected_followup_restores_declaration() {
        let (sup, dir, _w) = make_supervisor();
        let driver = tmux::driver();
        let resp = sup
            .acquire(
                &driver,
                artifactor_req("artifact.generate:ART-ROLLBACK", dir.path()),
            )
            .await
            .unwrap();
        sup.mark_terminal_declaration(&resp.run_id, "artifact_submitted")
            .await
            .unwrap();
        let checkpoint = sup
            .begin_artifactor_regenerate_round(&resp.run_id)
            .await
            .unwrap();
        assert!(matches!(
            sup.prepare_artifactor_submit_in_flight(&resp.run_id).await,
            Err(SupervisorError::ArtifactorLifecycleBusy(_))
        ));
        assert_eq!(checkpoint.terminal_round, 0);
        assert!(checkpoint.terminal_declaration.is_some());
        sup.rollback_artifactor_regenerate_round(&resp.run_id, checkpoint)
            .await
            .unwrap();
        let checkpoint2 = sup
            .begin_artifactor_regenerate_round(&resp.run_id)
            .await
            .unwrap();
        assert_eq!(
            checkpoint2.terminal_round, 0,
            "rollback must restore the prior round"
        );
        assert!(
            checkpoint2.terminal_declaration.is_some(),
            "rollback must restore the prior declaration"
        );
        sup.rollback_artifactor_regenerate_round(&resp.run_id, checkpoint2)
            .await
            .unwrap();
        let token = sup
            .prepare_artifactor_submit_in_flight(&resp.run_id)
            .await
            .unwrap();
        assert!(matches!(
            sup.begin_artifactor_regenerate_round(&resp.run_id).await,
            Err(SupervisorError::ArtifactorLifecycleBusy(_))
        ));
        sup.abort_artifactor_submit_in_flight(&resp.run_id, token)
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn artifactor_submit_for_task_is_atomic_and_timeout_defers_while_in_flight() {
        // orgasmic:TASK-ARZGD P1 — find+token under one lock; timeout never
        // false-Fails an in-flight submit.
        let (sup, dir, _w) = make_supervisor();
        let driver = AcceptingInputDriver;
        let mut req = artifactor_req("artifact.generate:ART-ATOMIC", dir.path());
        req.idle_timeout_secs = Some(1);
        let resp = sup.acquire(&driver, req).await.unwrap();
        let (run_id, token) = sup
            .begin_artifactor_submit_for_task("artifact.generate:ART-ATOMIC")
            .await
            .unwrap();
        assert_eq!(run_id, resp.run_id);

        // Force the idle clock past the threshold while submit is in flight.
        {
            let mut g = sup.inner.lock().await;
            let rec = g.runs.get_mut(&run_id).unwrap();
            rec.last_input_at = Instant::now() - Duration::from_secs(10);
            rec.last_driver_event_at = Instant::now() - Duration::from_secs(10);
            rec.run_started_at = Instant::now() - Duration::from_secs(10);
        }
        match sup
            .release_with_finalization(
                &run_id,
                "idle_timeout_exceeded",
                ReleaseOutcome::Failed,
                false,
                None,
            )
            .await
        {
            Err(SupervisorError::DeferredWhileInFlight(_)) => {}
            other => panic!("timeout during submit_in_flight must defer: {other:?}"),
        }
        let path = dir.path().join("artifact.generate:ART-ATOMIC.jsonl");
        assert!(
            !has_release_reason(&path, "idle_timeout_exceeded"),
            "timeout must not write a false Failed while submit is in flight"
        );

        sup.commit_artifactor_submit_in_flight(&run_id, token)
            .await
            .unwrap();
        let deadline = Instant::now() + Duration::from_secs(2);
        while Instant::now() < deadline {
            if has_release_reason(&path, "artifact_submitted") {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        assert!(
            session_events(&path).iter().any(|envelope| {
                envelope.kind == SessionEventKind::Lifecycle
                    && envelope.event.get("reason").and_then(|v| v.as_str())
                        == Some("artifact_submitted")
                    && envelope.event.get("outcome").and_then(|v| v.as_str()) == Some("completed")
            }),
            "deferred timeout after successful commit must resolve Completed, not Failed"
        );
    }

    #[tokio::test]
    async fn artifactor_operator_cancel_waits_for_submit_then_records_cancelled() {
        // orgasmic:TASK-ARZGD OQ2
        let (sup, dir, _w) = make_supervisor();
        let driver = AcceptingInputDriver;
        let resp = sup
            .acquire(
                &driver,
                artifactor_req("artifact.generate:ART-CANCEL", dir.path()),
            )
            .await
            .unwrap();
        let (run_id, token) = sup
            .begin_artifactor_submit_for_task("artifact.generate:ART-CANCEL")
            .await
            .unwrap();
        assert_eq!(run_id, resp.run_id);
        match sup
            .release_with_finalization(
                &run_id,
                "run released",
                ReleaseOutcome::Cancelled,
                false,
                None,
            )
            .await
        {
            Err(SupervisorError::DeferredWhileInFlight(_)) => {}
            other => panic!("cancel during submit_in_flight must defer: {other:?}"),
        }
        sup.commit_artifactor_submit_in_flight(&run_id, token)
            .await
            .unwrap();
        let path = dir.path().join("artifact.generate:ART-CANCEL.jsonl");
        let deadline = Instant::now() + Duration::from_secs(2);
        while Instant::now() < deadline {
            if has_release_reason(&path, "cancelled") {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        assert!(
            session_events(&path).iter().any(|envelope| {
                envelope.kind == SessionEventKind::Lifecycle
                    && envelope.event.get("reason").and_then(|v| v.as_str()) == Some("cancelled")
                    && envelope.event.get("outcome").and_then(|v| v.as_str()) == Some("cancelled")
            }),
            "deferred operator cancel must record Cancelled after writer commit"
        );
    }

    #[tokio::test]
    async fn artifactor_regenerate_in_flight_defers_protocol_end_until_rollback() {
        // orgasmic:TASK-ARZGD P3
        let (sup, dir, _w) = make_supervisor();
        let gate = Arc::new(tokio::sync::Notify::new());
        let driver = GatedProtocolEndDriver {
            gate: Arc::clone(&gate),
        };
        let resp = sup
            .acquire(
                &driver,
                artifactor_req("artifact.generate:ART-REGEN-RACE", dir.path()),
            )
            .await
            .unwrap();
        sup.mark_terminal_declaration(&resp.run_id, "artifact_submitted")
            .await
            .unwrap();
        let checkpoint = sup
            .begin_artifactor_regenerate_round(&resp.run_id)
            .await
            .unwrap();
        gate.notify_one();
        let path = dir.path().join("artifact.generate:ART-REGEN-RACE.jsonl");
        let deadline = Instant::now() + Duration::from_secs(2);
        while Instant::now() < deadline {
            if session_events(&path)
                .iter()
                .filter(|envelope| envelope.kind == SessionEventKind::DriverEvent)
                .count()
                >= 2
            {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        assert!(
            !has_release_reason(&path, "protocol_end_without_finalize"),
            "protocol-end during regenerate_in_flight must defer, not false-Fail"
        );
        sup.rollback_artifactor_regenerate_round(&resp.run_id, checkpoint)
            .await
            .unwrap();
        let deadline = Instant::now() + Duration::from_secs(2);
        while Instant::now() < deadline {
            if has_release_reason(&path, "artifact_submitted") {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        assert!(
            session_events(&path).iter().any(|envelope| {
                envelope.kind == SessionEventKind::Lifecycle
                    && envelope.event.get("reason").and_then(|v| v.as_str())
                        == Some("artifact_submitted")
                    && envelope.event.get("outcome").and_then(|v| v.as_str()) == Some("completed")
            }),
            "rollback after deferred drain must restore prior Completed declaration"
        );
    }

    #[tokio::test]
    async fn grill_protocol_end_without_finalize_is_failed() {
        // orgasmic:TASK-S52X9
        let (sup, dir, _w) = make_supervisor();
        let driver = ProtocolEndAcpDriver;
        let resp = sup
            .acquire(&driver, stage_grill_req("TASK-GRILL-PROTO", dir.path()))
            .await
            .unwrap();
        let path = dir.path().join("TASK-GRILL-PROTO.jsonl");
        let deadline = Instant::now() + Duration::from_secs(2);
        while Instant::now() < deadline {
            if has_release_reason(&path, "protocol_end_without_finalize") {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        assert_release_reason(&path, "protocol_end_without_finalize");
        let releases: Vec<_> = session_events(&path)
            .into_iter()
            .filter(|envelope| {
                envelope.kind == SessionEventKind::Lifecycle
                    && envelope.event.get("phase").and_then(|p| p.as_str()) == Some("release")
            })
            .collect();
        assert_eq!(releases.len(), 1, "expected one release: {releases:?}");
        assert_eq!(
            releases[0].event.get("outcome").and_then(|v| v.as_str()),
            Some("failed")
        );
        let snapshot = sup.snapshot().await;
        assert!(snapshot.runs.iter().all(|run| run.run_id != resp.run_id));
    }

    #[tokio::test]
    async fn grill_finalize_completes_with_worker_tombstone() {
        // orgasmic:TASK-S52X9
        let (sup, dir, _w) = make_supervisor();
        let driver = FinalizeThenProtocolEndDriver;
        let resp = sup
            .acquire(&driver, stage_grill_req("TASK-GRILL-FIN", dir.path()))
            .await
            .unwrap();
        sup.release_with_finalization(
            &resp.run_id,
            "worker finalize for TASK-GRILL-FIN",
            ReleaseOutcome::Completed,
            true,
            Some(&resp.identity),
        )
        .await
        .unwrap();
        let path = dir.path().join("TASK-GRILL-FIN.jsonl");
        let releases: Vec<_> = session_events(&path)
            .into_iter()
            .filter(|envelope| {
                envelope.kind == SessionEventKind::Lifecycle
                    && envelope.event.get("phase").and_then(|p| p.as_str()) == Some("release")
            })
            .collect();
        assert_eq!(releases.len(), 1, "expected one release: {releases:?}");
        assert_eq!(
            releases[0]
                .event
                .get("finalized_by_worker")
                .and_then(|v| v.as_bool()),
            Some(true)
        );
        assert_eq!(
            releases[0].event.get("outcome").and_then(|v| v.as_str()),
            Some("completed")
        );
    }

    #[tokio::test]
    async fn artifactor_protocol_end_without_submit_is_failed() {
        // orgasmic:TASK-S52X9
        let (sup, dir, _w) = make_supervisor();
        let driver = ProtocolEndAcpDriver;
        let resp = sup
            .acquire(
                &driver,
                artifactor_req("artifact.generate:ART-TEST1", dir.path()),
            )
            .await
            .unwrap();
        let path = dir.path().join("artifact.generate:ART-TEST1.jsonl");
        let deadline = Instant::now() + Duration::from_secs(2);
        while Instant::now() < deadline {
            if has_release_reason(&path, "protocol_end_without_finalize") {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        assert_release_reason(&path, "protocol_end_without_finalize");
        let snapshot = sup.snapshot().await;
        assert!(snapshot.runs.iter().all(|run| run.run_id != resp.run_id));
    }

    #[tokio::test]
    async fn artifactor_submit_tombstone_completes() {
        // orgasmic:TASK-S52X9
        let (sup, dir, _w) = make_supervisor();
        let driver = FinalizeThenProtocolEndDriver;
        let resp = sup
            .acquire(
                &driver,
                artifactor_req("artifact.generate:ART-TEST2", dir.path()),
            )
            .await
            .unwrap();
        sup.release_with_finalization(
            &resp.run_id,
            "artifact_submitted",
            ReleaseOutcome::Completed,
            true,
            Some(&resp.identity),
        )
        .await
        .unwrap();
        let path = dir.path().join("artifact.generate:ART-TEST2.jsonl");
        assert!(
            session_events(&path).iter().any(|envelope| {
                envelope.kind == SessionEventKind::Lifecycle
                    && envelope.event.get("phase").and_then(|p| p.as_str()) == Some("release")
                    && envelope
                        .event
                        .get("finalized_by_worker")
                        .and_then(|v| v.as_bool())
                        == Some(true)
                    && envelope.event.get("reason").and_then(|v| v.as_str())
                        == Some("artifact_submitted")
                    && envelope.event.get("outcome").and_then(|v| v.as_str()) == Some("completed")
            }),
            "artifact submit must write finalized_by_worker tombstone"
        );
    }

    #[tokio::test]
    async fn manager_release_tombstone_completes() {
        // orgasmic:TASK-S52X9
        let (sup, dir, _w) = make_supervisor();
        let driver = FinalizeThenProtocolEndDriver;
        let resp = sup
            .acquire(&driver, manager_req("manager.launch:proj", dir.path()))
            .await
            .unwrap();
        sup.release_with_finalization(
            &resp.run_id,
            "manager_released",
            ReleaseOutcome::Completed,
            true,
            Some(&resp.identity),
        )
        .await
        .unwrap();
        let path = dir.path().join("manager.launch:proj.jsonl");
        assert!(
            session_events(&path).iter().any(|envelope| {
                envelope.kind == SessionEventKind::Lifecycle
                    && envelope.event.get("phase").and_then(|p| p.as_str()) == Some("release")
                    && envelope
                        .event
                        .get("finalized_by_worker")
                        .and_then(|v| v.as_bool())
                        == Some(true)
                    && envelope.event.get("reason").and_then(|v| v.as_str())
                        == Some("manager_released")
                    && envelope.event.get("outcome").and_then(|v| v.as_str()) == Some("completed")
            }),
            "manager release must write finalized_by_worker tombstone"
        );
    }

    #[tokio::test]
    async fn manager_protocol_end_without_release_is_anomaly() {
        // orgasmic:TASK-S52X9 — unexpected protocol death without release
        // is Failed (anomaly), not silent Completed.
        let (sup, dir, _w) = make_supervisor();
        let driver = ProtocolEndAcpDriver;
        let resp = sup
            .acquire(&driver, manager_req("manager.launch:dead", dir.path()))
            .await
            .unwrap();
        let path = dir.path().join("manager.launch:dead.jsonl");
        let deadline = Instant::now() + Duration::from_secs(2);
        while Instant::now() < deadline {
            if has_release_reason(&path, "protocol_end_without_finalize") {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        assert_release_reason(&path, "protocol_end_without_finalize");
        let snapshot = sup.snapshot().await;
        assert!(snapshot.runs.iter().all(|run| run.run_id != resp.run_id));
    }

    #[tokio::test]
    async fn babysitter_runs_use_separate_jsonl() {
        let (sup, dir, _w) = make_supervisor();
        let driver = TmuxTuiDriver;
        let impl_run = sup
            .acquire(&driver, impl_req("TASK-BS", dir.path()))
            .await
            .unwrap();
        let bs_run = sup
            .spawn_babysitter(
                &driver,
                &impl_run.run_id,
                dir.path(),
                "babysitter-stall-detector",
                tmux::inert_config(),
                (None, None),
            )
            .await
            .unwrap();
        assert!(bs_run.run_id.starts_with("bs-"));
        let bs_path = dir
            .path()
            .join(format!("{}.babysitter.jsonl", impl_run.run_id));
        assert!(bs_path.exists(), "babysitter JSONL exists");
        // Implementer JSONL should record a BabysitterSpawned envelope.
        let impl_env = orgasmic_core::read_session_file(dir.path().join("TASK-BS.jsonl")).unwrap();
        let saw_spawn = impl_env.iter().any(|e| {
            e.kind == SessionEventKind::Lifecycle
                && e.event
                    .get("phase")
                    .and_then(|p| p.as_str())
                    .is_some_and(|p| p == "babysitter_spawned")
        });
        assert!(saw_spawn, "babysitter spawn recorded in target run");
    }

    #[tokio::test]
    async fn babysitter_summary_chunk_collapses_events() {
        let (sup, dir, _w) = make_supervisor();
        let driver = TmuxTuiDriver;
        let impl_run = sup
            .acquire(&driver, impl_req("TASK-S", dir.path()))
            .await
            .unwrap();
        let bs_run = sup
            .spawn_babysitter(
                &driver,
                &impl_run.run_id,
                dir.path(),
                "babysitter-stall-detector",
                tmux::inert_config(),
                (None, None),
            )
            .await
            .unwrap();
        // Drive a few events on the implementer side.
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        sup.transition_state(
            &impl_run.run_id,
            TransitionRequest {
                from: "ready".into(),
                to: "in_progress".into(),
                reason: "go".into(),
            },
            &impl_run.identity,
        )
        .await
        .unwrap();
        tokio::time::sleep(std::time::Duration::from_millis(30)).await;
        let chunk = sup
            .flush_babysitter_summary(&impl_run.run_id, &bs_run.run_id)
            .await
            .unwrap()
            .expect("at least one event accumulated");
        assert!(chunk.event_count >= 1);
        assert!(!chunk.headline.is_empty());
        // Persisted as BabysitterSummary kind on the babysitter's JSONL.
        let bs_env = orgasmic_core::read_session_file(
            dir.path()
                .join(format!("{}.babysitter.jsonl", impl_run.run_id)),
        )
        .unwrap();
        assert!(bs_env
            .iter()
            .any(|e| e.kind == SessionEventKind::BabysitterSummary));
    }

    #[tokio::test]
    async fn live_babysitter_summary_flushes_on_event_threshold() {
        let (sup, dir, _w) = make_supervisor();
        let driver = TmuxTuiDriver;
        let mut req = impl_req("TASK-BS-LIVE", dir.path());
        req.babysitter = Some(BabysitterAutoSpawn {
            worker_id: "babysitter-stall-detector".into(),
            mode: "tmux".into(),
            harness: "claude".into(),
            driver_config: tmux::inert_config(),
            stall_timeout_secs: None,
            max_run_duration_secs: None,
        });
        let impl_run = sup.acquire(&driver, req).await.unwrap();
        let bs_path = dir
            .path()
            .join(format!("{}.babysitter.jsonl", impl_run.run_id));

        for _ in 0..BABYSITTER_SUMMARY_EVENT_THRESHOLD {
            sup.transition_state(
                &impl_run.run_id,
                TransitionRequest {
                    from: "implementer.working".into(),
                    to: "implementer.working".into(),
                    reason: "exercise live summary threshold".into(),
                },
                &impl_run.identity,
            )
            .await
            .unwrap();
        }

        let deadline = Instant::now() + Duration::from_secs(2);
        loop {
            let env = orgasmic_core::read_session_file(&bs_path).unwrap();
            if env
                .iter()
                .any(|e| e.kind == SessionEventKind::BabysitterSummary)
            {
                break;
            }
            assert!(
                Instant::now() < deadline,
                "live summary threshold should append BabysitterSummary to {}",
                bs_path.display()
            );
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    }

    #[tokio::test]
    async fn continuation_run_emits_continuation_envelope() {
        let (sup, dir, _w) = make_supervisor();
        let driver = ClaudeAcpDriver;
        let req = AcquireRequest {
            task_id: "TASK-CONT".into(),
            kind: RunKind::Worker,
            worker_id: "implementer-claude-acp".into(),
            role: "implementer".into(),
            project_id: Some("orgasmic".into()),
            worktree: None,
            last_path: None,
            stdout_path: None,
            dispatch_attempt_token: None,
            session_path: dir.path().join("TASK-CONT.jsonl"),
            driver_config: orgasmic_drivers::adapters::claude::simulated_config(),
            babysitter_target: None,
            stall_timeout_secs: None,
            max_run_duration_secs: None,
            idle_timeout_secs: None,
            babysitter: None,
        };
        let seed = ContinuationSeed {
            previous_run: "run-prev-abc".into(),
            previous_session_path: dir.path().join("run-prev-abc.jsonl"),
            diff_summary: String::new(), // force the static summarizer to fill it
            acceptance_criteria: vec!["AC1".into(), "AC2".into()],
        };
        let resp = sup.acquire_continuation(&driver, req, seed).await.unwrap();
        tokio::time::sleep(std::time::Duration::from_millis(30)).await;
        sup.release(&resp.run_id, "done", ReleaseOutcome::Completed)
            .await
            .unwrap();
        let env = orgasmic_core::read_session_file(dir.path().join("TASK-CONT.jsonl")).unwrap();
        let cont = env.iter().find(|e| {
            e.kind == SessionEventKind::Lifecycle
                && e.event
                    .get("phase")
                    .and_then(|p| p.as_str())
                    .is_some_and(|p| p == "continuation")
        });
        let cont = cont.expect("Continuation envelope present");
        assert_eq!(cont.event["previous_run"].as_str().unwrap(), "run-prev-abc");
        // The static summarizer's text propagates.
        assert!(cont.event["diff_summary"]
            .as_str()
            .unwrap()
            .contains("crates/orgasmic-drivers"));
        assert_eq!(
            cont.event["acceptance_criteria"].as_array().unwrap().len(),
            2
        );
    }

    #[tokio::test]
    async fn babysitter_target_required_for_babysitter_kind() {
        let (sup, dir, _w) = make_supervisor();
        let driver = TmuxTuiDriver;
        let req = AcquireRequest {
            task_id: "TASK-X".into(),
            kind: RunKind::Babysitter,
            worker_id: "babysitter-stall-detector".into(),
            role: "babysitter".into(),
            project_id: None,
            worktree: None,
            last_path: None,
            stdout_path: None,
            dispatch_attempt_token: None,
            session_path: dir.path().join("bs.jsonl"),
            driver_config: tmux::inert_config(),
            babysitter_target: None,
            stall_timeout_secs: None,
            max_run_duration_secs: None,
            idle_timeout_secs: None,
            babysitter: None,
        };
        let err = sup.acquire(&driver, req).await.unwrap_err();
        assert!(matches!(err, SupervisorError::BabysitterTargetInvalid(_)));
    }

    #[tokio::test]
    async fn snapshot_lists_live_runs() {
        let (sup, dir, _w) = make_supervisor();
        let driver = TmuxTuiDriver;
        let _r1 = sup
            .acquire(&driver, impl_req("TASK-A", dir.path()))
            .await
            .unwrap();
        let _r2 = sup
            .acquire(&driver, impl_req("TASK-B", dir.path()))
            .await
            .unwrap();
        let snap = sup.snapshot().await;
        assert_eq!(snap.runs.len(), 2);
        let mut tasks: Vec<_> = snap.runs.iter().map(|r| r.task_id.clone()).collect();
        tasks.sort();
        assert_eq!(tasks, vec!["TASK-A".to_string(), "TASK-B".to_string()]);
        assert!(
            snap.runs.iter().all(|run| run.driver == "tmux-tui"),
            "snapshot driver should come from WorkerDriver::transport at acquire time"
        );
    }

    #[tokio::test]
    async fn babysitter_can_invoke_allowed_tool() {
        let (sup, dir, _w) = make_supervisor();
        let driver = TmuxTuiDriver;
        let impl_run = sup
            .acquire(&driver, impl_req("TASK-BT", dir.path()))
            .await
            .unwrap();
        let bs_run = sup
            .spawn_babysitter(
                &driver,
                &impl_run.run_id,
                dir.path(),
                "babysitter-stall-detector",
                tmux::inert_config(),
                (None, None),
            )
            .await
            .unwrap();
        let ack = sup
            .babysitter_action(
                &bs_run.run_id,
                BabysitterTool::Poke,
                json!({"reason": "checking in"}),
                &bs_run.identity,
            )
            .await
            .unwrap();
        assert!(ack.accepted);
    }

    #[tokio::test]
    async fn acp_stdio_acquire_auto_spawns_babysitter_jsonl() {
        use orgasmic_drivers::adapters::codex::simulated_config;
        use orgasmic_drivers::driver_for_mode_harness;

        let (sup, dir, _w) = make_supervisor();
        let driver = driver_for_mode_harness("acp-stdio", "codex").expect("codex stdio driver");
        let req = AcquireRequest {
            task_id: "TASK-079".into(),
            kind: RunKind::Worker,
            worker_id: "implementer-codex-stdio".into(),
            role: "implementer".into(),
            project_id: Some("orgasmic".into()),
            worktree: None,
            last_path: None,
            stdout_path: None,
            dispatch_attempt_token: None,
            session_path: dir.path().join("TASK-079.jsonl"),
            driver_config: simulated_config(),
            babysitter_target: None,
            stall_timeout_secs: None,
            max_run_duration_secs: None,
            idle_timeout_secs: None,
            babysitter: Some(BabysitterAutoSpawn {
                worker_id: "babysitter-stall-detector".into(),
                mode: "tmux".into(),
                harness: "claude".into(),
                driver_config: tmux::inert_config(),
                stall_timeout_secs: None,
                max_run_duration_secs: None,
            }),
        };
        let impl_run = sup.acquire(driver.as_ref(), req).await.unwrap();
        let bs_path = dir
            .path()
            .join(format!("{}.babysitter.jsonl", impl_run.run_id));
        assert!(
            bs_path.exists(),
            "babysitter JSONL exists for acp-stdio implementer"
        );
    }

    #[tokio::test]
    async fn supervisor_no_spin_on_stale_babysitter_lease() {
        let (sup, dir, _w) = make_supervisor();
        let driver = TmuxTuiDriver;
        sup.inner.lock().await.leases.insert(
            ("TASK-SPIN".to_string(), RunKind::Babysitter),
            "bs-stale-lease".to_string(),
        );

        for idx in 0..25 {
            let req = AcquireRequest {
                task_id: "TASK-SPIN".into(),
                kind: RunKind::Worker,
                worker_id: "implementer-codex-stdio".into(),
                role: "implementer".into(),
                project_id: Some("orgasmic".into()),
                worktree: None,
                last_path: None,
                stdout_path: None,
                dispatch_attempt_token: None,
                session_path: dir.path().join(format!("spin-{idx}.jsonl")),
                driver_config: tmux::inert_config(),
                babysitter_target: None,
                stall_timeout_secs: None,
                max_run_duration_secs: None,
                idle_timeout_secs: None,
                babysitter: Some(BabysitterAutoSpawn {
                    worker_id: "babysitter-stall-detector".into(),
                    mode: "tmux".into(),
                    harness: "claude".into(),
                    driver_config: tmux::inert_config(),
                    stall_timeout_secs: None,
                    max_run_duration_secs: None,
                }),
            };
            let resp = sup.acquire(&driver, req).await.unwrap();
            sup.release(&resp.run_id, "done", ReleaseOutcome::Completed)
                .await
                .unwrap();
        }

        let attempts = sup
            .babysitter_auto_spawn_attempts_for_test("TASK-SPIN")
            .await;
        assert!(
            attempts <= 3,
            "25 dispatch-triggered auto-spawns should stay bounded by backoff; got {attempts}"
        );
    }

    #[tokio::test]
    async fn babysitter_release_clears_auto_spawn_give_up() {
        let (sup, dir, _w) = make_supervisor();
        let driver = TmuxTuiDriver;
        let task_id = "TASK-BS-RECOVER";
        let held_bs = sup
            .acquire(
                &driver,
                AcquireRequest {
                    task_id: task_id.into(),
                    kind: RunKind::Babysitter,
                    worker_id: "babysitter-stall-detector".into(),
                    role: "babysitter".into(),
                    project_id: None,
                    worktree: None,
                    last_path: None,
                    stdout_path: None,
                    dispatch_attempt_token: None,
                    session_path: dir.path().join("held.babysitter.jsonl"),
                    driver_config: tmux::inert_config(),
                    babysitter_target: Some("external-target".into()),
                    stall_timeout_secs: None,
                    max_run_duration_secs: None,
                    idle_timeout_secs: None,
                    babysitter: None,
                },
            )
            .await
            .unwrap();

        let attempts_before = supervisor_metrics().babysitter_spawn_attempts;
        for idx in 0..BABYSITTER_AUTO_SPAWN_MAX_RETRIES {
            let mut req = impl_req(task_id, dir.path());
            req.session_path = dir.path().join(format!("recover-{idx}.jsonl"));
            req.babysitter = Some(BabysitterAutoSpawn {
                worker_id: "babysitter-stall-detector".into(),
                mode: "tmux".into(),
                harness: "claude".into(),
                driver_config: tmux::inert_config(),
                stall_timeout_secs: None,
                max_run_duration_secs: None,
            });
            let resp = sup.acquire(&driver, req).await.unwrap();
            sup.release(&resp.run_id, "done", ReleaseOutcome::Completed)
                .await
                .unwrap();
            if idx + 1 < BABYSITTER_AUTO_SPAWN_MAX_RETRIES {
                sup.force_babysitter_auto_spawn_retry_for_test(task_id)
                    .await;
            }
        }

        assert_eq!(
            sup.babysitter_auto_spawn_attempts_for_test(task_id).await,
            BABYSITTER_AUTO_SPAWN_MAX_RETRIES,
            "lease-held churn should put the task into give-up state"
        );

        sup.release(
            &held_bs.run_id,
            "babysitter lease released",
            ReleaseOutcome::Completed,
        )
        .await
        .unwrap();
        assert_eq!(
            sup.babysitter_auto_spawn_attempts_for_test(task_id).await,
            0,
            "releasing the held babysitter lease should clear give-up state"
        );

        let mut req = impl_req(task_id, dir.path());
        req.session_path = dir.path().join("recover-success.jsonl");
        req.babysitter = Some(BabysitterAutoSpawn {
            worker_id: "babysitter-stall-detector".into(),
            mode: "tmux".into(),
            harness: "claude".into(),
            driver_config: tmux::inert_config(),
            stall_timeout_secs: None,
            max_run_duration_secs: None,
        });
        let resp = sup.acquire(&driver, req).await.unwrap();
        let runs = sup.snapshot().await.runs;
        assert!(
            runs.iter()
                .any(|run| run.run_kind == RunKind::Babysitter && run.task_id == task_id),
            "fresh auto-spawn should succeed after babysitter release resets backoff"
        );
        assert!(
            supervisor_metrics().babysitter_spawn_attempts >= attempts_before + 11,
            "fresh attempt should increment past the give-up threshold"
        );
        sup.release(&resp.run_id, "done", ReleaseOutcome::Completed)
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn implementer_release_cascades_to_auto_spawned_babysitter() {
        use orgasmic_drivers::adapters::codex::simulated_config;
        use orgasmic_drivers::driver_for_mode_harness;

        let (sup, dir, _w) = make_supervisor();
        let driver = driver_for_mode_harness("acp-stdio", "codex").expect("codex stdio driver");
        let req = AcquireRequest {
            task_id: "TASK-082".into(),
            kind: RunKind::Worker,
            worker_id: "implementer-codex-stdio".into(),
            role: "implementer".into(),
            project_id: Some("orgasmic".into()),
            worktree: None,
            last_path: None,
            stdout_path: None,
            dispatch_attempt_token: None,
            session_path: dir.path().join("TASK-082.jsonl"),
            driver_config: simulated_config(),
            babysitter_target: None,
            stall_timeout_secs: None,
            max_run_duration_secs: None,
            idle_timeout_secs: None,
            babysitter: Some(BabysitterAutoSpawn {
                worker_id: "babysitter-stall-detector".into(),
                mode: "tmux".into(),
                harness: "claude".into(),
                driver_config: tmux::inert_config(),
                stall_timeout_secs: None,
                max_run_duration_secs: None,
            }),
        };

        let impl_run = sup.acquire(driver.as_ref(), req).await.unwrap();
        let runs = sup.snapshot().await.runs;
        assert_eq!(runs.len(), 2, "implementer plus babysitter are live");
        let bs_run_id = {
            let guard = sup.inner.lock().await;
            guard
                .runs
                .get(&impl_run.run_id)
                .and_then(|rec| rec.babysitter_run_id.clone())
                .expect("companion babysitter run_id set on implementer")
        };
        assert!(
            runs.iter()
                .any(|run| run.run_id == bs_run_id && run.run_kind == RunKind::Babysitter),
            "companion babysitter is live"
        );

        sup.release(&impl_run.run_id, "done", ReleaseOutcome::Completed)
            .await
            .unwrap();

        let runs_after = sup.snapshot().await.runs;
        assert!(
            !runs_after.iter().any(|run| run.run_id == impl_run.run_id),
            "implementer released"
        );
        assert!(
            !runs_after.iter().any(|run| run.run_id == bs_run_id),
            "babysitter cascade-released"
        );
    }

    fn release_event_count(path: &Path) -> usize {
        orgasmic_core::read_session_file(path)
            .unwrap()
            .iter()
            .filter(|env| {
                env.kind == SessionEventKind::Lifecycle
                    && env.event.get("phase").and_then(|phase| phase.as_str()) == Some("release")
            })
            .count()
    }

    #[tokio::test]
    async fn babysitter_release_before_implementer_release_is_idempotent() {
        use orgasmic_drivers::adapters::codex::simulated_config;
        use orgasmic_drivers::driver_for_mode_harness;

        let (sup, dir, _w) = make_supervisor();
        let driver = driver_for_mode_harness("acp-stdio", "codex").expect("codex stdio driver");
        let req = AcquireRequest {
            task_id: "TASK-083-BS-FIRST".into(),
            kind: RunKind::Worker,
            worker_id: "implementer-codex-stdio".into(),
            role: "implementer".into(),
            project_id: Some("orgasmic".into()),
            worktree: None,
            last_path: None,
            stdout_path: None,
            dispatch_attempt_token: None,
            session_path: dir.path().join("TASK-083-BS-FIRST.jsonl"),
            driver_config: simulated_config(),
            babysitter_target: None,
            stall_timeout_secs: None,
            max_run_duration_secs: None,
            idle_timeout_secs: None,
            babysitter: Some(BabysitterAutoSpawn {
                worker_id: "babysitter-stall-detector".into(),
                mode: "tmux".into(),
                harness: "claude".into(),
                driver_config: tmux::inert_config(),
                stall_timeout_secs: None,
                max_run_duration_secs: None,
            }),
        };

        let impl_run = sup.acquire(driver.as_ref(), req).await.unwrap();
        let runs = sup.snapshot().await.runs;
        assert_eq!(runs.len(), 2, "implementer plus babysitter are live");
        let bs_run_id = {
            let guard = sup.inner.lock().await;
            guard
                .runs
                .get(&impl_run.run_id)
                .and_then(|rec| rec.babysitter_run_id.clone())
                .expect("companion babysitter run_id set on implementer")
        };
        assert!(
            runs.iter()
                .any(|run| run.run_id == bs_run_id && run.run_kind == RunKind::Babysitter),
            "companion babysitter is live"
        );

        sup.release(&bs_run_id, "babysitter done", ReleaseOutcome::Completed)
            .await
            .unwrap();

        let runs_after_babysitter_release = sup.snapshot().await.runs;
        assert!(
            !runs_after_babysitter_release
                .iter()
                .any(|run| run.run_id == bs_run_id),
            "babysitter released first"
        );
        assert!(
            runs_after_babysitter_release
                .iter()
                .any(|run| run.run_id == impl_run.run_id),
            "implementer remains live after babysitter release"
        );

        sup.release(
            &impl_run.run_id,
            "implementer done",
            ReleaseOutcome::Completed,
        )
        .await
        .unwrap();

        let runs_after = sup.snapshot().await.runs;
        assert!(
            !runs_after.iter().any(|run| run.run_id == impl_run.run_id),
            "implementer released"
        );
        assert!(
            !runs_after.iter().any(|run| run.run_id == bs_run_id),
            "already-released babysitter remains released"
        );
        assert_eq!(
            release_event_count(
                &dir.path()
                    .join(format!("{}.babysitter.jsonl", impl_run.run_id))
            ),
            1,
            "cascade RunNotFound is swallowed without writing a second babysitter release"
        );
    }

    #[tokio::test]
    async fn implementer_release_twice_is_idempotent_for_cascade() {
        use orgasmic_drivers::adapters::codex::simulated_config;
        use orgasmic_drivers::driver_for_mode_harness;

        let (sup, dir, _w) = make_supervisor();
        let driver = driver_for_mode_harness("acp-stdio", "codex").expect("codex stdio driver");
        let req = AcquireRequest {
            task_id: "TASK-083-DOUBLE".into(),
            kind: RunKind::Worker,
            worker_id: "implementer-codex-stdio".into(),
            role: "implementer".into(),
            project_id: Some("orgasmic".into()),
            worktree: None,
            last_path: None,
            stdout_path: None,
            dispatch_attempt_token: None,
            session_path: dir.path().join("TASK-083-DOUBLE.jsonl"),
            driver_config: simulated_config(),
            babysitter_target: None,
            stall_timeout_secs: None,
            max_run_duration_secs: None,
            idle_timeout_secs: None,
            babysitter: Some(BabysitterAutoSpawn {
                worker_id: "babysitter-stall-detector".into(),
                mode: "tmux".into(),
                harness: "claude".into(),
                driver_config: tmux::inert_config(),
                stall_timeout_secs: None,
                max_run_duration_secs: None,
            }),
        };

        let impl_run = sup.acquire(driver.as_ref(), req).await.unwrap();
        let runs = sup.snapshot().await.runs;
        assert_eq!(runs.len(), 2, "implementer plus babysitter are live");
        let bs_run_id = {
            let guard = sup.inner.lock().await;
            guard
                .runs
                .get(&impl_run.run_id)
                .and_then(|rec| rec.babysitter_run_id.clone())
                .expect("companion babysitter run_id set on implementer")
        };
        assert!(
            runs.iter()
                .any(|run| run.run_id == bs_run_id && run.run_kind == RunKind::Babysitter),
            "companion babysitter is live"
        );

        sup.release(&impl_run.run_id, "done", ReleaseOutcome::Completed)
            .await
            .unwrap();

        let second_release = sup
            .release(&impl_run.run_id, "retry", ReleaseOutcome::Completed)
            .await;
        assert!(
            matches!(second_release, Err(SupervisorError::RunNotFound(run_id)) if run_id == impl_run.run_id),
            "release of an already-removed implementer should keep the existing RunNotFound contract"
        );

        let runs_after = sup.snapshot().await.runs;
        assert!(
            !runs_after.iter().any(|run| run.run_id == impl_run.run_id),
            "implementer remains released"
        );
        assert!(
            !runs_after.iter().any(|run| run.run_id == bs_run_id),
            "babysitter remains released"
        );
        assert_eq!(
            release_event_count(&dir.path().join("TASK-083-DOUBLE.jsonl")),
            1,
            "second implementer release does not write another release event"
        );
        assert_eq!(
            release_event_count(
                &dir.path()
                    .join(format!("{}.babysitter.jsonl", impl_run.run_id))
            ),
            1,
            "second implementer release does not re-trigger babysitter cascade"
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn poll_direct_child_pid_prefers_worker_server_over_generic_sibling() {
        let tmp = tempfile::tempdir().unwrap();
        let bin = tmp.path().join("bin");
        std::fs::create_dir_all(&bin).unwrap();
        let agent = bin.join("cursor-agent");
        let script = "#!/bin/bash\nsleep 300 &\nexec -a worker-server sleep 300 &\nsleep 3600\n";
        std::fs::write(&agent, script).unwrap();
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&agent, std::fs::Permissions::from_mode(0o755)).unwrap();
        let previous_path = std::env::var_os("PATH");
        let bin_str = bin.display().to_string();
        let new_path = match previous_path.as_ref() {
            Some(path) => format!("{bin_str}:{}", path.to_string_lossy()),
            None => bin_str,
        };
        std::env::set_var("PATH", &new_path);

        // Put the wrapper in its own process group and null its stdio so the
        // backgrounded `sleep` children it spawns neither inherit the test
        // runner's stdout/stderr (which would hold a piped `cargo test | tail`
        // open past test completion) nor survive cleanup as orphans reparented
        // to init. We kill the entire group below, not just the foreground pid.
        use std::os::unix::process::CommandExt as _;
        let mut wrapper = Command::new(&agent)
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .process_group(0)
            .spawn()
            .expect("spawn fake cursor-agent");
        let wrapper_pid = wrapper.id();
        tokio::time::sleep(Duration::from_millis(200)).await;

        let resolved = resolve_dispatch_watch_pid(Some(wrapper_pid))
            .await
            .expect("resolved watch pid");
        assert_ne!(resolved, wrapper_pid, "should not return wrapper pid");

        let output = Command::new("ps")
            .args(["-p", resolved.to_string().as_str(), "-o", "command="])
            .output()
            .expect("ps resolved pid");
        let command = String::from_utf8_lossy(&output.stdout);
        assert!(
            command.contains("worker-server"),
            "expected worker-server child, got {command:?}"
        );

        // Reap the whole process group (wrapper + its backgrounded sleeps).
        // The wrapper's pid is the group leader because of `process_group(0)`.
        let _ = Command::new("kill")
            .args(["-TERM", &format!("-{wrapper_pid}")])
            .status();
        let _ = wrapper.kill();
        let _ = wrapper.wait();
        match previous_path {
            Some(path) => std::env::set_var("PATH", path),
            None => std::env::remove_var("PATH"),
        }
    }

    #[cfg(unix)]
    #[test]
    fn direct_child_pid_finds_wrapper_child_process() {
        // Null stdio + own process group so the backgrounded `sleep 300` cannot
        // inherit a piped `cargo test | tail` stdout (which would block on EOF)
        // and is reaped with the group rather than orphaned to init.
        use std::os::unix::process::CommandExt as _;
        let mut wrapper = Command::new("sh")
            .args(["-c", "sleep 300 & cat"])
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .process_group(0)
            .spawn()
            .expect("spawn wrapper");
        let wrapper_pid = wrapper.id();
        std::thread::sleep(Duration::from_millis(100));
        let child_pid = live_direct_child_pid(wrapper_pid).expect("direct child pid");
        let output = Command::new("ps")
            .args(["-p", child_pid.to_string().as_str(), "-o", "command="])
            .output()
            .expect("ps child");
        let command = String::from_utf8_lossy(&output.stdout);
        assert!(
            command.contains("sleep 300"),
            "expected inner worker command, got {command:?}"
        );
        // Reap the whole process group (the `cat` wrapper + its backgrounded
        // `sleep 300`); the wrapper pid is the group leader.
        let _ = Command::new("kill")
            .args(["-TERM", &format!("-{wrapper_pid}")])
            .status();
        let _ = wrapper.kill();
        let _ = wrapper.wait();
    }

    #[tokio::test]
    async fn prepare_dispatch_cleanup_conflicts_on_live_token_mismatch() {
        let (sup, dir, _w) = make_supervisor();
        let worktree = dir.path().join("wt");
        std::fs::create_dir_all(&worktree).unwrap();
        let mut req = dispatch_impl_req("TASK-CLEANUP-A", dir.path());
        req.worktree = Some(worktree.clone());
        req.dispatch_attempt_token = Some("aaaa1111bbbb2222cccc3333dddd4444".into());
        req.last_path = Some(dir.path().join("a-last.txt"));
        req.stdout_path = Some(dir.path().join("a-stdout.log"));
        sup.acquire(&tmux::driver(), req).await.unwrap();

        let outcome = sup
            .prepare_dispatch_cleanup(
                &dir.path().join("sessions"),
                &DispatchCleanupParams {
                    task_id: "TASK-CLEANUP-A".into(),
                    kind: RunKind::Worker,
                    branch: "task-cleanup-a-impl".into(),
                    worktree_path: worktree.clone(),
                    dispatch_attempt_token: Some("bbbb1111cccc2222dddd3333eeee4444".into()),
                    last_path: Some(dir.path().join("a-last.txt")),
                    stdout_path: Some(dir.path().join("a-stdout.log")),
                },
            )
            .await
            .unwrap();
        assert_eq!(outcome, DispatchCleanupOutcome::Conflict);
        sup.finish_dispatch_cleanup(
            "TASK-CLEANUP-A",
            RunKind::Worker,
            &worktree,
            Some("task-cleanup-a-impl"),
            Some("aaaa1111bbbb2222cccc3333dddd4444"),
        )
        .await;
    }

    #[tokio::test]
    async fn prepare_dispatch_cleanup_releases_matching_live_attempt() {
        let (sup, dir, _w) = make_supervisor();
        let worktree = dir.path().join("wt");
        std::fs::create_dir_all(&worktree).unwrap();
        let last = dir.path().join("a-last.txt");
        let stdout = dir.path().join("a-stdout.log");
        let token = "aaaa1111bbbb2222cccc3333dddd4444";
        let mut req = dispatch_impl_req("TASK-CLEANUP-B", dir.path());
        req.worktree = Some(worktree.clone());
        req.dispatch_attempt_token = Some(token.into());
        req.last_path = Some(last.clone());
        req.stdout_path = Some(stdout.clone());
        let acquired = sup.acquire(&tmux::driver(), req).await.unwrap();

        let outcome = sup
            .prepare_dispatch_cleanup(
                &dir.path().join("sessions"),
                &DispatchCleanupParams {
                    task_id: "TASK-CLEANUP-B".into(),
                    kind: RunKind::Worker,
                    branch: "task-cleanup-b-impl".into(),
                    worktree_path: worktree.clone(),
                    dispatch_attempt_token: Some(token.into()),
                    last_path: Some(last.clone()),
                    stdout_path: Some(stdout.clone()),
                },
            )
            .await
            .unwrap();
        match outcome {
            DispatchCleanupOutcome::Proceed {
                released_run_id: Some(run_id),
            } => assert_eq!(run_id, acquired.run_id),
            other => panic!("expected proceed with release, got {other:?}"),
        }
        sup.finish_dispatch_cleanup(
            "TASK-CLEANUP-B",
            RunKind::Worker,
            &worktree,
            Some("task-cleanup-b-impl"),
            Some(token),
        )
        .await;
    }

    #[tokio::test]
    async fn prepare_dispatch_cleanup_blocks_while_reservation_held() {
        let (sup, dir, _w) = make_supervisor();
        let worktree = dir.path().join("wt-reservation");
        std::fs::create_dir_all(&worktree).unwrap();
        let last = dir.path().join("reservation-last.txt");
        let stdout = dir.path().join("reservation-stdout.log");
        let token = "aaaa1111bbbb2222cccc3333dddd4444";
        let params = DispatchCleanupParams {
            task_id: "TASK-CLEANUP-RESERVE".into(),
            kind: RunKind::Worker,
            branch: "task-cleanup-reserve-impl".into(),
            worktree_path: worktree.clone(),
            dispatch_attempt_token: Some(token.into()),
            last_path: Some(last.clone()),
            stdout_path: Some(stdout.clone()),
        };
        let sessions = dir.path().join("sessions");
        std::fs::create_dir_all(&sessions).unwrap();

        let first = sup
            .prepare_dispatch_cleanup(&sessions, &params)
            .await
            .unwrap();
        match first {
            DispatchCleanupOutcome::Proceed { .. } => {}
            other => panic!("expected first prepare to proceed, got {other:?}"),
        }

        let second = sup
            .prepare_dispatch_cleanup(&sessions, &params)
            .await
            .unwrap();
        assert_eq!(
            second,
            DispatchCleanupOutcome::Conflict,
            "held cleanup reservation must block a second prepare"
        );

        sup.finish_dispatch_cleanup(
            "TASK-CLEANUP-RESERVE",
            RunKind::Worker,
            &worktree,
            Some("task-cleanup-reserve-impl"),
            Some(token),
        )
        .await;
    }
}
