// arch: arch_A53QX.1, arch_A53QX.5, arch_Z3Z3V.1, arch_Z3Z3V.2
// orgasmic:arch_A53QX, arch_Z3Z3V, dec_CSKBD
//! Run supervisor — owns the live driver session map and the
//! `(task_id, kind)` lease table.
//!
//! Invariants the supervisor enforces:
//!
//! - **AC #2**: at most one active run per `(project_id, task_id, RunKind)`
//!   tuple. A second `acquire` for the same key while a run is live returns
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
//! - Failed runs terminate after the Failed release tombstone; the supervisor
//!   never auto-spawns a continuation. Manager rescue is out-of-band via
//!   `orgasmic run recover` (TASK-QPKCD).

use std::collections::HashMap;
use std::future::Future;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

use chrono::{DateTime, Utc};
use orgasmic_core::{
    read_session_file, BabysitterSummaryChunk, BabysitterTool, DriverEvent, Lifecycle,
    ReleaseOutcome, RunSubState, RuntimeIdentity, SessionEventKind, TextStream,
};
use orgasmic_drivers::{
    AttachOutcome, BabysitterAck, BabysitterRequest, DriverConfig, DriverContext, DriverControl,
    DriverError, NativeRuntimeMeta, RunKind, RuntimeOptionsRequest, TransitionAck,
    TransitionRequest, UserInputRequest, WorkerDriver,
};
use serde::{Deserialize, Serialize};
use thiserror::Error;
use tokio::sync::Mutex;
use tokio::time::Instant;
use tracing::{error, warn};
use uuid::Uuid;

use crate::driver_resolution::resolve_driver;
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
const DRIVER_RELEASE_TIMEOUT: Duration = Duration::from_secs(5);

/// The first token of every stall release reason, and the whole reason when the
/// daemon could not establish what was missing (TASK-JK66P). Consumers that
/// classify a tombstone match on this token, not on the whole string:
/// `is_stall_sweep_release_reason` (CLI) and
/// `anomalous_without_finalize_release_reason` (api).
pub(crate) const STALL_TIMEOUT_REASON: &str = "stall_timeout_exceeded";

/// How long the supervisor waits for a work-evidence probe before giving up on
/// it and releasing the run on the evidence it already has (TASK-JK66P).
///
/// The probe shells out (`rmux display-message`, `ps`), so a wedged rmux daemon
/// would otherwise be able to make every stalled run immortal — the exact
/// failure mode this task exists to close, re-entered through the fix. On
/// expiry the observation is [`WorkEvidence::Unknown`] and the release proceeds
/// with today's reason.
const WORK_PROBE_TIMEOUT: Duration = Duration::from_secs(5);

/// How long a run's event drain may keep waiting for its driver's stream to
/// end *after* a release has been requested for that run, before the drain
/// stops waiting and lets the release finish without it.
///
/// orgasmic:TASK-HAREX — derived, not chosen. Once a release is requested the
/// daemon has already stopped the driver, and
/// [`crate::api::RELEASE_FINALIZATION_DRAIN_TIMEOUT`] is the existing budget
/// for the whole of that teardown (5s driver release + 5s producer join + 10s
/// writer slack; see its doc comment). Reusing it as a deadline measured from
/// the release request means the drain can never push a release past the
/// budget the rest of the daemon already assumes a release fits inside, and
/// a future change to the shutdown cost moves this with it.
///
/// Before this bound existed the wait was `while let Some(evt) =
/// events.recv().await` with nothing to stop it: a driver that leaves one
/// sender clone alive outside the task the supervisor holds as `producer`
/// (aborting the producer then does not close the channel) parks the drain
/// forever. `release_one` awaits that drain before it removes the run record
/// and writes `Lifecycle::Release`, so the run stays in `runs` — live in
/// `GET /runs`, 404 from `POST /runs/:id/release` because
/// `explicit_release_in_progress` is already set, invisible to every timeout
/// because `timed_out_run` skips a record with a `terminal_outcome` — with no
/// tombstone and therefore no `manager.dispatch_orphaned`, until an operator
/// restarts the daemon. That is the measured 2026-07-26 incident.
///
/// The bound arms only on a *requested* release, never on silence, so a
/// healthy worker that says nothing for an hour while cargo builds is not
/// affected by it at all.
pub(crate) const RELEASE_DRAIN_BUDGET: Duration = crate::api::RELEASE_FINALIZATION_DRAIN_TIMEOUT;

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

/// `note` value of the durable finalize-admission marker (TASK-QSSQH).
///
/// Appended by [`Supervisor::release_one`] the moment a worker finalize is
/// admitted and *before* the driver is torn down, so the session itself records
/// where teardown begins. Read back by
/// `crate::api::stage_outcome_from_session`; see the comment at the append site
/// for why the trailing `Lifecycle::Release` cannot carry this boundary.
pub(crate) const WORKER_FINALIZE_ADMITTED_NOTE: &str = "worker_finalize_admitted";

/// Does `event` carry the finalize-admission marker written by
/// [`Supervisor::release_one`]?
pub(crate) fn is_worker_finalize_admitted_note(event: &serde_json::Value) -> bool {
    event.get("note").and_then(serde_json::Value::as_str) == Some(WORKER_FINALIZE_ADMITTED_NOTE)
}

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
    /// Allowed semantic sub-states for this addressed run.
    pub applicable_states: Vec<String>,
    /// Maximum native semantic turns before supervisor failure.
    pub max_iterations: Option<u32>,
    /// Predeclared run/runtime identity for crash-recoverable acquire paths
    /// (Failed recovery claims). When set, the supervisor uses this instead of
    /// minting a fresh hidden identity at acquire time.
    pub planned_identity: Option<RuntimeIdentity>,
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
    pub applicable_states: Vec<String>,
    pub linked_skills: Vec<String>,
    pub sandbox_permissions: Option<orgasmic_core::SandboxAllowlist>,
    pub max_iterations: Option<u32>,
    pub context_budget_chars: Option<u32>,
    pub harness_args: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AcquireResponse {
    pub run_id: String,
    pub identity: RuntimeIdentity,
    pub pid: Option<u32>,
}

/// Complete immutable recovery lifecycle installed before an attached
/// driver's queued events are allowed to drain.
#[derive(Clone)]
pub struct RecoveryReattachPlan {
    pub claim: crate::recovery_claim::RecoveryClaim,
    pub last_path: Option<PathBuf>,
    pub stdout_path: Option<PathBuf>,
    pub native_runtime: Option<NativeRuntimeMeta>,
    pub prompt_draft: String,
    pub session_file: Option<crate::recovery_claim::SessionFile>,
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
    /// The record IS present and some other authority is already releasing it
    /// (`explicit_release_in_progress` / `early_exit_release_taken`).
    ///
    /// orgasmic:TASK-RB1ZN — the opposite state from
    /// [`SupervisorError::RunNotFound`], which used to answer for both. A caller
    /// that cannot tell them apart cannot tell "this run is over" from "this run
    /// is live and busy dying": the first is a 404 and the second is a 409 whose
    /// only honest advice is to retry after the drain budget
    /// ([`RELEASE_DRAIN_BUDGET`]) bounds it. Collapsing them is what let
    /// `POST /runs/:id/release` answer 404 for a run `GET /runs/live` was still
    /// reporting — the 2026-07-26 incident's first symptom.
    #[error("release already in progress: {0}")]
    ReleaseInProgress(String),
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
    #[error("sub-state is not allowed by run governance: {0}")]
    DisallowedSubState(String),
    #[error(
        "dispatch cleanup in progress for task={task_id} kind={kind:?} worktree={worktree} held by {holder}"
    )]
    CleanupInProgress {
        task_id: String,
        kind: RunKind,
        worktree: String,
        holder: CleanupHolderDiagnostic,
    },
}

/// Who holds the reservation an acquire was refused on, as far as the daemon
/// can tell (TASK-95SGV ask 3). Without this the operator sees a refusal with
/// no way to distinguish a live cleanup from a stale leaked entry.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CleanupHolderDiagnostic {
    /// The opaque guard handle the reservation is released by, when it has one.
    pub guard_id: Option<String>,
    pub owner_pid: Option<u32>,
    /// `None` when the pid is missing or the platform cannot probe one.
    pub owner_alive: Option<bool>,
}

impl CleanupHolderDiagnostic {
    fn observed(holder: Option<&CloseGuardHolder>) -> Self {
        let owner_pid = holder.and_then(|h| h.owner_pid);
        Self {
            guard_id: holder.map(|h| h.close_guard_id.clone()),
            owner_pid,
            // `subprocess_exited` cannot answer on non-Unix targets (it always
            // reports "not exited"), so only Unix gets a liveness claim.
            owner_alive: owner_pid
                .filter(|_| cfg!(unix))
                .map(|pid| !subprocess_exited(pid)),
        }
    }
}

impl std::fmt::Display for CleanupHolderDiagnostic {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match &self.guard_id {
            Some(guard_id) => write!(f, "guard={guard_id}")?,
            None => write!(f, "guard=unknown")?,
        }
        match self.owner_pid {
            Some(pid) => write!(f, " owner_pid={pid}")?,
            None => write!(f, " owner_pid=unknown")?,
        }
        match self.owner_alive {
            Some(true) => write!(f, " (owner alive)"),
            Some(false) => write!(f, " (owner dead)"),
            None => write!(f, " (owner liveness unknown)"),
        }
    }
}

/// Canonical supervisor lease identity (TASK-QPKCD / TASK-EMY0M).
pub(crate) type LeaseKey = (String, String, RunKind);

pub(crate) fn lease_key(project_id: Option<&str>, task_id: &str, kind: RunKind) -> LeaseKey {
    (
        project_id.unwrap_or_default().to_string(),
        task_id.to_string(),
        kind,
    )
}

#[derive(Clone)]
pub struct Supervisor {
    inner: Arc<Mutex<Inner>>,
    writer: WriterHandle,
    boot: Arc<BootIdentity>,
    /// `false` while boot rehydration is still deciding which runtimes from the
    /// previous daemon are alive (TASK-AK6EM ask 2). A destructive close waits
    /// for this rather than reading a run map that is knowingly incomplete.
    /// Deliberately outside the mutex: the wait must not hold the lock the
    /// reattach it is waiting for needs.
    boot_reattach_resolved: Arc<tokio::sync::watch::Sender<bool>>,
    /// [`RELEASE_DRAIN_BUDGET`] in milliseconds, in a cell so a test can
    /// compress a tens-of-seconds production window to a few hundred
    /// milliseconds and still drive the real code path (TASK-HAREX). Read once
    /// per run, when `acquire` spawns that run's drain.
    release_drain_budget_ms: Arc<AtomicU64>,
    /// [`DRIVER_RELEASE_TIMEOUT`] in milliseconds, in a cell for the same
    /// reason the field above is one (TASK-J1XCB). `stop_and_join_driver_producer`
    /// spends this budget *twice* — once waiting on `control.release()` and
    /// once joining the producer — so a test whose driver hangs in both, which
    /// is the only way to exercise the abort path, sits through 10s of real
    /// time before it can observe anything. Read once per teardown.
    driver_release_timeout_ms: Arc<AtomicU64>,
    /// The second channel the stall detector reads before releasing a run
    /// (TASK-JK66P): what is actually running under it. Production installs
    /// [`ProcessSubtreeCpuProbe`]; supervisor tests swap in doubles, because no
    /// unit test can put a real cargo build under a real rmux pane. The
    /// production implementation is proven separately against a real process
    /// subtree.
    work_probe: Arc<std::sync::RwLock<Arc<dyn WorkEvidenceProbe>>>,
}

// orgasmic:TASK-AK6EM
/// Supervisor state, and the one door every live run comes through.
///
/// `Inner` lives in its own module for a single reason: `leases` is private to
/// it. TASK-1T3FZ installed the cleanup fence in `acquire_impl` and TASK-AK6EM
/// found the *second* admission path (`reattach`) that had never learned about
/// it, so the fix cannot be another per-call-site check — the next path added
/// would forget it the same way. Making a run live means writing `leases`, and
/// the only code that can write `leases` is [`Inner::admit_live_run`], which
/// runs the whole admission check. A future fourth path does not get to
/// forget: it cannot compile without going through this door, and its author
/// must name which [`AdmissionPath`] it is.
mod admission {
    use super::*;

    pub(super) struct Inner {
        pub(super) acquisition_paused: bool,
        /// `(project_id, task_id, RunKind)` → run_id. Single-entry guard for
        /// AC #2.
        ///
        /// PRIVATE ON PURPOSE — see the module docs. Read it through
        /// [`Inner::lease`], drop it through [`Inner::remove_lease`], and take
        /// one only through [`Inner::admit_live_run`].
        leases: HashMap<LeaseKey, String>,
        /// run_id → live run record. Holds the driver control handle so the
        /// supervisor can call `release` later.
        pub(super) runs: HashMap<String, RunRecord>,
        /// task_id → retry state after babysitter auto-spawn hits a stale
        /// `(task_id, Babysitter)` lease. Prevents dispatch churn from turning
        /// one stale babysitter lease into an immediate retry loop.
        pub(super) babysitter_auto_spawn_backoff: HashMap<String, BabysitterAutoSpawnBackoff>,
        /// Active dispatch cleanup reservations held through filesystem mutation
        /// (TASK-1FV1N). Blocks reuse of the same default worktree path.
        pub(super) cleanup_reservations: HashMap<CleanupReservationKey, DispatchCleanupReservation>,
        /// Where externally-held close guards are persisted so a replacement
        /// daemon inherits them (dec_NFZY2).
        pub(super) close_guards: CloseGuardStore,
    }

    /// Which admission path is asking. Every variant is an entry point that can
    /// make a run live in this daemon; there are exactly two, and this enum is
    /// where a third has to declare itself.
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub(super) enum AdmissionPath {
        /// A brand-new run: `acquire`, `acquire_recovery`, babysitter
        /// auto-spawn. Any held lease for the key is a conflict.
        Acquire,
        /// Rehydration of a runtime that outlived a daemon: explicit
        /// `reattach_tmux`, pending-plan `reattach_existing`, boot rehydration.
        /// A lease held by *this same run id* is the run itself and is not a
        /// conflict.
        Reattach,
    }

    /// What an admission path must state about the run it wants to make live.
    pub(super) struct LiveRunAdmission<'a> {
        pub(super) path: AdmissionPath,
        pub(super) lease_key: &'a LeaseKey,
        pub(super) run_id: &'a str,
        pub(super) task_id: &'a str,
        pub(super) kind: RunKind,
        /// The worktree the run will occupy, when it has one. `None` means the
        /// run touches no dispatch worktree (manager, babysitter, stage runs)
        /// and no cleanup fence can apply to it.
        pub(super) worktree: Option<&'a Path>,
    }

    /// Proof that a lease was taken through [`Inner::admit_live_run`]. The only
    /// way to build a [`LeaseReservation`], so an admission path that skipped
    /// the check has nothing to hand it.
    #[must_use]
    pub(super) struct AdmittedLease {
        key: LeaseKey,
        run_id: String,
    }

    impl AdmittedLease {
        pub(super) fn into_parts(self) -> (LeaseKey, String) {
            (self.key, self.run_id)
        }
    }

    impl Inner {
        pub(super) fn new(close_guards: CloseGuardStore) -> Self {
            let cleanup_reservations = close_guards.restore();
            Self {
                acquisition_paused: false,
                leases: HashMap::new(),
                runs: HashMap::new(),
                babysitter_auto_spawn_backoff: HashMap::new(),
                cleanup_reservations,
                close_guards,
            }
        }

        pub(super) fn lease(&self, key: &LeaseKey) -> Option<&String> {
            self.leases.get(key)
        }

        pub(super) fn remove_lease(&mut self, key: &LeaseKey) -> Option<String> {
            self.leases.remove(key)
        }

        /// Whether any lease is held by `run_id`.
        #[cfg(test)]
        pub(super) fn holds_lease_for_run(&self, run_id: &str) -> bool {
            self.leases.values().any(|held| held == run_id)
        }

        /// The single live-run admission decision.
        ///
        /// Order is deliberate and shared by both paths: pause, then lease,
        /// then the destructive-cleanup fence. The fence is last because it is
        /// the one that must be read under the *same* lock acquisition that
        /// installs the lease — `reserve_dispatch_close` installs its
        /// reservation under this lock, so from that instant no path here can
        /// admit a run into the reserved worktree.
        pub(super) fn admit_live_run(
            &mut self,
            req: LiveRunAdmission<'_>,
        ) -> Result<AdmittedLease, SupervisorError> {
            if self.acquisition_paused {
                return Err(SupervisorError::AcquisitionPaused);
            }
            match req.path {
                AdmissionPath::Acquire => {
                    if let Some(existing) = self.leases.get(req.lease_key) {
                        return Err(SupervisorError::LeaseHeld {
                            task_id: req.task_id.to_string(),
                            kind: req.kind,
                            run_id: existing.clone(),
                        });
                    }
                }
                AdmissionPath::Reattach => {
                    if let Some(active) = self.leases.get(req.lease_key) {
                        if active != req.run_id {
                            return Err(SupervisorError::ReattachLeaseConflict {
                                task_id: req.task_id.to_string(),
                                kind: req.kind,
                                active_run_id: active.clone(),
                            });
                        }
                    }
                }
            }
            if let Some(worktree) = req.worktree {
                let worktree_key = normalize_cleanup_worktree(worktree);
                super::drop_abandoned_cleanup_reservations(self);
                if let Some(reservation) = self
                    .cleanup_reservations
                    .iter()
                    .find(|(key, _)| key.worktree_key == worktree_key)
                    .map(|(_, reservation)| reservation)
                {
                    return Err(SupervisorError::CleanupInProgress {
                        task_id: req.task_id.to_string(),
                        kind: req.kind,
                        worktree: worktree.display().to_string(),
                        holder: CleanupHolderDiagnostic::observed(reservation.holder.as_ref()),
                    });
                }
            }
            self.leases
                .insert(req.lease_key.clone(), req.run_id.to_string());
            Ok(AdmittedLease {
                key: req.lease_key.clone(),
                run_id: req.run_id.to_string(),
            })
        }

        /// Install a lease without an admission decision. Tests only, and named
        /// so a production caller of it is obvious in review.
        #[cfg(test)]
        pub(super) fn insert_lease_for_test(&mut self, key: LeaseKey, run_id: String) {
            self.leases.insert(key, run_id);
        }
    }
}

use admission::{AdmissionPath, AdmittedLease, Inner, LiveRunAdmission};

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct CleanupReservationKey {
    project_id: String,
    task_id: String,
    kind: RunKind,
    worktree_key: PathBuf,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct DispatchCleanupReservation {
    branch: String,
    worktree_path: PathBuf,
    dispatch_attempt_token: Option<String>,
    last_path: Option<PathBuf>,
    stdout_path: Option<PathBuf>,
    /// Who holds this reservation and how it is released. `dispatch-close`
    /// installs an out-of-process CLI holder (TASK-1T3FZ); the daemon's own
    /// rollback path installs itself with [`HolderIdentity::DaemonCleanup`]
    /// (TASK-95SGV), so its release is also by opaque handle rather than by a
    /// worktree key recomputed from a path cleanup may already have deleted.
    /// `None` survives only for legacy in-memory entries; such a reservation
    /// is never swept.
    holder: Option<CloseGuardHolder>,
}

/// Who holds an externally-owned close guard, and how the daemon reclaims it
/// when they disappear (TASK-AK6EM ask 4).
///
/// Two independent reclamation signals, and the guard is abandoned if *either*
/// fires:
///
/// - `owner_pid` — a fast accelerator, and only where the daemon can actually
///   probe a pid. `subprocess_exited` answers `false` on non-Unix (there is no
///   portable `kill(pid, 0)`), so on those targets this signal simply never
///   fires and never reclaims anything on its own.
/// - `lease_expires_at` — the portable one, and the reason a Windows holder is
///   reclaimable at all. The holder renews it (`.../close-guard/renew`) while
///   it is still working; a holder that dies stops renewing and the guard falls
///   away one TTL later, on every platform, with no platform-specific code.
///
/// This is the project's existing PID-less holder primitive (dec_3Y2E1's
/// `ManagerRegistry`: opaque token plus a TTL that re-registering refreshes),
/// reused rather than reinvented. Release stays by guard-id UUID, so nothing
/// here can release someone else's reservation.
#[derive(Debug, Clone, Serialize, Deserialize)]
struct CloseGuardHolder {
    /// Opaque handle the holder presents to release this reservation. Looked
    /// up by value rather than by recomputed worktree key: cleanup deletes the
    /// directory, so `normalize_cleanup_worktree` can no longer canonicalize
    /// the path by the time the holder is done with it.
    close_guard_id: String,
    owner_pid: Option<u32>,
    governed_by: HolderIdentity,
    lease_expires_at: DateTime<Utc>,
}

/// Which signal decides whether a close guard's holder is gone. Chosen once,
/// when the guard is taken, so the answer cannot change under a reservation
/// that is already installed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
enum HolderIdentity {
    /// The daemon can probe the holder's pid (`kill(pid, 0)`), so pid liveness
    /// is authoritative and the renewal lease is not consulted at all. This is
    /// the Unix case, and it is what keeps the reviewer-cleared conservatism
    /// intact: an alive pid retains the guard no matter how long the holder has
    /// gone without talking to the daemon — including right across a daemon
    /// replacement, when there is no daemon to renew against.
    ProbeablePid,
    /// No probeable pid: either the caller supplied none, or the daemon runs on
    /// a target with no portable `kill(pid, 0)` (Windows). The holder proves it
    /// is alive by renewing; a holder that stops is reclaimed one TTL later.
    RenewalLease,
    /// The daemon's own rollback handler holds the reservation across its
    /// filesystem mutation (`prepare_dispatch_cleanup` →
    /// `finish_dispatch_cleanup`, TASK-95SGV). There is no renewal loop, so the
    /// lease must not govern it: where a pid can be probed a dead recorded
    /// owner is swept, and everywhere else the reservation is retained —
    /// exactly the pre-TASK-95SGV daemon-owned behavior.
    DaemonCleanup,
}

impl CloseGuardHolder {
    fn identity_for(owner_pid: Option<u32>) -> HolderIdentity {
        // `subprocess_exited` has no non-Unix implementation that can answer
        // "is this process alive" — it returns `false` unconditionally, which
        // is why a Windows holder used to be unreclaimable. On those targets
        // the lease is the identity.
        if cfg!(unix) && owner_pid.is_some() {
            HolderIdentity::ProbeablePid
        } else {
            HolderIdentity::RenewalLease
        }
    }

    /// Whether this holder is provably gone.
    fn is_abandoned(&self, now: DateTime<Utc>) -> bool {
        match self.governed_by {
            HolderIdentity::ProbeablePid | HolderIdentity::DaemonCleanup => {
                self.owner_pid.is_some_and(subprocess_exited)
            }
            HolderIdentity::RenewalLease => now > self.lease_expires_at,
        }
    }
}

/// How long a `dispatch-close` guard survives with no renewal from its holder.
pub const CLOSE_GUARD_LEASE_TTL: Duration = Duration::from_secs(90);

fn close_guard_lease_ttl() -> chrono::Duration {
    chrono::Duration::from_std(CLOSE_GUARD_LEASE_TTL)
        .unwrap_or_else(|_| chrono::Duration::seconds(90))
}

/// The renewal interval the daemon asks holders for. A third of the TTL, so two
/// consecutive lost renewals still do not drop a live holder's guard.
pub const CLOSE_GUARD_RENEW_WITHIN: Duration = Duration::from_secs(30);

/// How long `reserve_dispatch_close` waits for boot rehydration to finish
/// before refusing (TASK-AK6EM ask 2). Boot reattach is a bounded scan plus one
/// attach attempt per candidate; a close that waits longer than this is looking
/// at a daemon that is not going to resolve, and refusing is the safe answer.
pub const CLOSE_GUARD_BOOT_REATTACH_WAIT: Duration = Duration::from_secs(15);

/// A persisted close guard, as it survives daemon replacement.
#[derive(Debug, Clone, Serialize, Deserialize)]
struct PersistedCloseGuard {
    project_id: String,
    task_id: String,
    kind: RunKind,
    /// The canonicalized fence key. Persisted rather than recomputed: by the
    /// time a replacement reads this the directory may already be gone, and the
    /// fence has to key the same way it did when it was installed.
    worktree_key: PathBuf,
    reservation: DispatchCleanupReservation,
}

// orgasmic:dec_NFZY2
/// Where externally-held close guards are persisted (dec_NFZY2).
///
/// The destructive work of `dispatch-close` runs in the CLI, not the daemon, so
/// the guard is held by a process the daemon does not own. TASK-ATAXN made
/// daemon replacement routine; a guard that lives only in one daemon's memory
/// is dropped by that replacement while its holder is still deleting files.
/// This makes the guard outlive the daemon that minted it. dec_NFZY2 records
/// why this boundary owns the handoff and why the other two were rejected.
#[derive(Debug, Clone, Default)]
pub struct CloseGuardStore {
    dir: Option<PathBuf>,
}

impl CloseGuardStore {
    /// The production store, under `$ORGASMIC_HOME/state/close-guards`.
    pub fn at(dir: impl Into<PathBuf>) -> Self {
        Self {
            dir: Some(dir.into()),
        }
    }

    /// A supervisor whose guards die with it. For tests that never exercise
    /// daemon replacement; production goes through [`CloseGuardStore::at`].
    pub fn ephemeral() -> Self {
        Self { dir: None }
    }

    fn path_for(&self, guard_id: &str) -> Option<PathBuf> {
        // Guard ids are daemon-minted `close-guard-<uuidv4>`; refuse anything
        // else rather than let a caller-supplied id name a path.
        if !guard_id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-')
        {
            return None;
        }
        self.dir.as_ref().map(|dir| dir.join(guard_id))
    }

    fn write(&self, record: &PersistedCloseGuard) {
        let Some(holder) = record.reservation.holder.as_ref() else {
            return;
        };
        let Some(path) = self.path_for(&holder.close_guard_id) else {
            return;
        };
        let Some(dir) = path.parent() else {
            return;
        };
        if let Err(error) = std::fs::create_dir_all(dir) {
            tracing::warn!(error = %error, dir = %dir.display(), "close guard store unwritable");
            return;
        }
        match serde_json::to_vec_pretty(record) {
            Ok(bytes) => {
                if let Err(error) = std::fs::write(&path, bytes) {
                    tracing::warn!(
                        error = %error,
                        path = %path.display(),
                        "persisting close guard failed; it will not survive daemon replacement"
                    );
                }
            }
            Err(error) => tracing::warn!(error = %error, "serializing close guard failed"),
        }
    }

    fn remove(&self, guard_id: &str) {
        if let Some(path) = self.path_for(guard_id) {
            let _ = std::fs::remove_file(path);
        }
    }

    /// Reinstate the guards whose holders are still alive, and delete the rest.
    fn restore(&self) -> HashMap<CleanupReservationKey, DispatchCleanupReservation> {
        let mut restored = HashMap::new();
        let Some(dir) = self.dir.as_ref() else {
            return restored;
        };
        let Ok(entries) = std::fs::read_dir(dir) else {
            return restored;
        };
        let now = Utc::now();
        for entry in entries.flatten() {
            let path = entry.path();
            if !entry.file_type().map(|t| t.is_file()).unwrap_or(false) {
                continue;
            }
            let record: PersistedCloseGuard = match std::fs::read(&path)
                .map_err(|e| e.to_string())
                .and_then(|bytes| serde_json::from_slice(&bytes).map_err(|e| e.to_string()))
            {
                Ok(record) => record,
                Err(error) => {
                    tracing::warn!(
                        error = %error,
                        path = %path.display(),
                        "unreadable close guard record; discarding"
                    );
                    let _ = std::fs::remove_file(&path);
                    continue;
                }
            };
            let abandoned = record
                .reservation
                .holder
                .as_ref()
                .is_none_or(|holder| holder.is_abandoned(now));
            if abandoned {
                let _ = std::fs::remove_file(&path);
                continue;
            }
            tracing::info!(
                task_id = %record.task_id,
                worktree = %record.reservation.worktree_path.display(),
                "restored an in-flight dispatch-close guard from the previous daemon"
            );
            restored.insert(
                CleanupReservationKey {
                    project_id: record.project_id,
                    task_id: record.task_id,
                    kind: record.kind,
                    worktree_key: record.worktree_key,
                },
                record.reservation,
            );
        }
        restored
    }
}

/// Identity bundle for a destructive `dispatch-close` worktree guard
/// (TASK-1T3FZ).
#[derive(Debug, Clone)]
pub struct DispatchCloseGuardParams {
    pub project_id: String,
    pub task_id: String,
    pub kind: RunKind,
    pub branch: String,
    pub worktree_path: PathBuf,
    pub dispatch_attempt_token: Option<String>,
    pub last_path: Option<PathBuf>,
    pub stdout_path: Option<PathBuf>,
    /// The process that will perform the cleanup and release this guard.
    pub owner_pid: Option<u32>,
    /// The run this close is about to release. Excluded from the blocking
    /// scan: the close's own generation is what it is entitled to tear down.
    pub releasing_run_id: Option<String>,
    /// Every run id this dispatch generation has owned.
    pub owned_run_ids: Vec<String>,
}

/// Verdict of [`Supervisor::reserve_dispatch_close`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DispatchCloseGuardOutcome {
    /// The worktree is reserved and no live worker occupies it. The guard is
    /// held until `finish_dispatch_close(guard_id)` — or until the holder stops
    /// renewing it within `renew_within`.
    Reserved {
        guard_id: String,
        renew_within: Duration,
    },
    /// Boot rehydration has not finished deciding which runtimes survived the
    /// previous daemon, so the run map cannot yet answer "is anyone live in
    /// this worktree". Nothing is reserved; the caller must not clean up.
    BootReattachPending,
    /// Another cleanup already holds this worktree.
    ReservationHeld,
    /// A live worker occupies the worktree (or an acquire owns the lease
    /// without a run record yet, so liveness is undetermined). Nothing is
    /// reserved; the caller must not clean up.
    BlockedByLiveRun {
        run_id: String,
        worktree: Option<PathBuf>,
    },
}

/// Identity bundle for dispatch cleanup authorization (TASK-NW4WV).
#[derive(Debug, Clone)]
pub struct DispatchCleanupParams {
    pub project_id: String,
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
}

enum DurableScanError {
    UnreadableSessionsDir,
    UnreadableSessionFile,
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
    key: Option<LeaseKey>,
    run_id: String,
}

impl LeaseReservation {
    /// Takes the [`AdmittedLease`] rather than a bare key: a lease this holds
    /// is one that went through [`Inner::admit_live_run`], by construction.
    fn new(inner: Arc<Mutex<Inner>>, admitted: AdmittedLease) -> Self {
        let (key, run_id) = admitted.into_parts();
        Self {
            inner,
            key: Some(key),
            run_id,
        }
    }

    fn commit(&mut self) {
        self.key = None;
    }

    fn remove_if_unowned(inner: &mut Inner, key: &LeaseKey, run_id: &str) {
        let reserved_by_this_run = inner.lease(key).is_some_and(|held| held == run_id);
        if reserved_by_this_run && !inner.runs.contains_key(run_id) {
            inner.remove_lease(key);
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
    /// Instant of the last driver event that was *evidence of work*, per
    /// [`driver_event_advances_stall_clock`], or of the last work-evidence
    /// probe that found live work under the run. The stall clock reads this,
    /// never `last_driver_event_at`.
    ///
    /// orgasmic:TASK-VZMZE,TASK-JK66P — two measured failures of one clock.
    /// Keeping liveness (`last_driver_event_at`, which every event refreshes)
    /// separate from work is what lets a heartbeat-only run die on schedule
    /// while a silent pane with a cargo build under it survives.
    last_progress_at: Instant,
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
    /// Resolved :APPLICABLE_STATES: verbs for this run; empty disables checks.
    applicable_states: Vec<String>,
    /// Count of [`DriverEvent::AgentTurnComplete`] events for max_iterations.
    semantic_turn_count: u64,
    max_iterations: Option<u32>,
    next_event_seq: u64,
    terminal_outcome: Option<ReleaseOutcome>,
    control: Box<dyn DriverControl>,
    producer: Option<tokio::task::JoinHandle<()>>,
    event_drain: tokio::task::JoinHandle<()>,
    /// Babysitter coalescing buffer (None on implementer runs).
    babysitter_summary: Option<BabysitterSummaryBuffer>,
    /// PID-backed early-exit watcher owns canonical no-work release when set.
    early_exit_watcher_pid: Option<u32>,
    /// Owned PID observer. It may only publish observations into the record;
    /// receiver closure remains the sole normal release authority.
    early_exit_watcher: Option<tokio::task::JoinHandle<()>>,
    /// In-memory driver coordination oracle — updated as events are applied.
    driver_has_work: bool,
    driver_has_terminal: bool,
    driver_has_ready: bool,
    /// Exactly-once guard for canonical early-exit Failed release.
    early_exit_release_taken: bool,
    /// Driver event channel closed; all queued events have been drained.
    stream_ended: bool,
    /// PID watcher observed subprocess exit (classification-only; not a release gate).
    early_exit_pid_exited: bool,
    /// Explicit timeout/cancel/finalize release is draining; stream-end must defer.
    ///
    /// Only ever set through [`begin_explicit_release`], which also arms this
    /// run's drain deadline. Setting it directly would reintroduce TASK-HAREX:
    /// a release whose drain never ends and whose record therefore never leaves
    /// `runs`.
    explicit_release_in_progress: bool,
    /// Notified once, by [`begin_explicit_release`], when some authority has
    /// requested this run's release (worker finalize, timeout, cancel, an
    /// observed subprocess exit, a terminal driver event). The run's drain
    /// waits on it and, from that moment, stops waiting on the driver's stream
    /// unboundedly: it gets [`RELEASE_DRAIN_BUDGET`] and then ends regardless,
    /// so the release behind it can always reach its tombstone (TASK-HAREX).
    ///
    /// `notify_one` and not `notify_waiters`: the drain may be busy inside an
    /// event when the release is requested, and a stored permit is what makes
    /// the arming edge impossible to miss.
    release_requested: Arc<tokio::sync::Notify>,
    /// A terminal driver event has taken the control handle and is stopping
    /// the producer. Unlike other explicit releases, receiver closure may
    /// remove this record and write the terminal lifecycle event.
    terminal_event_shutdown_in_progress: bool,
    /// The PID observer has requested producer shutdown after observing the
    /// subprocess exit. The observer still never classifies or removes the
    /// run: dropping the producer closes the channel and the receiver owns the
    /// sole normal terminal boundary.
    pid_exit_shutdown_in_progress: bool,
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

/// Placeholder control left on a run record while an explicit release drains
/// the event channel with the record still in the map (orgasmic:task_3TEDA).
struct DetachedDriverControl;

#[async_trait::async_trait]
impl DriverControl for DetachedDriverControl {
    async fn transition_state(
        &mut self,
        _req: TransitionRequest,
    ) -> Result<TransitionAck, DriverError> {
        Err(DriverError::Unsupported("detached control"))
    }

    async fn babysitter_action(
        &mut self,
        _req: BabysitterRequest,
    ) -> Result<BabysitterAck, DriverError> {
        Err(DriverError::Unsupported("detached control"))
    }

    async fn release(&mut self, _reason: &str) -> Result<(), DriverError> {
        Ok(())
    }

    async fn send_input(
        &mut self,
        _req: UserInputRequest,
    ) -> Result<orgasmic_drivers::UserInputAck, DriverError> {
        Err(DriverError::Unsupported("detached control"))
    }
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
    transport: String,
    control: Box<dyn DriverControl>,
    producer: Option<tokio::task::JoinHandle<()>>,
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

/// Worktree diff summarizer used by manager-authorized recovery prompts
/// (`POST /runs/:id/recover`). Not a continuation injector.
pub struct GitDiffSummarizer;

impl GitDiffSummarizer {
    pub fn summarize(&self, worktree: Option<&Path>) -> String {
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

impl Supervisor {
    pub fn new(
        writer: WriterHandle,
        boot: Arc<BootIdentity>,
        close_guards: CloseGuardStore,
    ) -> Self {
        let supervisor = Self::unmonitored(writer, boot, close_guards);
        spawn_run_timeout_monitor(supervisor.clone());
        supervisor
    }

    /// A supervisor with no background run-timeout monitor.
    ///
    /// The monitor ticks every [`RUN_TIMEOUT_CHECK_INTERVAL`] (50ms) and calls
    /// `release_first_timed_out_run`. Any test that ages a run past its
    /// threshold and then awaits is therefore racing a second releaser for the
    /// same run, and cannot make a stable claim about what the release it
    /// drives by hand did or did not do.
    fn unmonitored(
        writer: WriterHandle,
        boot: Arc<BootIdentity>,
        close_guards: CloseGuardStore,
    ) -> Self {
        Self {
            inner: Arc::new(Mutex::new(Inner::new(close_guards))),
            writer,
            boot,
            // Resolved by default: a supervisor nobody has told about boot
            // rehydration has none pending. `begin_boot_reattach` is what the
            // daemon calls, before it binds.
            boot_reattach_resolved: Arc::new(tokio::sync::watch::channel(true).0),
            release_drain_budget_ms: Arc::new(AtomicU64::new(
                RELEASE_DRAIN_BUDGET.as_millis() as u64
            )),
            driver_release_timeout_ms: Arc::new(AtomicU64::new(
                DRIVER_RELEASE_TIMEOUT.as_millis() as u64
            )),
            work_probe: Arc::new(std::sync::RwLock::new(Arc::new(
                ProcessSubtreeCpuProbe::default(),
            ))),
        }
    }

    /// Replace the work-evidence probe (TASK-JK66P).
    ///
    /// Test-only, and for the same reason as
    /// [`Supervisor::set_driver_release_timeout`]: production has no better
    /// answer than the real probe, and a test cannot spawn a real harness pane
    /// running a real build. Every other line of the detection path — the
    /// clock, the deadline, the credit, the reason — stays on the executed
    /// path.
    #[cfg(test)]
    fn set_work_probe(&self, probe: Arc<dyn WorkEvidenceProbe>) {
        *self
            .work_probe
            .write()
            .expect("work probe lock is never poisoned: no panics under it") = probe;
    }

    fn work_probe(&self) -> Arc<dyn WorkEvidenceProbe> {
        Arc::clone(
            &self
                .work_probe
                .read()
                .expect("work probe lock is never poisoned: no panics under it"),
        )
    }

    /// Ask the probe what is running under a run, bounded by
    /// [`WORK_PROBE_TIMEOUT`]. The probe shells out, so it runs on the blocking
    /// pool and never under the supervisor lock.
    async fn observe_work_evidence(&self, target: WorkProbeTarget) -> WorkEvidence {
        let probe = self.work_probe();
        let observation = tokio::time::timeout(
            WORK_PROBE_TIMEOUT,
            tokio::task::spawn_blocking(move || probe.observe(&target)),
        )
        .await;
        match observation {
            Ok(Ok(evidence)) => evidence,
            // A probe that panicked or outran its budget establishes nothing.
            // Unknown, never Working: the release must not be blockable by a
            // probe that cannot answer.
            Ok(Err(_)) | Err(_) => WorkEvidence::Unknown,
        }
    }

    /// Override [`RELEASE_DRAIN_BUDGET`] for this supervisor (TASK-HAREX).
    ///
    /// The daemon calls this with `ShutdownBudgets::release_drain`, which is
    /// the same constant in production and an injectable one in the tests that
    /// drive a real shutdown with short budgets — so the window this bound uses
    /// and the window the shutdown path waits for a release cannot drift apart.
    /// Takes effect for runs acquired after the call.
    pub fn set_release_drain_budget(&self, budget: Duration) {
        self.release_drain_budget_ms
            .store(budget.as_millis() as u64, Ordering::SeqCst);
    }

    fn release_drain_budget(&self) -> Duration {
        Duration::from_millis(self.release_drain_budget_ms.load(Ordering::SeqCst))
    }

    /// Override [`DRIVER_RELEASE_TIMEOUT`] for this supervisor (TASK-J1XCB).
    ///
    /// Test-only, and deliberately narrower than
    /// [`Supervisor::set_release_drain_budget`]: no production caller sets it,
    /// because production has no shorter honest answer than the constant. A
    /// test that needs the driver-stop path to *reach* its abort must give the
    /// driver something that never returns, and then the two 5s waits are pure
    /// wall clock the test can neither shorten nor skip. Compressing them here
    /// keeps every line of the real teardown on the executed path and takes the
    /// test's dependence on a loaded machine's timer accuracy with it.
    #[cfg(test)]
    fn set_driver_release_timeout(&self, timeout: Duration) {
        self.driver_release_timeout_ms
            .store(timeout.as_millis() as u64, Ordering::SeqCst);
    }

    fn driver_release_timeout(&self) -> Duration {
        Duration::from_millis(self.driver_release_timeout_ms.load(Ordering::SeqCst))
    }

    /// Declare that boot rehydration is about to run, so a destructive close
    /// arriving in the meantime waits for it instead of reading an incomplete
    /// run map (TASK-AK6EM ask 2). Called before the listener binds.
    pub fn begin_boot_reattach(&self) {
        // `send_replace`, not `send`: a `watch::Sender` with no live receiver
        // fails `send` and leaves the value untouched, and the receivers here
        // are created on demand by waiters that may not exist yet.
        self.boot_reattach_resolved.send_replace(false);
    }

    /// Boot rehydration has finished — every candidate is either live in the
    /// run map or has been proven not reattachable.
    pub fn finish_boot_reattach(&self) {
        self.boot_reattach_resolved.send_replace(true);
    }

    /// Wait for boot rehydration to resolve. `false` means it did not within
    /// `budget`, and the caller must not treat the run map as complete.
    async fn wait_for_boot_reattach(&self, budget: Duration) -> bool {
        let mut rx = self.boot_reattach_resolved.subscribe();
        if *rx.borrow_and_update() {
            return true;
        }
        tokio::time::timeout(budget, async move {
            loop {
                if rx.changed().await.is_err() {
                    // Sender gone: nothing will ever resolve it, and the daemon
                    // it belonged to is going away.
                    return;
                }
                if *rx.borrow_and_update() {
                    return;
                }
            }
        })
        .await
        .is_ok()
            && *self.boot_reattach_resolved.borrow()
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
        let resp = self.acquire_impl(driver, req, None).await?;
        if auto_spawn_babysitter {
            if let Some(bs) = babysitter {
                if !self
                    .should_attempt_babysitter_auto_spawn(&task_id_for_babysitter)
                    .await
                {
                    return Ok(resp);
                }
                if let Some(bs_driver) = resolve_driver(&bs.mode, &bs.harness) {
                    record_babysitter_spawn_attempt();
                    match self
                        .spawn_babysitter(bs_driver.as_ref(), &resp.run_id, &sessions_dir, &bs)
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

    /// Acquire a claim-planned recovery run, installing the entire immutable
    /// lifecycle prefix before the driver receiver can be drained.
    pub async fn acquire_recovery(
        &self,
        driver: &dyn WorkerDriver,
        req: AcquireRequest,
        recovery_plan: RecoveryReattachPlan,
    ) -> Result<AcquireResponse, SupervisorError> {
        self.acquire_impl(driver, req, Some(recovery_plan)).await
    }

    async fn acquire_impl(
        &self,
        driver: &dyn WorkerDriver,
        req: AcquireRequest,
        recovery_plan: Option<RecoveryReattachPlan>,
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
        let (run_id, identity) = if let Some(planned) = req.planned_identity.clone() {
            (planned.run_id.clone(), planned)
        } else {
            let run_id = make_run_id(&req.kind);
            (
                run_id.clone(),
                RuntimeIdentity::new(&run_id, &self.boot.boot_id),
            )
        };
        let lease_key = lease_key(req.project_id.as_deref(), &req.task_id, req.kind);
        let admitted = {
            let mut guard = self.inner.lock().await;
            guard.admit_live_run(LiveRunAdmission {
                path: AdmissionPath::Acquire,
                lease_key: &lease_key,
                run_id: &run_id,
                task_id: &req.task_id,
                kind: req.kind,
                worktree: req.worktree.as_deref(),
            })?
        };
        let mut lease = LeaseReservation::new(self.inner.clone(), admitted);

        // Build the driver context and spawn. If the driver fails, release
        // the lease before returning.
        let ctx = DriverContext {
            identity: identity.clone(),
            run_kind: req.kind,
            task_id: req.task_id.clone(),
            worker_id: req.worker_id.clone(),
            project_id: req.project_id.clone(),
            worktree: req.worktree.clone(),
            babysitter_target: req.babysitter_target.clone(),
        };
        let transport = driver.transport().to_string();
        let harness = driver.harness().map(str::to_string);
        let session = match driver.acquire(ctx, req.driver_config.clone()).await {
            Ok(s) => s,
            Err(e) => return Err(SupervisorError::Driver(e)),
        };
        let pid = session.pid;
        crate::recovery_claim::recovery_failpoint("spawn_before_jsonl");

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
                authority: recovery_plan
                    .as_ref()
                    .and_then(|plan| plan.session_file.clone()),
                kind: SessionEventKind::Lifecycle,
                event: serde_json::to_value(&acquire_evt).map_err(into_anyhow)?,
            })
            .await
            .map_err(SupervisorError::Session)?;
        crate::recovery_claim::recovery_failpoint("acquire_append");

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
            // The mode the driver resolved for this very launch, lifted out of
            // the only per-run channel an adapter has (TASK-S0QRM). Read from
            // the session it just returned, so what is persisted is what was
            // spawned rather than what a second detection would say now.
            session
                .native_runtime
                .as_ref()
                .and_then(|native| native.credential_mode.clone()),
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

        if let Some(plan) = recovery_plan.as_ref() {
            self.backfill_recovery_session_lifecycle(
                &plan.claim,
                &run_id,
                &req.session_path,
                plan.session_file.as_ref().ok_or_else(|| {
                    SupervisorError::Session(anyhow::anyhow!(
                        "recovery plan is missing retained session authority"
                    ))
                })?,
                &identity,
                &req.task_id,
                req.kind,
                &req.worker_id,
                &req.role,
                run_requires_worker_finalize(&req.last_path, &req.role),
                &transport,
                harness.clone(),
                req.project_id.clone(),
                req.worktree.clone(),
                plan.last_path.clone(),
                plan.stdout_path.clone(),
                None,
                req.driver_config.clone(),
                plan.native_runtime.clone(),
                Some(&plan.prompt_draft),
                true,
            )
            .await?;
        }

        let run_started_at = Instant::now();
        let control = session.control;
        let producer = session.producer;
        let events = session.events;
        let kind = req.kind;
        // orgasmic:TASK-HAREX — the record and its drain share this handle, so
        // the drain learns that a release was requested without polling the
        // run map.
        let release_requested = Arc::new(tokio::sync::Notify::new());
        let release_drain_budget = self.release_drain_budget();
        let driver_release_timeout = self.driver_release_timeout();
        // Insert the run record before the drain task starts so stream-end and
        // early-exit coordination always find a resolvable lease owner.
        {
            let record = RunRecord {
                task_id: req.task_id.clone(),
                kind,
                worker_id: req.worker_id.clone(),
                role: req.role.clone(),
                transport: transport.clone(),
                harness: harness.clone(),
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
                last_progress_at: run_started_at,
                run_started_at,
                last_input_at: run_started_at,
                stall_timeout: resolve_timeout_secs(req.stall_timeout_secs, DEFAULT_STALL_TIMEOUT),
                max_run_duration: resolve_timeout_secs(
                    req.max_run_duration_secs,
                    DEFAULT_MAX_RUN_DURATION,
                ),
                idle_timeout: resolve_idle_timeout_secs(req.idle_timeout_secs),
                applicable_states: req.applicable_states.clone(),
                semantic_turn_count: 0,
                max_iterations: req.max_iterations,
                next_event_seq: 0,
                terminal_outcome: None,
                control,
                producer,
                event_drain: tokio::spawn(async {}),
                babysitter_summary: if kind == RunKind::Worker {
                    Some(BabysitterSummaryBuffer::default())
                } else {
                    None
                },
                early_exit_watcher_pid: pid,
                early_exit_watcher: None,
                driver_has_work: false,
                driver_has_terminal: false,
                driver_has_ready: false,
                early_exit_release_taken: false,
                stream_ended: false,
                early_exit_pid_exited: false,
                explicit_release_in_progress: false,
                release_requested: Arc::clone(&release_requested),
                terminal_event_shutdown_in_progress: false,
                pid_exit_shutdown_in_progress: false,
            };
            let mut g = self.inner.lock().await;
            g.runs.insert(run_id.clone(), record);
        }
        lease.commit();

        // Drain driver events into the JSONL session in a background task.
        let writer = self.writer.clone();
        let session_path = req.session_path.clone();
        let inner_for_drain = self.inner.clone();
        let run_id_for_drain = run_id.clone();
        let identity_for_drain = identity.clone();
        let drain = tokio::spawn(async move {
            let mut events = events;
            let mut gate = DrainGate::new(
                run_id_for_drain.clone(),
                release_requested,
                release_drain_budget,
            );
            while let Some(evt) = gate.next(&mut events).await {
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
                        authority: None,
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
                    let mut iteration_limit_hit = false;
                    if let Some(rec) = g.runs.get_mut(&run_id_for_drain) {
                        let seq = rec.next_event_seq;
                        rec.next_event_seq += 1;
                        if matches!(evt, DriverEvent::AgentTurnComplete { .. }) {
                            rec.semantic_turn_count += 1;
                            if let Some(max) = rec.max_iterations {
                                if rec.semantic_turn_count > u64::from(max) {
                                    rec.terminal_outcome = Some(ReleaseOutcome::Failed);
                                    iteration_limit_hit = true;
                                }
                            }
                        }
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
                    let terminal_release = if terminal_outcome.is_some() || iteration_limit_hit {
                        take_driver_terminal_release(&mut g, &run_id_for_drain, iteration_limit_hit)
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
                            authority: None,
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
                    finish_driver_terminal_release(&writer, release, driver_release_timeout).await;
                }
            }
            // Stream end: driver dropped its sender without an explicit
            // finalize/release. Claim the run atomically under the lock so a
            // concurrent `release_with_finalization` (worker finalize,
            // TASK-P4MGK / dec_WDR5K) cannot interleave a second lease
            // release or a second Lifecycle::Release write.
            finish_stream_end_terminal_drain(&writer, &inner_for_drain, &run_id_for_drain).await;
        });
        {
            let mut g = self.inner.lock().await;
            if let Some(rec) = g.runs.get_mut(&run_id) {
                rec.event_drain = drain;
            }
        }
        if let Some(pid) = pid {
            let watcher = spawn_early_exit_watcher(self.clone(), run_id.clone(), pid);
            let mut g = self.inner.lock().await;
            if let Some(rec) = g.runs.get_mut(&run_id) {
                rec.early_exit_watcher = Some(watcher);
            } else {
                watcher.abort();
            }
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
                authority: None,
                kind: SessionEventKind::Lifecycle,
                event: serde_json::to_value(&evt).map_err(into_anyhow)?,
            })
            .await
            .map_err(SupervisorError::Session)?;
        crate::recovery_claim::recovery_failpoint("native_runtime_append");
        Ok(())
    }

    /// Write a typed `Lifecycle::RecoveryOrigin` link into the replacement session.
    #[allow(clippy::too_many_arguments)]
    pub async fn write_recovery_origin(
        &self,
        run_id: &str,
        session_path: &Path,
        identity: &RuntimeIdentity,
        project_id: &str,
        origin_run_id: &str,
        origin_session_path: &Path,
        request_id: &str,
        action: &str,
        target: &str,
        claim: Option<serde_json::Value>,
    ) -> Result<(), SupervisorError> {
        let evt = Lifecycle::RecoveryOrigin {
            project_id: project_id.to_string(),
            origin_run_id: origin_run_id.to_string(),
            origin_session_path: origin_session_path.to_path_buf(),
            request_id: request_id.to_string(),
            replacement_run_id: run_id.to_string(),
            replacement_session_path: session_path.to_path_buf(),
            action: action.to_string(),
            target: Some(target.to_string()),
            claim,
        };
        self.writer
            .append_session(SessionAppend {
                run_id: run_id.to_string(),
                session_path: session_path.to_path_buf(),
                identity: identity.clone(),
                authority: None,
                kind: SessionEventKind::Lifecycle,
                event: serde_json::to_value(&evt).map_err(into_anyhow)?,
            })
            .await
            .map_err(SupervisorError::Session)?;
        crate::recovery_claim::recovery_failpoint("recovery_origin_append");
        crate::recovery_claim::recovery_failpoint("lifecycle_append");
        Ok(())
    }

    /// Idempotently backfill missing recovery lifecycle events after a live-handle
    /// reattach (pre-Acquire crash window). Order: Acquire -> RunMeta ->
    /// NativeRuntime -> PromptDraft -> RecoveryOrigin.
    #[allow(clippy::too_many_arguments)]
    pub async fn backfill_recovery_session_lifecycle(
        &self,
        claim: &crate::recovery_claim::RecoveryClaim,
        run_id: &str,
        session_path: &Path,
        session_file: &crate::recovery_claim::SessionFile,
        identity: &RuntimeIdentity,
        task_id: &str,
        kind: RunKind,
        worker_id: &str,
        role: &str,
        requires_worker_finalize: bool,
        transport: &str,
        harness: Option<String>,
        project_id: Option<String>,
        worktree: Option<PathBuf>,
        last_path: Option<PathBuf>,
        stdout_path: Option<PathBuf>,
        dispatch_attempt_token: Option<String>,
        driver_config: DriverConfig,
        native_runtime: Option<NativeRuntimeMeta>,
        prompt_draft: Option<&str>,
        include_recovery_origin: bool,
    ) -> Result<(), SupervisorError> {
        let envelopes = session_file.read_checked().map_err(|err| {
            SupervisorError::Session(anyhow::anyhow!(
                "recovery session authority failed: {err:?}"
            ))
        })?;
        if !crate::recovery_claim::pending_session_prefix_matches_claim(claim, &envelopes) {
            return Err(SupervisorError::Session(anyhow::anyhow!(
                "recovery lifecycle prefix conflicts with immutable pending plan"
            )));
        }
        let has_acquire = envelopes.iter().any(|envelope| {
            envelope.kind == SessionEventKind::Lifecycle
                && matches!(
                    serde_json::from_value::<Lifecycle>(envelope.event.clone()),
                    Ok(Lifecycle::Acquire { .. })
                )
        });
        let has_run_meta = envelopes.iter().any(|envelope| {
            envelope.kind == SessionEventKind::Lifecycle
                && matches!(
                    serde_json::from_value::<Lifecycle>(envelope.event.clone()),
                    Ok(Lifecycle::RunMeta { .. })
                )
        });
        let has_native = envelopes.iter().any(|envelope| {
            envelope.kind == SessionEventKind::Lifecycle
                && matches!(
                    serde_json::from_value::<Lifecycle>(envelope.event.clone()),
                    Ok(Lifecycle::NativeRuntime { .. })
                )
        });
        let has_prompt = envelopes.iter().any(|envelope| {
            envelope.kind == SessionEventKind::Lifecycle
                && matches!(
                    serde_json::from_value::<Lifecycle>(envelope.event.clone()),
                    Ok(Lifecycle::PromptDraft { .. })
                )
        });
        let has_origin = envelopes.iter().any(|envelope| {
            envelope.kind == SessionEventKind::Lifecycle
                && matches!(
                    serde_json::from_value::<Lifecycle>(envelope.event.clone()),
                    Ok(Lifecycle::RecoveryOrigin { .. })
                )
        });

        if !has_acquire {
            let acquire_evt = Lifecycle::Acquire {
                task_id: task_id.to_string(),
                kind: match kind {
                    RunKind::Worker => "worker".to_string(),
                    RunKind::Babysitter => "babysitter".to_string(),
                },
                worker_id: worker_id.to_string(),
            };
            self.writer
                .append_session(SessionAppend {
                    run_id: run_id.to_string(),
                    session_path: session_path.to_path_buf(),
                    identity: identity.clone(),
                    authority: Some(session_file.clone()),
                    kind: SessionEventKind::Lifecycle,
                    event: serde_json::to_value(&acquire_evt).map_err(into_anyhow)?,
                })
                .await
                .map_err(SupervisorError::Session)?;
            crate::recovery_claim::recovery_failpoint("lifecycle_append");
        }
        if !has_run_meta {
            self.write_run_meta(
                run_id,
                session_path,
                identity,
                transport,
                harness,
                project_id,
                worktree,
                last_path,
                stdout_path,
                dispatch_attempt_token,
                role.to_string(),
                requires_worker_finalize,
                native_runtime
                    .as_ref()
                    .and_then(|native| native.credential_mode.clone()),
                driver_config,
            )
            .await?;
            crate::recovery_claim::recovery_failpoint("lifecycle_append");
        }
        if !has_native {
            if let Some(native) = native_runtime {
                self.write_native_runtime(run_id, session_path, identity, native)
                    .await?;
                crate::recovery_claim::recovery_failpoint("lifecycle_append");
            }
        }
        if !has_prompt {
            if let Some(text) = prompt_draft {
                self.append_prompt_draft(run_id, session_path, identity, text)
                    .await?;
                crate::recovery_claim::recovery_failpoint("lifecycle_append");
            }
        }
        if include_recovery_origin && !has_origin {
            let origin_session_path = claim.origin_session_path.as_deref().ok_or_else(|| {
                SupervisorError::Session(anyhow::anyhow!(
                    "pending recovery plan is missing origin session path"
                ))
            })?;
            let action = claim.action.as_deref().ok_or_else(|| {
                SupervisorError::Session(anyhow::anyhow!("pending recovery plan is missing action"))
            })?;
            let target = claim.target.as_deref().ok_or_else(|| {
                SupervisorError::Session(anyhow::anyhow!("pending recovery plan is missing target"))
            })?;
            let mut committed_snapshot = claim.clone();
            committed_snapshot.status = crate::recovery_claim::RecoveryClaimStatus::Committed;
            self.write_recovery_origin(
                run_id,
                session_path,
                identity,
                &claim.project_id,
                &claim.origin_run_id,
                origin_session_path,
                &claim.request_id,
                action,
                target,
                Some(serde_json::to_value(committed_snapshot).map_err(into_anyhow)?),
            )
            .await?;
        }
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
        credential_mode: Option<String>,
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
            credential_mode,
            driver_config: driver_config.0,
        };
        self.writer
            .append_session(SessionAppend {
                run_id: run_id.to_string(),
                session_path: session_path.to_path_buf(),
                identity: identity.clone(),
                authority: None,
                kind: SessionEventKind::Lifecycle,
                event: serde_json::to_value(&evt).map_err(into_anyhow)?,
            })
            .await
            .map_err(SupervisorError::Session)?;
        crate::recovery_claim::recovery_failpoint("run_meta_append");
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
                authority: None,
                kind: SessionEventKind::Lifecycle,
                event: serde_json::to_value(&evt).map_err(into_anyhow)?,
            })
            .await
            .map_err(SupervisorError::Session)?;
        Ok(())
    }

    /// Append the stage identity of a `grill`/`plan`/`architect` launch to its
    /// session JSONL, so a daemon that restarts while the stage is live can
    /// rebuild the stage completion watcher it lost with the old process
    /// (TASK-KPMFK). Boot recovery reads this back in
    /// `api::boot_reattach_candidate`.
    pub async fn append_stage_meta(
        &self,
        run_id: &str,
        session_path: &Path,
        identity: &RuntimeIdentity,
        stage: &str,
    ) -> Result<(), SupervisorError> {
        let evt = Lifecycle::StageMeta {
            stage: stage.to_string(),
        };
        self.writer
            .append_session(SessionAppend {
                run_id: run_id.to_string(),
                session_path: session_path.to_path_buf(),
                identity: identity.clone(),
                authority: None,
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
                authority: None,
                kind: SessionEventKind::Lifecycle,
                event: serde_json::to_value(&evt).map_err(into_anyhow)?,
            })
            .await
            .map_err(SupervisorError::Session)?;
        crate::recovery_claim::recovery_failpoint("prompt_draft_append");
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
        append_reattach_marker: bool,
        recovery_plan: Option<RecoveryReattachPlan>,
    ) -> Result<AcquireResponse, SupervisorError> {
        let run_id = identity.run_id.clone();
        // Lease conflict guard: do not steal an occupied lease.
        let lease_key = lease_key(project_id.as_deref(), &task_id, kind);
        let admitted = {
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
            // orgasmic:TASK-AK6EM — the same door `acquire_impl` uses. This is
            // the path that used to check the lease by hand and never learned
            // about the cleanup fence, which let a crash-replay reattach install
            // a live replacement into a worktree a `dispatch-close` had already
            // reserved and was about to delete.
            guard.admit_live_run(LiveRunAdmission {
                path: AdmissionPath::Reattach,
                lease_key: &lease_key,
                run_id: &run_id,
                task_id: &task_id,
                kind,
                worktree: worktree.as_deref(),
            })?
        };
        let mut lease = LeaseReservation::new(self.inner.clone(), admitted);

        let ctx = DriverContext {
            identity: identity.clone(),
            run_kind: kind,
            task_id: task_id.clone(),
            worker_id: worker_id.clone(),
            project_id: project_id.clone(),
            worktree: worktree.clone(),
            babysitter_target: None,
        };
        let transport = driver.transport().to_string();
        let harness = driver.harness().map(str::to_string);
        let attached = match driver.attach(ctx, driver_config.clone()).await {
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

        // A pending recovery attach may already have queued Ready/terminal
        // events. Install and fsync the complete immutable lifecycle prefix
        // before any receiver task exists, so Acquire is unconditionally the
        // first envelope and RecoveryOrigin precedes all driver events.
        if let Some(plan) = recovery_plan.as_ref() {
            self.backfill_recovery_session_lifecycle(
                &plan.claim,
                &run_id,
                &session_path,
                plan.session_file.as_ref().ok_or_else(|| {
                    SupervisorError::Session(anyhow::anyhow!(
                        "recovery plan is missing retained session authority"
                    ))
                })?,
                &identity,
                &task_id,
                kind,
                &worker_id,
                &role,
                requires_worker_finalize,
                &transport,
                harness.clone(),
                project_id.clone(),
                worktree.clone(),
                plan.last_path.clone(),
                plan.stdout_path.clone(),
                None,
                driver_config.clone(),
                plan.native_runtime.clone(),
                Some(plan.prompt_draft.as_str()),
                true,
            )
            .await?;
        }

        // Append the reattach lifecycle marker to the ORIGINAL session JSONL,
        // recording the new boot that rehydrated the run.
        if append_reattach_marker {
            let reattach_evt = Lifecycle::Reattach {
                reattached_boot: self.boot.boot_id.clone(),
                transport: transport.clone(),
            };
            self.writer
                .append_session(SessionAppend {
                    run_id: run_id.clone(),
                    session_path: session_path.clone(),
                    identity: identity.clone(),
                    authority: None,
                    kind: SessionEventKind::Lifecycle,
                    event: serde_json::to_value(&reattach_evt).map_err(into_anyhow)?,
                })
                .await
                .map_err(SupervisorError::Session)?;
            crate::recovery_claim::recovery_failpoint("lifecycle_append");
        }

        let run_started_at = Instant::now();
        let recovery_last_path = recovery_plan
            .as_ref()
            .and_then(|plan| plan.last_path.clone());
        let recovery_stdout_path = recovery_plan
            .as_ref()
            .and_then(|plan| plan.stdout_path.clone());
        let control = session.control;
        let producer = session.producer;
        let events = session.events;
        // orgasmic:TASK-HAREX — a reattached run's drain is bounded the same
        // way as a fresh one's; a boot reattach is exactly the situation where
        // the driver on the other end may already be gone.
        let release_requested = Arc::new(tokio::sync::Notify::new());
        let release_drain_budget = self.release_drain_budget();
        let driver_release_timeout = self.driver_release_timeout();
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
            session_path: session_path.clone(),
            babysitter_target: None,
            last_path: recovery_last_path,
            stdout_path: recovery_stdout_path,
            dispatch_attempt_token: None,
            requires_worker_finalize,
            terminal_round: 0,
            terminal_declaration: None,
            artifactor_lifecycle: ArtifactorLifecycle::Idle,
            pending_terminal_drain: false,
            pending_cancel: false,
            babysitter_run_id: None,
            last_driver_event_at: run_started_at,
            last_progress_at: run_started_at,
            run_started_at,
            last_input_at: run_started_at,
            stall_timeout: (!is_interactive_manager_task(&task_id))
                .then_some(DEFAULT_STALL_TIMEOUT),
            max_run_duration: (!is_interactive_manager_task(&task_id))
                .then_some(DEFAULT_MAX_RUN_DURATION),
            idle_timeout: None,
            applicable_states: Vec::new(),
            semantic_turn_count: 0,
            max_iterations: None,
            next_event_seq: 0,
            terminal_outcome: None,
            control,
            producer,
            event_drain: tokio::spawn(async {}),
            babysitter_summary: if kind == RunKind::Worker {
                Some(BabysitterSummaryBuffer::default())
            } else {
                None
            },
            early_exit_watcher_pid: None,
            early_exit_watcher: None,
            driver_has_work: false,
            driver_has_terminal: false,
            driver_has_ready: false,
            early_exit_release_taken: false,
            stream_ended: false,
            early_exit_pid_exited: false,
            explicit_release_in_progress: false,
            release_requested: Arc::clone(&release_requested),
            terminal_event_shutdown_in_progress: false,
            pid_exit_shutdown_in_progress: false,
        };
        {
            let mut g = self.inner.lock().await;
            g.runs.insert(run_id.clone(), record);
        }
        lease.commit();

        // Only now may queued attached-driver events drain into the file.
        let writer = self.writer.clone();
        let drain_path = session_path.clone();
        let inner_for_drain = self.inner.clone();
        let run_id_for_drain = run_id.clone();
        let identity_for_drain = identity.clone();
        let drain = tokio::spawn(async move {
            let mut events = events;
            let mut gate = DrainGate::new(
                run_id_for_drain.clone(),
                release_requested,
                release_drain_budget,
            );
            while let Some(evt) = gate.next(&mut events).await {
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
                        authority: None,
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
                        take_driver_terminal_release(&mut g, &run_id_for_drain, false)
                    } else {
                        None
                    }
                };
                if let Some(release) = terminal_release {
                    finish_driver_terminal_release(&writer, release, driver_release_timeout).await;
                }
            }
            finish_stream_end_terminal_drain(&writer, &inner_for_drain, &run_id_for_drain).await;
        });

        let mut g = self.inner.lock().await;
        if let Some(rec) = g.runs.get_mut(&run_id) {
            rec.event_drain = drain;
        } else {
            drain.abort();
        }
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
        bs: &BabysitterAutoSpawn,
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
            worker_id: bs.worker_id.clone(),
            role: "babysitter".into(),
            project_id,
            worktree: None,
            last_path: None,
            stdout_path: None,
            dispatch_attempt_token: None,
            session_path: bs_path,
            driver_config: bs.driver_config.clone(),
            babysitter_target: Some(target_run.into()),
            stall_timeout_secs: bs.stall_timeout_secs,
            max_run_duration_secs: bs.max_run_duration_secs,
            idle_timeout_secs: None,
            babysitter: None,
            applicable_states: bs.applicable_states.clone(),
            max_iterations: bs.max_iterations,
            planned_identity: None,
        };
        let resp = self.acquire_impl(driver, req, None).await?;
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
                    authority: None,
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
                authority: None,
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
        if !applicable_state_allowed(&rec.applicable_states, &req.to) {
            return Err(SupervisorError::DisallowedSubState(req.to.clone()));
        }
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
    /// terminal event) and distinguish it from failed/orphaned termination.
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
                    // orgasmic:TASK-RB1ZN — `ReleaseInProgress` joins
                    // `RunNotFound` here rather than becoming new warn noise:
                    // before the split, a babysitter someone else was already
                    // releasing answered `RunNotFound` and was swallowed on
                    // exactly this line. Both still mean the same thing to a
                    // cascade — this babysitter is already someone's business.
                    if !matches!(
                        e,
                        SupervisorError::RunNotFound(_) | SupervisorError::ReleaseInProgress(_)
                    ) {
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

    /// Release exactly one run record.
    ///
    /// orgasmic:TASK-RB1ZN — two refusals, never one. `RunNotFound` means the
    /// map does not hold `run_id`: whatever it named is over and carries its own
    /// release tombstone. [`SupervisorError::ReleaseInProgress`] means the map
    /// DOES hold it and another authority already took the release: the run is
    /// live, the caller has nothing to add, and the wait is bounded by
    /// [`RELEASE_DRAIN_BUDGET`]. Both decisions are made under the one lock
    /// guard below, so a caller never has to re-read liveness to interpret the
    /// answer it was given.
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
            transport,
            control,
            producer,
            mut drain,
            watcher,
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
                .get_mut(run_id)
                .ok_or_else(|| SupervisorError::RunNotFound(run_id.into()))?;
            // orgasmic:TASK-RB1ZN — the record is HERE, and someone else is
            // already releasing it. That is not "not found", and the liveness
            // read that says so happens under this same lock guard as the
            // action it refuses (the TASK-1T3FZ close-guard shape), so no
            // caller has to re-read a snapshot afterwards and race the removal.
            if rec.explicit_release_in_progress || rec.early_exit_release_taken {
                return Err(SupervisorError::ReleaseInProgress(run_id.into()));
            }
            // orgasmic:task_3TEDA — stop control and drain while the record
            // remains in the map; freeze classification only after quiescence.
            begin_explicit_release(rec);
            let control = std::mem::replace(&mut rec.control, Box::new(DetachedDriverControl));
            let producer = rec.producer.take();
            let drain = std::mem::replace(&mut rec.event_drain, tokio::spawn(async {}));
            let watcher = rec.early_exit_watcher.take();
            (
                rec.task_id.clone(),
                rec.kind,
                rec.babysitter_run_id.clone(),
                rec.session_path.clone(),
                rec.identity.clone(),
                rec.transport.clone(),
                control,
                producer,
                drain,
                watcher,
            )
        };
        // orgasmic:TASK-QSSQH — the durable finalize-admission boundary.
        //
        // Admission is granted above (`explicit_release_in_progress = true`)
        // and the driver is still alive at this line: the `control.release()`
        // in `stop_and_join_driver_producer` below is what reaps the harness
        // process group, which is what makes the transport synthesize the
        // fatal "exited by signal" driver error TASK-C0XMR had to suppress.
        // Recording the boundary here — awaited, so it is on disk before the
        // teardown that follows can produce anything — is what lets a reader
        // tell "this error was caused by the finalize" from "this failure was
        // already there".
        //
        // The trailing `Lifecycle::Release` cannot carry that boundary. It is
        // appended after the drain, so a genuine pre-finalize failure and a
        // teardown-induced one are BOTH behind it, and both freeze
        // `terminal_outcome` to `Failed` (`terminal_outcome_for_event` maps
        // `RunFail` and fatal `DriverError` alike) — the two cases produce a
        // byte-identical `Release { outcome: Failed, finalized_by_worker:
        // true }`. The test pair
        // `worker_finalize_does_not_clear_a_run_fail_drained_during_teardown`
        // / `worker_finalize_still_suppresses_the_error_its_own_teardown_caused`
        // asserts exactly that: identical releases, opposite stage outcomes.
        //
        // A failed append is not fatal: a session missing the marker degrades
        // to the pre-TASK-QSSQH reading (finalize dominates preceding fatal
        // driver errors), which is also how sessions written before this
        // marker existed are read.
        if finalized_by_worker {
            if let Err(e) = self
                .writer
                .append_session(SessionAppend {
                    run_id: run_id.into(),
                    session_path: session_path.clone(),
                    identity: identity.clone(),
                    authority: None,
                    kind: SessionEventKind::Note,
                    event: serde_json::json!({
                        "note": WORKER_FINALIZE_ADMITTED_NOTE,
                        "reason": reason,
                    }),
                })
                .await
            {
                warn!(
                    error = %e,
                    run_id,
                    "worker finalize admission marker append failed; \
                     teardown errors in this session are no longer separable"
                );
            }
        }
        // Stop the producer first. The receiver remains live while the
        // control path closes its sender, then drains every queued event to
        // channel closure. Only after both producer and receiver are joined do
        // we freeze one terminal outcome. A legitimate terminal event racing
        // timeout/cancel therefore wins; no pre-stop snapshot can overwrite
        // an event that was durably drained during shutdown.
        stop_and_join_driver_producer(
            run_id,
            &transport,
            control,
            producer,
            reason,
            self.driver_release_timeout(),
        )
        .await;
        if let Some(watcher) = watcher {
            watcher.abort();
            let _ = watcher.await;
        }
        // Producer completion proves every sender is gone. Drain the receiver
        // to closure; never abort the receiver while an event can still land.
        let _ = (&mut drain).await;
        let (final_outcome, cleared_babysitter_backoff) = {
            let mut g = self.inner.lock().await;
            let rec = g
                .runs
                .remove(run_id)
                .ok_or_else(|| SupervisorError::RunNotFound(run_id.into()))?;
            g.remove_lease(&lease_key(
                rec.project_id.as_deref(),
                &rec.task_id,
                rec.kind,
            ));
            let cleared_babysitter_backoff = if rec.kind == RunKind::Babysitter {
                g.babysitter_auto_spawn_backoff
                    .remove(&rec.task_id)
                    .is_some()
            } else {
                false
            };
            (
                rec.terminal_outcome.unwrap_or(outcome),
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
                authority: None,
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

    /// Return a boot-reattached recovery run only when its durable identity,
    /// containing session, and project still match the immutable recovery
    /// plan. A fresh daemon may reattach the live mux handle before the
    /// operator retries POST /recover; the retry must reuse that owned handle
    /// rather than attempting a second attach against the same lease.
    pub async fn recovery_run_if_exact(
        &self,
        identity: &RuntimeIdentity,
        session_path: &Path,
        project_id: &str,
    ) -> Option<AcquireResponse> {
        let g = self.inner.lock().await;
        let rec = g.runs.get(&identity.run_id)?;
        (rec.identity.run_id == identity.run_id
            && rec.identity.runtime_id == identity.runtime_id
            && rec.identity.boot_id == identity.boot_id
            && rec.session_path == session_path
            && rec.project_id.as_deref() == Some(project_id))
        .then(|| AcquireResponse {
            run_id: identity.run_id.clone(),
            identity: identity.clone(),
            pid: rec.early_exit_watcher_pid,
        })
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
            rec.dispatch_attempt_token = dispatch_attempt_token;
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

    /// Restore immutable recovery-run timeout and cleanup-adjacent lifecycle
    /// options after reattaching a pre-Acquire runtime handle.
    pub async fn restore_recovery_run_options(
        &self,
        run_id: &str,
        stall_timeout_secs: Option<u32>,
        max_run_duration_secs: Option<u32>,
        idle_timeout_secs: Option<u32>,
        babysitter_target: Option<String>,
    ) {
        let mut g = self.inner.lock().await;
        if let Some(rec) = g.runs.get_mut(run_id) {
            rec.stall_timeout = resolve_timeout_secs(stall_timeout_secs, DEFAULT_STALL_TIMEOUT);
            rec.max_run_duration =
                resolve_timeout_secs(max_run_duration_secs, DEFAULT_MAX_RUN_DURATION);
            rec.idle_timeout = resolve_idle_timeout_secs(idle_timeout_secs);
            rec.babysitter_target = babysitter_target;
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
        // orgasmic:TASK-JK66P — the event channel says no work arrived. Before
        // shooting a run on that, look at the channel the events cannot carry:
        // what is running under the run right now. Only the stall clock asks —
        // `max_run_duration` is an absolute ceiling by design, and idle is
        // about operator input, not work.
        let missing_evidence = if revalidated.reason == STALL_TIMEOUT_REASON {
            let target = {
                let g = self.inner.lock().await;
                g.runs.get(&revalidated.run_id).map(|rec| WorkProbeTarget {
                    transport: rec.transport.clone(),
                    identity: rec.identity.clone(),
                    pid: rec.early_exit_watcher_pid,
                })
            };
            let Some(target) = target else {
                return;
            };
            match self.observe_work_evidence(target).await {
                WorkEvidence::Working { detail } => {
                    // Credit the probe as the progress event the transport
                    // could not emit, and leave the run alone. A run whose work
                    // never ends still meets `max_run_duration`.
                    let mut g = self.inner.lock().await;
                    let Some(rec) = g.runs.get_mut(&revalidated.run_id) else {
                        return;
                    };
                    rec.last_progress_at = Instant::now();
                    tracing::info!(
                        run_id = %revalidated.run_id,
                        quiet_secs = revalidated.elapsed.as_secs(),
                        evidence = %detail,
                        "stall deadline overridden: live work under a quiet run"
                    );
                    return;
                }
                WorkEvidence::Idle { detail } => Some(detail),
                WorkEvidence::Unknown => None,
            }
        } else {
            None
        };
        warn!(
            run_id = %revalidated.run_id,
            reason = revalidated.reason,
            threshold_secs = revalidated.threshold.as_secs(),
            elapsed_secs = revalidated.elapsed.as_secs(),
            work_evidence = missing_evidence.as_deref().unwrap_or("not established"),
            "supervisor run timeout exceeded"
        );
        // orgasmic:TASK-JK66P — name what was absent and for how long, so the
        // operator reading a tombstone can tell a wedge from a worker that was
        // killed mid-build. The reason's FIRST TOKEN stays
        // `stall_timeout_exceeded`; consumers classify on that token.
        let timeout_reason = match &missing_evidence {
            Some(detail) => format!(
                "{}: no work evidence for {}s; {detail}",
                revalidated.reason,
                revalidated.elapsed.as_secs()
            ),
            None => revalidated.reason.to_string(),
        };
        // orgasmic:TASK-S52X9 — hot-session artifactor that already submitted
        // ends idle with the finalize tombstone (Completed), not Failed.
        let (reason, outcome, finalized) = {
            let g = self.inner.lock().await;
            match g.runs.get(&revalidated.run_id).and_then(|rec| {
                rec.terminal_declaration
                    .filter(|decl| decl.round == rec.terminal_round)
                    .map(|decl| decl.reason)
            }) {
                Some(declared) => (declared.to_string(), ReleaseOutcome::Completed, true),
                None => (timeout_reason, ReleaseOutcome::Failed, false),
            }
        };
        if let Err(e) = self
            .release_with_finalization(&revalidated.run_id, &reason, outcome, finalized, None)
            .await
        {
            if matches!(e, SupervisorError::DeferredWhileInFlight(_)) {
                // In-flight artifactor writer/regenerate — defer; never a
                // false Failed timeout tombstone (TASK-ARZGD).
                return;
            }
            // orgasmic:TASK-RB1ZN — same reading as before the split: a run
            // another authority is already releasing is not a failed sweep.
            if !matches!(
                e,
                SupervisorError::RunNotFound(_) | SupervisorError::ReleaseInProgress(_)
            ) {
                warn!(
                    error = %e,
                    run_id = %revalidated.run_id,
                    reason = %reason,
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
    async fn release_dispatch_worker_for_cleanup(
        &self,
        params: &DispatchCleanupParams,
    ) -> Result<Option<String>, SupervisorError> {
        let key = lease_key(Some(&params.project_id), &params.task_id, params.kind);
        let run_id = {
            let g = self.inner.lock().await;
            g.lease(&key).cloned()
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
                    params.dispatch_attempt_token.as_deref(),
                    &params.worktree_path,
                    params.last_path.as_deref(),
                    params.stdout_path.as_deref(),
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
            project_id: params.project_id.clone(),
            task_id: params.task_id.clone(),
            kind: params.kind,
            worktree_key: worktree_key.clone(),
        };
        // Minted at install time, and the only thing release needs (TASK-95SGV):
        // cleanup deletes the directory, so by the time the release runs the
        // worktree key can no longer be recomputed from the path — on macOS
        // `/var/...` canonicalizes only while it exists.
        let cleanup_guard_id = format!("cleanup-guard-{}", Uuid::new_v4());
        let reservation = DispatchCleanupReservation {
            branch: params.branch.clone(),
            worktree_path: params.worktree_path.clone(),
            dispatch_attempt_token: params.dispatch_attempt_token.clone(),
            last_path: params.last_path.clone(),
            stdout_path: params.stdout_path.clone(),
            holder: Some(CloseGuardHolder {
                close_guard_id: cleanup_guard_id.clone(),
                owner_pid: Some(std::process::id()),
                governed_by: HolderIdentity::DaemonCleanup,
                lease_expires_at: Utc::now() + close_guard_lease_ttl(),
            }),
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
            drop_abandoned_cleanup_reservations(&mut g);
            if g.cleanup_reservations
                .keys()
                .any(|held| held.worktree_key == worktree_key)
            {
                return Ok(DispatchCleanupOutcome::Conflict);
            }
            let active_lease = lease_key(Some(&params.project_id), &params.task_id, params.kind);
            if g.lease(&active_lease)
                .is_some_and(|run_id| !g.runs.contains_key(run_id))
            {
                // An acquire owns this lease but has not installed its
                // RunRecord yet. Cleanup cannot prove its filesystem identity
                // and must fail closed rather than steal the in-flight attempt.
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
                    self.finish_dispatch_cleanup(&cleanup_guard_id).await;
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
            CleanupIdentityAuth::IdentityMismatch => {
                self.finish_dispatch_cleanup(&cleanup_guard_id).await;
                return Ok(DispatchCleanupOutcome::Conflict);
            }
            CleanupIdentityAuth::NoOwner | CleanupIdentityAuth::ExactOwner => {}
        }

        let released_run_id = match self.release_dispatch_worker_for_cleanup(params).await {
            Ok(released_run_id) => released_run_id,
            Err(err) => {
                // TASK-95SGV.1 — never strand the reservation we just
                // installed. `release_dispatch_worker_for_cleanup` can refuse
                // with `ReleaseInProgress` (another authority is already
                // releasing this run) or `DeferredWhileInFlight` /
                // `RunNotFound`; every such edge used to propagate straight
                // through the `?` and leave the daemon-owned cleanup
                // reservation held until restart, so the task's deterministic
                // worktree path became permanently undispatchable. Release
                // it by its minted handle — never by a key recomputed from a
                // path cleanup may already have deleted (TASK-95SGV) — before
                // propagating, so the refusal does not strand.
                self.finish_dispatch_cleanup(&cleanup_guard_id).await;
                return Err(err);
            }
        };

        Ok(DispatchCleanupOutcome::Proceed {
            released_run_id,
            cleanup_guard_id,
        })
    }

    /// Release a dispatch cleanup reservation after filesystem mutation
    /// finishes. Release is by the opaque handle minted in
    /// [`Supervisor::prepare_dispatch_cleanup`], never by a key recomputed from
    /// the worktree path: cleanup has usually deleted that directory, and a
    /// path that no longer exists does not canonicalize to the key it was
    /// installed under (TASK-95SGV). Unknown ids are a no-op, mirroring
    /// [`Supervisor::finish_dispatch_close`].
    pub async fn finish_dispatch_cleanup(&self, cleanup_guard_id: &str) {
        let mut g = self.inner.lock().await;
        g.cleanup_reservations.retain(|_, reservation| {
            reservation
                .holder
                .as_ref()
                .is_none_or(|holder| holder.close_guard_id != cleanup_guard_id)
        });
    }

    /// Reserve a worktree for a destructive `dispatch-close`, and decide, under
    /// the same lock, whether a live worker occupies it (TASK-1T3FZ).
    ///
    /// The reviewer finding this exists for: `dispatch-close` used to decide
    /// liveness in the CLI process and only then remove the worktree, with
    /// nothing reserving that worktree in daemon state across the gap. A
    /// concurrent `POST /runs/:origin/recover` acquires in *another process*,
    /// so no audit of `await` points in the CLI can close that window — the
    /// authority has to sit where the supervisor lock already is.
    ///
    /// The order inside the lock is the whole point, and it is the order
    /// [`Supervisor::prepare_dispatch_cleanup`] established: **install the
    /// fence first, decide liveness second**. Installing first is what makes
    /// the verdict monotone — from that instant `acquire_impl` refuses every
    /// new run for this worktree, so the set of occupants can only shrink, and
    /// a `Reserved` verdict stays true until the guard is released. Deciding
    /// first and reserving afterwards would leave exactly the window this task
    /// was filed to close.
    ///
    /// A blocked verdict reserves nothing: the caller is not cleaning up, so
    /// it must not fence anyone out either.
    pub async fn reserve_dispatch_close(
        &self,
        params: &DispatchCloseGuardParams,
    ) -> DispatchCloseGuardOutcome {
        // orgasmic:TASK-AK6EM — ask 2. Before any of the below means anything,
        // the run map has to be a complete answer to "who is live here". On a
        // daemon that is still rehydrating the previous daemon's runtimes it is
        // not: a worker whose mux session outlived the restart is alive in its
        // worktree and simply not in the map yet. Fencing it out of `reattach`
        // would only mean deleting its files without it watching. So the close
        // waits for rehydration to resolve, and refuses if it does not.
        if !self
            .wait_for_boot_reattach(CLOSE_GUARD_BOOT_REATTACH_WAIT)
            .await
        {
            return DispatchCloseGuardOutcome::BootReattachPending;
        }

        let worktree_key = normalize_cleanup_worktree(&params.worktree_path);
        let guard_id = format!("close-guard-{}", Uuid::new_v4());
        let reservation_key = CleanupReservationKey {
            project_id: params.project_id.clone(),
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
            holder: Some(CloseGuardHolder {
                close_guard_id: guard_id.clone(),
                owner_pid: params.owner_pid,
                governed_by: CloseGuardHolder::identity_for(params.owner_pid),
                lease_expires_at: Utc::now() + close_guard_lease_ttl(),
            }),
        };

        // One lock section, no awaits, from the fence to the verdict.
        let mut g = self.inner.lock().await;
        drop_abandoned_cleanup_reservations(&mut g);
        if g.cleanup_reservations
            .keys()
            .any(|held| held.worktree_key == worktree_key)
        {
            return DispatchCloseGuardOutcome::ReservationHeld;
        }
        g.cleanup_reservations
            .insert(reservation_key.clone(), reservation.clone());

        let blocking = blocking_run_for_close(&g, params, &worktree_key);
        if let Some((run_id, worktree)) = blocking {
            g.cleanup_reservations.remove(&reservation_key);
            return DispatchCloseGuardOutcome::BlockedByLiveRun { run_id, worktree };
        }
        // Persisted only once the verdict is `Reserved`: a fence that was taken
        // back is not a guard anyone holds, and must not be inherited.
        g.close_guards.write(&PersistedCloseGuard {
            project_id: params.project_id.clone(),
            task_id: params.task_id.clone(),
            kind: params.kind,
            worktree_key,
            reservation,
        });
        DispatchCloseGuardOutcome::Reserved {
            guard_id,
            renew_within: CLOSE_GUARD_RENEW_WITHIN,
        }
    }

    /// Extend a close guard's holder lease. `false` means no such guard is
    /// held — it was finished, or reclaimed as abandoned — which the holder
    /// needs to hear about, because it is still deleting files.
    pub async fn renew_dispatch_close(&self, guard_id: &str) -> bool {
        let mut g = self.inner.lock().await;
        let deadline = Utc::now() + close_guard_lease_ttl();
        let Some((key, reservation)) = g
            .cleanup_reservations
            .iter_mut()
            .find(|(_, reservation)| {
                reservation
                    .holder
                    .as_ref()
                    .is_some_and(|holder| holder.close_guard_id == guard_id)
            })
            .map(|(key, reservation)| (key.clone(), reservation))
        else {
            return false;
        };
        if let Some(holder) = reservation.holder.as_mut() {
            holder.lease_expires_at = deadline;
        }
        let record = PersistedCloseGuard {
            project_id: key.project_id.clone(),
            task_id: key.task_id.clone(),
            kind: key.kind,
            worktree_key: key.worktree_key.clone(),
            reservation: reservation.clone(),
        };
        g.close_guards.write(&record);
        true
    }

    /// Move a held guard's renewal lease into the past, as if its holder had
    /// stopped renewing for a full TTL. Tests only: it simulates elapsed time,
    /// not the reclamation decision, which still runs for real.
    #[cfg(test)]
    pub(crate) async fn expire_close_guard_lease_for_test(&self, guard_id: &str) {
        let mut g = self.inner.lock().await;
        for reservation in g.cleanup_reservations.values_mut() {
            if let Some(holder) = reservation.holder.as_mut() {
                if holder.close_guard_id == guard_id {
                    holder.lease_expires_at = Utc::now() - chrono::Duration::seconds(1);
                }
            }
        }
    }

    /// Rewrite a held reservation's recorded owner pid, as if the process that
    /// installed it had died and its pid were observed stale. Tests only: it
    /// simulates the dead holder, while the reclamation decision still runs for
    /// real.
    #[cfg(test)]
    pub(crate) async fn set_cleanup_owner_pid_for_test(&self, guard_id: &str, owner_pid: u32) {
        let mut g = self.inner.lock().await;
        for reservation in g.cleanup_reservations.values_mut() {
            if let Some(holder) = reservation.holder.as_mut() {
                if holder.close_guard_id == guard_id {
                    holder.owner_pid = Some(owner_pid);
                }
            }
        }
    }

    /// Release a `dispatch-close` guard once worktree and branch cleanup has
    /// finished (or failed). Unknown ids are a no-op — the sweep in
    /// [`drop_abandoned_cleanup_reservations`] is the backstop for a holder
    /// that never gets here.
    pub async fn finish_dispatch_close(&self, guard_id: &str) {
        let mut g = self.inner.lock().await;
        g.cleanup_reservations.retain(|_, reservation| {
            reservation
                .holder
                .as_ref()
                .is_none_or(|holder| holder.close_guard_id != guard_id)
        });
        g.close_guards.remove(guard_id);
    }

    /// Clear an *orphaned* lease: one held for `(project_id, task_id, kind)`
    /// whose run record no longer exists (e.g. the CLI timed out while the
    /// daemon completed the acquire, then the run died without releasing).
    /// Returns what happened so the caller can report it honestly. A lease
    /// backed by a live run is NOT cleared — release the run instead.
    pub async fn release_orphaned_lease(
        &self,
        project_id: &str,
        task_id: &str,
        kind: RunKind,
    ) -> OrphanedLeaseOutcome {
        let mut g = self.inner.lock().await;
        let key = lease_key(Some(project_id), task_id, kind);
        let Some(run_id) = g.lease(&key).cloned() else {
            return OrphanedLeaseOutcome::NoLease;
        };
        if g.runs.contains_key(&run_id) {
            return OrphanedLeaseOutcome::HeldByLiveRun { run_id };
        }
        g.remove_lease(&key);
        OrphanedLeaseOutcome::Released { run_id }
    }
}

/// Whether a dispatch cleanup request may proceed with filesystem mutation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DispatchCleanupOutcome {
    /// Identity matches; optional released run id when a live worker was
    /// stopped. `cleanup_guard_id` is the opaque handle the caller must pass to
    /// [`Supervisor::finish_dispatch_cleanup`] once filesystem mutation is done
    /// (TASK-95SGV).
    Proceed {
        released_run_id: Option<String>,
        cleanup_guard_id: String,
    },
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

fn spawn_early_exit_watcher(
    supervisor: Supervisor,
    run_id: String,
    pid: u32,
) -> tokio::task::JoinHandle<()> {
    let inner = supervisor.inner.clone();
    let driver_release_timeout = supervisor.driver_release_timeout();
    tokio::spawn(async move {
        loop {
            let still_watching = {
                let guard = inner.lock().await;
                guard.runs.get(&run_id).is_some_and(|rec| {
                    rec.early_exit_watcher_pid == Some(pid) && !rec.early_exit_release_taken
                })
            };
            if !still_watching {
                return;
            }
            let stream_ended = {
                let guard = inner.lock().await;
                guard.runs.get(&run_id).is_some_and(|rec| rec.stream_ended)
            };
            let exited = if process_is_zombie(pid) {
                true
            } else if stream_ended {
                subprocess_exited_or_unprobeable(pid)
            } else {
                subprocess_exited(pid)
            };
            if exited {
                let shutdown = {
                    let mut guard = inner.lock().await;
                    let Some(rec) = guard.runs.get_mut(&run_id) else {
                        return;
                    };
                    if rec.early_exit_watcher_pid != Some(pid) {
                        return;
                    }
                    // PID observation never classifies or removes. It only
                    // requests producer shutdown, whose sender closure lets
                    // the receiver drain and own the sole normal release.
                    rec.early_exit_pid_exited = true;
                    if rec.explicit_release_in_progress || rec.early_exit_release_taken {
                        None
                    } else {
                        begin_explicit_release(rec);
                        rec.pid_exit_shutdown_in_progress = true;
                        Some((
                            rec.transport.clone(),
                            std::mem::replace(&mut rec.control, Box::new(DetachedDriverControl)),
                            rec.producer.take(),
                        ))
                    }
                };
                if let Some((transport, control, producer)) = shutdown {
                    stop_and_join_driver_producer(
                        &run_id,
                        &transport,
                        control,
                        producer,
                        "observed subprocess exit",
                        driver_release_timeout,
                    )
                    .await;
                }
                return;
            }
            tokio::time::sleep(Duration::from_millis(250)).await;
        }
    })
}

#[cfg(unix)]
fn subprocess_exited_or_unprobeable(pid: u32) -> bool {
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
    match result {
        Ok(()) | Err(Some(libc::EPERM)) => false,
        Err(Some(libc::ESRCH)) => true,
        Err(errno) => {
            tracing::warn!(
                pid,
                ?errno,
                "unexpected kill(pid, 0) after stream end; treating as exited"
            );
            true
        }
    }
}

#[cfg(not(unix))]
fn subprocess_exited_or_unprobeable(_pid: u32) -> bool {
    true
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

    // orgasmic:TASK-VZMZE — keyed on `last_progress_at`, NOT on
    // `last_driver_event_at`. The distinction is the whole task: a heartbeat is
    // signal on the channel, not evidence of work, and a clock that cannot tell
    // them apart declares a run that made 0 tool calls in an hour exactly as
    // healthy as one that made 243.
    let stall = rec.stall_timeout.and_then(|threshold| {
        let elapsed = now.saturating_duration_since(rec.last_progress_at);
        (elapsed > threshold).then(|| RunTimeoutCandidate {
            run_id: run_id.to_string(),
            reason: STALL_TIMEOUT_REASON,
            threshold,
            elapsed,
            deadline: rec.last_progress_at + threshold,
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

// ───────────────────────── work evidence (TASK-JK66P) ──────────────────────
//
// The stall clock above reads one channel: driver events. A worker whose tool
// subprocess is building for ten minutes emits nothing on that channel — its
// pane is a terminal, and `scripts/run-tests.sh` redirects cargo to files — so
// the absence of events is not, on its own, the absence of work. The probe
// below is the second channel: what is actually running under the run. It also
// carries the third (TASK-JQ8AV): a run whose subtree is quiet because the
// harness is waiting on the provider — a multi-minute server-side think is a
// network wait — shows an open-turn statusline in its pane, and that content
// survives the very freeze that silences the byte channels.

/// Minimum CPU consumption, summed as a percentage of one core over the process
/// subtree under a run, that counts as "work is happening here".
///
/// Both bounds this sits between are measured, not chosen:
///
/// - TASK-VZMZE's wedged codex run consumed **6.77 s of CPU in 60 minutes** —
///   0.19 % of one core — while emitting a heartbeat every 30 s. It must not
///   read as work, or the fix for JK66P reintroduces VZMZE.
/// - TASK-JK66P's healthy claude/rmux worker was inside
///   `scripts/run-tests.sh`: a cargo build saturates at least one core and its
///   subtree reads in the hundreds of percent.
///
/// 5 % is more than an order of magnitude above the wedge and more than an
/// order below the build, so neither classification depends on exactly where in
/// that gap the line falls.
const MIN_WORK_CPU_PERCENT: f32 = 5.0;

/// What the daemon could establish about live work under a run at the moment
/// its stall deadline expired.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum WorkEvidence {
    /// Work is demonstrably running under this run right now. The stall clock
    /// was reading the wrong channel; it gets credited and the run lives.
    Working { detail: String },
    /// The daemon looked and found none. The stall stands, and `detail` says
    /// what was looked at so the operator can tell a wedge from a quiet worker.
    Idle { detail: String },
    /// The daemon could not look: no probe target, no rmux binary, no session,
    /// `ps` unavailable, or the probe outran [`WORK_PROBE_TIMEOUT`]. The stall
    /// stands on today's bare reason — a probe that cannot run must not be able
    /// to save a run, or a broken probe is an immortality switch.
    Unknown,
}

/// Everything a [`WorkEvidenceProbe`] is allowed to know about a run.
#[derive(Debug, Clone)]
pub(crate) struct WorkProbeTarget {
    /// Driver transport (`rmux`, `acp-stdio`, …).
    transport: String,
    /// Run identity — for pane transports this is what names the rmux session.
    identity: RuntimeIdentity,
    /// Wrapper pid, for transports that own a direct child process. `None` for
    /// pane transports: a tmux pane is a child of the mux server, not of us.
    pid: Option<u32>,
}

/// The second channel the stall detector consults before releasing a run.
///
/// A trait rather than a free function because no unit test can spawn a real
/// cargo build under a real rmux pane; the production implementation is proven
/// separately against a real process subtree
/// (`process_subtree_cpu_probe_*`), and the supervisor tests drive the
/// decision with doubles.
pub(crate) trait WorkEvidenceProbe: Send + Sync {
    fn observe(&self, target: &WorkProbeTarget) -> WorkEvidence;
}

/// Production probe: CPU burned by the process subtree under the run, and —
/// when that is quiet — whether the run's pane shows an open provider turn.
///
/// Liveness alone is deliberately NOT the test. VZMZE's wedged harness was
/// alive for the entire hour it did nothing; MRJRK's healthy worker and that
/// wedge are indistinguishable by "is a process there?" and separated by three
/// orders of magnitude of CPU.
///
/// orgasmic:TASK-JQ8AV — CPU is blind to a provider-bound turn. Measured
/// 2026-07-29, first hours of this clock in production: three healthy
/// claude-opus-5 high-effort workers were released with `no work evidence for
/// 600s; 1 process(es) at 0.0-0.3% cpu` while verifiably mid-turn — a long
/// server-side think is a network wait (~0% local cpu) and, in that state,
/// the TUI stops repainting, so pane bytes and CPU both read as VZMZE's
/// wedge. The channel that still discriminates is the pane's *content*: the
/// harness TUI keeps its open-turn statusline on screen for exactly as long
/// as a turn is open (the incident's own live capture read `Quantumizing…
/// (3m41s · ↓13.1k tokens · thinking with high effort)`), and removes it at
/// rest. Network-side candidates were measured and rejected: connection
/// existence is always-true for a live harness (telemetry sockets persist
/// idle for tens of minutes — the socket-channel analog of the heartbeat
/// trap), and traffic-rate thresholds cannot separate a slow think from the
/// bridge-session streaming that rides the same api-host sockets.
#[derive(Default)]
pub(crate) struct ProcessSubtreeCpuProbe {
    /// Explicit rmux socket for the pane lookups. `None` — production — is
    /// the default endpoint, where every dispatch session lives. Tests pin
    /// their owned server's socket so the probe reads panes the test created
    /// instead of live dispatch panes.
    rmux_socket: Option<std::path::PathBuf>,
}

#[cfg(test)]
impl ProcessSubtreeCpuProbe {
    fn with_rmux_socket(socket: &std::path::Path) -> Self {
        Self {
            rmux_socket: Some(socket.to_path_buf()),
        }
    }
}

impl WorkEvidenceProbe for ProcessSubtreeCpuProbe {
    fn observe(&self, target: &WorkProbeTarget) -> WorkEvidence {
        let socket = self.rmux_socket.as_deref();
        let Some(root) = work_probe_root_pid(target, socket) else {
            return WorkEvidence::Unknown;
        };
        let Some(table) = process_cpu_table() else {
            return WorkEvidence::Unknown;
        };
        let Some((processes, cpu_percent)) = subtree_cpu_percent(&table, root) else {
            return WorkEvidence::Idle {
                detail: format!("no live process under pid {root}"),
            };
        };
        let cpu_detail = format!(
            "{processes} process(es) under pid {root} at {cpu_percent:.1}% cpu \
             (work threshold {MIN_WORK_CPU_PERCENT:.1}%)"
        );
        if cpu_percent >= MIN_WORK_CPU_PERCENT {
            return WorkEvidence::Working { detail: cpu_detail };
        }
        // orgasmic:TASK-JQ8AV — the subtree is quiet; before calling that a
        // wedge, read the one channel that can still see a provider-bound
        // turn. Only rescues: a pane that cannot be read cannot save a run
        // (JK66P's fail-closed rule), it only gets named in the reason.
        if target.transport == "rmux" {
            match rmux_pane_content(&target.identity, socket) {
                Some(pane) => match pane_open_turn_marker(&pane) {
                    Some(marker) => {
                        return WorkEvidence::Working {
                            detail: format!(
                                "provider-bound turn open: pane statusline {marker:?}; \
                                 {cpu_detail}"
                            ),
                        };
                    }
                    None => {
                        return WorkEvidence::Idle {
                            detail: format!(
                                "{cpu_detail}; no open-turn statusline in pane capture"
                            ),
                        };
                    }
                },
                None => {
                    return WorkEvidence::Idle {
                        detail: format!("{cpu_detail}; pane capture unavailable"),
                    };
                }
            }
        }
        WorkEvidence::Idle { detail: cpu_detail }
    }
}

/// The process to walk down from. A subprocess transport hands us its wrapper
/// pid at acquire; a pane transport has none, so the pane's root process is
/// resolved from the mux by the run-scoped session name the driver built from
/// the same identity.
fn work_probe_root_pid(target: &WorkProbeTarget, socket: Option<&std::path::Path>) -> Option<u32> {
    if let Some(pid) = target.pid.filter(|pid| *pid != 0) {
        return Some(pid);
    }
    (target.transport == "rmux")
        .then(|| rmux_pane_pid(&target.identity, socket))
        .flatten()
}

/// An `rmux` invocation for the probe's read-only pane lookups, addressed at
/// the default endpoint in production (`socket: None`, where a dispatch's
/// session lives) or at an explicit `-S` socket in tests. A run on a private
/// endpoint resolves to `None` and therefore [`WorkEvidence::Unknown`], i.e.
/// the pre-probe behavior.
fn rmux_probe_command(socket: Option<&std::path::Path>) -> Option<Command> {
    let probe = orgasmic_drivers::modes::rmux::probe_rmux_binary();
    let rmux_bin = probe.path.filter(|_| probe.found)?;
    let mut cmd = Command::new(rmux_bin);
    if let Some(socket) = socket {
        cmd.arg("-S").arg(socket);
    }
    cmd.stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null());
    Some(cmd)
}

/// `rmux display-message -p -t <session> '#{pane_pid}'` — the pane's root
/// process (the shell the harness runs in), whose descendants are the harness
/// and everything the harness spawned.
fn rmux_pane_pid(identity: &RuntimeIdentity, socket: Option<&std::path::Path>) -> Option<u32> {
    let session = orgasmic_drivers::modes::rmux::rmux_session_name(identity);
    let output = rmux_probe_command(socket)?
        .args(["display-message", "-p", "-t", &session, "#{pane_pid}"])
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    String::from_utf8_lossy(&output.stdout).trim().parse().ok()
}

/// `rmux capture-pane -p -t <session>` — the pane's current *screen content*,
/// which survives exactly the state that starves the byte channels: a frozen
/// TUI keeps its last frame, and that frame carries the open-turn statusline.
// orgasmic:TASK-JQ8AV
fn rmux_pane_content(
    identity: &RuntimeIdentity,
    socket: Option<&std::path::Path>,
) -> Option<String> {
    let session = orgasmic_drivers::modes::rmux::rmux_session_name(identity);
    let output = rmux_probe_command(socket)?
        .args(["capture-pane", "-p", "-t", &session])
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    Some(String::from_utf8_lossy(&output.stdout).into_owned())
}

/// The harness TUI's open-turn statusline, if the captured pane shows one.
///
/// Both accepted shapes are measured, not guessed (2026-07-29):
///
/// - claude in-turn, live: `● Moonwalking… (20m 4s · ↓ 46.3k tokens)`; the
///   incident capture: `✽ Quantumizing… (3m41s · ↓13.1k tokens · thinking
///   with high effort)`. Anchor: a leading non-alphanumeric spinner glyph,
///   then `… (`, then either a `↓ … tokens` stream counter or an elapsed
///   time starting with a digit.
/// - the generic TUI interrupt hint (`esc to interrupt`, codex-style),
///   case-insensitive, for harnesses whose spinner line differs.
///
/// A claude pane at rest was measured to show none of these: the prompt box,
/// the `● ~/path | …` status bar (glyph but no `… (`), and collapsed
/// transcript lines (`⏺ Thinking for 8m 28s, …` — ellipsis but no `… (`) all
/// fall through. The last match wins because the live statusline sits at the
/// bottom of the screen, below any transcript text that might quote one.
///
/// The claim this encodes is deliberately bounded: a TUI frozen mid-turn with
/// the statusline burned on screen is rescued sweep after sweep, but only
/// until `max_run_duration` — "turn open" past that ceiling is its own
/// timeout class, not immortality.
// orgasmic:TASK-JQ8AV
fn pane_open_turn_marker(pane: &str) -> Option<String> {
    let mut marker = None;
    for raw in pane.lines() {
        let line = raw.trim();
        if line.is_empty() {
            continue;
        }
        if line.to_lowercase().contains("esc to interrupt") {
            marker = Some(line);
            continue;
        }
        let mut chars = line.chars();
        let Some(glyph) = chars.next() else {
            continue;
        };
        if glyph.is_alphanumeric() || chars.next() != Some(' ') {
            continue;
        }
        let Some((_, after)) = line.split_once("… (") else {
            continue;
        };
        let streaming = after.contains('↓') && after.contains("tokens");
        let elapsed = after.chars().next().is_some_and(|c| c.is_ascii_digit());
        if streaming || elapsed {
            marker = Some(line);
        }
    }
    marker.map(|line| line.chars().take(120).collect())
}

/// `(pid, ppid, %cpu)` for every process on the box, in one `ps` call.
///
/// `%cpu` is what both platforms already compute: a decaying utilization
/// average on macOS, cpu-time-over-lifetime on Linux. The Linux form
/// under-reports a long-lived process that only just started working — which is
/// why the sum is taken over the whole subtree, where the freshly spawned,
/// short-lived, CPU-bound children (cargo, rustc) report accurately.
fn process_cpu_table() -> Option<Vec<(u32, u32, f32)>> {
    let output = Command::new("ps")
        .args(["ax", "-o", "pid=,ppid=,pcpu="])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let table: Vec<(u32, u32, f32)> = String::from_utf8_lossy(&output.stdout)
        .lines()
        .filter_map(|line| {
            let mut parts = line.split_whitespace();
            let pid = parts.next()?.parse::<u32>().ok()?;
            let ppid = parts.next()?.parse::<u32>().ok()?;
            let pcpu = parts.next()?.parse::<f32>().ok()?;
            Some((pid, ppid, pcpu))
        })
        .collect();
    (!table.is_empty()).then_some(table)
}

/// Process count and summed `%cpu` for `root` and every descendant of it.
/// `None` when `root` is not in the table at all — the process is gone.
fn subtree_cpu_percent(table: &[(u32, u32, f32)], root: u32) -> Option<(usize, f32)> {
    let mut frontier = vec![root];
    let mut seen = std::collections::BTreeSet::new();
    let mut processes = 0_usize;
    let mut cpu_percent = 0.0_f32;
    let mut found_root = false;
    while let Some(pid) = frontier.pop() {
        if !seen.insert(pid) {
            continue;
        }
        if let Some((_, _, pcpu)) = table.iter().find(|(candidate, _, _)| *candidate == pid) {
            if pid == root {
                found_root = true;
            }
            processes += 1;
            cpu_percent += pcpu;
        }
        frontier.extend(
            table
                .iter()
                .filter(|(_, ppid, _)| *ppid == pid)
                .map(|(child, _, _)| *child),
        );
    }
    found_root.then_some((processes, cpu_percent))
}

fn terminal_outcome_for_event(evt: &DriverEvent) -> Option<ReleaseOutcome> {
    match evt {
        DriverEvent::RunComplete { .. } => Some(ReleaseOutcome::Completed),
        DriverEvent::RunFail { .. } => Some(ReleaseOutcome::Failed),
        DriverEvent::DriverError { fatal: true, .. } => Some(ReleaseOutcome::Failed),
        _ => None,
    }
}

fn applicable_state_allowed(allowed: &[String], target: &str) -> bool {
    if allowed.is_empty() {
        return true;
    }
    target
        .rsplit_once('.')
        .map(|(_, verb)| allowed.iter().any(|allowed| allowed == verb))
        .unwrap_or(false)
}

fn apply_driver_event_to_record(
    rec: &mut RunRecord,
    evt: &DriverEvent,
    event_at: Instant,
    terminal_outcome: Option<ReleaseOutcome>,
) {
    rec.last_driver_event_at = event_at;
    if driver_event_advances_stall_clock(evt) {
        rec.last_progress_at = event_at;
    }
    if let Some(outcome) = terminal_outcome {
        // A release-triggered control acknowledgement must not downgrade the
        // failure that caused transport shutdown (including iteration budget
        // breach) to Completed while queued events drain.
        if !(rec.terminal_event_shutdown_in_progress
            && rec.terminal_outcome == Some(ReleaseOutcome::Failed))
        {
            rec.terminal_outcome = Some(outcome);
        }
    }
    if matches!(evt, DriverEvent::Ready { .. }) {
        rec.driver_has_ready = true;
    }
    if driver_event_counts_as_work(evt) {
        rec.driver_has_work = true;
    }
    if matches!(
        evt,
        DriverEvent::RunComplete { .. } | DriverEvent::RunFail { .. }
    ) {
        rec.driver_has_terminal = true;
    }
    if let DriverEvent::TransitionState { to, .. } = evt {
        if applicable_state_allowed(&rec.applicable_states, to) {
            if let Ok(sub_state) = RunSubState::new(to.clone()) {
                rec.sub_state = Some(sub_state);
            }
        } else {
            tracing::warn!(
                target = %to,
                allowed = ?rec.applicable_states,
                "driver transition ignored: sub-state not in applicable_states"
            );
        }
    }
}

/// Whether an event is evidence the *worker* did something, for the early-exit
/// "reached ready and did no work" classification.
///
/// `PaneActivity` is excluded for the same reason `Heartbeat` is: a TUI paints
/// its own banner the moment it launches, so pane output is guaranteed even for
/// a harness that immediately wedges. It is a stall-clock signal (TASK-RWCRN),
/// not proof of work.
fn driver_event_counts_as_work(evt: &DriverEvent) -> bool {
    !matches!(
        evt,
        DriverEvent::Ready { .. }
            | DriverEvent::DriverError { .. }
            | DriverEvent::Heartbeat { .. }
            | DriverEvent::PaneActivity { .. }
    )
}

/// Whether an event advances the stall clock (`last_progress_at`), i.e. whether
/// it is *evidence of work* rather than signal on the channel.
///
/// orgasmic:TASK-VZMZE,TASK-JK66P — deliberately a different question from
/// [`driver_event_counts_as_work`], which answers "did the worker do anything
/// at all" for the early-exit classification. `PaneActivity` is false there and
/// true here, and that split is the point: a TUI painting its banner is not
/// proof the *worker* worked, but a pane that demonstrably wrote bytes is proof
/// the run is not frozen — and it is the ONLY stall input an rmux run has
/// (TASK-RWCRN; see the variant's doc comment in `orgasmic-core::session`,
/// which names this change as the one that must not drop it).
fn driver_event_advances_stall_clock(evt: &DriverEvent) -> bool {
    match evt {
        // Liveness, not work. TASK-VZMZE measured 118 of these at 30 s
        // intervals holding open a run with 0 tool calls, 0 worktree bytes and
        // 6.77 s of CPU in an hour.
        DriverEvent::Heartbeat { .. } => false,
        // The startup handshake. "Reached ready and then nothing" is the
        // wedge's exact shape, so ready must not buy a stall window.
        DriverEvent::Ready { .. } => false,
        // A transport reporting its own breakage is not the worker working.
        DriverEvent::DriverError { .. } => false,
        // Harness stderr is ambient on this fleet: VZMZE's wedged run emitted
        // 12 of these and nothing else, and the healthy run dispatched seven
        // minutes later on the same binary emitted the same messages. Counting
        // them would let a harness log its way to immortality.
        DriverEvent::TextChunk {
            stream: TextStream::Stderr,
            ..
        } => false,
        _ => true,
    }
}

fn remove_record_lease(inner: &mut Inner, rec: &RunRecord) {
    inner.remove_lease(&lease_key(
        rec.project_id.as_deref(),
        &rec.task_id,
        rec.kind,
    ));
}

/// Mark a run as having a release in progress, and arm its drain deadline.
///
/// orgasmic:TASK-HAREX — the single door for `explicit_release_in_progress`.
/// Every caller that sets it has already stopped, or is about to stop, the
/// driver; from that point the drain waiting on the driver's stream is waiting
/// on something the daemon has already told to go away, and a driver that
/// leaves a stray sender clone alive turns that wait into a permanent one.
/// Arming here rather than at each call site is what keeps the bound attached
/// to the state it protects.
fn begin_explicit_release(rec: &mut RunRecord) {
    rec.explicit_release_in_progress = true;
    rec.release_requested.notify_one();
}

/// The bounded `recv` a run's event drain uses instead of `events.recv()`
/// (TASK-HAREX).
///
/// Unbounded until [`begin_explicit_release`] fires, then bounded by
/// [`RELEASE_DRAIN_BUDGET`] measured from that moment. The asymmetry is the
/// whole point: silence from a *working* driver must never end a drain — a
/// dispatch worker can go quiet for twenty minutes while cargo builds — but
/// silence from a driver the daemon has already told to shut down is the one
/// case where waiting forever costs a run its tombstone.
struct DrainGate {
    run_id: String,
    release_requested: Arc<tokio::sync::Notify>,
    budget: Duration,
    deadline: Option<Instant>,
}

impl DrainGate {
    fn new(run_id: String, release_requested: Arc<tokio::sync::Notify>, budget: Duration) -> Self {
        Self {
            run_id,
            release_requested,
            budget,
            deadline: None,
        }
    }

    /// The next driver event, or `None` to end the drain.
    ///
    /// `None` means one of two things, and the caller treats them alike: the
    /// driver's stream ended (the normal boundary), or a release was requested
    /// a full budget ago and the stream still has not ended. Events that arrive
    /// after the second case are dropped — by then the daemon has spent the
    /// entire release budget waiting for a driver it already stopped, and a run
    /// with no release tombstone is a worse outcome than a session missing its
    /// last few events.
    async fn next(
        &mut self,
        events: &mut tokio::sync::mpsc::Receiver<DriverEvent>,
    ) -> Option<DriverEvent> {
        loop {
            match self.deadline {
                Some(deadline) => {
                    return match tokio::time::timeout_at(deadline, events.recv()).await {
                        Ok(evt) => evt,
                        Err(_) => {
                            warn!(
                                run_id = %self.run_id,
                                budget_ms = self.budget.as_millis() as u64,
                                "driver stream did not end within the release drain budget; \
                                 ending the drain so the release can write its tombstone"
                            );
                            None
                        }
                    }
                }
                None => {
                    tokio::select! {
                        biased;
                        evt = events.recv() => return evt,
                        () = self.release_requested.notified() => {
                            self.deadline = Some(Instant::now() + self.budget);
                        }
                    }
                }
            }
        }
    }
}

fn session_is_early_exit_no_work_record(rec: &RunRecord) -> bool {
    rec.driver_has_ready && !rec.driver_has_work && !rec.driver_has_terminal
}

fn early_exit_quiescence_ready(rec: &RunRecord) -> bool {
    // orgasmic:task_3TEDA — stream end alone is the release gate; PID exit is
    // classification-only and must not strand on watcher abort or reuse.
    rec.stream_ended
}

fn take_stream_end_release(inner: &mut Inner, run_id: &str) -> Option<RunRecord> {
    let rec = inner.runs.get(run_id)?;
    if rec.early_exit_release_taken {
        return None;
    }
    if rec.explicit_release_in_progress
        && !rec.terminal_event_shutdown_in_progress
        && !rec.pid_exit_shutdown_in_progress
    {
        return None;
    }
    // Receiver/channel closure is the sole normal release boundary for
    // PID-watched subprocess runs; the watcher only publishes observations.
    if rec.early_exit_watcher_pid.is_some() && !rec.stream_ended {
        return None;
    }
    if !early_exit_quiescence_ready(rec) {
        return None;
    }
    let mut rec = inner.runs.remove(run_id)?;
    rec.early_exit_release_taken = true;
    remove_record_lease(inner, &rec);
    Some(rec)
}

/// TUI transports emit a terminal driver event when their pane/process exits.
/// ACP / subprocess modes use their stream-end path for protocol termination.
fn terminal_event_releases_transport(transport: &str) -> bool {
    matches!(transport, "tmux" | "tmux-tui" | "rmux")
}

/// A terminal event that declares *failure* releases the transport on every
/// transport, not only the mux ones.
///
/// The asymmetry with success is deliberate. Leaving a subprocess transport to
/// shut itself down is a bet that the harness will exit once it has nothing
/// left to do. On the success path that bet is sound — exiting is part of
/// completing normally. On the failure path it is a bet that a harness which
/// just declared itself broken will nonetheless behave correctly, and that is
/// exactly the bet that lost on 2026-07-25 (TASK-TJKFC).
///
/// From that run's own session JSONL: `run_fail` at 07:38:45.175, then nothing
/// for **seventy minutes**. `claude -p --input-format stream-json` holds stdin
/// open and waits for another turn that is never coming, so the stream never
/// ended, the release never fired, the lease stayed held, and the dispatch
/// stayed open with a live pid that `dispatch-status` reported as `[pid-alive]`
/// — i.e. as work in progress. A fatal startup that leaves a live process is
/// worse than a crash, because it is indistinguishable from work.
///
/// Releasing here reaps the process group (`reap_process_group`: TERM, then
/// KILL), so the failure costs 0.4 s instead of running until an operator
/// notices. Artifact draining is unaffected — `artifactor_lifecycle_in_flight`
/// still defers the release through `pending_terminal_drain`.
fn terminal_failure_releases_any_transport(outcome: Option<ReleaseOutcome>) -> bool {
    matches!(outcome, Some(ReleaseOutcome::Failed))
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
        let envelopes =
            read_session_file(&path).map_err(|_| DurableScanError::UnreadableSessionFile)?;
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
                            return Err(DurableScanError::UnreadableSessionFile);
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
                    return Err(DurableScanError::UnreadableSessionFile);
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

/// The live worker a destructive `dispatch-close` must not clean up beneath
/// (TASK-1T3FZ), decided from supervisor state under the supervisor lock.
///
/// Two ways a run blocks the close:
/// - it occupies the worktree the close is about to remove, whatever id it
///   carries — this is the one that matters, because the hazard is a recovery
///   replacement whose origin→replacement association never reached the
///   ledger, so the close's record does not name it; or
/// - it is one of the run ids this generation has owned.
///
/// Plus the undetermined case: an acquire owns this task's lease but has not
/// installed its `RunRecord` yet. Liveness cannot be proven, so it refuses —
/// daemon reachability is not evidence that an unidentified worker is absent.
///
/// The run the close is about to release is excluded: tearing down its own
/// generation is exactly what the close is entitled to do. Babysitters are
/// excluded too — one may legitimately still be attached, and it is not the
/// worker whose output cleanup would destroy.
fn blocking_run_for_close(
    inner: &Inner,
    params: &DispatchCloseGuardParams,
    worktree_key: &Path,
) -> Option<(String, Option<PathBuf>)> {
    let active_lease = lease_key(Some(&params.project_id), &params.task_id, params.kind);
    if let Some(run_id) = inner.lease(&active_lease) {
        if !inner.runs.contains_key(run_id) {
            return Some((run_id.clone(), None));
        }
    }
    inner.runs.iter().find_map(|(run_id, rec)| {
        if rec.kind != RunKind::Worker {
            return None;
        }
        if Some(run_id.as_str()) == params.releasing_run_id.as_deref() {
            return None;
        }
        let occupies = rec
            .worktree
            .as_deref()
            .map(normalize_cleanup_worktree)
            .as_deref()
            == Some(worktree_key);
        let owned = params.owned_run_ids.iter().any(|owned| owned == run_id);
        (occupies || owned).then(|| (run_id.clone(), rec.worktree.clone()))
    })
}

/// Drop cleanup reservations whose out-of-process holder is gone (TASK-1T3FZ,
/// reclamation reworked by TASK-AK6EM).
///
/// A `dispatch-close` guard is held by the CLI across its own filesystem
/// cleanup, so a CLI that dies mid-cleanup would otherwise reserve that
/// worktree until the daemon restarts — and the worktree path for a task is
/// deterministic, so the task would become undispatchable. Reclamation is
/// [`CloseGuardHolder::is_abandoned`]: a dead pid where a pid can be probed, an
/// expired holder lease everywhere (which is what makes a Windows holder
/// reclaimable, and what makes a missing `owner_pid` reclaimable at all).
/// Daemon-owned reservations carry no holder and are never swept.
fn drop_abandoned_cleanup_reservations(inner: &mut Inner) {
    let now = Utc::now();
    let mut reclaimed: Vec<String> = Vec::new();
    inner.cleanup_reservations.retain(|_, reservation| {
        let Some(holder) = reservation.holder.as_ref() else {
            return true;
        };
        if holder.is_abandoned(now) {
            reclaimed.push(holder.close_guard_id.clone());
            return false;
        }
        true
    });
    for guard_id in reclaimed {
        tracing::warn!(
            guard_id = %guard_id,
            "reclaiming a dispatch cleanup reservation whose holder is gone"
        );
        inner.close_guards.remove(&guard_id);
    }
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
        remove_record_lease(inner, &rec);
        return DeferredArtifactorRelease::Cancel(rec);
    }
    if rec.pending_terminal_drain {
        let Some(rec) = inner.runs.remove(run_id) else {
            return DeferredArtifactorRelease::None;
        };
        remove_record_lease(inner, &rec);
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

fn take_driver_terminal_release(
    inner: &mut Inner,
    run_id: &str,
    force_transport_shutdown: bool,
) -> Option<TerminalRelease> {
    let should_release = inner
        .runs
        .get(run_id)
        .map(|rec| {
            force_transport_shutdown
                || terminal_event_releases_transport(&rec.transport)
                || terminal_failure_releases_any_transport(rec.terminal_outcome)
        })
        .unwrap_or(false);
    if !should_release {
        return None;
    }
    inner
        .runs
        .get(run_id)
        .and_then(|rec| rec.terminal_outcome)?;
    if inner
        .runs
        .get(run_id)
        .is_some_and(|rec| rec.explicit_release_in_progress || rec.early_exit_release_taken)
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

    let rec = inner.runs.get_mut(run_id)?;
    begin_explicit_release(rec);
    rec.terminal_event_shutdown_in_progress = true;
    Some(TerminalRelease {
        run_id: rec.identity.run_id.clone(),
        transport: rec.transport.clone(),
        control: std::mem::replace(&mut rec.control, Box::new(DetachedDriverControl)),
        producer: rec.producer.take(),
    })
}

async fn finish_stream_end_terminal_drain(
    writer: &WriterHandle,
    inner: &tokio::sync::Mutex<Inner>,
    run_id: &str,
) {
    let release = {
        let mut g = inner.lock().await;
        let Some(rec) = g.runs.get_mut(run_id) else {
            return;
        };
        rec.stream_ended = true;
        if artifactor_lifecycle_in_flight(rec) {
            rec.pending_terminal_drain = true;
            return;
        }
        take_stream_end_release(&mut g, run_id)
    };
    if let Some(rec) = release {
        let source = if rec.terminal_event_shutdown_in_progress {
            TerminalReleaseSource::TerminalEvent
        } else {
            TerminalReleaseSource::StreamEnd
        };
        write_terminal_release_from_record(writer, rec, source).await;
    }
}

async fn append_terminal_release(
    writer: &WriterHandle,
    mut rec: RunRecord,
    resolved: ResolvedTerminalRelease,
) {
    if let Some(watcher) = rec.early_exit_watcher.take() {
        watcher.abort();
        let _ = watcher.await;
    }
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
            authority: None,
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
    let resolved =
        if session_is_early_exit_no_work_record(&rec) && rec.early_exit_watcher_pid.is_some() {
            ResolvedTerminalRelease {
                reason: "early-exit subprocess with no work envelopes".into(),
                outcome: ReleaseOutcome::Failed,
                finalized_by_worker: false,
            }
        } else {
            resolve_terminal_release(&rec, source)
        };
    append_terminal_release(writer, rec, resolved).await;
}

async fn finish_driver_terminal_release(
    _writer: &WriterHandle,
    release: TerminalRelease,
    driver_release_timeout: Duration,
) {
    stop_and_join_driver_producer(
        &release.run_id,
        &release.transport,
        release.control,
        release.producer,
        "driver terminal event",
        driver_release_timeout,
    )
    .await;
    // The receiver owns the terminal boundary. Dropping the producer closes
    // the channel; the drain then persists every queued event before it
    // removes the record and writes Lifecycle::Release.
}

async fn stop_and_join_driver_producer(
    run_id: &str,
    transport: &str,
    mut control: Box<dyn DriverControl>,
    producer: Option<tokio::task::JoinHandle<()>>,
    reason: &str,
    budget: Duration,
) {
    match tokio::time::timeout(budget, control.release(reason)).await {
        Ok(Ok(())) => {}
        Ok(Err(release_error)) => {
            error!(
                run_id,
                transport,
                reason,
                error = %release_error,
                "driver release failed; continuing unconditional producer and lifecycle cleanup"
            );
        }
        Err(_) => {
            error!(
                run_id,
                transport,
                reason,
                timeout = ?budget,
                "driver release timed out; continuing unconditional producer and lifecycle cleanup"
            );
        }
    }
    drop(control);

    if let Some(mut producer) = producer {
        if tokio::time::timeout(budget, &mut producer).await.is_err() {
            producer.abort();
            let _ = producer.await;
        }
    }
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
    // Heartbeats, pane-activity liveness signals and turn-boundary protocol
    // signals carry no substantive content; they must not inflate the summary
    // window/count fed to the babysitter.
    if matches!(
        evt,
        DriverEvent::Heartbeat { .. }
            | DriverEvent::AgentTurnComplete { .. }
            | DriverEvent::PaneActivity { .. }
    ) {
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
        DriverEvent::Heartbeat { .. }
        | DriverEvent::AgentTurnComplete { .. }
        | DriverEvent::PaneActivity { .. } => {}
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
    use crate::driver_resolution::{stub_config, stub_driver, STUB_HARNESS, STUB_MODE};
    use crate::events::EventBus;
    use crate::writer::spawn as spawn_writer;
    use orgasmic_core::session::TextStream;
    use orgasmic_core::{read_session_file, SessionEnvelope};
    use orgasmic_drivers::{
        modes::{
            rmux::{
                probe_rmux_binary, rmux_session_name,
                test_tooling::{
                    assert_not_degraded, assert_required_test_tooling, own_rmux_server_for_tests,
                    skip_test_if_missing, test_environment_lock, StallableRmuxEndpoint,
                    ToolRequirement,
                },
            },
            tmux,
        },
        BabysitterAck, BabysitterRequest, DriverError, DriverSession, RmuxDriver, ShellAdapter,
        TmuxTuiDriver, TransitionAck, UserInputAck,
    };
    use serde_json::json;

    fn session_has_work_envelope(envelopes: &[SessionEnvelope]) -> bool {
        envelopes.iter().any(|envelope| {
            if envelope.kind != SessionEventKind::DriverEvent {
                return false;
            }
            match envelope.event.get("type").and_then(|value| value.as_str()) {
                Some("ready") => false,
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

    use orgasmic_drivers::modes::rmux::test_tooling::live_session_guard;

    /// Pane-transport double for the TASK-RWCRN stall tests: it emits `Ready`
    /// at acquire and then, like a real rmux pane, nothing at all unless the
    /// test injects it. The event sender lives behind a shared handle so the
    /// channel stays open (a closed channel is stream-end, which would release
    /// the run before any stall sweep ran) and `release` closes it.
    struct RmuxPaneDriver {
        event_tx: Arc<Mutex<Option<tokio::sync::mpsc::Sender<DriverEvent>>>>,
    }

    impl RmuxPaneDriver {
        fn new() -> Self {
            Self {
                event_tx: Arc::new(Mutex::new(None)),
            }
        }

        async fn inject(&self, evt: DriverEvent) {
            if let Some(tx) = self.event_tx.lock().await.as_ref() {
                tx.send(evt).await.expect("pane event channel is open");
            }
        }
    }

    #[async_trait::async_trait]
    impl WorkerDriver for RmuxPaneDriver {
        fn transport(&self) -> &'static str {
            "rmux"
        }

        fn harness(&self) -> Option<&'static str> {
            Some("claude")
        }

        async fn acquire(
            &self,
            ctx: DriverContext,
            _config: DriverConfig,
        ) -> Result<DriverSession, orgasmic_drivers::DriverError> {
            let (tx, rx) = tokio::sync::mpsc::channel(8);
            *self.event_tx.lock().await = Some(tx.clone());
            let _ = tx
                .send(DriverEvent::Ready {
                    protocol_version: "rmux/1".into(),
                    capabilities: json!({"tui": true}),
                })
                .await;
            Ok(DriverSession {
                identity: ctx.identity,
                pid: None,
                events: rx,
                control: Box::new(RmuxPaneControl {
                    event_tx: Arc::clone(&self.event_tx),
                }),
                producer: None,
                native_runtime: None,
            })
        }
    }

    /// TASK-VZMZE's measured shape: an acp-stdio harness that reaches `ready`,
    /// never begins a turn, and emits a heartbeat every interval forever.
    struct HeartbeatOnlyAcpDriver {
        event_tx: Arc<Mutex<Option<tokio::sync::mpsc::Sender<DriverEvent>>>>,
    }

    impl HeartbeatOnlyAcpDriver {
        fn new() -> Self {
            Self {
                event_tx: Arc::new(Mutex::new(None)),
            }
        }

        async fn inject(&self, evt: DriverEvent) {
            if let Some(tx) = self.event_tx.lock().await.as_ref() {
                tx.send(evt).await.expect("heartbeat channel is open");
            }
        }
    }

    #[async_trait::async_trait]
    impl WorkerDriver for HeartbeatOnlyAcpDriver {
        fn transport(&self) -> &'static str {
            "acp-stdio"
        }

        fn harness(&self) -> Option<&'static str> {
            Some("codex")
        }

        async fn acquire(
            &self,
            ctx: DriverContext,
            _config: DriverConfig,
        ) -> Result<DriverSession, orgasmic_drivers::DriverError> {
            let (tx, rx) = tokio::sync::mpsc::channel(16);
            *self.event_tx.lock().await = Some(tx.clone());
            let _ = tx
                .send(DriverEvent::Ready {
                    protocol_version: "acp/1".into(),
                    capabilities: json!({}),
                })
                .await;
            Ok(DriverSession {
                identity: ctx.identity,
                pid: None,
                events: rx,
                control: Box::new(RmuxPaneControl {
                    event_tx: Arc::clone(&self.event_tx),
                }),
                producer: None,
                native_runtime: None,
            })
        }
    }

    struct RmuxPaneControl {
        event_tx: Arc<Mutex<Option<tokio::sync::mpsc::Sender<DriverEvent>>>>,
    }

    #[async_trait::async_trait]
    impl DriverControl for RmuxPaneControl {
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
            let _ = self.event_tx.lock().await.take();
            Ok(())
        }
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
                producer: None,
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
                producer: None,
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
                producer: None,
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
                producer: None,
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

    /// tmux-like driver with no PID: emits Ready then closes the stream with
    /// no work envelopes — stream-end must release exactly once (TASK-QPKCD).
    struct NoPidReadyOnlyDriver;

    #[async_trait::async_trait]
    impl WorkerDriver for NoPidReadyOnlyDriver {
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
            });
            Ok(DriverSession {
                identity: ctx.identity,
                pid: None,
                events: rx,
                control: Box::new(ProtocolEndAcpControl),
                producer: None,
                native_runtime: None,
            })
        }
    }

    /// No-PID driver whose only post-Ready event is stderr-mode TextChunk work.
    struct NoPidStderrWorkDriver;

    #[async_trait::async_trait]
    impl WorkerDriver for NoPidStderrWorkDriver {
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
                    .send(DriverEvent::TextChunk {
                        stream: TextStream::Stderr,
                        chunk: "warning: shim output".into(),
                        seq: 1,
                    })
                    .await;
            });
            Ok(DriverSession {
                identity: ctx.identity,
                pid: None,
                events: rx,
                control: Box::new(ProtocolEndAcpControl),
                producer: None,
                native_runtime: None,
            })
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
                producer: None,
                native_runtime: None,
            })
        }
    }

    struct FinalizeThenProtocolEndControl {
        events: Option<tokio::sync::mpsc::Sender<DriverEvent>>,
    }

    /// Holds the driver stream open and exposes the sender for deterministic
    /// queued-event injection (orgasmic:task_3TEDA).
    struct QueuedBeforeTimeoutDriver {
        event_tx:
            std::sync::Arc<tokio::sync::Mutex<Option<tokio::sync::mpsc::Sender<DriverEvent>>>>,
        on_release: Option<DriverEvent>,
    }

    impl QueuedBeforeTimeoutDriver {
        fn new() -> Self {
            Self {
                event_tx: std::sync::Arc::new(tokio::sync::Mutex::new(None)),
                on_release: None,
            }
        }

        fn with_release_event(event: DriverEvent) -> Self {
            Self {
                event_tx: std::sync::Arc::new(tokio::sync::Mutex::new(None)),
                on_release: Some(event),
            }
        }

        async fn inject(&self, evt: DriverEvent) {
            if let Some(tx) = self.event_tx.lock().await.as_ref() {
                let _ = tx.send(evt).await;
            }
        }

        async fn close_events(&self) {
            let _ = self.event_tx.lock().await.take();
        }
    }

    #[async_trait::async_trait]
    impl WorkerDriver for QueuedBeforeTimeoutDriver {
        fn transport(&self) -> &'static str {
            "acp-stdio"
        }

        async fn acquire(
            &self,
            ctx: DriverContext,
            _config: DriverConfig,
        ) -> Result<DriverSession, orgasmic_drivers::DriverError> {
            let (tx, rx) = tokio::sync::mpsc::channel(8);
            *self.event_tx.lock().await = Some(tx.clone());
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
                control: Box::new(QueuedBeforeTimeoutControl {
                    event_tx: std::sync::Arc::clone(&self.event_tx),
                    on_release: self.on_release.clone(),
                }),
                producer: None,
                native_runtime: None,
            })
        }
    }

    struct QueuedBeforeTimeoutControl {
        event_tx:
            std::sync::Arc<tokio::sync::Mutex<Option<tokio::sync::mpsc::Sender<DriverEvent>>>>,
        on_release: Option<DriverEvent>,
    }

    struct HungProducerDriver {
        dead_pid: bool,
        producer_dropped: Arc<std::sync::atomic::AtomicBool>,
    }

    impl HungProducerDriver {
        fn new(dead_pid: bool) -> Self {
            Self {
                dead_pid,
                producer_dropped: Arc::new(std::sync::atomic::AtomicBool::new(false)),
            }
        }
    }

    struct ProducerDropProbe(Arc<std::sync::atomic::AtomicBool>);

    impl Drop for ProducerDropProbe {
        fn drop(&mut self) {
            self.0.store(true, Ordering::SeqCst);
        }
    }

    /// orgasmic:TASK-J1XCB — the "racing terminal" both `HungProducerDriver`
    /// tests are named after, made an ordering instead of a coincidence.
    ///
    /// The event used to be sent by the producer after a 6s sleep, which landed
    /// inside the join window only because `DRIVER_RELEASE_TIMEOUT` happened to
    /// be 5s: the window is `[budget, 2 * budget]` and 6s sat in it. That made
    /// the asserted release — `driver stream closed` / `completed` — a function
    /// of two timers on a machine under load, and it is why compressing the
    /// budget alone turned the release into `early-exit subprocess with no work
    /// envelopes`. Sending it from `release` puts it exactly where the tests say
    /// it is, at the moment the forced stop begins, for any budget.
    ///
    /// The sender is taken and dropped here rather than cloned and kept: a
    /// clone that outlives this call is the TASK-HAREX stray-sender wedge, and
    /// these tests need the producer abort to really close the channel.
    struct HungReleaseControl {
        events: Option<tokio::sync::mpsc::Sender<DriverEvent>>,
    }

    struct FailingRmuxReapDriver {
        producer_dropped: Arc<std::sync::atomic::AtomicBool>,
    }

    struct FailingRmuxReapControl {
        release_producer: Arc<tokio::sync::Notify>,
        events: Option<tokio::sync::mpsc::Sender<DriverEvent>>,
    }

    #[derive(Clone)]
    struct CapturedLog(Arc<std::sync::Mutex<Vec<u8>>>);

    struct CapturedLogWriter(Arc<std::sync::Mutex<Vec<u8>>>);

    impl std::io::Write for CapturedLogWriter {
        fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(bytes);
            Ok(bytes.len())
        }

        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for CapturedLog {
        type Writer = CapturedLogWriter;

        fn make_writer(&'a self) -> Self::Writer {
            CapturedLogWriter(Arc::clone(&self.0))
        }
    }

    #[async_trait::async_trait]
    impl WorkerDriver for FailingRmuxReapDriver {
        fn transport(&self) -> &'static str {
            "rmux"
        }

        async fn acquire(
            &self,
            ctx: DriverContext,
            _config: DriverConfig,
        ) -> Result<DriverSession, DriverError> {
            let (tx, rx) = tokio::sync::mpsc::channel(8);
            tx.send(DriverEvent::Ready {
                protocol_version: "failing-rmux-reap/1".into(),
                capabilities: json!({"test": true}),
            })
            .await
            .unwrap();
            let release_producer = Arc::new(tokio::sync::Notify::new());
            let producer_release = Arc::clone(&release_producer);
            let producer_dropped = Arc::clone(&self.producer_dropped);
            let producer = tokio::spawn(async move {
                let _drop_probe = ProducerDropProbe(producer_dropped);
                producer_release.notified().await;
            });
            Ok(DriverSession {
                identity: ctx.identity,
                pid: None,
                events: rx,
                control: Box::new(FailingRmuxReapControl {
                    release_producer,
                    events: Some(tx),
                }),
                producer: Some(producer),
                native_runtime: None,
            })
        }
    }

    #[async_trait::async_trait]
    impl DriverControl for FailingRmuxReapControl {
        async fn transition_state(
            &mut self,
            _req: TransitionRequest,
        ) -> Result<TransitionAck, DriverError> {
            Err(DriverError::Unsupported("transition_state"))
        }

        async fn babysitter_action(
            &mut self,
            _req: BabysitterRequest,
        ) -> Result<BabysitterAck, DriverError> {
            Err(DriverError::Unsupported("babysitter_action"))
        }

        async fn release(&mut self, reason: &str) -> Result<(), DriverError> {
            let _ = self
                .events
                .as_ref()
                .unwrap()
                .send(DriverEvent::RunComplete {
                    summary: Some(reason.to_string()),
                })
                .await;
            self.release_producer.notify_one();
            Err(DriverError::Transport(
                "SDK stalled and exact-endpoint CLI fallback refused".into(),
            ))
        }
    }

    #[async_trait::async_trait]
    impl DriverControl for HungReleaseControl {
        async fn transition_state(
            &mut self,
            _req: TransitionRequest,
        ) -> Result<TransitionAck, DriverError> {
            Err(DriverError::Unsupported("transition_state"))
        }

        async fn babysitter_action(
            &mut self,
            _req: BabysitterRequest,
        ) -> Result<BabysitterAck, DriverError> {
            Err(DriverError::Unsupported("babysitter_action"))
        }

        async fn release(&mut self, _reason: &str) -> Result<(), DriverError> {
            if let Some(events) = self.events.take() {
                let _ = events
                    .send(DriverEvent::RunComplete {
                        summary: Some("terminal event raced forced producer stop".into()),
                    })
                    .await;
            }
            std::future::pending().await
        }
    }

    #[async_trait::async_trait]
    impl WorkerDriver for HungProducerDriver {
        fn transport(&self) -> &'static str {
            "acp-stdio"
        }

        async fn acquire(
            &self,
            ctx: DriverContext,
            _config: DriverConfig,
        ) -> Result<DriverSession, DriverError> {
            let pid = self.dead_pid.then(|| {
                let mut child = Command::new("sh")
                    .args(["-c", "exit 0"])
                    .stdin(Stdio::null())
                    .stdout(Stdio::null())
                    .stderr(Stdio::null())
                    .spawn()
                    .expect("spawn short-lived child");
                let pid = child.id();
                child.wait().expect("reap short-lived child");
                pid
            });
            let (tx, rx) = tokio::sync::mpsc::channel(8);
            tx.send(DriverEvent::Ready {
                protocol_version: "hung-producer/1".into(),
                capabilities: json!({"test": true}),
            })
            .await
            .unwrap();
            let dropped = Arc::clone(&self.producer_dropped);
            let terminal_tx = tx.clone();
            let producer = tokio::spawn(async move {
                let _drop_probe = ProducerDropProbe(dropped);
                // Hung, and nothing else. The terminal event that races the
                // forced stop is emitted by the control below, at the instant
                // the stop begins — see `HungReleaseControl`.
                let _tx = tx;
                std::future::pending::<()>().await;
            });
            Ok(DriverSession {
                identity: ctx.identity,
                pid,
                events: rx,
                control: Box::new(HungReleaseControl {
                    events: Some(terminal_tx),
                }),
                producer: Some(producer),
                native_runtime: None,
            })
        }
    }

    /// A driver that keeps one clone of its event sender alive in a task the
    /// supervisor does not own (TASK-HAREX).
    ///
    /// Not an invented shape. `stop_and_join_driver_producer` stops the driver
    /// control, then joins — and if that does not finish, aborts — the single
    /// `producer` handle the driver returned. A real transport whose internal
    /// reader, notification or pty task also holds a sender survives all of
    /// that, so `events.recv()` never yields `None`: the run's drain parks
    /// forever and the release waiting behind it never removes the record or
    /// writes `Lifecycle::Release`.
    ///
    /// Everything else here is deliberately healthy — the control releases
    /// promptly, the producer is joinable — so a test that fails against this
    /// driver is failing on the drain and nothing else.
    struct StraySenderDriver {
        dead_pid: bool,
        producer_dropped: Arc<std::sync::atomic::AtomicBool>,
    }

    impl StraySenderDriver {
        fn new(dead_pid: bool) -> Self {
            Self {
                dead_pid,
                producer_dropped: Arc::new(std::sync::atomic::AtomicBool::new(false)),
            }
        }
    }

    #[async_trait::async_trait]
    impl WorkerDriver for StraySenderDriver {
        fn transport(&self) -> &'static str {
            // The transport of the measured incident, and one that
            // `terminal_event_releases_transport` does not allow-list.
            "acp-stdio"
        }

        async fn acquire(
            &self,
            ctx: DriverContext,
            _config: DriverConfig,
        ) -> Result<DriverSession, DriverError> {
            let pid = self.dead_pid.then(|| {
                let mut child = Command::new("sh")
                    .args(["-c", "exit 0"])
                    .stdin(Stdio::null())
                    .stdout(Stdio::null())
                    .stderr(Stdio::null())
                    .spawn()
                    .expect("spawn short-lived child");
                let pid = child.id();
                child.wait().expect("reap short-lived child");
                pid
            });
            let (tx, rx) = tokio::sync::mpsc::channel(8);
            tx.send(DriverEvent::Ready {
                protocol_version: "stray-sender/1".into(),
                capabilities: json!({"test": true}),
            })
            .await
            .unwrap();
            // A work envelope, so this models a worker that did its job and
            // then vanished rather than one that never started. The two are
            // classified differently on release — `early-exit subprocess with
            // no work envelopes` vs `protocol_end_without_finalize` — and the
            // incident is the second.
            tx.send(DriverEvent::TextChunk {
                stream: TextStream::Assistant,
                chunk: "wrote the report".into(),
                seq: 1,
            })
            .await
            .unwrap();
            // The clone no release path can reach.
            let stray = tx.clone();
            tokio::spawn(async move {
                let _stray = stray;
                std::future::pending::<()>().await;
            });
            let dropped = Arc::clone(&self.producer_dropped);
            let producer = tokio::spawn(async move {
                let _drop_probe = ProducerDropProbe(dropped);
                let _tx = tx;
                std::future::pending::<()>().await;
            });
            Ok(DriverSession {
                identity: ctx.identity,
                pid,
                events: rx,
                control: Box::new(NoopControl),
                producer: Some(producer),
                native_runtime: None,
            })
        }
    }

    #[async_trait::async_trait]
    impl DriverControl for QueuedBeforeTimeoutControl {
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
            if let Some(event) = self.on_release.take() {
                if let Some(sender) = self.event_tx.lock().await.as_ref() {
                    let _ = sender.send(event).await;
                }
            }
            let _ = self.event_tx.lock().await.take();
            Ok(())
        }
    }

    /// PID-backed driver with a controllable event channel for quiescence tests.
    struct PidBackedControllableDriver {
        event_tx:
            std::sync::Arc<tokio::sync::Mutex<Option<tokio::sync::mpsc::Sender<DriverEvent>>>>,
        reported_pid: Option<u32>,
        send_ready: bool,
    }

    impl PidBackedControllableDriver {
        fn new() -> Self {
            Self {
                event_tx: std::sync::Arc::new(tokio::sync::Mutex::new(None)),
                reported_pid: None,
                send_ready: true,
            }
        }

        fn with_reported_pid(pid: u32) -> Self {
            Self {
                reported_pid: Some(pid),
                ..Self::new()
            }
        }

        fn without_ready() -> Self {
            Self {
                send_ready: false,
                ..Self::new()
            }
        }

        async fn close_events(&self) {
            let _ = self.event_tx.lock().await.take();
        }
    }

    #[async_trait::async_trait]
    impl WorkerDriver for PidBackedControllableDriver {
        fn transport(&self) -> &'static str {
            "acp-stdio"
        }

        async fn acquire(
            &self,
            ctx: DriverContext,
            _config: DriverConfig,
        ) -> Result<DriverSession, orgasmic_drivers::DriverError> {
            let mut command = tokio::process::Command::new("sleep");
            command
                .arg("300")
                .stdin(std::process::Stdio::null())
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .kill_on_drop(true);
            let child = command.spawn().map_err(DriverError::Io)?;
            let pid = child.id().expect("sleep child pid");
            let (tx, rx) = tokio::sync::mpsc::channel(8);
            *self.event_tx.lock().await = Some(tx.clone());
            if self.send_ready {
                let _ = tx
                    .send(DriverEvent::Ready {
                        protocol_version: "test-acp/1".into(),
                        capabilities: json!({"simulated": true}),
                    })
                    .await;
            }
            Ok(DriverSession {
                identity: ctx.identity,
                pid: Some(self.reported_pid.unwrap_or(pid)),
                events: rx,
                control: Box::new(PidBackedControllableControl {
                    event_tx: std::sync::Arc::clone(&self.event_tx),
                    child: Some(child),
                }),
                producer: None,
                native_runtime: None,
            })
        }
    }

    struct PidBackedControllableControl {
        event_tx:
            std::sync::Arc<tokio::sync::Mutex<Option<tokio::sync::mpsc::Sender<DriverEvent>>>>,
        child: Option<tokio::process::Child>,
    }

    #[async_trait::async_trait]
    impl DriverControl for PidBackedControllableControl {
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
            let _ = self.event_tx.lock().await.take();
            if let Some(mut child) = self.child.take() {
                let _ = child.kill().await;
                let _ = child.wait().await;
            }
            Ok(())
        }
    }

    /// Emits Ready then a fatal DriverError and closes the stream.
    /// The 2026-07-25 incident, reproduced from its own session JSONL: a
    /// harness that declares the run failed and then *keeps its stream open*.
    ///
    /// This is the shape `claude -p --input-format stream-json` actually has —
    /// it holds stdin waiting for another turn — and it is why the real run sat
    /// there for seventy minutes. Every pre-existing fatal-path test drops the
    /// sender as soon as it has emitted, so stream end rescues them and none of
    /// them can see this.
    ///
    /// The sender lives in the *control* rather than the emitting task, so the
    /// only thing that can end this stream is a release. That mirrors the real
    /// transport, where releasing reaps the process group and the harness's
    /// exit is what closes the channel.
    struct FatalThenSilentDriver;

    #[async_trait::async_trait]
    impl WorkerDriver for FatalThenSilentDriver {
        fn transport(&self) -> &'static str {
            // The incident's transport. A mux transport would already release.
            "acp-stdio"
        }

        async fn acquire(
            &self,
            ctx: DriverContext,
            _config: DriverConfig,
        ) -> Result<DriverSession, orgasmic_drivers::DriverError> {
            let (tx, rx) = tokio::sync::mpsc::channel(8);
            let held = tx.clone();
            tokio::spawn(async move {
                let _ = tx
                    .send(DriverEvent::Ready {
                        protocol_version: "claude-code-stream-json/1".into(),
                        capabilities: json!({"simulated": false}),
                    })
                    .await;
                // The harness's own words, before any work: an instruction to
                // start an interactive flow nobody is attached to answer.
                let _ = tx
                    .send(DriverEvent::TextChunk {
                        stream: orgasmic_core::TextStream::Assistant,
                        chunk: "Not logged in \u{b7} Please run /login".into(),
                        seq: 1,
                    })
                    .await;
                let _ = tx
                    .send(DriverEvent::DriverError {
                        message: "claude authentication_failed".into(),
                        fatal: true,
                    })
                    .await;
                let _ = tx
                    .send(DriverEvent::RunFail {
                        error_code: "claude_result_error".into(),
                        error_markdown: "Not logged in \u{b7} Please run /login".into(),
                    })
                    .await;
                // The emitting task ends, but `held` keeps the stream open —
                // the real harness did not exit either.
            });
            Ok(DriverSession {
                identity: ctx.identity,
                pid: None,
                events: rx,
                control: Box::new(FatalThenSilentControl { held: Some(held) }),
                producer: None,
                native_runtime: None,
            })
        }
    }

    /// Holds the only remaining sender; releasing drops it, which is how a real
    /// transport's reap ends the stream.
    struct FatalThenSilentControl {
        held: Option<tokio::sync::mpsc::Sender<DriverEvent>>,
    }

    #[async_trait::async_trait]
    impl DriverControl for FatalThenSilentControl {
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
            self.held = None;
            Ok(())
        }
    }

    /// The same failure as [`FatalThenSilentDriver`], but declared *before*
    /// `acquire` returns — so it is already queued when the supervisor does its
    /// bookkeeping. TASK-TJKFC's race case.
    ///
    /// A harness can fail this early for real: authentication is settled before
    /// any model call, and the incident's own JSONL puts the fatal 0.4 s after
    /// the spawn. There is no rule that a driver must finish handing back a
    /// session before its transport has anything to say.
    ///
    /// The events go into the channel synchronously here rather than from a
    /// spawned task, which is what makes the ordering deterministic instead of
    /// merely likely: the queue is guaranteed non-empty at the instant the
    /// supervisor inserts the run record. Capacity 8 holds all four without
    /// blocking.
    struct FatalBeforeBookkeepingDriver;

    #[async_trait::async_trait]
    impl WorkerDriver for FatalBeforeBookkeepingDriver {
        fn transport(&self) -> &'static str {
            "acp-stdio"
        }

        async fn acquire(
            &self,
            ctx: DriverContext,
            _config: DriverConfig,
        ) -> Result<DriverSession, orgasmic_drivers::DriverError> {
            let (tx, rx) = tokio::sync::mpsc::channel(8);
            tx.send(DriverEvent::Ready {
                protocol_version: "claude-code-stream-json/1".into(),
                capabilities: json!({"simulated": false}),
            })
            .await
            .unwrap();
            tx.send(DriverEvent::TextChunk {
                stream: orgasmic_core::TextStream::Assistant,
                chunk: "Not logged in \u{b7} Please run /login".into(),
                seq: 1,
            })
            .await
            .unwrap();
            tx.send(DriverEvent::DriverError {
                message: "claude authentication_failed".into(),
                fatal: true,
            })
            .await
            .unwrap();
            tx.send(DriverEvent::RunFail {
                error_code: "claude_result_error".into(),
                error_markdown: "Not logged in \u{b7} Please run /login".into(),
            })
            .await
            .unwrap();
            Ok(DriverSession {
                identity: ctx.identity,
                pid: None,
                events: rx,
                // Same as the sibling stub: only a release can end this stream,
                // so nothing but the terminal-event path can free the lease.
                control: Box::new(FatalThenSilentControl { held: Some(tx) }),
                producer: None,
                native_runtime: None,
            })
        }
    }

    struct FatalDriverErrorDriver;

    #[async_trait::async_trait]
    impl WorkerDriver for FatalDriverErrorDriver {
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
                    .send(DriverEvent::DriverError {
                        message: "simulated fatal".into(),
                        fatal: true,
                    })
                    .await;
            });
            Ok(DriverSession {
                identity: ctx.identity,
                pid: None,
                events: rx,
                control: Box::new(ProtocolEndAcpControl),
                producer: None,
                native_runtime: None,
            })
        }
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

    fn make_supervisor() -> (Supervisor, tempfile::TempDir, WriterHandle) {
        let (sup, dir, writer, _events) = make_supervisor_with_events();
        (sup, dir, writer)
    }

    /// The same supervisor, plus the bus its writer publishes on.
    ///
    /// A test that has to observe work the writer performs in the background
    /// wants `EventPayload::RunEvent`, which is published *after* the append
    /// lands on disk. That is a completion signal; a polling budget is a guess
    /// about how long the machine will take, and under load it guesses wrong.
    fn make_supervisor_with_events() -> (Supervisor, tempfile::TempDir, WriterHandle, EventBus) {
        let dir = tempfile::tempdir().unwrap();
        let events = EventBus::new();
        let writer = spawn_writer(events.clone());
        let boot = Arc::new(BootIdentity::new());
        let sup = Supervisor::new(writer.clone(), boot, CloseGuardStore::ephemeral());
        sup.set_work_probe(Arc::new(UnobservableWorkProbe));
        (sup, dir, writer, events)
    }

    /// The probe every test supervisor starts with: it establishes nothing, so
    /// the stall path behaves exactly as it did before TASK-JK66P unless a test
    /// installs a probe that can answer. Also keeps the unit suite hermetic —
    /// the production probe shells out to `rmux` and `ps`.
    struct UnobservableWorkProbe;

    impl WorkEvidenceProbe for UnobservableWorkProbe {
        fn observe(&self, _target: &WorkProbeTarget) -> WorkEvidence {
            WorkEvidence::Unknown
        }
    }

    /// A probe with a fixed answer — the two halves of TASK-JK66P's acceptance.
    struct FixedWorkProbe(WorkEvidence);

    impl WorkEvidenceProbe for FixedWorkProbe {
        fn observe(&self, _target: &WorkProbeTarget) -> WorkEvidence {
            self.0.clone()
        }
    }

    /// A supervisor whose only releaser is the test itself.
    ///
    /// Use this whenever a test ages a run past a timeout and then drives
    /// `release_first_timed_out_run*` by hand. The monitor `Supervisor::new`
    /// spawns ticks every 50ms against the same run, so with it running the
    /// test is asserting on whichever releaser happened to win.
    fn make_unmonitored_supervisor() -> (Supervisor, tempfile::TempDir, WriterHandle) {
        let dir = tempfile::tempdir().unwrap();
        let writer = spawn_writer(EventBus::new());
        let boot = Arc::new(BootIdentity::new());
        let sup = Supervisor::unmonitored(writer.clone(), boot, CloseGuardStore::ephemeral());
        sup.set_work_probe(Arc::new(UnobservableWorkProbe));
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

    #[test]
    fn pane_activity_is_non_terminal_and_is_not_evidence_of_work() {
        // TASK-RWCRN. The pane liveness signal must reset the stall clock (which
        // the drain does for every drained event) without releasing the lease
        // and without satisfying the early-exit "did any work" test: a TUI
        // paints its banner even when the harness immediately wedges, so pane
        // output alone must never make a no-work run look productive.
        let evt = DriverEvent::PaneActivity { seq: 0, bytes: 480 };
        assert!(terminal_outcome_for_event(&evt).is_none());
        assert!(!driver_event_counts_as_work(&evt));
        assert!(driver_event_counts_as_work(&DriverEvent::ToolCall {
            call_id: "c1".into(),
            name: "Edit".into(),
            args: json!({}),
            seq: 0,
        }));

        // And it must not inflate the babysitter's summary window, for the same
        // reason a heartbeat must not.
        let mut buf = BabysitterSummaryBuffer::default();
        let now = Instant::now();
        update_babysitter_buffer(&mut buf, &evt, 7, now);
        assert_eq!(buf.count, 0);
        assert_eq!(buf.window_end_seq, 0);
        assert!(buf.headline.is_empty());
    }

    #[test]
    fn agent_turn_complete_does_not_pollute_babysitter_summary_window() {
        let mut buf = BabysitterSummaryBuffer::default();
        let now = Instant::now();
        update_babysitter_buffer(&mut buf, &DriverEvent::AgentTurnComplete { seq: 0 }, 7, now);
        update_babysitter_buffer(&mut buf, &DriverEvent::AgentTurnComplete { seq: 1 }, 8, now);
        assert_eq!(buf.count, 0);
        assert_eq!(buf.window_end_seq, 0);
        assert!(buf.headline.is_empty());
    }

    /// Emits Ready, substantive driver events, and turn boundaries on acquire.
    struct SemanticTurnCountDriver;

    #[async_trait::async_trait]
    impl WorkerDriver for SemanticTurnCountDriver {
        fn transport(&self) -> &'static str {
            "acp-stdio"
        }

        async fn acquire(
            &self,
            ctx: DriverContext,
            _config: DriverConfig,
        ) -> Result<DriverSession, orgasmic_drivers::DriverError> {
            let (tx, rx) = tokio::sync::mpsc::channel(32);
            let emit_tx = tx.clone();
            tokio::spawn(async move {
                let _ = emit_tx
                    .send(DriverEvent::Ready {
                        protocol_version: "semantic-turn-test/1".into(),
                        capabilities: json!({"simulated": true}),
                    })
                    .await;
                for i in 0..5 {
                    let _ = emit_tx
                        .send(DriverEvent::TextChunk {
                            stream: orgasmic_core::TextStream::Assistant,
                            chunk: format!("chunk-{i}"),
                            seq: i,
                        })
                        .await;
                }
                let _ = emit_tx
                    .send(DriverEvent::ToolCall {
                        call_id: "call-1".into(),
                        name: "grep".into(),
                        args: json!({"pattern": "foo"}),
                        seq: 10,
                    })
                    .await;
                let _ = emit_tx
                    .send(DriverEvent::ToolResult {
                        call_id: "call-1".into(),
                        ok: true,
                        output: json!("ok"),
                        seq: 11,
                    })
                    .await;
                let _ = emit_tx
                    .send(DriverEvent::AgentTurnComplete { seq: 0 })
                    .await;
                let _ = emit_tx
                    .send(DriverEvent::AgentTurnComplete { seq: 1 })
                    .await;
            });
            Ok(DriverSession {
                identity: ctx.identity,
                pid: None,
                events: rx,
                control: Box::new(AcceptingInputControl { _events: tx }),
                native_runtime: None,
                producer: None,
            })
        }
    }

    /// A driver that reports a resolved credential mode, the way the claude
    /// adapter does once it has chosen a tier (TASK-S0QRM).
    struct CredentialModeDriver;

    #[async_trait::async_trait]
    impl WorkerDriver for CredentialModeDriver {
        fn transport(&self) -> &'static str {
            "acp-stdio"
        }

        fn harness(&self) -> Option<&'static str> {
            Some("claude")
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
                control: Box::new(AcceptingInputControl { _events: tx }),
                producer: None,
                native_runtime: Some(NativeRuntimeMeta {
                    provider: "claude".into(),
                    session_id: Some("pinned-session".into()),
                    session_path: None,
                    launch_argv: vec!["claude".into(), "--safe-mode".into()],
                    resume_argv: Vec::new(),
                    credential_mode: Some("native_login".into()),
                }),
            })
        }
    }

    /// The mode a run authenticated with must be readable from the session
    /// JSONL afterwards, not merely inferable from an argv recorded by a
    /// different lifecycle event.
    ///
    /// Read back through `Lifecycle`'s own deserializer rather than by string
    /// matching, so this also pins the wire name and the backward-compatible
    /// shape: the field is optional, and JSONL written before it existed still
    /// reconciles.
    #[tokio::test]
    async fn the_resolved_credential_mode_round_trips_through_run_meta() {
        let (sup, dir, _writer) = make_supervisor();
        let req = impl_req("TASK-CREDENTIAL-MODE", dir.path());
        let session_path = req.session_path.clone();
        let _resp = sup.acquire(&CredentialModeDriver, req).await.unwrap();

        let run_meta = session_events(&session_path)
            .into_iter()
            .filter(|envelope| envelope.kind == SessionEventKind::Lifecycle)
            .find_map(|envelope| match serde_json::from_value(envelope.event) {
                Ok(Lifecycle::RunMeta {
                    credential_mode, ..
                }) => Some(credential_mode),
                _ => None,
            })
            .expect("acquire must write a RunMeta event");
        assert_eq!(run_meta.as_deref(), Some("native_login"));

        // A mode string, never credential material: this file is committable
        // evidence.
        let raw = std::fs::read_to_string(&session_path).expect("session jsonl");
        assert!(!raw.contains("sk-ant"), "session JSONL must carry no key");

        // Pre-upgrade JSONL, and every harness that resolves no mode, stay
        // readable and simply report nothing.
        let legacy = json!({
            "phase": "run_meta",
            "transport": "acp-stdio",
            "driver_config": {},
        });
        let Ok(Lifecycle::RunMeta {
            credential_mode, ..
        }) = serde_json::from_value::<Lifecycle>(legacy)
        else {
            panic!("RunMeta written before this field existed must still parse");
        };
        assert_eq!(credential_mode, None);
    }

    fn test_babysitter_auto_spawn() -> BabysitterAutoSpawn {
        BabysitterAutoSpawn {
            worker_id: "babysitter-stall-detector".into(),
            mode: STUB_MODE.into(),
            harness: STUB_HARNESS.into(),
            driver_config: stub_config(),
            stall_timeout_secs: None,
            max_run_duration_secs: None,
            applicable_states: Vec::new(),
            linked_skills: Vec::new(),
            sandbox_permissions: None,
            max_iterations: None,
            context_budget_chars: None,
            harness_args: Vec::new(),
        }
    }

    // orgasmic:TASK-AK6EM
    // ---------------------------------------------------------------------
    // Live-run admission fencing, close-guard handoff, and holder reclamation.
    // ---------------------------------------------------------------------

    /// A driver that always proves a live runtime handle, so an *admitted*
    /// reattach really does install a live run. Nothing in these tests is
    /// refused by the driver: every refusal comes from the admission check.
    struct AlwaysAttachableDriver;

    #[async_trait::async_trait]
    impl WorkerDriver for AlwaysAttachableDriver {
        fn transport(&self) -> &'static str {
            "tmux"
        }

        async fn acquire(
            &self,
            ctx: DriverContext,
            _config: DriverConfig,
        ) -> Result<DriverSession, DriverError> {
            let (tx, rx) = tokio::sync::mpsc::channel(8);
            Ok(DriverSession {
                identity: ctx.identity,
                pid: None,
                events: rx,
                control: Box::new(AlwaysAttachableControl { _events: tx }),
                producer: None,
                native_runtime: None,
            })
        }

        async fn attach(
            &self,
            ctx: DriverContext,
            config: DriverConfig,
        ) -> Result<AttachOutcome, DriverError> {
            let session = self.acquire(ctx, config).await?;
            Ok(AttachOutcome::Attached(orgasmic_drivers::Attached {
                session: Box::new(session),
            }))
        }
    }

    struct AlwaysAttachableControl {
        _events: tokio::sync::mpsc::Sender<DriverEvent>,
    }

    #[async_trait::async_trait]
    impl DriverControl for AlwaysAttachableControl {
        async fn transition_state(
            &mut self,
            _req: TransitionRequest,
        ) -> Result<TransitionAck, DriverError> {
            Err(DriverError::Unsupported("transition_state"))
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

    fn close_guard_params(
        task_id: &str,
        worktree: &Path,
        owner_pid: Option<u32>,
    ) -> DispatchCloseGuardParams {
        DispatchCloseGuardParams {
            project_id: "orgasmic".into(),
            task_id: task_id.into(),
            kind: RunKind::Worker,
            branch: "task-fence-impl".into(),
            worktree_path: worktree.to_path_buf(),
            dispatch_attempt_token: None,
            last_path: None,
            stdout_path: None,
            owner_pid,
            releasing_run_id: None,
            owned_run_ids: Vec::new(),
        }
    }

    fn reserved_guard_id(outcome: DispatchCloseGuardOutcome) -> String {
        match outcome {
            DispatchCloseGuardOutcome::Reserved { guard_id, .. } => guard_id,
            other => panic!("expected a reserved close guard, got {other:?}"),
        }
    }

    #[allow(clippy::too_many_arguments)]
    async fn reattach_into(
        sup: &Supervisor,
        run_id: &str,
        task_id: &str,
        worktree: &Path,
        session_path: &Path,
    ) -> Result<AcquireResponse, SupervisorError> {
        sup.reattach(
            &AlwaysAttachableDriver,
            RuntimeIdentity::new(run_id, "boot-previous"),
            RunKind::Worker,
            task_id.to_string(),
            "implementer-claude-acp".to_string(),
            "implementer".to_string(),
            true,
            Some("orgasmic".to_string()),
            Some(worktree.to_path_buf()),
            session_path.to_path_buf(),
            tmux::inert_config(),
            false,
            None,
        )
        .await
    }

    fn worktree_req(task_id: &str, dir: &Path, worktree: &Path) -> AcquireRequest {
        let mut req = impl_req(task_id, dir);
        req.worktree = Some(worktree.to_path_buf());
        req
    }

    /// The P0 this task exists for: `reattach` is a live-run admission path and
    /// must observe the same worktree reservation `acquire` does.
    ///
    /// Injection: delete the `worktree` field from the `LiveRunAdmission` the
    /// reattach path builds (or the whole reservation block in
    /// `Inner::admit_live_run`) and this reattach is admitted — a live run
    /// installed into a worktree a destructive close is holding.
    #[tokio::test]
    async fn a_held_close_guard_fences_reattach_out_of_the_reserved_worktree() {
        let (sup, dir, _w) = make_supervisor();
        let worktree = dir.path().join("worktrees/task-fence");
        std::fs::create_dir_all(&worktree).unwrap();

        let guard_id = reserved_guard_id(
            sup.reserve_dispatch_close(&close_guard_params(
                "TASK-FENCE",
                &worktree,
                Some(std::process::id()),
            ))
            .await,
        );

        let error = reattach_into(
            &sup,
            "run-reattach-fenced",
            "TASK-FENCE",
            &worktree,
            &dir.path().join("run-reattach-fenced.jsonl"),
        )
        .await
        .expect_err("a reattach into a reserved worktree must be refused");
        assert!(
            matches!(error, SupervisorError::CleanupInProgress { .. }),
            "the refusal must name the cleanup reservation, got {error:?}"
        );
        assert!(
            sup.snapshot().await.runs.is_empty(),
            "a refused reattach must leave no live run behind"
        );

        // And it is the guard doing it: release it and the identical reattach
        // is admitted.
        sup.finish_dispatch_close(&guard_id).await;
        let admitted = reattach_into(
            &sup,
            "run-reattach-fenced",
            "TASK-FENCE",
            &worktree,
            &dir.path().join("run-reattach-fenced.jsonl"),
        )
        .await
        .expect("the same reattach is admitted once the guard is released");
        assert_eq!(admitted.run_id, "run-reattach-fenced");
    }

    /// The ATAXN handoff P0: the guard is held by the CLI, across a daemon
    /// replacement. A replacement that starts with an empty reservation map
    /// admits work into a worktree the original CLI is still deleting.
    ///
    /// Injection: drop the `close_guards.write(..)` call in
    /// `reserve_dispatch_close`, or make `Inner::new` ignore
    /// `CloseGuardStore::restore`, and the replacement admits both the acquire
    /// and the reattach below.
    #[tokio::test]
    async fn a_close_guard_survives_daemon_replacement_until_its_holder_finishes() {
        let dir = tempfile::tempdir().unwrap();
        let store = dir.path().join("state/close-guards");
        let writer = spawn_writer(EventBus::new());
        let worktree = dir.path().join("worktrees/task-handoff");
        std::fs::create_dir_all(&worktree).unwrap();

        let predecessor = Supervisor::unmonitored(
            writer.clone(),
            Arc::new(BootIdentity::new()),
            CloseGuardStore::at(&store),
        );
        let guard_id = reserved_guard_id(
            predecessor
                .reserve_dispatch_close(&close_guard_params(
                    "TASK-HANDOFF",
                    &worktree,
                    Some(std::process::id()),
                ))
                .await,
        );
        // The whole daemon goes away while the CLI holder is still working.
        drop(predecessor);

        let replacement = Supervisor::unmonitored(
            writer.clone(),
            Arc::new(BootIdentity::new()),
            CloseGuardStore::at(&store),
        );
        let acquire_error = replacement
            .acquire(
                &AlwaysAttachableDriver,
                worktree_req("TASK-HANDOFF", dir.path(), &worktree),
            )
            .await
            .expect_err("the replacement must inherit the in-flight close guard");
        assert!(
            matches!(acquire_error, SupervisorError::CleanupInProgress { .. }),
            "got {acquire_error:?}"
        );
        let reattach_error = reattach_into(
            &replacement,
            "run-handoff-replacement",
            "TASK-HANDOFF",
            &worktree,
            &dir.path().join("run-handoff-replacement.jsonl"),
        )
        .await
        .expect_err("recovery must stay refused across the replacement too");
        assert!(
            matches!(reattach_error, SupervisorError::CleanupInProgress { .. }),
            "got {reattach_error:?}"
        );

        // The holder finishes against the replacement, using the guard id the
        // predecessor minted.
        replacement.finish_dispatch_close(&guard_id).await;
        replacement
            .acquire(
                &AlwaysAttachableDriver,
                worktree_req("TASK-HANDOFF", dir.path(), &worktree),
            )
            .await
            .expect("the worktree is acquirable once the holder is done");
        assert!(
            !store.join(&guard_id).exists(),
            "a finished guard must not be left on disk for the next daemon"
        );
    }

    /// A holder that died is reclaimed, so a task whose worktree path is
    /// deterministic does not become permanently undispatchable.
    #[cfg(unix)]
    #[tokio::test]
    async fn a_dead_pid_holder_is_reclaimed_on_the_next_admission() {
        let (sup, dir, _w) = make_supervisor();
        let worktree = dir.path().join("worktrees/task-dead-holder");
        std::fs::create_dir_all(&worktree).unwrap();

        let mut child = std::process::Command::new("true")
            .spawn()
            .expect("spawn a throwaway holder");
        let dead_pid = child.id();
        child.wait().expect("reap the throwaway holder");

        let _guard_id = reserved_guard_id(
            sup.reserve_dispatch_close(&close_guard_params(
                "TASK-DEAD-HOLDER",
                &worktree,
                Some(dead_pid),
            ))
            .await,
        );
        sup.acquire(
            &AlwaysAttachableDriver,
            worktree_req("TASK-DEAD-HOLDER", dir.path(), &worktree),
        )
        .await
        .expect("a guard whose pid is gone must not fence anyone out");
    }

    /// A live pid holds its guard however long it works, and specifically
    /// however long it goes without renewing — the reviewer-cleared Unix
    /// conservatism. A recycled pid reads as alive here and *retains*.
    #[cfg(unix)]
    #[tokio::test]
    async fn a_live_pid_holder_is_never_reclaimed_by_lease_expiry() {
        let (sup, dir, _w) = make_supervisor();
        let worktree = dir.path().join("worktrees/task-live-holder");
        std::fs::create_dir_all(&worktree).unwrap();

        let guard_id = reserved_guard_id(
            sup.reserve_dispatch_close(&close_guard_params(
                "TASK-LIVE-HOLDER",
                &worktree,
                Some(std::process::id()),
            ))
            .await,
        );
        // No renewal for a full TTL — the daemon-replacement window, where
        // there is no daemon to renew against.
        sup.expire_close_guard_lease_for_test(&guard_id).await;

        let error = sup
            .acquire(
                &AlwaysAttachableDriver,
                worktree_req("TASK-LIVE-HOLDER", dir.path(), &worktree),
            )
            .await
            .expect_err("an alive holder keeps its guard regardless of the lease");
        assert!(
            matches!(error, SupervisorError::CleanupInProgress { .. }),
            "got {error:?}"
        );
    }

    /// The P1: a holder the daemon cannot probe. `owner_pid: None` used to be
    /// retained forever — a permanent denial of service on that worktree — and
    /// it is also exactly the identity a Windows daemon has for every holder,
    /// because `subprocess_exited` cannot answer there.
    ///
    /// Injection: make `is_abandoned` return `false` for `RenewalLease` and
    /// this acquire is refused forever.
    #[tokio::test]
    async fn a_holder_with_no_probeable_pid_is_reclaimed_when_its_lease_expires() {
        let (sup, dir, _w) = make_supervisor();
        let worktree = dir.path().join("worktrees/task-no-pid-holder");
        std::fs::create_dir_all(&worktree).unwrap();

        let guard_id = reserved_guard_id(
            sup.reserve_dispatch_close(&close_guard_params("TASK-NO-PID-HOLDER", &worktree, None))
                .await,
        );

        // While it renews, it holds.
        let refused = sup
            .acquire(
                &AlwaysAttachableDriver,
                worktree_req("TASK-NO-PID-HOLDER", dir.path(), &worktree),
            )
            .await
            .expect_err("a renewing holder keeps its guard");
        assert!(
            matches!(refused, SupervisorError::CleanupInProgress { .. }),
            "got {refused:?}"
        );
        assert!(
            sup.renew_dispatch_close(&guard_id).await,
            "the daemon must renew a guard it holds"
        );

        // It stops renewing; one TTL later the guard is reclaimable.
        sup.expire_close_guard_lease_for_test(&guard_id).await;
        sup.acquire(
            &AlwaysAttachableDriver,
            worktree_req("TASK-NO-PID-HOLDER", dir.path(), &worktree),
        )
        .await
        .expect("an expired unprobeable holder must be reclaimed");
        assert!(
            !sup.renew_dispatch_close(&guard_id).await,
            "a reclaimed guard must tell its holder it is gone"
        );
    }

    /// The reclamation rule itself, both governance modes, without a daemon.
    #[test]
    fn holder_identity_decides_which_reclamation_signal_applies() {
        let past = Utc::now() - chrono::Duration::seconds(1);
        let future = Utc::now() + chrono::Duration::seconds(60);
        let now = Utc::now();

        // Windows / no-pid: the lease is the only signal, and it works.
        let unprobeable = CloseGuardHolder {
            close_guard_id: "close-guard-a".into(),
            owner_pid: Some(4242),
            governed_by: HolderIdentity::RenewalLease,
            lease_expires_at: past,
        };
        assert!(unprobeable.is_abandoned(now));
        assert!(!CloseGuardHolder {
            lease_expires_at: future,
            ..unprobeable.clone()
        }
        .is_abandoned(now));

        // Unix: a live pid retains no matter how stale the lease is.
        let probeable = CloseGuardHolder {
            close_guard_id: "close-guard-b".into(),
            owner_pid: Some(std::process::id()),
            governed_by: HolderIdentity::ProbeablePid,
            lease_expires_at: past,
        };
        assert!(
            !probeable.is_abandoned(now),
            "an alive pid retains its guard however stale the lease is"
        );

        // And which signal governs is chosen by what the daemon can actually
        // probe, not by hope: a holder with no pid is lease-governed on every
        // platform, and a holder with one is pid-governed only where
        // `subprocess_exited` can answer.
        assert_eq!(
            CloseGuardHolder::identity_for(None),
            HolderIdentity::RenewalLease
        );
        assert_eq!(
            CloseGuardHolder::identity_for(Some(4242)),
            if cfg!(unix) {
                HolderIdentity::ProbeablePid
            } else {
                HolderIdentity::RenewalLease
            }
        );
    }

    /// A guard whose holder died while the daemon was down must not be
    /// inherited: reclamation runs at restore, not only at admission.
    #[tokio::test]
    async fn an_expired_persisted_guard_is_not_inherited_by_the_replacement() {
        let dir = tempfile::tempdir().unwrap();
        let store = dir.path().join("state/close-guards");
        std::fs::create_dir_all(&store).unwrap();
        let worktree = dir.path().join("worktrees/task-expired");
        std::fs::create_dir_all(&worktree).unwrap();
        let guard_id = "close-guard-expired-holder";
        let record = serde_json::json!({
            "project_id": "orgasmic",
            "task_id": "TASK-EXPIRED",
            "kind": "worker",
            "worktree_key": normalize_cleanup_worktree(&worktree),
            "reservation": {
                "branch": "task-expired-impl",
                "worktree_path": worktree,
                "dispatch_attempt_token": null,
                "last_path": null,
                "stdout_path": null,
                "holder": {
                    "close_guard_id": guard_id,
                    "owner_pid": null,
                    "governed_by": "renewal_lease",
                    "lease_expires_at": Utc::now() - chrono::Duration::seconds(5),
                }
            }
        });
        std::fs::write(
            store.join(guard_id),
            serde_json::to_vec_pretty(&record).unwrap(),
        )
        .unwrap();

        let writer = spawn_writer(EventBus::new());
        let replacement = Supervisor::unmonitored(
            writer,
            Arc::new(BootIdentity::new()),
            CloseGuardStore::at(&store),
        );
        replacement
            .acquire(
                &AlwaysAttachableDriver,
                worktree_req("TASK-EXPIRED", dir.path(), &worktree),
            )
            .await
            .expect("a guard whose holder is gone must not survive the handoff");
        assert!(
            !store.join(guard_id).exists(),
            "the reclaimed guard record must be deleted, not left to be re-read"
        );
    }

    /// Ask 2: a close must not read the run map while boot rehydration is still
    /// deciding which of the previous daemon's runtimes are alive.
    #[tokio::test]
    async fn a_close_waits_for_boot_rehydration_before_it_reserves() {
        let (sup, dir, _w) = make_supervisor();
        let worktree = dir.path().join("worktrees/task-boot-wait");
        std::fs::create_dir_all(&worktree).unwrap();
        sup.begin_boot_reattach();

        let close = {
            let sup = sup.clone();
            let worktree = worktree.clone();
            tokio::spawn(async move {
                sup.reserve_dispatch_close(&close_guard_params(
                    "TASK-BOOT-WAIT",
                    &worktree,
                    Some(std::process::id()),
                ))
                .await
            })
        };
        // It is parked, not deciding: give it room to have decided wrongly.
        tokio::time::sleep(Duration::from_millis(150)).await;
        if close.is_finished() {
            panic!(
                "the close must not reserve while rehydration is unresolved; it returned {:?}",
                close.await
            );
        }

        // Rehydration finds the worktree occupied by a run that outlived the
        // previous daemon.
        reattach_into(
            &sup,
            "run-rehydrated",
            "TASK-BOOT-WAIT",
            &worktree,
            &dir.path().join("run-rehydrated.jsonl"),
        )
        .await
        .expect("boot rehydration installs the surviving run");
        sup.finish_boot_reattach();

        let outcome = close.await.expect("close task");
        match outcome {
            DispatchCloseGuardOutcome::BlockedByLiveRun { run_id, .. } => {
                assert_eq!(run_id, "run-rehydrated");
            }
            other => {
                panic!("a close that waited for rehydration must see the rehydrated run: {other:?}")
            }
        }
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
            applicable_states: Vec::new(),
            max_iterations: None,
            planned_identity: None,
        }
    }

    #[tokio::test]
    async fn semantic_turn_count_ignores_substantive_events_and_breaches_once() {
        let (sup, dir, _writer) = make_supervisor();
        let driver = SemanticTurnCountDriver;
        let mut req = impl_req("TASK-SEM-TURN", dir.path());
        req.max_iterations = Some(1);
        let resp = sup.acquire(&driver, req).await.unwrap();
        wait_for_run_release(&sup, &resp.run_id, Duration::from_secs(5)).await;

        let events = session_events(&dir.path().join("TASK-SEM-TURN.jsonl"));
        let agent_turns = events
            .iter()
            .filter(|envelope| {
                envelope.kind == SessionEventKind::DriverEvent
                    && envelope.event.get("type").and_then(|ty| ty.as_str())
                        == Some("agent_turn_complete")
            })
            .count();
        assert_eq!(agent_turns, 2, "driver emits two turn boundaries");

        let failed_releases = events
            .iter()
            .filter(|envelope| {
                envelope.kind == SessionEventKind::Lifecycle
                    && matches!(
                        serde_json::from_value::<Lifecycle>(envelope.event.clone()),
                        Ok(Lifecycle::Release {
                            outcome: ReleaseOutcome::Failed,
                            ..
                        })
                    )
            })
            .count();
        assert_eq!(
            failed_releases, 1,
            "iteration limit breach must write exactly one Failed tombstone"
        );
    }

    /// Control release emits RunComplete only — never a fabricated turn boundary.
    struct ReleaseRunCompleteOnlyControl {
        events: tokio::sync::mpsc::Sender<DriverEvent>,
        terminal_emitted: std::sync::atomic::AtomicBool,
    }

    #[async_trait::async_trait]
    impl DriverControl for ReleaseRunCompleteOnlyControl {
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

        async fn release(&mut self, reason: &str) -> Result<(), DriverError> {
            if !self
                .terminal_emitted
                .swap(true, std::sync::atomic::Ordering::SeqCst)
            {
                let _ = self
                    .events
                    .send(DriverEvent::RunComplete {
                        summary: Some(reason.to_string()),
                    })
                    .await;
            }
            Ok(())
        }
    }

    /// Emits an optional count of native turn boundaries, then stays alive until release.
    struct BoundedTurnDriver {
        turn_boundaries: u64,
    }

    #[async_trait::async_trait]
    impl WorkerDriver for BoundedTurnDriver {
        fn transport(&self) -> &'static str {
            "tmux"
        }

        async fn acquire(
            &self,
            ctx: DriverContext,
            _config: DriverConfig,
        ) -> Result<DriverSession, DriverError> {
            let (tx, rx) = tokio::sync::mpsc::channel(32);
            let emit_tx = tx.clone();
            let turn_boundaries = self.turn_boundaries;
            tokio::spawn(async move {
                let _ = emit_tx
                    .send(DriverEvent::Ready {
                        protocol_version: "bounded-turn-test/1".into(),
                        capabilities: json!({"simulated": true}),
                    })
                    .await;
                for seq in 0..turn_boundaries {
                    let _ = emit_tx.send(DriverEvent::AgentTurnComplete { seq }).await;
                }
            });
            Ok(DriverSession {
                identity: ctx.identity,
                pid: None,
                events: rx,
                control: Box::new(ReleaseRunCompleteOnlyControl {
                    events: tx,
                    terminal_emitted: std::sync::atomic::AtomicBool::new(false),
                }),
                native_runtime: None,
                producer: None,
            })
        }
    }

    fn release_outcomes(session_path: &Path) -> Vec<ReleaseOutcome> {
        session_events(session_path)
            .into_iter()
            .filter_map(|envelope| {
                if envelope.kind != SessionEventKind::Lifecycle {
                    return None;
                }
                match serde_json::from_value::<Lifecycle>(envelope.event) {
                    Ok(Lifecycle::Release { outcome, .. }) => Some(outcome),
                    _ => None,
                }
            })
            .collect()
    }

    #[tokio::test]
    async fn release_after_exact_max_turns_does_not_fabricate_iteration_limit_failure() {
        let (sup, dir, _writer) = make_supervisor();
        let driver = BoundedTurnDriver { turn_boundaries: 1 };
        let mut req = impl_req("TASK-RELEASE-AT-LIMIT", dir.path());
        req.max_iterations = Some(1);
        let resp = sup.acquire(&driver, req).await.unwrap();
        tokio::time::sleep(Duration::from_millis(50)).await;
        sup.release(&resp.run_id, "control release", ReleaseOutcome::Completed)
            .await
            .unwrap();
        wait_for_run_release(&sup, &resp.run_id, Duration::from_secs(5)).await;

        let outcomes = release_outcomes(&dir.path().join("TASK-RELEASE-AT-LIMIT.jsonl"));
        assert!(
            !outcomes.contains(&ReleaseOutcome::Failed),
            "release after exactly max native turns must not fail iteration limit: {outcomes:?}"
        );
    }

    #[tokio::test]
    async fn release_before_any_native_turn_counts_zero_turns() {
        let (sup, dir, _writer) = make_supervisor();
        let driver = BoundedTurnDriver { turn_boundaries: 0 };
        let mut req = impl_req("TASK-RELEASE-ZERO-TURNS", dir.path());
        req.max_iterations = Some(1);
        let resp = sup.acquire(&driver, req).await.unwrap();
        sup.release(&resp.run_id, "early release", ReleaseOutcome::Completed)
            .await
            .unwrap();
        wait_for_run_release(&sup, &resp.run_id, Duration::from_secs(5)).await;

        let outcomes = release_outcomes(&dir.path().join("TASK-RELEASE-ZERO-TURNS.jsonl"));
        assert!(
            !outcomes.contains(&ReleaseOutcome::Failed),
            "release before any native turn must not count a fabricated boundary: {outcomes:?}"
        );
    }

    #[tokio::test]
    async fn excess_native_turn_boundaries_fail_exactly_once() {
        let (sup, dir, _writer) = make_supervisor();
        let driver = BoundedTurnDriver { turn_boundaries: 2 };
        let mut req = impl_req("TASK-EXCESS-TURNS", dir.path());
        req.max_iterations = Some(1);
        let resp = sup.acquire(&driver, req).await.unwrap();
        wait_for_run_release(&sup, &resp.run_id, Duration::from_secs(5)).await;

        let outcomes = release_outcomes(&dir.path().join("TASK-EXCESS-TURNS.jsonl"));
        assert_eq!(
            outcomes
                .iter()
                .filter(|o| **o == ReleaseOutcome::Failed)
                .count(),
            1,
            "exactly one Failed tombstone after breaching max turns: {outcomes:?}"
        );
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

    /// Age a run's clocks. `last_driver_event_age` means "nothing at all has
    /// arrived on the driver channel for this long", so it ages the work clock
    /// with the liveness clock — no event means no work event either. A test
    /// that needs the two to diverge (TASK-JK66P: a live run whose channel is
    /// silent) drives real events at a real cadence instead.
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
            rec.last_progress_at = now - age;
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
            // A released run is absent, not zero. Collapsing the two costs a
            // debugging session every time: "reached event_count 0" reads as a
            // slow event task when it actually means something else released
            // the run, and no amount of waiting will ever satisfy the count.
            let seen = sup
                .snapshot()
                .await
                .runs
                .iter()
                .find(|run| run.run_id == run_id)
                .map(|run| run.event_count);
            match seen {
                Some(seen) if seen >= count => return,
                None => panic!(
                    "run {run_id} left the supervisor while waiting for event_count {count}; \
                     something released it (a background timeout monitor?), so this wait \
                     can never be satisfied"
                ),
                Some(seen) if tokio::time::Instant::now() >= deadline => {
                    panic!("run {run_id} reached event_count {seen}, wanted {count}")
                }
                Some(_) => {}
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    }

    fn assert_release_reason(path: &Path, reason: &str) {
        assert_release(path, reason, "failed");
    }

    fn assert_release(path: &Path, reason: &str, outcome: &str) {
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
            Some(outcome)
        );
    }

    fn release_count(path: &Path) -> usize {
        session_events(path)
            .iter()
            .filter(|envelope| {
                envelope.kind == SessionEventKind::Lifecycle
                    && envelope.event.get("phase").and_then(|phase| phase.as_str())
                        == Some("release")
            })
            .count()
    }

    fn driver_event_count(path: &Path) -> usize {
        session_events(path)
            .iter()
            .filter(|envelope| envelope.kind == SessionEventKind::DriverEvent)
            .count()
    }

    /// The release tombstone's reason verbatim — for the assertions that read
    /// the detail a stall now carries, not just its leading token.
    fn release_reason(path: &Path) -> Option<String> {
        session_events(path).into_iter().find_map(|envelope| {
            (envelope.kind == SessionEventKind::Lifecycle
                && envelope.event.get("phase").and_then(|phase| phase.as_str()) == Some("release"))
            .then(|| {
                envelope
                    .event
                    .get("reason")
                    .and_then(|value| value.as_str())
                    .unwrap_or_default()
                    .to_string()
            })
        })
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

    /// TASK-RWCRN, the working half. Reproduces the measured shape of
    /// run-20260726T193954-5ce5327e7b854438843e7f592f66dc4d: an rmux run whose
    /// last driver event is `ready`, aged past the real 600 s
    /// `DEFAULT_STALL_TIMEOUT` while the worker is still working. One
    /// `pane_activity` event — what the driver now publishes while the pane
    /// writes output — must save it. Before this signal existed the same run was
    /// released at exactly ten minutes with the worker mid-edit.
    ///
    /// Unmonitored on purpose: the assertion is that the run is STILL LIVE, and
    /// the background monitor would release the aged run during the awaits that
    /// deliver the injected event.
    #[tokio::test]
    async fn pane_activity_saves_a_working_rmux_pane_from_the_stall_detector() {
        let (sup, dir, _w) = make_unmonitored_supervisor();
        let driver = RmuxPaneDriver::new();
        let req = manual_req(
            "TASK-RMUX-PANE-WORKING",
            dir.path(),
            Some(DEFAULT_STALL_TIMEOUT.as_secs() as u32),
            None,
        );
        let session_path = req.session_path.clone();
        let resp = sup.acquire(&driver, req).await.unwrap();
        wait_for_event_count(&sup, &resp.run_id, 1).await;

        age_run(
            &sup,
            &resp.run_id,
            Some(DEFAULT_STALL_TIMEOUT + Duration::from_secs(1)),
            None,
        )
        .await;

        driver
            .inject(DriverEvent::PaneActivity {
                seq: 0,
                bytes: 16_480,
            })
            .await;
        wait_for_event_count(&sup, &resp.run_id, 2).await;
        sup.release_first_timed_out_run().await;

        assert!(
            run_is_live(&sup, &resp.run_id).await,
            "a pane that is still writing output must not be released as stalled"
        );
        assert!(!has_release_reason(&session_path, "stall_timeout_exceeded"));
    }

    /// TASK-RWCRN, the wedged half — the case that makes this a liveness signal
    /// rather than `stall_timeout_secs: Some(0)`. A pane that writes nothing
    /// (harness parked on an interactive gate, or finished without calling
    /// `dispatch finalize`) publishes no `pane_activity`, so the stall clock
    /// stays frozen and the run is still released at the threshold instead of
    /// burning the 4-hour `DEFAULT_MAX_RUN_DURATION`.
    #[tokio::test]
    async fn a_silent_rmux_pane_is_still_released_as_stalled() {
        let (sup, dir, _w) = make_supervisor();
        let driver = RmuxPaneDriver::new();
        let req = manual_req("TASK-RMUX-PANE-WEDGED", dir.path(), Some(1), None);
        let session_path = req.session_path.clone();
        let resp = sup.acquire(&driver, req).await.unwrap();
        wait_for_event_count(&sup, &resp.run_id, 1).await;

        age_run(&sup, &resp.run_id, Some(Duration::from_millis(1_001)), None).await;
        sup.release_first_timed_out_run().await;

        assert!(!run_is_live(&sup, &resp.run_id).await);
        assert_release_reason(&session_path, "stall_timeout_exceeded");
    }

    /// TASK-VZMZE, the wedge. Measured 2026-07-26 on
    /// run-20260726T144430-aa47b867840f42f282b30d3469949731: 118 heartbeats at
    /// 30 s intervals, 0 tool calls, 0 worktree bytes, 6.77 s of CPU in an
    /// hour — and the supervisor could not tell it from a run making 243 tool
    /// calls, because the drain refreshed `last_driver_event_at` for every
    /// drained event, variant-agnostically.
    ///
    /// Real cadence against a compressed budget, not a backdated clock: the
    /// heartbeats genuinely arrive faster than the stall window, exactly as
    /// they did in production. The clock they must not refresh is the work
    /// clock; the liveness clock they DO refresh is asserted below, because
    /// "the harness is gone" is a different classification that must stay
    /// available (this task's second acceptance line).
    #[tokio::test]
    async fn heartbeats_are_liveness_not_work_so_a_wedged_run_still_stalls() {
        let (sup, dir, _w) = make_unmonitored_supervisor();
        let driver = HeartbeatOnlyAcpDriver::new();
        let req = manual_req("TASK-HEARTBEAT-WEDGE", dir.path(), Some(1), None);
        let session_path = req.session_path.clone();
        let resp = sup.acquire(&driver, req).await.unwrap();
        wait_for_event_count(&sup, &resp.run_id, 1).await;

        // 30 s heartbeats against a 600 s window, compressed by the same
        // factor: 8 beats at 150 ms against a 1 s window.
        for seq in 0..8 {
            tokio::time::sleep(Duration::from_millis(150)).await;
            driver.inject(DriverEvent::Heartbeat { seq }).await;
            wait_for_event_count(&sup, &resp.run_id, 2 + seq).await;
        }

        sup.release_first_timed_out_run().await;

        assert!(
            !run_is_live(&sup, &resp.run_id).await,
            "a run whose only traffic is heartbeats has produced no evidence of \
             work and must stall on the normal schedule, not at max_run_duration"
        );
        assert_release_reason(&session_path, "stall_timeout_exceeded");
    }

    /// The mechanism behind the test above, asserted on the record itself: one
    /// event, two clocks, opposite answers. A heartbeat must keep proving the
    /// harness is *there* — "the harness is gone" is a different failure with a
    /// different response — while proving nothing about work.
    #[tokio::test]
    async fn a_heartbeat_refreshes_liveness_but_not_the_work_clock() {
        let (sup, dir, _w) = make_unmonitored_supervisor();
        let driver = HeartbeatOnlyAcpDriver::new();
        let req = manual_req("TASK-HEARTBEAT-CLOCKS", dir.path(), Some(600), None);
        let resp = sup.acquire(&driver, req).await.unwrap();
        wait_for_event_count(&sup, &resp.run_id, 1).await;

        tokio::time::sleep(Duration::from_millis(20)).await;
        let before_beat = Instant::now();
        driver.inject(DriverEvent::Heartbeat { seq: 0 }).await;
        wait_for_event_count(&sup, &resp.run_id, 2).await;

        let g = sup.inner.lock().await;
        let rec = g.runs.get(&resp.run_id).expect("run is still live");
        assert!(
            rec.last_driver_event_at >= before_beat,
            "a heartbeat must still refresh liveness"
        );
        assert!(
            rec.last_progress_at < before_beat,
            "a heartbeat must not refresh the work clock"
        );
    }

    /// TASK-JK66P, the healthy half. Measured 2026-07-29 on
    /// dispatch-TASK-MRJRK-implementer-20260729T000911: `pane_activity` every
    /// ~30 s until 00:41:22, then the worker ran `scripts/run-tests.sh` — whose
    /// output goes to files, so the pane writes nothing — and at 00:51:22, ten
    /// minutes of pane silence to the second, the daemon released it as
    /// stalled. It was healthy: report written, work committed, verify artifact
    /// self-tested PASS. It died at its final gate.
    ///
    /// The pane is the only channel an rmux run has, and it was empty. The
    /// evidence that existed was under the pane, not on it, and the probe is
    /// what reads it. Three consecutive expired budgets here, because the
    /// acceptance is that such a run survives *indefinitely* — one saved sweep
    /// would not distinguish a fix from an off-by-one.
    #[tokio::test]
    async fn a_silent_pane_with_live_work_under_it_is_never_stalled() {
        let (sup, dir, _w) = make_unmonitored_supervisor();
        sup.set_work_probe(Arc::new(FixedWorkProbe(WorkEvidence::Working {
            detail: "9 process(es) under pid 4242 at 412.0% cpu (work threshold 5.0%)".into(),
        })));
        let driver = RmuxPaneDriver::new();
        let req = manual_req("TASK-QUIET-BUT-WORKING", dir.path(), Some(1), None);
        let session_path = req.session_path.clone();
        let resp = sup.acquire(&driver, req).await.unwrap();
        wait_for_event_count(&sup, &resp.run_id, 1).await;

        for window in 0..3 {
            // No pane bytes at all in this window — the measured MRJRK shape.
            age_run(&sup, &resp.run_id, Some(Duration::from_millis(1_001)), None).await;
            sup.release_first_timed_out_run().await;
            assert!(
                run_is_live(&sup, &resp.run_id).await,
                "quiet window {window}: a run with live work under it is not stalled"
            );
        }
        assert!(!has_release_reason(&session_path, "stall_timeout_exceeded"));

        // And it is not immortal: the same run, once the work under it stops,
        // dies on the next expired budget. This is the half that keeps the fix
        // from being `stall_timeout_secs: Some(0)` by another name.
        sup.set_work_probe(Arc::new(FixedWorkProbe(WorkEvidence::Idle {
            detail: "1 process(es) under pid 4242 at 0.2% cpu (work threshold 5.0%)".into(),
        })));
        age_run(&sup, &resp.run_id, Some(Duration::from_millis(1_001)), None).await;
        sup.release_first_timed_out_run().await;
        assert!(
            !run_is_live(&sup, &resp.run_id).await,
            "when the work under a quiet pane stops, the stall stands"
        );
    }

    /// TASK-JK66P ask 4. `stall_timeout_exceeded` on its own told the operator
    /// nothing: MRJRK's healthy worker and VZMZE's wedge produced the identical
    /// tombstone. The reason now says which evidence was absent and for how
    /// long, while keeping `stall_timeout_exceeded` as its first token so every
    /// consumer that classifies a stall sweep still classifies it.
    #[tokio::test]
    async fn the_stall_reason_names_the_evidence_that_was_absent() {
        let (sup, dir, _w) = make_unmonitored_supervisor();
        sup.set_work_probe(Arc::new(FixedWorkProbe(WorkEvidence::Idle {
            detail: "1 process(es) under pid 4242 at 0.2% cpu (work threshold 5.0%)".into(),
        })));
        let driver = RmuxPaneDriver::new();
        let req = manual_req("TASK-STALL-REASON", dir.path(), Some(1), None);
        let session_path = req.session_path.clone();
        let resp = sup.acquire(&driver, req).await.unwrap();
        wait_for_event_count(&sup, &resp.run_id, 1).await;

        age_run(&sup, &resp.run_id, Some(Duration::from_secs(612)), None).await;
        sup.release_first_timed_out_run().await;

        let reason = release_reason(&session_path).expect("release tombstone");
        assert_eq!(
            reason.split(':').next(),
            Some("stall_timeout_exceeded"),
            "the leading token is what CLI and API classify on: {reason}"
        );
        assert!(
            reason.contains("no work evidence for 612s"),
            "the reason must say for how long: {reason}"
        );
        assert!(
            reason.contains("0.2% cpu"),
            "the reason must say what was looked at and what it showed: {reason}"
        );
    }

    /// A probe that cannot answer must not be able to save a run. This is the
    /// path every run with no resolvable pane or pid takes — and, if a future
    /// probe breaks or times out, the path all of them take.
    #[tokio::test]
    async fn an_unobservable_run_still_stalls_on_the_bare_reason() {
        let (sup, dir, _w) = make_unmonitored_supervisor();
        let driver = RmuxPaneDriver::new();
        let req = manual_req("TASK-UNPROBEABLE", dir.path(), Some(1), None);
        let session_path = req.session_path.clone();
        let resp = sup.acquire(&driver, req).await.unwrap();
        wait_for_event_count(&sup, &resp.run_id, 1).await;

        age_run(&sup, &resp.run_id, Some(Duration::from_millis(1_001)), None).await;
        sup.release_first_timed_out_run().await;

        assert!(!run_is_live(&sup, &resp.run_id).await);
        assert_release_reason(&session_path, "stall_timeout_exceeded");
    }

    /// The classification itself, event by event, with the reason each one is
    /// on the side it is on.
    #[test]
    fn the_stall_clock_advances_only_on_evidence_of_work() {
        // Not work: liveness, startup, transport breakage, harness stderr.
        assert!(!driver_event_advances_stall_clock(
            &DriverEvent::Heartbeat { seq: 7 }
        ));
        assert!(!driver_event_advances_stall_clock(&DriverEvent::Ready {
            protocol_version: "acp/1".into(),
            capabilities: json!({}),
        }));
        assert!(!driver_event_advances_stall_clock(
            &DriverEvent::DriverError {
                fatal: false,
                message: "failed to renew cache TTL".into(),
            }
        ));
        assert!(
            !driver_event_advances_stall_clock(&DriverEvent::TextChunk {
                stream: TextStream::Stderr,
                chunk: "codex_models_manager::cache: failed to load models cache".into(),
                seq: 0,
            }),
            "VZMZE's wedged run emitted 12 of these and nothing else, and its \
             healthy sibling emitted the same messages"
        );

        // Work: anything the worker did, plus a pane that demonstrably wrote.
        assert!(driver_event_advances_stall_clock(&DriverEvent::ToolCall {
            call_id: "1".into(),
            name: "read".into(),
            args: json!({}),
            seq: 0,
        }));
        assert!(driver_event_advances_stall_clock(&DriverEvent::TextChunk {
            stream: TextStream::Stdout,
            chunk: "test result: ok".into(),
            seq: 1,
        }));
        assert!(driver_event_advances_stall_clock(
            &DriverEvent::AgentTurnComplete { seq: 3 }
        ));

        // The RWCRN split, in one assertion: pane bytes are not proof the
        // WORKER worked (so the early-exit classifier ignores them) and are
        // proof the run is not frozen (so the stall clock takes them). An rmux
        // run has no other stall input; dropping this is what would kill every
        // rmux dispatch at 600s again.
        let pane = DriverEvent::PaneActivity { seq: 0, bytes: 480 };
        assert!(!driver_event_counts_as_work(&pane));
        assert!(driver_event_advances_stall_clock(&pane));
    }

    /// The production probe against a real process subtree — the part no
    /// supervisor test can prove, because it cannot put a cargo build under a
    /// pane. A `sh` burning a core is MRJRK's `cargo`; the assertion is that
    /// the probe's `ps` parse, subtree walk, and threshold agree with reality
    /// on this OS.
    #[cfg(unix)]
    #[tokio::test]
    async fn process_subtree_cpu_probe_sees_a_real_cpu_burning_child() {
        use std::os::unix::process::CommandExt as _;
        // A signed interpreter, not a fixture script: on macOS the first exec
        // of a newly created executable is serialized through syspolicy
        // (gotchas.org). Own process group + null stdio so the reap below takes
        // the whole tree and nothing holds the runner's pipes.
        let mut burner = Command::new("/bin/sh")
            .args(["-c", "while :; do :; done"])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .process_group(0)
            .spawn()
            .expect("spawn cpu burner");
        let pid = burner.id();
        let target = WorkProbeTarget {
            transport: "acp-stdio".into(),
            identity: probe_identity(),
            pid: Some(pid),
        };

        // macOS reports a decaying utilization average, so the number needs a
        // moment of real burning before it crosses the threshold; Linux's
        // lifetime average crosses almost immediately. Poll rather than sleep a
        // guessed amount.
        let deadline = Instant::now() + Duration::from_secs(10);
        let mut last = WorkEvidence::Unknown;
        while Instant::now() < deadline {
            last = ProcessSubtreeCpuProbe::default().observe(&target);
            if matches!(last, WorkEvidence::Working { .. }) {
                break;
            }
            tokio::time::sleep(Duration::from_millis(200)).await;
        }

        let _ = Command::new("kill")
            .args(["-KILL", &format!("-{pid}")])
            .status();
        let _ = burner.kill();
        let _ = burner.wait();

        assert!(
            matches!(last, WorkEvidence::Working { .. }),
            "a process burning a core is live work, got {last:?}"
        );
    }

    /// The other half of the same production path: an alive-but-idle process is
    /// NOT work. This is VZMZE's wedge in miniature — its harness was alive for
    /// the whole hour it did nothing — and it is why the probe measures CPU
    /// rather than liveness.
    #[cfg(unix)]
    #[tokio::test]
    async fn process_subtree_cpu_probe_does_not_mistake_a_live_idle_child_for_work() {
        use std::os::unix::process::CommandExt as _;
        let mut sleeper = Command::new("/bin/sh")
            .args(["-c", "sleep 30"])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .process_group(0)
            .spawn()
            .expect("spawn idle child");
        let pid = sleeper.id();
        let observed = ProcessSubtreeCpuProbe::default().observe(&WorkProbeTarget {
            transport: "acp-stdio".into(),
            identity: probe_identity(),
            pid: Some(pid),
        });

        let _ = Command::new("kill")
            .args(["-KILL", &format!("-{pid}")])
            .status();
        let _ = sleeper.kill();
        let _ = sleeper.wait();

        assert!(
            matches!(observed, WorkEvidence::Idle { .. }),
            "a live but idle process is not evidence of work, got {observed:?}"
        );
    }

    /// A pid that is gone is `Idle`, not `Unknown`: the daemon looked and there
    /// was nothing there, which is a finding, not a failure to observe.
    #[test]
    fn process_subtree_cpu_probe_reports_a_vanished_process_as_idle() {
        let table = vec![(1_u32, 0_u32, 0.0_f32), (2, 1, 0.0)];
        assert!(subtree_cpu_percent(&table, 4_242).is_none());

        // And the subtree really is a subtree: a grandchild's cpu counts.
        let table = vec![(10, 1, 0.1), (11, 10, 90.0), (12, 11, 5.0), (99, 1, 400.0)];
        let (processes, cpu) = subtree_cpu_percent(&table, 10).expect("root is live");
        assert_eq!(processes, 3, "root, child, grandchild — and not pid 99");
        assert!((cpu - 95.1).abs() < 0.01, "summed subtree cpu: {cpu}");
    }

    fn probe_identity() -> RuntimeIdentity {
        RuntimeIdentity {
            run_id: "run-probe".into(),
            runtime_id: "runtime-probe".into(),
            boot_id: "boot-probe".into(),
        }
    }

    /// TASK-JQ8AV, the classifier against the MEASURED captures — both
    /// in-turn shapes are verbatim from real panes (2026-07-29), and the
    /// at-rest material is what a throwaway claude TUI actually showed.
    #[test]
    fn pane_open_turn_marker_recognizes_the_measured_statuslines() {
        // In-turn, live capture of this fleet's own harness.
        let live = "● Moonwalking… (20m 4s · ↓ 46.3k tokens)";
        assert_eq!(pane_open_turn_marker(live).as_deref(), Some(live));
        // In-turn, the incident capture: the statusline the three killed
        // workers' panes were showing while the clock read them as wedged.
        let incident = "✽ Quantumizing… (3m41s · ↓13.1k tokens · thinking with high effort)";
        assert_eq!(pane_open_turn_marker(incident).as_deref(), Some(incident));
        // Elapsed-only spinner: a turn open before the first token arrives.
        assert!(pane_open_turn_marker("· Considering… (2s)").is_some());
        // The generic TUI interrupt hint, any case (codex-style spinners).
        assert!(pane_open_turn_marker("Working (7s • Esc to interrupt)").is_some());

        // At-rest pane furniture must all fall through: an at-rest harness is
        // the wedge's shape, and must not be rescued by its own chrome. The
        // status bar has the glyph anchor but no `… (`; the collapsed
        // transcript line has an ellipsis but no `… (`; the banner's second
        // char is not a space; the bare prompt has nothing after the glyph.
        let at_rest = " ▐▛███▜▌   Claude Code v2.1.220\n\
                       ▝▜█████▛▘  Fable 5 with high effort · Claude Max\n\
                       ❯ \n\
                       ● ~/Documents/code/tools/orgasmic | Fable 5 ⇢75k | 0k/1000k\n\
                       ⏵⏵ auto mode on (shift+tab to cycle) · ← 1 agent\n\
                       ⏺ Thinking for 8m 28s, running 12 shell commands…";
        assert_eq!(pane_open_turn_marker(at_rest), None);

        // The last match wins: the live statusline sits at the bottom of the
        // screen, below any transcript text that might quote an older one.
        let stacked = format!("{incident}\nsome transcript text\n{live}");
        assert_eq!(pane_open_turn_marker(&stacked).as_deref(), Some(live));
    }

    /// Create the run's pane on the test-owned rmux server, running `script`
    /// under `shell`. Addressed by explicit `-S`, same as every other probe
    /// call in this test family — an unpinned call would land on the server
    /// hosting live dispatch panes.
    #[cfg(unix)]
    fn spawn_probe_pane_stub(socket: &Path, session: &str, shell: &str, script: &str) {
        let probe = probe_rmux_binary();
        let rmux_bin = probe.path.filter(|_| probe.found).expect("rmux-gated test");
        let status = Command::new(rmux_bin)
            .arg("-S")
            .arg(socket)
            .args([
                "new-session",
                "-d",
                "-x",
                "200",
                "-y",
                "50",
                "-s",
                session,
                "--",
                shell,
                "-c",
                script,
            ])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .expect("spawn rmux pane stub");
        assert!(status.success(), "rmux new-session failed for {session}");
    }

    #[cfg(unix)]
    fn kill_probe_pane_stub(socket: &Path, session: &str) {
        let probe = probe_rmux_binary();
        let Some(rmux_bin) = probe.path.filter(|_| probe.found) else {
            return;
        };
        let _ = Command::new(rmux_bin)
            .arg("-S")
            .arg(socket)
            .args(["kill-session", "-t", session])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();
    }

    #[cfg(unix)]
    async fn wait_for_open_turn_marker(identity: &RuntimeIdentity, socket: &Path) {
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            if let Some(pane) = rmux_pane_content(identity, Some(socket)) {
                if pane_open_turn_marker(&pane).is_some() {
                    return;
                }
            }
            assert!(
                Instant::now() < deadline,
                "stub pane never showed the open-turn statusline"
            );
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    }

    #[cfg(unix)]
    async fn wait_for_pane_pid(identity: &RuntimeIdentity, socket: &Path) {
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            if rmux_pane_pid(identity, Some(socket)).is_some() {
                return;
            }
            assert!(
                Instant::now() < deadline,
                "stub pane never resolved a pane pid"
            );
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    }

    /// TASK-JQ8AV acceptance, both halves, against the REAL probe reading a
    /// REAL rmux pane — and a stub that is NETWORK-WAITING rather than
    /// sleeping, because the distinction is the task: a multi-minute
    /// server-side think is ~0% cpu with an ESTABLISHED provider connection
    /// and an open-turn statusline on screen, and under the two byte channels
    /// alone that is indistinguishable from VZMZE's wedge (which is what
    /// killed FZB6T, RB1ZN and SZJ2B on 2026-07-29).
    ///
    /// Three consecutive expired budgets, for the same reason
    /// [`a_silent_pane_with_live_work_under_it_is_never_stalled`] uses three:
    /// the acceptance is that a provider-bound run survives *indefinitely*,
    /// and one saved sweep would not distinguish a fix from an off-by-one.
    /// Then the wedge half: same run, pane at rest, process asleep with no
    /// connection anywhere — it must die on schedule, with a reason that
    /// names every channel that came up empty.
    #[cfg(unix)]
    #[tokio::test]
    async fn a_network_waiting_provider_turn_survives_the_stall_window_and_a_wedge_dies() {
        const TEST: &str =
            "a_network_waiting_provider_turn_survives_the_stall_window_and_a_wedge_dies";
        // Lock order is flock-then-environment (the owned-server fixture pins
        // process-global rmux endpoint resolution).
        let _live_guard = live_session_guard();
        let _environment = test_environment_lock().lock().await;
        if skip_test_if_missing(TEST, &[("rmux", probe_rmux_binary().usable())]) {
            return;
        }
        let server = own_rmux_server_for_tests();
        let socket = server.endpoint_path().to_path_buf();

        // The provider's stand-in: a local listener the stub connects to and
        // then blocks reading from.
        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind local listener");
        let port = listener.local_addr().expect("listener addr").port();

        let (sup, dir, _w) = make_unmonitored_supervisor();
        sup.set_work_probe(Arc::new(ProcessSubtreeCpuProbe::with_rmux_socket(&socket)));
        let driver = RmuxPaneDriver::new();
        let req = manual_req("TASK-PROVIDER-BOUND", dir.path(), Some(1), None);
        let session_path = req.session_path.clone();
        let resp = sup.acquire(&driver, req).await.unwrap();
        wait_for_event_count(&sup, &resp.run_id, 1).await;
        let session = rmux_session_name(&resp.identity);

        // The incident statusline verbatim, then a genuine network wait:
        // `exec 3<>/dev/tcp` holds an ESTABLISHED connection and `read`
        // blocks on it at ~0% cpu.
        let think_stub = format!(
            "printf '✽ Quantumizing… (3m41s · ↓13.1k tokens · thinking with high effort)\\n'; \
             exec 3<>/dev/tcp/127.0.0.1/{port}; read -u 3 line"
        );
        spawn_probe_pane_stub(&socket, &session, "/bin/bash", &think_stub);
        // Accepting the stub's connection is the proof it is network-waiting
        // and not asleep; holding the stream keeps it blocked mid-read.
        let _provider_side = tokio::time::timeout(
            Duration::from_secs(10),
            tokio::task::spawn_blocking(move || listener.accept()),
        )
        .await
        .expect("stub connected within 10s")
        .expect("accept task")
        .expect("accept establishes the stub's connection");
        wait_for_open_turn_marker(&resp.identity, &socket).await;

        for window in 0..3 {
            age_run(&sup, &resp.run_id, Some(Duration::from_millis(1_001)), None).await;
            sup.release_first_timed_out_run().await;
            assert!(
                run_is_live(&sup, &resp.run_id).await,
                "quiet window {window}: a provider-bound network-waiting turn was \
                 stall-released: {:?}",
                release_reason(&session_path)
            );
        }
        assert!(!has_release_reason(&session_path, "stall_timeout_exceeded"));

        // The wedge half. A fresh pane under the same session name shows no
        // statusline, and the process under it sleeps with no connection.
        kill_probe_pane_stub(&socket, &session);
        spawn_probe_pane_stub(&socket, &session, "/bin/sh", "sleep 300");
        wait_for_pane_pid(&resp.identity, &socket).await;
        age_run(&sup, &resp.run_id, Some(Duration::from_millis(1_001)), None).await;
        sup.release_first_timed_out_run().await;
        assert!(
            !run_is_live(&sup, &resp.run_id).await,
            "an idle run with no provider connection and an at-rest pane must still stall"
        );
        let reason = release_reason(&session_path).expect("release tombstone");
        assert_eq!(
            reason.split(':').next(),
            Some("stall_timeout_exceeded"),
            "the leading token is what CLI and API classify on: {reason}"
        );
        assert!(
            reason.contains("no work evidence for") && reason.contains("% cpu"),
            "the reason must still carry the cpu channel: {reason}"
        );
        assert!(
            reason.contains("no open-turn statusline in pane capture"),
            "the reason must say the pane was consulted and came up empty: {reason}"
        );

        kill_probe_pane_stub(&socket, &session);
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
        // Unmonitored on purpose: this test asserts the run is STILL LIVE after
        // the release it drives revalidates and backs off. The background
        // monitor would release the same aged run during the hook's awaits —
        // rarely when the test runs alone, ~2 runs in 3 under module load.
        let (sup, dir, _w) = make_unmonitored_supervisor();
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

    // orgasmic:task_JGHNC
    /// Where a PATH lookup of `tmux` lands, reported rather than reduced to a
    /// yes/no — the strict rule needs the path to follow, not the answer.
    fn which_tmux_for_test() -> Option<std::path::PathBuf> {
        let out = Command::new("which").arg("tmux").output().ok()?;
        if !out.status.success() {
            return None;
        }
        let path = String::from_utf8_lossy(&out.stdout).trim().to_string();
        (!path.is_empty()).then(|| std::path::PathBuf::from(path))
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
        // orgasmic:task_JGHNC
        // Not `command_available_for_test("tmux")`: inside an orgasmic worker
        // that lookup lands on the rmux shim, this probe then spawns a session
        // through it, succeeds, and reports tmux PRESENT — so the tooling
        // sentinel stays green and the honesty manifest never names the tmux
        // tests that did not run. The rule is the api-side one, shared rather
        // than copied, so gate and sentinel cannot disagree.
        if crate::api::tests::tmux_mode_availability_for(which_tmux_for_test().as_deref()).is_err()
        {
            return false;
        }
        // orgasmic:TASK-0RCRY
        // The one probe every tmux-gated test in this binary passes through:
        // claim the owned server before any session is created on it.
        tmux::own_tmux_server_for_tests();
        let session = format!(
            "orgasmic-supervisor-probe-{}",
            Utc::now().timestamp_nanos_opt().unwrap_or(0)
        );
        let status = tokio::process::Command::from(tmux::tmux_command())
            .args(["new-session", "-d", "-s", &session, "--", "sleep", "1"])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .await;
        let ok = status.map(|status| status.success()).unwrap_or(false);
        if ok {
            let _ = tokio::process::Command::from(tmux::tmux_command())
                .args(["kill-session", "-t", &session])
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .status()
                .await;
        }
        ok
    }

    #[tokio::test]
    async fn required_test_tooling_is_present() {
        let _live_guard = live_session_guard();
        let _environment = test_environment_lock().lock().await;
        assert_required_test_tooling(&[
            // orgasmic:task_K4G1D — +1 for the rmux arm of the parameterized
            // attach-proof test in `api`.
            // orgasmic:task_JGHNC — +5 for the rmux arms of the five reattach
            // and fencing shapes parameterized there. The tmux count is
            // corrected rather than moved: it was 6 while nine tests gated on
            // tmux, and a count that understates is the same lie as a probe
            // that overstates — it is the number the NOT RUN block prints.
            // Recount when a mux-gated test is added: api has six
            // `MuxMode::Tmux` arms, six `MuxMode::Rmux` arms, two direct tmux
            // gates and seven direct rmux gates; this module has one tmux
            // gate and two rmux gates.
            // orgasmic:task_JQ8AV — +1 for the network-waiting pane-stub
            // acceptance test.
            ToolRequirement::new("rmux", 15, probe_rmux_binary().found),
            ToolRequirement::new("tmux", 9, tmux_spawn_usable_for_test().await),
            ToolRequirement::new("bash", 1, command_available_for_test("bash")),
        ]);
    }

    async fn tmux_has_session_for_test(session: &str) -> bool {
        tokio::process::Command::from(tmux::tmux_command())
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
        if skip_test_if_missing(
            "tmux_process_exit_releases_supervisor_run_and_session",
            &[
                ("tmux", tmux_spawn_usable_for_test().await),
                ("bash", command_available_for_test("bash")),
            ],
        ) {
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
        let driver = AcceptingInputDriver;
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
            sup.release_orphaned_lease("orgasmic", "TASK-ORPHAN", RunKind::Worker)
                .await,
            OrphanedLeaseOutcome::NoLease
        );
        let r1 = sup
            .acquire(&driver, impl_req("TASK-ORPHAN", dir.path()))
            .await
            .unwrap();
        // A live run holds the lease → refuse to steal it.
        assert_eq!(
            sup.release_orphaned_lease("orgasmic", "TASK-ORPHAN", RunKind::Worker)
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
            sup.release_orphaned_lease("orgasmic", "TASK-ORPHAN", RunKind::Worker)
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
            applicable_states: Vec::new(),
            max_iterations: None,
            planned_identity: None,
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
    async fn no_pid_ready_only_stream_end_releases_failed_once() {
        let (sup, dir, _w) = make_supervisor();
        let driver = NoPidReadyOnlyDriver;
        let resp = sup
            .acquire(&driver, dispatch_impl_req("TASK-NO-PID-EARLY", dir.path()))
            .await
            .unwrap();
        let path = dir.path().join("TASK-NO-PID-EARLY.jsonl");
        let deadline = Instant::now() + Duration::from_secs(2);
        let mut release_count = 0usize;
        while Instant::now() < deadline {
            if let Ok(envelopes) = read_session_file(&path) {
                release_count = envelopes
                    .iter()
                    .filter(|envelope| {
                        envelope.kind == SessionEventKind::Lifecycle
                            && envelope.event.get("phase").and_then(|phase| phase.as_str())
                                == Some("release")
                    })
                    .count();
                if release_count == 1 {
                    break;
                }
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        assert_eq!(
            release_count, 1,
            "no-PID stream end must release exactly once"
        );
        assert_release_reason(&path, "protocol_end_without_finalize");
        let snapshot = sup.snapshot().await;
        assert!(snapshot.runs.iter().all(|run| run.run_id != resp.run_id));
        let held = sup
            .inner
            .lock()
            .await
            .lease(&lease_key(
                Some("orgasmic"),
                "TASK-NO-PID-EARLY",
                RunKind::Worker,
            ))
            .cloned();
        assert!(
            held.is_none(),
            "lease must be released after no-PID early exit"
        );
    }

    #[tokio::test]
    async fn no_pid_stderr_text_chunk_stream_end_releases_once() {
        let (sup, dir, _w) = make_supervisor();
        let driver = NoPidStderrWorkDriver;
        let resp = sup
            .acquire(&driver, dispatch_impl_req("TASK-NO-PID-STDERR", dir.path()))
            .await
            .unwrap();
        let path = dir.path().join("TASK-NO-PID-STDERR.jsonl");
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
    async fn failed_protocol_end_writes_failed_tombstone_without_continuation_spawn() {
        // orgasmic:TASK-QPKCD — failure ends at tombstone; no auto-continuation.
        let (sup, dir, _w) = make_supervisor();
        let driver = ProtocolEndAcpDriver;
        let mut req = dispatch_impl_req("TASK-NO-AUTO-CONT", dir.path());
        req.role = "implementer".into();
        let resp = sup.acquire(&driver, req).await.unwrap();
        let path = dir.path().join("TASK-NO-AUTO-CONT.jsonl");
        let deadline = Instant::now() + Duration::from_secs(2);
        while Instant::now() < deadline {
            if has_release_reason(&path, "protocol_end_without_finalize") {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        assert_release_reason(&path, "protocol_end_without_finalize");
        let release = session_events(&path)
            .into_iter()
            .rev()
            .find(|envelope| {
                envelope.kind == SessionEventKind::Lifecycle
                    && envelope.event.get("phase").and_then(|p| p.as_str()) == Some("release")
            })
            .expect("release tombstone");
        assert_eq!(
            release.event.get("outcome").and_then(|v| v.as_str()),
            Some("failed")
        );
        assert!(
            !session_events(&path).iter().any(|envelope| {
                envelope.kind == SessionEventKind::Lifecycle
                    && envelope.event.get("phase").and_then(|p| p.as_str()) == Some("continuation")
            }),
            "failed runs must not emit Lifecycle::Continuation"
        );
        let snapshot = sup.snapshot().await;
        assert!(
            snapshot.runs.is_empty(),
            "failed protocol-end must leave no live runs (got {:?})",
            snapshot.runs.iter().map(|r| &r.run_id).collect::<Vec<_>>()
        );
        assert!(snapshot.runs.iter().all(|run| run.run_id != resp.run_id));
    }

    #[tokio::test]
    async fn timeout_failure_does_not_spawn_continuation_run() {
        // orgasmic:TASK-QPKCD
        let (sup, dir, _w) = make_supervisor();
        let driver = AcceptingInputDriver;
        let mut req = dispatch_impl_req("TASK-TIMEOUT-NO-CONT", dir.path());
        req.stall_timeout_secs = Some(1);
        let path = req.session_path.clone();
        let resp = sup.acquire(&driver, req).await.unwrap();
        age_run(&sup, &resp.run_id, Some(Duration::from_millis(1_001)), None).await;
        sup.release_first_timed_out_run().await;
        assert_release_reason(&path, "stall_timeout_exceeded");
        let release = session_events(&path)
            .into_iter()
            .rev()
            .find(|envelope| {
                envelope.kind == SessionEventKind::Lifecycle
                    && envelope.event.get("phase").and_then(|p| p.as_str()) == Some("release")
            })
            .expect("timeout release tombstone");
        assert_eq!(
            release.event.get("outcome").and_then(|v| v.as_str()),
            Some("failed")
        );
        assert!(
            !session_events(&path).iter().any(|envelope| {
                envelope.kind == SessionEventKind::Lifecycle
                    && envelope.event.get("phase").and_then(|p| p.as_str()) == Some("continuation")
            }),
            "timeout must not emit Lifecycle::Continuation"
        );
        let snapshot = sup.snapshot().await;
        assert!(
            snapshot.runs.is_empty(),
            "timeout must leave no live/spawned runs"
        );
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
                &test_babysitter_auto_spawn(),
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
                &test_babysitter_auto_spawn(),
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
        let (sup, dir, _w, events) = make_supervisor_with_events();
        let driver = TmuxTuiDriver;
        let mut req = impl_req("TASK-BS-LIVE", dir.path());
        req.babysitter = Some(test_babysitter_auto_spawn());
        // Subscribed before anything is driven: the writer publishes its
        // append signal from another task, so a subscription taken after the
        // flush would be waiting for an event that has already been sent.
        let mut appends = events.subscribe();
        let impl_run = sup.acquire(&driver, req).await.unwrap();
        let bs_path = dir
            .path()
            .join(format!("{}.babysitter.jsonl", impl_run.run_id));
        // Nothing can flush to a babysitter that never spawned. Fail on that
        // here rather than letting it become a second, silent meaning for the
        // wait below (TASK-5FEN5).
        assert!(
            sup.snapshot().await.runs.iter().any(|run| {
                run.run_kind == RunKind::Babysitter
                    && run.babysitter_target.as_deref() == Some(impl_run.run_id.as_str())
            }),
            "acquire must auto-spawn a babysitter for {} before a threshold flush can land",
            impl_run.run_id
        );

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

        // The transitions reach the babysitter's JSONL through the run's event
        // drain and then the writer task, neither of which has run when
        // `transition_state` returns. Wait on the writer's own completion
        // signal — the `RunEvent` it publishes *after* an append lands — not
        // on a budget handed to that background work: under full-suite load
        // the budget is what expires, never the flush (TASK-5FEN5). The outer
        // timeout is a hang guard, not a work budget; it is only reached when
        // the summary never lands at all.
        let flushed = tokio::time::timeout(Duration::from_secs(30), async {
            loop {
                let env = orgasmic_core::read_session_file(&bs_path).unwrap();
                if env
                    .iter()
                    .any(|e| e.kind == SessionEventKind::BabysitterSummary)
                {
                    return;
                }
                match appends.recv().await {
                    Ok(_) => {}
                    // A slow subscriber loses the oldest events; the file
                    // re-read above, not the event payload, is the state that
                    // decides this test.
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {}
                    Err(tokio::sync::broadcast::error::RecvError::Closed) => {
                        panic!("writer event bus closed before the babysitter summary landed")
                    }
                }
            }
        })
        .await;
        assert!(
            flushed.is_ok(),
            "live summary threshold should append BabysitterSummary to {}",
            bs_path.display()
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
            applicable_states: Vec::new(),
            max_iterations: None,
            planned_identity: None,
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
                &test_babysitter_auto_spawn(),
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

    /// Renamed from `acp_stdio_acquire_auto_spawns_babysitter_jsonl`
    /// (TASK-3NJ9K). What it proves is that a non-mux worker acquire
    /// auto-spawns the babysitter JSONL; the acp-stdio address it used to name
    /// was incidental to that, and on a host with `codex` installed it made
    /// this unit test spawn `codex app-server`.
    #[tokio::test]
    async fn worker_acquire_auto_spawns_babysitter_jsonl() {
        let (sup, dir, _w) = make_supervisor();
        let driver = stub_driver();
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
            driver_config: stub_config(),
            babysitter_target: None,
            stall_timeout_secs: None,
            max_run_duration_secs: None,
            idle_timeout_secs: None,
            babysitter: Some(BabysitterAutoSpawn {
                worker_id: "babysitter-stall-detector".into(),
                mode: STUB_MODE.into(),
                harness: STUB_HARNESS.into(),
                driver_config: stub_config(),
                stall_timeout_secs: None,
                max_run_duration_secs: None,
                applicable_states: Vec::new(),
                linked_skills: Vec::new(),
                sandbox_permissions: None,
                max_iterations: None,
                context_budget_chars: None,
                harness_args: Vec::new(),
            }),
            applicable_states: Vec::new(),
            max_iterations: None,
            planned_identity: None,
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
        sup.inner.lock().await.insert_lease_for_test(
            lease_key(Some("orgasmic"), "TASK-SPIN", RunKind::Babysitter),
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
                    applicable_states: Vec::new(),
                    linked_skills: Vec::new(),
                    sandbox_permissions: None,
                    max_iterations: None,
                    context_budget_chars: None,
                    harness_args: Vec::new(),
                }),
                applicable_states: Vec::new(),
                max_iterations: None,
                planned_identity: None,
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
                    applicable_states: Vec::new(),
                    max_iterations: None,
                    planned_identity: None,
                },
            )
            .await
            .unwrap();

        let attempts_before = supervisor_metrics().babysitter_spawn_attempts;
        for idx in 0..BABYSITTER_AUTO_SPAWN_MAX_RETRIES {
            let mut req = impl_req(task_id, dir.path());
            req.session_path = dir.path().join(format!("recover-{idx}.jsonl"));
            req.babysitter = Some(test_babysitter_auto_spawn());
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
        req.babysitter = Some(test_babysitter_auto_spawn());
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
        let (sup, dir, _w) = make_supervisor();
        let driver = stub_driver();
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
            driver_config: stub_config(),
            babysitter_target: None,
            stall_timeout_secs: None,
            max_run_duration_secs: None,
            idle_timeout_secs: None,
            babysitter: Some(BabysitterAutoSpawn {
                worker_id: "babysitter-stall-detector".into(),
                mode: STUB_MODE.into(),
                harness: STUB_HARNESS.into(),
                driver_config: stub_config(),
                stall_timeout_secs: None,
                max_run_duration_secs: None,
                applicable_states: Vec::new(),
                linked_skills: Vec::new(),
                sandbox_permissions: None,
                max_iterations: None,
                context_budget_chars: None,
                harness_args: Vec::new(),
            }),
            applicable_states: Vec::new(),
            max_iterations: None,
            planned_identity: None,
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
        let (sup, dir, _w) = make_supervisor();
        let driver = stub_driver();
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
            driver_config: stub_config(),
            babysitter_target: None,
            stall_timeout_secs: None,
            max_run_duration_secs: None,
            idle_timeout_secs: None,
            babysitter: Some(BabysitterAutoSpawn {
                worker_id: "babysitter-stall-detector".into(),
                mode: STUB_MODE.into(),
                harness: STUB_HARNESS.into(),
                driver_config: stub_config(),
                stall_timeout_secs: None,
                max_run_duration_secs: None,
                applicable_states: Vec::new(),
                linked_skills: Vec::new(),
                sandbox_permissions: None,
                max_iterations: None,
                context_budget_chars: None,
                harness_args: Vec::new(),
            }),
            applicable_states: Vec::new(),
            max_iterations: None,
            planned_identity: None,
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
        let (sup, dir, _w) = make_supervisor();
        let driver = stub_driver();
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
            driver_config: stub_config(),
            babysitter_target: None,
            stall_timeout_secs: None,
            max_run_duration_secs: None,
            idle_timeout_secs: None,
            babysitter: Some(BabysitterAutoSpawn {
                worker_id: "babysitter-stall-detector".into(),
                mode: STUB_MODE.into(),
                harness: STUB_HARNESS.into(),
                driver_config: stub_config(),
                stall_timeout_secs: None,
                max_run_duration_secs: None,
                applicable_states: Vec::new(),
                linked_skills: Vec::new(),
                sandbox_permissions: None,
                max_iterations: None,
                context_budget_chars: None,
                harness_args: Vec::new(),
            }),
            applicable_states: Vec::new(),
            max_iterations: None,
            planned_identity: None,
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
        let ready = tmp.path().join("children-ready");
        // Put the wrapper in its own process group and null its stdio so the
        // backgrounded `sleep` children it spawns neither inherit the test
        // runner's stdout/stderr (which would hold a piped `cargo test | tail`
        // open past test completion) nor survive cleanup as orphans reparented
        // to init. The handle owns the whole group and reaps it on `Drop`
        // (orgasmic:task_BCYMM), so the `ready` deadline assertion below —
        // which is the one that fails under load (TASK-STWVB) — no longer
        // skips cleanup on its way out.
        let wrapper = crate::test_fixtures::spawn_in_own_process_group(
            Command::new(crate::test_fixtures::shared_test_executable())
                .args(["cursor-worker-sibling", ready.to_str().unwrap()])
                .stdin(Stdio::piped())
                .stdout(Stdio::null())
                .stderr(Stdio::null()),
            "fake cursor-agent",
        );
        let wrapper_pid = wrapper.id();
        let ready_deadline = Instant::now() + Duration::from_secs(30);
        while !ready.exists() {
            assert!(
                Instant::now() < ready_deadline,
                "fake cursor-agent did not start children"
            );
            tokio::time::sleep(Duration::from_millis(50)).await;
        }

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

        // The wrapper and its backgrounded sleeps are reaped as a group when
        // `wrapper` drops, on this path and on every panic path above it.
        drop(wrapper);
    }

    #[cfg(unix)]
    #[test]
    fn direct_child_pid_finds_wrapper_child_process() {
        // Null stdio + own process group so the backgrounded `sleep 300` cannot
        // inherit a piped `cargo test | tail` stdout (which would block on EOF)
        // and is reaped with the group rather than orphaned to init. The handle
        // reaps that group on `Drop` (orgasmic:task_BCYMM), including on the
        // `wrapper never forked a direct child` deadline panic below.
        let wrapper = crate::test_fixtures::spawn_in_own_process_group(
            Command::new("sh")
                .args(["-c", "sleep 300 & cat"])
                .stdin(Stdio::piped())
                .stdout(Stdio::null())
                .stderr(Stdio::null()),
            "wrapper",
        );
        let wrapper_pid = wrapper.id();
        // Wait for `sh` to actually fork `sleep 300` instead of assuming it has.
        // The shared state here is host scheduling: a fixed sleep asserts that
        // 500 concurrent tests plus whatever else the machine is running leave
        // `sh` enough CPU to fork within one fixed window, and on a loaded host
        // it does not (observed twice under a background `mediaanalysisd` spike).
        // Poll to a deadline, the same shape the wrapper-child sibling test
        // above already uses for its `ready` marker.
        let deadline = Instant::now() + Duration::from_secs(10);
        let child_pid = loop {
            if let Some(pid) = live_direct_child_pid(wrapper_pid) {
                break pid;
            }
            assert!(
                Instant::now() < deadline,
                "wrapper never forked a direct child"
            );
            std::thread::sleep(Duration::from_millis(25));
        };
        let output = Command::new("ps")
            .args(["-p", child_pid.to_string().as_str(), "-o", "command="])
            .output()
            .expect("ps child");
        let command = String::from_utf8_lossy(&output.stdout);
        assert!(
            command.contains("sleep 300"),
            "expected inner worker command, got {command:?}"
        );
        // The `cat` wrapper and its backgrounded `sleep 300` are reaped as a
        // group when `wrapper` drops, here and on every panic path above.
        drop(wrapper);
    }

    #[tokio::test]
    async fn early_exit_quiescence_requires_only_stream_end() {
        // orgasmic:task_3TEDA — PID observation must not gate release.
        let waiting_stream = RunRecord {
            stream_ended: false,
            early_exit_watcher_pid: Some(42),
            early_exit_pid_exited: true,
            ..test_run_record_shell()
        };
        assert!(!early_exit_quiescence_ready(&waiting_stream));

        let ready_without_pid_exit = RunRecord {
            driver_has_ready: true,
            driver_has_work: false,
            driver_has_terminal: false,
            stream_ended: true,
            early_exit_watcher_pid: Some(42),
            early_exit_pid_exited: false,
            ..test_run_record_shell()
        };
        assert!(early_exit_quiescence_ready(&ready_without_pid_exit));
    }

    #[tokio::test]
    async fn explicit_release_in_progress_blocks_stream_end_take() {
        let mut inner = Inner::new(CloseGuardStore::ephemeral());
        let run_id = "run-explicit-release".to_string();
        inner.runs.insert(
            run_id.clone(),
            RunRecord {
                stream_ended: true,
                explicit_release_in_progress: true,
                ..test_run_record_shell()
            },
        );
        assert!(take_stream_end_release(&mut inner, &run_id).is_none());
        assert!(inner.runs.contains_key(&run_id));
    }

    #[tokio::test]
    async fn stream_end_release_defers_until_channel_closure_when_pid_watched() {
        let mut inner = Inner::new(CloseGuardStore::ephemeral());
        let run_id = "run-quiescence".to_string();
        inner.runs.insert(
            run_id.clone(),
            RunRecord {
                driver_has_ready: true,
                driver_has_work: true,
                early_exit_watcher_pid: Some(99),
                early_exit_pid_exited: false,
                stream_ended: false,
                ..test_run_record_shell()
            },
        );
        assert!(take_stream_end_release(&mut inner, &run_id).is_none());
        inner.runs.get_mut(&run_id).unwrap().stream_ended = true;
        assert!(take_stream_end_release(&mut inner, &run_id).is_some());
        assert!(inner.runs.is_empty());
    }

    #[tokio::test]
    async fn watcher_abort_channel_closure_releases_pid_watched_no_work() {
        let (sup, dir, _w) = make_supervisor();
        let driver = PidBackedControllableDriver::new();
        let resp = sup
            .acquire(&driver, dispatch_impl_req("TASK-WATCHER-ABORT", dir.path()))
            .await
            .unwrap();
        wait_for_event_count(&sup, &resp.run_id, 1).await;
        {
            let g = sup.inner.lock().await;
            g.runs[&resp.run_id]
                .early_exit_watcher
                .as_ref()
                .expect("production watcher is owned")
                .abort();
        }
        let path = dir.path().join("TASK-WATCHER-ABORT.jsonl");
        driver.close_events().await;
        wait_for_run_release(&sup, &resp.run_id, Duration::from_secs(2)).await;
        assert_release(
            &path,
            "early-exit subprocess with no work envelopes",
            "failed",
        );
        assert_eq!(release_count(&path), 1);
        let event_count = session_events(&path).len();
        tokio::time::sleep(Duration::from_millis(300)).await;
        assert_eq!(
            session_events(&path).len(),
            event_count,
            "no post-release events"
        );
        let g = sup.inner.lock().await;
        assert!(!g.runs.contains_key(&resp.run_id));
        assert!(!g.holds_lease_for_run(&resp.run_id));
    }

    #[tokio::test]
    async fn closed_channel_with_live_reused_pid_still_releases_once() {
        let (sup, dir, _w) = make_supervisor();
        let driver = PidBackedControllableDriver::with_reported_pid(std::process::id());
        let resp = sup
            .acquire(&driver, dispatch_impl_req("TASK-PID-REUSE", dir.path()))
            .await
            .unwrap();
        wait_for_event_count(&sup, &resp.run_id, 1).await;
        driver.close_events().await;
        let path = dir.path().join("TASK-PID-REUSE.jsonl");
        wait_for_run_release(&sup, &resp.run_id, Duration::from_secs(2)).await;
        assert_release(
            &path,
            "early-exit subprocess with no work envelopes",
            "failed",
        );
        assert_eq!(release_count(&path), 1);
    }

    #[tokio::test]
    async fn dead_reported_pid_and_no_ready_converge_through_receiver_closure() {
        let (sup, dir, _w) = make_supervisor();
        let probe_error = PidBackedControllableDriver::with_reported_pid(u32::MAX);
        let probe = sup
            .acquire(
                &probe_error,
                dispatch_impl_req("TASK-PID-PROBE-ERROR", dir.path()),
            )
            .await
            .unwrap();
        wait_for_run_release(&sup, &probe.run_id, Duration::from_secs(2)).await;
        assert_eq!(
            release_count(&dir.path().join("TASK-PID-PROBE-ERROR.jsonl")),
            1
        );
        assert!(
            probe_error.event_tx.lock().await.is_none(),
            "dead PID observation must stop the retained producer and let receiver closure release"
        );

        let no_ready = PidBackedControllableDriver::without_ready();
        let no_ready_run = sup
            .acquire(&no_ready, dispatch_impl_req("TASK-NO-READY", dir.path()))
            .await
            .unwrap();
        no_ready.close_events().await;
        wait_for_run_release(&sup, &no_ready_run.run_id, Duration::from_secs(2)).await;
        assert_release(
            &dir.path().join("TASK-NO-READY.jsonl"),
            "protocol_end_without_finalize",
            "failed",
        );
    }

    #[tokio::test]
    async fn pid_exit_requests_producer_shutdown_and_receiver_releases() {
        let (sup, dir, _w) = make_supervisor();
        let driver = PidBackedControllableDriver::new();
        let resp = sup
            .acquire(&driver, dispatch_impl_req("TASK-PID-DEFER", dir.path()))
            .await
            .unwrap();
        let pid = resp.pid.expect("pid-backed acquire");
        wait_for_event_count(&sup, &resp.run_id, 1).await;
        assert!(run_is_live(&sup, &resp.run_id).await);
        let _ = Command::new("kill")
            .args(["-TERM", &pid.to_string()])
            .status();
        let path = dir.path().join("TASK-PID-DEFER.jsonl");
        wait_for_run_release(&sup, &resp.run_id, Duration::from_secs(2)).await;
        assert_release(
            &path,
            "early-exit subprocess with no work envelopes",
            "failed",
        );
        assert_eq!(release_count(&path), 1);
        assert!(
            driver.event_tx.lock().await.is_none(),
            "PID observation must request control shutdown so the retained sender closes"
        );
    }

    #[tokio::test]
    async fn timeout_stops_then_drains_terminal_event_racing_shutdown() {
        let (sup, dir, _w) = make_supervisor();
        let driver = QueuedBeforeTimeoutDriver::with_release_event(DriverEvent::RunComplete {
            summary: Some("completed while timeout stopped producer".into()),
        });
        let mut req = dispatch_impl_req("TASK-TIMEOUT-QUEUE", dir.path());
        req.stall_timeout_secs = Some(1);
        let session_path = req.session_path.clone();
        let resp = sup.acquire(&driver, req).await.unwrap();
        wait_for_event_count(&sup, &resp.run_id, 1).await;
        age_run(&sup, &resp.run_id, Some(Duration::from_millis(1_001)), None).await;
        sup.release_first_timed_out_run().await;
        wait_for_run_release(&sup, &resp.run_id, Duration::from_secs(2)).await;
        assert_release(&session_path, "stall_timeout_exceeded", "completed");
        assert_eq!(release_count(&session_path), 1);
    }

    #[tokio::test]
    async fn cleanup_reservation_blocks_same_path_for_different_task() {
        let (sup, dir, _w) = make_supervisor();
        let worktree = dir.path().join("wt-reservation-global");
        std::fs::create_dir_all(&worktree).unwrap();
        let sessions = dir.path().join("sessions");
        std::fs::create_dir_all(&sessions).unwrap();
        let params = DispatchCleanupParams {
            project_id: "project-a".into(),
            task_id: "TASK-CLEANUP-OWNER".into(),
            kind: RunKind::Worker,
            branch: "task-cleanup-owner-impl".into(),
            worktree_path: worktree.clone(),
            dispatch_attempt_token: Some("aaaa1111bbbb2222cccc3333dddd4444".into()),
            last_path: Some(dir.path().join("owner-last.txt")),
            stdout_path: Some(dir.path().join("owner-stdout.log")),
        };
        let DispatchCleanupOutcome::Proceed {
            cleanup_guard_id, ..
        } = sup
            .prepare_dispatch_cleanup(&sessions, &params)
            .await
            .unwrap()
        else {
            panic!("expected the cleanup to proceed");
        };

        let mut acquire = dispatch_impl_req("TASK-CLEANUP-OTHER", dir.path());
        acquire.project_id = Some("project-a".into());
        acquire.worktree = Some(worktree.clone());
        let err = sup.acquire(&tmux::driver(), acquire).await.unwrap_err();
        assert!(matches!(err, SupervisorError::CleanupInProgress { .. }));

        let mut other_params = params.clone();
        other_params.task_id = "TASK-CLEANUP-OTHER".into();
        assert_eq!(
            sup.prepare_dispatch_cleanup(&sessions, &other_params)
                .await
                .unwrap(),
            DispatchCleanupOutcome::Conflict
        );
        sup.finish_dispatch_cleanup(&cleanup_guard_id).await;
    }

    #[tokio::test]
    async fn cleanup_refuses_lease_to_run_record_gap() {
        let (sup, dir, _w) = make_supervisor();
        let worktree = dir.path().join("wt-acquire-gap");
        std::fs::create_dir_all(&worktree).unwrap();
        let sessions = dir.path().join("sessions");
        std::fs::create_dir_all(&sessions).unwrap();
        let params = DispatchCleanupParams {
            project_id: "project-a".into(),
            task_id: "TASK-ACQUIRE-GAP".into(),
            kind: RunKind::Worker,
            branch: "task-acquire-gap-impl".into(),
            worktree_path: worktree,
            dispatch_attempt_token: Some("bbbb1111cccc2222dddd3333eeee4444".into()),
            last_path: Some(dir.path().join("gap-last.txt")),
            stdout_path: Some(dir.path().join("gap-stdout.log")),
        };
        sup.inner.lock().await.insert_lease_for_test(
            lease_key(Some("project-a"), "TASK-ACQUIRE-GAP", RunKind::Worker),
            "run-in-acquire-gap".into(),
        );

        assert_eq!(
            sup.prepare_dispatch_cleanup(&sessions, &params)
                .await
                .unwrap(),
            DispatchCleanupOutcome::Conflict
        );
    }

    /// TASK-95SGV: the release must not depend on recomputing the worktree key
    /// from a path that no longer exists. On macOS every temp path lives under
    /// the `/var` → `/private/var` symlink; canonicalization resolves it only
    /// while the directory is present, so a key recomputed after removal
    /// differs from the installed one, the lookup misses, and the reservation
    /// leaks — refusing the task's deterministic worktree path forever.
    #[cfg(unix)]
    #[tokio::test]
    async fn finish_dispatch_cleanup_releases_even_after_the_worktree_is_removed() {
        let (sup, dir, _w) = make_supervisor();
        // A worktree reached through a symlinked prefix, the macOS layout.
        let real_parent = dir.path().join("real");
        std::fs::create_dir_all(real_parent.join("wt-95sgv")).unwrap();
        std::os::unix::fs::symlink(&real_parent, dir.path().join("link")).unwrap();
        let worktree = dir.path().join("link/wt-95sgv");
        let sessions = dir.path().join("sessions");
        std::fs::create_dir_all(&sessions).unwrap();
        let params = DispatchCleanupParams {
            project_id: "project-a".into(),
            task_id: "TASK-95SGV-REPRO".into(),
            kind: RunKind::Worker,
            branch: "task-95sgv-repro-impl".into(),
            worktree_path: worktree.clone(),
            dispatch_attempt_token: Some("cccc1111dddd2222eeee3333ffff4444".into()),
            last_path: None,
            stdout_path: None,
        };
        let DispatchCleanupOutcome::Proceed {
            cleanup_guard_id, ..
        } = sup
            .prepare_dispatch_cleanup(&sessions, &params)
            .await
            .unwrap()
        else {
            panic!("expected the cleanup to proceed");
        };

        // Cleanup removes the directory before the release runs.
        std::fs::remove_dir_all(real_parent.join("wt-95sgv")).unwrap();
        sup.finish_dispatch_cleanup(&cleanup_guard_id).await;

        assert!(
            sup.inner.lock().await.cleanup_reservations.is_empty(),
            "the reservation must be released even though the worktree directory is gone"
        );

        // The leak is not inert: dispatch recreates the deterministic worktree
        // path, and from then on every acquire is refused.
        std::fs::create_dir_all(real_parent.join("wt-95sgv")).unwrap();
        sup.acquire(
            &AlwaysAttachableDriver,
            worktree_req("TASK-95SGV-REPRO", dir.path(), &worktree),
        )
        .await
        .expect("the worktree must be acquirable again after cleanup finished");
    }

    /// TASK-95SGV asks 2 and 3: a reservation whose recorded owner is dead is
    /// swept on the next admission instead of refusing forever, and while the
    /// owner is alive the refusal names the reservation, its owner pid, and
    /// that the pid is alive.
    #[cfg(unix)]
    #[tokio::test]
    async fn a_dead_owner_cleanup_reservation_is_swept_and_a_refusal_names_its_holder() {
        let (sup, dir, _w) = make_supervisor();
        let worktree = dir.path().join("wt-95sgv-owner");
        std::fs::create_dir_all(&worktree).unwrap();
        let sessions = dir.path().join("sessions");
        std::fs::create_dir_all(&sessions).unwrap();
        let params = DispatchCleanupParams {
            project_id: "project-a".into(),
            task_id: "TASK-95SGV-OWNER".into(),
            kind: RunKind::Worker,
            branch: "task-95sgv-owner-impl".into(),
            worktree_path: worktree.clone(),
            dispatch_attempt_token: Some("dddd1111eeee2222ffff3333aaaa4444".into()),
            last_path: None,
            stdout_path: None,
        };
        let DispatchCleanupOutcome::Proceed {
            cleanup_guard_id, ..
        } = sup
            .prepare_dispatch_cleanup(&sessions, &params)
            .await
            .unwrap()
        else {
            panic!("expected the cleanup to proceed");
        };

        // While the owner (this daemon process) is alive, the refusal is
        // diagnosable: it names the guard, the pid, and that the pid is alive.
        let refused = sup
            .acquire(
                &AlwaysAttachableDriver,
                worktree_req("TASK-95SGV-OWNER", dir.path(), &worktree),
            )
            .await
            .expect_err("a held reservation with a live owner must refuse the acquire");
        let SupervisorError::CleanupInProgress { holder, .. } = &refused else {
            panic!("expected CleanupInProgress, got {refused:?}");
        };
        assert_eq!(holder.guard_id.as_deref(), Some(cleanup_guard_id.as_str()));
        assert_eq!(holder.owner_pid, Some(std::process::id()));
        assert_eq!(holder.owner_alive, Some(true));
        let message = refused.to_string();
        assert!(
            message.contains(&cleanup_guard_id)
                && message.contains(&format!("owner_pid={}", std::process::id()))
                && message.contains("(owner alive)"),
            "the refusal must name the reservation and its owner, got: {message}"
        );

        // The owner dies (simulated by rewriting the recorded pid to a reaped
        // child's); the next admission sweeps the reservation instead of
        // refusing forever.
        let mut child = std::process::Command::new("true")
            .spawn()
            .expect("spawn a throwaway owner");
        let dead_pid = child.id();
        child.wait().expect("reap the throwaway owner");
        sup.set_cleanup_owner_pid_for_test(&cleanup_guard_id, dead_pid)
            .await;
        sup.acquire(
            &AlwaysAttachableDriver,
            worktree_req("TASK-95SGV-OWNER", dir.path(), &worktree),
        )
        .await
        .expect("a reservation whose owner is dead must be swept, not refused");
    }

    /// TASK-95SGV.1 reviewer gap 1: `prepare_dispatch_cleanup` installs a
    /// cleanup reservation, then calls `release_dispatch_worker_for_cleanup`.
    /// When the matching live run is already being released by another
    /// authority (`ReleaseInProgress` — a production-reachable edge, since a
    /// concurrent release/early-exit can land between cleanup's liveness check
    /// and its `release` call), the `?` used to propagate that error without
    /// calling `finish_dispatch_cleanup`, stranding the daemon-owned
    /// reservation until restart and refusing the task's deterministic
    /// worktree path forever.
    ///
    /// This reproduces the bounded mid-release state the way TASK-RB1ZN's
    /// wedge test does — `StraySenderDriver` parks its producer so the release
    /// sits in the drain — then invokes cleanup against the wedged run and
    /// proves the reservation is cleared on the refusal, the first release
    /// still finishes, and the worktree is acquirable again.
    #[tokio::test]
    async fn a_mid_release_cleanup_refusal_releases_the_reservation_without_stranding() {
        let (sup, dir, _w) = make_supervisor();
        sup.set_release_drain_budget(Duration::from_secs(5));
        sup.set_driver_release_timeout(Duration::from_millis(150));
        let worktree = dir.path().join("wt-95sgv-midrelease");
        std::fs::create_dir_all(&worktree).unwrap();
        let sessions = dir.path().join("sessions");
        std::fs::create_dir_all(&sessions).unwrap();

        let task_id = "TASK-95SGV-MIDRELEASE";
        let last_path = dir.path().join("midrelease-last.txt");
        let stdout_path = dir.path().join("midrelease-stdout.log");
        std::fs::write(&last_path, "unfinished\n").unwrap();
        std::fs::write(&stdout_path, "unfinished\n").unwrap();
        let attempt_token = "eeee1111ffff2222aaaa3333bbbb4444";
        let mut req = dispatch_impl_req(task_id, dir.path());
        req.worktree = Some(worktree.clone());
        req.last_path = Some(last_path.clone());
        req.stdout_path = Some(stdout_path.clone());
        req.dispatch_attempt_token = Some(attempt_token.into());
        let session_path = req.session_path.clone();
        let resp = sup
            .acquire(&StraySenderDriver::new(false), req)
            .await
            .expect("acquire the run that will be wedged mid-release");
        wait_for_event_count(&sup, &resp.run_id, 1).await;

        // Wedge the run mid-release: `explicit_release_in_progress` is set under
        // the lock and the drain blocks on the parked producer.
        let wedged = tokio::spawn({
            let sup = sup.clone();
            let run_id = resp.run_id.clone();
            async move {
                sup.release_with_finalization(
                    &run_id,
                    "worker finalize for TASK-95SGV-MIDRELEASE",
                    ReleaseOutcome::Completed,
                    true,
                    None,
                )
                .await
            }
        });
        wait_for_finalize_admission(&session_path).await;
        assert!(
            run_is_live(&sup, &resp.run_id).await,
            "the run must still be present and mid-release for the refusal to be \
             ReleaseInProgress"
        );

        // Cleanup against the wedged run: identity matches, so the only thing
        // that can stop `release_dispatch_worker_for_cleanup` is the in-flight
        // release. The reservation was installed; the refusal must NOT strand.
        let params = DispatchCleanupParams {
            project_id: "orgasmic".into(),
            task_id: task_id.into(),
            kind: RunKind::Worker,
            branch: "task-95sgv-midrelease-impl".into(),
            worktree_path: worktree.clone(),
            dispatch_attempt_token: Some(attempt_token.into()),
            last_path: Some(last_path),
            stdout_path: Some(stdout_path),
        };
        let err = sup
            .prepare_dispatch_cleanup(&sessions, &params)
            .await
            .expect_err("a cleanup against a run being released must refuse, not proceed");
        assert!(
            matches!(err, SupervisorError::ReleaseInProgress(ref id) if *id == resp.run_id),
            "the refusal must be the in-flight release naming the run, got {err:?}"
        );
        assert!(
            sup.inner.lock().await.cleanup_reservations.is_empty(),
            "the cleanup reservation must be released on the refusal, not stranded"
        );

        // The first release still finishes within its drain budget.
        wedged
            .await
            .expect("the wedged release task")
            .expect("the wedged release completes within its drain budget");
        assert!(!run_is_live(&sup, &resp.run_id).await);

        // And the worktree is acquirable again — the task's deterministic path
        // is not permanently refused, which is the whole point of releasing the
        // reservation on the refusal.
        sup.acquire(
            &AlwaysAttachableDriver,
            worktree_req(task_id, dir.path(), &worktree),
        )
        .await
        .expect("the worktree must be acquirable again once the release drains");
    }

    #[tokio::test]
    async fn timeout_aborts_joins_hung_producer_then_drains_racing_terminal() {
        let (sup, dir, _w) = make_supervisor();
        // orgasmic:TASK-J1XCB — the same two unavoidable `DRIVER_RELEASE_TIMEOUT`
        // waits its dead-pid sibling below pays, for the same reason, and the
        // same compression. This one asserts no wall clock, so it was never
        // flaky — it just cost the suite 10s per run.
        sup.set_driver_release_timeout(Duration::from_millis(150));
        let driver = HungProducerDriver::new(false);
        let mut req = dispatch_impl_req("TASK-TIMEOUT-HUNG", dir.path());
        req.stall_timeout_secs = Some(1);
        let session_path = req.session_path.clone();
        let resp = sup.acquire(&driver, req).await.unwrap();
        wait_for_event_count(&sup, &resp.run_id, 1).await;
        age_run(&sup, &resp.run_id, Some(Duration::from_millis(1_001)), None).await;
        sup.release_first_timed_out_run().await;
        assert!(!run_is_live(&sup, &resp.run_id).await);
        assert!(driver.producer_dropped.load(Ordering::SeqCst));
        assert_release(&session_path, "stall_timeout_exceeded", "completed");
        assert_eq!(release_count(&session_path), 1);
    }

    /// orgasmic:TASK-J1XCB — the injected budget is what keeps this test off
    /// the wall clock.
    ///
    /// `HungProducerDriver` hangs in *both* halves of the driver stop:
    /// `HungReleaseControl::release` is `pending()` forever and the producer
    /// parks for 6s. `stop_and_join_driver_producer` therefore spends
    /// `DRIVER_RELEASE_TIMEOUT` twice before the abort that closes the channel,
    /// so pre-TASK-J1XCB this test took a measured 10.08s on an idle machine
    /// against a 12s assertion — a 1.19x margin that a loaded machine's late
    /// timers ate, and `run <id> did not release within 12s` was the third most
    /// common failure in this repo's gate runs. Both hangs are the point: they
    /// are the only way to reach the abort path this test is named after.
    /// Compressing the budget keeps every one of those lines executed and makes
    /// the whole test cost ~0.3s, so the 12s bound stops being a race and goes
    /// back to being what it was written as — a hang detector.
    #[tokio::test]
    async fn dead_pid_aborts_joins_hung_producer_then_receiver_releases() {
        let (sup, dir, _w) = make_supervisor();
        sup.set_driver_release_timeout(Duration::from_millis(150));
        let driver = HungProducerDriver::new(true);
        let req = impl_req("TASK-DEAD-PID-HUNG", dir.path());
        let session_path = req.session_path.clone();
        let resp = sup.acquire(&driver, req).await.unwrap();
        wait_for_run_release(&sup, &resp.run_id, Duration::from_secs(12)).await;
        assert!(driver.producer_dropped.load(Ordering::SeqCst));
        assert_release(&session_path, "driver stream closed", "completed");
        assert_eq!(release_count(&session_path), 1);
    }

    /// orgasmic:TASK-HAREX — the measured 2026-07-26 orphan, replayed.
    ///
    /// A dispatch worker on acp-stdio whose process is gone and whose driver
    /// left a sender behind. Before the drain gained a release-scoped bound,
    /// this run was unreleasable by anything short of restarting the daemon:
    /// the PID watcher fired and requested shutdown, the producer was aborted,
    /// and then `events.recv()` parked forever on the stray sender. The record
    /// stayed in `runs` — live in `GET /runs`, 404 from `POST
    /// /runs/:id/release` because `explicit_release_in_progress` was already
    /// set, and invisible to `timed_out_run`.
    ///
    /// The budget is compressed to 300ms; production spends
    /// `RELEASE_DRAIN_BUDGET` (20s), which no test can afford to sit through.
    /// That is the point of `set_release_drain_budget`, and it is the same
    /// window `ShutdownBudgets::release_drain` carries.
    #[tokio::test]
    async fn a_dead_worker_whose_stream_never_ends_is_still_released_as_orphaned() {
        let (sup, dir, _w) = make_supervisor();
        sup.set_release_drain_budget(Duration::from_millis(300));
        let driver = StraySenderDriver::new(true);
        let req = dispatch_impl_req("TASK-HAREX-DEAD", dir.path());
        let session_path = req.session_path.clone();
        let resp = sup.acquire(&driver, req).await.unwrap();
        wait_for_run_release(&sup, &resp.run_id, Duration::from_secs(10)).await;
        // Failed + protocol_end_without_finalize is what `record_dispatch_orphaned`
        // reads to write `manager.dispatch_orphaned`; a Completed or
        // finalized-by-worker tombstone here would release the lease and still
        // leave the manager with nothing to rescue.
        assert_release(&session_path, "protocol_end_without_finalize", "failed");
        assert_eq!(release_count(&session_path), 1);
    }

    /// orgasmic:TASK-HAREX — the same wedge reached through worker finalize.
    ///
    /// `release_one` awaits the drain before it removes the record and appends
    /// the tombstone, so a drain that never ends is a `dispatch finalize` that
    /// never returns and a run that is never released. The timeout below is
    /// the assertion: without the bound this await does not complete at all.
    #[tokio::test]
    async fn a_worker_finalize_completes_even_when_the_drain_never_ends() {
        let (sup, dir, _w) = make_supervisor();
        sup.set_release_drain_budget(Duration::from_millis(300));
        let driver = StraySenderDriver::new(false);
        let req = dispatch_impl_req("TASK-HAREX-FINALIZE", dir.path());
        let session_path = req.session_path.clone();
        let resp = sup.acquire(&driver, req).await.unwrap();
        wait_for_event_count(&sup, &resp.run_id, 1).await;
        tokio::time::timeout(
            Duration::from_secs(10),
            sup.release_with_finalization(
                &resp.run_id,
                "worker finalize for TASK-HAREX-FINALIZE",
                ReleaseOutcome::Completed,
                true,
                None,
            ),
        )
        .await
        .expect("worker finalize must not hang on a drain that never ends")
        .expect("worker finalize release");
        assert!(!run_is_live(&sup, &resp.run_id).await);
        assert_release(
            &session_path,
            "worker finalize for TASK-HAREX-FINALIZE",
            "completed",
        );
        assert_eq!(release_finalize_flags(&session_path), vec![true]);
    }

    /// Wait for the finalize-admission marker (TASK-QSSQH) to reach `path`.
    ///
    /// orgasmic:TASK-RB1ZN — the wedge detector, and deliberately not a sleep.
    /// `release_one` appends this marker immediately after it sets
    /// `explicit_release_in_progress`, under the lock, and before the teardown
    /// that follows, so its arrival is proof that the record is present AND
    /// already being released — the exact state the split is about. Polling with
    /// a second release call instead would race the first for admission and
    /// could win it.
    async fn wait_for_finalize_admission(path: &Path) {
        let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
        loop {
            if admission_marker_count(path) > 0 {
                return;
            }
            assert!(
                tokio::time::Instant::now() < deadline,
                "finalize admission marker never landed in {}",
                path.display()
            );
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    }

    /// orgasmic:TASK-RB1ZN — the collapse, on the wedge that produces it.
    ///
    /// One run, two refusals that used to be the same error. While the release
    /// is wedged in TASK-HAREX's bounded drain the record is still in `runs`, so
    /// `/runs/live` reports it; a second release then has to say "live, and
    /// already being released" (409 at both surfaces) rather than "not found"
    /// (404), which is what made the 2026-07-26 incident unreadable from its own
    /// error text. After the drain's budget expires and the record is gone, the
    /// same call must go back to the plain `RunNotFound` the CLI's
    /// already-released rescue branch keys on — both halves asserted here,
    /// against one run, in order.
    ///
    /// `StraySenderDriver` is what makes the wedge real rather than simulated:
    /// its parked sender clone means `events.recv()` never yields `None`, so the
    /// release sits in the drain for the whole budget. Compressed the way
    /// HAREX's own replays compress it — 1.5s of wedge against a window the
    /// assertions cross in microseconds, and the test costs one wedge.
    #[tokio::test]
    async fn a_run_wedged_mid_release_says_so_instead_of_run_not_found() {
        let (sup, dir, _w) = make_supervisor();
        sup.set_release_drain_budget(Duration::from_millis(1_500));
        // The producer this driver hands over parks forever, so the join ahead
        // of the drain otherwise spends a full production release timeout.
        sup.set_driver_release_timeout(Duration::from_millis(150));
        let driver = StraySenderDriver::new(false);
        let req = dispatch_impl_req("TASK-RB1ZN-WEDGE", dir.path());
        let session_path = req.session_path.clone();
        let resp = sup.acquire(&driver, req).await.unwrap();
        wait_for_event_count(&sup, &resp.run_id, 1).await;

        let wedged = tokio::spawn({
            let sup = sup.clone();
            let run_id = resp.run_id.clone();
            async move {
                sup.release_with_finalization(
                    &run_id,
                    "worker finalize for TASK-RB1ZN-WEDGE",
                    ReleaseOutcome::Completed,
                    true,
                    None,
                )
                .await
            }
        });
        wait_for_finalize_admission(&session_path).await;

        let err = sup
            .release(&resp.run_id, "manager cancel", ReleaseOutcome::Cancelled)
            .await
            .expect_err("a second release cannot succeed while the first holds admission");
        assert!(
            matches!(err, SupervisorError::ReleaseInProgress(ref id) if *id == resp.run_id),
            "a record that is present with a release running is the opposite of \
             absent, got {err:?}"
        );
        assert!(
            run_is_live(&sup, &resp.run_id).await,
            "the two views must agree: the refusal above says this run is live, so \
             the snapshot every liveness surface reads must still report it"
        );

        wedged
            .await
            .expect("the wedged release task")
            .expect("the wedged release still completes within its drain budget");

        assert!(!run_is_live(&sup, &resp.run_id).await);
        let err = sup
            .release(&resp.run_id, "manager cancel", ReleaseOutcome::Cancelled)
            .await
            .expect_err("the record is gone");
        assert!(
            matches!(err, SupervisorError::RunNotFound(ref id) if *id == resp.run_id),
            "a genuinely absent record must keep the plain RunNotFound the CLI's \
             already-released branch keys on, got {err:?}"
        );
        assert_eq!(release_count(&session_path), 1);
    }

    /// orgasmic:TASK-HAREX — the false positive this bound must not have.
    ///
    /// Dispatch sessions here routinely go silent for ten or twenty minutes
    /// while cargo builds. The drain bound arms on a *requested release*, never
    /// on silence, so a healthy quiet worker is untouched by it — this run sits
    /// through many multiples of its own budget with a live driver and stays
    /// exactly where it is.
    #[tokio::test]
    async fn a_quiet_healthy_worker_is_never_ended_by_the_release_drain_budget() {
        let (sup, dir, _w) = make_supervisor();
        sup.set_release_drain_budget(Duration::from_millis(50));
        let driver = StraySenderDriver::new(false);
        let req = dispatch_impl_req("TASK-HAREX-QUIET", dir.path());
        let session_path = req.session_path.clone();
        let resp = sup.acquire(&driver, req).await.unwrap();
        wait_for_event_count(&sup, &resp.run_id, 1).await;
        tokio::time::sleep(Duration::from_millis(500)).await;
        assert!(
            run_is_live(&sup, &resp.run_id).await,
            "a quiet worker with no release requested must stay live"
        );
        assert_eq!(release_count(&session_path), 0);
    }

    /// orgasmic:TASK-HAREX — the gate's own contract, on the production window.
    ///
    /// Paused time, so this asserts against `RELEASE_DRAIN_BUDGET` itself
    /// rather than a compressed stand-in: an hour of silence does not end an
    /// unarmed gate, and one budget of silence after arming does.
    #[tokio::test(start_paused = true)]
    async fn the_drain_gate_bounds_only_after_a_release_is_requested() {
        let (_tx, mut rx) = tokio::sync::mpsc::channel::<DriverEvent>(4);
        let release_requested = Arc::new(tokio::sync::Notify::new());
        let mut gate = DrainGate::new(
            "run-harex-gate".into(),
            Arc::clone(&release_requested),
            RELEASE_DRAIN_BUDGET,
        );
        assert!(
            tokio::time::timeout(Duration::from_secs(3_600), gate.next(&mut rx))
                .await
                .is_err(),
            "an unarmed gate must outwait any amount of driver silence"
        );
        release_requested.notify_one();
        assert_eq!(
            tokio::time::timeout(RELEASE_DRAIN_BUDGET * 2, gate.next(&mut rx))
                .await
                .expect("gate must end within its budget once a release is requested"),
            None
        );
    }

    #[tokio::test]
    async fn delayed_queued_work_observed_before_stream_end_release() {
        let (sup, dir, _w) = make_supervisor();
        let driver = QueuedBeforeTimeoutDriver::new();
        let resp = sup
            .acquire(&driver, dispatch_impl_req("TASK-DELAYED-WORK", dir.path()))
            .await
            .unwrap();
        wait_for_event_count(&sup, &resp.run_id, 1).await;
        driver
            .inject(DriverEvent::TextChunk {
                stream: TextStream::Assistant,
                chunk: "late work".into(),
                seq: 1,
            })
            .await;
        tokio::time::sleep(Duration::from_millis(50)).await;
        driver.close_events().await;
        let path = dir.path().join("TASK-DELAYED-WORK.jsonl");
        wait_for_run_release(&sup, &resp.run_id, Duration::from_secs(2)).await;
        assert_release(&path, "protocol_end_without_finalize", "failed");
        assert!(
            session_has_work_envelope(&session_events(&path)),
            "queued work must land before stream-end classification"
        );
    }

    /// `finalized_by_worker` flag of every `Lifecycle::Release` in the session.
    fn release_finalize_flags(path: &Path) -> Vec<bool> {
        session_events(path)
            .into_iter()
            .filter_map(|envelope| {
                if envelope.kind != SessionEventKind::Lifecycle {
                    return None;
                }
                match serde_json::from_value::<Lifecycle>(envelope.event) {
                    Ok(Lifecycle::Release {
                        finalized_by_worker,
                        ..
                    }) => Some(finalized_by_worker),
                    _ => None,
                }
            })
            .collect()
    }

    fn admission_marker_count(path: &Path) -> usize {
        session_events(path)
            .iter()
            .filter(|envelope| {
                envelope.kind == SessionEventKind::Note
                    && is_worker_finalize_admitted_note(&envelope.event)
            })
            .count()
    }

    /// Drive one worker finalize whose teardown drains `terminal_event`, and
    /// return the session path (orgasmic:TASK-QSSQH).
    ///
    /// `QueuedBeforeTimeoutDriver::with_release_event` emits from inside
    /// `control.release()`, which is the production teardown window: it is
    /// where the reaped harness's own terminal event lands, and equally where
    /// an event the harness had already queued when the finalize was admitted
    /// is drained. Deterministic, unlike injecting and hoping the drain has
    /// not run yet.
    async fn finalize_with_teardown_event(
        task: &str,
        terminal_event: DriverEvent,
    ) -> (tempfile::TempDir, PathBuf) {
        let (sup, dir, _w) = make_supervisor();
        let driver = QueuedBeforeTimeoutDriver::with_release_event(terminal_event);
        let req = dispatch_impl_req(task, dir.path());
        let session_path = req.session_path.clone();
        let resp = sup.acquire(&driver, req).await.unwrap();
        wait_for_event_count(&sup, &resp.run_id, 1).await;
        sup.release_with_finalization(
            &resp.run_id,
            &format!("worker finalize for {task}"),
            ReleaseOutcome::Completed,
            true,
            None,
        )
        .await
        .unwrap();
        wait_for_run_release(&sup, &resp.run_id, Duration::from_secs(2)).await;
        (dir, session_path)
    }

    /// TASK-QSSQH, production path. A `RunFail` drained inside the finalize's
    /// own teardown window is still a genuine failure, and the stage it backs
    /// must be recorded failed — the defect TASK-C0XMR's dominance rule
    /// introduced, which the reviewer called worse than the bug it fixed.
    #[tokio::test]
    async fn worker_finalize_does_not_clear_a_run_fail_drained_during_teardown() {
        let (_dir, session_path) = finalize_with_teardown_event(
            "TASK-QSSQH-RUNFAIL",
            DriverEvent::RunFail {
                error_code: "claude_result_error".into(),
                error_markdown: "the worker's own turn failed".into(),
            },
        )
        .await;

        assert_eq!(
            release_outcomes(&session_path),
            vec![ReleaseOutcome::Failed]
        );
        assert_eq!(release_finalize_flags(&session_path), vec![true]);
        assert!(matches!(
            crate::api::stage_outcome_from_session(&session_path),
            crate::api::StageOutcome::Failed { .. }
        ));
    }

    /// TASK-QSSQH, production path, the twin. The original TASK-C0XMR symptom:
    /// the fatal driver error is synthesized BY this release's teardown (in
    /// production, by reaping the harness process group), so the stage still
    /// completes.
    ///
    /// Read against its twin above, this test is the answer to the reviewer's
    /// open question. The two runs write byte-identical releases —
    /// `outcome: failed, finalized_by_worker: true` — because
    /// `terminal_outcome_for_event` maps `RunFail` and fatal `DriverError`
    /// alike, and both drain before the release is appended. So the release
    /// event cannot be the finalize-admission boundary. What separates them is
    /// the event KIND (teardown never synthesizes `RunFail`) plus the durable
    /// `worker_finalize_admitted` marker the supervisor writes before teardown
    /// starts, asserted here to be exactly one and to precede the error.
    #[tokio::test]
    async fn worker_finalize_still_suppresses_the_error_its_own_teardown_caused() {
        let (_dir, session_path) = finalize_with_teardown_event(
            "TASK-QSSQH-TEARDOWN",
            DriverEvent::DriverError {
                fatal: true,
                message: "rmux pane exited by signal 15; equivalent shell exit code 143".into(),
            },
        )
        .await;

        // The same release shape as the RunFail twin: not a discriminator.
        assert_eq!(
            release_outcomes(&session_path),
            vec![ReleaseOutcome::Failed]
        );
        assert_eq!(release_finalize_flags(&session_path), vec![true]);

        assert_eq!(
            admission_marker_count(&session_path),
            1,
            "the finalize-admission boundary must be durable, and written once"
        );
        let events = session_events(&session_path);
        let marker_at = events
            .iter()
            .position(|envelope| {
                envelope.kind == SessionEventKind::Note
                    && is_worker_finalize_admitted_note(&envelope.event)
            })
            .expect("admission marker");
        let error_at = events
            .iter()
            .position(|envelope| {
                envelope.kind == SessionEventKind::DriverEvent
                    && envelope.event.get("type").and_then(|v| v.as_str()) == Some("driver_error")
            })
            .expect("teardown driver error");
        assert!(
            marker_at < error_at,
            "the marker must be on disk before teardown can produce anything"
        );

        assert!(matches!(
            crate::api::stage_outcome_from_session(&session_path),
            crate::api::StageOutcome::Completed
        ));
    }

    #[tokio::test]
    async fn fatal_driver_error_stream_end_releases_once() {
        let (sup, dir, _w) = make_supervisor();
        let driver = FatalDriverErrorDriver;
        let resp = sup
            .acquire(&driver, dispatch_impl_req("TASK-FATAL", dir.path()))
            .await
            .unwrap();
        let path = dir.path().join("TASK-FATAL.jsonl");
        wait_for_run_release(&sup, &resp.run_id, Duration::from_secs(2)).await;
        assert_release(&path, "protocol_end_without_finalize", "failed");
        assert_eq!(release_count(&path), 1);
        assert!(driver_event_count(&path) >= 2);
    }

    /// A declared failure must end the run on its own, without waiting for the
    /// harness to exit. TASK-TJKFC.
    ///
    /// Before this, `terminal_event_releases_transport` allowlisted only the
    /// mux transports, so on acp-stdio a `run_fail` set the outcome and
    /// released nothing. The real run's lease stayed held and its process
    /// stayed alive for seventy minutes, which `dispatch-status` reported as
    /// `[pid-alive]` — indistinguishable from work in progress.
    ///
    /// The two-second bound is the whole assertion: this driver's stream never
    /// ends, so before the fix the wait could only ever time out.
    #[tokio::test]
    async fn a_declared_failure_releases_without_waiting_for_the_harness_to_exit() {
        let (sup, dir, _w) = make_supervisor();
        let resp = sup
            .acquire(
                &FatalThenSilentDriver,
                dispatch_impl_req("TASK-STARTUP-FATAL", dir.path()),
            )
            .await
            .unwrap();
        let path = dir.path().join("TASK-STARTUP-FATAL.jsonl");

        wait_for_run_release(&sup, &resp.run_id, Duration::from_secs(2)).await;

        assert_release(&path, "protocol_end_without_finalize", "failed");
        assert_eq!(release_count(&path), 1, "exactly one release");
        // The lease must be free again, or the next dispatch for this task is
        // refused by a run that has already failed.
        assert!(
            !sup.snapshot()
                .await
                .runs
                .iter()
                .any(|run| run.run_id == resp.run_id),
            "the failed run must not still hold its lease"
        );
    }

    /// A failure declared before the supervisor finished its bookkeeping must
    /// still end the run. TASK-TJKFC's race case.
    ///
    /// The window is narrow and the consequence is not: driver events are
    /// applied through `runs.get_mut(run_id)`, which is a no-op when the record
    /// does not exist yet. A fatal that lands in that gap sets no
    /// `terminal_outcome`, so the terminal-event release never fires — and this
    /// driver's stream never ends either, so nothing else would rescue it. The
    /// run would sit holding its lease until the stall timeout, which is the
    /// seventy-minute orphan again by a different route.
    ///
    /// What keeps that from happening is an ordering, not a lock:
    /// `acquire_impl` inserts the run record *before* spawning the drain task,
    /// so no event can be processed against a record that does not exist. This
    /// test is what makes that ordering load-bearing rather than incidental —
    /// verified by mutation: with the insert deferred past the drain's first
    /// events, no release is ever written and `assert_release` fails. (It fails
    /// fast rather than timing out, because a run absent from the map reads as
    /// already released — which is itself the shape of the bug: an orphan is
    /// indistinguishable from a finished run when nobody recorded it.)
    #[tokio::test]
    async fn a_failure_declared_before_the_run_is_recorded_still_ends_the_run() {
        let (sup, dir, _w) = make_supervisor();
        let resp = sup
            .acquire(
                &FatalBeforeBookkeepingDriver,
                dispatch_impl_req("TASK-STARTUP-RACE", dir.path()),
            )
            .await
            .unwrap();
        let path = dir.path().join("TASK-STARTUP-RACE.jsonl");

        wait_for_run_release(&sup, &resp.run_id, Duration::from_secs(2)).await;

        assert_release(&path, "protocol_end_without_finalize", "failed");
        assert_eq!(release_count(&path), 1, "exactly one release");
        assert!(
            !sup.snapshot()
                .await
                .runs
                .iter()
                .any(|run| run.run_id == resp.run_id),
            "the failed run must not still hold its lease"
        );
        // The events queued before the record existed must still be on record.
        // Releasing the lease while losing the reason why is a quieter version
        // of the same defect: the operator gets a failed run with no evidence.
        let events = session_events(&path);
        assert!(
            events.iter().any(|evt| evt
                .event
                .to_string()
                .contains("claude authentication_failed")),
            "the fatal that arrived before bookkeeping must reach the session"
        );
    }

    /// The rmux session name the driver reported on its persisted `Ready`, once
    /// the supervisor has drained it. Fails rather than skips on a degraded
    /// acquisition: an inert `Ready` owns no session, so every reap assertion
    /// below would pass vacuously (TASK-R2HDN).
    async fn live_rmux_session_from_session_file(path: &Path, timeout: Duration) -> String {
        let start = Instant::now();
        loop {
            for envelope in session_events(path) {
                if envelope.kind != SessionEventKind::DriverEvent
                    || envelope.event.get("type").and_then(|ty| ty.as_str()) != Some("ready")
                {
                    continue;
                }
                let capabilities = &envelope.event["capabilities"];
                assert_not_degraded(
                    "supervisor_release_reserves_time_for_rmux_cli_fallback_after_stalled_sdk_kill",
                    capabilities["inert"] == true,
                );
                return capabilities["session"]
                    .as_str()
                    .expect("live rmux Ready reports a session")
                    .to_string();
            }
            assert!(
                start.elapsed() < timeout,
                "no rmux Ready reached {} within {timeout:?}",
                path.display()
            );
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    }

    // orgasmic:task_69CW6
    /// TASK-6FNAY's acceptance criterion, driven through the production path
    /// instead of a double: a real `RmuxControl` owning a real
    /// `rmux_sdk::Session`, released by `Supervisor::release` at the real
    /// `DRIVER_RELEASE_TIMEOUT` boundary, with the SDK's kill request stalled so
    /// the endpoint-exact CLI fallback is the only thing that can reap.
    ///
    /// The surrogate this replaces slept and then flipped its own
    /// `session_live` flag. Deleting the CLI fallback from
    /// `RmuxControl::release`, or pointing it at a different daemon, left that
    /// test green. Both turn this one red: the first because a real rmux
    /// session survives; the second because the recorded argv stops naming the
    /// session's own socket — and, on any host where a bare `rmux` resolves
    /// somewhere else, because the session survives too.
    #[tokio::test]
    async fn supervisor_release_reserves_time_for_rmux_cli_fallback_after_stalled_sdk_kill() {
        const TEST: &str =
            "supervisor_release_reserves_time_for_rmux_cli_fallback_after_stalled_sdk_kill";
        // Lock order is flock-then-environment: the fixture starts a real rmux
        // daemon and repoints process-global rmux discovery at it.
        let _live_guard = live_session_guard();
        let _environment = test_environment_lock().lock().await;
        if skip_test_if_missing(TEST, &[("rmux", probe_rmux_binary().usable())]) {
            return;
        }
        let endpoint = StallableRmuxEndpoint::start()
            .await
            .expect("private stallable rmux endpoint");

        let (sup, dir, _writer) = make_supervisor();
        let session_path = dir.path().join("TASK-6FNAY-REAP.jsonl");
        let driver = RmuxDriver::new(Box::new(ShellAdapter::new()));
        let mut req = impl_req("TASK-6FNAY-REAP", dir.path());
        req.driver_config = DriverConfig::from_value(json!({
            "command": "sh",
            "args": ["-c", "while :; do printf 'reap\\n'; sleep 0.05; done"],
        }));
        let resp = sup.acquire(&driver, req).await.unwrap();
        let session =
            live_rmux_session_from_session_file(&session_path, Duration::from_secs(30)).await;
        assert!(
            endpoint.session_exists(&session),
            "rmux session {session} was not live before release"
        );

        // From here the SDK's ordered transport answers nothing. Only the CLI
        // fallback can still reach the daemon.
        endpoint.stall_sdk_transport();
        let started = Instant::now();
        sup.release(
            &resp.run_id,
            "rmux reap regression",
            ReleaseOutcome::Completed,
        )
        .await
        .unwrap();
        let elapsed = started.elapsed();

        assert!(
            !endpoint.session_exists(&session),
            "rmux session {session} survived a supervisor release with a stalled SDK kill"
        );
        let kill_invocations = endpoint
            .recorded_cli_invocations()
            .into_iter()
            .filter(|argv| argv.iter().any(|arg| arg == "kill-session"))
            .collect::<Vec<_>>();
        let endpoint_exact = vec![
            "-S".to_string(),
            endpoint.endpoint_path().display().to_string(),
            "kill-session".to_string(),
            "-t".to_string(),
            session.clone(),
        ];
        assert!(
            kill_invocations.contains(&endpoint_exact),
            "the CLI fallback must address the session's own endpoint and name; recorded \
             {kill_invocations:?}, expected {endpoint_exact:?}"
        );
        assert!(
            session_has_terminal_event(&session_events(&session_path)),
            "release-owned RunComplete must be drained before lifecycle cleanup"
        );
        // The stall has to have been real: a fallback that ran without the SDK
        // consuming its budget would mean the kill request reached the daemon.
        assert!(
            elapsed >= Duration::from_secs(1),
            "release returned in {elapsed:?}; the SDK kill cannot have stalled"
        );
    }

    #[tokio::test]
    async fn rmux_reap_failure_is_logged_with_context_and_cleanup_remains_unconditional() {
        let (sup, dir, _writer) = make_supervisor();
        let producer_dropped = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let driver = FailingRmuxReapDriver {
            producer_dropped: Arc::clone(&producer_dropped),
        };
        let session_path = dir.path().join("TASK-6FNAY-REAP-FAIL.jsonl");
        let resp = sup
            .acquire(&driver, impl_req("TASK-6FNAY-REAP-FAIL", dir.path()))
            .await
            .unwrap();
        let log_bytes = Arc::new(std::sync::Mutex::new(Vec::new()));
        let subscriber = tracing_subscriber::fmt()
            .without_time()
            .with_ansi(false)
            .with_writer(CapturedLog(Arc::clone(&log_bytes)))
            .finish();
        let dispatch = tracing::Dispatch::new(subscriber);
        let _guard = tracing::dispatcher::set_default(&dispatch);

        sup.release(
            &resp.run_id,
            "rmux reap failure regression",
            ReleaseOutcome::Completed,
        )
        .await
        .unwrap();

        assert!(producer_dropped.load(Ordering::SeqCst));
        assert!(
            !sup.snapshot()
                .await
                .runs
                .iter()
                .any(|run| run.run_id == resp.run_id),
            "reap failure must not strand the run record"
        );
        assert_eq!(release_count(&session_path), 1);
        let log = String::from_utf8(log_bytes.lock().unwrap().clone()).unwrap();
        assert!(log.contains(&resp.run_id), "{log}");
        assert!(log.contains("transport=\"rmux\""), "{log}");
        assert!(
            log.contains("SDK stalled and exact-endpoint CLI fallback refused"),
            "{log}"
        );
    }

    fn test_run_record_shell() -> RunRecord {
        RunRecord {
            task_id: "TASK".into(),
            kind: RunKind::Worker,
            worker_id: "w".into(),
            role: "implementer".into(),
            transport: "subprocess".into(),
            harness: None,
            project_id: None,
            worktree: None,
            sub_state: None,
            identity: RuntimeIdentity::new("run-test", "boot-test"),
            session_path: PathBuf::from("/tmp/run.jsonl"),
            babysitter_target: None,
            last_path: None,
            stdout_path: None,
            dispatch_attempt_token: None,
            requires_worker_finalize: true,
            terminal_round: 0,
            terminal_declaration: None,
            artifactor_lifecycle: ArtifactorLifecycle::Idle,
            pending_terminal_drain: false,
            pending_cancel: false,
            babysitter_run_id: None,
            last_driver_event_at: Instant::now(),
            last_progress_at: Instant::now(),
            run_started_at: Instant::now(),
            last_input_at: Instant::now(),
            stall_timeout: None,
            max_run_duration: None,
            idle_timeout: None,
            applicable_states: Vec::new(),
            semantic_turn_count: 0,
            max_iterations: None,
            next_event_seq: 0,
            terminal_outcome: None,
            control: Box::new(NoopControl),
            producer: None,
            event_drain: tokio::spawn(async {}),
            babysitter_summary: None,
            early_exit_watcher_pid: None,
            early_exit_watcher: None,
            driver_has_work: false,
            driver_has_terminal: false,
            driver_has_ready: false,
            early_exit_release_taken: false,
            stream_ended: false,
            early_exit_pid_exited: false,
            explicit_release_in_progress: false,
            release_requested: Arc::new(tokio::sync::Notify::new()),
            terminal_event_shutdown_in_progress: false,
            pid_exit_shutdown_in_progress: false,
        }
    }

    struct NoopControl;

    #[async_trait::async_trait]
    impl DriverControl for NoopControl {
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
            Ok(BabysitterAck {
                accepted: true,
                message: None,
            })
        }

        async fn release(&mut self, _reason: &str) -> Result<(), DriverError> {
            Ok(())
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
    }
}
