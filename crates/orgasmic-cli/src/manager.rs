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

use anyhow::{bail, Context, Result};
use clap::{ArgAction, Args, ValueEnum};
use orgasmic_core::{
    dotorg_tasks_dir, goal_file_path, iter_task_file_paths, parse_tx_file, project_dispatch_dir,
    projects, LifecycleStage, OrgFile, ProjectFile, TaskHeading, TxEntry,
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
    --brief /path/to/brief.md --worker implementer-cursor

  orgasmic manager dispatch --task TASK-053 --kind implementer \\
    --brief /path/to/brief.md --dry-run")]
pub struct DispatchArgs {
    #[arg(long = "task", action = ArgAction::Append, required = true)]
    pub task: Vec<String>,
    #[arg(long, value_enum)]
    pub kind: DispatchKind,
    #[arg(long)]
    pub brief: PathBuf,
    #[arg(long = "from")]
    pub from: Option<String>,
    #[arg(long)]
    pub model: Option<String>,
    #[arg(long)]
    pub effort: Option<String>,
    #[arg(long)]
    pub worktree: Option<PathBuf>,
    #[arg(long)]
    pub branch: Option<String>,
    #[arg(long)]
    pub reason: Option<String>,
    #[arg(long)]
    pub dry_run: bool,
    #[arg(long)]
    pub worker: Option<String>,
}

#[derive(Copy, Clone, Debug, Eq, PartialEq, ValueEnum)]
pub enum DispatchCloseStatus {
    Done,
    Aborted,
}

#[derive(Args, Debug, Clone)]
pub struct DispatchCloseArgs {
    #[arg(long = "task", action = ArgAction::Append, required = true)]
    pub task: Vec<String>,
    #[arg(long, value_enum)]
    pub status: DispatchCloseStatus,
    #[arg(long = "merge-sha")]
    pub merge_sha: Option<String>,
    #[arg(long = "worker-commit", alias = "codex-commit")]
    pub worker_commit: Option<String>,
    #[arg(long = "worker-session", alias = "codex-session")]
    pub worker_session: Option<String>,
    #[arg(long = "reviewed-diff")]
    pub reviewed_diff: Option<String>,
    #[arg(long = "property", value_parser = parse_close_property)]
    pub properties: Vec<(String, String)>,
    #[arg(long)]
    pub tokens: Option<u64>,
    #[arg(long)]
    pub wall: Option<String>,
    #[arg(long)]
    pub reason: Option<String>,
    #[arg(long = "worktree-remove", default_value_t = true)]
    pub worktree_remove: bool,
    #[arg(long = "no-worktree-remove")]
    pub no_worktree_remove: bool,
    #[arg(long = "branch-delete", default_value_t = false)]
    pub branch_delete: bool,
}

#[derive(Args, Debug, Clone)]
pub struct DispatchStatusArgs {
    #[arg(long)]
    pub task: Option<String>,
    #[arg(long = "orphans-only")]
    pub orphans_only: bool,
    #[arg(long = "cleanup-failed")]
    pub cleanup_failed: bool,
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

/// Worker-driven dispatch finalization (dec_3M7M0): the terminal action a
/// dispatched worker takes instead of relying on the daemon to infer
/// completion from an EOT marker + scrollback scrape. In one daemon call it
/// commits the worktree (`--commit`), writes `last.txt` verbatim from
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
    pub(crate) brief_path: PathBuf,
    pub(crate) brief_content: String,
    pub(crate) from_sha: String,
    pub(crate) worktree_path: PathBuf,
    pub(crate) branch: String,
    pub(crate) model_override: Option<String>,
    pub(crate) effort_override: Option<String>,
    pub(crate) last_path: PathBuf,
    pub(crate) stdout_path: PathBuf,
    pub(crate) goal_id: Option<String>,
    pub(crate) reason: Option<String>,
    pub(crate) dry_run: bool,
    pub(crate) worker_override: Option<String>,
}

impl DispatchPlan {
    pub(crate) fn dispatch_task(&self) -> String {
        task_list_property(&self.tasks)
    }
}

#[derive(Debug, Serialize)]
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
}

#[derive(Debug, Deserialize)]
struct RunReleaseResponse {
    #[allow(dead_code)]
    run_id: String,
}

#[derive(Debug, Default, Deserialize)]
struct RunsResponse {
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
    run_id: Option<String>,
    worker_id: Option<String>,
    driver: Option<String>,
    harness: Option<String>,
    pid: Option<u32>,
    started_at: Option<String>,
    worker_pid: Option<u32>,
    goal_id: Option<String>,
    closed_tasks: BTreeSet<String>,
    cleanup_already_run: bool,
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
        print_dispatch_plan(&plan);
        return Ok(());
    }

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
        let cleanup =
            cleanup_created_resources(&plan.project_root, &plan.worktree_path, &plan.branch);
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
            let reason = format!("daemon dispatch failed: {err}");
            let cleanup =
                if crate::daemon_client::DaemonClient::dispatch_failure_needs_daemon_cleanup(&err) {
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
                        },
                    }
                } else {
                    cleanup_created_resources(&plan.project_root, &plan.worktree_path, &plan.branch)
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

    println!(
        "dispatched: {} {} pid={} run_id={} worker={} driver={} harness={} brief={}",
        task_list_property(&plan.tasks),
        plan.kind,
        response.pid,
        response.run_id,
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
    let project_root = find_project_root()?;
    let project_id = read_project_id(&project_root)?;
    let tasks = normalize_tasks(args.task.clone())?;
    let open = latest_open_dispatch_for_tasks(&project_root, &tasks)?.ok_or_else(|| {
        anyhow::anyhow!(
            "no open manager.dispatch_started tx for {}",
            task_list_property(&tasks)
        )
    })?;
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
    // A failed liveness probe must fail the close: treating the error as
    // "no live runs" would skip the release and prune worktree artifacts
    // under a possibly still-live worker.
    let live_runs = runtime
        .block_on(fetch_live_runs(&client))
        .context("liveness check before dispatch-close cleanup")?;
    if let Some(run_id) = open.run_id.as_deref() {
        if live_runs.iter().any(|run| run.run_id == run_id) {
            runtime.block_on(release_dispatch_run(
                &client,
                run_id,
                &task_list_property(&tasks),
            ))?;
        }
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
        }
    } else {
        let remove_worktree = args.worktree_remove && !args.no_worktree_remove;
        cleanup_dispatch(&project_root, &open, remove_worktree, args.branch_delete)
    };
    if cleanup_status_reports_warning(cleanup.status) {
        eprintln!(
            "warning: dispatch cleanup status={} error={}",
            cleanup.status.as_str(),
            cleanup.error.as_deref().unwrap_or("-")
        );
    }
    let mut responses = Vec::new();
    match args.status {
        DispatchCloseStatus::Done => {
            for task in &missing_close_tasks {
                let request = close_done_request(
                    &project_id,
                    &open,
                    task,
                    &args,
                    merge_sha.as_deref(),
                    tx_type,
                    &cleanup,
                );
                responses.push(
                    runtime.block_on(client.post_json::<_, TxAppendResponse>("/tx", &request))?,
                );
            }
        }
        DispatchCloseStatus::Aborted => {
            let reason = abort_reason.as_deref().expect("validated aborted reason");
            for task in &missing_close_tasks {
                let request = close_aborted_request(&project_id, &open, task, reason, &cleanup);
                responses.push(
                    runtime.block_on(client.post_json::<_, TxAppendResponse>("/tx", &request))?,
                );
            }
        }
    };

    let transitions = close_lifecycle_transitions(&project_root, &tasks, &open, &args)?;
    if let Err(err) = apply_task_lifecycle_transitions(&client, &project_id, &transitions) {
        eprintln!("warning: close tx appended but lifecycle update failed: {err}");
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

#[derive(Debug, Clone, Deserialize)]
struct LiveRunInfo {
    run_id: String,
    task_id: String,
    kind: String,
    #[serde(default)]
    project_id: Option<String>,
    #[serde(default)]
    last_path: Option<PathBuf>,
}

#[derive(Debug, Default, Deserialize)]
struct LiveRunsResponse {
    #[serde(default)]
    live: Vec<LiveRunInfo>,
}

/// The worker-driven counterpart to `dispatch-close` (dec_3M7M0): a
/// dispatched worker calls this as its terminal action instead of relying on
/// the daemon to infer completion from an EOT marker + scrollback scrape. In
/// one daemon call it optionally commits the worktree, writes `last.txt`
/// verbatim from `--summary-file`, emits the terminal tx
/// (`implementer.done`/`reviewer.done`/`manager.dispatch_aborted`), and
/// releases the lease — converging on the same `release_dispatch_run`/`/tx`
/// plumbing `dispatch-close` uses.
pub fn cmd_dispatch_finalize(home: &Home, args: DispatchFinalizeArgs) -> Result<()> {
    let project_root = find_project_root()?;
    let project_id = read_project_id(&project_root)?;
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

    let task = resolve_finalize_task(&project_root, args.task.clone())?;

    let client = DaemonClient::from_home_autostart(home)?;
    let runtime = tokio::runtime::Runtime::new().context("create tokio runtime")?;
    let run = runtime.block_on(resolve_finalize_run(
        &client,
        &project_id,
        &task,
        args.run_id.clone(),
    ))?;

    let sha = if args.commit {
        Some(commit_worktree(
            &project_root,
            &finalize_commit_message(&task, args.status, &summary),
        )?)
    } else {
        args.sha.clone()
    };

    let tx_type = match args.status {
        FinalizeStatus::Done => done_tx_type_for_kind(&run.kind)?,
        FinalizeStatus::Blocked => "manager.dispatch_aborted",
    };

    let mut extra = vec![
        ("RUN_ID".to_string(), run.run_id.clone()),
        ("WORKTREE".to_string(), project_root.display().to_string()),
    ];
    if let Some(sha) = sha.as_deref() {
        extra.push(("SHA".to_string(), sha.to_string()));
        if matches!(tx_type, "implementer.done" | "architector.done") {
            extra.push(("MERGE_SHA".to_string(), sha.to_string()));
        }
    }

    // Order matters (reviewer #2): the terminal `*.done` tx is the LAST thing
    // we emit, only after the durable artifacts (commit above, last.txt) and
    // the lease release have all succeeded. If any earlier step fails or the
    // process dies, no `done` tx is on record — the run stalls and is flagged
    // orphan (rescuable), never a "done" claim with no report + a held lease.

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

    // 2. Release the lease, marked finalized_by_worker so the completion
    //    watcher suppresses its fallback scrape.
    runtime.block_on(release_dispatch_run_with_reason(
        &client,
        &run.run_id,
        &format!("worker finalize for {task}"),
        &task,
        true,
    ))?;

    // 3. Emit the terminal tx last.
    let tx_request = TxAppendRequest {
        request_id: Some(format!(
            "dispatch-finalize-{}-{}",
            request_slug(&task),
            Uuid::new_v4()
        )),
        ty: tx_type.to_string(),
        actor: Some(format!("agent.{}", run.kind)),
        machine: None,
        project: Some(project_id.clone()),
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
    let tx_response: TxAppendResponse = runtime.block_on(client.post_json("/tx", &tx_request))?;

    println!(
        "finalized: {} {} tx={} run={} last={}",
        task,
        tx_type,
        tx_response.tx_id,
        run.run_id,
        last_path.display()
    );
    Ok(())
}

fn finalize_commit_message(task: &str, status: FinalizeStatus, summary: &str) -> String {
    let subject: String = summary
        .lines()
        .map(str::trim)
        .find(|line| !line.is_empty())
        .unwrap_or("worker finalize")
        .chars()
        .take(72)
        .collect();
    format!(
        "{task}: {subject}\n\norgasmic dispatch finalize --status {}",
        status.as_str()
    )
}

fn worktree_has_uncommitted_changes(project_root: &Path) -> bool {
    Command::new("git")
        .args(["status", "--porcelain"])
        .current_dir(project_root)
        .output()
        .map(|output| output.status.success() && !output.stdout.is_empty())
        .unwrap_or(false)
}

/// Commit the worktree if dirty (so commit-stall is structurally impossible,
/// acceptance #2), then return the resulting HEAD sha either way.
fn commit_worktree(project_root: &Path, message: &str) -> Result<String> {
    if worktree_has_uncommitted_changes(project_root) {
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

/// Resolve the live run to finalize: an explicit `--run-id` is used as-is
/// (still fetched from `/runs` to recover its kind/last_path); otherwise the
/// single live run matching `task` (and `project_id`, when the daemon reports
/// one) is used. Deliberately does NOT fall back to scanning
/// `.orgasmic/tx`: a worker's own worktree checkout cannot see the live
/// (uncommitted) daemon writes to the manager's `.orgasmic/tx`, so the only
/// reliable source is the daemon's in-memory live run list.
async fn resolve_finalize_run(
    client: &DaemonClient,
    project_id: &str,
    task: &str,
    run_id: Option<String>,
) -> Result<LiveRunInfo> {
    let live = client.get::<LiveRunsResponse>("/runs").await?.live;
    if let Some(run_id) = run_id {
        let run = live
            .into_iter()
            .find(|run| run.run_id == run_id)
            .ok_or_else(|| anyhow::anyhow!("no live run {run_id}; already released?"))?;
        // Guard against finalizing a run that belongs to a different task or
        // project than the one this invocation resolved (cross-run confusion /
        // forge vector): the tx is stamped with `task`, so the target run's
        // own task/project must agree before we write its last.txt + release
        // its lease.
        if run.task_id != task {
            bail!(
                "run {} belongs to task {}, not {task}; refusing to finalize",
                run.run_id,
                run.task_id
            );
        }
        if let Some(run_project) = run.project_id.as_deref() {
            if run_project != project_id {
                bail!(
                    "run {} belongs to project {run_project}, not {project_id}; refusing to finalize",
                    run.run_id
                );
            }
        }
        return Ok(run);
    }
    let mut matches: Vec<LiveRunInfo> = live
        .into_iter()
        .filter(|run| {
            run.task_id == task
                && run
                    .project_id
                    .as_deref()
                    .map(|p| p == project_id)
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
    let project_root = find_project_root()?;
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
            "TX_ID={} TASK={} KIND={} STARTED_AT={} WORKTREE={} WORKER_PID={} RUN_ID={} WORKER={} DRIVER={} HARNESS={} {} {} {}{}",
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
        bail!(
            "dispatch already open for overlapping task(s) {} in {} (tx {})",
            task_list_property(&overlapping),
            task_list_property(&open.tasks),
            open.tx_id
        );
    }
    for task in &tasks {
        validate_task_dispatchable(&project_root, task, args.kind)?;
    }
    let brief_path = canonical_existing_file(&args.brief)?;
    let brief_content = std::fs::read_to_string(&brief_path)
        .with_context(|| format!("read brief {}", brief_path.display()))?;
    let from_ref = args.from.as_deref().unwrap_or("HEAD");
    let from_sha = resolve_commit(&project_root, from_ref)?;
    let worktree_path = normalize_path(&match args.worktree {
        Some(path) => absolutize(&path)?,
        None => default_worktree(&project_root, first_task(&tasks), args.kind),
    });
    let branch = args
        .branch
        .unwrap_or_else(|| default_branch(first_task(&tasks), args.kind));
    let (brief_path, last_path, stdout_path) = dispatch_artifact_paths(&project_root, &brief_path);
    let goal_id = read_active_goal_id(&project_root)?;
    Ok(DispatchPlan {
        project_root,
        project_id,
        tasks,
        kind: args.kind,
        brief_path,
        brief_content,
        from_sha,
        worktree_path,
        branch,
        model_override: args
            .model
            .map(|s| sanitize_tx_value(&s))
            .filter(|s| !s.is_empty()),
        effort_override: args
            .effort
            .map(|s| sanitize_tx_value(&s))
            .filter(|s| !s.is_empty()),
        last_path,
        stdout_path,
        goal_id,
        reason: args
            .reason
            .map(|s| sanitize_tx_value(&s))
            .filter(|s| !s.is_empty()),
        dry_run: args.dry_run,
        worker_override: args
            .worker
            .map(|s| sanitize_tx_value(&s))
            .filter(|s| !s.is_empty()),
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
    println!(
        "  worker:   {}{}",
        plan.worker_override.as_deref().unwrap_or("pipeline worker"),
        plan.model_override
            .as_deref()
            .map(|model| format!(" --model {model}"))
            .unwrap_or_default(),
    );
    if plan.effort_override.is_some() {
        println!(
            "  effort:   {}",
            plan.effort_override.as_deref().unwrap_or("-")
        );
    }
}

async fn fetch_live_runs(client: &DaemonClient) -> Result<Vec<RunSummary>> {
    Ok(client.get::<RunsResponse>("/runs").await?.live)
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
    )
    .await
}

/// Shared release call for both `dispatch-close` (manager authority) and
/// `dispatch finalize` (worker authority, dec_3M7M0) — same terminal
/// endpoint, differing only in reason text and `finalized_by_worker`.
async fn release_dispatch_run_with_reason(
    client: &DaemonClient,
    run_id: &str,
    reason: &str,
    request_slug_source: &str,
    finalized_by_worker: bool,
) -> Result<RunReleaseResponse> {
    let request = RunReleaseRequest {
        reason: Some(reason.to_string()),
        request_id: Some(format!(
            "dispatch-release-{}-{}",
            request_slug(request_slug_source),
            Uuid::new_v4()
        )),
        finalized_by_worker,
    };
    client
        .post_json(&format!("/runs/{}/release", path_segment(run_id)), &request)
        .await
}

fn close_done_request(
    project_id: &str,
    open: &DispatchRecord,
    task: &str,
    args: &DispatchCloseArgs,
    merge_sha: Option<&str>,
    tx_type: &str,
    cleanup: &CleanupOutcome,
) -> TxAppendRequest {
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
    push_cleanup_extra(&mut extra, cleanup);
    if let Some(goal_id) = optional_value(open.goal_id.as_deref()) {
        extra.push(("GOAL_ID".to_string(), goal_id));
    }
    TxAppendRequest {
        request_id: Some(format!(
            "dispatch-close-{}-{}",
            request_slug(task),
            Uuid::new_v4()
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
) -> TxAppendRequest {
    let mut extra = vec![("CLOSED_TX".to_string(), open.tx_id.clone())];
    if let Some(worktree) = &open.worktree {
        extra.push(("WORKTREE".to_string(), worktree.display().to_string()));
    }
    push_cleanup_extra(&mut extra, cleanup);
    TxAppendRequest {
        request_id: Some(format!(
            "dispatch-aborted-{}-{}",
            request_slug(task),
            Uuid::new_v4()
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
    match kind {
        "implementer" => Ok("implementer.done"),
        "reviewer" => Ok("reviewer.done"),
        "architector" => Ok("architector.done"),
        other => bail!("cannot close dispatch kind `{other}` as done"),
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

fn remove_worktree_if_present(project_root: &Path, path: &Path) -> Result<()> {
    if !path.exists() {
        return Ok(());
    }
    remove_worktree_required(project_root, path)
}

fn remove_worktree_required(project_root: &Path, path: &Path) -> Result<()> {
    if !path.exists() {
        bail!("worktree path missing: {}", path.display());
    }
    let output = Command::new("git")
        .args(["worktree", "remove", "--force"])
        .arg(path)
        .current_dir(project_root)
        .output()
        .with_context(|| format!("git worktree remove {}", path.display()))?;
    if !output.status.success() {
        bail!(
            "git worktree remove failed: {}{}",
            String::from_utf8_lossy(&output.stderr),
            String::from_utf8_lossy(&output.stdout)
        );
    }
    orgasmic_core::prune_dispatch_stem_after_worktree(path);
    Ok(())
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
    })
}

fn cleanup_created_resources(project_root: &Path, path: &Path, branch: &str) -> CleanupOutcome {
    let mut worktree_failed = false;
    let mut branch_failed = false;
    let mut errors = Vec::new();

    match remove_worktree_if_present(project_root, path) {
        Ok(()) => {}
        Err(err) => {
            worktree_failed = true;
            errors.push(format!("worktree: {err}"));
        }
    }
    if let Err(err) = delete_branch(project_root, branch) {
        branch_failed = true;
        errors.push(format!("branch: {err}"));
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
    }
}

fn cleanup_dispatch(
    project_root: &Path,
    open: &DispatchRecord,
    remove_worktree: bool,
    branch_delete: bool,
) -> CleanupOutcome {
    let mut worktree_failed = false;
    let mut branch_failed = false;
    let mut worktree_missing = false;
    let mut errors = Vec::new();

    if remove_worktree {
        match &open.worktree {
            Some(worktree) => {
                if let Err(err) = remove_worktree_required(project_root, worktree) {
                    worktree_failed = true;
                    errors.push(format!("worktree: {err}"));
                }
            }
            None => {
                worktree_missing = true;
                errors.push("worktree: open dispatch has no WORKTREE property".to_string());
            }
        }
    }

    if branch_delete {
        match &open.branch {
            Some(branch) => {
                if let Err(err) = delete_branch(project_root, branch) {
                    branch_failed = true;
                    errors.push(format!("branch: {err}"));
                }
            }
            None => {
                branch_failed = true;
                errors.push("branch: open dispatch has no BRANCH property".to_string());
            }
        }
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
}

fn cleanup_status_reports_warning(status: CleanupStatus) -> bool {
    !matches!(status, CleanupStatus::Ok | CleanupStatus::CleanupAlreadyRun)
}

fn delete_branch(project_root: &Path, branch: &str) -> Result<()> {
    let output = Command::new("git")
        .args(["branch", "-D", branch])
        .current_dir(project_root)
        .output()
        .with_context(|| format!("git branch -D {branch}"))?;
    if !output.status.success() {
        bail!(
            "git branch -D failed: {}{}",
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

fn close_lifecycle_transitions(
    project_root: &Path,
    tasks: &[String],
    open: &DispatchRecord,
    args: &DispatchCloseArgs,
) -> Result<Vec<(String, LifecycleStage)>> {
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
        transitions.push((info.id, stage));
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

/// Resolve the (brief, last, stdout) artifact paths for a dispatch. All three
/// live together in a per-task subfolder under `.orgasmic/tmp/dispatch/<stem>/`
/// (the stem encodes task + kind, e.g. `task-129-impl`), keeping the prefixed
/// filenames so they stay self-describing when read in isolation.
fn dispatch_artifact_paths(project_root: &Path, brief_path: &Path) -> (PathBuf, PathBuf, PathBuf) {
    let (file_name, stem) = dispatch_artifact_stem(brief_path);
    let dir = project_dispatch_dir(project_root).join(&stem);
    (
        dir.join(file_name),
        dir.join(format!("{stem}-last.txt")),
        dir.join(format!("{stem}-stdout.log")),
    )
}

/// Derive the last/stdout paths as siblings of an already-resolved brief. Once
/// the brief lives in its per-task subfolder, the siblings land in the same
/// folder automatically.
fn dispatch_sibling_artifact_paths(brief_path: &Path) -> (PathBuf, PathBuf) {
    let parent = brief_path.parent().unwrap_or_else(|| Path::new("."));
    let (_, stem) = dispatch_artifact_stem(brief_path);
    (
        parent.join(format!("{stem}-last.txt")),
        parent.join(format!("{stem}-stdout.log")),
    )
}

fn materialize_dispatch_brief(plan: &DispatchPlan) -> Result<()> {
    if let Some(parent) = plan.brief_path.parent() {
        std::fs::create_dir_all(parent).with_context(|| format!("create {}", parent.display()))?;
    }
    std::fs::write(&plan.brief_path, &plan.brief_content)
        .with_context(|| format!("write {}", plan.brief_path.display()))
}

fn latest_open_dispatch_for_tasks(
    project_root: &Path,
    tasks: &[String],
) -> Result<Option<DispatchRecord>> {
    let open = scan_open_dispatches(project_root)?;
    Ok(open.into_iter().rev().find(|record| {
        tasks
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

fn scan_open_dispatches(project_root: &Path) -> Result<Vec<DispatchRecord>> {
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
            "implementer.done"
            | "reviewer.done"
            | "architector.done"
            | "manager.dispatch_aborted" => close_matching_dispatch(&mut open, &entry),
            _ => {}
        }
    }
    Ok(open.into_iter().filter(|record| !record.closed).collect())
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
        run_id: None,
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
        closed: false,
    })
}

fn attach_run_created_to_dispatch(open: &mut [DispatchRecord], entry: &TxEntry) {
    if extra(entry, "ORIGIN") != Some("cli_dispatch") {
        return;
    }
    let dispatch_tx = extra(entry, "DISPATCH_TX");
    let run_id = extra(entry, "RUN_ID").map(str::to_string);
    let worker_id = extra(entry, "WORKER").map(str::to_string);
    let driver = extra(entry, "DRIVER").map(str::to_string);
    let harness = extra(entry, "HARNESS").map(str::to_string);
    let pid = extra(entry, "PID").and_then(|pid| pid.parse::<u32>().ok());
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
            record.run_id = run_id;
            record.worker_id = worker_id;
            record.driver = driver;
            record.harness = harness;
            record.pid = pid;
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
    let (last_path, _) = dispatch_sibling_artifact_paths(brief_path);
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
            run_id: None,
            worker_id: None,
            driver: None,
            harness: None,
            pid: None,
            started_at: None,
            worker_pid: None,
            goal_id: None,
            closed_tasks: BTreeSet::new(),
            cleanup_already_run: false,
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
            vec![("TASK-086".to_string(), LifecycleStage::Done)]
        );
    }

    #[test]
    fn places_dispatch_artifacts_under_per_task_subfolder() {
        let project_root = PathBuf::from("/repo/main");
        let brief = PathBuf::from("/elsewhere/task-045-impl-brief.md");
        let (resolved_brief, last, stdout) = dispatch_artifact_paths(&project_root, &brief);
        assert_eq!(
            resolved_brief,
            PathBuf::from("/repo/main/.orgasmic/tmp/dispatch/task-045-impl/task-045-impl-brief.md")
        );
        assert_eq!(
            last,
            PathBuf::from("/repo/main/.orgasmic/tmp/dispatch/task-045-impl/task-045-impl-last.txt")
        );
        assert_eq!(
            stdout,
            PathBuf::from(
                "/repo/main/.orgasmic/tmp/dispatch/task-045-impl/task-045-impl-stdout.log"
            )
        );
        // last/stdout derived as siblings of the resolved brief land in the
        // same per-task subfolder.
        let (sib_last, sib_stdout) = dispatch_sibling_artifact_paths(&resolved_brief);
        assert_eq!(sib_last, last);
        assert_eq!(sib_stdout, stdout);
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
    }
}
