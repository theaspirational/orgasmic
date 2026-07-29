// arch: arch_C87Z9.5, arch_R3EPE.1
//! Manager-owned dispatch helpers.
//!
//! This module intentionally keeps manager-side dispatch orchestration in the
//! CLI: worktree creation, daemon-mediated tx appends, lifecycle edits, and
//! close/status scans. Runtime acquisition goes through the daemon supervisor.

use std::collections::BTreeSet;
use std::fmt;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{self, AtomicBool};
use std::sync::Arc;
use std::time::Duration;

use anyhow::{bail, Context, Result};
use clap::{ArgAction, Args, ValueEnum};
use orgasmic_core::{
    dotorg_tasks_dir, goal_file_path, iter_task_file_paths, parse_tx_file, project_dispatch_dir,
    projects, read_session_file, Lifecycle, LifecycleStage, OrgFile, ProjectFile, RuntimeIdentity,
    SessionEventKind, TaskHeading, TxEntry,
};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::daemon_client::DaemonClient;
use crate::home::Home;

#[derive(Copy, Clone, Debug, Eq, PartialEq, ValueEnum)]
pub enum DispatchKind {
    Implementer,
    Reviewer,
    Architector,
}

impl DispatchKind {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::Implementer => "implementer",
            Self::Reviewer => "reviewer",
            Self::Architector => "architector",
        }
    }
}

impl fmt::Display for DispatchKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

#[derive(Args, Debug, Clone)]
#[command(after_help = "\
Examples:
  orgasmic manager dispatch --task TASK-053 --kind implementer \\
    --brief /path/to/brief.md --mode acp-stdio --harness cursor-agent

  orgasmic manager dispatch --task TASK-053 --kind implementer \\
    --brief /path/to/brief.md --mode rmux --harness custom \\
    --harness-arg opencode --harness-arg --print-logs --dry-run")]
pub struct DispatchArgs {
    /// Task id to dispatch, e.g. `TASK-XXXXX`; repeatable to send one worker
    /// at several tasks. The task must be in BACKLOG or TODO — a dispatch from
    /// any other lifecycle stage is refused by name.
    #[arg(long = "task", action = ArgAction::Append, required = true)]
    pub task: Vec<String>,
    /// Worker persona, which fixes the prompt spec the worker is compiled from.
    #[arg(long, value_enum)]
    pub kind: DispatchKind,
    /// PATH to a file holding the manager's handoff brief (not the brief text
    /// itself). Read at dispatch time and compiled into the worker prompt.
    #[arg(long)]
    pub brief: PathBuf,
    /// Transport mode from `orgasmic_drivers::SUPPORTED`.
    #[arg(long)]
    pub mode: String,
    /// Harness from `orgasmic_drivers::SUPPORTED`.
    #[arg(long)]
    pub harness: String,
    /// Raw argv token for custom harnesses (repeatable; preserved losslessly).
    #[arg(long = "harness-arg", action = ArgAction::Append, allow_hyphen_values = true)]
    pub harness_args: Vec<String>,
    /// Optional JSON array of argv tokens (alternative to repeated --harness-arg).
    #[arg(long = "harness-args-json")]
    pub harness_args_json: Option<String>,
    /// GIT REF the worker's worktree branches from (a branch name, tag or sha)
    /// — not a path. Omitted → the current branch HEAD.
    #[arg(long = "from")]
    pub from: Option<String>,
    /// Model id passed to the harness; the accepted values are the harness's
    /// own, listed per harness by `orgasmic manager drivers`.
    #[arg(long)]
    pub model: Option<String>,
    /// Reasoning-effort level passed to the harness; accepted values are the
    /// harness's own, listed by `orgasmic manager drivers`.
    #[arg(long)]
    pub effort: Option<String>,
    /// Force the harness credential tier for this dispatch: `auto` (default,
    /// detect), `bare_api_key` or `native_login`. Claude only; the daemon
    /// rejects an unknown value.
    #[arg(long = "credential-mode")]
    pub credential_mode: Option<String>,
    /// PATH for the worker's git worktree; omitted → a managed path under
    /// `.orgasmic/tmp/dispatch/`.
    #[arg(long)]
    pub worktree: Option<PathBuf>,
    /// Branch name to create for the worker; omitted → derived from the task
    /// id (e.g. `task-xxxxx-impl`).
    #[arg(long)]
    pub branch: Option<String>,
    /// Why this dispatch is happening; recorded on the
    /// `manager.dispatch_started` tx.
    #[arg(long)]
    pub reason: Option<String>,
    /// Plan the dispatch and print it without creating a worktree, writing a
    /// tx, or launching a worker.
    #[arg(long)]
    pub dry_run: bool,
    /// Sparse governance override as JSON (same shape as daemon GovernancePatch).
    #[arg(long = "governance-json")]
    pub governance_json: Option<String>,
}

#[derive(Copy, Clone, Debug, Eq, PartialEq, ValueEnum)]
pub enum DispatchCloseStatus {
    Done,
    Aborted,
}

#[derive(Args, Debug, Clone)]
pub struct DispatchCloseArgs {
    /// Task id whose dispatch is being closed; repeatable for a multi-task
    /// dispatch. All of them must belong to the same dispatch generation.
    #[arg(long = "task", action = ArgAction::Append, required = true)]
    pub task: Vec<String>,
    /// TX_ID of the `manager.dispatch_started` this close belongs to — the
    /// dispatch GENERATION, printed by `manager dispatch` as `started_tx=` and
    /// by `dispatch-status` as `TX_ID=`. Pass it: a close bound to a task
    /// rather than a generation can select a SUCCESSOR dispatch (TASK-6AYEJ.1).
    #[arg(long = "started-tx")]
    pub started_tx: Option<String>,
    /// How the dispatch ended. `done` records `implementer.done`; `aborted`
    /// records the abort and leaves the task where it is.
    #[arg(long, value_enum)]
    pub status: DispatchCloseStatus,
    /// Sha of the merge commit that landed the worker's branch, recorded on
    /// the close tx as evidence.
    #[arg(long = "merge-sha")]
    pub merge_sha: Option<String>,
    /// Sha of the worker's own last commit on its branch.
    #[arg(long = "worker-commit", alias = "codex-commit")]
    pub worker_commit: Option<String>,
    /// Harness session id for the worker run, for later transcript lookup.
    #[arg(long = "worker-session", alias = "codex-session")]
    pub worker_session: Option<String>,
    /// The diff range a reviewer actually read (e.g. `main..task-x-impl`).
    #[arg(long = "reviewed-diff")]
    pub reviewed_diff: Option<String>,
    /// Additional `KEY=VALUE` properties recorded on the close tx; repeatable.
    #[arg(long = "property", value_parser = parse_close_property)]
    pub properties: Vec<(String, String)>,
    /// Tokens the worker run consumed, recorded on the close tx.
    #[arg(long)]
    pub tokens: Option<u64>,
    /// Wall-clock duration of the worker run (free-form, e.g. `41m`).
    #[arg(long)]
    pub wall: Option<String>,
    /// Why the dispatch is being closed this way; recorded on the close tx.
    #[arg(long)]
    pub reason: Option<String>,
    /// Remove the worker's git worktree as part of the close. DEFAULTS TO
    /// TRUE (TASK-2BPWM): closing without saying anything removes it. The
    /// removal salvages uncommitted worker output to
    /// `refs/orgasmic/salvage/<sha>` first and then removes WITHOUT `--force`,
    /// so git's own clean check gates it — pass `--no-worktree-remove` to keep
    /// the worktree in place.
    #[arg(long = "worktree-remove", default_value_t = true)]
    pub worktree_remove: bool,
    /// Keep the worker's worktree on disk, overriding the default removal.
    #[arg(long = "no-worktree-remove")]
    pub no_worktree_remove: bool,
    /// Also delete the worker's branch. Defaults to FALSE: the branch survives
    /// the close unless this is passed.
    #[arg(long = "branch-delete", default_value_t = false)]
    pub branch_delete: bool,
}

/// Read-only dispatch inventory. Deliberately has NO `--project`: it reads
/// `.orgasmic/` as files, and a dispatch worktree's `.orgasmic/` is a frozen
/// snapshot, so it resolves the project from the cwd and refuses a frozen one
/// by name rather than accepting an id that could point anywhere (TASK-GQPGR).
#[derive(Args, Debug, Clone)]
#[command(after_help = "\
No --project: run this from the PRIMARY project root. A dispatch worktree
carries a frozen .orgasmic/ snapshot, and reading dispatch state from it
printed EMPTY with three dispatches open (TASK-GQPGR), so the refusal names
the primary root instead.")]
pub struct DispatchStatusArgs {
    /// Show only the dispatch for this task id.
    #[arg(long)]
    pub task: Option<String>,
    /// List only dispatches whose worker run is gone (no live process).
    #[arg(long = "orphans-only")]
    pub orphans_only: bool,
    /// Record the orphan close tx for dispatches whose worker died, clearing
    /// their leases. A WRITE, unlike the rest of this verb.
    #[arg(long = "cleanup-failed")]
    pub cleanup_failed: bool,
    /// List only multi-task dispatches that are closed for some tasks and
    /// still open for others.
    #[arg(long = "partial-closed")]
    pub partial_closed: bool,
}

#[derive(Args, Debug, Clone)]
pub struct LeaseReleaseArgs {
    /// Task whose dispatch lease should be cleared (e.g. TASK-099).
    #[arg(long)]
    pub task: String,
    /// Project id; defaults to the project containing the cwd.
    #[arg(long)]
    pub project: Option<String>,
    /// Lease kind: implementer (default; covers reviewer/architector
    /// dispatches too) or babysitter.
    #[arg(long, default_value = "implementer")]
    pub kind: String,
}

/// External manager self-registration (dec_3Y2E1): a manager session started
/// outside the app registers itself with the daemon so it appears in Running
/// Agents as a supervised run.
#[derive(Args, Debug, Clone)]
pub struct ManagerRegisterArgs {
    /// Project id; defaults to the project containing the cwd.
    #[arg(long)]
    pub project: Option<String>,
}

#[derive(Args, Debug, Clone)]
pub struct ManagerReleaseArgs {
    /// Project id; defaults to the project containing the cwd.
    #[arg(long)]
    pub project: Option<String>,
}

#[derive(Copy, Clone, Debug, Eq, PartialEq, ValueEnum)]
pub enum FinalizeStatus {
    Done,
    Blocked,
}

impl FinalizeStatus {
    fn as_str(self) -> &'static str {
        match self {
            Self::Done => "done",
            Self::Blocked => "blocked",
        }
    }
}

/// Worker-driven dispatch finalization (dec_3M7M0 / TASK-AFE5Q): the sole
/// success authority for a dispatched worker. In one daemon call it commits
/// the worktree (`--commit`), writes `last.txt` verbatim from
/// `--summary-file`, emits the terminal tx, and releases the lease.
#[derive(Args, Debug, Clone)]
#[command(after_help = "\
Examples:
  orgasmic dispatch finalize --summary-file /tmp/report.md --commit
  orgasmic dispatch finalize --run-id run-20260707T211315-19f5642cbf2d4fafbc8dabf834c95f5b \\
    --summary-file /tmp/report.md --commit
  orgasmic dispatch finalize --status blocked --reason \"brief impossible as written\" \\
    --summary-file /tmp/report.md")]
pub struct DispatchFinalizeArgs {
    /// Run id to finalize. Defaults to auto-resolving the single live run
    /// whose task matches (see --task) via the daemon's live run list —
    /// robust against a worker's own worktree checkout never seeing the
    /// live `.orgasmic/tx` writes the manager's checkout has.
    #[arg(long = "run-id")]
    pub run_id: Option<String>,
    /// Task id used for auto-resolving --run-id. Defaults to deriving it
    /// from the current git branch (e.g. task-wfw1n-impl -> TASK-WFW1N).
    #[arg(long)]
    pub task: Option<String>,
    /// Worker-authored report text. Written verbatim to last.txt — never
    /// scraped scrollback (acceptance #1).
    #[arg(long = "summary-file")]
    pub summary_file: PathBuf,
    /// Commit the worktree as part of finalize, so commit-stall is
    /// structurally impossible (acceptance #2).
    #[arg(long)]
    pub commit: bool,
    /// How this worker is finishing: `done` (the assignment was carried out)
    /// or `blocked` (it could not be, and `--reason` says why). Defaults to
    /// `done`.
    #[arg(long, value_enum, default_value = "done")]
    pub status: FinalizeStatus,
    /// Commit sha to record. Defaults to the sha `--commit` produces (or, if
    /// the worktree was already clean, the current HEAD).
    #[arg(long)]
    pub sha: Option<String>,
    /// Required when --status blocked.
    #[arg(long)]
    pub reason: Option<String>,
}

#[derive(Debug)]
pub(crate) struct DispatchPlan {
    pub(crate) project_root: PathBuf,
    pub(crate) project_id: String,
    pub(crate) tasks: Vec<String>,
    pub(crate) kind: DispatchKind,
    pub(crate) mode: String,
    pub(crate) harness: String,
    pub(crate) harness_args: Vec<String>,
    pub(crate) brief_path: PathBuf,
    pub(crate) brief_content: String,
    pub(crate) from_sha: String,
    pub(crate) worktree_path: PathBuf,
    pub(crate) branch: String,
    pub(crate) model_override: Option<String>,
    pub(crate) effort_override: Option<String>,
    pub(crate) credential_mode_override: Option<String>,
    pub(crate) last_path: PathBuf,
    pub(crate) stdout_path: PathBuf,
    pub(crate) dispatch_attempt_token: String,
    pub(crate) goal_id: Option<String>,
    pub(crate) reason: Option<String>,
    pub(crate) dry_run: bool,
    pub(crate) governance: Option<orgasmic_daemon::governance::GovernancePatch>,
}

impl DispatchPlan {
    pub(crate) fn dispatch_task(&self) -> String {
        task_list_property(&self.tasks)
    }

    fn with_artifacts(
        mut self,
        brief_path: PathBuf,
        last_path: PathBuf,
        stdout_path: PathBuf,
        dispatch_attempt_token: String,
    ) -> Self {
        self.brief_path = brief_path;
        self.last_path = last_path;
        self.stdout_path = stdout_path;
        self.dispatch_attempt_token = dispatch_attempt_token;
        self
    }
}

#[derive(Debug, Clone, Serialize)]
struct TxAppendRequest {
    request_id: Option<String>,
    #[serde(rename = "type")]
    ty: String,
    actor: Option<String>,
    machine: Option<String>,
    project: Option<String>,
    task: Option<String>,
    target: Option<String>,
    reason: Option<String>,
    extra: Vec<(String, String)>,
    tx_path: Option<PathBuf>,
}

#[derive(Debug, Deserialize)]
struct TxAppendResponse {
    tx_id: String,
    #[allow(dead_code)]
    tx_path: PathBuf,
    #[allow(dead_code)]
    time: String,
}

#[derive(Debug, Serialize)]
struct RunReleaseRequest {
    reason: Option<String>,
    request_id: Option<String>,
    #[serde(default)]
    finalized_by_worker: bool,
    /// Only `orgasmic dispatch finalize` sends this — the resolved run's own
    /// identity, so the daemon can reject a stale/reclaimed-slot release
    /// (TASK-DWJVH item A). `None` for dispatch-close/lease-release (human
    /// manager path, unauthenticated release unchanged).
    #[serde(default)]
    caller_identity: Option<RuntimeIdentity>,
    /// Only `orgasmic dispatch finalize` sends this — the worker's terminal tx,
    /// handed to the daemon so IT writes the tx right after the release it just
    /// performed (TASK-WGXKD). This process cannot be relied on to write it
    /// afterwards: the release tears down the driver, which reaps the harness's
    /// whole setsid process group, and this CLI runs inside that group.
    #[serde(default)]
    terminal_tx: Option<TxAppendRequest>,
}

#[derive(Debug, Deserialize)]
struct RunReleaseResponse {
    #[allow(dead_code)]
    run_id: String,
    /// Present when the daemon wrote the `terminal_tx` this release carried.
    /// Absent from a daemon that predates TASK-WGXKD — the client then still
    /// posts the tx itself (best effort; the deterministic request id makes a
    /// double emit a dedupe, not a duplicate).
    #[serde(default)]
    terminal_tx_id: Option<String>,
}

/// Capability token meaning "this daemon writes the `terminal_tx` a release
/// carries, as part of that release" (TASK-WGXKD, handshake added by
/// TASK-WGXKD.1). Mirrors `orgasmic_daemon::api::CAPABILITY_RELEASE_TERMINAL_TX`
/// — matched over the wire, so it is a string on both sides by design.
const CAPABILITY_RELEASE_TERMINAL_TX: &str = "release.terminal_tx";

#[derive(Debug, Default, Deserialize)]
struct DaemonCapabilitiesResponse {
    #[serde(default)]
    capabilities: Vec<String>,
}

/// `GET /runs/live` — run ids only, for health/orphan checks.
#[derive(Debug, Default, Deserialize)]
struct LiveRunsSummaryResponse {
    #[serde(default)]
    live: Vec<RunSummary>,
}

#[derive(Debug, Deserialize)]
struct RunSummary {
    run_id: String,
}

#[derive(Clone, Debug)]
struct DispatchRecord {
    tx_id: String,
    tasks: Vec<String>,
    kind: String,
    worktree: Option<PathBuf>,
    branch: Option<String>,
    model: Option<String>,
    effort: Option<String>,
    brief_path: Option<PathBuf>,
    last_path: Option<PathBuf>,
    stdout_path: Option<PathBuf>,
    dispatch_attempt_token: Option<String>,
    /// The run this generation is currently addressed by — the dispatched run,
    /// or the newest recovery replacement of it (TASK-6AYEJ.2).
    run_id: Option<String>,
    /// Every run id this generation has ever owned: the dispatched one plus any
    /// recovery replacements. A worker's `*.reported` may name any of them.
    run_ids: BTreeSet<String>,
    worker_id: Option<String>,
    driver: Option<String>,
    harness: Option<String>,
    pid: Option<u32>,
    started_at: Option<String>,
    worker_pid: Option<u32>,
    goal_id: Option<String>,
    closed_tasks: BTreeSet<String>,
    cleanup_already_run: bool,
    /// The worker finalized (`*.reported`) but the manager has not closed yet
    /// (TASK-6AYEJ). Only meaningful while `closed` is false.
    reported: bool,
    closed: bool,
}

#[derive(Debug)]
struct DispatchHealth {
    worktree_exists: bool,
    pid: Option<u32>,
    pid_alive: bool,
    run_alive: bool,
}

#[derive(Clone, Debug)]
struct TaskLifecycleInfo {
    id: String,
    stage: LifecycleStage,
    fix_subtask: bool,
}

#[derive(Clone, Debug)]
struct CleanupOutcome {
    status: CleanupStatus,
    error: Option<String>,
    salvage: Option<SalvageCommit>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct SalvageCommit {
    sha: String,
    ref_name: String,
    file_count: usize,
    worktree_removed: bool,
}

#[derive(Debug)]
struct WorktreeRemovalOutcome {
    removed: bool,
    salvage: Option<SalvageCommit>,
    error: Option<String>,
}

#[derive(Copy, Clone, Debug, Eq, PartialEq)]
enum CleanupStatus {
    Ok,
    WorktreeFailed,
    BranchFailed,
    Partial,
    WorktreeMissing,
    CleanupAlreadyRun,
}

impl CleanupStatus {
    fn as_str(self) -> &'static str {
        match self {
            Self::Ok => "ok",
            Self::WorktreeFailed => "worktree_failed",
            Self::BranchFailed => "branch_failed",
            Self::Partial => "partial",
            Self::WorktreeMissing => "worktree_missing",
            Self::CleanupAlreadyRun => "cleanup_already_run",
        }
    }
}

#[derive(Debug)]
struct CleanupFailureRecord {
    tx_id: String,
    ty: String,
    tasks: Vec<String>,
    status: String,
    error: Option<String>,
}

pub fn cmd_dispatch(home: &Home, args: DispatchArgs) -> Result<()> {
    let plan = build_dispatch_plan(home, args)?;
    if plan.brief_content.is_empty() {
        bail!("brief is empty: {}", plan.brief_path.display());
    }
    // Each dispatch kind owns a distinct default worktree suffix; reject any
    // accidental reuse of another kind's default path for the same task.
    for other_kind in [
        DispatchKind::Implementer,
        DispatchKind::Reviewer,
        DispatchKind::Architector,
    ] {
        if other_kind != plan.kind {
            let other_default =
                default_worktree(&plan.project_root, first_task(&plan.tasks), other_kind);
            if normalize_path(&plan.worktree_path) == normalize_path(&other_default) {
                bail!(
                    "{} worktree must not reuse {} default path: {}",
                    plan.kind,
                    other_kind,
                    plan.worktree_path.display()
                );
            }
        }
    }

    if plan.dry_run {
        let attempt_id = mint_dispatch_attempt_id();
        let (brief, last, stdout) =
            dispatch_artifact_paths_for_attempt(&plan.project_root, &plan.brief_path, &attempt_id);
        print_dispatch_plan(&plan.with_artifacts(brief, last, stdout, attempt_id));
        return Ok(());
    }

    // orgasmic:task_EP3H1 — a torn implementer close leaves its task short of
    // IN_REVIEW, which is exactly the stage the reviewer dispatch below
    // demands. Finish the transition before the gate reads it.
    reconcile_torn_closes_best_effort(home, &plan.project_root, &plan.project_id);

    let mut reservation =
        DispatchArtifactReservation::reserve(&plan.project_root, &plan.brief_path)?;
    let plan = plan.with_artifacts(
        reservation.brief_path(),
        reservation.last_path(),
        reservation.stdout_path(),
        reservation.attempt_token(),
    );

    materialize_dispatch_brief(&plan)?;

    let pre_dispatch_stages = capture_task_lifecycle_stages(&plan.project_root, &plan.tasks)?;

    create_worktree(
        &plan.project_root,
        &plan.worktree_path,
        &plan.branch,
        &plan.from_sha,
    )?;

    let runtime = tokio::runtime::Runtime::new().context("create tokio runtime")?;
    let client = DaemonClient::from_home_autostart(home)?;

    if let Err(err) = apply_task_lifecycle_transitions(
        &client,
        &plan.project_id,
        &dispatch_lifecycle_transitions(plan.kind, &plan.tasks),
    ) {
        let reason = format!("lifecycle update failed: {err}");
        let cleanup = cleanup_created_resources(
            &plan.project_root,
            &plan.worktree_path,
            &plan.branch,
            &plan.dispatch_task(),
            &plan.last_path,
            &plan.stdout_path,
        );
        if cleanup.status != CleanupStatus::Ok {
            bail!(
                "{reason}; cleanup status={} error={}",
                cleanup.status.as_str(),
                cleanup.error.as_deref().unwrap_or("-")
            );
        }
        bail!(reason);
    }

    let response = match runtime.block_on(client.post_dispatch(&plan)) {
        Ok(response) => response,
        Err(err) => {
            let ambiguous =
                crate::daemon_client::DaemonClient::dispatch_failure_needs_daemon_cleanup(&err);
            if ambiguous {
                // POST may have been accepted; commit ownership so a live daemon
                // run retains its declared artifact paths (TASK-1FV1N).
                reservation.commit();
            }
            let reason = format!("daemon dispatch failed: {err}");
            let cleanup = if ambiguous {
                // If the cleanup request can't reach the daemon either, do
                // NOT fall back to local deletion: the original failure was
                // ambiguous, so a spawned worker may own the worktree
                // (fencing invariant). Leave the resources for inspection
                // and keep going so lifecycle restore + the original error
                // still surface.
                match runtime.block_on(request_daemon_dispatch_cleanup(&client, &plan)) {
                    Ok(outcome) => outcome,
                    Err(cleanup_err) => CleanupOutcome {
                        status: CleanupStatus::Partial,
                        error: Some(sanitize_tx_value(&format!(
                            "daemon cleanup request failed: {cleanup_err}; worktree {} and branch {} left in place",
                            plan.worktree_path.display(),
                            plan.branch
                        ))),
                        salvage: None,
                    },
                }
            } else {
                cleanup_created_resources(
                    &plan.project_root,
                    &plan.worktree_path,
                    &plan.branch,
                    &plan.dispatch_task(),
                    &plan.last_path,
                    &plan.stdout_path,
                )
            };
            restore_task_lifecycle_stages(&client, &plan.project_id, &pre_dispatch_stages);
            if cleanup.status != CleanupStatus::Ok {
                bail!(
                    "{reason}; cleanup status={} error={}",
                    cleanup.status.as_str(),
                    cleanup.error.as_deref().unwrap_or("-")
                );
            }
            bail!(reason);
        }
    };

    // POST succeeded — commit artifact ownership before any further I/O.
    reservation.commit();

    // `started_tx` is the generation token `dispatch-close --started-tx` takes
    // (TASK-6AYEJ.1); print it here so the manager never has to go looking.
    println!(
        "dispatched: {} {} pid={} run_id={} started_tx={} worker={} driver={} harness={} brief={}",
        task_list_property(&plan.tasks),
        plan.kind,
        response.pid,
        response.run_id,
        response.dispatch_tx_id,
        response.worker_id,
        response.driver,
        response.harness,
        plan.brief_path.display()
    );
    if response.pid > 0 {
        println!(
            "watch: until [ -s {} ] || ! ps -p {} > /dev/null; do sleep 8; done",
            shell_quote(&plan.last_path),
            response.pid
        );
    } else {
        println!("watch: orgasmic run show {}", response.run_id);
    }
    Ok(())
}

pub fn cmd_dispatch_close(home: &Home, args: DispatchCloseArgs) -> Result<()> {
    let project_root = find_live_project_root(home, "manager dispatch-close")?;
    let project_id = read_project_id(&project_root)?;
    let tasks = normalize_tasks(args.task.clone())?;
    // orgasmic:task_EP3H1 — before anything else, including the already-closed
    // no-op below: a re-run of a torn close must finish the transition it lost
    // rather than report "already closed" over a task still stranded at its
    // pre-close stage.
    reconcile_torn_closes_best_effort(home, &project_root, &project_id);
    let open = match resolve_close_target(&project_root, &tasks, args.started_tx.as_deref())? {
        CloseTarget::Open(open) => open,
        CloseTarget::AlreadyClosed(closed) => {
            // Deliberately NOT re-running cleanup: "no-op" is the contract, and
            // the caller can tell the difference from a real close because the
            // line says `already-closed` and carries no new tx id. For the
            // historical worker-closed dispatches the worktree may well still
            // be on disk, so say so rather than let `--worktree-remove` look
            // like it ran.
            println!(
                "already-closed: {} started_tx={} (no-op)",
                task_list_property(&tasks),
                closed.tx_id
            );
            if args.worktree_remove && !args.no_worktree_remove {
                if let Some(worktree) = closed.worktree.as_deref().filter(|path| path.exists()) {
                    eprintln!(
                        "warning: dispatch {} was already closed, so --worktree-remove did not \
                         run; worktree {} is still on disk (remove it with `git worktree remove`)",
                        closed.tx_id,
                        worktree.display()
                    );
                }
            }
            return Ok(());
        }
    };
    for task in &tasks {
        if !open.tasks.iter().any(|open_task| open_task == task) {
            bail!(
                "open dispatch {} does not include requested task {}",
                task_list_property(&open.tasks),
                task
            );
        }
    }

    let tx_type = match args.status {
        DispatchCloseStatus::Done => done_tx_type(&open)?,
        DispatchCloseStatus::Aborted => "manager.dispatch_aborted",
    };
    let merge_sha = args
        .merge_sha
        .as_ref()
        .map(|s| sanitize_tx_value(s))
        .filter(|s| !s.is_empty());
    if args.status == DispatchCloseStatus::Done
        && tx_type == "implementer.done"
        && merge_sha.is_none()
    {
        bail!("--merge-sha is required when closing an implementer dispatch as implementer.done");
    }
    if args.status == DispatchCloseStatus::Done
        && tx_type == "architector.done"
        && merge_sha.is_none()
    {
        bail!("--merge-sha is required when closing an architector dispatch as architector.done");
    }
    // The TASK-6AYEJ.1 `--merge-sha` fence that used to sit here guarded the
    // TOKENLESS path against replaying an older generation's close onto a
    // successor. TASK-6AYEJ.2 removed that path entirely — a tokenless close can
    // no longer reach a live record at all — so reaching this point means
    // `--started-tx` named this exact generation and there is nothing left to
    // second-guess.
    let abort_reason = if args.status == DispatchCloseStatus::Aborted {
        Some(
            args.reason
                .as_ref()
                .map(|s| sanitize_tx_value(s))
                .filter(|s| !s.is_empty())
                .ok_or_else(|| anyhow::anyhow!("--reason is required when --status aborted"))?,
        )
    } else {
        None
    };

    let runtime = tokio::runtime::Runtime::new().context("create tokio runtime")?;
    let client = DaemonClient::from_home_autostart(home)?;
    let remove_worktree = args.worktree_remove && !args.no_worktree_remove;
    // TASK-1T3FZ: a destructive close takes a DAEMON-OWNED reservation on the
    // worktree before it releases — or fails to find — any run, and holds it
    // until cleanup is done. The competing recovery runs in another process
    // (`POST /runs/:origin/recover`), so a liveness decision made here and
    // acted on here has a window no amount of in-process care can close: only
    // the supervisor lock, which the acquire path also takes, can install a
    // fence and read liveness as one step. The verdict comes back with it.
    let mut close_guard = if remove_worktree {
        let guard = runtime
            .block_on(reserve_close_guard(&client, &project_id, &open))
            .context("liveness check before dispatch-close cleanup")?;
        if guard.is_some() {
            dispatch_close_pause_after_guard();
        }
        guard
    } else {
        None
    };

    let release = if let Some(run_id) = open.run_id.as_deref() {
        // Modern dispatch records carry the exact run id. Release it directly
        // so unrelated stale recovery records cannot block close. Transport and
        // other failures still fail before cleanup, preserving the live-worker
        // fencing invariant.
        //
        // A 404 no longer needs corroborating here: it says only "the id I am
        // holding is not live", and the thing it fails to rule out — a recovery
        // replacement whose origin→replacement association never reached the
        // ledger, live in this very worktree — is precisely what the guard
        // above already refused on, by worktree occupancy rather than by id.
        runtime
            .block_on(release_dispatch_run(
                &client,
                run_id,
                &task_list_property(&tasks),
            ))
            .err()
            .filter(|error| !is_release_run_not_found_error(error))
            .map(|error| error.context("release recorded run before dispatch-close cleanup"))
    } else if close_guard.is_none() {
        // No run identity to release and no guard: either this close touches no
        // files, or the record names no worktree, so there is nothing to fence
        // and nothing to destroy. Keep the daemon-reachability check it has
        // always had here — but note that reachability is all it is. The
        // destructive case no longer relies on it: it goes through the guard,
        // which is the only one of the two that is evidence of anything
        // (TASK-1T3FZ finding 2).
        runtime
            .block_on(fetch_live_runs(&client))
            .context("liveness check before dispatch-close cleanup")
            .err()
    } else {
        None
    };
    if let Some(error) = release {
        finish_close_guard(&runtime, &client, &project_id, &open, close_guard.as_mut());
        return Err(error);
    }

    let missing_close_tasks = tasks
        .iter()
        .filter(|task| !open.closed_tasks.contains(*task))
        .cloned()
        .collect::<Vec<_>>();
    let cleanup = if missing_close_tasks.is_empty() || open.cleanup_already_run {
        CleanupOutcome {
            status: CleanupStatus::CleanupAlreadyRun,
            error: None,
            salvage: None,
        }
    } else {
        cleanup_dispatch(&project_root, &open, remove_worktree, args.branch_delete)
    };
    finish_close_guard(&runtime, &client, &project_id, &open, close_guard.as_mut());
    if cleanup_status_reports_warning(cleanup.status) {
        eprintln!(
            "warning: dispatch cleanup status={} error={}",
            cleanup.status.as_str(),
            cleanup.error.as_deref().unwrap_or("-")
        );
    }
    if let Some(salvage) = &cleanup.salvage {
        if salvage.worktree_removed {
            println!(
                "cleanup: worktree salvaged sha={} ref={} files={}; worktree removed",
                salvage.sha, salvage.ref_name, salvage.file_count
            );
        } else {
            println!(
                "cleanup: worktree salvaged sha={} ref={} files={}; worktree retained after removal failure",
                salvage.sha, salvage.ref_name, salvage.file_count
            );
        }
    }
    // orgasmic:task_EP3H1 — computed BEFORE the close txs so each one can carry
    // the transition it is about to make. That is what makes a lost lifecycle
    // leg repairable from the ledger instead of by hand.
    let transitions = close_lifecycle_transitions(&project_root, &tasks, &open, &args)?;
    let mut responses = Vec::new();
    match args.status {
        DispatchCloseStatus::Done => {
            for task in &missing_close_tasks {
                let request = close_done_request(
                    &project_id,
                    &open,
                    task,
                    &args,
                    &CloseTxFacts {
                        tx_type,
                        merge_sha: merge_sha.as_deref(),
                        cleanup: &cleanup,
                        transition: transition_for(&transitions, task),
                    },
                );
                responses.push(
                    runtime.block_on(client.post_json::<_, TxAppendResponse>("/tx", &request))?,
                );
            }
        }
        DispatchCloseStatus::Aborted => {
            let reason = abort_reason.as_deref().expect("validated aborted reason");
            for task in &missing_close_tasks {
                let request = close_aborted_request(
                    &project_id,
                    &open,
                    task,
                    reason,
                    &cleanup,
                    transition_for(&transitions, task),
                );
                responses.push(
                    runtime.block_on(client.post_json::<_, TxAppendResponse>("/tx", &request))?,
                );
            }
        }
    };

    if let Err(err) =
        apply_close_lifecycle_transitions(&client, &runtime, &project_id, &open.tx_id, &transitions)
    {
        eprintln!(
            "warning: close tx appended but lifecycle update failed: {err}\n  \
             the close tx records the transition it intended; the next `orgasmic manager` \
             command finishes it"
        );
    }

    let tx_ids = if responses.is_empty() {
        "already_closed".to_string()
    } else {
        responses
            .iter()
            .map(|response| response.tx_id.as_str())
            .collect::<Vec<_>>()
            .join(" ")
    };
    println!(
        "closed: {} {} tx={}",
        task_list_property(&tasks),
        tx_type,
        tx_ids
    );
    Ok(())
}

/// Clear an orphaned dispatch lease through the daemon. The daemon refuses
/// when a live run still holds the lease (release the run instead), so this is
/// always safe to try — and it replaces the "restart the daemon to clear a
/// lease" anti-pattern, which the restart guard now refuses outright.
pub fn cmd_lease_release(home: &Home, args: LeaseReleaseArgs) -> Result<()> {
    let project_id = match args.project.clone() {
        Some(project) => project,
        None => read_project_id(&find_project_root()?)?,
    };
    let client = DaemonClient::from_home_autostart(home)?;
    let runtime = tokio::runtime::Runtime::new().context("create tokio runtime")?;

    #[derive(Deserialize)]
    struct LeaseReleaseResponse {
        status: String,
        run_id: Option<String>,
    }

    let response: LeaseReleaseResponse = runtime.block_on(client.post_json(
        &format!(
            "/projects/{}/tasks/{}/lease/release",
            path_segment(&project_id),
            path_segment(&args.task)
        ),
        &serde_json::json!({ "kind": args.kind }),
    ))?;

    match response.status.as_str() {
        "released" => println!(
            "✓ cleared orphaned lease for {} (was run {})",
            args.task,
            response.run_id.as_deref().unwrap_or("-")
        ),
        "no_lease" => println!("no lease held for {}; nothing to clear", args.task),
        other => println!("lease release: {other}"),
    }
    Ok(())
}

/// Terminal session-leader pid (`getsid(0)`) for the calling process, when
/// resolvable. This is what the daemon polls (~30s) to know the registrant
/// terminal is still alive (dec_3Y2E1).
#[cfg(unix)]
fn terminal_session_leader_pid() -> Option<u32> {
    let sid = unsafe { libc::getsid(0) };
    u32::try_from(sid).ok()
}

#[cfg(not(unix))]
fn terminal_session_leader_pid() -> Option<u32> {
    None
}

#[derive(Debug, Serialize)]
struct ManagerRegisterHttpRequest {
    project_id: String,
    pid: Option<u32>,
    holder_token: Option<String>,
}

#[derive(Debug, Deserialize)]
struct ManagerRegisterHttpResponse {
    status: String,
    run_id: Option<String>,
    message: Option<String>,
    #[serde(default)]
    holder_token: Option<String>,
}

const MANAGER_HOLDER_TOKEN_ENV: &str = "ORGASMIC_MANAGER_HOLDER_TOKEN";

fn safe_registration_component(value: &str) -> String {
    value
        .chars()
        .map(|ch| {
            if ch.is_ascii_alphanumeric() || matches!(ch, '-' | '_' | '.') {
                ch
            } else {
                '_'
            }
        })
        .collect()
}

fn manager_session_scope() -> Option<String> {
    for name in [
        "ORGASMIC_MANAGER_SESSION_ID",
        "TERM_SESSION_ID",
        "WT_SESSION",
        "TMUX_PANE",
        "RMUX_SESSION_ID",
    ] {
        if let Ok(value) = std::env::var(name) {
            let value = value.trim();
            if !value.is_empty() {
                return Some(format!("{name}-{value}"));
            }
        }
    }
    #[cfg(unix)]
    {
        let parent = unsafe { libc::getppid() };
        if parent > 0 {
            return Some(format!("ppid-{parent}"));
        }
    }
    None
}

/// PID-less registration token storage is scoped to the invoking terminal
/// session, never merely to the project. Otherwise a second terminal could
/// read the first terminal's token and silently extend its TTL.
fn manager_holder_token_path(home: &Home, project_id: &str) -> Option<PathBuf> {
    let scope = manager_session_scope()?;
    Some(home.state().join("manager-registration").join(format!(
        "{}--{}.token",
        safe_registration_component(project_id),
        safe_registration_component(&scope)
    )))
}

fn read_manager_holder_token(path: Option<&Path>) -> Option<String> {
    std::env::var(MANAGER_HOLDER_TOKEN_ENV)
        .ok()
        .filter(|token| !token.trim().is_empty())
        .or_else(|| {
            std::fs::read_to_string(path?)
                .ok()
                .map(|token| token.trim().to_string())
                .filter(|token| !token.is_empty())
        })
}

fn persist_manager_holder_token(path: Option<&Path>, token: &str) -> Result<bool> {
    let Some(path) = path else {
        return Ok(false);
    };
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).with_context(|| {
            format!("create manager registration state at {}", parent.display())
        })?;
    }
    std::fs::write(path, format!("{token}\n"))
        .with_context(|| format!("write manager registration state at {}", path.display()))?;
    Ok(true)
}

/// External manager self-registration (dec_3Y2E1). The entry router runs
/// this unconditionally on every manager startup; when `ORGASMIC_RUN_ID` is
/// already set (a PTY the daemon launched, per drivers exporting it into
/// every session) this is a no-op — the command itself knows when it applies,
/// so the router carries no conditional prose.
pub fn cmd_manager_register(home: &Home, args: ManagerRegisterArgs) -> Result<()> {
    if let Ok(run_id) = std::env::var("ORGASMIC_RUN_ID") {
        let run_id = run_id.trim();
        if !run_id.is_empty() {
            println!("already supervised as {run_id}; nothing to do");
            return Ok(());
        }
    }

    let project_id = match args.project.clone() {
        Some(project) => project,
        None => read_project_id(&find_project_root()?)?,
    };
    let pid = terminal_session_leader_pid();
    let token_path = pid
        .is_none()
        .then(|| manager_holder_token_path(home, &project_id))
        .flatten();
    let holder_token = pid
        .is_none()
        .then(|| read_manager_holder_token(token_path.as_deref()))
        .flatten();
    let client = DaemonClient::from_home_autostart(home)?;
    let runtime = tokio::runtime::Runtime::new().context("create tokio runtime")?;
    let response: ManagerRegisterHttpResponse = runtime.block_on(client.post_json(
        "/manager/register",
        &ManagerRegisterHttpRequest {
            project_id: project_id.clone(),
            pid,
            holder_token,
        },
    ))?;

    match response.status.as_str() {
        "registered" => {
            if let Some(token) = response.holder_token.as_deref() {
                if !persist_manager_holder_token(token_path.as_deref(), token)? {
                    println!(
                        "PID unavailable; export {MANAGER_HOLDER_TOKEN_ENV}={token} to refresh this registration"
                    );
                }
            }
            println!(
                "registered manager for {project_id} as {}",
                response.run_id.as_deref().unwrap_or("-")
            );
            Ok(())
        }
        "refreshed" => {
            if let Some(token) = response.holder_token.as_deref() {
                let _ = persist_manager_holder_token(token_path.as_deref(), token)?;
            }
            println!(
                "manager registration for {project_id} refreshed ({})",
                response.run_id.as_deref().unwrap_or("-")
            );
            Ok(())
        }
        "refused" => bail!(
            "{}",
            response
                .message
                .unwrap_or_else(|| "manager registration refused".to_string())
        ),
        other => bail!("unexpected manager register status: {other}"),
    }
}

#[derive(Debug, Serialize)]
struct ManagerReleaseHttpRequest {
    project_id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    run_id: Option<String>,
}

#[derive(Debug, Deserialize)]
struct ManagerReleaseHttpResponse {
    status: String,
    run_id: Option<String>,
}

/// Explicit deregistration for `orgasmic manager register`. A no-op (not an
/// error) when nothing is registered.
pub fn cmd_manager_release(home: &Home, args: ManagerReleaseArgs) -> Result<()> {
    let project_id = match args.project.clone() {
        Some(project) => project,
        None => read_project_id(&find_project_root()?)?,
    };
    let run_id = std::env::var("ORGASMIC_RUN_ID")
        .ok()
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty());
    let client = DaemonClient::from_home_autostart(home)?;
    let runtime = tokio::runtime::Runtime::new().context("create tokio runtime")?;
    let response: ManagerReleaseHttpResponse = runtime.block_on(client.post_json(
        "/manager/release",
        &ManagerReleaseHttpRequest {
            project_id: project_id.clone(),
            run_id,
        },
    ))?;

    match response.status.as_str() {
        "released" => println!(
            "released manager registration for {project_id} (was {})",
            response.run_id.as_deref().unwrap_or("-")
        ),
        "not_registered" => println!("no manager registered for {project_id}; nothing to do"),
        other => println!("manager release: {other}"),
    }
    if matches!(response.status.as_str(), "released" | "not_registered") {
        if let Some(path) = manager_holder_token_path(home, &project_id) {
            if let Err(e) = std::fs::remove_file(path) {
                if e.kind() != std::io::ErrorKind::NotFound {
                    return Err(e).context("remove manager registration state");
                }
            }
        }
    }
    Ok(())
}

#[derive(Debug, Clone, Deserialize)]
struct LiveRunInfo {
    run_id: String,
    task_id: String,
    kind: String,
    #[serde(default)]
    project_id: Option<String>,
    #[serde(default)]
    last_path: Option<PathBuf>,
    /// The dispatched worktree root, when the daemon knows it (mirrors
    /// `RunSummary::worktree`). `orgasmic dispatch finalize --commit`
    /// cross-checks this against the resolved git toplevel before
    /// committing, refusing to commit a root that isn't the dispatched
    /// worktree (TASK-QKQ3R).
    #[serde(default)]
    worktree: Option<PathBuf>,
    /// Session JSONL path for the live run (`RunSummary::session_path`).
    /// Used after release to read the terminal `Lifecycle::Release` tombstone
    /// when finalize's own release lands on an already-released run.
    #[serde(default)]
    session_path: Option<PathBuf>,
    /// The run's own `RuntimeIdentity`, always present on `/runs/live`
    /// (`RunSummary::identity`). Presented back on the finalize release call
    /// (TASK-DWJVH item A) so the daemon can reject a stale/reclaimed-slot
    /// release.
    identity: RuntimeIdentity,
}

#[derive(Debug, Default, Deserialize)]
struct LiveRunsResponse {
    #[serde(default)]
    live: Vec<LiveRunInfo>,
}

#[derive(Debug, Deserialize)]
struct LiveRunResponse {
    run: LiveRunInfo,
}

/// The worker-driven counterpart to `dispatch-close` (dec_3M7M0 / TASK-AFE5Q):
/// a dispatched worker calls this as its terminal action — the sole success
/// authority for the WORKER's completion. In one daemon call it optionally
/// commits the worktree, writes `last.txt` verbatim from `--summary-file`,
/// emits the worker-completion tx
/// (`implementer.reported`/`reviewer.reported`/`manager.dispatch_aborted`), and
/// releases the lease — converging on the same `release_dispatch_run`/`/tx`
/// plumbing `dispatch-close` uses.
///
/// It does NOT close the dispatch (TASK-6AYEJ): the worktree, the branch, the
/// merge sha and the closing tx belong to the manager's `dispatch-close`. See
/// [`finalize_tx_type_for_kind`].
pub fn cmd_dispatch_finalize(home: &Home, args: DispatchFinalizeArgs) -> Result<()> {
    // The commit/branch/worktree boundary is the git worktree toplevel, NOT
    // the `.orgasmic/project.org` marker walk: a dispatch worktree checkout
    // lacks `.orgasmic/` whenever the project hasn't committed it yet
    // (greenfield window), and the marker walk then escapes the worktree and
    // resolves the manager's live repo root instead (TASK-QKQ3R). Reading the
    // project id is the only thing still allowed to fall back to the marker
    // walk (harmless — no writes bind to it).
    let cwd = std::env::current_dir().context("cwd")?;
    let git_root = git_toplevel(&cwd)?;
    let project_id = resolve_finalize_project_id(&git_root);
    let summary = std::fs::read_to_string(&args.summary_file)
        .with_context(|| format!("read --summary-file {}", args.summary_file.display()))?;
    if args.status == FinalizeStatus::Blocked
        && args
            .reason
            .as_deref()
            .map(str::trim)
            .unwrap_or_default()
            .is_empty()
    {
        bail!("--reason is required when --status blocked");
    }

    let client = DaemonClient::from_home_autostart(home)?;
    let runtime = tokio::runtime::Runtime::new().context("create tokio runtime")?;
    let env_run_id = std::env::var("ORGASMIC_RUN_ID")
        .ok()
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty());
    let explicit_run_id = args.run_id.clone().or(env_run_id);
    let (task, run) = if let Some(run_id) = explicit_run_id {
        let run = runtime.block_on(resolve_finalize_run_by_id(
            &client,
            project_id.as_deref(),
            &run_id,
        ))?;
        (run.task_id.clone(), run)
    } else {
        let task = resolve_finalize_task(&git_root, args.task.clone())?;
        let run = runtime.block_on(resolve_finalize_run(
            &client,
            project_id.as_deref(),
            &task,
            None,
        ))?;
        (task, run)
    };

    // Authoritative cross-check (TASK-QKQ3R part B): the daemon knows the
    // worktree it dispatched for this run. If the resolved git toplevel
    // doesn't match it, refuse to commit a root that isn't the dispatched
    // worktree. Without `--commit` a mismatch is a warning only — finalize
    // `--sha` from an unexpected cwd is a legitimate rescue flow.
    if let Some(run_worktree) = run.worktree.as_deref() {
        if normalize_path(&git_root) != normalize_path(run_worktree) {
            if args.commit {
                bail!(
                    "refusing --commit: resolved git worktree {} does not match \
                     the dispatched worktree {} for run {}",
                    git_root.display(),
                    run_worktree.display(),
                    run.run_id
                );
            }
            eprintln!(
                "warning: resolved git worktree {} does not match the dispatched \
                 worktree {} for run {} (finalize from an unexpected cwd?)",
                git_root.display(),
                run_worktree.display(),
                run.run_id
            );
        }
    }

    // Version-skew handshake (TASK-WGXKD.1). Deliberately before the commit and
    // before last.txt, not merely before the release: a refusal then leaves the
    // worktree exactly as this finalize found it, so the retry after a daemon
    // restart is the same command with the same effect.
    runtime.block_on(require_daemon_writes_terminal_tx(&client, &task))?;

    let sha = if args.commit {
        Some(commit_worktree(
            &git_root,
            &finalize_commit_message(&task, args.status, &summary),
        )?)
    } else {
        args.sha.clone()
    };

    let tx_type = match args.status {
        FinalizeStatus::Done => finalize_tx_type_for_kind(&run.kind)?,
        FinalizeStatus::Blocked => "manager.dispatch_aborted",
    };

    let mut extra = vec![
        ("RUN_ID".to_string(), run.run_id.clone()),
        ("WORKTREE".to_string(), git_root.display().to_string()),
    ];
    if let Some(sha) = sha.as_deref() {
        // The worker's own commit, and only that. `MERGE_SHA` is the sha the
        // MANAGER merged to its base branch; it is recorded by `dispatch-close`
        // and is not knowable here (TASK-6AYEJ). A worker branch tip written as
        // MERGE_SHA points at a commit that was never on main as such.
        extra.push(("SHA".to_string(), sha.to_string()));
    }

    // Order matters (reviewer #2): the worker's terminal tx lands only after
    // the durable artifacts (commit above, last.txt) and the lease release have
    // all succeeded, so there is never a "done" claim with no report and a
    // still-held lease. The release is also the authority on whether this
    // worker may claim completion at all — it is what rejects a reclaimed slot
    // or a stall-swept run — so a refused release must leave no tx behind.
    //
    // What that ordering cost while the CLIENT owned the gap, measured
    // (TASK-WGXKD, runs run-20260727T131952-…, run-20260728T042231-…,
    // run-20260728T042232-…): the intent was that a death before the tx leaves
    // the run stalled, orphan-flagged and rescuable. It did not. Step 2 tears
    // down the driver, which reaps the harness's whole setsid process group
    // (`reap_process_group`, TERM then KILL) — and this CLI is a member of that
    // group, because the harness spawned it. Release kills the process that
    // still had to write the tx. On acp-stdio that reap is a direct
    // `kill(-pgid, …)` and the tx was lost every time (3/3); on rmux it goes
    // through the rmux server, and the extra hop left just enough time to win
    // (2/2). Losing it left a durable commit, a durable last.txt, a RELEASED
    // lease, no terminal tx — and NO orphan flag, because from the daemon's
    // side the release *was* a worker finalize. That fourth state is invisible
    // to both the success path and the rescue path; only `dispatch-status`'s
    // `[unreported]` marker showed it.
    //
    // The fix keeps this ordering and moves the gap off this process: the tx is
    // handed to the daemon WITH the release (`terminal_tx`), and the daemon
    // writes it immediately after the release it just performed. Whatever this
    // process's fate, the tx is on record iff the lease was released. The
    // client-side POST below survives only as the fallback for the two paths
    // the daemon cannot own: a pre-daemon-fix daemon, and the stall-sweep race
    // where our release call 404s and no daemon-side release happened at all.

    // 1. Write last.txt verbatim — the run's own artifact path, resolved from
    //    the daemon's live run record, never scraped scrollback.
    let last_path = run.last_path.clone().ok_or_else(|| {
        anyhow::anyhow!(
            "live run {} has no last_path (not a CLI dispatch run?)",
            run.run_id
        )
    })?;
    if let Some(parent) = last_path.parent() {
        std::fs::create_dir_all(parent).with_context(|| format!("create {}", parent.display()))?;
    }
    std::fs::write(&last_path, &summary)
        .with_context(|| format!("write {}", last_path.display()))?;

    // The terminal tx, built before the release because the release is what
    // carries it (TASK-WGXKD). Request id is deterministic per run so
    // concurrent double-finalize cannot double-emit (writer dedupes replays) —
    // which also makes the client-side fallback POST below safe to attempt.
    let tx_request = TxAppendRequest {
        request_id: Some(format!(
            "dispatch-finalize-{}-{}",
            request_slug(&task),
            run.run_id
        )),
        ty: tx_type.to_string(),
        actor: Some(format!("agent.{}", run.kind)),
        machine: None,
        project: project_id.clone(),
        task: Some(task.clone()),
        target: None,
        reason: args
            .reason
            .as_ref()
            .map(|s| sanitize_tx_value(s))
            .filter(|s| !s.is_empty()),
        extra,
        tx_path: None,
    };

    // 2. Release the lease, marked finalized_by_worker so the completion
    //    watcher suppresses its fallback scrape, and carrying the terminal tx
    //    so the daemon writes it the moment the release succeeds. Presents this
    //    run's own identity so the daemon can reject a stale/reclaimed-slot
    //    release (TASK-DWJVH item A). Resilient to the stall-sweep race (item
    //    B): the commit + last.txt write above already made this run's work
    //    durable, so if the stall sweep released this same run in the
    //    window between `resolve_finalize_run` and here, "already released"
    //    is a success-with-warning, not a hard error — otherwise the run
    //    would end up a done-less orphan despite an intact report.
    if let Some(delay) = finalize_release_delay_for_tests() {
        std::thread::sleep(delay);
    }
    let release = match runtime.block_on(release_dispatch_run_with_reason(
        &client,
        &run.run_id,
        &format!("worker finalize for {task}"),
        &task,
        true,
        Some(&run.identity),
        Some(tx_request.clone()),
    )) {
        Ok(response) => Some(response),
        Err(e) => {
            if is_release_run_not_found_error(&e) {
                let session_path = run.session_path.as_deref().ok_or_else(|| {
                    anyhow::anyhow!(
                        "live run {} has no session_path; cannot verify release tombstone",
                        run.run_id
                    )
                })?;
                match dispatch_release_tombstone(session_path)? {
                    DispatchReleaseTombstone::WorkerFinalized
                    | DispatchReleaseTombstone::ArtifactSubmitted
                    | DispatchReleaseTombstone::ManagerReleased => {
                        eprintln!(
                            "warning: run {} was already released by a prior worker \
                             finalize; skipping duplicate terminal tx",
                            run.run_id
                        );
                        println!(
                            "finalized: {} {} already recorded run={} last={}",
                            task,
                            tx_type,
                            run.run_id,
                            last_path.display()
                        );
                        return Ok(());
                    }
                    DispatchReleaseTombstone::StallSweep => {
                        eprintln!(
                            "warning: run {} was already released before finalize's own \
                             release call (stall-sweep race); proceeding — commit and \
                             last.txt are already durable",
                            run.run_id
                        );
                    }
                    DispatchReleaseTombstone::ProtocolEndWithoutFinalize => {
                        bail!(
                            "run {} ended at protocol before finalize could release the \
                             lease; refusing to emit {tx_type} (would record both done \
                             and orphaned)",
                            run.run_id
                        );
                    }
                    DispatchReleaseTombstone::Unrecognized | DispatchReleaseTombstone::None => {
                        bail!(
                            "run {} was already released but session has no \
                             worker-finalize tombstone; refusing to emit {tx_type}",
                            run.run_id
                        );
                    }
                }
                None
            } else {
                return Err(e);
            }
        }
    };

    // Test-only: die exactly where production dies — the instant the release
    // call returns, before anything else this process might still do. A
    // finalize that still needs its own turn after the release cannot survive
    // the group reap the release triggers.
    finalize_kill_self_after_release_for_tests();

    // 3. The terminal tx is on record. Normally the daemon wrote it as part of
    // the release above, which is what makes "lease released, run unreported"
    // impossible even when this process is killed by that release. The
    // client-side POST now covers only the stall-sweep race above, where our
    // release 404'd and the daemon never released anything for us — in that
    // case nothing killed this process and it does get another turn. The old
    // daemon case is gone: the handshake refuses before releasing rather than
    // relying on a fallback that a group reap would never let run
    // (TASK-WGXKD.1). Same deterministic request id either way, so a redundant
    // attempt dedupes.
    let tx_id = match release.and_then(|response| response.terminal_tx_id) {
        Some(tx_id) => tx_id,
        None => {
            let tx_response: TxAppendResponse =
                runtime.block_on(client.post_json("/tx", &tx_request))?;
            tx_response.tx_id
        }
    };

    println!(
        "finalized: {} {} tx={} run={} last={}",
        task,
        tx_type,
        tx_id,
        run.run_id,
        last_path.display()
    );
    Ok(())
}

/// Resolve the git worktree toplevel from `cwd` via `git rev-parse
/// --show-toplevel`. Unlike [`find_project_root`]'s `.orgasmic/project.org`
/// marker walk, this stops at the worktree boundary (a linked worktree has
/// its own `.git` file) rather than escaping into the manager's live repo
/// root (TASK-QKQ3R). Canonicalized so comparisons are stable across the
/// macOS `/var` vs `/private/var` gotcha.
fn git_toplevel(cwd: &Path) -> Result<PathBuf> {
    let output = Command::new("git")
        .args(["rev-parse", "--show-toplevel"])
        .current_dir(cwd)
        .output()
        .context("git rev-parse --show-toplevel")?;
    if !output.status.success() {
        bail!(
            "git rev-parse --show-toplevel failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }
    let path = PathBuf::from(String::from_utf8_lossy(&output.stdout).trim());
    std::fs::canonicalize(&path)
        .with_context(|| format!("canonicalize git toplevel {}", path.display()))
}

/// Best-effort project id for finalize: try reading it from the git
/// toplevel first, and only fall back to the `.orgasmic/project.org` marker
/// walk (which may escape the worktree) for this READ. No git write ever
/// binds to the marker-walk result — only `git_toplevel` does that
/// (TASK-QKQ3R part C). `None` when neither resolves, tolerated by
/// [`resolve_finalize_run`].
fn resolve_finalize_project_id(git_root: &Path) -> Option<String> {
    read_project_id(git_root).ok().or_else(|| {
        find_project_root()
            .ok()
            .and_then(|root| read_project_id(&root).ok())
    })
}

fn finalize_commit_message(task: &str, status: FinalizeStatus, summary: &str) -> String {
    let subject: String = strip_markdown_heading(
        summary
            .lines()
            .map(str::trim)
            .find(|line| !line.is_empty())
            .unwrap_or("worker finalize"),
    )
    .chars()
    .take(72)
    .collect();
    format!(
        "{task}: {subject}\n\norgasmic dispatch finalize --status {}",
        status.as_str()
    )
}

/// Strip leading markdown heading markers (`#`, `##`, ... followed by
/// whitespace) so commit subjects don't read `TASK-X: # TASK-X (...)` when a
/// summary's first line is a markdown heading.
fn strip_markdown_heading(line: &str) -> &str {
    let stripped = line.trim_start_matches('#');
    if stripped.len() != line.len() {
        stripped.trim_start()
    } else {
        line
    }
}

fn worktree_status_porcelain(project_root: &Path) -> Result<Vec<u8>> {
    let output = Command::new("git")
        .args(["status", "--porcelain=v1", "-z", "--untracked-files=all"])
        .current_dir(project_root)
        .output()
        .context("git status --porcelain=v1 -z --untracked-files=all")?;
    if !output.status.success() {
        bail!(
            "git status --porcelain=v1 -z --untracked-files=all failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }
    Ok(output.stdout)
}

fn worktree_has_uncommitted_changes(project_root: &Path) -> Result<bool> {
    Ok(!worktree_status_porcelain(project_root)?.is_empty())
}

/// Commit the worktree if dirty (so commit-stall is structurally impossible,
/// acceptance #2), then return the resulting HEAD sha either way.
fn commit_worktree(project_root: &Path, message: &str) -> Result<String> {
    if worktree_has_uncommitted_changes(project_root)? {
        let add = Command::new("git")
            .args(["add", "-A"])
            .current_dir(project_root)
            .output()
            .context("git add -A")?;
        if !add.status.success() {
            bail!(
                "git add -A failed: {}",
                String::from_utf8_lossy(&add.stderr)
            );
        }
        let commit = Command::new("git")
            .args(["commit", "-m", message])
            .current_dir(project_root)
            .output()
            .context("git commit")?;
        if !commit.status.success() {
            bail!(
                "git commit failed: {}",
                String::from_utf8_lossy(&commit.stderr)
            );
        }
        if worktree_has_uncommitted_changes(project_root)? {
            bail!("git commit left uncommitted changes in the worktree");
        }
    }
    let sha = Command::new("git")
        .args(["rev-parse", "HEAD"])
        .current_dir(project_root)
        .output()
        .context("git rev-parse HEAD")?;
    if !sha.status.success() {
        bail!(
            "git rev-parse HEAD failed: {}",
            String::from_utf8_lossy(&sha.stderr)
        );
    }
    Ok(String::from_utf8_lossy(&sha.stdout).trim().to_string())
}

fn changed_file_count_between(worktree: &Path, parent: &str, commit: &str) -> Result<usize> {
    let output = Command::new("git")
        .args([
            "diff-tree",
            "--no-commit-id",
            "--name-only",
            "-r",
            "-z",
            parent,
            commit,
        ])
        .current_dir(worktree)
        .output()
        .context("git diff-tree for salvage file count")?;
    if !output.status.success() {
        bail!(
            "git diff-tree for salvage file count failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }
    Ok(output
        .stdout
        .split(|byte| *byte == b'\0')
        .filter(|path| !path.is_empty())
        .count())
}

fn anchor_salvage_ref(project_root: &Path, sha: &str) -> Result<String> {
    let ref_name = format!("refs/orgasmic/salvage/{sha}");
    let existing = Command::new("git")
        .args(["rev-parse", "--verify", "--quiet", &ref_name])
        .current_dir(project_root)
        .output()
        .with_context(|| format!("resolve salvage ref {ref_name}"))?;
    if existing.status.success() {
        let existing = String::from_utf8_lossy(&existing.stdout).trim().to_string();
        if existing == sha {
            return Ok(ref_name);
        }
        bail!("salvage ref {ref_name} unexpectedly resolves to {existing}");
    }

    let zero_oid = "0".repeat(sha.len());
    let update = Command::new("git")
        .args(["update-ref", &ref_name, sha, &zero_oid])
        .current_dir(project_root)
        .output()
        .with_context(|| format!("create salvage ref {ref_name}"))?;
    if !update.status.success() {
        bail!(
            "git update-ref failed for salvage ref {ref_name}: {}{}",
            String::from_utf8_lossy(&update.stderr),
            String::from_utf8_lossy(&update.stdout)
        );
    }
    Ok(ref_name)
}

fn salvage_worktree_if_dirty(
    project_root: &Path,
    worktree: &Path,
    task: &str,
    expected_branch: &str,
    expected_branch_oid: &str,
) -> Result<Option<SalvageCommit>> {
    if !worktree_has_uncommitted_changes(worktree)? {
        return Ok(None);
    }

    let current_branch_oid = resolve_branch_oid(project_root, expected_branch)?;
    if current_branch_oid.as_deref() != Some(expected_branch_oid) {
        bail!(
            "recorded dispatch branch {expected_branch} moved before salvage (expected {expected_branch_oid}, found {})",
            current_branch_oid.as_deref().unwrap_or("<missing>")
        );
    }

    let add = Command::new("git")
        .args(["add", "-A"])
        .current_dir(worktree)
        .output()
        .context("git add -A")?;
    if !add.status.success() {
        bail!(
            "git add -A failed: {}",
            String::from_utf8_lossy(&add.stderr)
        );
    }
    let tree = Command::new("git")
        .args(["write-tree"])
        .current_dir(worktree)
        .output()
        .context("git write-tree")?;
    if !tree.status.success() {
        bail!(
            "git write-tree failed: {}",
            String::from_utf8_lossy(&tree.stderr)
        );
    }
    let tree = String::from_utf8_lossy(&tree.stdout).trim().to_string();
    let commit = Command::new("git")
        .args([
            "commit-tree",
            &tree,
            "-p",
            expected_branch_oid,
            "-m",
            &format!("{task}: manager-salvaged uncommitted worker output"),
        ])
        .current_dir(worktree)
        .output()
        .context("git commit-tree")?;
    if !commit.status.success() {
        bail!(
            "git commit-tree failed: {}",
            String::from_utf8_lossy(&commit.stderr)
        );
    }
    let sha = String::from_utf8_lossy(&commit.stdout).trim().to_string();
    let file_count = changed_file_count_between(worktree, expected_branch_oid, &sha)?;

    // Point only this linked worktree at the synthetic commit. Plain checkout
    // is deliberately fail-closed: it refuses a concurrent late write instead
    // of overwriting it, and it does not move whichever branch HEAD happened
    // to name when the worker stopped.
    let checkout = Command::new("git")
        .args(["checkout", "--detach", &sha])
        .current_dir(worktree)
        .output()
        .context("git checkout --detach salvage commit")?;
    if !checkout.status.success() {
        bail!(
            "git checkout --detach salvage commit failed: {}{}",
            String::from_utf8_lossy(&checkout.stderr),
            String::from_utf8_lossy(&checkout.stdout)
        );
    }
    if worktree_has_uncommitted_changes(worktree)? {
        bail!("git checkout left uncommitted changes in the worktree");
    }
    let ref_name = anchor_salvage_ref(project_root, &sha)?;

    Ok(Some(SalvageCommit {
        sha,
        ref_name,
        file_count,
        worktree_removed: false,
    }))
}

fn current_git_branch(project_root: &Path) -> Result<String> {
    let output = Command::new("git")
        .args(["rev-parse", "--abbrev-ref", "HEAD"])
        .current_dir(project_root)
        .output()
        .context("git rev-parse --abbrev-ref HEAD")?;
    if !output.status.success() {
        bail!(
            "git rev-parse --abbrev-ref HEAD failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }
    Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
}

/// Reverse of `default_branch`/`task_slug`: `task-wfw1n-impl` -> `TASK-WFW1N`.
fn task_from_branch(branch: &str) -> Option<String> {
    let slug = branch
        .strip_suffix("-impl")
        .or_else(|| branch.strip_suffix("-review"))
        .or_else(|| branch.strip_suffix("-arch"))
        .unwrap_or(branch);
    let rest = slug.strip_prefix("task-")?;
    if rest.is_empty() {
        return None;
    }
    Some(format!("TASK-{}", rest.to_ascii_uppercase()))
}

fn resolve_finalize_task(project_root: &Path, task_override: Option<String>) -> Result<String> {
    if let Some(task) = task_override {
        let task = task.trim().to_string();
        if task.is_empty() {
            bail!("--task must not be empty");
        }
        return Ok(task);
    }
    let branch = current_git_branch(project_root)?;
    task_from_branch(&branch).ok_or_else(|| {
        anyhow::anyhow!("could not derive task from branch `{branch}`; pass --task explicitly")
    })
}

/// Resolve the live run to finalize from an explicit run id (typically
/// `ORGASMIC_RUN_ID` exported into a stage or dispatch pane). Derives task
/// identity from the daemon record instead of the git branch (TASK-TZJFF).
async fn resolve_finalize_run_by_id(
    client: &DaemonClient,
    project_id: Option<&str>,
    run_id: &str,
) -> Result<LiveRunInfo> {
    // An exact run id is sufficient supervisor authority. Do not enumerate
    // every durable recovery record just to resolve the worker that is
    // finalizing; unrelated stale sessions must not block its terminal action.
    let run = client
        .get::<LiveRunResponse>(&format!("/runs/{}", path_segment(run_id)))
        .await
        .map_err(|error| anyhow::anyhow!("no live run {run_id}; already released? ({error})"))?
        .run;
    if let (Some(run_project), Some(project_id)) = (run.project_id.as_deref(), project_id) {
        if run_project != project_id {
            bail!(
                "run {} belongs to project {run_project}, not {project_id}; refusing to finalize",
                run.run_id
            );
        }
    }
    Ok(run)
}

/// Resolve the live run to finalize: an explicit `--run-id` is used as-is
/// (fetched from `/runs/:id` to recover its kind/last_path); otherwise the
/// single live run from `/runs/live` matching `task` (and `project_id`, when the
/// daemon reports one) is used. Deliberately does NOT fall back to scanning
/// `.orgasmic/tx`: a worker's own worktree checkout cannot see the live
/// (uncommitted) daemon writes to the manager's `.orgasmic/tx`, so the only
/// reliable source is the daemon's in-memory live run list.
/// `project_id` is `None` when finalize couldn't resolve one at all (neither
/// the git toplevel nor the marker-walk fallback had a readable
/// `project.org`, TASK-QKQ3R part C). The project filter/guard below applies
/// only when BOTH sides know a project id; task match (plus single-live-run
/// matching in the no-`--run-id` path) stays the backstop.
async fn resolve_finalize_run(
    client: &DaemonClient,
    project_id: Option<&str>,
    task: &str,
    run_id: Option<String>,
) -> Result<LiveRunInfo> {
    if let Some(run_id) = run_id {
        return resolve_finalize_run_by_id(client, project_id, &run_id).await;
    }
    let live = client.get::<LiveRunsResponse>("/runs/live").await?.live;
    let mut matches: Vec<LiveRunInfo> = live
        .into_iter()
        .filter(|run| {
            run.task_id == task
                && run
                    .project_id
                    .as_deref()
                    .zip(project_id)
                    .map(|(run_project, project_id)| run_project == project_id)
                    .unwrap_or(true)
        })
        .collect();
    match matches.len() {
        0 => bail!("no live run found for task {task}; pass --run-id explicitly"),
        1 => Ok(matches.remove(0)),
        _ => bail!("multiple live runs found for task {task}; pass --run-id explicitly"),
    }
}

pub fn cmd_dispatch_status(home: &Home, args: DispatchStatusArgs) -> Result<()> {
    let project_root = find_live_project_root(home, "manager dispatch-status")?;
    // orgasmic:task_EP3H1 — the command an operator runs after a close warns
    // about a lost lifecycle leg is this one. Repair before reporting, so what
    // it reports is the repaired state.
    if let Ok(project_id) = read_project_id(&project_root) {
        reconcile_torn_closes_best_effort(home, &project_root, &project_id);
    }
    if args.cleanup_failed {
        let mut failures = scan_cleanup_failures(&project_root)?;
        if let Some(task) = args.task.as_deref() {
            failures.retain(|record| record.tasks.iter().any(|got| got == task));
        }
        for record in failures {
            println!(
                "TX_ID={} TASK={} TYPE={} CLEANUP_STATUS={} CLEANUP_ERROR={}",
                record.tx_id,
                task_list_property(&record.tasks),
                record.ty,
                record.status,
                record.error.as_deref().unwrap_or("-")
            );
        }
        return Ok(());
    }

    let live_runs = match DaemonClient::from_home_autostart(home) {
        Ok(client) => {
            let runtime = tokio::runtime::Runtime::new().context("create tokio runtime")?;
            runtime
                .block_on(fetch_live_runs(&client))
                .unwrap_or_default()
        }
        Err(_) => Vec::new(),
    };
    let mut open = scan_open_dispatches(&project_root)?;
    if let Some(task) = args.task.as_deref() {
        open.retain(|record| record.tasks.iter().any(|got| got == task));
    }
    for record in open {
        let health = dispatch_health(&record, &live_runs);
        if args.orphans_only && health.worktree_exists && (health.pid_alive || health.run_alive) {
            continue;
        }
        let partial_closed = partial_closed_annotation(&record);
        if args.partial_closed && partial_closed.is_none() {
            continue;
        }
        println!(
            "TX_ID={} TASK={} KIND={} STARTED_AT={} WORKTREE={} WORKER_PID={} RUN_ID={} WORKER={} DRIVER={} HARNESS={} {} {} {} {}{}",
            record.tx_id,
            task_list_property(&record.tasks),
            record.kind,
            record.started_at.as_deref().unwrap_or("-"),
            record
                .worktree
                .as_ref()
                .map(|path| path.display().to_string())
                .unwrap_or_else(|| "-".to_string()),
            record
                .worker_pid
                .or(record.pid)
                .map(|pid| pid.to_string())
                .or_else(|| health.pid.map(|pid| format!("{pid} (derived)")))
                .unwrap_or_else(|| "-".to_string()),
            record.run_id.as_deref().unwrap_or("-"),
            record.worker_id.as_deref().unwrap_or("-"),
            record.driver.as_deref().unwrap_or("-"),
            record.harness.as_deref().unwrap_or("-"),
            if health.worktree_exists {
                "[exists]"
            } else {
                "[missing]"
            },
            if health.pid_alive {
                "[pid-alive]"
            } else {
                "[pid-gone]"
            },
            if health.run_alive {
                "[run-live]"
            } else {
                "[run-gone]"
            },
            // TASK-6AYEJ: distinguishes "the worker finalized, this is waiting
            // on `dispatch-close`" from "the run vanished without reporting".
            if record.reported {
                "[reported]"
            } else {
                "[unreported]"
            },
            partial_closed
                .map(|annotation| format!(" {annotation}"))
                .unwrap_or_default()
        );
    }
    Ok(())
}

fn build_dispatch_plan(home: &Home, args: DispatchArgs) -> Result<DispatchPlan> {
    let cwd_project_root = find_project_root()?;
    let project_id = read_project_id(&cwd_project_root)?;
    let project_root = registered_project_root(home, &project_id)?;
    let tasks = normalize_tasks(args.task)?;
    if let Some(open) = latest_open_dispatch_overlapping_tasks(&project_root, &tasks)? {
        let overlapping = overlapping_tasks(&open.tasks, &tasks);
        // TASK-6AYEJ made a reported-but-unclosed dispatch a normal state, so
        // this guard now fires on the ordinary "worker finished, manager hasn't
        // closed yet" case — e.g. dispatching the reviewer before closing the
        // implementer. Name the remedy instead of leaving the manager to
        // rediscover it.
        // The hint must carry `--started-tx` (TASK-6AYEJ.2): a copyable close
        // that omits the generation token is exactly the unsafe form this fix
        // now refuses, and teaching it here is how it spreads.
        let hint = if open.reported {
            format!(
                " — its worker has reported; close it first with \
                 `orgasmic manager dispatch-close --task {} --started-tx {} \
                 --status done --merge-sha <sha>`",
                task_list_property(&open.tasks),
                open.tx_id
            )
        } else {
            String::new()
        };
        bail!(
            "dispatch already open for overlapping task(s) {} in {} (tx {}){}",
            task_list_property(&overlapping),
            task_list_property(&open.tasks),
            open.tx_id,
            hint
        );
    }
    for task in &tasks {
        validate_task_dispatchable(&project_root, task, args.kind)?;
    }
    let brief_path = canonical_existing_file(&args.brief)?;
    let brief_content = std::fs::read_to_string(&brief_path)
        .with_context(|| format!("read brief {}", brief_path.display()))?;
    let mode = args.mode.trim().to_string();
    let harness = args.harness.trim().to_string();
    if mode.is_empty() || harness.is_empty() {
        bail!("--mode and --harness are required");
    }
    orgasmic_daemon::addressing::validate_supported_pair(&mode, &harness)
        .map_err(|e| anyhow::anyhow!(e))?;
    let mut harness_args = args.harness_args;
    if let Some(json) = args.harness_args_json.as_deref() {
        let parsed: Vec<String> = serde_json::from_str(json)
            .with_context(|| format!("parse --harness-args-json: {json}"))?;
        harness_args.extend(parsed);
    }
    let from_ref = args.from.as_deref().unwrap_or("HEAD");
    let from_sha = resolve_commit(&project_root, from_ref)?;
    let worktree_path = normalize_path(&match args.worktree {
        Some(path) => absolutize(&path)?,
        None => default_worktree(&project_root, first_task(&tasks), args.kind),
    });
    let branch = args
        .branch
        .unwrap_or_else(|| default_branch(first_task(&tasks), args.kind));
    let goal_id = read_active_goal_id(&project_root)?;
    let governance = match args.governance_json.as_deref() {
        None => None,
        Some(json) => Some(
            serde_json::from_str(json)
                .with_context(|| format!("parse --governance-json: {json}"))?,
        ),
    };
    let _ = home;
    Ok(DispatchPlan {
        project_root,
        project_id,
        tasks,
        kind: args.kind,
        mode,
        harness,
        harness_args,
        brief_path,
        brief_content,
        from_sha,
        worktree_path,
        branch,
        // Model and effort are opaque provider-owned identifiers. Preserve
        // explicit CLI bytes (including case and surrounding whitespace)
        // through the HTTP boundary; provider adapters decide how to use them.
        model_override: args.model,
        effort_override: args.effort,
        // Same treatment: an opaque driver-owned value, preserved verbatim so
        // the driver can reject an unknown one by name.
        credential_mode_override: args.credential_mode,
        last_path: PathBuf::new(),
        stdout_path: PathBuf::new(),
        dispatch_attempt_token: String::new(),
        goal_id,
        reason: args
            .reason
            .map(|s| sanitize_tx_value(&s))
            .filter(|s| !s.is_empty()),
        dry_run: args.dry_run,
        governance,
    })
}

fn print_dispatch_plan(plan: &DispatchPlan) {
    println!("dispatch plan:");
    println!("  project:  {}", plan.project_id);
    println!("  task:     {}", task_list_property(&plan.tasks));
    println!("  kind:     {}", plan.kind);
    println!("  from:     {}", plan.from_sha);
    println!("  worktree: {}", plan.worktree_path.display());
    println!("  branch:   {}", plan.branch);
    println!("  brief:    {}", plan.brief_path.display());
    println!("  last:     {}", plan.last_path.display());
    println!("  stdout:   {}", plan.stdout_path.display());
    println!("  tx:       manager.dispatch_started on daemon dispatch");
    println!("  mode:     {}", plan.mode);
    println!("  harness:  {}", plan.harness);
    if !plan.harness_args.is_empty() {
        println!("  argv:     {:?}", plan.harness_args);
    }
    if let Some(model) = plan.model_override.as_deref() {
        println!("  model:    {model}");
    }
    if let Some(effort) = plan.effort_override.as_deref() {
        println!("  effort:   {effort}");
    }
    if let Some(credential_mode) = plan.credential_mode_override.as_deref() {
        println!("  cred:     {credential_mode}");
    }
}

/// orgasmic:task_6HJYT — the supervisor-local liveness answer. `dispatch-close`
/// and `dispatch-status` ask "what is running right now"; that question is not
/// the recovery inventory's, and must not be blocked by unrelated durable
/// history the inventory cannot read.
async fn fetch_live_runs(client: &DaemonClient) -> Result<Vec<RunSummary>> {
    Ok(client
        .get::<LiveRunsSummaryResponse>("/runs/live")
        .await?
        .live)
}

#[derive(Debug, Serialize)]
struct CloseGuardRequest<'a> {
    worktree_path: &'a Path,
    branch: Option<&'a str>,
    dispatch_attempt_token: Option<&'a str>,
    last_path: Option<&'a Path>,
    stdout_path: Option<&'a Path>,
    owner_pid: u32,
    releasing_run_id: Option<&'a str>,
    owned_run_ids: Vec<&'a str>,
}

#[derive(Debug, Deserialize)]
struct CloseGuardResponse {
    status: String,
    #[serde(default)]
    guard_id: Option<String>,
    #[serde(default)]
    renew_within_secs: Option<u64>,
    #[serde(default)]
    blocking_run_id: Option<String>,
    #[serde(default)]
    blocking_worktree: Option<PathBuf>,
}

#[derive(Debug, Serialize)]
struct CloseGuardRenewRequest<'a> {
    guard_id: &'a str,
}

#[derive(Debug, Deserialize)]
struct CloseGuardRenewResponse {
    status: String,
}

/// A close guard this process holds, and the heartbeat that proves it is still
/// here (TASK-AK6EM).
///
/// The daemon reclaims a guard whose holder stopped renewing. That is the only
/// reclamation signal available on targets where the daemon cannot probe a pid
/// (`subprocess_exited` has no portable non-Unix form), so the renewal runs on
/// every platform rather than only the one that needs it — a heartbeat that
/// only Windows exercised would be a heartbeat nobody ever tested.
struct HeldCloseGuard {
    guard_id: String,
    stop: Arc<AtomicBool>,
    /// Set by the heartbeat when the daemon says it no longer holds this guard.
    lost: Arc<AtomicBool>,
    heartbeat: Option<std::thread::JoinHandle<()>>,
}

impl HeldCloseGuard {
    fn id(&self) -> &str {
        &self.guard_id
    }

    fn was_lost(&self) -> bool {
        self.lost.load(atomic::Ordering::Relaxed)
    }

    fn stop_heartbeat(&mut self) {
        self.stop.store(true, atomic::Ordering::Relaxed);
        if let Some(handle) = self.heartbeat.take() {
            let _ = handle.join();
        }
    }
}

impl Drop for HeldCloseGuard {
    fn drop(&mut self) {
        self.stop_heartbeat();
    }
}

#[derive(Debug, Serialize)]
struct CloseGuardFinishRequest<'a> {
    guard_id: &'a str,
}

#[derive(Debug, Deserialize)]
struct CloseGuardFinishResponse {
    #[allow(dead_code)]
    status: String,
}

fn close_guard_path(project_id: &str, open: &DispatchRecord, suffix: &str) -> String {
    format!(
        "/projects/{}/tasks/{}/dispatch/close-guard{suffix}",
        path_segment(project_id),
        path_segment(&task_list_property(&open.tasks)),
    )
}

/// Take the daemon-owned worktree reservation a destructive `dispatch-close`
/// must hold across its liveness decision and its removal (TASK-1T3FZ), and
/// turn a refusal into the operator-facing error.
///
/// `Ok(None)` means there is no worktree to reserve — the record has no
/// `WORKTREE` property, so cleanup will report `worktree_missing` and destroy
/// nothing.
async fn reserve_close_guard(
    client: &DaemonClient,
    project_id: &str,
    open: &DispatchRecord,
) -> Result<Option<HeldCloseGuard>> {
    let Some(worktree) = open.worktree.as_deref() else {
        return Ok(None);
    };
    let request = CloseGuardRequest {
        worktree_path: worktree,
        branch: open.branch.as_deref(),
        dispatch_attempt_token: open.dispatch_attempt_token.as_deref(),
        last_path: open.last_path.as_deref(),
        stdout_path: open.stdout_path.as_deref(),
        owner_pid: std::process::id(),
        releasing_run_id: open.run_id.as_deref(),
        owned_run_ids: open.run_ids.iter().map(String::as_str).collect(),
    };
    let response: CloseGuardResponse = client
        .post_json(&close_guard_path(project_id, open, ""), &request)
        .await
        .map_err(|error| {
            // A daemon older than TASK-1T3FZ has no such route. Say so rather
            // than leave a bare 404: the close is refused, which is the right
            // direction — that daemon cannot fence this worktree at all, so it
            // cannot be shown that removing it is safe.
            if error.to_string().contains("daemon returned 404") {
                return error.context(
                    "this daemon cannot reserve the worktree for a destructive close (no \
                     close-guard route); restart it onto the current runtime (`orgasmic daemon \
                     restart`) and re-run, or close without --worktree-remove",
                );
            }
            error
        })?;
    match response.status.as_str() {
        "reserved" => {
            let guard_id = response.guard_id.ok_or_else(|| {
                anyhow::anyhow!("daemon reserved the worktree but returned no guard id")
            })?;
            Ok(Some(spawn_close_guard_heartbeat(
                client,
                &close_guard_path(project_id, open, "/renew"),
                guard_id,
                response.renew_within_secs,
            )))
        }
        "boot_reattach_pending" => bail!(
            "refusing to clean up dispatch {}: the daemon is still rehydrating runs that \
             outlived its predecessor, so it cannot yet say whether a live worker occupies \
             worktree {}. Re-run this close in a moment.",
            open.tx_id,
            worktree.display()
        ),
        "blocked" => {
            let blocking = response.blocking_run_id.unwrap_or_else(|| "?".to_string());
            bail!(
                "refusing to clean up dispatch {}: run {} is still live{}. Liveness is decided \
                 in the daemon under the same lock that admits a recovery, so this is not a \
                 stale snapshot — a replacement whose origin→replacement association never \
                 reached the ledger occupies this worktree under an id the record does not \
                 name. Inspect the live run (`orgasmic run show {}`) and let it finalize, or \
                 re-run this close without --worktree-remove.",
                open.tx_id,
                blocking,
                response
                    .blocking_worktree
                    .as_deref()
                    .map(|path| format!(" in worktree {}", path.display()))
                    .unwrap_or_default(),
                blocking,
            )
        }
        "reservation_held" => bail!(
            "refusing to clean up dispatch {}: another cleanup already holds worktree {}. \
             Let it finish, then re-run this close.",
            open.tx_id,
            worktree.display()
        ),
        other => bail!("unexpected dispatch-close guard status from daemon: {other}"),
    }
}

/// Keep renewing a held close guard until cleanup is done.
///
/// A plain OS thread with its own runtime: the cleanup it protects
/// (`cleanup_dispatch`) is synchronous and can be slow — salvage walks the whole
/// worktree — so the renewal cannot share the close's own runtime turn.
fn spawn_close_guard_heartbeat(
    client: &DaemonClient,
    renew_path: &str,
    guard_id: String,
    renew_within_secs: Option<u64>,
) -> HeldCloseGuard {
    let stop = Arc::new(AtomicBool::new(false));
    let lost = Arc::new(AtomicBool::new(false));
    // Renew at a third of the deadline the daemon asked for, so two lost
    // renewals in a row still do not drop a guard whose holder is alive.
    let interval = Duration::from_secs(renew_within_secs.unwrap_or(30).max(3) / 3);
    let heartbeat = {
        let client = client.clone();
        let renew_path = renew_path.to_string();
        let guard_id = guard_id.clone();
        let stop = Arc::clone(&stop);
        let lost = Arc::clone(&lost);
        std::thread::Builder::new()
            .name("close-guard-heartbeat".into())
            .spawn(move || {
                let Ok(runtime) = tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                else {
                    return;
                };
                loop {
                    // Sleep in short slices so `stop` ends the thread promptly.
                    let deadline = std::time::Instant::now() + interval;
                    while std::time::Instant::now() < deadline {
                        if stop.load(atomic::Ordering::Relaxed) {
                            return;
                        }
                        std::thread::sleep(Duration::from_millis(50));
                    }
                    if stop.load(atomic::Ordering::Relaxed) {
                        return;
                    }
                    let renewed = runtime.block_on(client.post_json::<_, CloseGuardRenewResponse>(
                        &renew_path,
                        &CloseGuardRenewRequest {
                            guard_id: &guard_id,
                        },
                    ));
                    match renewed {
                        Ok(response) if response.status == "renewed" => {}
                        Ok(response) => {
                            // The daemon no longer holds this guard: it was
                            // reclaimed, or a replacement never inherited it.
                            // Say so — this cleanup is no longer fenced.
                            if !lost.swap(true, atomic::Ordering::Relaxed) {
                                eprintln!(
                                    "warning: the daemon no longer holds this dispatch-close \
                                     worktree reservation (renew status={}); cleanup is \
                                     continuing without a fence",
                                    response.status
                                );
                            }
                        }
                        Err(_) => {
                            // Transport failure: the daemon may be restarting.
                            // Keep trying; the lease outlives several misses.
                        }
                    }
                }
            })
            .ok()
    };
    HeldCloseGuard {
        guard_id,
        stop,
        lost,
        heartbeat,
    }
}

/// Hand the worktree reservation back. Best effort on purpose: the close's own
/// outcome must not turn on it, and a holder that never gets here is reclaimed
/// by the daemon once its pid is gone or its holder lease expires.
fn finish_close_guard(
    runtime: &tokio::runtime::Runtime,
    client: &DaemonClient,
    project_id: &str,
    open: &DispatchRecord,
    guard: Option<&mut HeldCloseGuard>,
) {
    let Some(guard) = guard else {
        return;
    };
    guard.stop_heartbeat();
    if guard.was_lost() {
        eprintln!(
            "warning: this dispatch-close lost its worktree reservation before cleanup \
             finished; the worktree was no longer fenced while it was being removed"
        );
    }
    let result = runtime.block_on(client.post_json::<_, CloseGuardFinishResponse>(
        &close_guard_path(project_id, open, "/finish"),
        &CloseGuardFinishRequest {
            guard_id: guard.id(),
        },
    ));
    if let Err(err) = result {
        eprintln!(
            "warning: dispatch-close could not release its worktree reservation ({err}); the \
             daemon releases it when this process exits or its holder lease expires"
        );
    }
}

/// Test-only rendezvous between the guard response and the destructive work it
/// protects, mirroring `recovery_failpoint`'s env-driven shape.
///
/// The interleaving TASK-AK6EM is about — a daemon replaced *while an external
/// process holds a close guard* — cannot be driven from inside the daemon: by
/// construction the holder is another process, and the window is exactly the
/// one where it is not talking to the daemon at all. No-op unless the env var
/// names a file.
fn dispatch_close_pause_after_guard() {
    let Ok(raw) = std::env::var("ORGASMIC_DISPATCH_CLOSE_PAUSE_FILE") else {
        return;
    };
    if raw.is_empty() {
        return;
    }
    let path = PathBuf::from(raw);
    let _ = std::fs::write(path.with_extension("reached"), "1");
    while path.exists() {
        std::thread::sleep(Duration::from_millis(25));
    }
}

async fn release_dispatch_run(
    client: &DaemonClient,
    run_id: &str,
    task_property: &str,
) -> Result<RunReleaseResponse> {
    release_dispatch_run_with_reason(
        client,
        run_id,
        &format!("dispatch close for {task_property}"),
        task_property,
        false,
        None,
        None,
    )
    .await
}

/// Shared release call for both `dispatch-close` (manager authority) and
/// `dispatch finalize` (worker authority, dec_3M7M0) — same terminal
/// endpoint, differing only in reason text, `finalized_by_worker`,
/// `caller_identity` (only `dispatch finalize` presents one; TASK-DWJVH item A)
/// and `terminal_tx` (only `dispatch finalize` sends one; TASK-WGXKD).
async fn release_dispatch_run_with_reason(
    client: &DaemonClient,
    run_id: &str,
    reason: &str,
    request_slug_source: &str,
    finalized_by_worker: bool,
    caller_identity: Option<&RuntimeIdentity>,
    terminal_tx: Option<TxAppendRequest>,
) -> Result<RunReleaseResponse> {
    let request = RunReleaseRequest {
        reason: Some(reason.to_string()),
        request_id: Some(format!(
            "dispatch-release-{}-{}",
            request_slug(request_slug_source),
            Uuid::new_v4()
        )),
        finalized_by_worker,
        caller_identity: caller_identity.cloned(),
        terminal_tx,
    };
    client
        .post_json(&format!("/runs/{}/release", path_segment(run_id)), &request)
        .await
}

/// Refuse to release the lease unless the daemon has proven, BEFORE the
/// release call, that it will write the terminal tx as part of that release
/// (TASK-WGXKD.1, reviewer finding 1).
///
/// Why a pre-flight probe and not a post-release fallback: on acp-stdio there
/// is no "after the release" for this process. The release tears down the
/// driver, the driver reaps the harness's setsid process group, and this CLI is
/// in it — so a fallback that runs after the release call returns never runs at
/// all. New CLI against a not-yet-restarted daemon (a source build, or the
/// window between a runtime install and the daemon kickstart) otherwise
/// reproduces the original defect exactly: committed, reported to last.txt,
/// lease released, no terminal tx, no orphan flag.
///
/// NOTE FOR THE NEXT READER — this deliberately inverts the invariant the
/// original finalize-ordering comment argued for. Failing here leaves the LEASE
/// HELD, which that comment was written to avoid. Take the trade anyway: a held
/// lease is visible in `dispatch-status`, and the orphan/stall paths can rescue
/// it. A released-but-unreported run is the invisible fourth state TASK-WGXKD
/// exists to eliminate — nothing surfaces it except an `[unreported]` marker
/// nobody is looking at. Visible-and-wrong beats invisible-and-wrong. Do not
/// "fix" this back into a post-release fallback.
async fn require_daemon_writes_terminal_tx(client: &DaemonClient, task: &str) -> Result<()> {
    let probe = client
        .get::<DaemonCapabilitiesResponse>("/daemon/capabilities")
        .await;
    let detail = match &probe {
        Ok(response) => {
            if response
                .capabilities
                .iter()
                .any(|capability| capability == CAPABILITY_RELEASE_TERMINAL_TX)
            {
                return Ok(());
            }
            format!("it does not advertise `{CAPABILITY_RELEASE_TERMINAL_TX}`")
        }
        // A daemon older than TASK-WGXKD has no such route and answers 404;
        // any other probe failure is equally not a proof of support.
        Err(error) => format!("the capability probe failed: {error}"),
    };
    bail!(
        "refusing to release the lease for {task}: this daemon cannot be shown to write \
         the worker's terminal tx as part of the release ({detail}). Releasing anyway \
         would report nothing and leave no orphan flag — the run would simply vanish \
         from reporting. The lease is still held and this finalize is safe to re-run: \
         restart the daemon onto the current runtime (`orgasmic daemon restart`), then \
         run the same `orgasmic dispatch finalize` command again."
    )
}

/// Whether `err` is the daemon's "run not found" response to a release call
/// — i.e. some other party (the stall sweep, in practice) already released
/// this run before this call landed. Distinct from every other release
/// failure (in particular an ownership mismatch, meaning a *different* run
/// reclaimed this run_id), which must still hard-error (TASK-DWJVH item B).
fn is_release_run_not_found_error(err: &anyhow::Error) -> bool {
    err.to_string().contains("daemon returned 404")
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum DispatchReleaseTombstone {
    WorkerFinalized,
    /// Artifactor terminal declaration (`orgasmic artifact submit`).
    ArtifactSubmitted,
    /// Manager terminal declaration (`orgasmic manager release`).
    ManagerReleased,
    StallSweep,
    ProtocolEndWithoutFinalize,
    Unrecognized,
    None,
}

// orgasmic:TASK-JK66P — matched on the reason's FIRST TOKEN, not the whole
// string. A stall release now appends what evidence was absent and for how long
// (`stall_timeout_exceeded: no work evidence for 612s; …`) so an operator can
// tell a wedged harness from a worker shot mid-build; the classification must
// not change because the tombstone got more informative.
fn is_stall_sweep_release_reason(reason: &str) -> bool {
    matches!(
        release_reason_token(reason),
        "stall_timeout_exceeded" | "max_run_duration_exceeded" | "idle_timeout_exceeded"
    )
}

/// The leading `snake_case` token of a release reason, before any `:` detail.
fn release_reason_token(reason: &str) -> &str {
    reason.split(':').next().unwrap_or(reason).trim()
}

#[cfg(test)]
fn is_worker_declared_tombstone(tombstone: DispatchReleaseTombstone) -> bool {
    matches!(
        tombstone,
        DispatchReleaseTombstone::WorkerFinalized
            | DispatchReleaseTombstone::ArtifactSubmitted
            | DispatchReleaseTombstone::ManagerReleased
    )
}

/// Last `Lifecycle::Release` on the run's session JSONL — the daemon's
/// terminal tombstone once the lease is gone.
fn dispatch_release_tombstone(session_path: &Path) -> Result<DispatchReleaseTombstone> {
    let envelopes = read_session_file(session_path)
        .with_context(|| format!("read session tombstone {}", session_path.display()))?;
    for envelope in envelopes.into_iter().rev() {
        if envelope.kind != SessionEventKind::Lifecycle {
            continue;
        }
        let Ok(Lifecycle::Release {
            reason,
            finalized_by_worker,
            ..
        }) = serde_json::from_value(envelope.event.clone())
        else {
            continue;
        };
        // orgasmic:TASK-S52X9 — extend tombstone vocab with artifact_submitted
        // and manager_released as valid worker declarations (same
        // finalized_by_worker flag; distinct reasons).
        return Ok(if finalized_by_worker {
            match reason.as_str() {
                "artifact_submitted" => DispatchReleaseTombstone::ArtifactSubmitted,
                "manager_released" => DispatchReleaseTombstone::ManagerReleased,
                _ => DispatchReleaseTombstone::WorkerFinalized,
            }
        } else if reason == "protocol_end_without_finalize" {
            DispatchReleaseTombstone::ProtocolEndWithoutFinalize
        } else if is_stall_sweep_release_reason(&reason) {
            DispatchReleaseTombstone::StallSweep
        } else {
            DispatchReleaseTombstone::Unrecognized
        });
    }
    Ok(DispatchReleaseTombstone::None)
}

/// Test-only: artificial delay between writing last.txt and releasing the
/// lease in `cmd_dispatch_finalize`, so integration tests can deterministically
/// land a concurrent stall-sweep release inside the window the release step
/// is resilient to (TASK-DWJVH item B). Unset in production — zero effect.
fn finalize_release_delay_for_tests() -> Option<std::time::Duration> {
    std::env::var("ORGASMIC_TEST_FINALIZE_RELEASE_DELAY_MS")
        .ok()
        .and_then(|raw| raw.parse::<u64>().ok())
        .filter(|ms| *ms > 0)
        .map(std::time::Duration::from_millis)
}

/// Test-only: SIGKILL this process the instant the release call in
/// `cmd_dispatch_finalize` returns, reproducing what the release itself does to
/// a real worker (the driver teardown reaps the harness's setsid process group,
/// and this CLI is in it — TASK-WGXKD). Unset in production — zero effect.
fn finalize_kill_self_after_release_for_tests() {
    if std::env::var("ORGASMIC_TEST_FINALIZE_KILL_SELF_AFTER_RELEASE").as_deref() != Ok("1") {
        return;
    }
    // SIGKILL, not exit(): nothing in this process may get another turn, no
    // destructor, no flush — exactly the production death.
    unsafe {
        libc::kill(std::process::id() as libc::pid_t, libc::SIGKILL);
    }
}

/// The transition a close intends for one task, when it has one.
fn transition_for<'a>(
    transitions: &'a [CloseTransition],
    task: &str,
) -> Option<&'a CloseTransition> {
    transitions
        .iter()
        .find(|transition| transition.task == task)
}

/// Record the intended lifecycle move on the close tx (TASK-EP3H1). Absent
/// only when the task could not be read at all, in which case there is no
/// transition to lose.
fn push_lifecycle_extra(extra: &mut Vec<(String, String)>, transition: Option<&CloseTransition>) {
    if let Some(transition) = transition {
        extra.push((
            LIFECYCLE_FROM_KEY.to_string(),
            transition.from.as_str().to_string(),
        ));
        extra.push((
            LIFECYCLE_TO_KEY.to_string(),
            transition.to.as_str().to_string(),
        ));
    }
}

/// What a close knows about itself by the time it writes its tx: the terminal
/// tx vocabulary, the merge it landed, what cleanup did, and the lifecycle move
/// it is about to attempt.
struct CloseTxFacts<'a> {
    tx_type: &'a str,
    merge_sha: Option<&'a str>,
    cleanup: &'a CleanupOutcome,
    transition: Option<&'a CloseTransition>,
}

fn close_done_request(
    project_id: &str,
    open: &DispatchRecord,
    task: &str,
    args: &DispatchCloseArgs,
    facts: &CloseTxFacts<'_>,
) -> TxAppendRequest {
    let CloseTxFacts {
        tx_type,
        merge_sha,
        cleanup,
        transition,
    } = *facts;
    let mut extra = Vec::new();
    if let Some(session) = optional_value(args.worker_session.as_deref()) {
        extra.push(("WORKER_SESSION".to_string(), session));
    }
    if let Some(model) = optional_value(open.model.as_deref()) {
        extra.push(("MODEL".to_string(), model));
    }
    if let Some(effort) = optional_value(open.effort.as_deref()) {
        extra.push(("EFFORT".to_string(), effort));
    }
    if let Some(commit) = optional_value(args.worker_commit.as_deref()) {
        extra.push(("WORKER_COMMIT".to_string(), commit));
    }
    if matches!(tx_type, "implementer.done" | "architector.done") {
        if let Some(merge_sha) = merge_sha {
            extra.push(("MERGE_SHA".to_string(), merge_sha.to_string()));
        }
        if let Some(branch) = optional_value(open.branch.as_deref()) {
            extra.push(("BRANCH".to_string(), branch));
        }
    }
    if let Some(wall) = optional_value(args.wall.as_deref()) {
        extra.push(("WALL".to_string(), wall));
    }
    if let Some(tokens) = args.tokens {
        extra.push(("TOKENS".to_string(), tokens.to_string()));
    }
    if let Some(reviewed_diff) = optional_value(args.reviewed_diff.as_deref()) {
        extra.push(("REVIEWED_DIFF".to_string(), reviewed_diff));
    }
    for (key, value) in &args.properties {
        extra.push((key.clone(), sanitize_tx_value(value)));
    }
    extra.push(("CLOSED_TX".to_string(), open.tx_id.clone()));
    push_lifecycle_extra(&mut extra, transition);
    push_cleanup_extra(&mut extra, cleanup);
    if let Some(goal_id) = optional_value(open.goal_id.as_deref()) {
        extra.push(("GOAL_ID".to_string(), goal_id));
    }
    TxAppendRequest {
        // Deterministic per (task, dispatch generation), not per invocation
        // (TASK-6AYEJ.1): a replayed close of the same generation dedupes at
        // the writer instead of appending a second terminal tx.
        request_id: Some(format!(
            "dispatch-close-{}-{}",
            request_slug(task),
            open.tx_id
        )),
        ty: tx_type.to_string(),
        actor: Some(format!("agent.{}", open.kind)),
        machine: None,
        project: Some(project_id.to_string()),
        task: Some(task.to_string()),
        target: None,
        reason: args
            .reason
            .as_ref()
            .map(|s| sanitize_tx_value(s))
            .filter(|s| !s.is_empty()),
        extra,
        tx_path: None,
    }
}

fn close_aborted_request(
    project_id: &str,
    open: &DispatchRecord,
    task: &str,
    reason: &str,
    cleanup: &CleanupOutcome,
    transition: Option<&CloseTransition>,
) -> TxAppendRequest {
    let mut extra = vec![("CLOSED_TX".to_string(), open.tx_id.clone())];
    if let Some(worktree) = &open.worktree {
        extra.push(("WORKTREE".to_string(), worktree.display().to_string()));
    }
    push_lifecycle_extra(&mut extra, transition);
    push_cleanup_extra(&mut extra, cleanup);
    TxAppendRequest {
        // Deterministic per (task, dispatch generation) — see close_done_request.
        request_id: Some(format!(
            "dispatch-aborted-{}-{}",
            request_slug(task),
            open.tx_id
        )),
        ty: "manager.dispatch_aborted".to_string(),
        actor: None,
        machine: None,
        project: Some(project_id.to_string()),
        task: Some(task.to_string()),
        target: None,
        reason: Some(sanitize_tx_value(reason)),
        extra,
        tx_path: None,
    }
}

fn done_tx_type(open: &DispatchRecord) -> Result<&'static str> {
    done_tx_type_for_kind(&open.kind)
}

/// Shared by `dispatch-close` (kind read back from a `DispatchRecord`) and
/// `dispatch finalize` (kind read from the daemon's live `RunSummary`) so both
/// converge on the same terminal-tx vocabulary.
fn done_tx_type_for_kind(kind: &str) -> Result<&'static str> {
    // orgasmic:TASK-S52X9 — stage grill/plan accept finalize; their terminal
    // txs mirror the dispatch-worker `*.done` vocabulary. Stage completion
    // watchers still emit `grill.completed` / `plan.completed` from the
    // finalize tombstone.
    match kind {
        "implementer" => Ok("implementer.done"),
        "reviewer" => Ok("reviewer.done"),
        "architector" => Ok("architector.done"),
        "griller" => Ok("griller.done"),
        "planner" => Ok("planner.done"),
        other => bail!("cannot close dispatch kind `{other}` as done"),
    }
}

/// The tx a worker's own `dispatch finalize --status done` emits.
///
/// orgasmic:TASK-6AYEJ — a worker reports that IT is finished; it does not get
/// to declare the DISPATCH closed. Closing means the manager read the report,
/// merged, and released the worktree and branch, and only `dispatch-close` can
/// say that. So the dispatch-worker kinds — the ones
/// [`scan_open_dispatches`] treats as terminal — finalize with a
/// `*.reported` tx that leaves the dispatch open, and `*.done` stays the
/// manager's word. dec_3M7M0 is untouched: finalize is still the worker's sole
/// success signal (report + commit + lease release in one call), still the last
/// thing a worker persona does, and the daemon still keys completion off the
/// finalize tombstone rather than the tx type.
///
/// Stage kinds (`griller`/`planner`) keep the terminal `*.done`: they have no
/// `manager.dispatch_started` record and no manager close, so there is nothing
/// for a report-only tx to leave open.
///
/// `architector` is deliberately in the reported set even though it is ALSO a
/// stage kind, because finalize sees only `run.kind` and cannot tell a
/// dispatched architector from `stage architect` on main. Dispatched
/// architectors are closed by the manager (`architector-watch-then-integrate`,
/// `--merge-sha` required), so treating them as terminal would leave this whole
/// defect in place for that kind; the stage direction costs nothing, since a
/// stage run has no dispatch record to leave open and stage completion is
/// derived from the finalize tombstone, not the tx type.
fn finalize_tx_type_for_kind(kind: &str) -> Result<&'static str> {
    match kind {
        "implementer" => Ok("implementer.reported"),
        "reviewer" => Ok("reviewer.reported"),
        "architector" => Ok("architector.reported"),
        other => done_tx_type_for_kind(other),
    }
}

fn create_worktree(project_root: &Path, path: &Path, branch: &str, from_sha: &str) -> Result<()> {
    if path.exists() {
        bail!("worktree path already exists: {}", path.display());
    }
    let parent = path
        .parent()
        .ok_or_else(|| anyhow::anyhow!("worktree path has no parent: {}", path.display()))?;
    let parent_preexisted = parent.exists();
    if !parent_preexisted {
        std::fs::create_dir_all(parent).with_context(|| format!("create {}", parent.display()))?;
    }
    let output = Command::new("git")
        .args(["worktree", "add", "-b", branch])
        .arg(path)
        .arg(from_sha)
        .current_dir(project_root)
        .output()
        .with_context(|| format!("git worktree add {}", path.display()))?;
    if !output.status.success() {
        if !parent_preexisted {
            let _ = std::fs::remove_dir(parent);
        }
        bail!(
            "git worktree add failed: {}{}",
            String::from_utf8_lossy(&output.stderr),
            String::from_utf8_lossy(&output.stdout)
        );
    }
    // Note: a dispatched `claude` in this fresh worktree shows the "Is this a
    // project you trust?" dialog (`--dangerously-skip-permissions` does NOT
    // clear it in Claude 2.1.x). The driver accepts that dialog by sending a
    // keystroke before pasting the brief — see `accept_folder_trust` in the
    // tmux/rmux drivers — so no global Claude config mutation is needed here.
    Ok(())
}

struct DispatchCleanupLock(std::fs::File);

impl Drop for DispatchCleanupLock {
    fn drop(&mut self) {
        let _ = fs2::FileExt::unlock(&self.0);
    }
}

fn acquire_dispatch_cleanup_lock(project_root: &Path) -> Result<DispatchCleanupLock> {
    let output = Command::new("git")
        .args(["rev-parse", "--git-common-dir"])
        .current_dir(project_root)
        .output()
        .context("git rev-parse --git-common-dir")?;
    if !output.status.success() {
        bail!(
            "git rev-parse --git-common-dir failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }
    let common_dir = PathBuf::from(String::from_utf8_lossy(&output.stdout).trim().to_string());
    let common_dir = if common_dir.is_absolute() {
        common_dir
    } else {
        project_root.join(common_dir)
    };
    let path = common_dir.join("orgasmic-dispatch-cleanup.lock");
    let file = std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(&path)
        .with_context(|| format!("open cleanup lock {}", path.display()))?;
    // MSRV 1.87: call fs2 explicitly; the std methods stabilized in 1.89.
    fs2::FileExt::lock_exclusive(&file)
        .with_context(|| format!("lock dispatch cleanup {}", path.display()))?;
    Ok(DispatchCleanupLock(file))
}

fn remove_worktree_if_present(
    project_root: &Path,
    path: &Path,
    last_path: Option<&Path>,
    stdout_path: Option<&Path>,
    task: &str,
    branch: Option<&str>,
    expected_branch_oid: Option<&str>,
) -> Result<WorktreeRemovalOutcome> {
    if !path.exists() {
        return Ok(WorktreeRemovalOutcome {
            removed: false,
            salvage: None,
            error: None,
        });
    }
    remove_worktree_required(
        project_root,
        path,
        last_path,
        stdout_path,
        task,
        branch,
        expected_branch_oid,
    )
}

fn remove_worktree_required(
    project_root: &Path,
    path: &Path,
    last_path: Option<&Path>,
    stdout_path: Option<&Path>,
    task: &str,
    branch: Option<&str>,
    expected_branch_oid: Option<&str>,
) -> Result<WorktreeRemovalOutcome> {
    remove_worktree_required_with_hook(
        project_root,
        path,
        last_path,
        stdout_path,
        task,
        branch,
        expected_branch_oid,
        |_| {},
    )
}

#[allow(clippy::too_many_arguments)]
fn remove_worktree_required_with_hook(
    project_root: &Path,
    path: &Path,
    last_path: Option<&Path>,
    stdout_path: Option<&Path>,
    task: &str,
    branch: Option<&str>,
    expected_branch_oid: Option<&str>,
    before_remove: impl FnOnce(&Path),
) -> Result<WorktreeRemovalOutcome> {
    let _cleanup_lock = acquire_dispatch_cleanup_lock(project_root)?;
    if !path.exists() {
        bail!("worktree path missing: {}", path.display());
    }
    let artifacts = orgasmic_core::validate_dispatch_cleanup_targets(
        project_root,
        path,
        last_path,
        stdout_path,
    )
    .map_err(|err| anyhow::anyhow!(err))?;
    orgasmic_core::verify_dispatch_worktree_identity(&artifacts, path)
        .map_err(|err| anyhow::anyhow!(err))?;
    let mut salvage = if worktree_has_uncommitted_changes(path)? {
        let branch =
            branch.ok_or_else(|| anyhow::anyhow!("open dispatch has no recorded branch"))?;
        let resolved_oid = match expected_branch_oid {
            Some(oid) => oid.to_string(),
            None => resolve_branch_oid(project_root, branch)?.ok_or_else(|| {
                anyhow::anyhow!("recorded dispatch branch {branch} does not exist")
            })?,
        };
        salvage_worktree_if_dirty(project_root, path, task, branch, &resolved_oid)?
    } else {
        None
    };
    before_remove(path);
    let output = match Command::new("git")
        .args(["worktree", "remove"])
        .arg(path)
        .current_dir(project_root)
        .output()
    {
        Ok(output) => output,
        Err(err) => {
            return Ok(WorktreeRemovalOutcome {
                removed: false,
                salvage,
                error: Some(format!("git worktree remove {}: {err}", path.display())),
            });
        }
    };
    if !output.status.success() {
        return Ok(WorktreeRemovalOutcome {
            removed: false,
            salvage,
            error: Some(format!(
                "git worktree remove failed: {}{}",
                String::from_utf8_lossy(&output.stderr),
                String::from_utf8_lossy(&output.stdout)
            )),
        });
    }
    if let Some(salvage) = &mut salvage {
        salvage.worktree_removed = true;
    }
    let error = orgasmic_core::prune_validated_dispatch_attempt(&artifacts)
        .err()
        .map(|err| format!("prune dispatch artifacts for {}: {err}", path.display()));
    Ok(WorktreeRemovalOutcome {
        removed: true,
        salvage,
        error,
    })
}

async fn request_daemon_dispatch_cleanup(
    client: &crate::daemon_client::DaemonClient,
    plan: &DispatchPlan,
) -> Result<CleanupOutcome> {
    let response = client.post_dispatch_cleanup(plan).await?;
    daemon_cleanup_to_outcome(&response)
}

fn daemon_cleanup_to_outcome(
    response: &crate::daemon_client::DispatchCleanupResponse,
) -> Result<CleanupOutcome> {
    let mut errors = Vec::new();
    if let Some(error) = response.error.as_deref() {
        errors.push(error.to_string());
    }
    let status = match response.status.as_str() {
        "ok" | "noop" => CleanupStatus::Ok,
        "partial" => CleanupStatus::Partial,
        "failed" if !response.worktree_removed && !response.branch_deleted => {
            CleanupStatus::WorktreeFailed
        }
        "failed" if !response.worktree_removed => CleanupStatus::WorktreeFailed,
        "failed" if !response.branch_deleted => CleanupStatus::BranchFailed,
        "failed" => CleanupStatus::Partial,
        other => {
            errors.push(format!("unexpected daemon cleanup status: {other}"));
            CleanupStatus::Partial
        }
    };
    Ok(CleanupOutcome {
        status,
        error: if errors.is_empty() {
            None
        } else {
            Some(sanitize_tx_value(&errors.join("; ")))
        },
        salvage: None,
    })
}

fn cleanup_created_resources(
    project_root: &Path,
    path: &Path,
    branch: &str,
    task: &str,
    last_path: &Path,
    stdout_path: &Path,
) -> CleanupOutcome {
    let mut worktree_failed = false;
    let mut branch_failed = false;
    let mut errors = Vec::new();
    let expected_branch_oid = match resolve_branch_oid(project_root, branch) {
        Ok(oid) => oid,
        Err(err) => {
            return CleanupOutcome {
                status: CleanupStatus::BranchFailed,
                error: Some(sanitize_tx_value(&format!("branch validation: {err}"))),
                salvage: None,
            };
        }
    };

    let mut salvage = None;
    let mut worktree_removed = false;
    match remove_worktree_if_present(
        project_root,
        path,
        Some(last_path),
        Some(stdout_path),
        task,
        Some(branch),
        expected_branch_oid.as_deref(),
    ) {
        Ok(outcome) => {
            worktree_removed = outcome.removed;
            salvage = outcome.salvage;
            if let Some(err) = outcome.error {
                worktree_failed = true;
                errors.push(format!("worktree: {err}"));
            } else if !worktree_removed {
                worktree_failed = true;
                errors.push("worktree: path missing before cleanup".to_string());
            }
        }
        Err(err) => {
            worktree_failed = true;
            errors.push(format!("worktree: {err}"));
        }
    }
    if !worktree_failed && worktree_removed {
        if let Err(err) =
            delete_branch_if_matches(project_root, branch, expected_branch_oid.as_deref())
        {
            branch_failed = true;
            errors.push(format!("branch: {err}"));
        }
    }

    let status = match (worktree_failed, branch_failed) {
        (false, false) => CleanupStatus::Ok,
        (true, false) => CleanupStatus::WorktreeFailed,
        (false, true) => CleanupStatus::BranchFailed,
        (true, true) => CleanupStatus::Partial,
    };
    CleanupOutcome {
        status,
        error: if errors.is_empty() {
            None
        } else {
            Some(sanitize_tx_value(&errors.join("; ")))
        },
        salvage,
    }
}

fn resolve_project_path(project_root: &Path, path: Option<PathBuf>) -> Option<PathBuf> {
    path.map(|mut path| {
        if path.is_relative() {
            path = project_root.join(path);
        }
        path
    })
}

fn cleanup_dispatch(
    project_root: &Path,
    open: &DispatchRecord,
    remove_worktree: bool,
    branch_delete: bool,
) -> CleanupOutcome {
    let last_path = resolve_project_path(project_root, open.last_path.clone());
    let stdout_path = resolve_project_path(project_root, open.stdout_path.clone());
    let mut worktree_failed = false;
    let mut branch_failed = false;
    let mut worktree_missing = false;
    let mut worktree_removed = false;
    let mut errors = Vec::new();
    let mut salvage = None;
    let expected_branch_oid = if branch_delete {
        match &open.branch {
            Some(branch) => match resolve_branch_oid(project_root, branch) {
                Ok(oid) => oid,
                Err(err) => {
                    return CleanupOutcome {
                        status: CleanupStatus::BranchFailed,
                        error: Some(sanitize_tx_value(&format!("branch validation: {err}"))),
                        salvage: None,
                    };
                }
            },
            None => {
                return CleanupOutcome {
                    status: CleanupStatus::BranchFailed,
                    error: Some("branch: open dispatch has no BRANCH property".to_string()),
                    salvage: None,
                };
            }
        }
    } else {
        None
    };

    if remove_worktree {
        match &open.worktree {
            Some(worktree) => {
                match remove_worktree_required(
                    project_root,
                    worktree,
                    last_path.as_deref(),
                    stdout_path.as_deref(),
                    &task_list_property(&open.tasks),
                    open.branch.as_deref(),
                    expected_branch_oid.as_deref(),
                ) {
                    Ok(outcome) => {
                        worktree_removed = outcome.removed;
                        salvage = outcome.salvage;
                        if let Some(err) = outcome.error {
                            worktree_failed = true;
                            errors.push(format!("worktree: {err}"));
                        }
                    }
                    Err(err) => {
                        worktree_failed = true;
                        errors.push(format!("worktree: {err}"));
                    }
                }
            }
            None => {
                worktree_missing = true;
                errors.push("worktree: open dispatch has no WORKTREE property".to_string());
            }
        }
    }

    if branch_delete && !remove_worktree {
        branch_failed = true;
        errors.push("branch: deletion requires successful worktree removal".to_string());
    } else if branch_delete && !worktree_failed && !worktree_missing && worktree_removed {
        match &open.branch {
            Some(branch) => {
                if let Err(err) =
                    delete_branch_if_matches(project_root, branch, expected_branch_oid.as_deref())
                {
                    branch_failed = true;
                    errors.push(format!("branch: {err}"));
                }
            }
            None => {
                branch_failed = true;
                errors.push("branch: open dispatch has no BRANCH property".to_string());
            }
        }
    } else if branch_delete && !worktree_failed && !worktree_missing {
        branch_failed = true;
        errors.push("branch: worktree was not removed".to_string());
    }

    let status = match (worktree_missing, worktree_failed, branch_failed) {
        (false, false, false) => CleanupStatus::Ok,
        (true, false, false) => CleanupStatus::WorktreeMissing,
        (false, true, false) => CleanupStatus::WorktreeFailed,
        (false, false, true) => CleanupStatus::BranchFailed,
        _ => CleanupStatus::Partial,
    };
    CleanupOutcome {
        status,
        error: if errors.is_empty() {
            None
        } else {
            Some(sanitize_tx_value(&errors.join("; ")))
        },
        salvage,
    }
}

fn push_cleanup_extra(extra: &mut Vec<(String, String)>, cleanup: &CleanupOutcome) {
    extra.push((
        "CLEANUP_STATUS".to_string(),
        cleanup.status.as_str().to_string(),
    ));
    if let Some(error) = optional_value(cleanup.error.as_deref()) {
        extra.push(("CLEANUP_ERROR".to_string(), error));
    }
    if let Some(salvage) = &cleanup.salvage {
        extra.push(("SALVAGE_SHA".to_string(), salvage.sha.clone()));
        extra.push(("SALVAGE_REF".to_string(), salvage.ref_name.clone()));
        extra.push((
            "SALVAGE_FILE_COUNT".to_string(),
            salvage.file_count.to_string(),
        ));
    }
}

fn cleanup_status_reports_warning(status: CleanupStatus) -> bool {
    !matches!(status, CleanupStatus::Ok | CleanupStatus::CleanupAlreadyRun)
}

fn resolve_branch_oid(project_root: &Path, branch: &str) -> Result<Option<String>> {
    let valid = Command::new("git")
        .args(["check-ref-format", "--branch", branch])
        .current_dir(project_root)
        .output()
        .with_context(|| format!("validate branch {branch}"))?;
    if !valid.status.success() {
        bail!("invalid branch name {branch}");
    }
    let branch_ref = format!("refs/heads/{branch}");
    let output = Command::new("git")
        .args(["rev-parse", "--verify", "--quiet", &branch_ref])
        .current_dir(project_root)
        .output()
        .with_context(|| format!("resolve branch {branch}"))?;
    if !output.status.success() {
        return Ok(None);
    }
    let oid = String::from_utf8_lossy(&output.stdout).trim().to_string();
    if oid.is_empty() {
        bail!("branch {branch} resolved to an empty object id");
    }
    Ok(Some(oid))
}

fn delete_branch_if_matches(
    project_root: &Path,
    branch: &str,
    expected_oid: Option<&str>,
) -> Result<()> {
    let Some(expected_oid) = expected_oid else {
        return Ok(());
    };
    let branch_ref = format!("refs/heads/{branch}");
    let output = Command::new("git")
        .args(["update-ref", "-d", &branch_ref, expected_oid])
        .current_dir(project_root)
        .output()
        .with_context(|| format!("delete branch {branch} at {expected_oid}"))?;
    if !output.status.success() {
        bail!(
            "git update-ref -d failed: {}{}",
            String::from_utf8_lossy(&output.stderr),
            String::from_utf8_lossy(&output.stdout)
        );
    }
    Ok(())
}

pub(crate) fn resolve_project(project: Option<String>) -> Result<String> {
    match project {
        Some(id) if !id.is_empty() => Ok(id),
        _ => {
            let root = find_project_root()?;
            read_project_id(&root)
        }
    }
}

pub(crate) fn find_project_root() -> Result<PathBuf> {
    let mut dir = std::env::current_dir().context("cwd")?;
    loop {
        if dir.join(".orgasmic/project.org").is_file() {
            return Ok(dir);
        }
        if !dir.pop() {
            bail!("could not find .orgasmic/project.org in cwd or ancestors");
        }
    }
}

/// Project root for verbs that read or write `.orgasmic/` state as FILES —
/// as opposed to the daemon-routed verbs, which carry only a project id and
/// let the daemon bind it to the live root.
///
/// A dispatch worktree carries a FROZEN `.orgasmic/` snapshot from the commit
/// it was created at, so [`find_project_root`]'s marker walk stops inside the
/// worktree and hands back a project that is plausibly shaped and arbitrarily
/// stale. Measured 2026-07-28 (TASK-GQPGR): `dispatch-status` printed EMPTY
/// with three dispatches open and their workers healthy, and
/// `dispatch-close --started-tx` denied a tx that exists — both read as fact.
///
/// Policy is REFUSE, not silently re-resolve to the primary root. Three
/// reasons, recorded here because the task asked for the choice to be argued:
/// (1) `dispatch-close` performs destructive cleanup (`--worktree-remove`)
/// and could be pointed at the very worktree it is running in, which
/// auto-resolution would make silently reachable; (2) the failure this guards
/// is one of misplaced confidence, and a stderr note on an otherwise
/// successful command is exactly the signal that already goes unread in agent
/// transcripts; (3) neither verb accepts `--project`, so `cd` to the primary
/// root is the single unambiguous remedy and the error can just name it.
// orgasmic:task_GQPGR
fn find_live_project_root(home: &Home, verb: &str) -> Result<PathBuf> {
    let root = find_project_root()?;
    if let Some(primary) = frozen_snapshot_primary_root(home, &root) {
        let dispatch = dispatch_worktree_task_hint(&primary, &root)
            .map(|tasks| format!("the dispatch worktree for {tasks}"))
            .unwrap_or_else(|| "a linked git worktree of this project".to_string());
        bail!(
            "{verb}: refusing to read project state from {} — it is {dispatch}, and its \
             .orgasmic/ is a frozen snapshot of the commit the worktree was created at, so \
             any answer here is stale rather than live. Run this from the primary project \
             root instead: {}",
            root.display(),
            primary.display()
        );
    }
    Ok(root)
}

/// `Some(primary_root)` when `root` is a linked git worktree whose project id
/// resolves, on the live board, to a DIFFERENT directory — i.e. `root` holds a
/// point-in-time copy of `.orgasmic/` and not the live one.
///
/// Every uncertain case (primary checkout, unreadable `project.org`, project
/// not registered on the board) returns `None`, so the guard can only fire on
/// the shape it positively identifies. A stale project id that no longer
/// matches the board falls through here and stays loud downstream: the
/// daemon-routed verbs reject the unknown project outright.
fn frozen_snapshot_primary_root(home: &Home, root: &Path) -> Option<PathBuf> {
    // A linked worktree's `.git` is a FILE (`gitdir: ...`); a primary checkout
    // has a directory. Anything else is not a worktree and needs no guard.
    if !root.join(".git").is_file() {
        return None;
    }
    let project_id = read_project_id(root).ok()?;
    let primary = registered_project_root(home, &project_id).ok()?;
    (normalize_path(root) != normalize_path(&primary)).then_some(primary)
}

/// Name the dispatch a worktree belongs to, read from the LIVE project's tx
/// log, so the refusal can say "the dispatch worktree for TASK-X". Best
/// effort: an unreadable or unmatched live ledger just drops the detail.
fn dispatch_worktree_task_hint(primary: &Path, worktree: &Path) -> Option<String> {
    let target = normalize_path(worktree);
    scan_open_dispatches(primary)
        .ok()?
        .into_iter()
        .find(|record| {
            record
                .worktree
                .as_deref()
                .is_some_and(|path| normalize_path(path) == target)
        })
        .map(|record| task_list_property(&record.tasks))
}

fn registered_project_root(home: &Home, project_id: &str) -> Result<PathBuf> {
    let board = projects::read_board(home).context("read project board")?;
    let entry = board
        .iter()
        .find(|entry| entry.id == project_id)
        .ok_or_else(|| anyhow::anyhow!("project {project_id} is not registered on the board"))?;
    let root = std::fs::canonicalize(&entry.path).with_context(|| {
        format!(
            "canonicalize registered project root {}",
            entry.path.display()
        )
    })?;
    if !root.join(".orgasmic/project.org").is_file() {
        bail!(
            "registered project root for {project_id} is missing .orgasmic/project.org: {}",
            root.display()
        );
    }
    Ok(root)
}

pub(crate) fn read_project_id(project_root: &Path) -> Result<String> {
    let path = project_root.join(".orgasmic/project.org");
    let source =
        std::fs::read_to_string(&path).with_context(|| format!("read {}", path.display()))?;
    let file = OrgFile::parse(source, path.to_string_lossy())?;
    let project = ProjectFile::from_org(&file, path.to_string_lossy().as_ref())?;
    Ok(project.id.to_string())
}

fn normalize_tasks(tasks: Vec<String>) -> Result<Vec<String>> {
    let mut seen = BTreeSet::new();
    let mut normalized = Vec::new();
    for task in tasks {
        let task = task.trim().to_string();
        if task.is_empty() {
            bail!("--task must not be empty");
        }
        if !seen.insert(task.clone()) {
            bail!("duplicate --task {task}");
        }
        normalized.push(task);
    }
    if normalized.is_empty() {
        bail!("at least one --task is required");
    }
    Ok(normalized)
}

fn first_task(tasks: &[String]) -> &str {
    tasks.first().map(String::as_str).unwrap_or("")
}

fn task_list_property(tasks: &[String]) -> String {
    tasks.join(" ")
}

fn split_task_list(task_value: &str) -> Vec<String> {
    task_value
        .split_whitespace()
        .map(str::trim)
        .filter(|task| !task.is_empty())
        .map(str::to_string)
        .collect()
}

fn request_slug(value: &str) -> String {
    value
        .chars()
        .map(|ch| {
            if ch.is_ascii_alphanumeric() || ch == '-' || ch == '_' || ch == '.' {
                ch.to_ascii_lowercase()
            } else {
                '-'
            }
        })
        .collect::<String>()
        .trim_matches('-')
        .to_string()
}

fn validate_task_dispatchable(
    project_root: &Path,
    task_id: &str,
    kind: DispatchKind,
) -> Result<()> {
    let task = read_task_lifecycle(project_root, task_id)?;
    if dispatchable_stage(kind, task.stage) {
        return Ok(());
    }
    bail!(
        "task {} is in lifecycle stage {}; {} dispatch is allowed only from {}",
        task_id,
        task.stage,
        kind,
        allowed_stage_text(kind)
    );
}

fn read_task_lifecycle(project_root: &Path, task_id: &str) -> Result<TaskLifecycleInfo> {
    for path in iter_task_file_paths(project_root) {
        if !path.exists() {
            continue;
        }
        let source =
            std::fs::read_to_string(&path).with_context(|| format!("read {}", path.display()))?;
        let file = OrgFile::parse(source.clone(), path.to_string_lossy())?;
        for heading in &file.headings {
            if heading.property("ID") != Some(task_id) {
                continue;
            }
            let fix_subtask = heading
                .property("FIX_SUBTASK")
                .map(trueish_property_value)
                .unwrap_or(false);
            let task = TaskHeading::from_heading(&file, heading, path.to_string_lossy().as_ref())?;
            return Ok(TaskLifecycleInfo {
                id: task.id.to_string(),
                stage: task.lifecycle_stage,
                fix_subtask,
            });
        }
    }
    bail!(
        "task {task_id} not found in any task file under {}",
        dotorg_tasks_dir(project_root).display()
    );
}

fn trueish_property_value(value: &str) -> bool {
    let value = value.trim();
    value == "1"
        || value.eq_ignore_ascii_case("t")
        || value.eq_ignore_ascii_case("true")
        || value.eq_ignore_ascii_case("y")
        || value.eq_ignore_ascii_case("yes")
}

fn capture_task_lifecycle_stages(
    project_root: &Path,
    tasks: &[String],
) -> Result<Vec<(String, LifecycleStage)>> {
    tasks
        .iter()
        .map(|task| {
            let info = read_task_lifecycle(project_root, task)?;
            Ok((info.id, info.stage))
        })
        .collect()
}

fn restore_task_lifecycle_stages(
    client: &DaemonClient,
    project_id: &str,
    stages: &[(String, LifecycleStage)],
) {
    for (task_id, stage) in stages {
        let transition = [(task_id.clone(), *stage)];
        if let Err(err) = apply_task_lifecycle_transitions(client, project_id, &transition) {
            eprintln!("warning: failed to restore lifecycle stage for {task_id}: {err}");
        }
    }
}

fn dispatch_lifecycle_transitions(
    kind: DispatchKind,
    tasks: &[String],
) -> Vec<(String, LifecycleStage)> {
    let stage = match kind {
        DispatchKind::Implementer => LifecycleStage::InProgress,
        DispatchKind::Reviewer => LifecycleStage::InReview,
        DispatchKind::Architector => LifecycleStage::InProgress,
    };
    tasks.iter().map(|task| (task.clone(), stage)).collect()
}

/// The lifecycle move a `dispatch-close` intends for one task, recorded on the
/// close tx itself (`LIFECYCLE_FROM`/`LIFECYCLE_TO`) before the transition is
/// attempted.
///
/// orgasmic:task_EP3H1 — the close is two daemon writes (close tx, then task
/// transition) and cannot be one commit without either a multi-tx writer
/// transaction or collapsing `task.state_transitioned` into the close tx. So
/// the tx carries its own intent instead: a close whose second leg is lost
/// leaves a ledger that still says where the task was going, and
/// [`reconcile_torn_closes`] finishes it on the next manager command.
#[derive(Clone, Debug, Eq, PartialEq)]
struct CloseTransition {
    task: String,
    from: LifecycleStage,
    to: LifecycleStage,
}

const LIFECYCLE_FROM_KEY: &str = "LIFECYCLE_FROM";
const LIFECYCLE_TO_KEY: &str = "LIFECYCLE_TO";

fn close_lifecycle_transitions(
    project_root: &Path,
    tasks: &[String],
    open: &DispatchRecord,
    args: &DispatchCloseArgs,
) -> Result<Vec<CloseTransition>> {
    let mut transitions = Vec::new();
    for task in tasks {
        let info = read_task_lifecycle(project_root, task)?;
        let stage = match args.status {
            DispatchCloseStatus::Aborted => match open.kind.as_str() {
                "implementer" => LifecycleStage::Todo,
                "reviewer" => LifecycleStage::InReview,
                "architector" => LifecycleStage::Todo,
                other => bail!("cannot close dispatch kind `{other}` as aborted"),
            },
            DispatchCloseStatus::Done => match open.kind.as_str() {
                "implementer" => {
                    if info.fix_subtask {
                        LifecycleStage::Done
                    } else {
                        LifecycleStage::InReview
                    }
                }
                "reviewer" => reviewer_done_stage(args),
                "architector" => LifecycleStage::Done,
                other => bail!("cannot close dispatch kind `{other}` as done"),
            },
        };
        transitions.push(CloseTransition {
            task: info.id,
            from: info.stage,
            to: stage,
        });
    }
    Ok(transitions)
}

fn reviewer_done_stage(args: &DispatchCloseArgs) -> LifecycleStage {
    let verdict_clean = close_property_value(args, "VERDICT")
        .map(|value| value == "clean" || value == "ship")
        .unwrap_or(false);
    let recommended_empty = close_property_value(args, "RECOMMENDED_SUBTASKS")
        .map(recommended_subtasks_empty)
        .unwrap_or(true);
    if verdict_clean && recommended_empty {
        LifecycleStage::Done
    } else {
        LifecycleStage::InProgress
    }
}

fn close_property_value<'a>(args: &'a DispatchCloseArgs, key: &str) -> Option<&'a str> {
    args.properties
        .iter()
        .rev()
        .find(|(got, _)| got == key)
        .map(|(_, value)| value.as_str())
}

fn recommended_subtasks_empty(value: &str) -> bool {
    let value = value.trim();
    value.is_empty() || value == "-"
}

fn apply_task_lifecycle_transitions(
    client: &DaemonClient,
    project_id: &str,
    transitions: &[(String, LifecycleStage)],
) -> Result<()> {
    if transitions.is_empty() {
        return Ok(());
    }
    let runtime = tokio::runtime::Runtime::new().context("create tokio runtime")?;
    runtime.block_on(async {
        for (task_id, stage) in transitions {
            let _: serde_json::Value = client
                .post_json(
                    &format!("/projects/{project_id}/tasks/{task_id}"),
                    &serde_json::json!({ "state": stage.as_str() }),
                )
                .await?;
        }
        Ok(())
    })
}

/// The request id a close's lifecycle leg carries, deterministic per (task,
/// dispatch generation) exactly as the close tx's own id is.
///
/// orgasmic:task_EP3H1 — a client timeout is not a server failure: the leg
/// that "failed" may have landed. Because the retry (by hand, or by
/// [`reconcile_torn_closes`]) re-sends the SAME request id, the daemon can
/// answer `status=already_applied` with the tx it wrote instead of an
/// unlabelled empty change set.
fn close_lifecycle_request_id(task: &str, started_tx: &str) -> String {
    format!("dispatch-close-state-{}-{}", request_slug(task), started_tx)
}

/// Apply a close's lifecycle transitions, reporting per-task whether the
/// daemon applied them now or recognised them as already applied.
fn apply_close_lifecycle_transitions(
    client: &DaemonClient,
    runtime: &tokio::runtime::Runtime,
    project_id: &str,
    started_tx: &str,
    transitions: &[CloseTransition],
) -> Result<Vec<TaskStateOutcome>> {
    let mut outcomes = Vec::new();
    for transition in transitions {
        outcomes.push(runtime.block_on(post_task_state(
            client,
            project_id,
            transition,
            &close_lifecycle_request_id(&transition.task, started_tx),
        ))?);
    }
    Ok(outcomes)
}

/// The daemon's labelled no-op contract for `POST /projects/:id/tasks/:task`
/// with a `state`, mirrored on the client (see `update_task_state` in the
/// daemon's `api.rs`, where the contract is documented).
#[derive(Debug, Deserialize)]
struct TaskStateOutcome {
    #[serde(default)]
    status: Option<String>,
    #[serde(default)]
    tx_id: String,
}

impl TaskStateOutcome {
    fn already_applied(&self) -> bool {
        self.status.as_deref() == Some("already_applied")
    }
}

async fn post_task_state(
    client: &DaemonClient,
    project_id: &str,
    transition: &CloseTransition,
    request_id: &str,
) -> Result<TaskStateOutcome> {
    client
        .post_json(
            &format!(
                "/projects/{project_id}/tasks/{}",
                path_segment(&transition.task)
            ),
            &serde_json::json!({
                "state": transition.to.as_str(),
                "request_id": request_id,
            }),
        )
        .await
}

/// Finish any `dispatch-close` whose lifecycle leg never landed.
///
/// orgasmic:task_EP3H1 — a close appends its tx and then transitions the task
/// in a second daemon request. Under load the second one times out
/// client-side (measured at load average ~190 on 2026-07-29) and the operator
/// is left with a closed dispatch and a task stranded at its pre-close stage.
/// The close tx records the transition it intended, so the repair is decidable
/// from the ledger alone: a close is torn when it is the last lifecycle event
/// for its task AND the task is still sitting at the recorded `LIFECYCLE_FROM`.
/// Any later `task.state_transitioned` — including one an operator made on
/// purpose — clears the candidate, so this never drags a deliberately moved
/// task back.
fn reconcile_torn_closes(
    client: &DaemonClient,
    runtime: &tokio::runtime::Runtime,
    project_root: &Path,
    project_id: &str,
) -> Result<()> {
    for (started_tx, transition) in torn_close_candidates(project_root)? {
        let current = match read_task_lifecycle(project_root, &transition.task) {
            Ok(info) => info.stage,
            // A task that is no longer in any task file (archived, renamed)
            // is not a tear this command can or should repair.
            Err(_) => continue,
        };
        if current != transition.from || transition.from == transition.to {
            continue;
        }
        let outcome = runtime.block_on(post_task_state(
            client,
            project_id,
            &transition,
            &close_lifecycle_request_id(&transition.task, &started_tx),
        ));
        match outcome {
            Ok(outcome) => println!(
                "reconciled: {} {} -> {} (torn close {}{})",
                transition.task,
                transition.from.as_str(),
                transition.to.as_str(),
                started_tx,
                if outcome.already_applied() {
                    "; the timed-out request had already applied it".to_string()
                } else if outcome.tx_id.is_empty() {
                    String::new()
                } else {
                    format!(" tx={}", outcome.tx_id)
                }
            ),
            Err(err) => eprintln!(
                "warning: could not finish torn close {started_tx} for {}: {err}",
                transition.task
            ),
        }
    }
    Ok(())
}

/// Best-effort reconciliation for a manager command that has not built a
/// daemon client of its own. A daemon that cannot be reached is not a reason
/// to fail the command the operator actually asked for.
fn reconcile_torn_closes_best_effort(home: &Home, project_root: &Path, project_id: &str) {
    let Ok(client) = DaemonClient::from_home_autostart(home) else {
        return;
    };
    let Ok(runtime) = tokio::runtime::Runtime::new() else {
        return;
    };
    if let Err(err) = reconcile_torn_closes(&client, &runtime, project_root, project_id) {
        eprintln!("warning: torn-close reconciliation skipped: {err}");
    }
}

/// Per task, the close transition still owed by the ledger: the newest close
/// tx carrying `LIFECYCLE_FROM`/`LIFECYCLE_TO`, dropped again as soon as a
/// later `task.state_transitioned` for that task appears.
fn torn_close_candidates(project_root: &Path) -> Result<Vec<(String, CloseTransition)>> {
    let mut pending: Vec<(String, CloseTransition)> = Vec::new();
    for entry in read_tx_entries(project_root)? {
        let Some(task) = entry.task.as_deref() else {
            continue;
        };
        // A close tx names exactly one task (`dispatch-close` appends one per
        // task), so a task list here is not a close and carries no intent.
        match entry.ty.as_str() {
            "implementer.done"
            | "reviewer.done"
            | "architector.done"
            | "manager.dispatch_aborted" => {
                pending.retain(|(_, pending)| pending.task != task);
                let from = extra(&entry, LIFECYCLE_FROM_KEY).and_then(|v| v.parse().ok());
                let to = extra(&entry, LIFECYCLE_TO_KEY).and_then(|v| v.parse().ok());
                if let (Some(from), Some(to), Some(started_tx)) =
                    (from, to, extra(&entry, "CLOSED_TX"))
                {
                    pending.push((
                        started_tx.to_string(),
                        CloseTransition {
                            task: task.to_string(),
                            from,
                            to,
                        },
                    ));
                }
            }
            "task.state_transitioned" => {
                pending.retain(|(_, pending)| pending.task != task);
            }
            _ => {}
        }
    }
    Ok(pending)
}

fn dispatchable_stage(kind: DispatchKind, stage: LifecycleStage) -> bool {
    match kind {
        DispatchKind::Implementer => {
            matches!(stage, LifecycleStage::Backlog | LifecycleStage::Todo)
        }
        DispatchKind::Architector => {
            matches!(stage, LifecycleStage::Backlog | LifecycleStage::Todo)
        }
        DispatchKind::Reviewer => {
            matches!(stage, LifecycleStage::InReview)
        }
    }
}

fn allowed_stage_text(kind: DispatchKind) -> &'static str {
    match kind {
        DispatchKind::Implementer => "BACKLOG or TODO",
        DispatchKind::Reviewer => "IN_REVIEW",
        DispatchKind::Architector => "BACKLOG or TODO",
    }
}

fn resolve_commit(project_root: &Path, commitish: &str) -> Result<String> {
    let rev = format!("{commitish}^{{commit}}");
    let output = Command::new("git")
        .args(["rev-parse", "--verify", &rev])
        .current_dir(project_root)
        .output()
        .with_context(|| format!("git rev-parse {commitish}"))?;
    if !output.status.success() {
        bail!(
            "cannot resolve commit `{}`: {}{}",
            commitish,
            String::from_utf8_lossy(&output.stderr),
            String::from_utf8_lossy(&output.stdout)
        );
    }
    Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
}

fn read_active_goal_id(project_root: &Path) -> Result<Option<String>> {
    let path = goal_file_path(project_root);
    if !path.exists() {
        return Ok(None);
    }
    let source =
        std::fs::read_to_string(&path).with_context(|| format!("read {}", path.display()))?;
    let file = OrgFile::parse(source, path.to_string_lossy())?;
    for heading in &file.headings {
        if heading.property("STATUS") == Some("active") {
            if let Some(id) = heading.property("ID") {
                return Ok(Some(id.to_string()));
            }
        }
    }
    Ok(None)
}

fn canonical_existing_file(path: &Path) -> Result<PathBuf> {
    if !path.is_file() {
        bail!("brief must exist and be a file: {}", path.display());
    }
    std::fs::canonicalize(path).with_context(|| format!("canonicalize {}", path.display()))
}

fn absolutize(path: &Path) -> Result<PathBuf> {
    if path.is_absolute() {
        Ok(path.to_path_buf())
    } else {
        Ok(std::env::current_dir().context("cwd")?.join(path))
    }
}

/// Normalize a path for stable comparison and storage (handles macOS
/// `/var` vs `/private/var` and non-existent leaf components).
fn normalize_path(path: &Path) -> PathBuf {
    if let Ok(canon) = std::fs::canonicalize(path) {
        return canon;
    }

    let mut current = path;
    let mut missing = Vec::new();
    while let Some(parent) = current.parent() {
        if let Some(name) = current.file_name() {
            missing.push(name.to_os_string());
        }
        if let Ok(mut canon) = std::fs::canonicalize(parent) {
            for component in missing.iter().rev() {
                canon.push(component);
            }
            return canon;
        }
        current = parent;
    }

    path.to_path_buf()
}

fn task_slug(task: &str) -> String {
    format!(
        "task-{}",
        task.to_ascii_lowercase().trim_start_matches("task-")
    )
}

fn default_worktree(project_root: &Path, task: &str, kind: DispatchKind) -> PathBuf {
    let slug = task_slug(task);
    let stem = match kind {
        DispatchKind::Implementer => slug,
        DispatchKind::Reviewer => format!("{slug}-review"),
        DispatchKind::Architector => format!("{slug}-arch"),
    };
    project_dispatch_dir(project_root)
        .join(stem)
        .join("worktree")
}

fn default_branch(task: &str, kind: DispatchKind) -> String {
    let slug = task_slug(task);
    match kind {
        DispatchKind::Implementer => format!("{slug}-impl"),
        DispatchKind::Reviewer => format!("{slug}-review"),
        DispatchKind::Architector => format!("{slug}-arch"),
    }
}

fn dispatch_artifact_stem(brief_path: &Path) -> (String, String) {
    let file_name = brief_path
        .file_name()
        .and_then(|s| s.to_str())
        .unwrap_or("dispatch-brief.md")
        .to_string();
    let stem = if let Some(prefix) = file_name.strip_suffix("-brief.md") {
        prefix.to_string()
    } else {
        brief_path
            .file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or("dispatch")
            .to_string()
    };
    (file_name, stem)
}

fn mint_dispatch_attempt_id() -> String {
    uuid::Uuid::new_v4().simple().to_string()
}

const DISPATCH_ARTIFACT_RESERVE_RETRIES: usize = 8;

/// RAII reservation for dispatch last/stdout artifact pair. Rolls back only
/// files this attempt created on drop unless committed (TASK-KE0JW).
struct DispatchArtifactReservation {
    owned: Vec<PathBuf>,
    brief_path: PathBuf,
    last_path: PathBuf,
    stdout_path: PathBuf,
    attempt_token: String,
    committed: bool,
}

impl DispatchArtifactReservation {
    fn reserve(project_root: &Path, brief_path: &Path) -> Result<Self> {
        for _ in 0..DISPATCH_ARTIFACT_RESERVE_RETRIES {
            let attempt_id = mint_dispatch_attempt_id();
            let (brief, last, stdout) =
                dispatch_artifact_paths_for_attempt(project_root, brief_path, &attempt_id);
            if let Some(parent) = brief.parent() {
                std::fs::create_dir_all(parent).with_context(|| {
                    format!("create dispatch artifact dir {}", parent.display())
                })?;
            }
            match reserve_dispatch_artifact_pair(&last, &stdout) {
                Ok(owned) => {
                    return Ok(Self {
                        owned,
                        brief_path: brief,
                        last_path: last,
                        stdout_path: stdout,
                        attempt_token: attempt_id,
                        committed: false,
                    });
                }
                Err(ReservePairError::Collision) => continue,
                Err(ReservePairError::Io(err)) => return Err(err.into()),
            }
        }
        bail!("failed to reserve dispatch artifact pair after {DISPATCH_ARTIFACT_RESERVE_RETRIES} attempts");
    }

    fn brief_path(&self) -> PathBuf {
        self.brief_path.clone()
    }

    fn last_path(&self) -> PathBuf {
        self.last_path.clone()
    }

    fn stdout_path(&self) -> PathBuf {
        self.stdout_path.clone()
    }

    fn attempt_token(&self) -> String {
        self.attempt_token.clone()
    }

    fn commit(&mut self) {
        self.committed = true;
        self.owned.clear();
    }
}

impl Drop for DispatchArtifactReservation {
    fn drop(&mut self) {
        if self.committed {
            return;
        }
        if let Err(err) = self.rollback_owned() {
            tracing::warn!(
                "dispatch artifact reservation rollback failed: {err}; paths may remain"
            );
        }
    }
}

impl DispatchArtifactReservation {
    fn rollback_owned(&mut self) -> Result<()> {
        let mut last_error = None;
        for path in self.owned.drain(..) {
            if let Err(err) = std::fs::remove_file(&path) {
                if err.kind() != std::io::ErrorKind::NotFound {
                    last_error = Some(err);
                }
            }
        }
        if let Some(err) = last_error {
            Err(err.into())
        } else {
            Ok(())
        }
    }
}

enum ReservePairError {
    Collision,
    Io(std::io::Error),
}

fn reserve_dispatch_artifact_pair(
    last: &Path,
    stdout: &Path,
) -> Result<Vec<PathBuf>, ReservePairError> {
    let mut owned = Vec::new();
    match std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(last)
    {
        Ok(_) => owned.push(last.to_path_buf()),
        Err(err) if err.kind() == std::io::ErrorKind::AlreadyExists => {
            return Err(ReservePairError::Collision);
        }
        Err(err) => return Err(ReservePairError::Io(err)),
    }
    match std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(stdout)
    {
        Ok(_) => {
            owned.push(stdout.to_path_buf());
            Ok(owned)
        }
        Err(err) if err.kind() == std::io::ErrorKind::AlreadyExists => {
            for path in owned {
                std::fs::remove_file(path).map_err(ReservePairError::Io)?;
            }
            Err(ReservePairError::Collision)
        }
        Err(err) => {
            for path in owned {
                if let Err(cleanup_err) = std::fs::remove_file(path) {
                    if cleanup_err.kind() != std::io::ErrorKind::NotFound {
                        return Err(ReservePairError::Io(cleanup_err));
                    }
                }
            }
            Err(ReservePairError::Io(err))
        }
    }
}

/// Atomically reserve the (brief, last, stdout) artifact paths for a dispatch.
/// Prefer [`DispatchArtifactReservation`] in production dispatch paths.
#[cfg(test)]
fn reserve_dispatch_artifact_paths(
    project_root: &Path,
    brief_path: &Path,
) -> Result<(PathBuf, PathBuf, PathBuf, String)> {
    let mut reservation = DispatchArtifactReservation::reserve(project_root, brief_path)?;
    let paths = (
        reservation.brief_path(),
        reservation.last_path(),
        reservation.stdout_path(),
        reservation.attempt_token(),
    );
    reservation.commit();
    Ok(paths)
}

fn dispatch_artifact_paths_for_attempt(
    project_root: &Path,
    brief_path: &Path,
    attempt_id: &str,
) -> (PathBuf, PathBuf, PathBuf) {
    let (file_name, stem) = dispatch_artifact_stem(brief_path);
    let dir = project_dispatch_dir(project_root).join(&stem);
    (
        dir.join(file_name),
        dir.join(format!("{stem}-{attempt_id}-last.txt")),
        dir.join(format!("{stem}-{attempt_id}-stdout.log")),
    )
}

/// Derive the last/stdout paths as siblings of an already-resolved brief when
/// the attempt id is known (e.g. from a live run's recorded `last_path`).
fn dispatch_sibling_artifact_paths(brief_path: &Path) -> (PathBuf, PathBuf) {
    let parent = brief_path.parent().unwrap_or_else(|| Path::new("."));
    let (_, stem) = dispatch_artifact_stem(brief_path);
    (
        parent.join(format!("{stem}-last.txt")),
        parent.join(format!("{stem}-stdout.log")),
    )
}

fn dispatch_sibling_artifact_paths_from_last(last_path: &Path) -> (PathBuf, PathBuf) {
    let parent = last_path.parent().unwrap_or_else(|| Path::new("."));
    let file = last_path
        .file_name()
        .and_then(|s| s.to_str())
        .unwrap_or("last.txt");
    let stdout = if file.ends_with("-last.txt") {
        file.replacen("-last.txt", "-stdout.log", 1)
    } else {
        "stdout.log".to_string()
    };
    (last_path.to_path_buf(), parent.join(stdout))
}

fn materialize_dispatch_brief(plan: &DispatchPlan) -> Result<()> {
    if let Some(parent) = plan.brief_path.parent() {
        std::fs::create_dir_all(parent).with_context(|| format!("create {}", parent.display()))?;
    }
    std::fs::write(&plan.brief_path, &plan.brief_content)
        .with_context(|| format!("write {}", plan.brief_path.display()))
}

/// Every still-open dispatch covering every requested task, oldest first.
fn open_dispatches_for_tasks(project_root: &Path, tasks: &[String]) -> Result<Vec<DispatchRecord>> {
    Ok(scan_open_dispatches(project_root)?
        .into_iter()
        .filter(|record| {
            tasks
                .iter()
                .all(|task| record.tasks.iter().any(|got| got == task))
        })
        .collect())
}

/// Which dispatch generation a `dispatch-close` invocation acts on.
#[derive(Debug)]
enum CloseTarget {
    /// A still-open dispatch: close it for real.
    Open(DispatchRecord),
    /// The generation this close names is already closed: no-op.
    AlreadyClosed(DispatchRecord),
}

/// Resolve the dispatch generation a close acts on (TASK-6AYEJ.1).
///
/// Close identity is a GENERATION — one `manager.dispatch_started` tx — not a
/// task. A task outlives its dispatches: closing the implementer moves the task
/// to IN_REVIEW and a reviewer is opened for the SAME task, so a task-bound
/// retry of the implementer close selects the reviewer's open record and
/// releases and cleans up a live dispatch. With `--started-tx` the named
/// generation decides and nothing else is consulted: a replay against an
/// already-closed generation is a no-op *even while another dispatch for the
/// task is open*, and a generation that is not in the ledger at all is a hard
/// error rather than a fall-through to "whatever is open for this task".
///
/// Without `--started-tx` a close may only act on an ALREADY-CLOSED record
/// (TASK-6AYEJ.2). That keeps the ~10 historical worker-closed dispatches —
/// which have no generation token and never will — a clean no-op, while making
/// it impossible for a tokenless close to release a LIVE dispatch it never
/// named. If an open matching dispatch exists the close is refused, and the
/// refusal prints the candidate tokens so the operator can copy one rather than
/// have the tool guess for them.
fn resolve_close_target(
    project_root: &Path,
    tasks: &[String],
    started_tx: Option<&str>,
) -> Result<CloseTarget> {
    if let Some(started_tx) = started_tx {
        let record = scan_dispatches(project_root)?
            .into_iter()
            .rev()
            .find(|record| record.tx_id == started_tx)
            .ok_or_else(|| {
                anyhow::anyhow!(
                    "no manager.dispatch_started tx {started_tx} in the tx log \
                     (--started-tx names the dispatch generation to close; \
                     `orgasmic manager dispatch-status` prints it as TX_ID=)"
                )
            })?;
        for task in tasks {
            if !record.tasks.iter().any(|got| got == task) {
                bail!(
                    "dispatch {} covers {} and does not include requested task {}",
                    record.tx_id,
                    task_list_property(&record.tasks),
                    task
                );
            }
        }
        return Ok(if record.closed {
            CloseTarget::AlreadyClosed(record)
        } else {
            CloseTarget::Open(record)
        });
    }
    // A tokenless close must never act on a live dispatch (TASK-6AYEJ.2):
    // task-bound selection picks "the newest open dispatch covering this task",
    // which is the successor, not the generation the caller meant. Name the
    // candidates and let the operator choose.
    let open = open_dispatches_for_tasks(project_root, tasks)?;
    if !open.is_empty() {
        let candidates = open
            .iter()
            .map(|record| format!("  --started-tx {} ({})", record.tx_id, record.kind))
            .collect::<Vec<_>>()
            .join("\n");
        bail!(
            "--started-tx is required: {} has {} open dispatch generation(s), and a \
             close bound to a task rather than a generation can release a SUCCESSOR \
             dispatch (TASK-6AYEJ.1). Re-run with one of:\n{candidates}",
            task_list_property(tasks),
            open.len()
        );
    }
    // Closing an already-closed dispatch is a no-op, not an error
    // (TASK-6AYEJ): a manager that died mid-integration and re-runs the close
    // must not be punished, and neither must a dispatch closed before this fix
    // by the worker's own finalize. Only a task with no dispatch record at all
    // is still an error.
    let closed = latest_closed_dispatch_for_tasks(project_root, tasks)?.ok_or_else(|| {
        anyhow::anyhow!(
            "no open manager.dispatch_started tx for {}",
            task_list_property(tasks)
        )
    })?;
    Ok(CloseTarget::AlreadyClosed(closed))
}

/// The newest already-closed dispatch covering every requested task, used only
/// to make a repeated `dispatch-close` a clean no-op (TASK-6AYEJ).
fn latest_closed_dispatch_for_tasks(
    project_root: &Path,
    tasks: &[String],
) -> Result<Option<DispatchRecord>> {
    Ok(scan_dispatches(project_root)?
        .into_iter()
        .rev()
        .find(|record| {
            record.closed
                && tasks
                    .iter()
                    .all(|task| record.tasks.iter().any(|got| got == task))
        }))
}

fn latest_open_dispatch_overlapping_tasks(
    project_root: &Path,
    tasks: &[String],
) -> Result<Option<DispatchRecord>> {
    let open = scan_open_dispatches(project_root)?;
    Ok(open.into_iter().rev().find(|record| {
        record
            .tasks
            .iter()
            .any(|task| tasks.iter().any(|requested| requested == task))
    }))
}

fn overlapping_tasks(open_tasks: &[String], requested_tasks: &[String]) -> Vec<String> {
    requested_tasks
        .iter()
        .filter(|task| open_tasks.iter().any(|open_task| open_task == *task))
        .cloned()
        .collect()
}

/// Every `manager.dispatch_started` in the tx log, each carrying whether it has
/// since been closed. [`scan_open_dispatches`] is this filtered to the still-open
/// ones; `dispatch-close` also needs the closed ones so a re-run is a no-op
/// instead of "no open dispatch" (TASK-6AYEJ).
fn scan_dispatches(project_root: &Path) -> Result<Vec<DispatchRecord>> {
    let mut open = Vec::<DispatchRecord>::new();
    for entry in read_tx_entries(project_root)? {
        match entry.ty.as_str() {
            "manager.dispatch_started" => {
                if let Some(mut record) = dispatch_record_from_entry(&entry) {
                    // Tx records store project-relative paths (no user-specific
                    // prefixes in committed files); resolve them back against
                    // the project root for local use (ps matching, cleanup).
                    for path in [&mut record.worktree, &mut record.brief_path] {
                        if let Some(p) = path.as_mut() {
                            if p.is_relative() {
                                *p = project_root.join(&p);
                            }
                        }
                    }
                    open.push(record);
                }
            }
            "run.created" => {
                attach_run_created_to_dispatch(&mut open, &entry);
            }
            // A worker's own finalize (TASK-6AYEJ): the worker is done, the
            // dispatch is not. Record it so `dispatch-status` can tell
            // "awaiting the manager's close" apart from "the worker died",
            // but leave the dispatch open for `dispatch-close`.
            "implementer.reported" | "reviewer.reported" | "architector.reported" => {
                mark_matching_dispatch_reported(&mut open, &entry)
            }
            // Historical note: until TASK-6AYEJ these same `*.done` types were
            // ALSO emitted by `dispatch finalize`, so ~10 dispatches on this
            // repo are closed by a worker-authored tx. They stay closed — the
            // terminal set is unchanged, so no backfill or migration is needed.
            "implementer.done"
            | "reviewer.done"
            | "architector.done"
            | "manager.dispatch_aborted" => close_matching_dispatch(&mut open, &entry),
            _ => {}
        }
    }
    Ok(open)
}

fn scan_open_dispatches(project_root: &Path) -> Result<Vec<DispatchRecord>> {
    Ok(scan_dispatches(project_root)?
        .into_iter()
        .filter(|record| !record.closed)
        .collect())
}

fn read_tx_entries(project_root: &Path) -> Result<Vec<TxEntry>> {
    let tx_dir = project_root.join(".orgasmic/tx");
    if !tx_dir.is_dir() {
        return Ok(Vec::new());
    }
    let mut paths = Vec::new();
    for entry in std::fs::read_dir(&tx_dir).with_context(|| format!("read {}", tx_dir.display()))? {
        let entry = entry.with_context(|| format!("read entry in {}", tx_dir.display()))?;
        let path = entry.path();
        if path.extension().and_then(|s| s.to_str()) == Some("org") {
            paths.push(path);
        }
    }
    paths.sort();
    let mut entries = Vec::new();
    for path in paths {
        let source =
            std::fs::read_to_string(&path).with_context(|| format!("read {}", path.display()))?;
        let mut parsed = parse_tx_file(&source, path.to_string_lossy().as_ref())
            .with_context(|| format!("parse {}", path.display()))?;
        entries.append(&mut parsed);
    }
    Ok(entries)
}

fn dispatch_record_from_entry(entry: &TxEntry) -> Option<DispatchRecord> {
    let task = entry.task.clone()?;
    let tasks = split_task_list(&task);
    if tasks.is_empty() {
        return None;
    }
    let kind = extra(entry, "KIND")?.to_string();
    Some(DispatchRecord {
        tx_id: entry.tx_id.clone(),
        tasks,
        kind,
        worktree: extra(entry, "WORKTREE").map(PathBuf::from),
        branch: extra(entry, "BRANCH").map(str::to_string),
        // Read the harness-neutral keys first, falling back to the legacy
        // `CODEX_*` spellings so historical tx records still parse.
        model: extra_compat(entry, "MODEL", "CODEX_MODEL").map(str::to_string),
        effort: extra_compat(entry, "EFFORT", "CODEX_EFFORT").map(str::to_string),
        brief_path: extra_compat(entry, "BRIEF_PATH", "CODEX_BRIEF_PATH").map(PathBuf::from),
        last_path: None,
        stdout_path: None,
        dispatch_attempt_token: None,
        run_id: None,
        run_ids: BTreeSet::new(),
        worker_id: None,
        driver: None,
        harness: None,
        pid: None,
        started_at: extra(entry, "STARTED_AT")
            .map(str::to_string)
            .or_else(|| Some(entry.time.clone())),
        worker_pid: extra_compat(entry, "WORKER_PID", "CODEX_PID")
            .and_then(|pid| pid.parse::<u32>().ok()),
        goal_id: extra(entry, "GOAL_ID").map(str::to_string),
        closed_tasks: BTreeSet::new(),
        cleanup_already_run: false,
        reported: false,
        closed: false,
    })
}

/// A fresh recovery/resume acquires a REPLACEMENT run for the same dispatch
/// generation (TASK-6AYEJ.2). The daemon records the origin→replacement link as
/// `run.created ORIGIN=recovery`; carry it onto the generation so a finalize
/// from the replacement still resolves to this dispatch. Both ids stay valid —
/// they are the same generation, so a report from either is honestly this
/// dispatch's report.
fn attach_recovery_run_to_dispatch(open: &mut [DispatchRecord], entry: &TxEntry) {
    let (Some(origin_run_id), Some(run_id)) =
        (extra(entry, "ORIGIN_RUN_ID"), extra(entry, "RUN_ID"))
    else {
        return;
    };
    for record in open.iter_mut().rev() {
        if !record.closed && record.run_ids.iter().any(|got| got == origin_run_id) {
            record.run_ids.insert(run_id.to_string());
            record.run_id = Some(run_id.to_string());
            return;
        }
    }
}

fn attach_run_created_to_dispatch(open: &mut [DispatchRecord], entry: &TxEntry) {
    if extra(entry, "ORIGIN") == Some("recovery") {
        attach_recovery_run_to_dispatch(open, entry);
        return;
    }
    if extra(entry, "ORIGIN") != Some("cli_dispatch") {
        return;
    }
    let dispatch_tx = extra(entry, "DISPATCH_TX");
    let run_id = extra(entry, "RUN_ID").map(str::to_string);
    let worker_id = extra(entry, "WORKER").map(str::to_string);
    let driver = extra(entry, "DRIVER").map(str::to_string);
    let harness = extra(entry, "HARNESS").map(str::to_string);
    let pid = extra(entry, "PID").and_then(|pid| pid.parse::<u32>().ok());
    let last_path = extra(entry, "LAST_PATH").map(PathBuf::from);
    let stdout_path = extra(entry, "STDOUT_PATH").map(PathBuf::from);
    let dispatch_attempt_token = extra(entry, "DISPATCH_ATTEMPT").map(str::to_string);
    let kind = extra(entry, "KIND");
    let tasks = entry
        .task
        .as_deref()
        .map(split_task_list)
        .unwrap_or_default();
    for record in open.iter_mut().rev() {
        let tx_matches = dispatch_tx.map(|tx| tx == record.tx_id).unwrap_or(false);
        let task_matches = !tasks.is_empty()
            && tasks
                .iter()
                .any(|task| record.tasks.iter().any(|got| got == task));
        let kind_matches = kind.map(|got| got == record.kind).unwrap_or(true);
        if tx_matches || (task_matches && kind_matches && record.run_id.is_none()) {
            if let Some(run_id) = run_id.as_deref() {
                record.run_ids.insert(run_id.to_string());
            }
            record.run_id = run_id;
            record.worker_id = worker_id;
            record.driver = driver;
            record.harness = harness;
            record.pid = pid;
            if last_path.is_some() {
                record.last_path = last_path;
            }
            if stdout_path.is_some() {
                record.stdout_path = stdout_path;
            }
            if dispatch_attempt_token.is_some() {
                record.dispatch_attempt_token = dispatch_attempt_token;
            }
            return;
        }
    }
}

/// Attach a worker's `*.reported` finalize tx to its still-open dispatch
/// (TASK-6AYEJ). Matches on the run id when the report carries one — against
/// every run id the generation has owned, so a finalize from a recovery
/// replacement still lands (TASK-6AYEJ.2) — and falls back to the
/// newest-unclosed-overlapping-task rule [`close_matching_dispatch`] uses ONLY
/// for a report that genuinely lacks a RUN_ID.
///
/// A present-but-unmatched RUN_ID fails closed (TASK-6AYEJ.1): a late report
/// for run A, dispatched and then aborted before run B took the same task,
/// cannot match A (A is closed, and closed records are excluded), and falling
/// through to task overlap would flag B — telling the manager the wrong worker
/// finished. Unattached is the honest answer.
fn mark_matching_dispatch_reported(open: &mut [DispatchRecord], reported: &TxEntry) {
    if let Some(run_id) = extra(reported, "RUN_ID") {
        for record in open.iter_mut().rev() {
            if !record.closed && record.run_ids.iter().any(|got| got == run_id) {
                record.reported = true;
                return;
            }
        }
        return;
    }
    let reported_tasks = reported
        .task
        .as_deref()
        .map(split_task_list)
        .unwrap_or_default();
    for record in open.iter_mut().rev() {
        if !record.closed
            && reported_tasks
                .iter()
                .any(|task| record.tasks.iter().any(|got| got == task))
        {
            record.reported = true;
            return;
        }
    }
}

fn close_matching_dispatch(open: &mut [DispatchRecord], close: &TxEntry) {
    let close_tasks = close
        .task
        .as_deref()
        .map(split_task_list)
        .unwrap_or_default();
    if let Some(closed_tx) = extra(close, "CLOSED_TX") {
        for record in open.iter_mut().rev() {
            if record.tx_id == closed_tx {
                if close_tx_ran_cleanup(close) {
                    record.cleanup_already_run = true;
                }
                mark_dispatch_closed(record, &close_tasks);
                return;
            }
        }
    }
    for record in open.iter_mut().rev() {
        if !record.closed
            && close_tasks
                .iter()
                .any(|task| record.tasks.iter().any(|got| got == task))
        {
            if close_tx_ran_cleanup(close) {
                record.cleanup_already_run = true;
            }
            mark_dispatch_closed(record, &close_tasks);
            return;
        }
    }
}

fn close_tx_ran_cleanup(close: &TxEntry) -> bool {
    matches!(
        extra(close, "CLEANUP_STATUS"),
        Some(status)
            if status == CleanupStatus::Ok.as_str()
                || status == CleanupStatus::WorktreeMissing.as_str()
    )
}

fn mark_dispatch_closed(record: &mut DispatchRecord, close_tasks: &[String]) {
    if close_tasks.is_empty() {
        for task in &record.tasks {
            record.closed_tasks.insert(task.clone());
        }
    } else {
        for task in close_tasks {
            if record.tasks.iter().any(|got| got == task) {
                record.closed_tasks.insert(task.clone());
            }
        }
    }
    record.closed = record.closed_tasks.len() >= record.tasks.len();
}

fn partial_closed_annotation(record: &DispatchRecord) -> Option<String> {
    if record.closed_tasks.is_empty() || record.closed_tasks.len() >= record.tasks.len() {
        return None;
    }
    let missing = record
        .tasks
        .iter()
        .filter(|task| !record.closed_tasks.contains(*task))
        .cloned()
        .collect::<Vec<_>>();
    Some(format!(
        "PARTIAL_CLOSED={}/{} missing=[{}]",
        record.closed_tasks.len(),
        record.tasks.len(),
        missing.join(", ")
    ))
}

fn extra<'a>(entry: &'a TxEntry, key: &str) -> Option<&'a str> {
    entry
        .extra
        .iter()
        .find(|(got, _)| got == key)
        .map(|(_, value)| value.as_str())
}

/// Read a tx property by its current key, falling back to a legacy key for
/// records written before the de-codex rename (dual-read back-compat).
fn extra_compat<'a>(entry: &'a TxEntry, key: &str, legacy_key: &str) -> Option<&'a str> {
    extra(entry, key).or_else(|| extra(entry, legacy_key))
}

// The CLI-side `live_run_blocking_cleanup` that used to live here was
// TASK-6AYEJ.3's answer to "is a live worker occupying this worktree". It read
// a `/runs/live` snapshot in this process and acted on it in this process, so a
// recovery landing in between was invisible to it. TASK-1T3FZ moved that
// decision to `Supervisor::reserve_dispatch_close`, which makes it under the
// same lock that admits an acquire and installs the fence before answering.
// Do not reintroduce a CLI-side copy: a second opinion that cannot see the
// reservation is exactly the thing that read as safe and was not.

fn dispatch_health(record: &DispatchRecord, live_runs: &[RunSummary]) -> DispatchHealth {
    let worktree_exists = record
        .worktree
        .as_ref()
        .map(|path| path.exists())
        .unwrap_or(false);
    let derived_pid = match record.worker_pid {
        Some(pid) => Some(pid),
        None if record.pid.is_some() => record.pid,
        None => derive_worker_pid(record),
    };
    let pid_alive = derived_pid.map(pid_is_alive).unwrap_or(false);
    let run_alive = record
        .run_id
        .as_deref()
        .map(|run_id| live_runs.iter().any(|run| run.run_id == run_id))
        .unwrap_or(false);
    DispatchHealth {
        worktree_exists,
        pid: derived_pid,
        pid_alive,
        run_alive,
    }
}

fn scan_cleanup_failures(project_root: &Path) -> Result<Vec<CleanupFailureRecord>> {
    let mut failures = Vec::new();
    for entry in read_tx_entries(project_root)? {
        if !matches!(
            entry.ty.as_str(),
            "implementer.done" | "reviewer.done" | "architector.done" | "manager.dispatch_aborted"
        ) {
            continue;
        }
        let Some(status) = extra(&entry, "CLEANUP_STATUS") else {
            continue;
        };
        if !cleanup_status_reports_failure(status) {
            continue;
        }
        let tasks = entry
            .task
            .as_deref()
            .map(split_task_list)
            .unwrap_or_default();
        failures.push(CleanupFailureRecord {
            tx_id: entry.tx_id.clone(),
            ty: entry.ty.clone(),
            tasks,
            status: status.to_string(),
            error: extra(&entry, "CLEANUP_ERROR").map(str::to_string),
        });
    }
    Ok(failures)
}

fn cleanup_status_reports_failure(status: &str) -> bool {
    status != CleanupStatus::Ok.as_str() && status != CleanupStatus::CleanupAlreadyRun.as_str()
}

/// Best-effort recovery of a detached worker's pid by matching its process
/// against the dispatch's last-message artifact path. Only the codex harness
/// has a known process signature (`codex exec --output-last-message <path>`);
/// for any other harness we cannot yet derive the pid and return None.
fn derive_worker_pid(record: &DispatchRecord) -> Option<u32> {
    // Harness is recorded on the `run.created` tx. Skip the codex-specific ps
    // grep only when the harness is explicitly something else; an unknown/absent
    // harness (legacy records, dispatch_started before run.created) falls
    // through to the codex best-effort, preserving prior behavior.
    if let Some(harness) = record.harness.as_deref() {
        if harness != "codex" {
            tracing::debug!(
                harness,
                "pid derivation not implemented for this harness; skipping"
            );
            return None;
        }
    }
    let brief_path = record.brief_path.as_ref()?;
    let (last_path, _) = record
        .last_path
        .as_ref()
        .map(|path| dispatch_sibling_artifact_paths_from_last(path))
        .unwrap_or_else(|| dispatch_sibling_artifact_paths(brief_path));
    let last_path = last_path.display().to_string();
    let output = Command::new("ps")
        .args(["-axo", "pid=,command="])
        .output()
        .or_else(|_| Command::new("ps").args(["-eo", "pid=,command="]).output())
        .ok()?;
    if !output.status.success() {
        return None;
    }
    for line in String::from_utf8_lossy(&output.stdout).lines() {
        if !line.contains("codex") || !line.contains("exec") {
            continue;
        }
        if !line.contains("--output-last-message") || !line.contains(&last_path) {
            continue;
        }
        let pid = line.split_whitespace().next()?.parse::<u32>().ok()?;
        return Some(pid);
    }
    None
}

fn pid_is_alive(pid: u32) -> bool {
    #[cfg(unix)]
    {
        let pid = match i32::try_from(pid) {
            Ok(pid) => pid,
            Err(_) => return false,
        };
        let rc = unsafe { libc::kill(pid, 0) };
        if rc == 0 {
            return true;
        }
        let err = std::io::Error::last_os_error();
        err.raw_os_error() == Some(libc::EPERM)
    }
    #[cfg(not(unix))]
    {
        let output = Command::new("ps").args(["-p", &pid.to_string()]).output();
        output.map(|out| out.status.success()).unwrap_or(false)
    }
}

fn optional_value(value: Option<&str>) -> Option<String> {
    value
        .map(sanitize_tx_value)
        .filter(|value| !value.is_empty())
}

fn parse_close_property(value: &str) -> Result<(String, String), String> {
    let (key, raw_value) = value
        .split_once('=')
        .ok_or_else(|| "property must be KEY=VALUE".to_string())?;
    if !is_uppercase_snake_key(key) {
        return Err("property key must match [A-Z][A-Z0-9_]*".to_string());
    }
    Ok((key.to_string(), raw_value.to_string()))
}

fn is_uppercase_snake_key(key: &str) -> bool {
    let mut chars = key.chars();
    match chars.next() {
        Some(ch) if ch.is_ascii_uppercase() => {}
        _ => return false,
    }
    chars.all(|ch| ch.is_ascii_uppercase() || ch.is_ascii_digit() || ch == '_')
}

fn sanitize_tx_value(value: &str) -> String {
    value
        .chars()
        .map(|ch| if ch == '\n' || ch == '\r' { ' ' } else { ch })
        .collect::<String>()
        .trim()
        .to_string()
}

fn shell_quote(path: &Path) -> String {
    let value = path.display().to_string();
    let escaped = value.replace('\'', "'\"'\"'");
    format!("'{escaped}'")
}

fn path_segment(value: &str) -> String {
    let mut encoded = String::new();
    for byte in value.bytes() {
        if byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.' | b'~') {
            encoded.push(char::from(byte));
        } else {
            encoded.push_str(&format!("%{byte:02X}"));
        }
    }
    encoded
}

#[cfg(test)]
mod tests {
    use super::*;

    fn architector_record() -> DispatchRecord {
        DispatchRecord {
            tx_id: "tx-arch".to_string(),
            tasks: vec!["TASK-086".to_string()],
            kind: "architector".to_string(),
            worktree: None,
            branch: None,
            model: None,
            effort: None,
            brief_path: None,
            last_path: None,
            stdout_path: None,
            dispatch_attempt_token: None,
            run_id: None,
            run_ids: BTreeSet::new(),
            worker_id: None,
            driver: None,
            harness: None,
            pid: None,
            started_at: None,
            worker_pid: None,
            goal_id: None,
            closed_tasks: BTreeSet::new(),
            cleanup_already_run: false,
            reported: false,
            closed: false,
        }
    }

    #[test]
    fn derives_slug_defaults_for_task_paths() {
        let project_root = PathBuf::from("/repo/main");
        assert_eq!(task_slug("TASK-047.5.1"), "task-047.5.1");
        assert_eq!(
            default_worktree(&project_root, "TASK-047.5.1", DispatchKind::Implementer),
            PathBuf::from("/repo/main/.orgasmic/tmp/dispatch/task-047.5.1/worktree")
        );
        assert_eq!(
            default_worktree(&project_root, "TASK-047.5.1", DispatchKind::Reviewer),
            PathBuf::from("/repo/main/.orgasmic/tmp/dispatch/task-047.5.1-review/worktree")
        );
        assert_eq!(
            default_worktree(&project_root, "TASK-086", DispatchKind::Architector),
            PathBuf::from("/repo/main/.orgasmic/tmp/dispatch/task-086-arch/worktree")
        );
        assert_eq!(
            default_branch("TASK-047.5.1", DispatchKind::Implementer),
            "task-047.5.1-impl"
        );
        assert_eq!(
            default_branch("TASK-047.5.1", DispatchKind::Reviewer),
            "task-047.5.1-review"
        );
        assert_eq!(
            default_branch("TASK-086", DispatchKind::Architector),
            "task-086-arch"
        );
        assert_ne!(
            default_worktree(&project_root, "TASK-086", DispatchKind::Architector),
            default_worktree(&project_root, "TASK-086", DispatchKind::Implementer)
        );
        assert_ne!(
            default_worktree(&project_root, "TASK-086", DispatchKind::Architector),
            default_worktree(&project_root, "TASK-086", DispatchKind::Reviewer)
        );
    }

    /// orgasmic:TASK-JK66P — a stall tombstone now names the evidence that was
    /// absent and for how long, so an operator can tell a wedged harness from a
    /// worker shot mid-build. Classification keys on the leading token, and a
    /// more informative reason must not stop reading as a stall sweep.
    #[test]
    fn a_stall_reason_carrying_its_evidence_detail_is_still_a_stall_sweep() {
        assert!(is_stall_sweep_release_reason("stall_timeout_exceeded"));
        assert!(is_stall_sweep_release_reason(
            "stall_timeout_exceeded: no work evidence for 612s; 1 process(es) under \
             pid 4242 at 0.2% cpu (work threshold 5.0%)"
        ));
        assert!(is_stall_sweep_release_reason("idle_timeout_exceeded"));
        assert!(is_stall_sweep_release_reason("max_run_duration_exceeded"));
        // Still not a sweep: a worker-declared or operator release, whatever
        // it carries after a colon.
        assert!(!is_stall_sweep_release_reason("worker finalize for TASK-X"));
        assert!(!is_stall_sweep_release_reason(
            "protocol_end_without_finalize"
        ));
        assert!(!is_stall_sweep_release_reason(""));
    }

    #[test]
    fn dispatch_release_tombstone_reads_worker_finalize_and_protocol_end() {
        let tmp = tempfile::tempdir().unwrap();
        let identity = RuntimeIdentity {
            run_id: "run-tomb".into(),
            runtime_id: "rt-tomb".into(),
            boot_id: "boot-tomb".into(),
        };

        let path = tmp.path().join("session.jsonl");
        let mut writer = orgasmic_core::SessionWriter::open(&path, identity.clone()).unwrap();
        writer
            .append(
                SessionEventKind::Lifecycle,
                serde_json::to_value(Lifecycle::Release {
                    reason: "protocol_end_without_finalize".into(),
                    outcome: orgasmic_core::ReleaseOutcome::Failed,
                    finalized_by_worker: false,
                })
                .unwrap(),
            )
            .unwrap();
        assert_eq!(
            dispatch_release_tombstone(&path).unwrap(),
            DispatchReleaseTombstone::ProtocolEndWithoutFinalize
        );

        let path2 = tmp.path().join("session2.jsonl");
        let mut writer2 = orgasmic_core::SessionWriter::open(&path2, identity).unwrap();
        writer2
            .append(
                SessionEventKind::Lifecycle,
                serde_json::to_value(Lifecycle::Release {
                    reason: "worker finalize for TASK-X".into(),
                    outcome: orgasmic_core::ReleaseOutcome::Completed,
                    finalized_by_worker: true,
                })
                .unwrap(),
            )
            .unwrap();
        assert_eq!(
            dispatch_release_tombstone(&path2).unwrap(),
            DispatchReleaseTombstone::WorkerFinalized
        );

        // orgasmic:TASK-S52X9 — artifact_submitted / manager_released are
        // valid worker-declared tombstones (idempotent finalize proceeds).
        let path3 = tmp.path().join("session3.jsonl");
        let mut writer3 = orgasmic_core::SessionWriter::open(
            &path3,
            RuntimeIdentity {
                run_id: "run-art".into(),
                runtime_id: "rt-art".into(),
                boot_id: "boot-art".into(),
            },
        )
        .unwrap();
        writer3
            .append(
                SessionEventKind::Lifecycle,
                serde_json::to_value(Lifecycle::Release {
                    reason: "artifact_submitted".into(),
                    outcome: orgasmic_core::ReleaseOutcome::Completed,
                    finalized_by_worker: true,
                })
                .unwrap(),
            )
            .unwrap();
        assert_eq!(
            dispatch_release_tombstone(&path3).unwrap(),
            DispatchReleaseTombstone::ArtifactSubmitted
        );
        assert!(is_worker_declared_tombstone(
            DispatchReleaseTombstone::ArtifactSubmitted
        ));

        let path4 = tmp.path().join("session4.jsonl");
        let mut writer4 = orgasmic_core::SessionWriter::open(
            &path4,
            RuntimeIdentity {
                run_id: "run-mgr".into(),
                runtime_id: "rt-mgr".into(),
                boot_id: "boot-mgr".into(),
            },
        )
        .unwrap();
        writer4
            .append(
                SessionEventKind::Lifecycle,
                serde_json::to_value(Lifecycle::Release {
                    reason: "manager_released".into(),
                    outcome: orgasmic_core::ReleaseOutcome::Completed,
                    finalized_by_worker: true,
                })
                .unwrap(),
            )
            .unwrap();
        assert_eq!(
            dispatch_release_tombstone(&path4).unwrap(),
            DispatchReleaseTombstone::ManagerReleased
        );
        assert!(is_worker_declared_tombstone(
            DispatchReleaseTombstone::ManagerReleased
        ));
    }

    #[test]
    fn classifies_release_not_found_vs_other_release_errors() {
        // TASK-DWJVH item B: only a 404 ("already released", e.g. the
        // stall sweep won the race) is treated as success-with-warning.
        // Everything else — in particular a 409 ownership mismatch, meaning
        // a *different* run reclaimed this run_id — must still hard-error.
        let not_found = anyhow::anyhow!(
            "daemon returned 404 Not Found: {{\"error\":\"active run run-x not found\"}}"
        );
        assert!(is_release_run_not_found_error(&not_found));

        let ownership_conflict = anyhow::anyhow!(
            "conflict — node changed on disk; reload base_version and retry: {{\"error\":\"runtime ownership mismatch\"}}"
        );
        assert!(!is_release_run_not_found_error(&ownership_conflict));

        let unreachable =
            anyhow::anyhow!("daemon request failed: connection refused — is the daemon reachable?");
        assert!(!is_release_run_not_found_error(&unreachable));
    }

    #[test]
    fn parses_architector_kind_and_maps_lifecycle() {
        let kind = <DispatchKind as ValueEnum>::from_str("architector", false).unwrap();
        assert_eq!(kind, DispatchKind::Architector);
        assert_eq!(kind.as_str(), "architector");
        assert_eq!(kind.to_string(), "architector");

        assert_eq!(
            dispatch_lifecycle_transitions(kind, &["TASK-086".to_string()]),
            vec![("TASK-086".to_string(), LifecycleStage::InProgress)]
        );
        assert!(dispatchable_stage(kind, LifecycleStage::Backlog));
        assert!(dispatchable_stage(kind, LifecycleStage::Todo));
        assert!(!dispatchable_stage(kind, LifecycleStage::InReview));
        assert_eq!(allowed_stage_text(kind), "BACKLOG or TODO");

        let open = architector_record();
        assert_eq!(done_tx_type(&open).unwrap(), "architector.done");
    }

    #[test]
    fn closes_architector_lifecycle_to_done() {
        let tmp = tempfile::tempdir().unwrap();
        let in_progress = tmp.path().join(".orgasmic/tasks/in_progress.org");
        std::fs::create_dir_all(in_progress.parent().unwrap()).unwrap();
        std::fs::write(
            &in_progress,
            "#+title: in progress\n#+orgasmic_version: 1\n\n* IN_PROGRESS TASK-086 Architecture run\n:PROPERTIES:\n:ID:               TASK-086\n:END:\n",
        )
        .unwrap();
        let open = architector_record();
        let args = DispatchCloseArgs {
            task: vec!["TASK-086".to_string()],
            started_tx: None,
            status: DispatchCloseStatus::Done,
            merge_sha: Some("abc123".to_string()),
            worker_commit: None,
            worker_session: None,
            reviewed_diff: None,
            properties: Vec::new(),
            tokens: None,
            wall: None,
            reason: None,
            worktree_remove: true,
            no_worktree_remove: false,
            branch_delete: false,
        };

        assert_eq!(
            close_lifecycle_transitions(tmp.path(), &["TASK-086".to_string()], &open, &args)
                .unwrap(),
            vec![CloseTransition {
                task: "TASK-086".to_string(),
                from: LifecycleStage::InProgress,
                to: LifecycleStage::Done,
            }]
        );
    }

    /// TASK-EP3H1: the reconciler's whole safety argument is "the close tx is
    /// the last lifecycle word on this task". Anything later — a deliberate
    /// move, a newer close — takes the candidate off the list.
    #[test]
    fn torn_close_candidates_yield_to_any_later_lifecycle_event() {
        let tmp = tempfile::tempdir().unwrap();
        let tx_dir = tmp.path().join(".orgasmic/tx");
        std::fs::create_dir_all(&tx_dir).unwrap();
        let close = |tx_id: &str, task: &str, from: &str, to: &str| {
            format!(
                "* TX 2026-07-29 Wed 10:00:00 implementer.done {task}\n:PROPERTIES:\n:TX_ID:        {tx_id}\n:TIME:         [2026-07-29 Wed 10:00:00]\n:TYPE:         implementer.done\n:ACTOR:        a@example.com\n:MACHINE:      host\n:PROJECT:      orgasmic\n:TASK:         {task}\n:CLOSED_TX:    tx-start-{task}\n:LIFECYCLE_FROM: {from}\n:LIFECYCLE_TO: {to}\n:END:\n"
            )
        };
        let transitioned = |task: &str| {
            format!(
                "* TX 2026-07-29 Wed 11:00:00 task.state_transitioned {task}\n:PROPERTIES:\n:TX_ID:        tx-moved-{task}\n:TIME:         [2026-07-29 Wed 11:00:00]\n:TYPE:         task.state_transitioned\n:ACTOR:        a@example.com\n:MACHINE:      host\n:PROJECT:      orgasmic\n:TASK:         {task}\n:END:\n"
            )
        };
        std::fs::write(
            tx_dir.join("2026-07.org"),
            format!(
                "#+title: tx\n#+orgasmic_version: 1\n\n{}\n{}\n{}\n{}",
                close("tx-1", "TASK-TORN", "in_progress", "in_review"),
                close("tx-2", "TASK-LANDED", "in_progress", "in_review"),
                transitioned("TASK-LANDED"),
                // No LIFECYCLE_* at all: a close written before this task
                // shipped carries no intent and is not repairable.
                "* TX 2026-07-29 Wed 12:00:00 implementer.done TASK-LEGACY\n:PROPERTIES:\n:TX_ID:        tx-3\n:TIME:         [2026-07-29 Wed 12:00:00]\n:TYPE:         implementer.done\n:ACTOR:        a@example.com\n:MACHINE:      host\n:PROJECT:      orgasmic\n:TASK:         TASK-LEGACY\n:CLOSED_TX:    tx-start-legacy\n:END:\n",
            ),
        )
        .unwrap();

        let candidates = torn_close_candidates(tmp.path()).unwrap();
        assert_eq!(
            candidates,
            vec![(
                "tx-start-TASK-TORN".to_string(),
                CloseTransition {
                    task: "TASK-TORN".to_string(),
                    from: LifecycleStage::InProgress,
                    to: LifecycleStage::InReview,
                }
            )]
        );
    }

    #[test]
    fn close_lifecycle_request_id_is_stable_per_task_and_generation() {
        // TASK-EP3H1: the repair must re-send the SAME request id the close
        // sent, or the daemon cannot recognise a lost-response replay.
        assert_eq!(
            close_lifecycle_request_id("TASK-086", "tx-20260729-orgasmic-1"),
            close_lifecycle_request_id("TASK-086", "tx-20260729-orgasmic-1")
        );
        assert_ne!(
            close_lifecycle_request_id("TASK-086", "tx-20260729-orgasmic-1"),
            close_lifecycle_request_id("TASK-086", "tx-20260729-orgasmic-2")
        );
    }

    #[test]
    fn reserve_dispatch_artifact_pair_creates_zero_length_files() {
        let tmp = tempfile::tempdir().unwrap();
        let project_root = tmp.path().join("repo");
        let brief = project_root.join("task-reserve-brief.md");
        let (_, last, stdout, attempt) =
            reserve_dispatch_artifact_paths(&project_root, &brief).unwrap();
        assert_eq!(std::fs::metadata(&last).unwrap().len(), 0);
        assert_eq!(std::fs::metadata(&stdout).unwrap().len(), 0);
        assert_eq!(attempt.len(), 32);
        assert!(attempt.chars().all(|c| c.is_ascii_hexdigit()));
    }

    #[test]
    fn reserve_dispatch_artifact_pair_preserves_preexisting_collider() {
        let tmp = tempfile::tempdir().unwrap();
        let project_root = tmp.path().join("repo");
        let brief = project_root.join("task-reserve-brief.md");
        let attempt = "aaaa1111bbbb2222cccc3333dddd4444";
        let (_, last, stdout) = dispatch_artifact_paths_for_attempt(&project_root, &brief, attempt);
        std::fs::create_dir_all(last.parent().unwrap()).unwrap();
        std::fs::write(&last, "existing").unwrap();
        assert!(matches!(
            reserve_dispatch_artifact_pair(&last, &stdout),
            Err(ReservePairError::Collision)
        ));
        assert_eq!(std::fs::read_to_string(&last).unwrap(), "existing");
        assert!(!stdout.exists());
    }

    #[test]
    fn reserve_dispatch_artifact_pair_second_file_collision_preserves_first_collider() {
        let tmp = tempfile::tempdir().unwrap();
        let project_root = tmp.path().join("repo");
        let brief = project_root.join("task-reserve-brief.md");
        let attempt = "aaaa1111bbbb2222cccc3333dddd4444";
        let (_, last, stdout) = dispatch_artifact_paths_for_attempt(&project_root, &brief, attempt);
        std::fs::create_dir_all(last.parent().unwrap()).unwrap();
        std::fs::write(&last, "existing-last").unwrap();
        std::fs::write(&stdout, "existing-stdout").unwrap();
        assert!(matches!(
            reserve_dispatch_artifact_pair(&last, &stdout),
            Err(ReservePairError::Collision)
        ));
        assert_eq!(std::fs::read_to_string(&last).unwrap(), "existing-last");
        assert_eq!(std::fs::read_to_string(&stdout).unwrap(), "existing-stdout");
    }

    #[test]
    fn dispatch_failure_needs_daemon_cleanup_decode_is_ambiguous() {
        let err = anyhow::anyhow!("decode daemon response: EOF while parsing a value");
        assert!(crate::daemon_client::DaemonClient::dispatch_failure_needs_daemon_cleanup(&err));
    }

    #[test]
    fn places_dispatch_artifacts_under_per_task_subfolder() {
        let project_root = PathBuf::from("/repo/main");
        let brief = PathBuf::from("/elsewhere/task-045-impl-brief.md");
        let (resolved_brief, last, stdout) =
            dispatch_artifact_paths_for_attempt(&project_root, &brief, "a1b2c3d4");
        assert_eq!(
            resolved_brief,
            PathBuf::from("/repo/main/.orgasmic/tmp/dispatch/task-045-impl/task-045-impl-brief.md")
        );
        assert_eq!(
            last,
            PathBuf::from(
                "/repo/main/.orgasmic/tmp/dispatch/task-045-impl/task-045-impl-a1b2c3d4-last.txt"
            )
        );
        assert_eq!(
            stdout,
            PathBuf::from(
                "/repo/main/.orgasmic/tmp/dispatch/task-045-impl/task-045-impl-a1b2c3d4-stdout.log"
            )
        );
        let (sib_last, sib_stdout) = dispatch_sibling_artifact_paths_from_last(&last);
        assert_eq!(sib_last, last);
        assert_eq!(sib_stdout, stdout);
    }

    #[test]
    fn attempt_scoped_paths_isolate_consecutive_dispatches() {
        let tmp = tempfile::tempdir().unwrap();
        let project_root = tmp.path().join("repo");
        let brief =
            project_root.join(".orgasmic/tmp/dispatch/task-045-impl/task-045-impl-brief.md");
        let (_, last1, _) = dispatch_artifact_paths_for_attempt(&project_root, &brief, "attempt1");
        let (_, last2, _) = dispatch_artifact_paths_for_attempt(&project_root, &brief, "attempt2");
        assert_ne!(last1, last2);
        std::fs::create_dir_all(last1.parent().unwrap()).unwrap();
        std::fs::write(&last1, "attempt 1 report").unwrap();
        assert!(
            !last2.exists(),
            "attempt 2 waiter path must not observe attempt 1 report"
        );
    }

    #[test]
    fn tx_scan_returns_only_unclosed_dispatches() {
        let tmp = tempfile::tempdir().unwrap();
        let tx_dir = tmp.path().join(".orgasmic/tx");
        std::fs::create_dir_all(&tx_dir).unwrap();
        std::fs::write(
            tx_dir.join("2026-05.org"),
            "#+title: tx\n#+orgasmic_version: 1\n\n* TX 2026-05-23 Sat 10:00:00 manager.dispatch_started TASK-1\n:PROPERTIES:\n:TX_ID:        tx-start-1\n:TIME:         [2026-05-23 Sat 10:00:00]\n:TYPE:         manager.dispatch_started\n:ACTOR:        a@example.com\n:MACHINE:      host\n:PROJECT:      orgasmic\n:TASK:         TASK-1\n:KIND:         implementer\n:WORKTREE:     /tmp/orgasmic-worktrees/task-1\n:BRANCH:       task-1-impl\n:CODEX_MODEL:  gpt-5.5\n:CODEX_EFFORT: high\n:STARTED_AT:   [2026-05-23 Sat 10:00:00]\n:END:\n\n* TX 2026-05-23 Sat 10:10:00 implementer.done TASK-1\n:PROPERTIES:\n:TX_ID:        tx-done-1\n:TIME:         [2026-05-23 Sat 10:10:00]\n:TYPE:         implementer.done\n:ACTOR:        agent.implementer\n:MACHINE:      host\n:PROJECT:      orgasmic\n:TASK:         TASK-1\n:CLOSED_TX:    tx-start-1\n:END:\n\n* TX 2026-05-23 Sat 10:20:00 manager.dispatch_started TASK-2\n:PROPERTIES:\n:TX_ID:        tx-start-2\n:TIME:         [2026-05-23 Sat 10:20:00]\n:TYPE:         manager.dispatch_started\n:ACTOR:        a@example.com\n:MACHINE:      host\n:PROJECT:      orgasmic\n:TASK:         TASK-2\n:KIND:         reviewer\n:WORKTREE:     /tmp/orgasmic-worktrees/task-2-review\n:BRANCH:       task-2-review\n:CODEX_MODEL:  gpt-5.5\n:CODEX_EFFORT: high\n:STARTED_AT:   [2026-05-23 Sat 10:20:00]\n:END:\n",
        )
        .unwrap();

        let open = scan_open_dispatches(tmp.path()).unwrap();
        assert_eq!(open.len(), 1);
        assert_eq!(open[0].tx_id, "tx-start-2");
        assert_eq!(open[0].tasks, vec!["TASK-2".to_string()]);
        assert_eq!(open[0].kind, "reviewer");

        // The TASK-1 close above is the HISTORICAL shape: an `implementer.done`
        // authored by `agent.implementer` — a worker's own finalize, from
        // before TASK-6AYEJ split reporting from closing. Those ~10 records on
        // this repo must stay closed with no migration, and `dispatch-close`
        // must find them so a re-close is a no-op rather than an error.
        let closed = latest_closed_dispatch_for_tasks(tmp.path(), &["TASK-1".to_string()])
            .unwrap()
            .expect("historical worker-closed dispatch must still resolve as closed");
        assert_eq!(closed.tx_id, "tx-start-1");
        assert!(closed.closed);
        assert!(
            latest_closed_dispatch_for_tasks(tmp.path(), &["TASK-2".to_string()])
                .unwrap()
                .is_none(),
            "an open dispatch must not be reported as already closed"
        );
    }

    /// TASK-6AYEJ: a worker's finalize reports completion; it does not close
    /// the dispatch. The dispatch stays open (and is flagged reported) until
    /// the manager's `dispatch-close` emits the `*.done` tx.
    #[test]
    fn worker_reported_tx_keeps_dispatch_open_until_manager_done() {
        let tmp = tempfile::tempdir().unwrap();
        let tx_dir = tmp.path().join(".orgasmic/tx");
        std::fs::create_dir_all(&tx_dir).unwrap();
        let started = "* TX 2026-07-26 Sun 10:00:00 manager.dispatch_started TASK-9\n:PROPERTIES:\n:TX_ID:        tx-start-9\n:TIME:         [2026-07-26 Sun 10:00:00]\n:TYPE:         manager.dispatch_started\n:ACTOR:        a@example.com\n:MACHINE:      host\n:PROJECT:      orgasmic\n:TASK:         TASK-9\n:KIND:         implementer\n:WORKTREE:     /tmp/orgasmic-worktrees/task-9\n:BRANCH:       task-9-impl\n:STARTED_AT:   [2026-07-26 Sun 10:00:00]\n:END:\n\n";
        let reported = "* TX 2026-07-26 Sun 10:10:00 implementer.reported TASK-9\n:PROPERTIES:\n:TX_ID:        tx-reported-9\n:TIME:         [2026-07-26 Sun 10:10:00]\n:TYPE:         implementer.reported\n:ACTOR:        agent.implementer\n:MACHINE:      host\n:PROJECT:      orgasmic\n:TASK:         TASK-9\n:SHA:          e7837f1\n:END:\n\n";
        let closed = "* TX 2026-07-26 Sun 11:00:00 implementer.done TASK-9\n:PROPERTIES:\n:TX_ID:        tx-done-9\n:TIME:         [2026-07-26 Sun 11:00:00]\n:TYPE:         implementer.done\n:ACTOR:        a@example.com\n:MACHINE:      host\n:PROJECT:      orgasmic\n:TASK:         TASK-9\n:MERGE_SHA:    0daa77c\n:CLOSED_TX:    tx-start-9\n:END:\n";
        let header = "#+title: tx\n#+orgasmic_version: 1\n\n";

        std::fs::write(
            tx_dir.join("2026-07.org"),
            format!("{header}{started}{reported}"),
        )
        .unwrap();
        let open = scan_open_dispatches(tmp.path()).unwrap();
        assert_eq!(
            open.len(),
            1,
            "a worker finalize must leave the dispatch open for the manager"
        );
        assert_eq!(open[0].tx_id, "tx-start-9");
        assert!(
            open[0].reported,
            "the open dispatch should be flagged as reported by its worker"
        );

        std::fs::write(
            tx_dir.join("2026-07.org"),
            format!("{header}{started}{reported}{closed}"),
        )
        .unwrap();
        assert!(
            scan_open_dispatches(tmp.path()).unwrap().is_empty(),
            "the manager's `*.done` must close the dispatch"
        );
    }

    // ---- TASK-6AYEJ.1 generation-bound close fixtures -------------------

    fn tx_started(tx_id: &str, task: &str, kind: &str, time: &str) -> String {
        format!(
            "* TX 2026-07-26 Sun {time} manager.dispatch_started {task}\n:PROPERTIES:\n:TX_ID:        {tx_id}\n:TIME:         [2026-07-26 Sun {time}]\n:TYPE:         manager.dispatch_started\n:ACTOR:        a@example.com\n:MACHINE:      host\n:PROJECT:      orgasmic\n:TASK:         {task}\n:KIND:         {kind}\n:WORKTREE:     /tmp/orgasmic-worktrees/{tx_id}\n:BRANCH:       {tx_id}-branch\n:STARTED_AT:   [2026-07-26 Sun {time}]\n:END:\n\n"
        )
    }

    fn tx_run_created(
        tx_id: &str,
        task: &str,
        dispatch_tx: &str,
        run_id: &str,
        kind: &str,
        time: &str,
    ) -> String {
        format!(
            "* TX 2026-07-26 Sun {time} run.created {task}\n:PROPERTIES:\n:TX_ID:        {tx_id}\n:TIME:         [2026-07-26 Sun {time}]\n:TYPE:         run.created\n:ACTOR:        daemon\n:MACHINE:      host\n:PROJECT:      orgasmic\n:TASK:         {task}\n:ORIGIN:       cli_dispatch\n:DISPATCH_TX:  {dispatch_tx}\n:RUN_ID:       {run_id}\n:KIND:         {kind}\n:END:\n\n"
        )
    }

    fn tx_terminal(tx_id: &str, task: &str, ty: &str, closed_tx: &str, time: &str) -> String {
        format!(
            "* TX 2026-07-26 Sun {time} {ty} {task}\n:PROPERTIES:\n:TX_ID:        {tx_id}\n:TIME:         [2026-07-26 Sun {time}]\n:TYPE:         {ty}\n:ACTOR:        a@example.com\n:MACHINE:      host\n:PROJECT:      orgasmic\n:TASK:         {task}\n:REASON:       fixture\n:CLOSED_TX:    {closed_tx}\n:END:\n\n"
        )
    }

    fn tx_reported(tx_id: &str, task: &str, ty: &str, run_id: Option<&str>, time: &str) -> String {
        let run_id_line = run_id
            .map(|run_id| format!(":RUN_ID:       {run_id}\n"))
            .unwrap_or_default();
        format!(
            "* TX 2026-07-26 Sun {time} {ty} {task}\n:PROPERTIES:\n:TX_ID:        {tx_id}\n:TIME:         [2026-07-26 Sun {time}]\n:TYPE:         {ty}\n:ACTOR:        agent.implementer\n:MACHINE:      host\n:PROJECT:      orgasmic\n:TASK:         {task}\n{run_id_line}:END:\n"
        )
    }

    fn write_tx_log(project_root: &Path, body: &str) {
        let tx_dir = project_root.join(".orgasmic/tx");
        std::fs::create_dir_all(&tx_dir).unwrap();
        std::fs::write(
            tx_dir.join("2026-07.org"),
            format!("#+title: tx\n#+orgasmic_version: 1\n\n{body}"),
        )
        .unwrap();
    }

    /// TASK-6AYEJ.1, the ship blocker: closing the implementer moves the task
    /// to IN_REVIEW and a reviewer is opened for the SAME task, so a replayed
    /// implementer close must no-op against its own already-closed generation
    /// even though another dispatch for that task is open. Task-bound
    /// selection picks the live reviewer instead and would release and clean
    /// it up.
    #[test]
    fn stale_close_retry_noops_against_its_own_generation_while_a_successor_is_open() {
        let tmp = tempfile::tempdir().unwrap();
        write_tx_log(
            tmp.path(),
            &format!(
                "{}{}{}",
                tx_started("tx-start-impl", "TASK-9", "implementer", "10:00:00"),
                tx_terminal(
                    "tx-done-impl",
                    "TASK-9",
                    "implementer.done",
                    "tx-start-impl",
                    "11:00:00"
                ),
                tx_started("tx-start-review", "TASK-9", "reviewer", "11:05:00"),
            ),
        );
        let tasks = vec!["TASK-9".to_string()];

        match resolve_close_target(tmp.path(), &tasks, Some("tx-start-impl")).unwrap() {
            CloseTarget::AlreadyClosed(record) => assert_eq!(record.tx_id, "tx-start-impl"),
            CloseTarget::Open(record) => panic!(
                "a stale implementer close must no-op, not open-close {}",
                record.tx_id
            ),
        }

        // The successor is untouched and still closable on its own identity.
        match resolve_close_target(tmp.path(), &tasks, Some("tx-start-review")).unwrap() {
            CloseTarget::Open(record) => {
                assert_eq!(record.tx_id, "tx-start-review");
                assert_eq!(record.kind, "reviewer");
            }
            CloseTarget::AlreadyClosed(record) => {
                panic!("the live reviewer must still be open: {}", record.tx_id)
            }
        }

        // Fail closed: a generation that is not in the ledger is an error, not
        // a fall-through to "whatever is open for this task".
        let err = resolve_close_target(tmp.path(), &tasks, Some("tx-start-ghost")).unwrap_err();
        assert!(
            err.to_string().contains("tx-start-ghost"),
            "unexpected error: {err}"
        );

        // A --started-tx for a different task never silently retargets.
        let err = resolve_close_target(
            tmp.path(),
            &["TASK-OTHER".to_string()],
            Some("tx-start-impl"),
        )
        .unwrap_err();
        assert!(
            err.to_string().contains("does not include requested task"),
            "unexpected error: {err}"
        );
    }

    /// The other generation shape named by the review: a dispatch is aborted,
    /// a fresh one is dispatched for the same task, and the abort is replayed.
    #[test]
    fn stale_abort_retry_noops_after_the_task_was_redispatched() {
        let tmp = tempfile::tempdir().unwrap();
        write_tx_log(
            tmp.path(),
            &format!(
                "{}{}{}",
                tx_started("tx-start-a", "TASK-9", "implementer", "10:00:00"),
                tx_terminal(
                    "tx-abort-a",
                    "TASK-9",
                    "manager.dispatch_aborted",
                    "tx-start-a",
                    "10:30:00"
                ),
                tx_started("tx-start-b", "TASK-9", "implementer", "10:35:00"),
            ),
        );
        let tasks = vec!["TASK-9".to_string()];

        match resolve_close_target(tmp.path(), &tasks, Some("tx-start-a")).unwrap() {
            CloseTarget::AlreadyClosed(record) => assert_eq!(record.tx_id, "tx-start-a"),
            CloseTarget::Open(record) => panic!(
                "a stale abort must not select the redispatched generation {}",
                record.tx_id
            ),
        }
        match resolve_close_target(tmp.path(), &tasks, Some("tx-start-b")).unwrap() {
            CloseTarget::Open(record) => assert_eq!(record.tx_id, "tx-start-b"),
            CloseTarget::AlreadyClosed(record) => {
                panic!("the redispatched run must still be open: {}", record.tx_id)
            }
        }
    }

    /// TASK-6AYEJ.2, the other half of the boundary: a tokenless close may act
    /// on a CLOSED record, never on a LIVE one. With an open generation for the
    /// task, task-bound selection would have released it; now the close is
    /// refused and the refusal names the token to copy.
    #[test]
    fn close_without_started_tx_refuses_while_a_dispatch_is_live() {
        let tmp = tempfile::tempdir().unwrap();
        write_tx_log(
            tmp.path(),
            &format!(
                "{}{}{}",
                tx_started("tx-start-impl", "TASK-9", "implementer", "10:00:00"),
                tx_terminal(
                    "tx-done-impl",
                    "TASK-9",
                    "implementer.done",
                    "tx-start-impl",
                    "11:00:00"
                ),
                tx_started("tx-start-review", "TASK-9", "reviewer", "11:05:00"),
            ),
        );
        let tasks = vec!["TASK-9".to_string()];

        let err = resolve_close_target(tmp.path(), &tasks, None).unwrap_err();
        let message = err.to_string();
        assert!(
            message.contains("--started-tx is required"),
            "unexpected error: {message}"
        );
        assert!(
            message.contains("--started-tx tx-start-review"),
            "the refusal must print a copyable candidate token: {message}"
        );
        assert!(
            !message.contains("tx-start-impl"),
            "only OPEN generations are candidates: {message}"
        );
    }

    /// The compatible half of the same boundary: the ~10 historical
    /// worker-closed dispatches have no generation token and never will, so a
    /// tokenless close of a task whose only dispatch is already closed stays a
    /// clean no-op.
    #[test]
    fn close_without_started_tx_still_noops_on_a_historical_closed_record() {
        let tmp = tempfile::tempdir().unwrap();
        write_tx_log(
            tmp.path(),
            &format!(
                "{}{}",
                tx_started("tx-start-old", "TASK-9", "implementer", "10:00:00"),
                tx_terminal(
                    "tx-done-old",
                    "TASK-9",
                    "implementer.done",
                    "tx-start-old",
                    "11:00:00"
                ),
            ),
        );
        let tasks = vec!["TASK-9".to_string()];
        match resolve_close_target(tmp.path(), &tasks, None).unwrap() {
            CloseTarget::AlreadyClosed(record) => assert_eq!(record.tx_id, "tx-start-old"),
            CloseTarget::Open(record) => panic!("expected already-closed, got {}", record.tx_id),
        }
        let err = resolve_close_target(tmp.path(), &["TASK-NONE".to_string()], None).unwrap_err();
        assert!(
            err.to_string().contains("no open manager.dispatch_started"),
            "unexpected error: {err}"
        );
    }

    /// TASK-6AYEJ.1 finding 3: a PRESENT but unmatched RUN_ID must not fall
    /// through to task overlap. Case 1 — run A aborted, run B dispatched for
    /// the same task, a late `*.reported` for A arrives.
    #[test]
    fn late_report_for_an_aborted_run_does_not_flag_its_successor_reported() {
        let tmp = tempfile::tempdir().unwrap();
        write_tx_log(
            tmp.path(),
            &format!(
                "{}{}{}{}{}{}",
                tx_started("tx-start-a", "TASK-9", "implementer", "10:00:00"),
                tx_run_created(
                    "tx-run-a",
                    "TASK-9",
                    "tx-start-a",
                    "run-a",
                    "implementer",
                    "10:01:00"
                ),
                tx_terminal(
                    "tx-abort-a",
                    "TASK-9",
                    "manager.dispatch_aborted",
                    "tx-start-a",
                    "10:30:00"
                ),
                tx_started("tx-start-b", "TASK-9", "implementer", "10:35:00"),
                tx_run_created(
                    "tx-run-b",
                    "TASK-9",
                    "tx-start-b",
                    "run-b",
                    "implementer",
                    "10:36:00"
                ),
                tx_reported(
                    "tx-reported-a",
                    "TASK-9",
                    "implementer.reported",
                    Some("run-a"),
                    "10:40:00"
                ),
            ),
        );
        let open = scan_open_dispatches(tmp.path()).unwrap();
        assert_eq!(open.len(), 1);
        assert_eq!(open[0].tx_id, "tx-start-b");
        assert!(
            !open[0].reported,
            "run A's late report must not claim run B finished"
        );
    }

    /// Case 2 — two overlapping OPEN records for the same task: the report
    /// attaches to the run it names and to no other.
    #[test]
    fn report_with_run_id_attaches_only_to_the_run_it_names() {
        let tmp = tempfile::tempdir().unwrap();
        write_tx_log(
            tmp.path(),
            &format!(
                "{}{}{}{}{}",
                tx_started("tx-start-a", "TASK-9", "implementer", "10:00:00"),
                tx_run_created(
                    "tx-run-a",
                    "TASK-9",
                    "tx-start-a",
                    "run-a",
                    "implementer",
                    "10:01:00"
                ),
                tx_started("tx-start-b", "TASK-9", "reviewer", "10:05:00"),
                tx_run_created(
                    "tx-run-b",
                    "TASK-9",
                    "tx-start-b",
                    "run-b",
                    "reviewer",
                    "10:06:00"
                ),
                tx_reported(
                    "tx-reported-a",
                    "TASK-9",
                    "implementer.reported",
                    Some("run-a"),
                    "10:40:00"
                ),
            ),
        );
        let open = scan_open_dispatches(tmp.path()).unwrap();
        assert_eq!(open.len(), 2);
        let by_tx = |tx: &str| {
            open.iter()
                .find(|record| record.tx_id == tx)
                .unwrap_or_else(|| panic!("missing {tx}"))
        };
        assert!(by_tx("tx-start-a").reported, "run-a's report attaches to A");
        assert!(
            !by_tx("tx-start-b").reported,
            "the newest overlapping record must not absorb another run's report"
        );
    }

    /// Case 3 — the only case where task overlap is still allowed: a report
    /// that genuinely carries no RUN_ID (pre-RUN_ID records).
    #[test]
    fn report_without_run_id_still_falls_back_to_task_overlap() {
        let tmp = tempfile::tempdir().unwrap();
        write_tx_log(
            tmp.path(),
            &format!(
                "{}{}",
                tx_started("tx-start-legacy", "TASK-9", "implementer", "10:00:00"),
                tx_reported(
                    "tx-reported-legacy",
                    "TASK-9",
                    "implementer.reported",
                    None,
                    "10:40:00"
                ),
            ),
        );
        let open = scan_open_dispatches(tmp.path()).unwrap();
        assert_eq!(open.len(), 1);
        assert!(
            open[0].reported,
            "a legacy report with no RUN_ID must still attach by task overlap"
        );
    }

    #[test]
    fn finalize_commit_message_strips_markdown_heading_prefix() {
        let message = finalize_commit_message(
            "TASK-QKQ3R",
            FinalizeStatus::Done,
            "# TASK-QKQ3R (finalize fix)\n\nbody",
        );
        assert!(
            message.starts_with("TASK-QKQ3R: TASK-QKQ3R (finalize fix)"),
            "expected stripped heading marker, got: {message}"
        );
        assert!(!message.contains("# TASK-QKQ3R (finalize fix)"));

        // A summary with no heading marker is passed through untouched.
        let plain = finalize_commit_message("TASK-QKQ3R", FinalizeStatus::Done, "plain subject");
        assert!(plain.starts_with("TASK-QKQ3R: plain subject"));

        // A nested heading level (`## `) strips all `#`s.
        let nested = finalize_commit_message("TASK-QKQ3R", FinalizeStatus::Done, "## nested");
        assert!(nested.starts_with("TASK-QKQ3R: nested"));
    }

    #[test]
    fn resolve_finalize_run_project_filter_requires_both_sides_known() {
        // Exercises the pure helper logic that `resolve_finalize_run`'s
        // implicit-match filter relies on: the project id check must apply
        // only when both the finalize invocation and the live run report a
        // project id (TASK-QKQ3R part C tolerance for an unknown project id).
        fn matches(run_project: Option<&str>, project_id: Option<&str>) -> bool {
            run_project
                .zip(project_id)
                .map(|(run_project, project_id)| run_project == project_id)
                .unwrap_or(true)
        }
        assert!(matches(Some("orgasmic"), Some("orgasmic")));
        assert!(!matches(Some("orgasmic"), Some("other")));
        assert!(matches(Some("orgasmic"), None));
        assert!(matches(None, Some("orgasmic")));
        assert!(matches(None, None));
    }

    #[test]
    fn local_dispatch_rollback_keeps_branch_when_worktree_validation_fails() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        assert!(Command::new("git")
            .args(["init", "-q"])
            .current_dir(root)
            .status()
            .unwrap()
            .success());
        std::fs::write(root.join("file"), "content").unwrap();
        assert!(Command::new("git")
            .args(["add", "file"])
            .current_dir(root)
            .status()
            .unwrap()
            .success());
        assert!(Command::new("git")
            .args([
                "-c",
                "user.name=Test",
                "-c",
                "user.email=test@example.com",
                "commit",
                "-qm",
                "init",
            ])
            .current_dir(root)
            .status()
            .unwrap()
            .success());
        assert!(Command::new("git")
            .args(["branch", "cleanup-candidate"])
            .current_dir(root)
            .status()
            .unwrap()
            .success());
        let invalid_worktree = root.join("not-a-dispatch-worktree");
        std::fs::create_dir(&invalid_worktree).unwrap();

        let outcome = cleanup_created_resources(
            root,
            &invalid_worktree,
            "cleanup-candidate",
            "TASK-CLEANUP",
            &root.join("missing-last.txt"),
            &root.join("missing-stdout.log"),
        );
        assert_eq!(outcome.status, CleanupStatus::WorktreeFailed);
        assert!(resolve_branch_oid(root, "cleanup-candidate")
            .unwrap()
            .is_some());
    }

    struct DispatchCleanupFixture {
        _tmp: tempfile::TempDir,
        root: PathBuf,
        worktree: PathBuf,
        branch: String,
        last: PathBuf,
        stdout: PathBuf,
    }

    fn dispatch_cleanup_fixture(slug: &str) -> DispatchCleanupFixture {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().to_path_buf();
        for args in [
            vec!["init", "-q"],
            vec!["config", "user.name", "Test"],
            vec!["config", "user.email", "test@example.com"],
        ] {
            assert!(Command::new("git")
                .args(args)
                .current_dir(&root)
                .status()
                .unwrap()
                .success());
        }
        std::fs::write(root.join("base.txt"), "base\n").unwrap();
        for args in [vec!["add", "base.txt"], vec!["commit", "-qm", "init"]] {
            assert!(Command::new("git")
                .args(args)
                .current_dir(&root)
                .status()
                .unwrap()
                .success());
        }
        let branch = format!("{slug}-impl");
        let dispatch_dir = root.join(".orgasmic/tmp/dispatch").join(slug);
        let worktree = dispatch_dir.join("worktree");
        std::fs::create_dir_all(worktree.parent().unwrap()).unwrap();
        assert!(Command::new("git")
            .args(["worktree", "add", "-q", "-b", &branch])
            .arg(&worktree)
            .current_dir(&root)
            .status()
            .unwrap()
            .success());
        let brief = dispatch_dir.join(format!("{slug}-impl-brief.md"));
        let (_, last, stdout) =
            dispatch_artifact_paths_for_attempt(&root, &brief, "aaaaaaaa11111111bbbbbbbb22222222");
        std::fs::create_dir_all(last.parent().unwrap()).unwrap();
        std::fs::write(&last, "summary\n").unwrap();
        std::fs::write(&stdout, "output\n").unwrap();
        DispatchCleanupFixture {
            _tmp: tmp,
            root,
            worktree,
            branch,
            last,
            stdout,
        }
    }

    fn dispatch_cleanup_record(fixture: &DispatchCleanupFixture, task: &str) -> DispatchRecord {
        let mut open = architector_record();
        open.tasks = vec![task.to_string()];
        open.kind = "implementer".to_string();
        open.worktree = Some(fixture.worktree.clone());
        open.branch = Some(fixture.branch.clone());
        open.last_path = Some(fixture.last.clone());
        open.stdout_path = Some(fixture.stdout.clone());
        open
    }

    fn resolve_ref_oid(root: &Path, ref_name: &str) -> String {
        let output = Command::new("git")
            .args(["rev-parse", "--verify", ref_name])
            .current_dir(root)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "resolve {ref_name}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        String::from_utf8_lossy(&output.stdout).trim().to_string()
    }

    #[test]
    fn dispatch_close_preserves_dirty_worktree_output_on_named_salvage_ref() {
        let fixture = dispatch_cleanup_fixture("task-salvage");
        std::fs::write(
            fixture.worktree.join("worker-output.txt"),
            "unfinished work\n",
        )
        .unwrap();
        let open = dispatch_cleanup_record(&fixture, "TASK-SALVAGE");

        let cleanup = cleanup_dispatch(&fixture.root, &open, true, false);
        assert_eq!(cleanup.status, CleanupStatus::Ok, "{:?}", cleanup.error);
        let salvage = cleanup.salvage.as_ref().expect("dirty worktree salvaged");
        assert_eq!(salvage.file_count, 1);
        assert!(salvage.worktree_removed);
        assert!(!fixture.worktree.exists());
        let recovered = Command::new("git")
            .args(["show", &format!("{}:worker-output.txt", salvage.ref_name)])
            .current_dir(&fixture.root)
            .output()
            .unwrap();
        assert!(
            recovered.status.success(),
            "dirty output was not recoverable from {}: {}",
            salvage.ref_name,
            String::from_utf8_lossy(&recovered.stderr)
        );
        assert_eq!(recovered.stdout, b"unfinished work\n");

        let subject = Command::new("git")
            .args(["show", "-s", "--format=%s", &salvage.sha])
            .current_dir(&fixture.root)
            .output()
            .unwrap();
        assert!(subject.status.success());
        assert_eq!(
            String::from_utf8_lossy(&subject.stdout).trim(),
            "TASK-SALVAGE: manager-salvaged uncommitted worker output"
        );

        let close = close_aborted_request(
            "orgasmic",
            &open,
            "TASK-SALVAGE",
            "worker interrupted",
            &cleanup,
            None,
        );
        assert_eq!(
            close
                .extra
                .iter()
                .find(|(key, _)| key == "SALVAGE_SHA")
                .map(|(_, value)| value.as_str()),
            Some(salvage.sha.as_str())
        );
        assert_eq!(
            close
                .extra
                .iter()
                .find(|(key, _)| key == "SALVAGE_REF")
                .map(|(_, value)| value.as_str()),
            Some(salvage.ref_name.as_str())
        );
        assert_eq!(
            close
                .extra
                .iter()
                .find(|(key, _)| key == "SALVAGE_FILE_COUNT")
                .map(|(_, value)| value.as_str()),
            Some("1")
        );
    }

    #[test]
    fn dispatch_close_clean_worktree_has_no_salvage_side_effects() {
        let fixture = dispatch_cleanup_fixture("task-clean");
        let before = resolve_branch_oid(&fixture.root, &fixture.branch).unwrap();
        let open = dispatch_cleanup_record(&fixture, "TASK-CLEAN");

        let cleanup = cleanup_dispatch(&fixture.root, &open, true, false);
        assert_eq!(cleanup.status, CleanupStatus::Ok, "{:?}", cleanup.error);
        assert_eq!(cleanup.salvage, None);
        assert!(!fixture.worktree.exists());
        assert_eq!(
            resolve_branch_oid(&fixture.root, &fixture.branch).unwrap(),
            before
        );

        let close = close_aborted_request(
            "orgasmic",
            &open,
            "TASK-CLEAN",
            "clean close",
            &cleanup,
            None,
        );
        assert!(!close
            .extra
            .iter()
            .any(|(key, _)| key.starts_with("SALVAGE_")));
    }

    #[test]
    fn dispatch_close_salvage_is_anchored_from_detached_or_switched_head() {
        for (slug, switch_to_branch) in [
            ("task-salvage-detached", false),
            ("task-salvage-switched", true),
        ] {
            let fixture = dispatch_cleanup_fixture(slug);
            let recorded_oid = resolve_branch_oid(&fixture.root, &fixture.branch)
                .unwrap()
                .unwrap();
            let unrelated = format!("{slug}-unrelated");
            let mut checkout = Command::new("git");
            checkout.current_dir(&fixture.worktree);
            if switch_to_branch {
                checkout.args(["checkout", "-qb", &unrelated]);
            } else {
                checkout.args(["checkout", "--detach"]);
            }
            let output = checkout.output().unwrap();
            assert!(
                output.status.success(),
                "{slug} checkout: {}{}",
                String::from_utf8_lossy(&output.stderr),
                String::from_utf8_lossy(&output.stdout)
            );
            std::fs::write(
                fixture.worktree.join("worker-output.txt"),
                "unfinished work\n",
            )
            .unwrap();
            let open = dispatch_cleanup_record(&fixture, "TASK-SALVAGE-IDENTITY");

            let cleanup = cleanup_dispatch(&fixture.root, &open, true, false);
            assert_eq!(cleanup.status, CleanupStatus::Ok, "{:?}", cleanup.error);
            let salvage = cleanup.salvage.expect("dirty worktree salvaged");
            assert_eq!(
                resolve_ref_oid(&fixture.root, &salvage.ref_name),
                salvage.sha
            );
            assert_eq!(
                resolve_ref_oid(&fixture.root, &format!("{}^", salvage.sha)),
                recorded_oid,
                "salvage parent must be the recorded dispatch branch"
            );
            assert_eq!(
                resolve_branch_oid(&fixture.root, &fixture.branch).unwrap(),
                Some(recorded_oid.clone()),
                "salvage must not move the recorded dispatch branch"
            );
            if switch_to_branch {
                assert_eq!(
                    resolve_branch_oid(&fixture.root, &unrelated).unwrap(),
                    Some(recorded_oid.clone()),
                    "salvage must not commit onto an unrelated checked-out branch"
                );
            }
        }
    }

    #[test]
    fn dispatch_close_dirty_branch_delete_keeps_nested_files_on_salvage_ref() {
        let fixture = dispatch_cleanup_fixture("task-salvage-delete");
        std::fs::create_dir_all(fixture.worktree.join("nested/deeper")).unwrap();
        std::fs::write(fixture.worktree.join("nested/one.txt"), "one\n").unwrap();
        std::fs::write(fixture.worktree.join("nested/deeper/two.txt"), "two\n").unwrap();
        let open = dispatch_cleanup_record(&fixture, "TASK-SALVAGE-DELETE");

        let cleanup = cleanup_dispatch(&fixture.root, &open, true, true);
        assert_eq!(cleanup.status, CleanupStatus::Ok, "{:?}", cleanup.error);
        let salvage = cleanup.salvage.expect("dirty worktree salvaged");
        assert_eq!(salvage.file_count, 2);
        assert_eq!(
            resolve_ref_oid(&fixture.root, &salvage.ref_name),
            salvage.sha
        );
        assert_eq!(
            resolve_branch_oid(&fixture.root, &fixture.branch).unwrap(),
            None,
            "--branch-delete should delete only the fenced dispatch branch"
        );
        for path in ["nested/one.txt", "nested/deeper/two.txt"] {
            let output = Command::new("git")
                .args(["show", &format!("{}:{path}", salvage.ref_name)])
                .current_dir(&fixture.root)
                .output()
                .unwrap();
            assert!(output.status.success(), "recover {path}");
        }
    }

    #[test]
    fn dispatch_close_late_writer_blocks_non_force_removal_after_salvage() {
        let fixture = dispatch_cleanup_fixture("task-salvage-late-writer");
        std::fs::write(
            fixture.worktree.join("worker-output.txt"),
            "unfinished work\n",
        )
        .unwrap();
        let expected_oid = resolve_branch_oid(&fixture.root, &fixture.branch)
            .unwrap()
            .unwrap();

        let removal = remove_worktree_required_with_hook(
            &fixture.root,
            &fixture.worktree,
            Some(&fixture.last),
            Some(&fixture.stdout),
            "TASK-SALVAGE-LATE-WRITER",
            Some(&fixture.branch),
            Some(&expected_oid),
            |path| std::fs::write(path.join("late-writer.txt"), "late\n").unwrap(),
        )
        .unwrap();

        assert!(!removal.removed);
        assert!(removal.error.is_some());
        let salvage = removal.salvage.expect("initial dirty output was salvaged");
        assert_eq!(
            resolve_ref_oid(&fixture.root, &salvage.ref_name),
            salvage.sha
        );
        assert!(fixture.worktree.exists());
        assert_eq!(
            std::fs::read_to_string(fixture.worktree.join("late-writer.txt")).unwrap(),
            "late\n"
        );
    }

    #[test]
    fn concurrent_dispatch_closes_cannot_delete_the_salvage_ref() {
        let fixture = dispatch_cleanup_fixture("task-salvage-concurrent");
        std::fs::write(
            fixture.worktree.join("worker-output.txt"),
            "unfinished work\n",
        )
        .unwrap();
        let open = dispatch_cleanup_record(&fixture, "TASK-SALVAGE-CONCURRENT");
        let barrier = std::sync::Barrier::new(2);

        let outcomes = std::thread::scope(|scope| {
            let first = scope.spawn(|| {
                barrier.wait();
                cleanup_dispatch(&fixture.root, &open, true, true)
            });
            let second = scope.spawn(|| {
                barrier.wait();
                cleanup_dispatch(&fixture.root, &open, true, true)
            });
            vec![first.join().unwrap(), second.join().unwrap()]
        });

        let salvages = outcomes
            .iter()
            .filter_map(|outcome| outcome.salvage.as_ref())
            .collect::<Vec<_>>();
        assert_eq!(salvages.len(), 1, "{outcomes:?}");
        assert_eq!(
            resolve_ref_oid(&fixture.root, &salvages[0].ref_name),
            salvages[0].sha
        );
        assert_eq!(
            resolve_branch_oid(&fixture.root, &fixture.branch).unwrap(),
            None
        );
        assert!(
            outcomes
                .iter()
                .any(|outcome| outcome.status == CleanupStatus::WorktreeFailed),
            "the losing close must fail closed after the worktree disappears"
        );
    }

    #[test]
    fn dispatch_close_does_not_remove_worktree_when_salvage_commit_fails() {
        let fixture = dispatch_cleanup_fixture("task-failed-salvage");
        std::fs::write(
            fixture.worktree.join("worker-output.txt"),
            "unfinished work\n",
        )
        .unwrap();
        assert!(Command::new("git")
            .args(["config", "user.name", ""])
            .current_dir(&fixture.root)
            .status()
            .unwrap()
            .success());
        let open = dispatch_cleanup_record(&fixture, "TASK-FAILED-SALVAGE");

        let cleanup = cleanup_dispatch(&fixture.root, &open, true, false);
        assert_eq!(cleanup.status, CleanupStatus::WorktreeFailed);
        assert!(cleanup.salvage.is_none());
        assert!(cleanup
            .error
            .as_deref()
            .is_some_and(|error| error.contains("git commit-tree failed")));
        assert!(fixture.worktree.exists());
        assert_eq!(
            std::fs::read_to_string(fixture.worktree.join("worker-output.txt")).unwrap(),
            "unfinished work\n"
        );
        assert!(worktree_has_uncommitted_changes(&fixture.worktree).unwrap());
    }
}
