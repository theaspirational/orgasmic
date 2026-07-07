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
}

struct RunRecord {
    task_id: String,
    kind: RunKind,
    worker_id: String,
    role: String,
    transport: String,
    harness: Option<String>,
    project_id: Option<String>,
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

struct TerminalRelease {
    run_id: String,
    session_path: PathBuf,
    identity: RuntimeIdentity,
    outcome: ReleaseOutcome,
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
        {
            let mut guard = self.inner.lock().await;
            if guard.acquisition_paused {
                return Err(SupervisorError::AcquisitionPaused);
            }
            let key = (req.task_id.clone(), req.kind);
            if let Some(existing) = guard.leases.get(&key) {
                return Err(SupervisorError::LeaseHeld {
                    task_id: req.task_id.clone(),
                    kind: req.kind,
                    run_id: existing.clone(),
                });
            }
            guard.leases.insert(key, run_id.clone());
        }

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
            Err(e) => {
                self.release_lease(&req.task_id, req.kind).await;
                return Err(SupervisorError::Driver(e));
            }
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
            // Stream end: surface a release-with-interrupted marker if the
            // run record still exists (driver dropped its sender without
            // an explicit release).
            let g = inner_for_drain.lock().await;
            if let Some(rec) = g.runs.get(&run_id_for_drain) {
                let path = rec.session_path.clone();
                let identity = rec.identity.clone();
                let task_id = rec.task_id.clone();
                let kind_owned = rec.kind;
                let terminal_outcome = rec.terminal_outcome;
                drop(g);
                let release = Lifecycle::Release {
                    reason: "driver stream closed".into(),
                    outcome: terminal_outcome.unwrap_or(ReleaseOutcome::Interrupted),
                    finalized_by_worker: false,
                };
                let _ = writer
                    .append_session(SessionAppend {
                        run_id: run_id_for_drain.clone(),
                        session_path: path,
                        identity,
                        kind: SessionEventKind::Lifecycle,
                        event: serde_json::to_value(&release).unwrap_or(serde_json::Value::Null),
                    })
                    .await;
                let mut g = inner_for_drain.lock().await;
                g.leases.remove(&(task_id, kind_owned));
                g.runs.remove(&run_id_for_drain);
            }
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
            sub_state: initial_working_sub_state(&req.role),
            identity: identity.clone(),
            session_path: req.session_path.clone(),
            babysitter_target: req.babysitter_target.clone(),
            last_path: req.last_path.clone(),
            stdout_path: req.stdout_path.clone(),
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
        {
            let mut guard = self.inner.lock().await;
            if guard.acquisition_paused {
                return Err(SupervisorError::AcquisitionPaused);
            }
            let key = (req.task_id.clone(), req.kind);
            if let Some(existing) = guard.leases.get(&key) {
                return Err(SupervisorError::LeaseHeld {
                    task_id: req.task_id.clone(),
                    kind: req.kind,
                    run_id: existing.clone(),
                });
            }
            guard.leases.insert(key, run_id.clone());
        }
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
            Err(e) => {
                self.release_lease(&req.task_id, req.kind).await;
                return Err(SupervisorError::Driver(e));
            }
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
            let mut g = inner_for_drain.lock().await;
            if let Some(rec) = g.runs.remove(&run_id_for_drain) {
                g.leases.remove(&(rec.task_id, rec.kind));
            }
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
            sub_state: initial_working_sub_state(&req.role),
            identity: identity.clone(),
            session_path: std::mem::take(&mut req.session_path),
            babysitter_target: req.babysitter_target.clone(),
            last_path: req.last_path.clone(),
            stdout_path: req.stdout_path.clone(),
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
        driver_config: DriverConfig,
    ) -> Result<(), SupervisorError> {
        let evt = Lifecycle::RunMeta {
            transport: transport.to_string(),
            harness,
            project_id,
            worktree,
            last_path,
            stdout_path,
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
        project_id: Option<String>,
        worktree: Option<PathBuf>,
        session_path: PathBuf,
        driver_config: DriverConfig,
    ) -> Result<AcquireResponse, SupervisorError> {
        let run_id = identity.run_id.clone();
        // Lease conflict guard: do not steal an occupied lease.
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
            let key = (task_id.clone(), kind);
            if let Some(active) = guard.leases.get(&key) {
                if active != &run_id {
                    return Err(SupervisorError::ReattachLeaseConflict {
                        task_id,
                        kind,
                        active_run_id: active.clone(),
                    });
                }
            }
            guard.leases.insert(key, run_id.clone());
        }

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
                self.release_lease(&task_id, kind).await;
                return Err(SupervisorError::NotReattachable {
                    run_id,
                    reason: "driver could not prove a live runtime handle".into(),
                });
            }
            Err(e) => {
                self.release_lease(&task_id, kind).await;
                return Err(SupervisorError::Driver(e));
            }
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
            let mut g = inner_for_drain.lock().await;
            if let Some(rec) = g.runs.remove(&run_id_for_drain) {
                g.leases.remove(&(rec.task_id, rec.kind));
            }
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
                sub_state: rec.sub_state.clone(),
                identity: rec.identity.clone(),
                session_path: rec.session_path.clone(),
                babysitter_target: rec.babysitter_target.clone(),
                event_count: rec.next_event_seq,
                last_path: rec.last_path.clone(),
                stdout_path: rec.stdout_path.clone(),
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
    ) {
        let mut g = self.inner.lock().await;
        if let Some(rec) = g.runs.get_mut(run_id) {
            rec.last_path = Some(last_path);
            rec.stdout_path = Some(stdout_path);
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
        if let Err(e) = self
            .release(
                &revalidated.run_id,
                revalidated.reason,
                ReleaseOutcome::Failed,
            )
            .await
        {
            if !matches!(e, SupervisorError::RunNotFound(_)) {
                warn!(
                    error = %e,
                    run_id = %revalidated.run_id,
                    reason = revalidated.reason,
                    "supervisor timeout release failed"
                );
            }
        }
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

    async fn release_lease(&self, task_id: &str, kind: RunKind) {
        let mut g = self.inner.lock().await;
        g.leases.remove(&(task_id.into(), kind));
    }

    /// Stop any live dispatch worker (or clear an orphaned lease) before
    /// daemon-side worktree/branch cleanup. Returns the released run id when
    /// a worker or lease was cleared.
    pub async fn release_dispatch_worker_for_cleanup(
        &self,
        task_id: &str,
        kind: RunKind,
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
        if live {
            self.release(
                &run_id,
                "dispatch failure cleanup",
                ReleaseOutcome::Interrupted,
            )
            .await?;
            return Ok(Some(run_id));
        }
        match self.release_orphaned_lease(task_id, kind).await {
            OrphanedLeaseOutcome::Released { run_id } => Ok(Some(run_id)),
            OrphanedLeaseOutcome::NoLease => Ok(None),
            OrphanedLeaseOutcome::HeldByLiveRun { run_id } => {
                self.release(
                    &run_id,
                    "dispatch failure cleanup",
                    ReleaseOutcome::Interrupted,
                )
                .await?;
                Ok(Some(run_id))
            }
        }
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

fn subprocess_exited(pid: u32) -> bool {
    if pid == 0 {
        return false;
    }
    Command::new("kill")
        .args(["-0", pid.to_string().as_str()])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map(|status| !status.success())
        .unwrap_or(true)
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
    pub sub_state: Option<RunSubState>,
    pub identity: RuntimeIdentity,
    pub session_path: PathBuf,
    pub babysitter_target: Option<String>,
    pub event_count: u64,
    /// Dispatch artifact paths, when this is a CLI-dispatched run (`None`
    /// for manager/recovery/stage runs). `orgasmic dispatch finalize`
    /// resolves the report path for the current run from this field rather
    /// than scanning `.orgasmic/tx`, which a worker's own worktree checkout
    /// cannot see live daemon writes to.
    #[serde(default)]
    pub last_path: Option<PathBuf>,
    #[serde(default)]
    pub stdout_path: Option<PathBuf>,
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

fn terminal_event_releases_transport(transport: &str) -> bool {
    matches!(transport, "tmux" | "tmux-tui" | "rmux")
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

    let rec = inner.runs.remove(run_id)?;
    inner.leases.remove(&(rec.task_id.clone(), rec.kind));
    Some(TerminalRelease {
        run_id: run_id.to_string(),
        session_path: rec.session_path,
        identity: rec.identity,
        outcome: rec.terminal_outcome.unwrap_or(ReleaseOutcome::Interrupted),
        control: rec.control,
    })
}

async fn finish_driver_terminal_release(writer: &WriterHandle, release: TerminalRelease) {
    let mut control = release.control;
    let _ = control.release("driver terminal event").await;
    drop(control);
    let evt = Lifecycle::Release {
        reason: "driver terminal event".into(),
        outcome: release.outcome,
        finalized_by_worker: false,
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
            session_path: dir.join(format!("{task}.jsonl")),
            driver_config: tmux::inert_config(),
            babysitter_target: None,
            stall_timeout_secs: None,
            max_run_duration_secs: None,
            idle_timeout_secs: None,
            babysitter: None,
        }
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

    async fn send_tmux_line_for_test(session: &str, line: &str) {
        let status = tokio::process::Command::new("tmux")
            .args(["send-keys", "-t", session, line, "Enter"])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .await
            .expect("tmux send-keys");
        assert!(status.success(), "tmux send-keys failed");
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
    async fn tmux_terminal_event_releases_supervisor_run_and_session() {
        let _live_guard = live_session_guard();
        if !tmux_spawn_usable_for_test().await || !command_available_for_test("bash") {
            eprintln!(
                "skipping tmux_terminal_event_releases_supervisor_run_and_session: tmux/bash unavailable"
            );
            return;
        }
        let (sup, dir, _w) = make_supervisor();
        let driver = TmuxTuiDriver;
        let mut req = impl_req("TASK-TMUX-MOCK", dir.path());
        req.worker_id = "implementer-claude-tmux".into();
        req.driver_config = DriverConfig::from_value(json!({
            "command": "bash",
            "args": ["-lc", "printf 'mock output\\n'; cat"],
        }));
        let resp = sup.acquire(&driver, req).await.unwrap();
        let session = tmux::tmux_session_name(&resp.identity);
        tokio::time::sleep(Duration::from_millis(300)).await;
        assert!(tmux_has_session_for_test(&session).await);

        let marker = format!("[orgasmic-eot:{}]", resp.run_id);
        send_tmux_line_for_test(&session, &marker).await;
        wait_for_run_release(&sup, &resp.run_id, Duration::from_secs(8)).await;
        tokio::time::sleep(Duration::from_millis(200)).await;
        assert!(
            !tmux_has_session_for_test(&session).await,
            "tmux session should be killed after EOT release"
        );

        let events = session_events(&dir.path().join("TASK-TMUX-MOCK.jsonl"));
        assert!(events.iter().any(|envelope| {
            envelope.kind == SessionEventKind::DriverEvent
                && matches!(
                    serde_json::from_value::<DriverEvent>(envelope.event.clone()),
                    Ok(DriverEvent::Ready { .. })
                )
        }));
        assert!(events.iter().any(|envelope| {
            envelope.kind == SessionEventKind::DriverEvent
                && matches!(
                    serde_json::from_value::<DriverEvent>(envelope.event.clone()),
                    Ok(DriverEvent::TextChunk { chunk, .. }) if chunk.contains("mock output")
                )
        }));
        assert!(events.iter().any(|envelope| {
            envelope.kind == SessionEventKind::DriverEvent
                && matches!(
                    serde_json::from_value::<DriverEvent>(envelope.event.clone()),
                    Ok(DriverEvent::RunComplete { .. })
                )
        }));
        assert!(events.iter().any(|envelope| {
            envelope.kind == SessionEventKind::Lifecycle
                && matches!(
                    serde_json::from_value::<Lifecycle>(envelope.event.clone()),
                    Ok(Lifecycle::Release {
                        outcome: ReleaseOutcome::Completed,
                        ..
                    })
                )
        }));
    }

    #[tokio::test]
    async fn tmux_real_claude_wire_smoke_releases_on_eot() {
        if !tmux_spawn_usable_for_test().await || !command_available_for_test("claude") {
            eprintln!(
                "skipping tmux_real_claude_wire_smoke_releases_on_eot: tmux/claude unavailable"
            );
            return;
        }
        if std::env::var_os("ORGASMIC_RUN_REAL_CLAUDE_SMOKE").is_none() {
            eprintln!(
                "skipping tmux_real_claude_wire_smoke_releases_on_eot: set ORGASMIC_RUN_REAL_CLAUDE_SMOKE=1 to exercise real claude"
            );
            return;
        }
        let (sup, dir, _w) = make_supervisor();
        let driver = TmuxTuiDriver;
        let mut req = impl_req("TASK-TMUX-CLAUDE", dir.path());
        req.worker_id = "implementer-claude-tmux".into();
        req.driver_config = DriverConfig::from_value(json!({
            "prompt_bundle_text": "Respond with just the requested orgasmic end-of-turn marker and no other text.",
        }));

        let resp = sup.acquire(&driver, req).await.unwrap();
        let session = tmux::tmux_session_name(&resp.identity);
        wait_for_run_release(&sup, &resp.run_id, Duration::from_secs(60)).await;
        tokio::time::sleep(Duration::from_millis(200)).await;
        assert!(
            !tmux_has_session_for_test(&session).await,
            "real claude tmux session should be gone after terminal release"
        );

        let events = session_events(&dir.path().join("TASK-TMUX-CLAUDE.jsonl"));
        assert!(events.iter().any(|envelope| {
            envelope.kind == SessionEventKind::DriverEvent
                && matches!(
                    serde_json::from_value::<DriverEvent>(envelope.event.clone()),
                    Ok(DriverEvent::RunComplete { .. })
                )
        }));
        assert!(events.iter().any(|envelope| {
            envelope.kind == SessionEventKind::Lifecycle
                && matches!(
                    serde_json::from_value::<Lifecycle>(envelope.event.clone()),
                    Ok(Lifecycle::Release {
                        outcome: ReleaseOutcome::Completed,
                        ..
                    })
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
}
