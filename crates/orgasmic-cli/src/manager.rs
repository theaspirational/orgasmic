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
use std::time::{Duration, Instant};

use anyhow::{bail, Context, Result};
use clap::{ArgAction, Args, ValueEnum};
use orgasmic_core::{
    fold_dispatches, goal_file_path, parse_tx_file, project_dispatch_dir, projects, read_claims,
    read_session_file, task_node_file_path, DispatchFold, Lifecycle, LifecycleStage, OrgFile,
    ProjectFile, RuntimeIdentity, SessionEventKind, TaskHeading, TxEntry,
};
// orgasmic:task_ZKZBF.2 — the ONE key-shape rule (this used to be a verbatim
// copy of core's; a copy drifting is how the drawer check and the ledger
// writer came to disagree on `FOO-BAR`).
use orgasmic_core::tx::is_uppercase_snake_key;
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::daemon_client::DaemonClient;
use crate::home::Home;
use crate::sequencer_markers::SEQUENCER_MARKERS;

/// The kinds `orgasmic manager dispatch --kind` will start.
///
/// dec_HBK6A retired `architector`, so it is no longer a value this enum
/// accepts and no dispatch can start one. The READ side is deliberately not
/// this enum: a `DispatchRecord`'s `kind` is a plain `String`, and every close
/// / finalize / scan path below matches it as a string precisely so a persisted
/// `architector` row keeps parsing after the verb is gone.
#[derive(Copy, Clone, Debug, Eq, PartialEq, ValueEnum)]
pub enum DispatchKind {
    Implementer,
    Reviewer,
}

impl DispatchKind {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::Implementer => "implementer",
            Self::Reviewer => "reviewer",
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
    --brief /path/to/brief.md --mode stdio --harness cursor-agent

  orgasmic manager dispatch --task TASK-053 --kind implementer \\
    --brief /path/to/brief.md --mode tmux --harness custom \\
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
    /// — not a path. Omitted → HEAD of the checkout you dispatch from
    /// (refused if that is the `orgasmic` ledger branch).
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
    /// Create a new checkout instead of reusing a closed implementer round's
    /// worktree. When the managed default still exists, pair this with
    /// `--worktree <new-path>`.
    #[arg(long = "fresh-worktree")]
    pub fresh_worktree: bool,
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

// orgasmic:TASK-YN5FJ.1
/// The canonical verdict vocabulary a reviewer close records.
///
/// RULING 1 (TASK-YN5FJ.1): EVERY value here satisfies the default-branch
/// review gate, `Reject` included. The gate's question is "did an independent
/// review happen and say something", not "did the reviewer approve":
///
/// 1. Merge still precedes review in practice here, so a reject the manager
///    then resolves in a follow-up commit is a normal, good outcome — it is
///    what happened on TASK-XCJYC (`d54fba5` reviewed REJECT, fixed in
///    `dd494ab`).
/// 2. If a reject blocked the close, the only way out would be
///    `--no-review-required --reason`, which stamps `NO_REVIEW_REQUIRED=true`
///    on a dispatch that WAS reviewed. That is strictly worse evidence than
///    recording the reject.
/// 3. The consequence of a bad verdict already has a home: `reviewer_done_stage`
///    sends a non-clean verdict's task back to `in_progress`. Making the gate
///    also judge verdict content would give one value two jobs.
///
/// RULING 2: this set is a SUPERSET of the legacy free-text vocabulary rather
/// than a replacement. `clean`, `ship`, `has-issues` and anything else stay
/// reachable through `--property VERDICT=...` with their current behaviour;
/// `--verdict` deliberately does not accept them, so there is one canonical
/// surface and one documented compatibility path.
#[derive(Copy, Clone, Debug, Eq, PartialEq, ValueEnum)]
pub enum ReviewVerdict {
    #[value(name = "approve")]
    Approve,
    #[value(name = "approve-with-follow-ups")]
    ApproveWithFollowUps,
    #[value(name = "reject")]
    Reject,
}

impl ReviewVerdict {
    /// The string written to the close tx's `VERDICT` property. One storage
    /// key shared with the legacy `--property VERDICT=` spelling — the flag is
    /// a typed front door onto the same value, not a second property.
    fn as_str(self) -> &'static str {
        match self {
            ReviewVerdict::Approve => "approve",
            ReviewVerdict::ApproveWithFollowUps => "approve-with-follow-ups",
            ReviewVerdict::Reject => "reject",
        }
    }

    /// The value list the refusal and the conflict error print, so an operator
    /// never has to go looking for the vocabulary.
    fn value_list() -> String {
        [
            ReviewVerdict::Approve,
            ReviewVerdict::ApproveWithFollowUps,
            ReviewVerdict::Reject,
        ]
        .iter()
        .map(|verdict| verdict.as_str())
        .collect::<Vec<_>>()
        .join("|")
    }
}

#[derive(Copy, Clone, Debug, Eq, PartialEq)]
enum ManagerOwnedClosePropertyPolicy {
    Reserved,
    AliasedVerdict,
}

/// A close-tx key whose generic `--property` spelling earns a LOUD REFUSAL.
///
/// orgasmic:TASK-4WKNX.1.1 — this is deliberately NOT the manager-owned
/// namespace. Value precedence is settled by construction: `close_done_request`
/// accumulates every structured value in `manager_extra` and the caller's
/// properties can only join through `finish_close_tx_extras`, which appends
/// them last, so first-wins readers take the manager's value for any key,
/// listed or not. Entries here exist for the keys where being silently
/// overridden is not enough — the deliberate bypass flags, where forging the
/// spelling must be an error rather than a no-op.
///
/// The only alias is `VERDICT`: its property spelling predates the typed flag
/// and deliberately remains free-text, with the both-spellings conflict.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
struct ManagerOwnedCloseProperty {
    key: &'static str,
    typed_flag: &'static str,
    policy: ManagerOwnedClosePropertyPolicy,
}

impl ManagerOwnedCloseProperty {
    const fn reserved(key: &'static str, typed_flag: &'static str) -> Self {
        Self {
            key,
            typed_flag,
            policy: ManagerOwnedClosePropertyPolicy::Reserved,
        }
    }

    const fn aliased_verdict() -> Self {
        Self {
            key: "VERDICT",
            typed_flag: "--verdict",
            policy: ManagerOwnedClosePropertyPolicy::AliasedVerdict,
        }
    }
}

// orgasmic:TASK-4WKNX.1.1 — the loud-refusal list, NOT the manager-owned
// namespace. DO NOT add a structural key here to make the manager's value win:
// it already does, by construction, via `finish_close_tx_extras`. Adding one
// would re-create the enumeration dependency round 3 removed — a key protected
// only if someone remembered to list it. Add a key here only when its generic
// spelling must be REFUSED, which today means the deliberate bypass flags.
const MANAGER_OWNED_CLOSE_PROPERTIES: &[ManagerOwnedCloseProperty] = &[
    ManagerOwnedCloseProperty::reserved("FIX_ROUND_FINAL", "--fix-round-final"),
    ManagerOwnedCloseProperty::reserved("NO_REVIEW_REQUIRED", "--no-review-required"),
    ManagerOwnedCloseProperty::aliased_verdict(),
];

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
    /// Manager-owned keys are reserved for their typed flags, except the
    /// supported legacy `VERDICT` alias documented below.
    #[arg(long = "property", value_parser = parse_close_property)]
    pub properties: Vec<(String, String)>,
    /// Reviewer verdict, recorded as `VERDICT` on the close tx; ANY verdict
    /// clears the default-branch review gate, `reject` included — the verdict
    /// steers the task's next stage, not the merge.
    ///
    /// Valid only on a close that records `reviewer.done`. Legacy free-text
    /// spellings (`clean`, `ship`, `has-issues`, …) stay reachable through
    /// `--property VERDICT=<value>`; passing both is an error.
    #[arg(long = "verdict", value_enum)]
    pub verdict: Option<ReviewVerdict>,
    /// Tokens the worker run consumed, recorded on the close tx.
    #[arg(long)]
    pub tokens: Option<u64>,
    /// Wall-clock duration of the worker run (free-form, e.g. `41m`).
    #[arg(long)]
    pub wall: Option<String>,
    /// Why the dispatch is being closed this way; recorded on the close tx.
    #[arg(long)]
    pub reason: Option<String>,
    /// Bypass the reviewer-verdict gate for an implementer merge. Requires
    /// --reason and records NO_REVIEW_REQUIRED=true on the close tx.
    #[arg(long = "no-review-required")]
    pub no_review_required: bool,
    /// Close a `:FIX_SUBTASK:` fix round straight to `done`, opting out of the
    /// review round it otherwise gets. Requires --reason and records
    /// FIX_ROUND_FINAL=true on the close tx.
    ///
    /// This is NOT `--no-review-required`, and the two are deliberately not
    /// one flag (TASK-4WKNX). That one waives the default-branch MERGE gate —
    /// "does a reviewer verdict exist for this merge". This one waives the fix
    /// round's own REVIEW ROUND — "does this fix get reviewed at all".
    #[arg(long = "fix-round-final")]
    pub fix_round_final: bool,
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
    /// Delete the worker's branch after its worktree is safely removed.
    /// Successful closes do this by default; aborted closes require this flag.
    /// Deletion is fenced to the recorded branch tip.
    #[arg(long = "branch-delete", conflicts_with = "no_branch_delete")]
    pub branch_delete: bool,
    /// Keep the worker's branch after a successful close.
    #[arg(long = "no-branch-delete", conflicts_with = "branch_delete")]
    pub no_branch_delete: bool,
}

fn dispatch_close_deletes_branch(args: &DispatchCloseArgs) -> bool {
    args.branch_delete
        || (args.status == DispatchCloseStatus::Done
            && args.worktree_remove
            && !args.no_worktree_remove
            && !args.no_branch_delete)
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
#[command(after_help = "\
Exit status: 0 all requested generations reported; 1 invalid/unknown generation, daemon/API, or transport error; 2 a generation died before reporting; 3 timeout while one remains waiting.")]
pub struct DispatchWaitArgs {
    /// Dispatch generation (`manager.dispatch_started` TX_ID) printed by
    /// `manager dispatch`. Repeat to wait for a whole round.
    #[arg(long = "started-tx", required = true, action = ArgAction::Append)]
    pub started_tx: Vec<String>,
    /// Maximum wait in `30s`, `5m`, or `1h` form. Omit to wait indefinitely.
    #[arg(long, value_parser = parse_wait_duration)]
    pub timeout: Option<Duration>,
}

fn parse_wait_duration(raw: &str) -> Result<Duration, String> {
    let raw = raw.trim();
    let (number, unit) = raw
        .chars()
        .last()
        .filter(|ch| matches!(ch, 's' | 'm' | 'h'))
        .map(|unit| (&raw[..raw.len() - unit.len_utf8()], unit))
        .ok_or_else(|| "timeout must end in s, m, or h".to_string())?;
    let number = number
        .parse::<u64>()
        .map_err(|_| "timeout must be a positive integer".to_string())?;
    if number == 0 {
        return Err("timeout must be greater than zero".into());
    }
    Ok(match unit {
        's' => Duration::from_secs(number),
        'm' => Duration::from_secs(number.saturating_mul(60)),
        'h' => Duration::from_secs(number.saturating_mul(3600)),
        _ => unreachable!(),
    })
}

/// Explicit reclamation of managed worktrees under
/// `<home>/worktrees/<project-id>/`. The ONLY surface that removes one outside
/// `dispatch-close`; detection of what it could reclaim runs automatically in
/// `dispatch-status`. Resolves the project from the cwd for the same reason
/// `dispatch-status` does (TASK-GQPGR).
// orgasmic:TASK-M47E5
#[derive(Args, Debug, Clone)]
#[command(after_help = "\
Refuses any worktree an OPEN dispatch names, whatever its run health: ending a
dispatch is `dispatch-close`'s job, and an abandoned one is reported with the
verb that releases it. Salvages a dirty tree to refs/orgasmic/salvage/<sha>
first, then removes it through this verb's OWN anchored directory handle rather
than by handing a pathname to `git worktree remove`. It refuses what a
non-forced `git worktree remove` refuses, checked here: a LOCKED worktree, one
containing an INITIALIZED SUBMODULE, and one still dirty after salvage. The
submodule refusal is CATEGORICAL and runs before EVERY removal this verb makes,
the repo-gone one included — but WHAT IT CAN CHECK DIFFERS BY BRANCH, so read
both of the next two sentences. While the repository is still there it reads the
worktree's own INDEX for gitlinks as git does, plus `.gitmodules`, and a
submodule record it cannot read is itself a refusal. Once the repository is gone
that gitlink record is gone with it — the admin directory holding it is what
vanished — and git has no verdict left to reproduce, so that branch is checked a
DIFFERENT way, neither strictly stronger nor strictly weaker: `.gitmodules`
lives in the tree rather than that directory, so it survives and is still read
there, and ON TOP of it the anchored walk of the tree's own contents refuses a
NESTED `.git` entry, of any type — which can KEEP a worktree over a vendored
repository git itself would have let you delete. A worktree whose repository is
gone cannot be salvaged, so unless something refuses it, it is removed with NO
salvage, and the report says so. There is no `--force` on this verb, so a
refusal names what to clear by hand instead of offering an override. Before
deletion begins, the anchored walk must completely traverse the worktree. A
worktree containing an unreadable descendant or exceeding the 64-directory-level
depth bound is SKIPPED whole, and nothing within it is deleted. During removal,
KEPT means no content was deleted; a failure after any deletion is a failed,
incomplete removal reported as PARTIAL with the affected worktree PATH, and is
not counted as reclaimed.")]
pub struct WorktreePruneArgs {
    /// Report what would be reclaimed and change nothing on disk.
    #[arg(long = "dry-run")]
    pub dry_run: bool,
    /// Reclaim only the managed worktrees for this task id (both kinds).
    #[arg(long)]
    pub task: Option<String>,
}

#[derive(Args, Debug, Clone)]
pub struct LeaseReleaseArgs {
    /// Task whose dispatch lease should be cleared (e.g. TASK-099).
    #[arg(long)]
    pub task: String,
    /// Project id; defaults to the project containing the cwd.
    #[arg(long)]
    pub project: Option<String>,
    /// Lease kind: implementer (covers reviewer/architector dispatches too).
    #[arg(long, default_value = "implementer")]
    pub kind: String,
}

// orgasmic:TASK-3CM0Q
/// The manager's recorded process-tier declaration (TASK-3CM0Q).
///
/// The `default` workflow computes the tier from countable triggers and tells
/// the manager to do so before its first source edit. That is a reading
/// obligation, and a manager already implementing skims one. This is the
/// writing obligation that leaves a trace: one command, one `manager.tier` tx,
/// and an undeclared task is visible as such afterwards.
#[derive(Args, Debug, Clone)]
#[command(after_help = "\
Examples:
  orgasmic manager tier --task TASK-XXXXX --tier trivial
  orgasmic manager tier --task TASK-XXXXX --tier ordinary --triggers coupling
  orgasmic manager tier --task TASK-XXXXX --tier risky --triggers blast_radius,breadth \\
    --reason \"touches writer durability and spans three crates\"
  orgasmic manager tier --task TASK-XXXXX   # read back what was declared")]
pub struct ManagerTierArgs {
    /// Task the declaration is about, e.g. TASK-3CM0Q.
    #[arg(long)]
    pub task: String,
    /// The computed tier: `trivial`, `ordinary`, or `risky`. Omit to read back
    /// the tier already declared for --task; reading exits non-zero when
    /// nothing has been declared, which is the out-of-policy state.
    #[arg(long)]
    pub tier: Option<String>,
    /// Triggers that fired, comma-separated or repeated: `priority`,
    /// `blast_radius`, `breadth`, `coupling`. Required above `trivial`, because
    /// the floor only rises when one fires and a reader checks the arithmetic.
    #[arg(long)]
    pub triggers: Vec<String>,
    /// One line of why. Optional for a plain `trivial`; required for the
    /// no-tracked-source exemption and for `--lower`.
    #[arg(long)]
    pub reason: Option<String>,
    /// Record a downgrade of an existing declaration as a correction. Scope
    /// that grew re-declares upward and does not need this.
    #[arg(long)]
    pub lower: bool,
    /// Project id; defaults to the project containing the cwd.
    #[arg(long)]
    pub project: Option<String>,
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
#[command(after_help = "\
Wake outcomes use stable exits: accepted=0, busy=4, unavailable=5, mismatch=6, unsupported=7.")]
pub struct ManagerWakeArgs {
    /// Project id; defaults to the project containing the cwd.
    #[arg(long)]
    pub project: Option<String>,
}

const MANAGER_WAKE_BUSY_EXIT: i32 = 4;
const MANAGER_WAKE_UNAVAILABLE_EXIT: i32 = 5;
const MANAGER_WAKE_MISMATCH_EXIT: i32 = 6;
const MANAGER_WAKE_UNSUPPORTED_EXIT: i32 = 7;

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
    pub(crate) reuse_worktree: bool,
    pub(crate) branch: String,
    pub(crate) model_override: Option<String>,
    pub(crate) effort_override: Option<String>,
    pub(crate) credential_mode_override: Option<String>,
    pub(crate) last_path: PathBuf,
    pub(crate) stdout_path: PathBuf,
    pub(crate) dispatch_attempt_token: String,
    pub(crate) goal_id: Option<String>,
    /// Reported dispatch generation(s) this reviewer was allowed to overlap.
    /// The daemon records these on manager.dispatch_started as REVIEWS_TX.
    pub(crate) reviewed_dispatch_txs: Vec<String>,
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
struct DispatchCloseCommitRequest {
    close_tx: TxAppendRequest,
    state: String,
    reason: String,
    request_id: String,
}

#[derive(Debug, Deserialize)]
struct DispatchCloseCommitResponse {
    close_tx: TxAppendResponse,
    #[allow(dead_code)]
    transition_tx_id: String,
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

/// One live run, as the daemon reports it.
///
/// This used to deserialize `run_id` AND NOTHING ELSE, which is why the CLI
/// could not match a live run to a worktree at all and `worktree-prune` had to
/// take the tx ledger's word for who owned a directory (TASK-M47E5.2 finding 2).
/// The daemon's own `RunSummary` has carried `worktree`, `project_id` and
/// `task_id` all along; only this mirror was blind. Every field is `default`ed
/// so an older daemon that omits one degrades to "unknown" rather than to a
/// decode failure — and "unknown" refuses to reclaim, which is the safe way to
/// be wrong.
#[derive(Debug, Deserialize)]
struct RunSummary {
    run_id: String,
    #[serde(default)]
    task_id: Option<String>,
    #[serde(default)]
    project_id: Option<String>,
    #[serde(default)]
    worktree: Option<PathBuf>,
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
    /// Repo-relative path of the promoted worker report (`last.txt`), when
    /// close moved it out of gitignored `tmp/` (TASK-QGWK7).
    report_path: Option<String>,
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
    /// Whether this removal destroyed ANYTHING, whether or not it finished.
    ///
    /// TASK-RMA18 made "kept means untouched" a hard property: a removal that
    /// fails part-way has already deleted files, and a report that calls that
    /// KEPT is a lie an operator acts on. `removed == false && touched == true`
    /// is a PARTIAL, and the report says so.
    touched: bool,
    salvage: Option<SalvageCommit>,
    /// A failure of the REMOVAL itself. Callers may classify the close as
    /// `worktree_failed` on this — and only on this.
    error: Option<String>,
    /// A failure of promoting/persisting the worker report AFTER a successful
    /// removal (TASK-QGWK7.1.1 M-1). It rides its own channel because folding
    /// it into `error` made a `git add` failure report `worktree_failed` for a
    /// worktree that WAS removed, which then silently suppressed every
    /// `--branch-delete` arm and kept the branch with nothing naming it.
    report_error: Option<String>,
    report_path: Option<String>,
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
    for other_kind in [DispatchKind::Implementer, DispatchKind::Reviewer] {
        if other_kind != plan.kind {
            let other_default =
                default_worktree(home, &plan.project_id, first_task(&plan.tasks), other_kind)?;
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
            dispatch_artifact_paths_for_attempt(&plan.project_root, &plan.brief_path, &attempt_id)?;
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

    prepare_worktree(&plan)?;

    // TASK-79VKP.6: each worker receives Cargo's default target directory
    // inside its own checkout.  Do not seed it from the manager target: that
    // would either copy tens of GiB before the worker starts, or risk stale
    // workspace fingerprints and build-script paths from another checkout.
    let cache_seed = private_worktree_target_policy(&plan.project_root, &plan.worktree_path);
    eprintln!(
        "dispatch cache-seed: status={} target={} duration_ms={}{}",
        cache_seed.status(),
        plan.worktree_path.join("target").display(),
        cache_seed.elapsed().as_millis(),
        cache_seed.detail(),
    );

    let runtime = tokio::runtime::Runtime::new().context("create tokio runtime")?;
    let client = DaemonClient::from_home_autostart(home)?;

    if let Err(err) = apply_task_lifecycle_transitions(
        &client,
        &plan.project_id,
        &dispatch_lifecycle_transitions(plan.kind, &plan.tasks),
    ) {
        let reason = format!("lifecycle update failed: {err}");
        let cleanup = if plan.reuse_worktree {
            retain_reused_worktree_after_failed_dispatch(&plan)
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
            let mut reason = format!("daemon dispatch failed: {err}");
            if ambiguous && plan.reuse_worktree {
                // Fencing wins over warmth: the POST may have started a worker,
                // so only the daemon may decide whether this reused tree is free.
                reason.push_str(
                    "; reused chain worktree was not re-locked and was handed to daemon cleanup because dispatch acceptance was ambiguous",
                );
            }
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
                        report_path: None,
                    },
                }
            } else if plan.reuse_worktree {
                retain_reused_worktree_after_failed_dispatch(&plan)
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
    println!(
        "watch: orgasmic manager dispatch-wait --started-tx {} &",
        response.dispatch_tx_id
    );
    Ok(())
}

pub fn cmd_dispatch_close(home: &Home, mut args: DispatchCloseArgs) -> Result<()> {
    let project_root = find_live_project_root(home, "manager dispatch-close")?;
    let project_id = read_project_id(&project_root)?;
    let tasks = normalize_tasks(args.task.clone())?;
    // orgasmic:task_EP3H1 — before anything else, including the already-closed
    // no-op below: a re-run of a torn close must finish the transition it lost
    // rather than report "already closed" over a task still stranded at its
    // pre-close stage.
    reconcile_torn_closes_best_effort(home, &project_root, &project_id);
    // orgasmic:TASK-QGWK7.1.1 — M-5: refuse before anything is destroyed, so a
    // fixable typo costs nothing. TASK-QGWK7.1.1.1 F-6: and BELOW the
    // reconciliation, not above it. Inserting the refusal first silently demoted
    // a path that was explicitly ordered first — a torn close re-run with the
    // same command line (which is how it is re-run) carries the same
    // `REPORT_PATH`, so it bailed before the stranded transition was repaired.
    normalize_report_path_property(&project_root, &mut args.properties)?;
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
            // orgasmic:TASK-QGWK7.1.1.1 — F-1: "no-op" is the contract for
            // CLEANUP, not for a record the previous close promoted onto disk
            // and then failed to commit. That state has no other way back into
            // git, and this is the only command a manager re-runs after it.
            repersist_dispatch_record_best_effort(&project_root, &closed);
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
    if args.no_review_required
        && !(args.status == DispatchCloseStatus::Done && tx_type == "implementer.done")
    {
        bail!("--no-review-required is valid only when closing an implementer dispatch as done");
    }
    if args.no_review_required
        && args
            .reason
            .as_ref()
            .map(|reason| sanitize_tx_value(reason))
            .filter(|reason| !reason.is_empty())
            .is_none()
    {
        bail!("--no-review-required requires --reason so the bypass is auditable");
    }
    // orgasmic:TASK-4WKNX — fenced HERE, next to the other early refusals,
    // rather than in `close_lifecycle_transitions`: that runs after worktree
    // cleanup, so a refusal from there would arrive with the worktree already
    // removed.
    validate_fix_round_final(&project_root, &tasks, &args, tx_type)?;
    validate_manager_owned_close_properties(&args)?;
    // orgasmic:task_ZKZBF
    // `--status aborted` has no generic property channel: `close_aborted_request`
    // records only its structured fields (--reason, worktree, lifecycle,
    // cleanup), so every `--property` value used to be accepted here and then
    // silently dropped — the TASK-HXSW0 shape. Refuse by name instead.
    if args.status == DispatchCloseStatus::Aborted && !args.properties.is_empty() {
        let keys = args
            .properties
            .iter()
            .map(|(key, _)| key.as_str())
            .collect::<Vec<_>>()
            .join(", ");
        bail!(
            "--property {keys} is not recorded by `--status aborted`: the abort tx carries only \
             its structured fields and used to silently drop --property values; re-run without \
             --property, or close with --status done to record them"
        );
    }
    // Fenced the same way as `--no-review-required`: `--verdict` is meaningful
    // only on the close that records the reviewer's own terminal tx.
    if args.verdict.is_some()
        && !(args.status == DispatchCloseStatus::Done && tx_type == "reviewer.done")
    {
        bail!(
            "--verdict is valid only when closing a reviewer dispatch as done: it records the \
             VERDICT property on the reviewer.done tx that the default-branch review gate reads"
        );
    }
    let verified_merge = if matches!(tx_type, "implementer.done" | "architector.done") {
        merge_sha
            .as_deref()
            .map(|merge_sha| {
                verify_merge_evidence(&project_root, merge_sha, args.worker_commit.as_deref())
            })
            .transpose()?
    } else {
        None
    };
    if tx_type == "implementer.done" {
        let merge = verified_merge.as_ref().expect("verified implementer merge");
        if merge_lands_on_default_branch(home, &project_id, &project_root, &merge.sha)?
            && !args.no_review_required
            && !reviewer_verdict_exists(&project_root, &tasks, &open.tx_id)?
        {
            // orgasmic:TASK-YN5FJ.1 — the remedy printed here has to be one an
            // operator can follow verbatim. It used to say only "dispatch and
            // close a reviewer", while the gate also requires that close to
            // carry a non-empty VERDICT — so following it exactly earned the
            // same refusal again. Name the requirement and the flag.
            //
            // orgasmic:TASK-YN5FJ.1.1 — and it has to be PASTEABLE. The first
            // pass printed `manager dispatch-close --task <task> …`: no
            // executable, and a task placeholder standing in for something this
            // refusal is holding in its hand. The rule now is that every token
            // the refusal already knows is printed as its real value, and the
            // message says which of the rest are placeholders — so a reader can
            // tell "substitute this" from "type this" without guessing.
            let remedy = format!(
                "orgasmic manager dispatch-close {} --started-tx <reviewer-tx> --status done \
                 --verdict <{}>",
                tasks
                    .iter()
                    .map(|task| format!("--task {task}"))
                    .collect::<Vec<_>>()
                    .join(" "),
                ReviewVerdict::value_list()
            );
            bail!(
                "refusing --merge-sha `{}` on the default branch: no reviewer verdict exists for \
                 implementer generation {} and task(s) {}. The gate needs a reviewer.done that \
                 reviewed this generation AND carries a VERDICT. Dispatch a reviewer for that \
                 reported generation and close it with `{}` — paste it as printed: its only \
                 placeholders are <reviewer-tx>, the started_tx= that reviewer dispatch prints, \
                 and the <{}> choice; every other token is already its real value. Any of those \
                 verdicts clears the gate, including reject. Or re-run with --no-review-required \
                 --reason <why>",
                merge.sha,
                open.tx_id,
                task_list_property(&tasks),
                remedy,
                ReviewVerdict::value_list()
            );
        }
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
    // `aborted` means abandonment by default. Only the operator's explicit
    // keep flag says another implementer round is coming.
    let keep_chain_worktree = args.status == DispatchCloseStatus::Aborted
        && args.no_worktree_remove
        && open.kind == DispatchKind::Implementer.as_str()
        && open.worktree.as_deref().is_some_and(Path::is_dir);
    let remove_worktree = args.worktree_remove && !args.no_worktree_remove && !keep_chain_worktree;
    let delete_branch = dispatch_close_deletes_branch(&args);
    // TASK-1T3FZ: a destructive close takes a DAEMON-OWNED reservation on the
    // worktree before it releases — or fails to find — any run, and holds it
    // until cleanup is done. The competing recovery runs in another process
    // (`POST /runs/:origin/recover`), so a liveness decision made here and
    // acted on here has a window no amount of in-process care can close: only
    // the supervisor lock, which the acquire path also takes, can install a
    // fence and read liveness as one step. The verdict comes back with it.
    //
    // TASK-QGWK7.1 F-3: `--no-worktree-remove` promote also unlinks tmp
    // artifacts, so it takes the same guard whenever promotable paths exist.
    let mut close_guard = if close_needs_artifact_fence(remove_worktree, &open) {
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
    if keep_chain_worktree {
        if let Err(error) = hold_chain_worktree(
            &project_root,
            open.worktree.as_deref().expect("checked chain worktree"),
            &open.tasks,
        ) {
            finish_close_guard(&runtime, &client, &project_id, &open, close_guard.as_mut());
            return Err(error);
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
            salvage: None,
            report_path: None,
        }
    } else {
        cleanup_dispatch(&project_root, &open, remove_worktree, delete_branch)
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
    for task in &missing_close_tasks {
        let transition = transition_for(&transitions, task).ok_or_else(|| {
            anyhow::anyhow!("close task {task} has no prepared lifecycle transition")
        })?;
        let close_tx = match args.status {
            DispatchCloseStatus::Done => close_done_request(
                &project_id,
                &open,
                task,
                &args,
                &CloseTxFacts {
                    tx_type,
                    merge_sha: verified_merge.as_ref().map(|merge| merge.sha.as_str()),
                    worker_commit: verified_merge
                        .as_ref()
                        .and_then(|merge| merge.worker_commit.as_deref()),
                    cleanup: &cleanup,
                    transition: Some(transition),
                },
            ),
            DispatchCloseStatus::Aborted => close_aborted_request(
                &project_id,
                &open,
                task,
                abort_reason.as_deref().expect("validated aborted reason"),
                &cleanup,
                Some(transition),
            ),
        };
        let response: DispatchCloseCommitResponse = runtime.block_on(client.post_json(
            &format!(
                "/projects/{}/tasks/{}/dispatch/close",
                path_segment(&project_id),
                path_segment(task)
            ),
            &DispatchCloseCommitRequest {
                close_tx,
                state: transition.to.as_str().to_string(),
                reason: format!(
                    "transition {} to {}",
                    transition.task,
                    transition.to.as_str()
                ),
                request_id: close_lifecycle_request_id(task, &open.tx_id),
            },
        ))?;
        responses.push(response.close_tx);
    }

    // A terminal tx written before close became atomic may carry no
    // LIFECYCLE_FROM/TO metadata, so the ledger reconciler cannot infer its
    // missing lifecycle leg. Preserve that legacy recovery only while the task
    // is still at the stage owned by the dispatch: a later deliberate move is
    // stronger evidence than the old close. Atomic close records are never
    // replayed here, even if their task was subsequently moved.
    let legacy_replay_tasks = legacy_close_replay_tasks(&project_root, &open, &tasks)?;
    for task in tasks
        .iter()
        .filter(|task| legacy_replay_tasks.contains(*task))
    {
        let transition = transition_for(&transitions, task).ok_or_else(|| {
            anyhow::anyhow!("already-closed task {task} has no prepared lifecycle transition")
        })?;
        runtime
            .block_on(post_task_state(
                &client,
                &project_id,
                transition,
                &open.tx_id,
                &close_lifecycle_request_id(task, &open.tx_id),
            ))
            .with_context(|| format!("recover lifecycle transition for legacy close {task}"))?;
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
    if args.status == DispatchCloseStatus::Done && open.kind == DispatchKind::Implementer.as_str() {
        release_chain_worktree_holds(&project_root, &tasks);
    }
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
const MANAGER_TERMINAL_CAPABILITY_ENV: &str = "ORGASMIC_MANAGER_TERMINAL_CAPABILITY";
const MANAGER_TERMINAL_CAPABILITY_HEADER: &str = "x-orgasmic-manager-terminal-capability";

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
/// this unconditionally on every manager startup. An app-launched manager has
/// a run id but no terminal capability and succeeds as an idempotent no-op;
/// only the bare custom terminal receives the capability that can claim a
/// manager lease.
pub fn cmd_manager_register(home: &Home, args: ManagerRegisterArgs) -> Result<()> {
    let project_id = match args.project.clone() {
        Some(project) => project,
        None => read_project_id(&find_project_root()?)?,
    };
    if std::env::var("ORGASMIC_RUN_ID").is_ok() {
        let Some(capability) = std::env::var(MANAGER_TERMINAL_CAPABILITY_ENV)
            .ok()
            .filter(|value| !value.trim().is_empty())
        else {
            println!("manager already supervised; registration is a no-op");
            return Ok(());
        };
        let client = DaemonClient::from_home_autostart(home)?;
        let runtime = tokio::runtime::Runtime::new().context("create tokio runtime")?;
        let response: ManagerRegisterHttpResponse =
            runtime.block_on(client.post_json_with_header(
                "/manager/register",
                &ManagerRegisterHttpRequest {
                    project_id: project_id.clone(),
                    pid: None,
                    holder_token: None,
                },
                MANAGER_TERMINAL_CAPABILITY_HEADER,
                &capability,
            ))?;
        match response.status.as_str() {
            "claimed" => println!("claimed terminal manager for {project_id}"),
            "refused" => bail!(
                "{}",
                response
                    .message
                    .unwrap_or_else(|| "manager terminal claim refused".into())
            ),
            other => bail!("unexpected manager terminal claim status: {other}"),
        }
        return Ok(());
    }
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
struct ManagerWakeHttpRequest {
    project_id: String,
}

#[derive(Debug, Deserialize)]
struct ManagerWakeHttpResponse {
    status: String,
    run_id: Option<String>,
    message: Option<String>,
}

/// Send a turn to the claimed manager terminal. The daemon transport owns the
/// final foreground-provider/composer proof; this CLI only reports its typed
/// outcome so callers can distinguish a busy provider from a dead claim.
pub fn cmd_manager_wake(home: &Home, args: ManagerWakeArgs) -> Result<()> {
    let project_id = match args.project {
        Some(project) => project,
        None => read_project_id(&find_project_root()?)?,
    };
    let client = DaemonClient::from_home_autostart(home)?;
    let runtime = tokio::runtime::Runtime::new().context("create tokio runtime")?;
    let response: ManagerWakeHttpResponse = runtime
        .block_on(client.post_json("/manager/wake", &ManagerWakeHttpRequest { project_id }))?;
    println!(
        "manager wake: {} run_id={} {}",
        response.status,
        response.run_id.as_deref().unwrap_or("-"),
        response.message.as_deref().unwrap_or("")
    );
    match response.status.as_str() {
        "accepted" => Ok(()),
        "busy" => std::process::exit(MANAGER_WAKE_BUSY_EXIT),
        "unavailable" => std::process::exit(MANAGER_WAKE_UNAVAILABLE_EXIT),
        "mismatch" => std::process::exit(MANAGER_WAKE_MISMATCH_EXIT),
        "unsupported" => std::process::exit(MANAGER_WAKE_UNSUPPORTED_EXIT),
        other => bail!("unexpected manager wake status: {other}"),
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
    // App-created manager panes deliberately need only their exported run id
    // to release themselves. The runtime/boot ids are part of the stronger
    // terminal-claim identity, not the app-manager release contract.
    let run_id = std::env::var("ORGASMIC_RUN_ID")
        .ok()
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty());
    let capability = std::env::var(MANAGER_TERMINAL_CAPABILITY_ENV)
        .ok()
        .filter(|value| !value.trim().is_empty());
    let client = DaemonClient::from_home_autostart(home)?;
    let runtime = tokio::runtime::Runtime::new().context("create tokio runtime")?;
    let request = ManagerReleaseHttpRequest {
        project_id: project_id.clone(),
        run_id,
    };
    let response: ManagerReleaseHttpResponse = match capability.as_deref() {
        Some(capability) => runtime.block_on(client.post_json_with_header(
            "/manager/release",
            &request,
            MANAGER_TERMINAL_CAPABILITY_HEADER,
            capability,
        ))?,
        None => runtime.block_on(client.post_json("/manager/release", &request))?,
    };

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

// orgasmic:TASK-3CM0Q
#[derive(Debug, Serialize)]
struct ManagerTierHttpRequest {
    project: String,
    task: String,
    tier: String,
    triggers: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    reason: Option<String>,
    lower: bool,
}

#[derive(Debug, Deserialize)]
struct ManagerTierHttpResponse {
    status: String,
    tx_id: String,
    tier: String,
    #[serde(default)]
    triggers: Vec<String>,
    #[serde(default)]
    previous_tier: Option<String>,
    #[serde(default)]
    lowered: bool,
}

#[derive(Debug, Deserialize)]
struct ManagerTierDeclarationView {
    tier: String,
    #[serde(default)]
    triggers: Vec<String>,
    #[serde(default)]
    reason: Option<String>,
    tx_id: String,
    time: String,
    #[serde(default)]
    actor: String,
    #[serde(default)]
    lowered: bool,
}

#[derive(Debug, Deserialize)]
struct ManagerTierStatusHttpResponse {
    task: String,
    project: String,
    declared: bool,
    #[serde(default)]
    current: Option<ManagerTierDeclarationView>,
    #[serde(default)]
    declarations: usize,
}

fn format_triggers(triggers: &[String]) -> String {
    if triggers.is_empty() {
        "none".to_string()
    } else {
        triggers.join(", ")
    }
}

// orgasmic:TASK-3CM0Q
/// Record — or read back — the manager's computed process tier for a task.
///
/// The `default` workflow already computes the tier from countable triggers and
/// tells the manager to do so before its first source edit. That is a *reading*
/// obligation, and a manager already implementing skims one without noticing.
/// This is the writing obligation: one command, one `manager.tier` tx on the
/// append-only ledger, and a task nobody declared is visibly undeclared
/// afterwards.
///
/// It is a statement to the record, not a question to the operator. Nothing
/// here blocks on an answer, because the tier is computed and there is no
/// decision to escalate.
pub fn cmd_manager_tier(home: &Home, args: ManagerTierArgs) -> Result<()> {
    let task = args.task.trim().to_string();
    if task.is_empty() {
        bail!("--task names the task the declaration is about, e.g. TASK-3CM0Q");
    }
    let project_id = match args.project.clone() {
        Some(project) => project,
        None => read_project_id(&find_project_root()?)?,
    };
    let client = DaemonClient::from_home_autostart(home)?;
    let runtime = tokio::runtime::Runtime::new().context("create tokio runtime")?;

    // No --tier: read back what is on the ledger. This is the audit side of the
    // same verb — the omission the declaration exists to make detectable is
    // detected here, and exits non-zero so a check can be scripted.
    let Some(tier) = args.tier.clone() else {
        return cmd_manager_tier_read(&client, &runtime, &project_id, &task);
    };

    let response: ManagerTierHttpResponse = runtime.block_on(client.post_json(
        "/manager/tier",
        &ManagerTierHttpRequest {
            project: project_id.clone(),
            task: task.clone(),
            tier,
            triggers: args.triggers.clone(),
            reason: args.reason.clone(),
            lower: args.lower,
        },
    ))?;

    let triggers = format_triggers(&response.triggers);
    match response.status.as_str() {
        "declared" => println!(
            "declared {task} {} (triggers: {triggers}) — {}",
            response.tier, response.tx_id
        ),
        "redeclared" => println!(
            "re-declared {task} {} → {} (triggers: {triggers}){} — {}",
            response.previous_tier.as_deref().unwrap_or("-"),
            response.tier,
            if response.lowered {
                ", recorded as a downgrade"
            } else {
                ""
            },
            response.tx_id
        ),
        other => println!("manager tier: {other} ({})", response.tx_id),
    }
    if response.tier != "trivial" {
        println!(
            "{task} is not manager-direct: dispatch it, and run an independent reviewer pass if \
             risky"
        );
    }
    Ok(())
}

// orgasmic:TASK-3CM0Q
/// The audit half of `manager tier`: read back what the ledger holds.
///
/// Exits non-zero when nothing was ever declared, so the out-of-policy state is
/// scriptable and not merely readable.
fn cmd_manager_tier_read(
    client: &DaemonClient,
    runtime: &tokio::runtime::Runtime,
    project_id: &str,
    task: &str,
) -> Result<()> {
    let status: ManagerTierStatusHttpResponse = runtime.block_on(client.get(&format!(
        "/manager/tier?project={}&task={}",
        path_segment(project_id),
        path_segment(task)
    )))?;
    let Some(current) = status.current else {
        debug_assert!(!status.declared);
        bail!(
            "no tier declared for {} in {}: manager-direct source edits are out of policy until \
             one is recorded. Compute the tier from the `default` workflow's triggers and state \
             it:\n  orgasmic manager tier --task {} --tier trivial|ordinary|risky [--triggers ...]",
            status.task,
            status.project,
            status.task
        )
    };
    println!(
        "{} is declared {} (triggers: {})",
        status.task,
        current.tier,
        format_triggers(&current.triggers)
    );
    if let Some(reason) = current.reason.as_deref() {
        println!("  reason: {reason}");
    }
    println!(
        "  {} at {} by {}{}",
        current.tx_id,
        current.time,
        if current.actor.is_empty() {
            "-"
        } else {
            current.actor.as_str()
        },
        if current.lowered {
            " (recorded as a downgrade)"
        } else {
            ""
        }
    );
    if status.declarations > 1 {
        println!(
            "  {} declarations on the ledger for this task",
            status.declarations
        );
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
    // orgasmic:TASK-QGWK7 — name where the report will live after close, keyed
    // by the dispatch generation. Close promotes last.txt there; until then
    // the streaming sink remains under tmp/.
    if let Some(report_path) =
        durable_report_path_for_finalize(home, project_id.as_deref(), &run.run_id)
    {
        extra.push(("REPORT_PATH".to_string(), report_path));
    } else if let Some(rel) =
        project_relative_report_path_fallback(home, project_id.as_deref(), run.last_path.as_deref())
    {
        // TASK-QGWK7.1 F-6: never write an absolute /Users/... path into a
        // committed tx. Prefer no REPORT_PATH over a machine-specific one.
        extra.push(("REPORT_PATH".to_string(), rel));
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
    // still had to write the tx. On stdio that reap is a direct
    // `kill(-pgid, …)` and the tx was lost every time (3/3). Losing it left a
    // durable commit, a durable last.txt, a RELEASED
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
            // orgasmic:TASK-RB1ZN — checked BEFORE the already-released rescue,
            // because it is the state that rescue cannot read. Another authority
            // (a stall sweep, a protocol-end release, a manager's abandon) holds
            // this run's release; it owns the tombstone, and it has not written
            // one yet. Refuse the terminal tx rather than claim a completion
            // this call did not perform — the commit and last.txt above are
            // already durable, so re-running finalize once that release lands is
            // safe and lands on whichever answer its tombstone justifies.
            if is_release_in_progress_error(&e) {
                return Err(e).with_context(|| {
                    format!(
                        "refusing to emit {tx_type} for {task}: the release that IS \
                         running owns run {}'s tombstone. Re-run this same `orgasmic \
                         dispatch finalize` once it lands",
                        run.run_id
                    )
                });
            }
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

/// Resolve the durable `:REPORT_PATH:` a finalize tx should name (TASK-QGWK7).
/// Looks up the live project's tx log for this run's `DISPATCH_TX` (the
/// generation / `started_tx`), then returns the tracked path close will
/// promote `last.txt` into. Returns `None` when the live project or the
/// pairing cannot be resolved — finalize then falls back to a project-relative
/// tmp last_path when one can be derived.
fn durable_report_path_for_finalize(
    home: &Home,
    project_id: Option<&str>,
    run_id: &str,
) -> Option<String> {
    let project_id = project_id?;
    let board = projects::read_board(home).ok()?;
    let project_root = board
        .into_iter()
        .find(|entry| entry.id == project_id)
        .map(|entry| entry.path)?;
    let record = scan_dispatches(&project_root)
        .ok()?
        .into_iter()
        .find(|record| record.run_ids.iter().any(|id| id == run_id))?;
    orgasmic_core::dispatch_record_report_rel(record.tasks.first()?, &record.tx_id).ok()
}

/// Make a manager-supplied `--property REPORT_PATH=` project-relative, or
/// refuse the close (TASK-QGWK7.1.1 M-5).
///
/// This is the fourth `:REPORT_PATH:` emitter and the one the F-6 fix missed:
/// the other three relativize or emit nothing, while this one won over them
/// unvalidated, so a manager who pasted an absolute path wrote a
/// machine-specific `/Users/...` into a committed tx that other clones read.
/// A path under the project root is rewritten; one outside it has no relative
/// form, so the close says so rather than committing it or silently dropping
/// the manager's curated pointer.
fn normalize_report_path_property(
    project_root: &Path,
    properties: &mut [(String, String)],
) -> Result<()> {
    for (key, value) in properties.iter_mut() {
        if key != "REPORT_PATH" {
            continue;
        }
        let path = Path::new(value.as_str());
        if path.is_relative() {
            continue;
        }
        // Through `normalize_path` on both sides: a manager pastes the path a
        // shell printed, and on macOS that is `/var/...` for a project root
        // resolved as `/private/var/...`.
        let rel = path
            .strip_prefix(project_root)
            .map(Path::to_path_buf)
            .or_else(|_| {
                normalize_path(path)
                    .strip_prefix(normalize_path(project_root))
                    .map(Path::to_path_buf)
            });
        match rel {
            Ok(rel) => *value = rel.display().to_string(),
            Err(_) => bail!(
                "--property REPORT_PATH={value} is outside the project root ({}); :REPORT_PATH: \
                 is committed to the tx log, so it must be project-relative",
                project_root.display()
            ),
        }
    }
    Ok(())
}

/// Project-relative fallback for finalize `:REPORT_PATH:` (TASK-QGWK7.1 F-6).
/// Absolute paths are stripped against the project root; if that fails, returns
/// `None` rather than committing a machine-specific path.
fn project_relative_report_path_fallback(
    home: &Home,
    project_id: Option<&str>,
    last_path: Option<&Path>,
) -> Option<String> {
    let last = last_path?;
    if last.is_relative() {
        return Some(last.display().to_string());
    }
    let project_id = project_id?;
    let board = projects::read_board(home).ok()?;
    let project_root = board
        .into_iter()
        .find(|entry| entry.id == project_id)
        .map(|entry| entry.path)?;
    last.strip_prefix(&project_root)
        .ok()
        .map(|rel| rel.display().to_string())
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

    salvage_worktree_onto(project_root, worktree, task, expected_branch_oid)
}

/// Commit a dirty worktree's contents onto `parent_oid` and anchor the result
/// at `refs/orgasmic/salvage/<sha>`. The ONE salvage mechanism: the close path
/// reaches it through `salvage_worktree_if_dirty` (which first proves the
/// recorded dispatch branch has not moved), and `worktree-prune` reaches it
/// directly with the worktree's own HEAD as the parent, because a worktree
/// with no open dispatch has no recorded branch left to validate against.
// orgasmic:TASK-M47E5
fn salvage_worktree_onto(
    project_root: &Path,
    worktree: &Path,
    task: &str,
    parent_oid: &str,
) -> Result<Option<SalvageCommit>> {
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
            parent_oid,
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
    let file_count = changed_file_count_between(worktree, parent_oid, &sha)?;

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
    let project_id = read_project_id(&project_root).ok();
    // orgasmic:task_EP3H1 — the command an operator runs after a close warns
    // about a lost lifecycle leg is this one. Repair before reporting, so what
    // it reports is the repaired state.
    if let Some(project_id) = project_id.as_deref() {
        reconcile_torn_closes_best_effort(home, &project_root, project_id);
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
        // TASK-M47E5: closing an orphan is exactly the moment its worktree
        // becomes reclaimable, so say so here too. No live-run fetch on this
        // leg — the ledger alone decides whether a worktree is held, and a run
        // list would only sharpen the wording of an entry already refused.
        if let Some(project_id) = project_id.as_deref() {
            report_managed_worktrees(home, &project_root, project_id, &[]);
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
    let claims = read_claims(&project_root).context("read task claims for dispatch-status")?;
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
        let holders = record
            .tasks
            .iter()
            .filter_map(|task| claims.get(task).map(|claim| claim.holder.clone()))
            .collect::<BTreeSet<_>>();
        let double_claims = record
            .tasks
            .iter()
            .filter_map(|task| {
                claims
                    .get(task)
                    .filter(|claim| claim.contenders.len() > 1)
                    .map(|claim| format!("{task}:[{}]", claim.contenders.join(",")))
            })
            .collect::<Vec<_>>();
        println!(
            "TX_ID={} TASK={} KIND={} STARTED_AT={} WORKTREE={} WORKER_PID={} RUN_ID={} WORKER={} DRIVER={} HARNESS={} {} {} {} {} CLAIM_HOLDER={} DOUBLE_CLAIM={}{}",
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
            pid_flag(&health),
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
            if holders.is_empty() {
                "-".to_string()
            } else {
                holders.into_iter().collect::<Vec<_>>().join(",")
            },
            if double_claims.is_empty() {
                "-".to_string()
            } else {
                double_claims.join(";")
            },
            partial_closed
                .map(|annotation| format!(" {annotation}"))
                .unwrap_or_default()
        );
    }
    // TASK-M47E5: the automatic detection half. Managed worktrees now live
    // outside the repo, where `git status` and the operator's eyes no longer
    // find them, so the inventory verb has to name the ones nothing owns.
    if let Some(project_id) = project_id.as_deref() {
        report_managed_worktrees(home, &project_root, project_id, &live_runs);
    }
    Ok(())
}

/// Block until every named dispatch generation reports, dies, or reaches its
/// caller-supplied deadline.  This intentionally consults the daemon's live
/// run inventory rather than `[pid-gone]`: pane transports may record no
/// worker PID, and a missing PID is not evidence that the generation ended.
#[derive(Debug, Deserialize)]
struct ManagerDispatchWaitGeneration {
    started_tx: String,
    status: String,
    run_id: Option<String>,
}

#[derive(Debug, Deserialize)]
struct ManagerDispatchWaitHttpResponse {
    generations: Vec<ManagerDispatchWaitGeneration>,
}

enum DispatchWaitRound<'a> {
    Unknown(&'a ManagerDispatchWaitGeneration),
    Died(&'a ManagerDispatchWaitGeneration),
    Reported,
    Waiting,
}

fn classify_dispatch_wait_round(
    generations: &[ManagerDispatchWaitGeneration],
) -> DispatchWaitRound<'_> {
    if let Some(generation) = generations
        .iter()
        .find(|generation| generation.status == "unknown")
    {
        return DispatchWaitRound::Unknown(generation);
    }
    if let Some(generation) = generations
        .iter()
        .find(|generation| generation.status == "died")
    {
        return DispatchWaitRound::Died(generation);
    }
    if generations
        .iter()
        .all(|generation| matches!(generation.status.as_str(), "reported" | "closed"))
    {
        DispatchWaitRound::Reported
    } else {
        DispatchWaitRound::Waiting
    }
}

pub fn cmd_dispatch_wait(home: &Home, args: DispatchWaitArgs) -> Result<()> {
    let project_root = find_live_project_root(home, "manager dispatch-wait")?;
    let project_id = read_project_id(&project_root)?;
    let client = DaemonClient::from_home_autostart(home)?;
    let runtime = tokio::runtime::Runtime::new().context("create tokio runtime")?;
    let requested = args.started_tx.into_iter().collect::<BTreeSet<_>>();
    let started = std::time::Instant::now();
    loop {
        #[derive(Serialize)]
        struct Request<'a> {
            project_id: &'a str,
            started_tx: Vec<&'a str>,
        }
        let response: ManagerDispatchWaitHttpResponse = runtime
            .block_on(client.post_json(
                "/manager/dispatch-wait",
                &Request {
                    project_id: &project_id,
                    started_tx: requested.iter().map(String::as_str).collect(),
                },
            ))
            .context(
                "dispatch-wait lost (daemon unreachable or errored) — worker state unknown, \
                 not ended; re-run `orgasmic manager dispatch-status`",
            )?;
        match classify_dispatch_wait_round(&response.generations) {
            DispatchWaitRound::Unknown(generation) => {
                bail!(
                    "dispatch-wait: no open dispatch generation {}",
                    generation.started_tx
                );
            }
            DispatchWaitRound::Died(generation) => {
                println!(
                    "dispatch-wait: died TX_ID={} RUN_ID={}",
                    generation.started_tx,
                    generation.run_id.as_deref().unwrap_or("-")
                );
                std::process::exit(2);
            }
            DispatchWaitRound::Reported => {
                for generation in &response.generations {
                    println!(
                        "dispatch-wait: reported TX_ID={} RUN_ID={}",
                        generation.started_tx,
                        generation.run_id.as_deref().unwrap_or("-")
                    );
                }
                return Ok(());
            }
            DispatchWaitRound::Waiting => {}
        }
        if args
            .timeout
            .is_some_and(|timeout| started.elapsed() >= timeout)
        {
            eprintln!("dispatch-wait: timeout");
            std::process::exit(3);
        }
        std::thread::sleep(Duration::from_secs(1));
    }
}

// ===== TASK-M47E5: managed worktree reclamation ==========================
//
// RECLAMATION IS AN EXPLICIT VERB; DETECTION IS AUTOMATIC. The split is
// deliberate and is the whole design, so it is recorded here rather than in a
// commit message.
//
// Removing a worktree is the sharpest tool in this codebase. A worker's
// uncommitted output is the single most easily destroyed thing here, and
// TASK-2BPWM and TASK-D0GA3 exist because it was destroyed once already. A
// reaper that fires on a timer, at daemon boot, or as a side effect of close
// would put that loss one bug away from being automatic and unattended. So
// nothing gains the authority to delete: `orgasmic manager worktree-prune` is
// operator-run, and it is the only surface here that removes anything.
//
// But a verb nobody runs is exactly how invisible garbage accumulates, and
// that is precisely why this shipped WITH the relocation instead of after it.
// Before the move, a stray worktree sat inside the repo where `git status`,
// `git worktree list` and the operator's own eyes found it. Under
// `~/.orgasmic` the same leak is silent multi-GB accumulation. So DETECTION
// runs automatically on every `manager dispatch-status` — the existing orphan
// surface, not a parallel one — and names what is reclaimable, why, and how
// many bytes it would return.

// ===== TASK-RMA18: the handle is the reference point, end to end ==========
//
// TASK-M47E5.2 finding 1 changed the reference point for ENUMERATION and for
// the final `unlinkat`, and that much was right: `openat`/`unlinkat` name
// entries relative to the inode a handle holds, so renaming or relinking the
// path that handle was opened through cannot redirect a removal. Two review
// rounds then found that the property was asserted in a comment and not
// actually held, in two independent ways.
//
// FINDING 4 — `O_NOFOLLOW` GUARDS ONLY THE FINAL COMPONENT. Opening
// `<home>/worktrees/<project-id>` in one syscall makes the kernel resolve
// `<home>/worktrees` by pathname, following whatever it happens to be. Replace
// that ANCESTOR with a symlink and the handle anchors a victim directory, with
// every downstream fd-relative guarantee intact and pointed at the wrong tree.
// So the walk starts at a TRUST ROOT — the home directory the CLI was
// configured with, resolved once — and every component below it is opened with
// `openat(..., O_NOFOLLOW | O_DIRECTORY)`. No ancestor of the managed root is
// ever resolved by pathname again.
//
// FINDING 5 — THE HANDLE WAS NOT THE REFERENCE POINT FOR THE WHOLE DECISION.
// Enumeration used the fd, and then classification rebuilt `root.path()/name`
// and re-resolved metadata, `.git`, size and the daemon reservation through
// that pathname. The reservation could therefore describe tree B while
// `unlinkat` deleted anchored tree A. So a child is now opened ONCE, its
// (device, inode) recorded as its IDENTITY, and that identity is what
// classification reads through, what the daemon reservation is proved against,
// and what the removal re-proves immediately before the entry is unlinked. A
// pathname survives only as a label for the report and as the argument to the
// `git` subprocesses that cannot take a handle — and every such use is fenced
// by re-proving that the path resolves to the recorded identity right now.
//
// The one thing a pathname is still trusted for is the TRUST ROOT itself. That
// is a boundary, not an oversight: `~/.orgasmic` is where the operator points
// this runtime, its own ancestors are outside anything this verb can reason
// about, and macOS puts `/var` and `/tmp` behind symlinks, so refusing every
// link above the home directory would refuse every temp-rooted fixture as well
// as some real installs.
#[cfg(unix)]
mod anchored_dir {
    use anyhow::{bail, Context, Result};
    use std::ffi::{CString, OsStr, OsString};
    use std::fs::File;
    use std::os::fd::{AsRawFd, FromRawFd};
    use std::os::unix::ffi::{OsStrExt, OsStringExt};
    use std::path::Path;

    /// Deepest nesting this will descend into before refusing.
    ///
    /// The recursion holds ONE open fd per active level, and macOS ships a
    /// soft `RLIMIT_NOFILE` of 256 in many shells, so the bound has to sit well
    /// under that or a deep tree exhausts the fd table instead of being
    /// removed. 64 levels is already far past any real build tree; beyond it,
    /// refusing and keeping the directory is the right answer. A shallower
    /// `EMFILE` behaves the same way — `openat` fails, the removal reports the
    /// error, and the worktree survives.
    pub(super) const MAX_DEPTH: u32 = 64;

    /// A directory's identity: the `(device, inode)` pair the kernel reported
    /// for an open handle on it.
    ///
    /// This is the identity carried from enumeration through classification to
    /// removal (TASK-RMA18, finding 5). It is what a pathname is not: it does
    /// not move when the directory is renamed, it does not follow when the name
    /// is rebound to something else, and two different pathnames that resolve to
    /// it are the same directory rather than merely equal strings.
    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    pub(super) struct DirIdentity {
        dev: u64,
        ino: u64,
    }

    impl std::fmt::Display for DirIdentity {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            write!(f, "dev={} ino={}", self.dev, self.ino)
        }
    }

    impl DirIdentity {
        fn of_meta(meta: &std::fs::Metadata) -> Option<Self> {
            use std::os::unix::fs::MetadataExt;
            meta.is_dir().then(|| Self {
                dev: meta.dev(),
                ino: meta.ino(),
            })
        }
    }

    /// The identity of an OPEN handle. Cannot race: the handle already names the
    /// inode being described.
    pub(super) fn identity_of(dir: &File) -> Result<DirIdentity> {
        let meta = dir.metadata().context("fstat a directory handle")?;
        DirIdentity::of_meta(&meta)
            .ok_or_else(|| anyhow::anyhow!("a directory handle does not name a directory"))
    }

    /// The identity `path` resolves to RIGHT NOW, following symlinks.
    ///
    /// Following is deliberate. This answers "does this recorded pathname lead
    /// to the directory I hold open?", which is a question about where a path
    /// arrives, not about how it gets there — a live run's recorded worktree, or
    /// the argument about to be handed to `git`, is legitimately allowed to
    /// travel through a link as long as it lands on the anchored inode.
    pub(super) fn identity_of_path(path: &Path) -> Option<DirIdentity> {
        DirIdentity::of_meta(&std::fs::metadata(path).ok()?)
    }

    /// The identity of `name` inside `dir` WITHOUT following a final symlink.
    /// `None` when it is absent, or is not a directory, or is a symlink.
    #[allow(clippy::unnecessary_cast)]
    pub(super) fn identity_at(dir: &File, name: &OsStr) -> Option<DirIdentity> {
        let cname = c_name(name).ok()?;
        let mut st: libc::stat = unsafe { std::mem::zeroed() };
        let rc = unsafe {
            libc::fstatat(
                dir.as_raw_fd(),
                cname.as_ptr(),
                &mut st,
                libc::AT_SYMLINK_NOFOLLOW,
            )
        };
        if rc != 0 {
            return None;
        }
        if st.st_mode & libc::S_IFMT != libc::S_IFDIR {
            return None;
        }
        Some(DirIdentity {
            // `dev_t` and `ino_t` differ in width and signedness across the
            // unixes this builds for, so both are widened explicitly here even
            // where one of them is already `u64`.
            dev: st.st_dev as u64,
            ino: st.st_ino as u64,
        })
    }

    /// Open the TRUST ROOT. Its ancestors are resolved by the kernel exactly
    /// once, here, and no path below it is ever resolved again — see the design
    /// note above this module for why the boundary sits at the home directory.
    pub(super) fn open_trust_root(path: &Path) -> std::io::Result<File> {
        use std::os::unix::fs::OpenOptionsExt;
        std::fs::OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_DIRECTORY | libc::O_CLOEXEC)
            .open(path)
    }

    fn c_name(name: &OsStr) -> Result<CString> {
        CString::new(name.as_bytes())
            .with_context(|| format!("directory name {name:?} contains an interior NUL"))
    }

    /// What `openat(dir, name, O_NOFOLLOW | O_DIRECTORY)` found.
    pub(super) enum ChildOpen {
        Dir(File),
        /// Present, but not a directory this may descend into — a file, a
        /// device, or a SYMLINK refused by `O_NOFOLLOW`.
        NotADirectory,
        /// No such entry.
        Absent,
    }

    /// Open `name` inside `dir` without following a symlink at that component.
    pub(super) fn open_child_dir(dir: &File, name: &OsStr) -> Result<ChildOpen> {
        let cname = c_name(name)?;
        let fd = unsafe {
            libc::openat(
                dir.as_raw_fd(),
                cname.as_ptr(),
                libc::O_RDONLY | libc::O_NOFOLLOW | libc::O_DIRECTORY | libc::O_CLOEXEC,
            )
        };
        if fd >= 0 {
            return Ok(ChildOpen::Dir(unsafe { File::from_raw_fd(fd) }));
        }
        let err = std::io::Error::last_os_error();
        match err.raw_os_error() {
            // Not a directory, or a symlink refused by O_NOFOLLOW.
            Some(libc::ENOTDIR) | Some(libc::ELOOP) | Some(libc::EMLINK) => {
                Ok(ChildOpen::NotADirectory)
            }
            Some(libc::ENOENT) => Ok(ChildOpen::Absent),
            _ => Err(err).with_context(|| format!("openat {name:?}")),
        }
    }

    /// Open a NON-directory entry `name` inside `dir` for reading, without
    /// following a symlink at that component. Used to read a worktree's `.git`
    /// link through the anchored handle instead of through a pathname.
    pub(super) fn open_child_file(dir: &File, name: &OsStr) -> std::io::Result<File> {
        let cname = c_name(name).map_err(std::io::Error::other)?;
        let fd = unsafe {
            libc::openat(
                dir.as_raw_fd(),
                cname.as_ptr(),
                libc::O_RDONLY | libc::O_NOFOLLOW | libc::O_CLOEXEC,
            )
        };
        if fd >= 0 {
            return Ok(unsafe { File::from_raw_fd(fd) });
        }
        Err(std::io::Error::last_os_error())
    }

    /// The facts an fd-relative `lstat` has to report. `std::fs::Metadata`
    /// cannot be constructed outside `std`, so the three the callers use travel
    /// in their own type.
    pub(super) struct FileKind {
        mode: libc::mode_t,
        len: u64,
    }

    impl FileKind {
        pub(super) fn is_dir(&self) -> bool {
            self.mode & libc::S_IFMT == libc::S_IFDIR
        }
        pub(super) fn is_symlink(&self) -> bool {
            self.mode & libc::S_IFMT == libc::S_IFLNK
        }
        pub(super) fn len(&self) -> u64 {
            self.len
        }
    }

    /// `lstat` of `name` inside `dir`, so a worktree's `.git` — and every entry
    /// a size walk visits — is classified through the anchored handle rather
    /// than through a pathname.
    pub(super) fn stat_at(dir: &File, name: &OsStr) -> std::io::Result<FileKind> {
        let cname = c_name(name).map_err(std::io::Error::other)?;
        let mut st: libc::stat = unsafe { std::mem::zeroed() };
        let rc = unsafe {
            libc::fstatat(
                dir.as_raw_fd(),
                cname.as_ptr(),
                &mut st,
                libc::AT_SYMLINK_NOFOLLOW,
            )
        };
        if rc != 0 {
            return Err(std::io::Error::last_os_error());
        }
        Ok(FileKind {
            mode: st.st_mode,
            len: st.st_size as u64,
        })
    }

    /// Address of this thread's `errno`, where this build knows how to find it.
    ///
    /// `None` means "this platform's slot is not known here", and the caller
    /// then falls back to the pre-TASK-RMA18 reading of a null `readdir` as
    /// end-of-stream. That fallback is not silent: a short listing leaves
    /// entries behind, the enclosing `rmdir` fails `ENOTEMPTY`, and the removal
    /// reports what it touched rather than reporting KEPT.
    fn errno_slot() -> Option<*mut libc::c_int> {
        #[cfg(any(target_os = "linux", target_os = "android"))]
        {
            Some(unsafe { libc::__errno_location() })
        }
        #[cfg(any(
            target_os = "macos",
            target_os = "ios",
            target_os = "freebsd",
            target_os = "dragonfly"
        ))]
        {
            Some(unsafe { libc::__error() })
        }
        #[cfg(not(any(
            target_os = "linux",
            target_os = "android",
            target_os = "macos",
            target_os = "ios",
            target_os = "freebsd",
            target_os = "dragonfly"
        )))]
        {
            None
        }
    }

    /// Entry names inside `dir`, read through the handle rather than through a
    /// path, so nothing about the path's ancestors can steer the listing.
    ///
    /// A `readdir` that fails mid-stream returns the same null pointer as
    /// end-of-stream and is distinguished only by `errno`. Confusing them
    /// produces a SHORT list, which used to mean entries were quietly left
    /// behind and the directory reported KEPT after its contents had already
    /// been destroyed. So `errno` is cleared before each call and read after a
    /// null: a mid-stream failure is an ERROR here, not an end (TASK-RMA18,
    /// "kept means untouched").
    pub(super) fn entry_names(dir: &File) -> Result<Vec<OsString>> {
        // `fdopendir` takes ownership of the fd it is handed, so give it a dup
        // and leave the caller's handle intact for the removals that follow.
        let fd = unsafe { libc::dup(dir.as_raw_fd()) };
        if fd < 0 {
            return Err(std::io::Error::last_os_error()).context("dup a directory handle");
        }
        let stream = unsafe { libc::fdopendir(fd) };
        if stream.is_null() {
            let err = std::io::Error::last_os_error();
            unsafe { libc::close(fd) };
            return Err(err).context("fdopendir a directory handle");
        }
        // `dup` shares the FILE DESCRIPTION, and therefore the directory read
        // offset, with the caller's handle. Without this rewind a SECOND
        // enumeration of the same handle resumes at EOF and returns an EMPTY
        // list — which every caller here reads as "the directory is empty".
        // That is a fail-open in a destructive verb: an empty enumeration means
        // no nested repository to refuse over and nothing left to remove. Found
        // by `a_repo_gone_worktree_is_refused_over_a_nested_git_of_any_type`,
        // which walks one handle twice (TASK-RMA18.1.1.1).
        unsafe { libc::rewinddir(stream) };
        let slot = errno_slot();
        let mut names = Vec::new();
        let mut failure: Option<std::io::Error> = None;
        loop {
            if let Some(slot) = slot {
                unsafe { *slot = 0 };
            }
            let entry = unsafe { libc::readdir(stream) };
            if entry.is_null() {
                if let Some(slot) = slot {
                    let code = unsafe { *slot };
                    if code != 0 {
                        failure = Some(std::io::Error::from_raw_os_error(code));
                    }
                }
                break;
            }
            // `readdir` returns a pointer the next call invalidates, so copy now.
            let raw = unsafe { (*entry).d_name };
            let bytes: Vec<u8> = raw
                .iter()
                .take_while(|byte| **byte != 0)
                .map(|byte| *byte as u8)
                .collect();
            if bytes == b"." || bytes == b".." {
                continue;
            }
            names.push(OsString::from_vec(bytes));
        }
        unsafe { libc::closedir(stream) };
        if let Some(err) = failure {
            return Err(err).context(
                "readdir failed part-way through a directory, so its listing is incomplete",
            );
        }
        names.sort();
        Ok(names)
    }

    fn unlink_at(dir: &File, name: &OsStr, flags: libc::c_int) -> Result<bool> {
        let cname = c_name(name)?;
        let rc = unsafe { libc::unlinkat(dir.as_raw_fd(), cname.as_ptr(), flags) };
        if rc == 0 {
            return Ok(true);
        }
        let err = std::io::Error::last_os_error();
        if err.kind() == std::io::ErrorKind::NotFound {
            // Somebody else removed it; the post-condition holds either way, and
            // this process did not touch it.
            return Ok(false);
        }
        Err(err).with_context(|| format!("unlinkat {name:?}"))
    }

    /// A removal that did not complete, and whether it had already destroyed
    /// anything when it stopped.
    ///
    /// `touched` is the whole point: a caller may only print KEPT for
    /// `touched == false`. Anything else is a partial removal and has to say so
    /// (TASK-RMA18, "kept means untouched").
    pub(super) struct RemovalFailure {
        pub(super) touched: bool,
        pub(super) error: anyhow::Error,
    }

    fn remove_contents(dir: &File, depth: u32, touched: &mut bool) -> Result<()> {
        if depth > MAX_DEPTH {
            bail!("refusing to descend deeper than {MAX_DEPTH} directory levels");
        }
        for name in entry_names(dir)? {
            match open_child_dir(dir, &name)? {
                ChildOpen::Dir(child) => {
                    remove_contents(&child, depth + 1, touched)?;
                    drop(child);
                    *touched |= unlink_at(dir, &name, libc::AT_REMOVEDIR)?;
                }
                ChildOpen::NotADirectory => *touched |= unlink_at(dir, &name, 0)?,
                ChildOpen::Absent => {}
            }
        }
        Ok(())
    }

    /// Recursively remove the directory `name` inside `dir`, resolving every
    /// component against a directory handle rather than a path, and only if that
    /// entry still names `expected`.
    ///
    /// The identity is checked TWICE and both checks matter. The first is on the
    /// handle the contents are destroyed through, so nothing outside the
    /// classified inode can be emptied. The second is immediately before the
    /// enclosing `unlinkat(AT_REMOVEDIR)`, because that syscall names an ENTRY
    /// rather than an inode and is the one step this cannot express as "operate
    /// on the thing I hold open". The residual window is between that `fstatat`
    /// and that `unlinkat`, and what fits in it is the removal of a directory
    /// entry which must ALSO be an empty directory at that instant.
    pub(super) fn remove_dir_all_at(
        dir: &File,
        name: &OsStr,
        expected: DirIdentity,
    ) -> std::result::Result<(), RemovalFailure> {
        let untouched = |error: anyhow::Error| RemovalFailure {
            touched: false,
            error,
        };
        let child = match open_child_dir(dir, name) {
            Ok(ChildOpen::Dir(child)) => child,
            Ok(ChildOpen::NotADirectory) => {
                return Err(untouched(anyhow::anyhow!(
                    "refusing to remove {name:?}: it is not a directory, or it is a symlink"
                )))
            }
            Ok(ChildOpen::Absent) => {
                return Err(untouched(anyhow::anyhow!(
                    "refusing to remove {name:?}: it is no longer there"
                )))
            }
            Err(err) => return Err(untouched(err)),
        };
        match identity_of(&child) {
            Ok(found) if found == expected => {}
            Ok(found) => {
                return Err(untouched(anyhow::anyhow!(
                    "refusing to remove {name:?}: it now names a different directory \
                     ({found}) than the one this prune classified and reserved ({expected})"
                )))
            }
            Err(err) => return Err(untouched(err)),
        }
        let mut touched = false;
        if let Err(error) = remove_contents(&child, 1, &mut touched) {
            return Err(RemovalFailure { touched, error });
        }
        drop(child);
        if identity_at(dir, name) != Some(expected) {
            return Err(RemovalFailure {
                touched,
                error: anyhow::anyhow!(
                    "refusing to unlink {name:?}: it stopped naming the directory this prune \
                     classified and reserved ({expected}) before the entry could be removed"
                ),
            });
        }
        match unlink_at(dir, name, libc::AT_REMOVEDIR) {
            Ok(_) => Ok(()),
            Err(error) => Err(RemovalFailure {
                touched: true,
                error,
            }),
        }
    }
}

/// This project's managed worktree root, reached by opening every component
/// below the trust root with `openat(..., O_NOFOLLOW | O_DIRECTORY)` and HELD
/// OPEN for the life of the operation, together with the identity that handle
/// names. See the design note above [`anchored_dir`].
// orgasmic:TASK-M47E5.2,TASK-RMA18
#[derive(Debug)]
struct AnchoredManagedRoot {
    /// Reported to the operator and used to build the pathnames handed to
    /// `git`. Never resolved for a removal, and never trusted for one: every
    /// path use is fenced by re-proving it resolves to a recorded identity.
    path: PathBuf,
    #[cfg(unix)]
    dir: std::fs::File,
}

/// A direct child of the anchored root, held open with the identity it names.
#[cfg(unix)]
struct AnchoredChild {
    dir: std::fs::File,
    identity: anchored_dir::DirIdentity,
}

impl AnchoredManagedRoot {
    /// `Ok(None)` when the root does not exist: there is nothing to scan and
    /// nothing to refuse.
    ///
    /// Every component below `<home>` is opened `O_NOFOLLOW`, so an ancestor
    /// symlink is a refusal rather than a redirection (TASK-RMA18, finding 4).
    #[cfg(unix)]
    fn open(home: &Home, project_id: &str) -> Result<Option<Self>> {
        // Validates the id as a single safe component, and produces the display
        // path. The walk below is what the removals actually resolve against.
        let path = managed_worktree_root(home, project_id)?;
        let trust_root = match anchored_dir::open_trust_root(&home.root) {
            Ok(dir) => dir,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(err) => bail!(
                "refusing to scan or prune the managed worktree root {}: its home directory {} \
                 could not be opened as a real directory: {err}",
                path.display(),
                home.root.display()
            ),
        };
        let mut dir = trust_root;
        for component in ["worktrees", project_id] {
            let name = std::ffi::OsStr::new(component);
            dir = match anchored_dir::open_child_dir(&dir, name)? {
                anchored_dir::ChildOpen::Dir(child) => child,
                anchored_dir::ChildOpen::Absent => return Ok(None),
                anchored_dir::ChildOpen::NotADirectory => {
                    // Name the shape rather than the errno: an operator whose
                    // `worktrees` directory is a symlink needs to hear exactly
                    // that, and it is the finding-4 case when `component` is an
                    // ancestor of the root rather than the root itself.
                    let component_path = if component == project_id {
                        path.clone()
                    } else {
                        home.root.join(component)
                    };
                    let shape = match std::fs::symlink_metadata(&component_path) {
                        Ok(meta) if meta.file_type().is_symlink() => format!(
                            "{} is a symlink, and a prune that followed it would scan and remove \
                             directories outside the root",
                            component_path.display()
                        ),
                        Ok(_) => format!("{} is not a directory", component_path.display()),
                        Err(err) => format!(
                            "{} could not be opened as a real directory: {err}",
                            component_path.display()
                        ),
                    };
                    bail!(
                        "refusing to scan or prune the managed worktree root {}: {shape}",
                        path.display()
                    )
                }
            };
        }
        Ok(Some(Self { path, dir }))
    }

    #[cfg(not(unix))]
    fn open(home: &Home, project_id: &str) -> Result<Option<Self>> {
        let path = managed_worktree_root(home, project_id)?;
        if !path.exists() {
            return Ok(None);
        }
        bail!(
            "refusing to scan or prune the managed worktree root {}: reclaiming a worktree needs \
             directory-handle removal, which is implemented for unix only",
            path.display()
        )
    }

    fn path(&self) -> &Path {
        &self.path
    }

    /// Entry names directly under the anchored root, read through the handle.
    #[cfg(unix)]
    fn child_names(&self) -> Result<Vec<std::ffi::OsString>> {
        anchored_dir::entry_names(&self.dir)
    }

    #[cfg(not(unix))]
    fn child_names(&self) -> Result<Vec<std::ffi::OsString>> {
        Ok(Vec::new())
    }

    /// Open a direct child and record the identity it names. `Ok(None)` when the
    /// entry is not a real directory — a file, or a symlink planted in the root,
    /// neither of which this verb classifies or removes.
    #[cfg(unix)]
    fn open_child(&self, name: &std::ffi::OsStr) -> Result<Option<AnchoredChild>> {
        match anchored_dir::open_child_dir(&self.dir, name)? {
            anchored_dir::ChildOpen::Dir(dir) => {
                let identity = anchored_dir::identity_of(&dir)?;
                Ok(Some(AnchoredChild { dir, identity }))
            }
            anchored_dir::ChildOpen::NotADirectory | anchored_dir::ChildOpen::Absent => Ok(None),
        }
    }

    /// Refuse unless `path` resolves, RIGHT NOW, to the identity this prune
    /// classified. The only fence available for the operations that must go
    /// through a pathname because they are `git` subprocesses.
    ///
    /// Note what this does NOT do any more: it does not compare strings against
    /// `<root>/<name>`, and it does not re-stat the root. A string comparison
    /// says nothing about which inode the string reaches, which is finding 5.
    #[cfg(unix)]
    fn assert_path_names(&self, path: &Path, identity: anchored_dir::DirIdentity) -> Result<()> {
        match anchored_dir::identity_of_path(path) {
            Some(found) if found == identity => Ok(()),
            Some(found) => bail!(
                "refusing to touch {}: it now resolves to a different directory ({found}) than \
                 the one this prune classified and reserved ({identity})",
                path.display()
            ),
            None => bail!(
                "refusing to touch {}: it no longer resolves to a real directory",
                path.display()
            ),
        }
    }

    #[cfg(not(unix))]
    fn assert_path_names(&self, path: &Path, _identity: ()) -> Result<()> {
        bail!("refusing to touch {}: unsupported platform", path.display())
    }

    /// Recursively remove a direct child, entirely through the anchored handle,
    /// and only while that entry still names `identity`.
    #[cfg(unix)]
    fn remove_child(
        &self,
        name: &std::ffi::OsStr,
        identity: anchored_dir::DirIdentity,
    ) -> std::result::Result<(), anchored_dir::RemovalFailure> {
        anchored_dir::remove_dir_all_at(&self.dir, name, identity)
    }
}

/// A directory found directly under the managed worktree root, and what may be
/// done with it.
#[derive(Clone, Debug)]
struct ManagedWorktree {
    /// Reported to the operator, and handed to the `git` subprocesses that
    /// cannot take a handle. NEVER the reference point for a decision: every
    /// use is fenced by re-proving the path resolves to `identity`.
    path: PathBuf,
    /// The entry name inside the anchored root. Removal is by NAME relative to
    /// the root handle.
    name: std::ffi::OsString,
    /// The `(device, inode)` this directory was when it was classified. This is
    /// what the daemon reservation is taken against and what the removal
    /// re-proves — the one thing that is the same at all three points
    /// (TASK-RMA18, finding 5).
    #[cfg(unix)]
    identity: anchored_dir::DirIdentity,
    disposition: WorktreeDisposition,
    /// This is an expired orgasmic chain lock. Actual prune releases it only
    /// after the daemon reservation and anchored identity checks are held.
    release_chain_hold: bool,
    /// Recursive size, measured only for reclaimable entries. Sizing a held
    /// worktree would put a multi-GB directory walk on the hot path of a status
    /// verb to inform no decision.
    bytes: Option<u64>,
    /// The first NESTED `.git` the size walk saw, relative to the worktree root
    /// — the `RepoGone` submodule signal, carried out of a walk that was
    /// already being paid for rather than earned by a second traversal
    /// (TASK-RMA18.1.1.1 finding A). `None` when the walk did not run or found
    /// none. See [`worktree_submodule_refusal`] for what consumes it and why
    /// only the `NoRepository` branch may.
    nested_git: Option<String>,
}

#[derive(Clone, Debug)]
enum WorktreeDisposition {
    /// No open dispatch names it and its repository answers: reclaimable by
    /// salvage followed by a removal this verb performs itself, refusing the
    /// same states a non-forced `git worktree remove` refuses.
    Unclaimed,
    /// The worktree's `.git` link names an admin directory that is gone, so
    /// there is no repository to salvage into. This case is NEW with the
    /// relocation — worktrees used to die with their repo — and it is
    /// reclaimable only by direct removal, with NO salvage possible.
    RepoGone { detail: String },
    /// Something still owns it. NEVER reclaimed, whatever the run's health: the
    /// authority to remove a dispatched worktree belongs to `dispatch-close`,
    /// which is also the only surface that knows the recorded branch a salvage
    /// commit must be parented on.
    Held { detail: String },
    /// The worktree could not be classified — an I/O failure that is not
    /// absence. NEVER reclaimed (TASK-M47E5.2 finding 3): an unreadable `.git`
    /// used to fall through to `RepoGone`, the one disposition that skips
    /// salvage and deletes, so a permission error destroyed a worker's
    /// uncommitted output with no salvage attempted.
    Undetermined { detail: String },
    /// The preliminary anchored walk could not completely traverse the tree.
    /// NEVER reclaimed: this is the fail-closed half of the same errors and
    /// depth limit [`anchored_dir::remove_contents`] propagates during removal.
    UnsafeTraversal { detail: String },
}

impl ManagedWorktree {
    fn reclaimable(&self) -> bool {
        disposition_is_reclaimable(&self.disposition)
    }

    fn why(&self) -> String {
        if self.release_chain_hold {
            return "chain hold has no pending implementer round".to_string();
        }
        match &self.disposition {
            WorktreeDisposition::Unclaimed => "no open dispatch names it".to_string(),
            WorktreeDisposition::RepoGone { detail } => {
                format!("repo gone ({detail}); removable but NOT salvageable")
            }
            WorktreeDisposition::Held { detail } => detail.clone(),
            WorktreeDisposition::Undetermined { detail } => {
                format!("repository state undetermined ({detail}); kept until it can be proven")
            }
            WorktreeDisposition::UnsafeTraversal { detail } => {
                format!(
                    "worktree traversal incomplete ({detail}); the whole worktree was skipped and \
                     nothing within it was deleted; make the offending descendant readable (for \
                     example with chmod) or remove it by hand, then re-run — this verb has no \
                     `--force` override"
                )
            }
        }
    }

    fn name(&self) -> String {
        self.name.to_string_lossy().to_string()
    }
}

/// Classify every directory under the ANCHORED managed worktree root, reading
/// each one through a HANDLE on it rather than through a pathname.
///
/// Three independent owners can hold an entry, and each is read from the
/// authority that actually knows: the process's own cwd, the daemon's live-run
/// map, and the tx ledger. The ledger used to be the sole ownership decision
/// with live-run data only decorating a record it already held — which is how a
/// live worker whose `WORKTREE` never reached the ledger classified as
/// UNCLAIMED (TASK-M47E5.2 finding 2). It is now one of three, and the
/// enforcement that matters is downstream of all of them: the daemon's own
/// cleanup reservation, taken per worktree in [`worktree_prune`].
///
/// Every one of those three comparisons is by IDENTITY first (TASK-RMA18,
/// finding 5). A recorded pathname is matched by asking which inode it reaches
/// now, so a worktree renamed under a live worker still matches the run that
/// occupies it, and a pathname rebound to some other directory stops matching
/// rather than transferring the claim. String comparison survives only as the
/// fallback for a recorded path that no longer resolves at all — which cannot
/// name this child, and so can only ever add a refusal.
// orgasmic:TASK-M47E5,TASK-M47E5.2,TASK-RMA18
#[cfg(unix)]
fn scan_managed_worktrees(
    root: &AnchoredManagedRoot,
    project_root: &Path,
    project_id: &str,
    live_runs: &[RunSummary],
) -> Result<Vec<ManagedWorktree>> {
    let open = scan_open_dispatches(project_root)?;
    let registrations = git_worktree_registrations(project_root)?;
    let cwd = std::env::current_dir().ok();
    let cwd_normalized = cwd.as_deref().map(normalize_path);

    let mut names = root.child_names()?;
    names.sort();

    let mut found = Vec::with_capacity(names.len());
    for name in names {
        // The handle is opened FIRST and everything below reads through it. A
        // symlink or a plain file planted in the root yields `None` here and is
        // never classified, never sized, and never removed.
        let Some(child) = root.open_child(&name)? else {
            continue;
        };
        let path = root.path().join(&name);
        let normalized = normalize_path(&path);
        let claims = |candidate: Option<&Path>| -> bool {
            let Some(candidate) = candidate else {
                return false;
            };
            match anchored_dir::identity_of_path(candidate) {
                Some(found) => found == child.identity,
                None => normalize_path(candidate) == normalized,
            }
        };
        let lock_reason = registrations
            .iter()
            .find(|registration| registration.path == normalized)
            .and_then(|registration| registration.lock_reason.as_deref());
        let expired_chain_hold = lock_reason.is_some_and(|reason| {
            reason.starts_with(CHAIN_WORKTREE_LOCK_PREFIX)
                && !chain_hold_has_pending_round(project_root, reason)
        });
        let mut disposition = if cwd_is_inside(
            cwd.as_deref(),
            cwd_normalized.as_deref(),
            &child,
            &normalized,
        ) {
            // Refuse the tree we are standing in before anything else. Nothing
            // downstream should have to be careful about this.
            WorktreeDisposition::Held {
                detail: "the current directory is inside it".to_string(),
            }
        } else if let Some(record) = open
            .iter()
            .find(|record| claims(record.worktree.as_deref()))
        {
            WorktreeDisposition::Held {
                detail: held_by_dispatch_detail(record, live_runs),
            }
        } else if let Some(run) = live_runs.iter().find(|run| {
            run.project_id.as_deref().is_none_or(|id| id == project_id)
                && claims(run.worktree.as_deref())
        }) {
            WorktreeDisposition::Held {
                detail: live_run_holds_detail(run),
            }
        } else if let Some(reason) = lock_reason.filter(|_| !expired_chain_hold) {
            WorktreeDisposition::Held {
                detail: if reason.starts_with(CHAIN_WORKTREE_LOCK_PREFIX) {
                    format!(
                        "held for the next implementer round ({reason}); a final implementer \
                         close releases it"
                    )
                } else if reason.is_empty() {
                    "git worktree lock holds it; unlock it before pruning".to_string()
                } else {
                    format!("git worktree lock holds it ({reason}); unlock it before pruning")
                },
            }
        } else {
            match worktree_repo_state(&child.dir, &path) {
                WorktreeRepoState::Present => WorktreeDisposition::Unclaimed,
                WorktreeRepoState::Gone(detail) => WorktreeDisposition::RepoGone { detail },
                WorktreeRepoState::Undetermined(detail) => {
                    WorktreeDisposition::Undetermined { detail }
                }
            }
        };
        // ONE walk, TWO answers, and it stays where it already was — see
        // [`walk_worktree`] and the ordering note in
        // [`worktree_submodule_refusal`].
        let walk = if disposition_is_reclaimable(&disposition) {
            match walk_worktree(&child.dir) {
                Ok(walk) => Some(walk),
                Err(err) => {
                    disposition = WorktreeDisposition::UnsafeTraversal {
                        detail: format!("{err:#}"),
                    };
                    None
                }
            }
        } else {
            None
        };
        found.push(ManagedWorktree {
            path,
            name,
            identity: child.identity,
            release_chain_hold: expired_chain_hold && disposition_is_reclaimable(&disposition),
            disposition,
            bytes: walk.as_ref().map(|walk| walk.bytes),
            nested_git: walk.and_then(|walk| walk.nested_git),
        });
    }
    Ok(found)
}

#[cfg(not(unix))]
fn scan_managed_worktrees(
    _root: &AnchoredManagedRoot,
    _project_root: &Path,
    _project_id: &str,
    _live_runs: &[RunSummary],
) -> Result<Vec<ManagedWorktree>> {
    Ok(Vec::new())
}

/// Is this process standing inside the child directory?
///
/// Identity answers it exactly when the cwd IS the worktree, which is the case
/// that matters and the one a rename cannot confuse. A cwd nested deeper has no
/// handle to compare, so the normalized-prefix test still carries that half —
/// it can only add a refusal, never authorise a removal.
#[cfg(unix)]
fn cwd_is_inside(
    cwd: Option<&Path>,
    cwd_normalized: Option<&Path>,
    child: &AnchoredChild,
    normalized: &Path,
) -> bool {
    if let Some(cwd) = cwd {
        if anchored_dir::identity_of_path(cwd) == Some(child.identity) {
            return true;
        }
    }
    cwd_normalized.is_some_and(|cwd| cwd.starts_with(normalized))
}

fn disposition_is_reclaimable(disposition: &WorktreeDisposition) -> bool {
    matches!(
        disposition,
        WorktreeDisposition::Unclaimed | WorktreeDisposition::RepoGone { .. }
    )
}

fn live_run_holds_detail(run: &RunSummary) -> String {
    format!(
        "live run {} occupies it{} [run-live] — the daemon names this worktree even though no \
         open dispatch record does; let the run finalize, or close it, then prune",
        run.run_id,
        run.task_id
            .as_deref()
            .map(|task| format!(" for {task}"))
            .unwrap_or_default()
    )
}

/// Say WHY an open dispatch holds a worktree, and — when its run is already
/// gone — which verb releases it. An abandoned dispatch is still a dispatch:
/// prune refuses it and points at the close, rather than growing a second way
/// to end a dispatch.
fn held_by_dispatch_detail(record: &DispatchRecord, live_runs: &[RunSummary]) -> String {
    let health = dispatch_health(record, live_runs);
    let tasks = task_list_property(&record.tasks);
    if health.run_alive || health.pid_alive {
        format!(
            "dispatch {} is open for {tasks} and its worker is alive [{}{}]",
            record.tx_id,
            if health.run_alive {
                "run-live"
            } else {
                "run-gone"
            },
            match pid_flag(&health) {
                "" => "",
                "[pid-alive]" => " pid-alive",
                _ => " pid-gone",
            }
        )
    } else {
        format!(
            "dispatch {} is open for {tasks} but its run is gone [run-gone pid-gone] — close it \
             first (`orgasmic manager dispatch-close --task {tasks} --started-tx {}` or \
             `orgasmic manager dispatch-status --cleanup-failed`), then prune",
            record.tx_id, record.tx_id
        )
    }
}

/// What can be PROVEN about a managed worktree's repository, kept separate from
/// what may be done about it (TASK-M47E5.2 finding 3).
///
/// The old shape returned `Option<String>` and so had nowhere to put "I could
/// not tell": every `read_to_string` failure fell through to `(!dot_git.is_dir())`,
/// which is false for an unreadable regular file, yielding "no .git link". A
/// permission error, an ACL, or a transient I/O failure therefore selected the
/// one disposition that skips salvage and deletes. Absence is now the only thing
/// that can be concluded from absence.
#[derive(Clone, Debug)]
enum WorktreeRepoState {
    /// A `.git` link or admin directory is there and its target resolves.
    Present,
    /// PROVEN absent: `.git` itself, or the admin directory it names, answered
    /// `NotFound`. The only state that authorises the unsalvaged removal.
    Gone(String),
    /// Something failed that is not absence. Evidence of nothing.
    Undetermined(String),
}

/// Classify a worktree's repository THROUGH THE HANDLE on that worktree.
///
/// `worktree` is the handle the caller classified and will remove; `path` is
/// only a label for the operator-facing detail and the base for a RELATIVE
/// `gitdir:` target. The target itself is read by pathname because it points
/// into the project repository, which is outside anything this verb anchors and
/// is never what it deletes.
// orgasmic:TASK-M47E5,TASK-M47E5.2,TASK-RMA18
#[cfg(unix)]
fn worktree_repo_state(worktree: &std::fs::File, path: &Path) -> WorktreeRepoState {
    use std::io::Read;

    let dot_git_name = std::ffi::OsStr::new(".git");
    let dot_git = path.join(".git");
    let kind = match anchored_dir::stat_at(worktree, dot_git_name) {
        Ok(kind) => kind,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
            return WorktreeRepoState::Gone("no .git link".to_string())
        }
        Err(err) => {
            return WorktreeRepoState::Undetermined(format!(
                "{} did not stat: {err}",
                dot_git.display()
            ))
        }
    };
    if kind.is_dir() {
        // The ordinary non-linked case: `.git` IS the admin directory.
        return WorktreeRepoState::Present;
    }
    if kind.is_symlink() {
        return WorktreeRepoState::Undetermined(format!(
            "{} is a symlink, so what it names cannot be read through this worktree's handle",
            dot_git.display()
        ));
    }
    // A linked worktree's `.git` is a FILE holding `gitdir: <admin dir>`.
    let mut contents = String::new();
    match anchored_dir::open_child_file(worktree, dot_git_name)
        .and_then(|mut file| file.read_to_string(&mut contents))
    {
        Ok(_) => {}
        Err(err) => {
            return WorktreeRepoState::Undetermined(format!(
                "{} did not read: {err}",
                dot_git.display()
            ))
        }
    }
    let Some(gitdir) = contents
        .lines()
        .find_map(|line| line.strip_prefix("gitdir:"))
        .map(|gitdir| gitdir.trim().to_string())
    else {
        // A `.git` that reads but does not parse is not an absent repository.
        return WorktreeRepoState::Present;
    };
    let resolved = if Path::new(&gitdir).is_absolute() {
        PathBuf::from(&gitdir)
    } else {
        path.join(&gitdir)
    };
    match std::fs::symlink_metadata(&resolved) {
        Ok(_) => WorktreeRepoState::Present,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
            WorktreeRepoState::Gone(format!("gitdir {gitdir} no longer exists"))
        }
        Err(err) => WorktreeRepoState::Undetermined(format!("gitdir {gitdir} did not stat: {err}")),
    }
}

/// What one anchored walk of a worktree learns.
#[cfg(unix)]
#[derive(Debug, Default)]
struct WorktreeWalk {
    /// Recursive apparent size in bytes.
    bytes: u64,
    /// The first NESTED entry named `.git`, relative to the worktree root, OF
    /// ANY TYPE.
    ///
    /// TYPE-AGNOSTIC ON PURPOSE, and the type is the trap: `git submodule
    /// update --init` inside a linked worktree writes the submodule's `.git` as
    /// a FILE holding `gitdir: ../../.git/modules/<name>` (measured on git
    /// 2.52.0), not as a directory. In the one state that consumes this — a
    /// worktree whose repository is GONE — that admin directory is gone with
    /// it, so the nested repository presents as a `.git` file pointing at
    /// nothing. A "populated `.git` DIRECTORY" predicate returns false on the
    /// single likeliest shape (TASK-RMA18.1.1.1, the reviewer's first
    /// correction to the C1 ruling).
    ///
    /// NESTED, so the worktree's OWN `.git` — always present at depth 1, and on
    /// the `RepoGone` path always the dangling link that put it there — is not
    /// its own refusal.
    nested_git: Option<String>,
}

/// Walk a worktree from its own HANDLE: recursive size, plus the nested-`.git`
/// signal the `RepoGone` refusal needs.
///
/// ONE traversal for both. The size walk was already paid for before the
/// reservation; the submodule signal rides along on it as a single `Option`
/// rather than earning a second full descent of a possibly multi-GB tree
/// (TASK-RMA18.1.1.1 finding A).
///
/// Symlinks are counted at their own size and never followed, so a link out of
/// the tree can neither inflate the number nor walk the machine — and because
/// the walk descends by `openat` rather than by pathname, a directory swapped
/// underneath it cannot redirect the walk out of the anchored tree either. That
/// is also what makes it a sound source for a REFUSAL and a `PathBuf`-based
/// walk would not be: a `.git` reported here was reached through
/// `O_NOFOLLOW|O_DIRECTORY` handles from the worktree root, so no symlink can
/// have pointed the name at something outside the tree.
///
/// A traversal error makes the worktree unsafe to reclaim. The automatic report
/// can still describe other worktrees, but this one is kept before deletion.
/// This is deliberately the same fail-closed depth and I/O policy
/// [`anchored_dir::remove_contents`] enforces if removal reaches the tree.
// orgasmic:TASK-RMA18,TASK-RMA18.1.1.1,TASK-GRCWC
#[cfg(unix)]
fn walk_worktree(root: &std::fs::File) -> Result<WorktreeWalk> {
    fn walk(dir: &std::fs::File, depth: u32, prefix: &str, found: &mut WorktreeWalk) -> Result<()> {
        if depth > anchored_dir::MAX_DEPTH {
            bail!(
                "refusing to descend deeper than {} directory levels while scanning {}",
                anchored_dir::MAX_DEPTH,
                if prefix.is_empty() { "." } else { prefix }
            );
        }
        let names = anchored_dir::entry_names(dir).with_context(|| {
            format!(
                "could not list worktree descendant {}",
                if prefix.is_empty() { "." } else { prefix }
            )
        })?;
        for name in names {
            if depth > 1 && found.nested_git.is_none() && name == std::ffi::OsStr::new(".git") {
                found.nested_git = Some(format!("{prefix}.git"));
            }
            match anchored_dir::open_child_dir(dir, &name) {
                Ok(anchored_dir::ChildOpen::Dir(child)) => {
                    walk(
                        &child,
                        depth + 1,
                        &format!("{prefix}{}/", name.to_string_lossy()),
                        found,
                    )?;
                }
                Ok(_) => {
                    if let Ok(kind) = anchored_dir::stat_at(dir, &name) {
                        found.bytes = found.bytes.saturating_add(kind.len());
                    }
                }
                Err(err) => {
                    return Err(err).with_context(|| {
                        format!(
                            "could not open worktree descendant {prefix}{}",
                            name.to_string_lossy()
                        )
                    });
                }
            }
        }
        Ok(())
    }
    let mut found = WorktreeWalk::default();
    walk(root, 1, "", &mut found)?;
    Ok(found)
}

/// Compact size for a `KEY=value` field, so it carries no space.
fn format_bytes(bytes: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KiB", "MiB", "GiB", "TiB"];
    let mut value = bytes as f64;
    let mut unit = 0;
    while value >= 1024.0 && unit + 1 < UNITS.len() {
        value /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{bytes}B")
    } else {
        format!("{value:.1}{}", UNITS[unit])
    }
}

/// The automatic half of the split: every `dispatch-status` names what
/// `worktree-prune` could reclaim. Best effort and never fatal — this is a
/// report appended to an inventory verb, not the inventory itself.
// orgasmic:TASK-M47E5,TASK-RMA18
fn report_managed_worktrees(
    home: &Home,
    project_root: &Path,
    project_id: &str,
    live_runs: &[RunSummary],
) {
    let root = match AnchoredManagedRoot::open(home, project_id) {
        Ok(Some(root)) => root,
        Ok(None) => return,
        Err(err) => {
            eprintln!("warning: could not scan managed worktrees: {err}");
            return;
        }
    };
    let found = match scan_managed_worktrees(&root, project_root, project_id, live_runs) {
        Ok(found) => found,
        Err(err) => {
            eprintln!("warning: could not scan managed worktrees: {err}");
            return;
        }
    };
    if found.is_empty() {
        return;
    }
    let mut total: u64 = 0;
    let mut count = 0usize;
    for worktree in &found {
        if worktree.reclaimable() {
            let bytes = worktree.bytes.unwrap_or(0);
            total = total.saturating_add(bytes);
            count += 1;
            println!(
                "RECLAIMABLE_WORKTREE PATH={} BYTES={bytes} SIZE={} WHY={}",
                worktree.path.display(),
                format_bytes(bytes),
                worktree.why()
            );
        } else {
            // Two different reasons not to reclaim, reported apart: somebody
            // owns it, versus nobody could tell (TASK-M47E5.2 finding 3).
            let line = match worktree.disposition {
                WorktreeDisposition::Undetermined { .. }
                | WorktreeDisposition::UnsafeTraversal { .. } => "KEPT_WORKTREE",
                _ => "HELD_WORKTREE",
            };
            println!(
                "{line} PATH={} WHY={}",
                worktree.path.display(),
                worktree.why()
            );
        }
    }
    if count > 0 {
        println!(
            "RECLAIMABLE_TOTAL COUNT={count} BYTES={total} SIZE={} RECLAIM_WITH=orgasmic manager worktree-prune",
            format_bytes(total)
        );
    }
}

/// `git worktree prune` in the project, so `.git/worktrees/<name>` admin
/// entries for directories removed out of band do not accumulate. Part 1 makes
/// that MORE likely, not less: the new root is a plausible thing for an
/// operator to `rm -rf`.
fn git_worktree_prune(project_root: &Path, dry_run: bool) -> Result<String> {
    let mut command = Command::new("git");
    command.args(["worktree", "prune", "-v"]);
    if dry_run {
        command.arg("--dry-run");
    }
    let output = command
        .current_dir(project_root)
        .output()
        .context("git worktree prune")?;
    if !output.status.success() {
        bail!(
            "git worktree prune failed: {}{}",
            String::from_utf8_lossy(&output.stderr),
            String::from_utf8_lossy(&output.stdout)
        );
    }
    // Measured against git 2.52.0: `-v` reports what it removed (or, with
    // --dry-run, would remove) on STDERR, and stdout stays empty. Reading
    // stdout here silently reports "pruned nothing" on every run.
    Ok(String::from_utf8_lossy(&output.stderr).trim().to_string())
}

fn worktree_head_oid(worktree: &Path) -> Option<String> {
    let output = Command::new("git")
        .args(["rev-parse", "--verify", "--quiet", "HEAD"])
        .current_dir(worktree)
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let oid = String::from_utf8_lossy(&output.stdout).trim().to_string();
    (!oid.is_empty()).then_some(oid)
}

/// The submodule refusal `git worktree remove` makes, reproduced from the
/// ANCHORED HANDLE on the worktree rather than from another subprocess.
///
/// `Some(reason)` means refuse. git's own test is `validate_no_submodules`
/// (`builtin/worktree.c`) and it is CATEGORICAL and CHECKED BEFORE CLEANLINESS:
/// a worktree containing an initialized submodule cannot be removed without
/// `--force`, however clean the parent reports it to be. TASK-RMA18 reproduced
/// only git's locked and unclean refusals, so this verb recursively deleted
/// trees git itself declines to touch.
///
/// Salvage cannot stand in for the refusal, which is why it is a refusal rather
/// than an extra salvage step: the parent's `git add -A` records a submodule as
/// a GITLINK, so files inside one are never captured by any salvage commit. And
/// a committed `submodule.<name>.ignore = all` — inherited by every clone —
/// makes a submodule holding uncommitted work report CLEAN to the parent, so
/// without this check the deletion happened with no salvage at all.
///
/// Both of git's branches are reproduced, in git's order:
///   1. the worktree's own admin directory holds a `modules/` directory — which
///      is what a `git submodule update --init` inside a LINKED worktree creates
///      (measured: `.git/worktrees/<id>/modules/<path>`), and which survives a
///      later `deinit` or an edit to `.gitmodules`;
///   2. a submodule working path is a NON-EMPTY directory (git: `!is_empty_dir`,
///      which is why an UNINITIALIZED submodule — an empty placeholder — is not
///      a refusal, matching git).
///
/// BRANCH 2'S SOURCE OF TRUTH IS THE INDEX, which is where git's own
/// `validate_no_submodules` reads it: it walks every cache entry and considers
/// only the ones whose mode is `S_IFGITLINK` (0160000). TASK-RMA18.1 read
/// `.gitmodules` instead, and TASK-RMA18.1.1 finding 1 measured the divergence
/// on the production path — a gitlink committed with `git update-index
/// --cacheinfo 160000` and populated by an ordinary standalone clone has no
/// `.gitmodules` entry and creates no admin `modules/` directory, so neither
/// implemented branch fired and the tree was deleted while git itself exits 128
/// on it. `.gitmodules` is still read ON TOP of the index, never instead of it:
/// it costs nothing and can only add refusals.
///
/// UNKNOWN MEANS KEEP. An index that should exist and does not open, does not
/// parse, or defers to a shared split index is a REFUSAL, not an empty one. The
/// one case with no index at all is a worktree with NO REPOSITORY BEHIND IT AT
/// ALL — a `RepoGone` orphan, where the admin directory the index lives in is
/// provably absent rather than unreadable. That case has no index for anything
/// to have been fail-open about and no verdict of git's to reproduce: git
/// cannot run `worktree remove` without the repository, so `RepoGone`
/// reclamation is a thing orgasmic invented and not a parity question at all.
///
/// SO `RepoGone` GETS ITS OWN, WEAKER TEST, and this is the whole of
/// TASK-RMA18.1.1.1 finding A. All three record-reading sources go quiet AT
/// ONCE there — no admin directory means no `modules/`, no index, and a
/// `.gitmodules` that a worker's standalone `git clone` never wrote — so the
/// shipped predicate returned `None` and a populated independent repository
/// holding uncommitted work was deleted on the ONE branch that removes with no
/// salvage at all. What replaces the record is the disk: `nested_git`, the
/// first nested entry named `.git` OF ANY TYPE that the size walk already saw
/// ([`WorktreeWalk`]). It is a heuristic and it is scoped to a state that only
/// arises once the repository has already disappeared; the cost asymmetry is
/// the one this project has already ruled on — a kept orphan is an operator
/// re-running a verb, a deleted one is a worker's unrecoverable output.
///
/// SCOPED TO `NoRepository` ONLY. Not `Unreadable`, not the ordinary path.
/// Wherever the index answers, git's own oracle is available and reproducing it
/// is the whole job; a vendored `.git` on disk that the index does not record
/// is git's business to permit, not this verb's to refuse.
///
/// `.gitmodules` IS STILL READ ON `RepoGone`, which does not contradict the
/// paragraph above: finding A's fixture had none, but the file itself lives in
/// the WORKTREE rather than in the admin directory, so wherever a tree has one
/// it survives the repository intact. So this predicate can refuse THREE ways on
/// that branch, not one — the walk's nested `.git`, a `.gitmodules` that exists
/// and does not read, and a `.gitmodules` naming a populated directory — and the
/// last of those needs a remedy of its own, because the `--force` escape the
/// ordinary branch offers cannot run once the repository is gone
/// (TASK-RMA18.1.1.1.1 findings 1 and 2).
///
/// ORDERING, stated because it was raised as a consequence and turns out not to
/// be one: the walk this consumes runs in [`scan_managed_worktrees`], which is
/// the FIRST thing `worktree_prune` does — before the cleanup lock, before the
/// reservation, and long before this refusal, which runs inside
/// [`reclaim_managed_worktree`] under the guard. Nothing was reordered, and
/// nothing needed to be. What that DOES cost is stated at the call site: the
/// boolean is a classification-time observation consumed under the guard.
// orgasmic:TASK-RMA18.1,TASK-RMA18.1.1,TASK-RMA18.1.1.1
#[cfg(unix)]
fn worktree_submodule_refusal(
    worktree: &std::fs::File,
    path: &Path,
    nested_git: Option<&str>,
) -> Option<String> {
    // THREE remedies, because one string was being appended to refusals that
    // are not about a submodule and to a branch where the escape it named
    // cannot run (TASK-RMA18.1.1.1 finding D, and the reviewer's second
    // correction). `--force` is offered conditionally in the first and not at
    // all in the third: this verb has no `--force` of its own, so every escape
    // here is something the operator does by hand.
    //
    // The `NoRepository` branch needs its own copy of the LAST refusal too, not
    // only of the nested-`.git` one: `.gitmodules` lives in the WORKTREE rather
    // than in the admin directory, so it survives a gone repository intact and
    // can still refuse there — and `submodule_advice`'s conditional escape names
    // a condition the operator cannot satisfy, because the repository being gone
    // is what put them on this branch (TASK-RMA18.1.1.1.1 finding 2). That
    // variant is written at its own call site below, where the module path it
    // must name is in hand.
    let submodule_advice =
        "no salvage can capture what is inside a submodule, because the parent records it as a \
         gitlink. Rescue its contents, then either remove the submodule's checkout yourself and \
         re-run, or — while the repository is still there — remove the worktree with \
         `git worktree remove --force`";
    let record_advice =
        "nothing here says there IS a submodule; only that the record which would say so could \
         not be read, and an unreadable record is not a licence to delete. Repair or remove what \
         did not read and re-run — this verb has no `--force` of its own";
    if worktree_admin_dir_holds_modules(worktree, path) {
        return Some(format!(
            "{} contains an initialized submodule (its worktree admin directory holds a \
             `modules` directory), and `git worktree remove` refuses such a worktree outright \
             — {submodule_advice}",
            path.display()
        ));
    }

    let mut candidates = Vec::new();
    let mut repo_gone = false;
    match worktree_index_gitlinks(worktree, path) {
        WorktreeIndexGitlinks::Recorded(paths) => candidates.extend(paths),
        WorktreeIndexGitlinks::NoRepository => {
            repo_gone = true;
            if let Some(nested) = nested_git {
                return Some(format!(
                    "{} has no repository behind it, so no index and no `modules` directory \
                     survive to say whether it contains a submodule — and the walk of its own \
                     contents found a nested repository (a submodule checkout, or a standalone \
                     clone a worker made itself) at {nested}. Its uncommitted work cannot be \
                     salvaged, because there is nothing left to salvage into, and this branch \
                     removes with NO salvage at all — so it is kept. `git worktree remove \
                     --force` CANNOT run here, since the repository it would run against is the \
                     one that is gone, and this verb has no `--force` of its own: rescue what \
                     you need, then delete {nested} yourself and re-run",
                    path.display()
                ));
            }
        }
        WorktreeIndexGitlinks::Unreadable(detail) => {
            return Some(format!(
                "the index that records whether {} contains a submodule could not be read \
                 ({detail}), and this verb does not delete a tree whose submodule record it \
                 cannot check — {record_advice}",
                path.display()
            ))
        }
    }
    match gitmodules_paths(worktree) {
        Ok(paths) => candidates.extend(paths),
        Err(detail) => {
            return Some(format!(
                "{} has a `.gitmodules` that could not be read ({detail}), and this verb does \
                 not delete a tree whose submodule record it cannot check — {record_advice}",
                path.display()
            ))
        }
    }
    candidates.sort();
    candidates.dedup();

    for module in candidates {
        if nonempty_dir_under(worktree, &module) {
            // WHICH branch got here decides the remedy, because one of them
            // cannot perform the escape the other offers. On `NoRepository` the
            // only record that can have produced this candidate is the
            // `.gitmodules` inside the tree, so the message names it — that is
            // also what distinguishes this refusal from the nested-`.git` one
            // above, which names the entry the walk found instead.
            if repo_gone {
                return Some(format!(
                    "{} has no repository behind it, so no index and no `modules` directory \
                     survive to say whether it contains a submodule — but its own `.gitmodules` \
                     does survive inside the tree, and names {module}, which this verb found to \
                     be a directory it could not list or listed as NON-EMPTY (it does not \
                     distinguish the two, and neither is a licence to delete). Its uncommitted \
                     work cannot be salvaged, because there is nothing left to salvage into, and \
                     this branch removes with NO salvage at all — so it is kept. `git worktree \
                     remove --force` CANNOT run here, since the repository it would run against \
                     is the one that is gone, and this verb has no `--force` of its own: rescue \
                     what you need, then remove {module} yourself and re-run",
                    path.display()
                ));
            }
            return Some(format!(
                "{} records a submodule at {module}, which this verb found to be a directory it \
                 could not list or listed as NON-EMPTY (it does not distinguish the two, and \
                 neither is a licence to delete), and `git worktree remove` refuses a worktree \
                 holding an initialized submodule outright — {submodule_advice}",
                path.display()
            ));
        }
    }
    None
}

/// What the worktree's OWN index says about gitlinks, read through the anchored
/// handle wherever the index is inside the anchored tree.
///
/// The index a linked worktree uses is its own, under the admin directory its
/// `.git` file names (`.git/worktrees/<id>/index`, measured on git 2.52.0) —
/// NOT the project repository's `.git/index`. Reading the wrong one is fail-open
/// in exactly the way this task exists to close, so the two cases are resolved
/// separately: when `.git` is a real DIRECTORY the index is `.git/index` and is
/// opened `openat`-relative to the worktree handle; when `.git` is a linked
/// worktree's `gitdir:` FILE the admin directory is outside anything this verb
/// anchors or deletes, and is resolved by pathname — the same boundary
/// [`worktree_repo_state`] and [`worktree_admin_dir_holds_modules`] already take.
///
/// A repository whose index file is simply ABSENT reports no gitlinks, because
/// that is exactly what git sees: git reads the index, and a missing index is an
/// empty one. Every other failure to reach or parse it is [`Unreadable`], which
/// the caller turns into a refusal.
///
/// [`Unreadable`]: WorktreeIndexGitlinks::Unreadable
// orgasmic:TASK-RMA18.1.1
#[cfg(unix)]
enum WorktreeIndexGitlinks {
    /// The index was read and parsed. These are its mode-0160000 paths.
    Recorded(Vec<String>),
    /// There is no repository behind this worktree, so there is no index. NOT
    /// the same as an index that could not be read.
    NoRepository,
    /// The index should exist and could not be opened or parsed. Refuse.
    Unreadable(String),
}

#[cfg(unix)]
fn worktree_index_gitlinks(worktree: &std::fs::File, path: &Path) -> WorktreeIndexGitlinks {
    use std::io::Read;

    let read_index = |mut file: std::fs::File| -> WorktreeIndexGitlinks {
        let mut bytes = Vec::new();
        if let Err(err) = file.read_to_end(&mut bytes) {
            return WorktreeIndexGitlinks::Unreadable(format!("index did not read: {err}"));
        }
        match index_gitlink_paths(&bytes) {
            Ok(paths) => WorktreeIndexGitlinks::Recorded(paths),
            Err(detail) => WorktreeIndexGitlinks::Unreadable(detail),
        }
    };

    let dot_git = std::ffi::OsStr::new(".git");
    let index = std::ffi::OsStr::new("index");
    let kind = match anchored_dir::stat_at(worktree, dot_git) {
        Ok(kind) => kind,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
            return WorktreeIndexGitlinks::NoRepository
        }
        Err(err) => {
            return WorktreeIndexGitlinks::Unreadable(format!(".git did not stat: {err}"));
        }
    };
    if kind.is_symlink() {
        return WorktreeIndexGitlinks::Unreadable(
            ".git is a symlink, so what it names cannot be read through this worktree's handle"
                .to_string(),
        );
    }
    if kind.is_dir() {
        let admin = match anchored_dir::open_child_dir(worktree, dot_git) {
            Ok(anchored_dir::ChildOpen::Dir(admin)) => admin,
            Ok(_) => {
                return WorktreeIndexGitlinks::Unreadable(
                    ".git did not open as a directory".to_string(),
                )
            }
            Err(err) => {
                return WorktreeIndexGitlinks::Unreadable(format!(".git did not open: {err}"))
            }
        };
        return match anchored_dir::open_child_file(&admin, index) {
            Ok(file) => read_index(file),
            // git reads the index; a repository that has never written one has
            // no entries and therefore no gitlinks.
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
                WorktreeIndexGitlinks::Recorded(Vec::new())
            }
            Err(err) => {
                WorktreeIndexGitlinks::Unreadable(format!(".git/index did not open: {err}"))
            }
        };
    }

    let mut contents = String::new();
    if let Err(err) = anchored_dir::open_child_file(worktree, dot_git)
        .and_then(|mut file| file.read_to_string(&mut contents))
    {
        return WorktreeIndexGitlinks::Unreadable(format!(".git did not read: {err}"));
    }
    let Some(gitdir) = contents
        .lines()
        .find_map(|line| line.strip_prefix("gitdir:"))
        .map(str::trim)
    else {
        return WorktreeIndexGitlinks::Unreadable(
            ".git is a file that names no `gitdir:`, so this worktree's index cannot be located"
                .to_string(),
        );
    };
    let resolved = if Path::new(gitdir).is_absolute() {
        PathBuf::from(gitdir)
    } else {
        path.join(gitdir)
    };
    match std::fs::symlink_metadata(&resolved) {
        Ok(_) => {}
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
            return WorktreeIndexGitlinks::NoRepository
        }
        Err(err) => {
            return WorktreeIndexGitlinks::Unreadable(format!(
                "gitdir {gitdir} did not stat: {err}"
            ))
        }
    }
    match std::fs::File::open(resolved.join("index")) {
        Ok(file) => read_index(file),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
            WorktreeIndexGitlinks::Recorded(Vec::new())
        }
        Err(err) => {
            WorktreeIndexGitlinks::Unreadable(format!("{gitdir}/index did not open: {err}"))
        }
    }
}

/// Every mode-0160000 path in a git index file, or why it could not be read.
///
/// Format: `DIRC`, a version, an entry count, then that many entries, then
/// extensions, then a trailing hash. Versions 2, 3 and 4 are the ones git
/// writes. The object hash length is NOT recorded in the header — a SHA-256
/// repository writes 32-byte object ids into the same layout — so both lengths
/// are tried and the one whose entries land EXACTLY on a well-formed extension
/// chain and trailing hash wins. Nothing is guessed: if neither length parses
/// cleanly this returns an error, and the caller refuses.
///
/// A SPLIT INDEX is refused rather than answered partially: its entries are a
/// delta against a shared index that is not this file, so the gitlinks visible
/// here are not the whole set. Measured on git 2.52.0, it is caught by BOTH of
/// the checks that can see it — a `link` extension in the extension chain, and
/// the empty-pathname entries git writes for the positions the shared index
/// still owns.
///
/// The FIRST attempt's error is the one reported: SHA-1 is git's default, so
/// when both lengths fail its diagnosis is the one that describes the file.
// orgasmic:TASK-RMA18.1.1
#[cfg(unix)]
fn index_gitlink_paths(bytes: &[u8]) -> std::result::Result<Vec<String>, String> {
    if bytes.len() < 12 || &bytes[..4] != b"DIRC" {
        return Err("not a git index file".to_string());
    }
    let version = u32::from_be_bytes([bytes[4], bytes[5], bytes[6], bytes[7]]);
    if !(2..=4).contains(&version) {
        return Err(format!("unsupported git index version {version}"));
    }
    let count = u32::from_be_bytes([bytes[8], bytes[9], bytes[10], bytes[11]]) as usize;

    let mut first: Option<String> = None;
    for hash_len in [20usize, 32usize] {
        match index_entries(bytes, version, count, hash_len) {
            Ok(paths) => return Ok(paths),
            Err(detail) => {
                if first.is_none() {
                    first = Some(detail);
                }
            }
        }
    }
    Err(first.unwrap_or_else(|| "index did not parse".to_string()))
}

/// One parse attempt at a fixed object-hash length. Errors on ANY inconsistency,
/// which is what makes trying both hash lengths sound.
#[cfg(unix)]
fn index_entries(
    bytes: &[u8],
    version: u32,
    count: usize,
    hash_len: usize,
) -> std::result::Result<Vec<String>, String> {
    const GITLINK_MODE: u32 = 0o160000;

    let malformed = |what: &str| format!("malformed index ({what})");
    let end = bytes
        .len()
        .checked_sub(hash_len)
        .ok_or_else(|| malformed("shorter than its trailing hash"))?;
    let mut off = 12usize;
    let mut previous: Vec<u8> = Vec::new();
    let mut gitlinks = Vec::new();

    for _ in 0..count {
        let start = off;
        let fixed = 40 + hash_len + 2;
        if off + fixed > end {
            return Err(malformed("entry runs past the end"));
        }
        let mode = u32::from_be_bytes([
            bytes[off + 24],
            bytes[off + 25],
            bytes[off + 26],
            bytes[off + 27],
        ]);
        let flags = u16::from_be_bytes([bytes[off + 40 + hash_len], bytes[off + 41 + hash_len]]);
        off += fixed;
        if flags & 0x4000 != 0 {
            if version < 3 {
                return Err(malformed("extended flags in a version-2 entry"));
            }
            if off + 2 > end {
                return Err(malformed("extended flags run past the end"));
            }
            off += 2;
        }

        let name = if version < 4 {
            let nul = bytes[off..end]
                .iter()
                .position(|byte| *byte == 0)
                .ok_or_else(|| malformed("unterminated path"))?;
            let name = bytes[off..off + nul].to_vec();
            let declared = (flags & 0x0FFF) as usize;
            if declared != 0x0FFF && declared != name.len() {
                return Err(malformed("path length disagrees with the entry flags"));
            }
            off += nul + 1;
            // git pads every v2/v3 entry with NULs to a multiple of 8.
            while !(off - start).is_multiple_of(8) {
                if off >= end {
                    return Err(malformed("padding runs past the end"));
                }
                if bytes[off] != 0 {
                    return Err(malformed("entry padding is not NUL"));
                }
                off += 1;
            }
            name
        } else {
            // Version 4 prefix-compresses paths: a varint saying how many bytes
            // to strip off the END of the previous path, then the new suffix,
            // NUL-terminated, with no padding.
            let (strip, consumed) = index_varint(&bytes[off..end])
                .ok_or_else(|| malformed("truncated path-compression varint"))?;
            off += consumed;
            let nul = bytes[off..end]
                .iter()
                .position(|byte| *byte == 0)
                .ok_or_else(|| malformed("unterminated path"))?;
            let keep = previous
                .len()
                .checked_sub(strip)
                .ok_or_else(|| malformed("path compression strips more than the previous path"))?;
            let mut name = previous[..keep].to_vec();
            name.extend_from_slice(&bytes[off..off + nul]);
            off += nul + 1;
            name
        };
        if name.is_empty() {
            // Measured on git 2.52.0: this is how a SPLIT INDEX marks the
            // positions its shared index still owns. Either way the file does
            // not describe the whole tree.
            return Err(
                "this index has an entry with no path, which is how a SPLIT INDEX defers to a \
                 shared index this cannot see"
                    .to_string(),
            );
        }
        if mode & 0o170000 == GITLINK_MODE {
            let name = String::from_utf8(name.clone())
                .map_err(|_| malformed("gitlink path is not valid UTF-8"))?;
            gitlinks.push(name);
        }
        previous = name;
    }

    // Whatever follows the entries must be a well-formed extension chain ending
    // exactly at the trailing hash. This is the check that makes picking the
    // object-hash length by trial sound rather than a guess.
    while off < end {
        if off + 8 > end {
            return Err(malformed("extension header runs past the end"));
        }
        let signature = &bytes[off..off + 4];
        if !signature.iter().all(|byte| byte.is_ascii_alphanumeric()) {
            return Err(malformed("extension signature is not alphanumeric"));
        }
        if signature == b"link" {
            return Err(
                "this worktree uses a SPLIT INDEX, so the gitlinks in its own index are not the \
                 whole set"
                    .to_string(),
            );
        }
        let size = u32::from_be_bytes([
            bytes[off + 4],
            bytes[off + 5],
            bytes[off + 6],
            bytes[off + 7],
        ]) as usize;
        off = off
            .checked_add(8 + size)
            .ok_or_else(|| malformed("extension length overflows"))?;
        if off > end {
            return Err(malformed("extension runs past the end"));
        }
    }
    if off != end {
        return Err(malformed("entries do not end at the trailing hash"));
    }
    Ok(gitlinks)
}

/// git's `decode_varint`, used by index version 4 for path compression.
#[cfg(unix)]
fn index_varint(bytes: &[u8]) -> Option<(usize, usize)> {
    let mut value: usize = 0;
    for (read, byte) in bytes.iter().enumerate() {
        if read == 0 {
            value = (*byte & 0x7f) as usize;
        } else {
            value = value.checked_add(1)?.checked_mul(128)? + (*byte & 0x7f) as usize;
        }
        if *byte & 0x80 == 0 {
            return Some((value, read + 1));
        }
        if read >= 9 {
            return None;
        }
    }
    None
}

/// git's branch 1: `is_directory(worktree_git_path(wt, "modules"))`.
///
/// `.git` is read through the worktree's own handle. When it is a real directory
/// the `modules` lookup is fd-relative too; when it is a linked worktree's
/// `gitdir:` FILE the target is resolved by pathname, because it points into the
/// project repository — outside anything this verb anchors and never what it
/// deletes. That is the same boundary [`worktree_repo_state`] already takes.
#[cfg(unix)]
fn worktree_admin_dir_holds_modules(worktree: &std::fs::File, path: &Path) -> bool {
    use std::io::Read;

    let dot_git = std::ffi::OsStr::new(".git");
    let modules = std::ffi::OsStr::new("modules");
    let Ok(kind) = anchored_dir::stat_at(worktree, dot_git) else {
        return false;
    };
    if kind.is_dir() {
        return match anchored_dir::open_child_dir(worktree, dot_git) {
            Ok(anchored_dir::ChildOpen::Dir(admin)) => anchored_dir::stat_at(&admin, modules)
                .map(|kind| kind.is_dir())
                .unwrap_or(false),
            _ => false,
        };
    }
    if kind.is_symlink() {
        return false;
    }
    let mut contents = String::new();
    if anchored_dir::open_child_file(worktree, dot_git)
        .and_then(|mut file| file.read_to_string(&mut contents))
        .is_err()
    {
        return false;
    }
    let Some(gitdir) = contents
        .lines()
        .find_map(|line| line.strip_prefix("gitdir:"))
        .map(str::trim)
    else {
        return false;
    };
    let resolved = if Path::new(gitdir).is_absolute() {
        PathBuf::from(gitdir)
    } else {
        path.join(gitdir)
    };
    resolved.join("modules").is_dir()
}

/// `submodule.<name>.path` values from the worktree's `.gitmodules`, read
/// through the anchored handle.
///
/// An ABSENT file records nothing and is `Ok(none)`. A file that is there and
/// cannot be read is an ERROR, which the caller turns into a refusal: this is a
/// destructive path, and a `.gitmodules` that exists but did not open is exactly
/// the "unknown" that must not read as "no submodules" (TASK-RMA18.1.1).
// orgasmic:TASK-RMA18.1.1
#[cfg(unix)]
fn gitmodules_paths(worktree: &std::fs::File) -> std::result::Result<Vec<String>, String> {
    use std::io::Read;

    let name = std::ffi::OsStr::new(".gitmodules");
    match anchored_dir::stat_at(worktree, name) {
        Ok(kind) if kind.is_dir() => {
            return Err(".gitmodules is a directory, not a file".to_string())
        }
        Ok(kind) if kind.is_symlink() => {
            return Err(
                ".gitmodules is a symlink, so what it names cannot be read through this \
                 worktree's handle"
                    .to_string(),
            )
        }
        Ok(_) => {}
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(err) => return Err(format!(".gitmodules did not stat: {err}")),
    }
    let mut contents = String::new();
    if let Err(err) = anchored_dir::open_child_file(worktree, name)
        .and_then(|mut file| file.read_to_string(&mut contents))
    {
        return Err(format!(".gitmodules did not read: {err}"));
    }
    let mut paths = Vec::new();
    let mut in_submodule = false;
    for line in contents.lines() {
        let line = line.trim();
        if let Some(header) = line.strip_prefix('[') {
            in_submodule = header
                .split(|c: char| c.is_whitespace() || c == ']')
                .next()
                .is_some_and(|section| section.eq_ignore_ascii_case("submodule"));
            continue;
        }
        if !in_submodule {
            continue;
        }
        let Some((key, value)) = line.split_once('=') else {
            continue;
        };
        if key.trim().eq_ignore_ascii_case("path") {
            let value = value.trim().trim_matches('"');
            if !value.is_empty() {
                paths.push(value.to_string());
            }
        }
    }
    Ok(paths)
}

/// git's branch 2: is `rel` a NON-EMPTY directory inside this worktree?
///
/// Walked component by component with `openat(O_NOFOLLOW)` from the worktree's
/// handle, so a `.gitmodules` that names `../..` or routes through a symlink
/// cannot make this look outside the tree. Anything that does not resolve to a
/// plain directory is not a submodule checkout and is not a refusal; anything
/// that resolves but cannot be listed IS a refusal, because "it might be empty"
/// is not a licence to delete it.
#[cfg(unix)]
fn nonempty_dir_under(worktree: &std::fs::File, rel: &str) -> bool {
    let mut dir = match worktree.try_clone() {
        Ok(dir) => dir,
        Err(_) => return false,
    };
    for component in rel.split('/') {
        if component.is_empty() || component == "." {
            continue;
        }
        if component == ".." {
            return false;
        }
        dir = match anchored_dir::open_child_dir(&dir, std::ffi::OsStr::new(component)) {
            Ok(anchored_dir::ChildOpen::Dir(child)) => child,
            _ => return false,
        };
    }
    match anchored_dir::entry_names(&dir) {
        Ok(names) => !names.is_empty(),
        // Listed nothing because the listing FAILED. Refuse.
        Err(_) => true,
    }
}

/// Does `git` consider this worktree removable without `--force`?
///
/// This is the check `git worktree remove` performs for itself, performed here
/// instead, because TASK-RMA18 finding 5 took the RECURSIVE DELETION away from
/// git: `git worktree remove` is itself the destructive remover and it receives
/// only a pathname, re-resolved inside a subprocess this process cannot fence,
/// with no child inode ever captured or passed. There was no way to prove the
/// tree git deleted was the tree this verb classified and reserved. So git keeps
/// the two jobs it is uniquely able to do — deciding whether the tree is clean,
/// and clearing the admin entry afterwards via `git worktree prune` — and the
/// removal itself happens through the anchored handle.
///
/// All three refusals git makes without `--force` are reproduced, in git's own
/// order: a LOCKED worktree, a worktree containing an INITIALIZED SUBMODULE (see
/// [`worktree_submodule_refusal`] — TASK-RMA18 omitted this one, which is what
/// TASK-RMA18.1 exists for), and an UNCLEAN one. The clean check passes
/// `--ignore-submodules=none` exactly as git's own `remove_cmd` does, so a
/// committed `submodule.<name>.ignore = all` cannot make a dirty tree read
/// clean here.
// orgasmic:TASK-RMA18,TASK-RMA18.1
#[cfg(unix)]
fn git_would_remove_worktree(
    project_root: &Path,
    worktree: &Path,
    handle: &std::fs::File,
) -> Result<()> {
    let listed = Command::new("git")
        .args(["worktree", "list", "--porcelain"])
        .current_dir(project_root)
        .output()
        .context("git worktree list --porcelain")?;
    if !listed.status.success() {
        bail!(
            "git worktree list failed: {}",
            String::from_utf8_lossy(&listed.stderr).trim()
        );
    }
    let listed = String::from_utf8_lossy(&listed.stdout);
    let wanted = normalize_path(worktree);
    let mut locked = false;
    let mut registered = false;
    for record in listed.split("\n\n") {
        let Some(path) = record
            .lines()
            .find_map(|line| line.strip_prefix("worktree "))
        else {
            continue;
        };
        if normalize_path(Path::new(path.trim())) != wanted {
            continue;
        }
        registered = true;
        locked = record
            .lines()
            .any(|line| line.trim_start().starts_with("locked"));
    }
    if !registered {
        bail!(
            "{} is not a registered worktree of {}",
            worktree.display(),
            project_root.display()
        );
    }
    if locked {
        bail!(
            "{} is locked; unlock it (`git worktree unlock`) before it can be reclaimed",
            worktree.display()
        );
    }
    // BEFORE cleanliness, as git does — and categorical, so no amount of clean
    // makes it removable (TASK-RMA18.1 finding 1).
    //
    // No nested-`.git` signal, and none is wanted: everything above this line
    // already read the repository (`git worktree list` named this worktree), so
    // the index answers and git's own oracle is what gets reproduced. The
    // disk fallback exists only where that oracle is gone.
    if let Some(reason) = worktree_submodule_refusal(handle, worktree, None) {
        bail!("{reason}");
    }
    // `--ignore-submodules=none` is what git's own `remove_cmd` passes. Without
    // it a committed `submodule.<name>.ignore = all` hides a dirty submodule and
    // this reads CLEAN (measured on git 2.52.0).
    let status = Command::new("git")
        .args(["status", "--porcelain", "--ignore-submodules=none"])
        .current_dir(worktree)
        .output()
        .context("git status --porcelain --ignore-submodules=none")?;
    if !status.status.success() {
        bail!(
            "git status failed: {}",
            String::from_utf8_lossy(&status.stderr).trim()
        );
    }
    let dirty = String::from_utf8_lossy(&status.stdout);
    if !dirty.trim().is_empty() {
        bail!(
            "{} still contains modified or untracked files:\n{}",
            worktree.display(),
            dirty.trim()
        );
    }
    Ok(())
}

/// Reclaim one worktree: salvage first, then a removal that refuses everything
/// a non-forced `git worktree remove` refuses — so a tree git still considers
/// dirty, locked, or submodule-bearing survives and is reported instead of
/// destroyed.
///
/// Called only while this process holds the daemon's cleanup reservation for
/// this worktree, and that reservation was taken against `worktree.identity`.
/// THE IDENTITY CLASSIFIED, THE IDENTITY RESERVED AND THE IDENTITY DELETED ARE
/// THE SAME ONE (TASK-RMA18, finding 5): the removal goes through the anchored
/// handle and re-proves that identity at the `unlinkat`, so no rename or relink
/// can redirect what is destroyed.
///
/// THE WINDOW THAT REMAINS, stated rather than papered over (TASK-RMA18.1
/// finding 2, CORRECTED by TASK-RMA18.1.1 finding 3 — the correction was to the
/// STATEMENT, not to the code, because the narrower guarantee the old wording
/// claimed is not one this shape can offer). Every `git` step below is a
/// SUBPROCESS and a subprocess can only be given a PATHNAME, which it resolves
/// itself, inside itself, after this process has stopped looking.
/// `assert_path_names` proves the pathname reaches the classified identity AT
/// THE MOMENT IT RUNS, and it runs at three points here: before the
/// dirty-or-clean read, before salvage, and before `git_would_remove_worktree`.
/// It does NOT run before every path-resolving `execve`, and the real windows
/// are therefore wider than one assert-to-`execve` gap:
///   - `worktree_head_oid` runs `git rev-parse` after the assert that preceded
///     `worktree_has_uncommitted_changes`, with no assert of its own;
///   - `salvage_worktree_onto` runs `add`, `write-tree`, `commit-tree`,
///     `diff-tree`, `checkout --detach`, a second status and the salvage-ref
///     commands behind ONE assert;
///   - `git_would_remove_worktree` runs `git worktree list` FIRST and resolves
///     the worktree pathname only afterwards, for `git status` — so an actor
///     does not need to hit a narrow gap at all. It can wait out the whole
///     `worktree list` subprocess, bind a clean decoy for the `status` call, and
///     bind the original back before the final assert.
///
/// What all of that buys the actor is bounded, and that bound is the real
/// mitigation:
///   - it CANNOT redirect the deletion, which never resolves a pathname — the
///     `unlinkat` goes through the anchored handle and re-proves the identity;
///   - it CAN make the CLEAN CHECK — described below as the last gate between
///     this verb and a worker's unrecoverable output — answer about a tree
///     other than the one deleted, so the inode removed can be a dirty,
///     unsalvaged one;
///   - it CAN make the salvage commit the decoy's contents instead.
///
/// Closing it needs git to accept a directory handle, which `git worktree` does
/// not offer (TASK-RMA18 ruled on that), or the clean decision to be computed
/// here from the handle the way the submodule refusal now is. Adding an assert
/// before each subprocess would not close it either — the third case above is a
/// window BETWEEN two subprocesses of one call — so the asserts stay where they
/// are and this comment describes what they actually cover.
// orgasmic:TASK-M47E5,TASK-M47E5.2,TASK-RMA18,TASK-RMA18.1,TASK-RMA18.1.1
#[cfg(unix)]
fn reclaim_managed_worktree(
    project_root: &Path,
    root: &AnchoredManagedRoot,
    worktree: &ManagedWorktree,
) -> WorktreeRemovalOutcome {
    let kept = |error: String| WorktreeRemovalOutcome {
        removed: false,
        touched: false,
        salvage: None,
        error: Some(error),
        report_error: None,
        report_path: None,
    };

    // Re-open the child through the root HANDLE and re-prove its identity before
    // anything else. Between classification and here the reservation round-trip
    // and a size walk have happened; this is where a substituted child stops.
    let child = match root.open_child(&worktree.name) {
        Ok(Some(child)) if child.identity == worktree.identity => child,
        Ok(Some(other)) => {
            return kept(format!(
                "{:?} now names a different directory ({}) than the one classified and reserved \
                 ({}); kept",
                worktree.name, other.identity, worktree.identity
            ))
        }
        Ok(None) => {
            return kept(format!(
                "{:?} is no longer a real directory under the anchored root; kept",
                worktree.name
            ))
        }
        Err(err) => {
            return kept(format!(
                "could not re-open it under the anchored root: {err}"
            ))
        }
    };

    // Dispatch worktrees carry initialized submodules (`create_worktree` runs
    // `git submodule update --init --recursive`), and the categorical refusal
    // below would keep every one of them forever. Settle them first: a
    // submodule that provably holds nothing of the worker's is deinited and
    // the worktree's private object store cleared, returning the tree to the
    // shape a non-forced `git worktree remove` accepts. ANY doubt leaves the
    // tree untouched and the refusal below reports it. Skipped on `RepoGone`
    // (no repository, no git to ask) and fenced like every other subprocess
    // step, because settle hands the pathname to `git`.
    if !matches!(worktree.disposition, WorktreeDisposition::RepoGone { .. })
        && root
            .assert_path_names(&worktree.path, worktree.identity)
            .is_ok()
    {
        settle_as_initialized_submodules(&worktree.path);
    }

    // The submodule refusal is CATEGORICAL, and it runs ABOVE EVERY DESTRUCTIVE
    // BRANCH — including `RepoGone`, which returns straight through
    // `remove_child` below and is the ONE branch that deletes with no salvage at
    // all. It used to sit under that early return, so it guarded only the
    // `Unclaimed` path and a repo-gone worktree holding an ordinary populated
    // submodule was deleted outright (TASK-RMA18.1.1 finding 2).
    //
    // It also runs BEFORE the salvage, not only inside `git_would_remove_worktree`
    // after it. Salvage cannot capture anything inside a submodule (the parent
    // records a gitlink), so running it first would mutate a tree that is going
    // to be kept anyway — staging its index and detaching its HEAD — for no gain.
    // Computed from the HANDLE, so this one decision needs no pathname and no
    // subprocess.
    //
    // `nested_git` is the ONE input here that was measured EARLIER, in the
    // classification walk, rather than now under the guard — that is what buys
    // the `RepoGone` fallback for a single boolean instead of a second full
    // descent (TASK-RMA18.1.1.1 finding A). It describes the same IDENTITY the
    // handle above was just re-proven to be, so it cannot have drifted onto
    // some other directory; what it can be is STALE about that directory's
    // contents. A nested repository created after the scan is not seen —
    // residual, and stated rather than papered over. It runs in the safe
    // direction for the reverse case: one deleted after the scan only produces
    // a refusal, and the operator re-runs.
    if let Some(reason) =
        worktree_submodule_refusal(&child.dir, &worktree.path, worktree.nested_git.as_deref())
    {
        return kept(format!("refusing to remove it: {reason}"));
    }

    if let WorktreeDisposition::RepoGone { .. } = worktree.disposition {
        // TASK-M47E5.2 finding 3: classification happened before the multi-GB
        // size walk and before the reservation, and this is the ONE path that
        // deletes without salvaging. Ask the repository again, under the guard
        // and THROUGH THE RE-PROVEN HANDLE, immediately before the removal — an
        // unreadable-then-restored gitdir must not lose a worker's work to a
        // stale verdict.
        match worktree_repo_state(&child.dir, &worktree.path) {
            WorktreeRepoState::Gone(_) => {}
            WorktreeRepoState::Present => {
                return kept(
                    "the repository became reachable again between classification and removal, \
                     so this is no longer an unsalvageable orphan; kept"
                        .to_string(),
                );
            }
            WorktreeRepoState::Undetermined(detail) => {
                return kept(format!(
                    "repository state undetermined under the guard ({detail}); kept rather than \
                     removed without salvage"
                ));
            }
        }
        drop(child);
        return removal_outcome(root.remove_child(&worktree.name, worktree.identity), None);
    }

    // Everything below hands `worktree.path` to a `git` subprocess, which cannot
    // take a handle. This is the fence for all of it: the path resolves to the
    // classified and reserved identity right now, so git is looking at the same
    // tree the removal below will delete through the anchor. The doc comment
    // above enumerates the windows it leaves open (TASK-RMA18.1.1.1 finding C).
    if let Err(err) = root.assert_path_names(&worktree.path, worktree.identity) {
        return kept(err.to_string());
    }

    let salvage = match worktree_has_uncommitted_changes(&worktree.path) {
        Ok(false) => None,
        Ok(true) => match worktree_head_oid(&worktree.path) {
            Some(parent) => {
                if let Err(err) = root.assert_path_names(&worktree.path, worktree.identity) {
                    return kept(err.to_string());
                }
                match salvage_worktree_onto(project_root, &worktree.path, &worktree.name(), &parent)
                {
                    Ok(salvage) => salvage,
                    // NOT "untouched". `salvage_worktree_onto` mutates in four
                    // steps — `git add -A`, `write-tree`, `commit-tree`,
                    // `checkout --detach`, `anchor_salvage_ref` — and a failure
                    // at any of them after the first leaves the tree staged and
                    // possibly detached at a salvage commit. Nothing is
                    // DESTROYED, which is why `touched` stays false, but the
                    // operator must not read KEPT as "exactly as the worker left
                    // it" (TASK-RMA18.1 finding 7).
                    Err(err) => {
                        return kept(format!(
                            "salvage failed, worktree kept and NOTHING DELETED — but the salvage \
                             had already started, so its index is staged and its HEAD may be \
                             detached at a salvage commit; inspect it before re-running: {err}"
                        ))
                    }
                }
            }
            None => {
                return kept(
                    "worktree is dirty and its HEAD does not resolve, so its contents cannot be \
                     salvaged; kept"
                        .to_string(),
                )
            }
        },
        Err(err) => return kept(format!("could not read worktree status: {err}")),
    };

    // The last gate between this verb and a worker's unrecoverable output, and
    // the same one `dispatch-close` relies on: git's own clean check. Salvage
    // leaves the tree detached at the salvage commit and therefore CLEAN, so a
    // tree that is still dirty here is one salvage could not capture.
    if let Err(err) = root.assert_path_names(&worktree.path, worktree.identity) {
        return WorktreeRemovalOutcome {
            removed: false,
            touched: false,
            salvage,
            error: Some(err.to_string()),
            report_error: None,
            report_path: None,
        };
    }
    if let Err(err) = git_would_remove_worktree(project_root, &worktree.path, &child.dir) {
        return WorktreeRemovalOutcome {
            removed: false,
            touched: false,
            salvage,
            error: Some(format!("refusing to remove it: {err}")),
            report_error: None,
            report_path: None,
        };
    }
    // Re-prove after the subprocesses: `git` ran, time passed, and this is the
    // last statement before anything is destroyed.
    if let Err(err) = root.assert_path_names(&worktree.path, worktree.identity) {
        return WorktreeRemovalOutcome {
            removed: false,
            touched: false,
            salvage,
            error: Some(err.to_string()),
            report_error: None,
            report_path: None,
        };
    }
    drop(child);
    removal_outcome(
        root.remove_child(&worktree.name, worktree.identity),
        salvage,
    )
}

/// Deinit every submodule that is verifiably still AS-INITIALIZED and, when
/// they all are, delete the worktree's private submodule object store
/// (`<gitdir>/modules/`) — the two things `git worktree remove`'s submodule
/// refusal fires on. "As-initialized" is proven per submodule, and ALL FOUR
/// proofs must hold for EVERY listed submodule or nothing at all is touched:
///   1. `git submodule status` flag ` ` — checked-out HEAD equals the gitlink
///      the parent's index records (`+`/`U` refuse; `-` is an uninitialized
///      placeholder with nothing to settle);
///   2. empty `status --porcelain --ignore-submodules=none` inside it — no
///      modified, untracked, or staged files;
///   3. no local-only history — every local ref is reachable from the origin's
///      refs (`log --all --not --remotes` empty). The clone `--init` makes
///      always carries the origin's default branch, so "no branches" would
///      never hold; what must not exist is a COMMIT the origin doesn't have;
///   4. no stash (stashes live outside `--all`).
///
/// Only then is the store discardable: it holds nothing beyond what `--init`
/// fetched from the origin. UNKNOWN MEANS KEEP, same as the refusal this
/// feeds: any git failure, parse surprise, or failed proof returns without
/// touching anything, and `worktree_submodule_refusal` then reports the tree
/// exactly as before. A gitlink recorded only in the index (no `.gitmodules`)
/// makes `git submodule status` fail, so that shape is untouched here too.
#[cfg(unix)]
fn settle_as_initialized_submodules(worktree: &Path) {
    let Ok(status) = git_capture(worktree, ["submodule", "status"]) else {
        return;
    };
    let mut initialized = Vec::new();
    for line in status.lines() {
        // `<flag><oid> <path>[ (<ref>)]`. The in-sync flag is a SPACE, and
        // `git_capture` trims the output, so a line that opens with an oid
        // character IS the in-sync case; only `-`/`+`/`U` survive the trim.
        let (flag, rest) = match line.chars().next() {
            None => continue,
            Some(flag @ ('-' | '+' | 'U')) => (flag, &line[1..]),
            Some(_) => (' ', line),
        };
        let Some(path) = rest.split_whitespace().nth(1) else {
            return;
        };
        match flag {
            '-' => {}
            ' ' => initialized.push(path.to_string()),
            _ => return,
        }
    }
    if initialized.is_empty() {
        return;
    }
    for sub in &initialized {
        let dir = worktree.join(sub);
        match git_capture(&dir, ["status", "--porcelain", "--ignore-submodules=none"]) {
            Ok(out) if out.is_empty() => {}
            _ => return,
        }
        match git_capture(&dir, ["log", "--oneline", "--all", "--not", "--remotes"]) {
            Ok(out) if out.is_empty() => {}
            _ => return,
        }
        match git_capture(&dir, ["stash", "list"]) {
            Ok(out) if out.is_empty() => {}
            _ => return,
        }
    }
    for sub in &initialized {
        // Un-forced on purpose: git re-checks for local modifications, so a
        // change racing in since the proofs above still refuses here.
        if git_capture(worktree, ["submodule", "deinit", "--", sub]).is_err() {
            return;
        }
    }
    let Ok(gitdir) = git_capture(worktree, ["rev-parse", "--absolute-git-dir"]) else {
        return;
    };
    let gitdir = PathBuf::from(gitdir);
    // Only ever the LINKED-worktree admin shape `.../worktrees/<id>/modules`;
    // a main checkout's `modules/` holds the primary clone's stores and is
    // not this verb's to delete.
    if gitdir.parent().and_then(Path::file_name) == Some(std::ffi::OsStr::new("worktrees")) {
        let modules = gitdir.join("modules");
        if modules.is_dir() {
            let _ = std::fs::remove_dir_all(&modules);
        }
    }
}

/// Turn an anchored removal into the outcome the report reads, PRESERVING
/// whether anything was destroyed.
///
/// `touched` is why this exists. A removal that fails part-way has already
/// deleted files, and reporting that as KEPT is a lie the operator acts on
/// (TASK-RMA18: "kept means untouched").
#[cfg(unix)]
fn removal_outcome(
    result: std::result::Result<(), anchored_dir::RemovalFailure>,
    mut salvage: Option<SalvageCommit>,
) -> WorktreeRemovalOutcome {
    match result {
        Ok(()) => {
            if let Some(salvage) = &mut salvage {
                salvage.worktree_removed = true;
            }
            WorktreeRemovalOutcome {
                removed: true,
                touched: true,
                salvage,
                error: None,
                report_error: None,
                report_path: None,
            }
        }
        Err(failure) => WorktreeRemovalOutcome {
            removed: false,
            touched: failure.touched,
            salvage,
            error: Some(failure.error.to_string()),
            report_error: None,
            report_path: None,
        },
    }
}

#[cfg(not(unix))]
fn reclaim_managed_worktree(
    _project_root: &Path,
    _root: &AnchoredManagedRoot,
    _worktree: &ManagedWorktree,
) -> WorktreeRemovalOutcome {
    WorktreeRemovalOutcome {
        removed: false,
        touched: false,
        salvage: None,
        error: Some("reclaiming a worktree is implemented for unix only".to_string()),
        report_error: None,
        report_path: None,
    }
}

// TASK-M47E5.2 finding 1: `remove_orphaned_worktree_dir` used to live here, and
// is deliberately gone rather than repaired. Every guard it had was a statement
// about a PATH — no `..` component, `path.parent() == managed_root`, and a
// final `symlink_metadata` — and all three passed for a victim directory
// reached through a symlinked root. `path` was built as `<managed_root>/<child>`,
// so the parent comparison was trivially true; `normalize_path` canonicalized
// both sides through the same link, so it passed under either reading; and the
// final stat saw a real directory because it was one.
//
// The replacement is `AnchoredManagedRoot::remove_child`, which does not take a
// path at all. See the design note above `anchored_dir`.

/// Test-only rendezvous occupying the window between the daemon's reservation
/// and the destructive work it protects — the interleaving in which a
/// `POST /runs/:origin/recover` acquires in ANOTHER PROCESS. Mirrors
/// [`dispatch_close_pause_after_guard`], and no-op unless the env var names a
/// file.
///
/// COMPILE-GATED OUT OF RELEASE BUILDS (TASK-RMA18). The hook parks the process
/// indefinitely while it holds BOTH the global dispatch cleanup lock and the
/// daemon's cleanup reservation for the worktree — so in a shipped binary a
/// stray environment variable, inherited by any child of a shell that once ran
/// the test suite, wedges reclamation and blocks every acquire into that
/// worktree until the process is killed. A test-only rendezvous belongs in test
/// builds.
///
/// What proves that is `the_pause_rendezvous_hooks_park_only_in_debug_builds`,
/// which CALLS this hook and watches whether it parks. TASK-RMA18 proved it
/// instead with a `const fn` whose body was `cfg!(debug_assertions)` compared
/// against `cfg!(debug_assertions)` — a tautology that stayed green with the
/// `#[cfg]` deleted, and which is gone (TASK-RMA18.1 finding 3).
// orgasmic:TASK-M47E5.2,TASK-RMA18,TASK-RMA18.1
#[cfg(debug_assertions)]
fn worktree_prune_pause_after_guard() {
    pause_until_file_is_removed("ORGASMIC_WORKTREE_PRUNE_PAUSE_FILE");
}

#[cfg(not(debug_assertions))]
fn worktree_prune_pause_after_guard() {}

/// Explicit, operator-run reclamation of managed worktrees. See the design note
/// at the top of this section for why removal never happens automatically.
// orgasmic:TASK-M47E5,TASK-RMA18
pub fn cmd_worktree_prune(home: &Home, args: WorktreePruneArgs) -> Result<()> {
    worktree_prune(home, args)
}

fn worktree_prune(home: &Home, args: WorktreePruneArgs) -> Result<()> {
    let project_root = find_live_project_root(home, "manager worktree-prune")?;
    let project_id = read_project_id(&project_root)?;
    // Anchor before anything reads or removes. Every component below the home
    // directory is opened `O_NOFOLLOW`, so an ancestor symlink is refused here
    // rather than followed (TASK-RMA18 finding 4), and every classification and
    // every removal below resolves against these handles rather than against a
    // path (TASK-M47E5.2 finding 1, TASK-RMA18 finding 5).
    // An ABSENT root is not an error and not an early exit: it means there are
    // no worktrees to classify, but `git worktree prune` below still has stale
    // `.git/worktrees` admin entries to clear — which is precisely the state an
    // operator who `rm -rf`'d `~/.orgasmic/worktrees` leaves behind.
    let anchored_root = AnchoredManagedRoot::open(home, &project_id)?;
    // Fail CLOSED on an unreachable daemon. Reclamation now requires the
    // daemon's cleanup reservation, and a daemon we cannot reach is a daemon
    // that cannot prove nothing is live in these directories — which used to be
    // silently treated as "nothing is live" (TASK-M47E5.2 finding 2).
    let runtime = tokio::runtime::Runtime::new().context("create tokio runtime")?;
    let client = DaemonClient::from_home_autostart(home).context(
        "worktree-prune needs the daemon to reserve each worktree before reclaiming it, and \
         cannot prove a worktree is unoccupied without it",
    )?;
    let live_runs = runtime
        .block_on(fetch_live_runs(&client))
        .context("read live runs before classifying managed worktrees")?;
    let mut found = match anchored_root.as_ref() {
        Some(root) => scan_managed_worktrees(root, &project_root, &project_id, &live_runs)?,
        None => Vec::new(),
    };
    if let Some(task) = args.task.as_deref() {
        let wanted = [
            worktree_stem(task, DispatchKind::Implementer),
            worktree_stem(task, DispatchKind::Reviewer),
        ];
        found.retain(|worktree| wanted.iter().any(|stem| *stem == worktree.name()));
    }

    // Say what is being left alone and why BEFORE doing anything, so a run that
    // reclaims nothing still reads as a report rather than as a no-op.
    let mut skipped = 0usize;
    for worktree in found.iter().filter(|worktree| !worktree.reclaimable()) {
        skipped += 1;
        println!(
            "SKIP PATH={} WHY={}",
            worktree.path.display(),
            worktree.why()
        );
    }
    let reclaimable: Vec<ManagedWorktree> = found.into_iter().filter(|w| w.reclaimable()).collect();

    if args.dry_run {
        let mut total: u64 = 0;
        for worktree in &reclaimable {
            let bytes = worktree.bytes.unwrap_or(0);
            total = total.saturating_add(bytes);
            println!(
                "WOULD_RECLAIM PATH={} BYTES={bytes} SIZE={} WHY={}",
                worktree.path.display(),
                format_bytes(bytes),
                worktree.why()
            );
        }
        match git_worktree_prune(&project_root, true) {
            Ok(report) if !report.is_empty() => {
                for line in report.lines() {
                    println!("WOULD_PRUNE_METADATA {line}");
                }
            }
            Ok(_) => {}
            Err(err) => eprintln!("warning: {err}"),
        }
        println!(
            "DRY_RUN RECLAIMABLE={} BYTES={total} SIZE={} SKIPPED={skipped}",
            reclaimable.len(),
            format_bytes(total)
        );
        return Ok(());
    }

    // One lock for the whole reclaim, shared with `dispatch-close`'s worktree
    // removal. Held across the loop, so nothing below may take it again.
    //
    // Note the lock ORDER against a concurrent close, which takes its daemon
    // reservation first and this file lock second. That inversion cannot
    // deadlock because the daemon reservation never waits: an already-held
    // worktree comes back `reservation_held` immediately, this loop skips it,
    // and the file lock is released at the end of the verb.
    let _cleanup_lock = acquire_dispatch_cleanup_lock(&project_root)?;
    // Re-read the ledger under the lock: a dispatch may have opened between the
    // classification above and this point, and a live worker's tree must never
    // be reclaimed on the strength of a stale read. This is a cheap early exit,
    // not the authority — the ledger is exactly what finding 2 showed cannot be
    // trusted alone, and the reservation below is what actually decides.
    let now_open = scan_open_dispatches(&project_root)?;

    let mut reclaimed = 0usize;
    let mut kept = 0usize;
    let mut partial = 0usize;
    let mut reclaimed_bytes: u64 = 0;
    for worktree in &reclaimable {
        let normalized = normalize_path(&worktree.path);
        if let Some(record) = now_open.iter().find(|record| {
            record.worktree.as_deref().is_some_and(|path| {
                anchored_dir::identity_of_path(path)
                    .map(|found| found == worktree.identity)
                    .unwrap_or_else(|| normalize_path(path) == normalized)
            })
        }) {
            skipped += 1;
            println!(
                "SKIP PATH={} WHY={}",
                worktree.path.display(),
                held_by_dispatch_detail(record, &live_runs)
            );
            continue;
        }
        // The daemon reservation is keyed on a PATH, because that is what a
        // recovery in another process records for the run it admits. So the
        // path is proved to name the classified identity immediately BEFORE the
        // request — otherwise the fence could describe one directory while the
        // removal below destroys another (TASK-RMA18 finding 5).
        if let Some(root) = anchored_root.as_ref() {
            if let Err(err) = root.assert_path_names(&worktree.path, worktree.identity) {
                skipped += 1;
                println!("SKIP PATH={} WHY={err}", worktree.path.display());
                continue;
            }
        }
        // The authority. Held from here across salvage and removal, released
        // after — so no acquire in another process can enter this worktree in
        // the gap, and none can already be in it undetected.
        let task_property = worktree_reservation_task_id(&worktree.name());
        let mut guard = match reserve_worktree_for_prune(
            &runtime,
            &client,
            &project_id,
            &task_property,
            &worktree.path,
        ) {
            Ok(WorktreeReservation::Held(guard)) => guard,
            Ok(WorktreeReservation::Refused(reason)) => {
                skipped += 1;
                println!("SKIP PATH={} WHY={reason}", worktree.path.display());
                continue;
            }
            Err(err) => {
                skipped += 1;
                println!(
                    "SKIP PATH={} WHY=could not reserve it for reclamation: {err}",
                    worktree.path.display()
                );
                continue;
            }
        };
        worktree_prune_pause_after_guard();
        // And once more with the fence installed: the reservation is only a
        // fence for the identity the path named when the daemon took it, so a
        // path rebound across the round-trip means the guard is protecting
        // something else and this worktree is not reclaimable under it.
        if let Some(root) = anchored_root.as_ref() {
            if let Err(err) = root.assert_path_names(&worktree.path, worktree.identity) {
                finish_worktree_guard(&runtime, &client, &project_id, &task_property, &mut guard);
                skipped += 1;
                println!("SKIP PATH={} WHY={err}", worktree.path.display());
                continue;
            }
        }
        if worktree.release_chain_hold {
            if let Err(err) = unlock_chain_worktree(&project_root, &worktree.path) {
                finish_worktree_guard(&runtime, &client, &project_id, &task_property, &mut guard);
                kept += 1;
                println!(
                    "KEPT PATH={} WHY=could not release expired chain hold: {err}",
                    worktree.path.display()
                );
                continue;
            }
        }
        let bytes = worktree.bytes.unwrap_or(0);
        let outcome = match anchored_root.as_ref() {
            Some(root) => reclaim_managed_worktree(&project_root, root, worktree),
            // Unreachable by construction — nothing is reclaimable when there
            // was no root to enumerate — and stated rather than unwrapped,
            // because the alternative is a panic inside a destructive verb.
            None => WorktreeRemovalOutcome {
                removed: false,
                touched: false,
                salvage: None,
                error: Some("the managed worktree root was not anchored".to_string()),
                report_error: None,
                report_path: None,
            },
        };
        finish_worktree_guard(&runtime, &client, &project_id, &task_property, &mut guard);
        if let Some(salvage) = &outcome.salvage {
            println!(
                "SALVAGED PATH={} SHA={} REF={} FILES={}",
                worktree.path.display(),
                salvage.sha,
                salvage.ref_name,
                salvage.file_count
            );
        }
        if outcome.removed {
            reclaimed += 1;
            reclaimed_bytes = reclaimed_bytes.saturating_add(bytes);
            println!(
                "RECLAIMED PATH={} BYTES={bytes} SIZE={}",
                worktree.path.display(),
                format_bytes(bytes)
            );
        } else {
            // KEPT MEANS UNTOUCHED (TASK-RMA18). A removal that failed after it
            // had already deleted something is a PARTIAL, and saying KEPT there
            // tells an operator the tree is intact when it is a ruin.
            if outcome.touched {
                partial += 1;
            } else {
                kept += 1;
            }
            println!(
                "{} PATH={} WHY={}",
                if outcome.touched { "PARTIAL" } else { "KEPT" },
                worktree.path.display(),
                outcome.error.as_deref().unwrap_or("removal did not run")
            );
        }
    }

    match git_worktree_prune(&project_root, false) {
        Ok(report) => {
            for line in report.lines().filter(|line| !line.trim().is_empty()) {
                println!("PRUNED_METADATA {line}");
            }
        }
        Err(err) => {
            kept += 1;
            // `git worktree prune` clears admin metadata and removes no
            // worktree, so a failure here has destroyed nothing.
            println!("KEPT PATH={} WHY={err}", project_root.display());
        }
    }

    println!(
        "PRUNE_SUMMARY RECLAIMED={reclaimed} BYTES={reclaimed_bytes} SIZE={} PARTIAL={partial} KEPT={kept} SKIPPED={skipped}",
        format_bytes(reclaimed_bytes)
    );
    Ok(())
}

fn build_dispatch_plan(home: &Home, args: DispatchArgs) -> Result<DispatchPlan> {
    let cwd_project_root = find_project_root()?;
    let project_id = read_project_id(&cwd_project_root)?;
    let project_root = registered_project_root(home, &project_id)?;
    let tasks = normalize_tasks(args.task)?;
    let overlapping_open = open_dispatches_overlapping_tasks(&project_root, &tasks)?;
    if let Some(open) = overlapping_open
        .iter()
        .rev()
        .find(|open| !open.reported || open.kind == args.kind.as_str())
    {
        let overlapping = overlapping_tasks(&open.tasks, &tasks);
        // A reported generation may overlap a DIFFERENT kind: that is the
        // handoff which lets review precede implementer close/merge. The same
        // kind still collides, and an unreported worker is still active.
        let hint = if open.reported {
            format!(
                " — its worker has reported, but a second {} dispatch still collides; \
                 close it first with \
                 `orgasmic manager dispatch-close --task {} --started-tx {} \
                 --status done --merge-sha <sha>`",
                open.kind,
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
    let reviewed_dispatch_txs = if args.kind == DispatchKind::Reviewer {
        overlapping_open
            .iter()
            .filter(|open| open.reported && open.kind != args.kind.as_str())
            .map(|open| open.tx_id.clone())
            .collect()
    } else {
        Vec::new()
    };
    for task in &tasks {
        let reported_handoff = overlapping_open.iter().any(|open| {
            open.reported
                && open.kind != args.kind.as_str()
                && open.tasks.iter().any(|open_task| open_task == task)
        });
        if !reported_handoff {
            validate_task_dispatchable(&project_root, task, args.kind)?;
        }
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
    let from_sha = match args.from.as_deref() {
        Some(named) => resolve_commit(&project_root, named)?,
        None => {
            // Post-cutover the registered root is the ledger worktree, whose
            // HEAD is the `orgasmic` tracker branch — a worktree built from it
            // holds tracker files and no source. Default to the HEAD of the
            // checkout the manager is dispatching from, and refuse when that
            // is itself the ledger.
            let cwd = std::env::current_dir().context("cwd")?;
            let head_branch = git_capture(&cwd, ["symbolic-ref", "-q", "--short", "HEAD"])
                .unwrap_or_default();
            if head_branch.trim() == "orgasmic" {
                bail!(
                    "refusing to dispatch from the `orgasmic` ledger branch; \
                     run from a source checkout or pass --from <ref>"
                );
            }
            resolve_commit(&cwd, "HEAD")?
        }
    };
    let requested_worktree = args
        .worktree
        .as_deref()
        .map(absolutize)
        .transpose()?
        .map(|path| normalize_path(&path));
    let reusable = if args.kind == DispatchKind::Implementer && !args.fresh_worktree {
        scan_dispatches(&project_root)?
            .into_iter()
            .rev()
            .find(|record| {
                record.closed
                    && record.kind == DispatchKind::Implementer.as_str()
                    && record.tasks.len() == tasks.len()
                    && record.tasks.iter().all(|task| tasks.contains(task))
            })
            .and_then(|record| record.worktree)
            .map(|path| normalize_path(&path))
            .filter(|path| path.is_dir())
            .filter(|path| {
                requested_worktree
                    .as_ref()
                    .is_none_or(|requested| requested == path)
            })
    } else {
        None
    };
    let reuse_worktree = reusable.is_some();
    let worktree_path = normalize_path(&reusable.or(requested_worktree).unwrap_or(
        default_worktree(home, &project_id, first_task(&tasks), args.kind)?,
    ));
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
        reuse_worktree,
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
        reviewed_dispatch_txs,
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
    println!("  reuse:    {}", plan.reuse_worktree);
    println!("  branch:   {}", plan.branch);
    println!("  brief:    {}", plan.brief_path.display());
    println!("  last:     {}", plan.last_path.display());
    println!("  stdout:   {}", plan.stdout_path.display());
    println!("  tx:       manager.dispatch_started on daemon dispatch");
    println!("  mode:     {}", plan.mode);
    println!("  harness:  {}", plan.harness);
    // The daemon re-addresses chat-capable harnesses onto the canonical chat
    // runtime; say so here or the plan describes a launch that never happens.
    if let Some((driver, harness)) = orgasmic_daemon::addressing::canonical_chat_address(
        &plan.mode,
        &plan.harness,
        &plan.harness_args,
    ) {
        if driver != plan.mode || harness != plan.harness {
            println!(
                "  resolves: driver={driver} harness={harness} (canonical chat runtime; \
                 requested mode and harness args are not used)"
            );
        }
    }
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
    close_guard_path_for(project_id, &task_list_property(&open.tasks), suffix)
}

fn close_guard_path_for(project_id: &str, task_property: &str, suffix: &str) -> String {
    format!(
        "/projects/{}/tasks/{}/dispatch/close-guard{suffix}",
        path_segment(project_id),
        path_segment(task_property),
    )
}

/// What the daemon said when `worktree-prune` asked to reserve a worktree.
enum WorktreeReservation {
    Held(HeldCloseGuard),
    /// Refused, with the reason to print beside the worktree that was kept.
    Refused(String),
}

/// Take the SAME daemon-owned worktree reservation a destructive
/// `dispatch-close` takes, for a `worktree-prune` reclaim (TASK-M47E5.2
/// finding 2).
///
/// This is deliberately not a second scheme. `Supervisor::reserve_dispatch_close`
/// already installs a fence and decides liveness under the one lock the acquire
/// path also takes, in that order, which is what makes the verdict monotone: from
/// the instant it is installed, `admit_live_run` refuses every new run for this
/// worktree, so the set of occupants can only shrink. Prune reproduced exactly
/// the defect TASK-1T3FZ was filed to close — a liveness decision made in the
/// CLI, acted on in the CLI, with a `POST /runs/:origin/recover` acquiring in
/// ANOTHER PROCESS in between — so it routes through the same authority rather
/// than growing its own.
///
/// The request differs from a close's in exactly two ways, and both say
/// "prune is entitled to nothing": `releasing_run_id` is `None` and
/// `owned_run_ids` is empty, so EVERY live run occupying the worktree blocks.
/// A close excludes its own generation because tearing that down is what a close
/// is for; prune has no generation of its own to tear down.
// orgasmic:TASK-M47E5.2
fn reserve_worktree_for_prune(
    runtime: &tokio::runtime::Runtime,
    client: &DaemonClient,
    project_id: &str,
    task_property: &str,
    worktree: &Path,
) -> Result<WorktreeReservation> {
    let request = CloseGuardRequest {
        worktree_path: worktree,
        branch: None,
        dispatch_attempt_token: None,
        last_path: None,
        stdout_path: None,
        owner_pid: std::process::id(),
        releasing_run_id: None,
        owned_run_ids: Vec::new(),
    };
    let response: CloseGuardResponse = runtime
        .block_on(client.post_json(
            &close_guard_path_for(project_id, task_property, ""),
            &request,
        ))
        .map_err(|error| {
            if error.to_string().contains("daemon returned 404") {
                return error.context(
                    "this daemon cannot reserve a worktree for reclamation (no close-guard \
                     route); restart it onto the current runtime (`orgasmic daemon restart`) \
                     and re-run",
                );
            }
            error
        })?;
    Ok(match response.status.as_str() {
        "reserved" => {
            let guard_id = response.guard_id.ok_or_else(|| {
                anyhow::anyhow!("daemon reserved the worktree but returned no guard id")
            })?;
            WorktreeReservation::Held(spawn_close_guard_heartbeat(
                client,
                &close_guard_path_for(project_id, task_property, "/renew"),
                guard_id,
                response.renew_within_secs,
            ))
        }
        "blocked" => WorktreeReservation::Refused(format!(
            "run {} is still live in it{} — liveness is decided in the daemon under the same \
             lock that admits a recovery, so this is not a stale snapshot",
            response.blocking_run_id.as_deref().unwrap_or("?"),
            response
                .blocking_worktree
                .as_deref()
                .map(|path| format!(" ({})", path.display()))
                .unwrap_or_default()
        )),
        "reservation_held" => WorktreeReservation::Refused(
            "another cleanup already holds this worktree; let it finish, then prune".to_string(),
        ),
        "boot_reattach_pending" => WorktreeReservation::Refused(
            "the daemon is still rehydrating runs that outlived its predecessor, so it cannot \
             yet say whether a live worker occupies this worktree; re-run in a moment"
                .to_string(),
        ),
        other => WorktreeReservation::Refused(format!(
            "unexpected reservation status from daemon: {other}"
        )),
    })
}

/// Hand a prune reservation back. Best effort, exactly as
/// [`finish_close_guard`] is: a holder that never gets here is reclaimed once
/// its pid is gone or its holder lease expires.
fn finish_worktree_guard(
    runtime: &tokio::runtime::Runtime,
    client: &DaemonClient,
    project_id: &str,
    task_property: &str,
    guard: &mut HeldCloseGuard,
) {
    guard.stop_heartbeat();
    if guard.was_lost() {
        eprintln!(
            "warning: this reclaim lost its worktree reservation before removal finished; the \
             worktree was no longer fenced while it was being removed"
        );
    }
    let result = runtime.block_on(client.post_json::<_, CloseGuardFinishResponse>(
        &close_guard_path_for(project_id, task_property, "/finish"),
        &CloseGuardFinishRequest {
            guard_id: guard.id(),
        },
    ));
    if let Err(err) = result {
        eprintln!(
            "warning: worktree-prune could not release its worktree reservation ({err}); the \
             daemon releases it when this process exits or its holder lease expires"
        );
    }
}

/// The task a managed worktree's directory name encodes, for the reservation
/// key. `worktree_stem` writes `<task-slug>` and `<task-slug>-review`.
///
/// A name that does not match falls back to itself, and that is not a guess
/// dressed up as an answer. Both things the reservation actually enforces are
/// keyed on the CANONICAL WORKTREE, not on this string:
/// `reserve_dispatch_close` refuses any reservation already held for the same
/// `worktree_key`, and `blocking_run_for_close` blocks on any run occupying it.
/// The task id sharpens exactly one extra check — a lease held by an acquire
/// that has not installed its `RunRecord` yet — which is per-task, so a name
/// matching no task adds nothing rather than weakening anything.
// orgasmic:TASK-M47E5.2
fn worktree_reservation_task_id(name: &str) -> String {
    let stem = name.strip_suffix("-review").unwrap_or(name);
    match stem.strip_prefix("task-") {
        Some(rest) if !rest.is_empty() => format!("TASK-{}", rest.to_ascii_uppercase()),
        _ => name.to_string(),
    }
}

/// Whether this close mutates dispatch artifacts and therefore needs the
/// daemon-owned close guard (TASK-1T3FZ, TASK-QGWK7.1 F-3).
///
/// The guard reserves a WORKTREE, so a record without one cannot be fenced by
/// it and this must not claim otherwise (TASK-QGWK7.1.1 M-6): the predicate
/// used to be true for a `LAST_PATH` + `STDOUT_PATH` record with no `WORKTREE`
/// while [`reserve_close_guard`] returned `Ok(None)` for exactly that shape —
/// the fence was requested, silently absent, and the promote arm unlinked
/// anyway. Promotion for such a record still takes the in-process cleanup lock
/// (`promote_dispatch_artifacts_in_place`); what it cannot have is the
/// cross-process reservation, and saying so is the honest state.
fn close_needs_artifact_fence(remove_worktree: bool, open: &DispatchRecord) -> bool {
    open.worktree.is_some()
        && (remove_worktree || (open.last_path.is_some() && open.stdout_path.is_some()))
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
                     restart`) and re-run. Closing without --worktree-remove still promotes \
                     and unlinks tmp artifacts, so it needs the same fence",
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
                 name. Inspect the live run (`orgasmic run show {}`) and let it finish; do \
                 not close while it occupies the worktree — even without --worktree-remove, \
                 close promotes and unlinks the tmp report.",
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
///
/// COMPILE-GATED OUT OF RELEASE BUILDS for the same reason its `worktree-prune`
/// twin is (TASK-RMA18.1 finding 3): this hook parks the process indefinitely
/// while `dispatch-close` holds BOTH the global dispatch cleanup lock and the
/// daemon reservation, so in a shipped binary a stray environment variable —
/// inherited by any child of a shell that once ran the suite — wedges the close
/// and blocks every acquire into that worktree until the process is killed.
/// TASK-RMA18 gated the prune hook and left this one, 400 lines away in the same
/// file, holding the same two locks.
// orgasmic:TASK-RMA18.1
#[cfg(debug_assertions)]
fn dispatch_close_pause_after_guard() {
    pause_until_file_is_removed("ORGASMIC_DISPATCH_CLOSE_PAUSE_FILE");
}

#[cfg(not(debug_assertions))]
fn dispatch_close_pause_after_guard() {}

#[cfg(debug_assertions)]
fn pause_until_file_is_removed(env_var: &str) {
    let Ok(raw) = std::env::var(env_var) else {
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
        .post_run_release(&format!("/runs/{}/release", path_segment(run_id)), &request)
        .await
        .map_err(|error| {
            // orgasmic:TASK-RB1ZN — the daemon answers 409 for a run that is
            // live with a release already running, and the client's generic 409
            // sentence ("node changed on disk; reload base_version and retry")
            // is about node writes: it names the wrong subject and offers advice
            // that does nothing here. The daemon's own body — which names the
            // HAREX drain budget — rides along behind this context.
            if is_release_in_progress_error(&error) {
                return error.context(format!(
                    "run {run_id} is live and another authority is already releasing \
                     it, so this call released nothing"
                ));
            }
            error
        })
}

/// Refuse to release the lease unless the daemon has proven, BEFORE the
/// release call, that it will write the terminal tx as part of that release
/// (TASK-WGXKD.1, reviewer finding 1).
///
/// Why a pre-flight probe and not a post-release fallback: on stdio there
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

/// Whether `err` is the daemon's 409 for a run that is LIVE with a release
/// already running for it (`SupervisorError::ReleaseInProgress`, TASK-RB1ZN).
///
/// The opposite state from [`is_release_run_not_found_error`], and the reason
/// the two must not share a branch: the already-released rescue works by reading
/// the run's release tombstone off its session, and a run whose release is still
/// running has not written one yet. Routing this here would either miss the
/// tombstone (and refuse with a sentence that names the wrong reason) or, worse,
/// read a stale one and let a finalize claim completion for a release that was
/// never its own.
///
/// Matched against the whole cause chain (`{:#}`), not just the outermost
/// message: the release helper adds its own context on this error, and
/// `Error::to_string` shows only the top of the chain.
fn is_release_in_progress_error(err: &anyhow::Error) -> bool {
    format!("{err:#}").contains("release already in progress")
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
    worker_commit: Option<&'a str>,
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
        worker_commit,
        cleanup,
        transition,
    } = *facts;
    let mut manager_extra = Vec::new();
    if let Some(session) = optional_value(args.worker_session.as_deref()) {
        manager_extra.push(("WORKER_SESSION".to_string(), session));
    }
    if let Some(model) = optional_value(open.model.as_deref()) {
        manager_extra.push(("MODEL".to_string(), model));
    }
    if let Some(effort) = optional_value(open.effort.as_deref()) {
        manager_extra.push(("EFFORT".to_string(), effort));
    }
    if let Some(commit) = worker_commit
        .map(str::to_string)
        .or_else(|| optional_value(args.worker_commit.as_deref()))
    {
        manager_extra.push(("WORKER_COMMIT".to_string(), commit));
    }
    if matches!(tx_type, "implementer.done" | "architector.done") {
        if let Some(merge_sha) = merge_sha {
            manager_extra.push(("MERGE_SHA".to_string(), merge_sha.to_string()));
        }
        if let Some(branch) = optional_value(open.branch.as_deref()) {
            manager_extra.push(("BRANCH".to_string(), branch));
        }
    }
    if let Some(wall) = optional_value(args.wall.as_deref()) {
        manager_extra.push(("WALL".to_string(), wall));
    }
    if let Some(tokens) = args.tokens {
        manager_extra.push(("TOKENS".to_string(), tokens.to_string()));
    }
    if let Some(reviewed_diff) = optional_value(args.reviewed_diff.as_deref()) {
        manager_extra.push(("REVIEWED_DIFF".to_string(), reviewed_diff));
    }
    // orgasmic:TASK-YN5FJ.1 — the flag writes the SAME `VERDICT` key the legacy
    // `--property VERDICT=` spelling writes; `dispatch_close` has already
    // refused a close that passes both, so exactly one of these can land.
    if let Some(verdict) = args.verdict {
        manager_extra.push(("VERDICT".to_string(), verdict.as_str().to_string()));
    }
    if args.no_review_required {
        manager_extra.push(("NO_REVIEW_REQUIRED".to_string(), "true".to_string()));
    }
    // orgasmic:TASK-4WKNX — the opt-out is stamped, not just obeyed: the
    // difference between "this fix round was reviewed" and "this fix round was
    // declared not to need one" has to be readable off the ledger later.
    if args.fix_round_final {
        manager_extra.push(("FIX_ROUND_FINAL".to_string(), "true".to_string()));
    }
    manager_extra.push(("CLOSED_TX".to_string(), open.tx_id.clone()));
    push_lifecycle_extra(&mut manager_extra, transition);
    push_cleanup_extra(&mut manager_extra, cleanup);
    // A manager-supplied `--property REPORT_PATH=` (historical reviewer.done
    // curated report) wins over the auto-promoted last.txt path. The promoted
    // file still exists on disk either way.
    if args.properties.iter().any(|(key, _)| key == "REPORT_PATH") {
        manager_extra.retain(|(key, _)| key != "REPORT_PATH");
    }
    if let Some(goal_id) = optional_value(open.goal_id.as_deref()) {
        manager_extra.push(("GOAL_ID".to_string(), goal_id));
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
        extra: finish_close_tx_extras(manager_extra, &args.properties),
        tx_path: None,
    }
}

/// Finish a close tx's extras without enumerating its manager-owned keys.
///
/// Tx-extra value readers are first-wins, so this consuming boundary appends
/// the generic property channel only after every structured value. A future
/// manager-owned key is protected by being pushed into `manager_extra`; it does
/// not need a matching entry in [`MANAGER_OWNED_CLOSE_PROPERTIES`].
fn finish_close_tx_extras(
    mut manager_extra: Vec<(String, String)>,
    properties: &[(String, String)],
) -> Vec<(String, String)> {
    manager_extra.extend(
        properties
            .iter()
            .map(|(key, value)| (key.clone(), sanitize_tx_value(value))),
    );
    manager_extra
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
    //
    // `architector` is LEGACY (dec_HBK6A): no verb starts one any more, but
    // this function is fed the `kind` string read back off a persisted
    // `DispatchRecord`, so an architector dispatch opened before the excision
    // must still be closable. Removing the arm would strand it.
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
/// `architector` stays in the reported set as LEGACY (dec_HBK6A retired both
/// the dispatch kind and the `architect` stage). It is kept because finalize is
/// fed `run.kind` off a live or persisted run record: a worker still running
/// inside an architector dispatch opened before the excision must finalize with
/// the tx the manager's close is waiting for.
fn finalize_tx_type_for_kind(kind: &str) -> Result<&'static str> {
    match kind {
        "implementer" => Ok("implementer.reported"),
        "reviewer" => Ok("reviewer.reported"),
        "architector" => Ok("architector.reported"),
        other => done_tx_type_for_kind(other),
    }
}

/// Outcome of applying the private target policy to one dispatch.
#[derive(Debug, Clone, PartialEq, Eq)]
enum WorktreeTargetSeed {
    Skipped {
        elapsed: Duration,
        reason: &'static str,
    },
}

impl WorktreeTargetSeed {
    fn status(&self) -> &'static str {
        match self {
            Self::Skipped { .. } => "skipped",
        }
    }

    fn elapsed(&self) -> Duration {
        match self {
            Self::Skipped { elapsed, .. } => *elapsed,
        }
    }

    fn detail(&self) -> String {
        match self {
            Self::Skipped { reason, .. } => format!(" reason={reason}"),
        }
    }
}

/// Keep Cargo state private by deliberately leaving the new worktree target
/// empty.  This is a bounded startup operation and the only universal cache
/// policy that cannot expose a linked checkout's artifacts.  A future
/// compiler-object cache may improve cold builds, but it must prove its own
/// keying and process isolation rather than reuse Cargo target state.
// orgasmic:TASK-79VKP.6
fn private_worktree_target_policy(project_root: &Path, worktree: &Path) -> WorktreeTargetSeed {
    let started = Instant::now();
    if !project_root.join("Cargo.toml").is_file() {
        return WorktreeTargetSeed::Skipped {
            elapsed: started.elapsed(),
            reason: "not-cargo",
        };
    }
    let target = worktree.join("target");
    if target.exists() {
        return WorktreeTargetSeed::Skipped {
            elapsed: started.elapsed(),
            reason: "private-target-present",
        };
    }
    WorktreeTargetSeed::Skipped {
        elapsed: started.elapsed(),
        reason: "empty-private-target",
    }
}

const CHAIN_WORKTREE_LOCK_PREFIX: &str = "orgasmic: next implementer round";

#[derive(Clone, Debug)]
struct GitWorktreeRegistration {
    path: PathBuf,
    /// `Some("")` is a lock with no reason; `None` is unlocked.
    lock_reason: Option<String>,
}

fn git_worktree_registrations(project_root: &Path) -> Result<Vec<GitWorktreeRegistration>> {
    Ok(
        git_capture(project_root, ["worktree", "list", "--porcelain", "-z"])
            .map_err(anyhow::Error::msg)?
            .split("\0\0")
            .filter_map(|record| {
                let path = record
                    .split('\0')
                    .find_map(|field| field.strip_prefix("worktree "))?;
                let lock_reason = record.split('\0').find_map(|field| {
                    field
                        .strip_prefix("locked")
                        .map(|reason| reason.trim().to_string())
                });
                Some(GitWorktreeRegistration {
                    path: normalize_path(Path::new(path.trim())),
                    lock_reason,
                })
            })
            .collect(),
    )
}

fn worktree_registration(
    project_root: &Path,
    path: &Path,
) -> Result<Option<GitWorktreeRegistration>> {
    let wanted = normalize_path(path);
    Ok(git_worktree_registrations(project_root)?
        .into_iter()
        .find(|registration| registration.path == wanted))
}

fn chain_worktree_lock_reason(tasks: &[String]) -> String {
    format!(
        "{CHAIN_WORKTREE_LOCK_PREFIX} for {}",
        task_list_property(tasks)
    )
}

fn chain_hold_has_pending_round(project_root: &Path, reason: &str) -> bool {
    let Some(tasks) = reason
        .strip_prefix(CHAIN_WORKTREE_LOCK_PREFIX)
        .and_then(|suffix| suffix.strip_prefix(" for "))
        .map(split_task_list)
        .filter(|tasks| !tasks.is_empty())
    else {
        return true;
    };
    let mut pending = true;
    for task in tasks {
        let Ok(task) = read_task_lifecycle(project_root, &task) else {
            return true;
        };
        pending &= dispatchable_stage(DispatchKind::Implementer, task.stage);
    }
    pending
}

fn hold_chain_worktree(project_root: &Path, path: &Path, tasks: &[String]) -> Result<()> {
    let registration = worktree_registration(project_root, path)?.ok_or_else(|| {
        anyhow::anyhow!(
            "cannot keep implementer chain worktree {}: it is no longer registered; inspect it, \
             then retry the close",
            path.display()
        )
    })?;
    if let Some(reason) = registration.lock_reason {
        if reason.starts_with(CHAIN_WORKTREE_LOCK_PREFIX) {
            return Ok(());
        }
        bail!(
            "cannot keep implementer chain worktree {}: it is already locked{}; unlock or \
             inspect it, then retry the close",
            path.display(),
            if reason.is_empty() {
                String::new()
            } else {
                format!(" ({reason})")
            }
        );
    }
    let reason = chain_worktree_lock_reason(tasks);
    git_capture(
        project_root,
        [
            std::ffi::OsStr::new("worktree"),
            std::ffi::OsStr::new("lock"),
            std::ffi::OsStr::new("--reason"),
            std::ffi::OsStr::new(&reason),
            path.as_os_str(),
        ],
    )
    .map_err(|err| {
        anyhow::anyhow!(
            "could not hold implementer chain worktree {} for the next round: {err}",
            path.display()
        )
    })?;
    Ok(())
}

fn unlock_chain_worktree(project_root: &Path, path: &Path) -> Result<()> {
    git_capture(
        project_root,
        [
            std::ffi::OsStr::new("worktree"),
            std::ffi::OsStr::new("unlock"),
            path.as_os_str(),
        ],
    )
    .map_err(|err| anyhow::anyhow!("git worktree unlock failed for {}: {err}", path.display()))?;
    Ok(())
}

fn release_chain_worktree_holds(project_root: &Path, tasks: &[String]) {
    let records = match scan_dispatches(project_root) {
        Ok(records) => records,
        Err(err) => {
            eprintln!("warning: could not scan implementer chain holds after final close: {err}");
            return;
        }
    };
    let mut seen = BTreeSet::new();
    for path in records
        .into_iter()
        .filter(|record| {
            record.closed
                && record.kind == DispatchKind::Implementer.as_str()
                && record.tasks.iter().any(|task| tasks.contains(task))
        })
        .filter_map(|record| record.worktree)
        .filter(|path| path.is_dir())
    {
        let path = normalize_path(&path);
        if !seen.insert(path.clone()) {
            continue;
        }
        match worktree_registration(project_root, &path) {
            Ok(Some(registration))
                if registration
                    .lock_reason
                    .as_deref()
                    .is_some_and(|reason| reason.starts_with(CHAIN_WORKTREE_LOCK_PREFIX)) =>
            {
                if let Err(err) = unlock_chain_worktree(project_root, &path) {
                    eprintln!(
                        "warning: final close could not release implementer chain hold on {}: \
                         {err}",
                        path.display()
                    );
                }
            }
            Ok(_) => {}
            Err(err) => eprintln!(
                "warning: final close could not inspect implementer chain hold on {}: {err}",
                path.display()
            ),
        }
    }
}

fn prepare_worktree(plan: &DispatchPlan) -> Result<()> {
    if !plan.reuse_worktree {
        return create_worktree(
            &plan.project_root,
            &plan.worktree_path,
            &plan.branch,
            &plan.from_sha,
        );
    }

    let registration =
        worktree_registration(&plan.project_root, &plan.worktree_path)?.ok_or_else(|| {
            anyhow::anyhow!(
                "cannot reuse implementer chain worktree {}: it is not registered (wedged or \
                 moved); repair it, or pass --fresh-worktree --worktree <new-path>",
                plan.worktree_path.display()
            )
        })?;
    let dirty = git_capture(
        &plan.worktree_path,
        [
            "status",
            "--porcelain",
            "--ignore-submodules=none",
            "--untracked-files=normal",
        ],
    )
    .map_err(|err| {
        anyhow::anyhow!(
            "cannot reuse implementer chain worktree {}: tree is wedged ({err}); repair it, or \
             pass --fresh-worktree --worktree <new-path>",
            plan.worktree_path.display()
        )
    })?;
    if !dirty.trim().is_empty() {
        bail!(
            "cannot reuse implementer chain worktree {}: tree is dirty:\n{}\ncommit or clean \
             it, or pass --fresh-worktree --worktree <new-path>",
            plan.worktree_path.display(),
            dirty.trim()
        );
    }
    match registration.lock_reason.as_deref() {
        Some(reason) if reason.starts_with(CHAIN_WORKTREE_LOCK_PREFIX) => {
            unlock_chain_worktree(&plan.project_root, &plan.worktree_path)?;
        }
        Some(reason) => {
            bail!(
                "cannot reuse implementer chain worktree {}: tree is locked{} (wedged for \
                 chain reuse); unlock or repair it, or pass --fresh-worktree --worktree \
                 <new-path>",
                plan.worktree_path.display(),
                if reason.is_empty() {
                    String::new()
                } else {
                    format!(" ({reason})")
                }
            );
        }
        None => {}
    }

    if let Err(err) = git_capture(
        &plan.worktree_path,
        [
            "checkout",
            "-b",
            plan.branch.as_str(),
            plan.from_sha.as_str(),
        ],
    ) {
        let _ = hold_chain_worktree(&plan.project_root, &plan.worktree_path, &plan.tasks);
        bail!(
            "cannot create round branch {} from {} inside reused worktree {}: {err}; choose an \
             unused --branch, repair the tree, or pass --fresh-worktree --worktree <new-path>",
            plan.branch,
            plan.from_sha,
            plan.worktree_path.display()
        );
    }
    eprintln!(
        "dispatch worktree: reused={} branch={} from={}",
        plan.worktree_path.display(),
        plan.branch,
        plan.from_sha
    );
    init_worktree_submodules(&plan.worktree_path);
    Ok(())
}

fn retain_reused_worktree_after_failed_dispatch(plan: &DispatchPlan) -> CleanupOutcome {
    let held = hold_chain_worktree(&plan.project_root, &plan.worktree_path, &plan.tasks);
    CleanupOutcome {
        status: CleanupStatus::Partial,
        error: Some(sanitize_tx_value(&match held {
            Ok(()) => format!(
                "reused worktree retained and re-locked after dispatch failure; branch {} remains \
                 in {} (retry with a new --branch, or use --fresh-worktree --worktree <new-path>)",
                plan.branch,
                plan.worktree_path.display()
            ),
            Err(err) => format!(
                "reused worktree retained after dispatch failure, but its chain hold could not be \
                 restored: {err}"
            ),
        })),
        salvage: None,
        report_path: None,
    }
}

fn create_worktree(project_root: &Path, path: &Path, branch: &str, from_sha: &str) -> Result<()> {
    if path.exists() {
        if worktree_registration(project_root, path)
            .ok()
            .flatten()
            .and_then(|registration| registration.lock_reason)
            .is_some_and(|reason| reason.starts_with(CHAIN_WORKTREE_LOCK_PREFIX))
        {
            bail!(
                "worktree path already exists: {} is held for an implementer chain; use \
                 --fresh-worktree --worktree <new-path>",
                path.display()
            );
        }
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
    // `git worktree add` leaves submodules as empty placeholders, so a worker
    // in a submodule-bearing repo (vscode-orsl → orsl-language-server) could
    // neither read nor build the sub-repo. Populate them; the objects land in
    // the worktree's PRIVATE store (`.git/worktrees/<id>/modules/`), which
    // cleanup settles back out of existence when the submodules are left
    // as-initialized (see `settle_as_initialized_submodules`).
    //
    // A failure (offline, unreachable URL) warns instead of failing the
    // dispatch: an empty placeholder is exactly what every worktree had
    // before this ran, and many tasks never touch the sub-repo.
    init_worktree_submodules(path);
    // Note: a dispatched `claude` in this fresh worktree shows the "Is this a
    // project you trust?" dialog (`--dangerously-skip-permissions` does NOT
    // clear it in Claude 2.1.x). The driver accepts that dialog by sending a
    // keystroke before pasting the brief — see `accept_folder_trust` in the
    // tmux driver — so no global Claude config mutation is needed here.
    Ok(())
}

fn init_worktree_submodules(path: &Path) {
    if path.join(".gitmodules").is_file() {
        // `alternateLocation=superproject` borrows objects from the main
        // checkout's existing submodule store instead of re-cloning from the
        // origin — read-only at init time, so worktree-side submodule commits
        // never touch the main store. `alternateErrorStrategy=info` falls back
        // to a plain clone when the main checkout never initialized the
        // submodule.
        if let Err(err) = git_capture(
            path,
            [
                "-c",
                "submodule.alternateLocation=superproject",
                "-c",
                "submodule.alternateErrorStrategy=info",
                "submodule",
                "update",
                "--init",
                "--recursive",
            ],
        ) {
            eprintln!(
                "warning: worktree submodule init failed (sub-repo dirs stay empty): {err}"
            );
        }
    }
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

#[allow(clippy::too_many_arguments)]
fn remove_worktree_if_present(
    project_root: &Path,
    path: &Path,
    last_path: Option<&Path>,
    stdout_path: Option<&Path>,
    task: &str,
    branch: Option<&str>,
    expected_branch_oid: Option<&str>,
    started_tx: Option<&str>,
) -> Result<WorktreeRemovalOutcome> {
    if !path.exists() {
        return Ok(WorktreeRemovalOutcome {
            removed: false,
            touched: false,
            salvage: None,
            error: None,
            report_error: None,
            report_path: None,
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
        started_tx,
    )
}

#[allow(clippy::too_many_arguments)]
fn remove_worktree_required(
    project_root: &Path,
    path: &Path,
    last_path: Option<&Path>,
    stdout_path: Option<&Path>,
    task: &str,
    branch: Option<&str>,
    expected_branch_oid: Option<&str>,
    started_tx: Option<&str>,
) -> Result<WorktreeRemovalOutcome> {
    remove_worktree_required_with_hook(
        project_root,
        path,
        last_path,
        stdout_path,
        task,
        branch,
        expected_branch_oid,
        started_tx,
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
    started_tx: Option<&str>,
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
                // `git` never started, so it removed nothing.
                touched: false,
                salvage,
                error: Some(format!("git worktree remove {}: {err}", path.display())),
                report_error: None,
                report_path: None,
            });
        }
    };
    if !output.status.success() {
        return Ok(WorktreeRemovalOutcome {
            removed: false,
            // `git worktree remove` ran and refused. It reports its refusals
            // before it deletes, but this process cannot prove that from the
            // outside, so the honest answer is "possibly".
            touched: true,
            salvage,
            error: Some(format!(
                "git worktree remove failed: {}{}",
                String::from_utf8_lossy(&output.stderr),
                String::from_utf8_lossy(&output.stdout)
            )),
            report_error: None,
            report_path: None,
        });
    }
    if let Some(salvage) = &mut salvage {
        salvage.worktree_removed = true;
    }
    // orgasmic:TASK-QGWK7 — promote the report into a tracked path keyed by
    // the dispatch generation, rather than deleting it with the tmp artifacts.
    // Failed-dispatch rollback passes `started_tx: None` and still deletes.
    let (report_path, report_error, error) = match started_tx {
        Some(started_tx) => {
            let task_id = task.split_whitespace().next().unwrap_or(task);
            match promote_and_persist_dispatch_record(
                &artifacts,
                project_root,
                task_id,
                started_tx,
                path,
            ) {
                Ok(outcome) => (outcome.report_path, outcome.error, None),
                Err(err) => (
                    None,
                    Some(format!(
                        "promote dispatch report for {}: {err}",
                        path.display()
                    )),
                    None,
                ),
            }
        }
        None => (
            None,
            None,
            orgasmic_core::prune_validated_dispatch_attempt(&artifacts)
                .err()
                .map(|err| format!("prune dispatch artifacts for {}: {err}", path.display())),
        ),
    };
    Ok(WorktreeRemovalOutcome {
        removed: true,
        touched: true,
        salvage,
        error,
        report_error,
        report_path,
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
        report_path: None,
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
                report_path: None,
            };
        }
    };

    let mut salvage = None;
    let mut worktree_removed = false;
    // Failed-dispatch rollback: delete tmp artifacts, do not promote
    // (TASK-QGWK7 promotion is for manager close of a finished worker).
    match remove_worktree_if_present(
        project_root,
        path,
        Some(last_path),
        Some(stdout_path),
        task,
        Some(branch),
        expected_branch_oid.as_deref(),
        None,
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
        report_path: None,
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
    let mut report_path = None;
    let expected_branch_oid = if branch_delete {
        match &open.branch {
            Some(branch) => match resolve_branch_oid(project_root, branch) {
                Ok(oid) => oid,
                Err(err) => {
                    return CleanupOutcome {
                        status: CleanupStatus::BranchFailed,
                        error: Some(sanitize_tx_value(&format!("branch validation: {err}"))),
                        salvage: None,
                        report_path: None,
                    };
                }
            },
            None => {
                return CleanupOutcome {
                    status: CleanupStatus::BranchFailed,
                    error: Some("branch: open dispatch has no BRANCH property".to_string()),
                    salvage: None,
                    report_path: None,
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
                    Some(open.tx_id.as_str()),
                ) {
                    Ok(outcome) => {
                        worktree_removed = outcome.removed;
                        salvage = outcome.salvage;
                        report_path = outcome.report_path;
                        if let Some(err) = outcome.error {
                            worktree_failed = true;
                            errors.push(format!("worktree: {err}"));
                        }
                        // orgasmic:TASK-QGWK7.1.1 — M-1: a report that could
                        // not be promoted or committed is reported, never
                        // classified as a worktree failure. Both cleanup arms
                        // now agree on this: `report: <why>` in `CLEANUP_ERROR`
                        // and a `partial` status, with `--branch-delete` left
                        // free to fire on the worktree that really was removed.
                        if let Some(err) = outcome.report_error {
                            errors.push(format!("report: {err}"));
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
    } else if let (Some(last), Some(stdout)) = (last_path.as_deref(), stdout_path.as_deref()) {
        // orgasmic:TASK-QGWK7 / TASK-QGWK7.1 — even with `--no-worktree-remove`,
        // promote the report out of gitignored tmp. The worktree may already
        // be reclaimed (F-4); promotion still succeeds from the tmp artifacts.
        match promote_dispatch_artifacts_in_place(
            project_root,
            open.worktree.as_deref(),
            last,
            stdout,
            open.tasks.first().map(String::as_str).unwrap_or("task"),
            &open.tx_id,
        ) {
            Ok(outcome) => {
                report_path = outcome.report_path;
                if let Some(err) = outcome.error {
                    errors.push(format!("report: {err}"));
                }
            }
            Err(err) => errors.push(format!("report: {err}")),
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
        (false, false, false) if errors.is_empty() => CleanupStatus::Ok,
        (false, false, false) => CleanupStatus::Partial,
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
        report_path,
    }
}

/// Promote tmp dispatch artifacts without removing the worktree.
///
/// Takes the same cleanup lock as worktree-removing cleanup (TASK-QGWK7.1 F-3):
/// promote unlinks the tmp inodes and must not race a recovery writer.
// orgasmic:TASK-QGWK7.1
fn promote_dispatch_artifacts_in_place(
    project_root: &Path,
    worktree: Option<&Path>,
    last_path: &Path,
    stdout_path: &Path,
    task_id: &str,
    started_tx: &str,
) -> Result<orgasmic_core::PromoteOutcome, String> {
    let _cleanup_lock =
        acquire_dispatch_cleanup_lock(project_root).map_err(|err| err.to_string())?;
    if !last_path.exists() {
        return Err(format!("last_path missing: {}", last_path.display()));
    }
    let artifacts = match worktree {
        Some(worktree) if worktree.exists() => orgasmic_core::validate_dispatch_cleanup_targets(
            project_root,
            worktree,
            Some(last_path),
            Some(stdout_path),
        )?,
        _ => orgasmic_core::validate_dispatch_promote_targets(
            project_root,
            Some(last_path),
            Some(stdout_path),
        )?,
    };
    promote_and_persist_dispatch_record(&artifacts, project_root, task_id, started_tx, last_path)
}

/// Promote validated artifacts, then put the promoted directory into git
/// history so a fresh clone can read it (TASK-QGWK7.1 F-1) without leaving the
/// index dirty (TASK-QGWK7.1.1 M-0). A failure here is reported through
/// `CLEANUP_ERROR` and never fails the close.
fn promote_and_persist_dispatch_record(
    artifacts: &orgasmic_core::DispatchAttemptArtifacts,
    project_root: &Path,
    task_id: &str,
    started_tx: &str,
    label_path: &Path,
) -> Result<orgasmic_core::PromoteOutcome, String> {
    let mut outcome = orgasmic_core::promote_validated_dispatch_attempt(
        artifacts,
        project_root,
        task_id,
        started_tx,
    )?;
    if outcome.report_path.is_some() {
        if let Err(err) = commit_promoted_dispatch_record(project_root, task_id, started_tx) {
            let commit_err = format!(
                "commit promoted dispatch record for {}: {err}",
                label_path.display()
            );
            outcome.error = Some(match outcome.error.take() {
                Some(prior) => format!("{prior}; {commit_err}"),
                None => commit_err,
            });
        }
    }
    Ok(outcome)
}

/// Put `.orgasmic/tasks/<ID>/dispatches/<started_tx>/` into git history as a
/// dedicated, path-scoped commit.
///
/// TASK-QGWK7.1 shipped this as `git add` alone. That met "in the index" but
/// broke the very next step of the flow the review gate prescribes: a staged
/// path — ANY staged path, not just one the merge touches — makes `git merge`
/// refuse with "your local changes would be overwritten", so a close followed
/// by a merge failed (TASK-QGWK7.1.1 M-0). Committing is what makes the
/// promised property real: a fresh clone can read the record, and the index is
/// clean afterwards so the merge that follows the close still runs.
///
/// The commit is built through a THROWAWAY index seeded from `HEAD`, so it can
/// contain nothing but the record directory — whatever else the manager has
/// staged is neither committed nor disturbed. Deciding when to commit the
/// manager's own change still belongs to the manager; this commits only
/// orgasmic's own bookkeeping, under a path no worker change can occupy.
// orgasmic:TASK-QGWK7.1,TASK-QGWK7.1.1
fn commit_promoted_dispatch_record(
    project_root: &Path,
    task_id: &str,
    started_tx: &str,
) -> Result<(), String> {
    use std::ffi::OsStr;

    let dest_dir = orgasmic_core::dispatch_record_dir(project_root, task_id, started_tx)?;
    let git_dir = PathBuf::from(git_capture(
        project_root,
        ["rev-parse", "--absolute-git-dir"],
    )?);
    // orgasmic:TASK-QGWK7.1.1.1 — F-3: refuse BEFORE the real `git add`, so a
    // refusal stages nothing. A record commit written inside a conflicted
    // rebase is discarded by `git rebase --abort` along with the promoted file
    // (measured; the staged-only baseline loses it identically, so this is a
    // pre-existing class, not a regression). Skipping persistence keeps the
    // files, and the re-run of the close puts them into history.
    if let Some(operation) = sequencer_operation_in_progress(&git_dir) {
        // orgasmic:TASK-QGWK7.1.1.1.1.1.1 — D-4: promise only what is true. An
        // `--abort` discards the record commit for `rebase` ALONE; for revert,
        // cherry-pick, merge and am the abort exits 0 with the record intact
        // (measured). What the refusal actually buys is the record not landing
        // mid-operation. D-1: the leftover todo list needs its own remedy,
        // because `--continue` is wrong for a range already abandoned.
        let remedy = if operation == crate::sequencer_markers::STOPPED_PICK_RANGE {
            "it is a `.git/sequencer` todo list with no pick stopped beside it — `cat \
             .git/sequencer/todo` shows what is still pending, and tells you whether this is a \
             revert or a cherry-pick. If those picks still matter, finish the range with `git \
             revert --continue` (or `git cherry-pick --continue`; the wrong verb errors and \
             names the right one). If you already abandoned it with `git reset --hard`, clear \
             the leftover list with `git revert --quit` — which ABANDONS whatever that todo \
             lists. Then re-run this close"
                .to_string()
        } else {
            format!("re-run this close once the {operation} finishes")
        };
        return Err(format!(
            "a git {operation} is in progress, so the record was promoted to disk but not \
             committed — it must not land in the middle of an operation you have not finished, \
             on a branch whose shape you are still deciding; {remedy}"
        ));
    }
    // orgasmic:TASK-QGWK7.1.1.1 — F-5: on a detached HEAD `update-ref HEAD`
    // moves only the detached HEAD, so the record commit is on NO branch and
    // the manager's next checkout orphans it — while the close reports `ok`.
    // Refuse instead, before anything is staged, and CAS the branch this close
    // actually resolved rather than whatever HEAD names at update time (F-2).
    let branch_ref = git_capture(project_root, ["symbolic-ref", "-q", "HEAD"]).map_err(|_| {
        "HEAD is detached, so a record commit would land on no branch and the next checkout \
         would orphan it; the record was promoted to disk but not committed — re-run this close \
         from a branch"
            .to_string()
    })?;
    let head = git_capture(project_root, ["rev-parse", "--verify", "HEAD^{commit}"])?;
    git_capture(
        project_root,
        [OsStr::new("add"), OsStr::new("--"), dest_dir.as_os_str()],
    )?;
    // A thinned record is worth keeping and worth naming; it is not a reason to
    // keep nothing. Commit either way and report the delta.
    let thinned = verify_dispatch_record_staged(project_root, &dest_dir);
    match write_dispatch_record_commit(project_root, &dest_dir, &head, &branch_ref) {
        Ok(()) => thinned,
        Err(err) => {
            // Never leave the record staged-but-uncommitted: that is exactly
            // the state that blocks the manager's next merge. Put the index
            // back to HEAD for this path. The promoted files stay on disk
            // either way — the close reports why they are not in history yet.
            //
            // orgasmic:TASK-QGWK7.1.1.1 — F-1: and REPORT the rollback rather
            // than dropping it behind `let _ =`. If the index lock is held at
            // this moment the restore fails, and a silently left-staged record
            // is M-0's symptom verbatim: the manager's next merge refuses with
            // no clue why.
            match git_capture(
                project_root,
                [
                    OsStr::new("restore"),
                    OsStr::new("--staged"),
                    OsStr::new("--"),
                    dest_dir.as_os_str(),
                ],
            ) {
                Ok(_) => Err(err),
                Err(restore_err) => Err(format!(
                    "{err}; AND the record is left STAGED because the rollback failed too \
                     ({restore_err}) — run `git restore --staged -- {}` before your next merge, \
                     which will otherwise refuse",
                    dest_dir.display()
                )),
            }
        }
    }
}

/// The git sequencer operation in progress in this worktree, if any
/// (TASK-QGWK7.1.1.1 F-3). A handful of `exists()` calls close the whole class:
/// only `rebase --abort` was measured to destroy a record, but `--continue`,
/// `--skip` and the merge/cherry-pick/revert equivalents all rewrite HEAD out
/// from under a commit this close just wrote.
///
/// `REVERT_HEAD` is here for CONSISTENCY, not for a measured loss
/// (TASK-QGWK7.1.1.1.1.1 C-1). TASK-QGWK7.1.1.1.1 added it on a symptom that
/// turned out to be false of this code: re-measured in the production shape —
/// the real-index `git add` above, THEN the throwaway index — a record commit
/// written inside a revert survives the operator's `git revert --abort`, which
/// exits 0 and clears `REVERT_HEAD`. The exit-128 `Untracked working tree file
/// … would be overwritten by merge` wedge reproduces only when that real-index
/// `git add` is skipped, which production never does. `merge --abort`,
/// `cherry-pick --abort` and `am --abort` end the same way — exit 0, record
/// still in `HEAD` and on disk — but do NOT share one mechanism
/// (TASK-QGWK7.1.1.1.1.1.1 D-3): a single-pick `revert`/`cherry-pick` abort and
/// `merge --abort` are a `reset --merge` to the CURRENT HEAD, so they rewind
/// and the record survives only because it already IS that HEAD; only `am`
/// declines outright (`You seem to have moved HEAD … Not rewinding`). The
/// rewinding aborts discard the manager's own conflict resolution; the `am`
/// abort, having declined to rewind, leaves it staged (measured). `rebase
/// --abort` is the one guarded operation whose abort really does destroy the
/// commit and the promoted file — and a rebase detaches HEAD (both backends),
/// so the detached-HEAD refusal above catches that case first.
///
/// What the refusal buys, then, is not rescue from an abort: it is that the
/// record does not land in the middle of an operation the manager has not
/// finished, on a branch whose shape they are still deciding. It enters history
/// once, cleanly, at a point they chose, for the cost of a promote plus a
/// re-run. The window is not only a CONFLICTED revert: a clean `git revert -n`
/// — the ordinary way to stage a revert before editing it — leaves
/// `REVERT_HEAD` present with `CHERRY_PICK_HEAD` absent (measured).
///
/// The `.git/sequencer` entry covers the stopped range that has NO `*_HEAD`
/// marker (TASK-QGWK7.1.1.1.1.1.1 D-1); see [`SEQUENCER_MARKERS`] for the
/// measurement and for why it is checked last. It does latch — `.git/sequencer`
/// survives the `git reset --hard` that abandons such a range, while
/// `REVERT_HEAD`/`CHERRY_PICK_HEAD` do not (measured) — and TASK-QGWK7.1.1.1.1.1
/// C-2 removed the entry for that reason. But the latch was never the entry's
/// fault: it was the MESSAGE, which told the manager to wait for a sequence
/// that could not finish. The refusal above now names `git revert --quit` for
/// exactly that state, which is a one-command fix rather than a permanent
/// refusal.
fn sequencer_operation_in_progress(git_dir: &Path) -> Option<&'static str> {
    for &(marker, operation, _) in SEQUENCER_MARKERS {
        if git_dir.join(marker).exists() {
            return Some(operation);
        }
    }
    None
}

/// Re-commit a promoted record whose close failed to persist it
/// (TASK-QGWK7.1.1.1 F-1).
///
/// Promotion unlinks the tmp artifacts as soon as the COPIES succeed
/// (`paths.rs`), so a persist that failed afterwards used to be terminal:
/// `promote_dispatch_artifacts_in_place` bails on `last_path missing`, the
/// re-run of the close is an `already-closed` no-op by design, and there is no
/// re-persist verb. But the promoted files are still on disk and
/// [`commit_promoted_dispatch_record`] needs only that directory — only its
/// CALLER was gated on tmp. So the no-op is exactly where the repair belongs.
///
/// Best-effort and silent when there is nothing to do: this runs on every
/// re-run of every close, and a close that persisted fine must stay a no-op.
///
/// The repair lands on the branch the RE-RUN is standing on, not the one the
/// failed close resolved (TASK-QGWK7.1.1.1.1 B-2, measured through the binary:
/// close on `feature-x` with the index locked, `git checkout main`, re-run —
/// the record commit lands on `main`, `feature-x` never moves, and going back
/// to `feature-x` afterwards removes the now-tracked files from the working
/// tree, so further re-runs there are silent no-ops). Refusing that would trade
/// a record that IS in history for one that is in none until the manager
/// remembers which branch they were on, and the record's home branch is not a
/// property anything downstream reads. So it is allowed, documented in the
/// convention, and the print below names the ref it landed on.
// orgasmic:TASK-QGWK7.1.1.1,TASK-QGWK7.1.1.1.1
fn repersist_dispatch_record_best_effort(project_root: &Path, closed: &DispatchRecord) {
    // Reachability is load-bearing on the core dispatch fold setting
    // `cleanup_already_run` for `ok`/`worktree_missing`
    // ONLY: that is what stops a staggered multi-task close stamping
    // `cleanup_already_run` over the `partial` this repair keys on.
    let Some(status) = closed_dispatch_cleanup_status(project_root, &closed.tx_id) else {
        return;
    };
    if !cleanup_status_reports_failure(&status) {
        return;
    }
    let Some(task_id) = closed.tasks.first() else {
        return;
    };
    let Ok(dest_dir) = orgasmic_core::dispatch_record_dir(project_root, task_id, &closed.tx_id)
    else {
        return;
    };
    if !dest_dir.is_dir() {
        // Nothing was promoted (or the failure was elsewhere entirely, e.g.
        // worktree removal). There is no record to put into history.
        return;
    }
    if dispatch_record_is_in_history(project_root, &dest_dir) {
        return;
    }
    // Same lock the promote path takes: this stages and commits a path another
    // close may be touching right now.
    let _cleanup_lock = match acquire_dispatch_cleanup_lock(project_root) {
        Ok(lock) => lock,
        Err(err) => {
            eprintln!("warning: dispatch record re-persist skipped: {err}");
            return;
        }
    };
    match commit_promoted_dispatch_record(project_root, task_id, &closed.tx_id) {
        // orgasmic:TASK-QGWK7.1.1.1.1 — B-3: `Ok(())` is not proof that
        // anything was committed. If `promote last.txt` fails AFTER
        // `create_dir_all` succeeds (`paths.rs`), `dest_dir` exists but is
        // EMPTY, and every step downstream reports success over nothing
        // (measured): `git add -- <empty dir>` exits 0, the throwaway index's
        // `write-tree` equals `head_tree` so the commit takes its early
        // return, and `verify_dispatch_record_staged` finds no file to miss.
        // The one line a manager would trust has to be gated on the record
        // really being in `HEAD` afterwards, or every future re-run repeats
        // the same false claim.
        Ok(()) if dispatch_record_is_in_history(project_root, &dest_dir) => println!(
            "re-persisted: dispatch record {} committed onto {} (the close that promoted it \
             reported cleanup status={status})",
            closed.tx_id,
            git_capture(project_root, ["symbolic-ref", "-q", "HEAD"])
                .unwrap_or_else(|_| "HEAD".to_string())
        ),
        // orgasmic:TASK-QGWK7.1.1.1.1.1 — C-4: name the remedy. This arm runs on
        // every re-run of the close, and the repair is skipped outright once
        // `dest_dir` stops being a directory (see the `is_dir` return above), so
        // one `rmdir` ends the warning permanently. It also does not ASSERT the
        // directory is empty: `dispatch_record_is_in_history` is
        // `unwrap_or(false)`, so a `git ls-tree` that errored after a successful
        // commit reaches here too, and a flat "holds no files" would be false.
        Ok(()) => eprintln!(
            "warning: dispatch record {} is promoted at {} but had nothing to commit and is \
             still not in git history — normally an EMPTY directory, left by the close \
             reporting cleanup status={status} failing between creating it and promoting \
             into it; if it is empty, `rmdir` it and this warning stops for good",
            closed.tx_id,
            dest_dir.display()
        ),
        Err(err) => eprintln!(
            "warning: dispatch record {} is promoted at {} but still not in git history \
             (the close reported cleanup status={status}): {err}",
            closed.tx_id,
            dest_dir.display()
        ),
    }
}

/// `CLEANUP_STATUS` recorded by the close tx that closed this generation.
fn closed_dispatch_cleanup_status(project_root: &Path, started_tx: &str) -> Option<String> {
    let entries = read_tx_entries(project_root).ok()?;
    entries
        .iter()
        .rev()
        .find(|entry| {
            matches!(
                entry.ty.as_str(),
                "implementer.done"
                    | "reviewer.done"
                    | "architector.done"
                    | "manager.dispatch_aborted"
            ) && extra(entry, "CLOSED_TX") == Some(started_tx)
        })
        .and_then(|entry| extra(entry, "CLEANUP_STATUS"))
        .map(str::to_string)
}

/// Whether the promoted record directory is already in `HEAD`'s tree, so a
/// re-run has nothing to repair.
fn dispatch_record_is_in_history(project_root: &Path, dest_dir: &Path) -> bool {
    use std::ffi::OsStr;

    git_capture(
        project_root,
        [
            OsStr::new("ls-tree"),
            OsStr::new("--name-only"),
            OsStr::new("HEAD"),
            OsStr::new("--"),
            dest_dir.as_os_str(),
        ],
    )
    .map(|listed| !listed.trim().is_empty())
    .unwrap_or(false)
}

/// Prove every promoted file actually reached the index (TASK-QGWK7.1.1 M-2).
///
/// `git add -- <dir>` exits 0 on a directory that is only PARTIALLY ignored and
/// silently stages the rest, so a project rule as ordinary as `*.log` would
/// drop `stdout.log` from the record while the close reported success. Compare
/// what is on disk against what `git ls-files` reports and name the delta.
fn verify_dispatch_record_staged(project_root: &Path, dest_dir: &Path) -> Result<(), String> {
    let rel_dir = dest_dir
        .strip_prefix(project_root)
        .map_err(|_| "promoted record is not under the project root".to_string())?;
    let mut expected: Vec<String> = Vec::new();
    for entry in std::fs::read_dir(dest_dir).map_err(|err| err.to_string())? {
        let entry = entry.map_err(|err| err.to_string())?;
        if entry.file_type().map_err(|err| err.to_string())?.is_file() {
            expected.push(
                rel_dir
                    .join(entry.file_name())
                    .to_string_lossy()
                    .into_owned(),
            );
        }
    }
    let listed = git_capture(
        project_root,
        [
            std::ffi::OsStr::new("ls-files"),
            std::ffi::OsStr::new("--"),
            dest_dir.as_os_str(),
        ],
    )?;
    let tracked: std::collections::HashSet<&str> = listed.lines().collect();
    let mut missing: Vec<&str> = expected
        .iter()
        .map(String::as_str)
        .filter(|path| !tracked.contains(path))
        .collect();
    if missing.is_empty() {
        return Ok(());
    }
    missing.sort_unstable();
    Err(format!(
        "git add did not stage every promoted file (a project ignore rule is thinning the \
         record): {}",
        missing.join(", ")
    ))
}

/// Write the record-only commit and advance the current branch to it.
///
/// Plumbing rather than `git commit` so the commit can be scoped to one
/// directory without touching the manager's staged work, and so no hook-owning
/// porcelain path runs inside a close. `update-ref` is given the old value, so
/// a branch that moved underneath this close refuses instead of clobbering.
///
/// The compare-and-swap names `branch_ref` — the ref `HEAD` resolved to when
/// this close started — rather than `HEAD` (TASK-QGWK7.1.1.1 F-2). Through
/// `HEAD` the CAS compares OIDs, not ref identity: a concurrent checkout onto a
/// SIBLING branch sitting at the same OID passes the check and lands the record
/// on a branch this close never resolved (measured). Naming the ref makes the
/// swap refuse on the one thing that matters — the branch moved — and land
/// nowhere else.
fn write_dispatch_record_commit(
    project_root: &Path,
    dest_dir: &Path,
    head: &str,
    branch_ref: &str,
) -> Result<(), String> {
    use std::ffi::OsStr;

    let head_tree_rev = format!("{head}^{{tree}}");
    let head_tree = git_capture(
        project_root,
        ["rev-parse", "--verify", head_tree_rev.as_str()],
    )?;
    let git_dir = git_capture(project_root, ["rev-parse", "--absolute-git-dir"])?;
    let index_path =
        PathBuf::from(git_dir).join(format!("orgasmic-record-index-{}", std::process::id()));
    let _ = std::fs::remove_file(&index_path);
    let built = (|| {
        let scratch = [("GIT_INDEX_FILE", index_path.as_os_str())];
        git_capture_env(project_root, ["read-tree", head], scratch)?;
        git_capture_env(
            project_root,
            [OsStr::new("add"), OsStr::new("--"), dest_dir.as_os_str()],
            scratch,
        )?;
        let tree = git_capture_env(project_root, ["write-tree"], scratch)?;
        if tree == head_tree {
            // A replayed close of the same generation: the record is already
            // in history. Committing again would only add an empty commit.
            return Ok(());
        }
        let message = format!(
            "chore(orgasmic): dispatch record {}",
            dest_dir
                .file_name()
                .map(|name| name.to_string_lossy().into_owned())
                .unwrap_or_default()
        );
        let commit = git_capture(
            project_root,
            [
                "commit-tree",
                "-p",
                head,
                "-m",
                message.as_str(),
                tree.as_str(),
            ],
        )?;
        git_capture(
            project_root,
            [
                "update-ref",
                "-m",
                "orgasmic dispatch record",
                branch_ref,
                commit.as_str(),
                head,
            ],
        )?;
        Ok(())
    })();
    let _ = std::fs::remove_file(&index_path);
    built
}

fn git_capture<I, S>(project_root: &Path, args: I) -> Result<String, String>
where
    I: IntoIterator<Item = S>,
    S: AsRef<std::ffi::OsStr>,
{
    let no_env: [(&str, &std::ffi::OsStr); 0] = [];
    git_capture_env(project_root, args, no_env)
}

fn git_capture_env<'a, I, S, E>(project_root: &Path, args: I, env: E) -> Result<String, String>
where
    I: IntoIterator<Item = S>,
    S: AsRef<std::ffi::OsStr>,
    E: IntoIterator<Item = (&'a str, &'a std::ffi::OsStr)>,
{
    let mut command = Command::new("git");
    command.current_dir(project_root);
    let mut label = String::new();
    for (index, arg) in args.into_iter().enumerate() {
        if index == 0 {
            label = arg.as_ref().to_string_lossy().into_owned();
        }
        command.arg(arg);
    }
    for (key, value) in env {
        command.env(key, value);
    }
    let output = command.output().map_err(|err| err.to_string())?;
    if !output.status.success() {
        return Err(format!(
            "git {label} failed: {}{}",
            String::from_utf8_lossy(&output.stderr).trim(),
            String::from_utf8_lossy(&output.stdout).trim()
        ));
    }
    Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
}

fn push_cleanup_extra(extra: &mut Vec<(String, String)>, cleanup: &CleanupOutcome) {
    extra.push((
        "CLEANUP_STATUS".to_string(),
        cleanup.status.as_str().to_string(),
    ));
    if let Some(error) = optional_value(cleanup.error.as_deref()) {
        extra.push(("CLEANUP_ERROR".to_string(), error));
    }
    if let Some(report_path) = optional_value(cleanup.report_path.as_deref()) {
        // orgasmic:TASK-QGWK7 — close tx names the durable report so a
        // body-less completion entry is resolvable after tmp/ is gone.
        extra.push(("REPORT_PATH".to_string(), report_path));
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
    let home = Home::from_env().context("resolve orgasmic home")?;
    find_project_root_optional_from(&home, &std::env::current_dir().context("cwd")?)?
        .ok_or_else(|| anyhow::anyhow!("could not resolve a registered orgasmic project from cwd"))
}

pub(crate) fn find_project_root_optional_from(home: &Home, cwd: &Path) -> Result<Option<PathBuf>> {
    let mut dir = cwd.to_path_buf();
    loop {
        if dir.join(".orgasmic/project.org").is_file() {
            return Ok(Some(dir));
        }
        if !dir.pop() {
            break;
        }
    }

    let Some(common_dir) = git_common_dir(cwd) else {
        return Ok(None);
    };
    let matches = projects::read_board(home)?
        .into_iter()
        .filter(|entry| git_common_dir(&entry.path).as_ref() == Some(&common_dir))
        .map(|entry| entry.path)
        .collect::<Vec<_>>();
    match matches.as_slice() {
        [] => Ok(None),
        [root] => Ok(Some(root.clone())),
        _ => bail!(
            "multiple registered projects share git common dir {}",
            common_dir.display()
        ),
    }
}

fn git_common_dir(path: &Path) -> Option<PathBuf> {
    let output = Command::new("git")
        .args(["rev-parse", "--git-common-dir"])
        .current_dir(path)
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let value = PathBuf::from(String::from_utf8_lossy(&output.stdout).trim().to_string());
    std::fs::canonicalize(if value.is_absolute() {
        value
    } else {
        path.join(value)
    })
    .ok()
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
    let path = task_node_file_path(project_root, task_id);
    let source = std::fs::read_to_string(&path)
        .with_context(|| format!("task {task_id} not found at {}", path.display()))?;
    let file = OrgFile::parse(source, path.to_string_lossy())?;
    let heading = file
        .headings
        .first()
        .context("task node.org has no heading")?;
    if heading.property("ID") != Some(task_id) {
        bail!("{} does not contain task {task_id}", path.display());
    }
    let fix_subtask = heading
        .property("FIX_SUBTASK")
        .map(trueish_property_value)
        .unwrap_or(false);
    let task = TaskHeading::from_heading(&file, heading, path.to_string_lossy().as_ref())?;
    Ok(TaskLifecycleInfo {
        id: task.id.to_string(),
        stage: task.lifecycle_stage,
        fix_subtask,
    })
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
    };
    tasks.iter().map(|task| (task.clone(), stage)).collect()
}

/// The lifecycle move a `dispatch-close` intends for one task, recorded on the
/// close tx itself (`LIFECYCLE_FROM`/`LIFECYCLE_TO`) before the transition is
/// attempted.
///
/// The tx keeps this intent for the derived transition view and so the
/// legacy-only [`reconcile_torn_closes`] can repair pre-atomic closes.
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
        // `open.kind` is the string on the persisted `manager.dispatch_started`
        // record, not a `DispatchKind`. The `architector` arms are legacy
        // (dec_HBK6A) and exist so a dispatch opened before the excision still
        // closes; nothing can open a new one.
        let stage = match args.status {
            DispatchCloseStatus::Aborted => match open.kind.as_str() {
                "implementer" => LifecycleStage::Todo,
                "reviewer" => LifecycleStage::InReview,
                "architector" => LifecycleStage::Todo,
                other => bail!("cannot close dispatch kind `{other}` as aborted"),
            },
            DispatchCloseStatus::Done => match open.kind.as_str() {
                "implementer" => implementer_done_stage(info.fix_subtask, args.fix_round_final),
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

/// THE rule for where an implementer's `--status done` close lands. Every
/// answer to "does a fix round get its own review" comes from this function;
/// `shipped/prompt-studio/conventions/manager-dispatch.org` and
/// `shipped/schema/state-machine.org` describe it by naming
/// `--fix-round-final`, and `cli_parity` fails if that flag stops existing —
/// so the prose cannot drift into being a second, disagreeing answer.
///
/// orgasmic:TASK-4WKNX — the default used to be inverted: `:FIX_SUBTASK: t`
/// closed straight to `done`, while a reviewer dispatch is refused FROM `done`.
/// So a fix round minted from a BLOCK SHIP finding could only be reviewed if
/// the manager knew to flip `done` -> `in_review` by hand, and the goal clause
/// that requires that review was unenforceable by anything the board could
/// express. On 2026-08-05 one such round (TASK-M47E5.2, three data-loss
/// findings) was itself REJECTED with three more BLOCK SHIP findings, two P0 —
/// under the old default those would have landed unreviewed. The two failure
/// modes are not symmetric: a trivial fix waiting for a cheap review is
/// recoverable, an unreviewed fix for a data-loss finding is what the goal
/// exists to prevent. Hence review by default, `--fix-round-final` to opt out.
fn implementer_done_stage(fix_subtask: bool, fix_round_final: bool) -> LifecycleStage {
    if fix_subtask && fix_round_final {
        LifecycleStage::Done
    } else {
        LifecycleStage::InReview
    }
}

/// Enforce the generic-property policy for every manager-owned close-tx key.
///
/// This runs in `dispatch_close` with the other argument refusals, before merge
/// verification, run release, cleanup, tx append, or lifecycle mutation.
fn validate_manager_owned_close_properties(args: &DispatchCloseArgs) -> Result<()> {
    for owned in MANAGER_OWNED_CLOSE_PROPERTIES {
        let Some(property_value) = close_property_value(args, owned.key) else {
            continue;
        };
        match owned.policy {
            ManagerOwnedClosePropertyPolicy::Reserved => {
                bail!(
                    "--property {}={} cannot set manager-owned close property {}; use {} instead",
                    owned.key,
                    property_value,
                    owned.key,
                    owned.typed_flag
                );
            }
            ManagerOwnedClosePropertyPolicy::AliasedVerdict => {
                let Some(flag) = args.verdict else {
                    continue;
                };
                // TASK-YN5FJ.1 RULING 3: preserve this refusal verbatim. Both
                // spellings write the same key, so neither silently wins.
                bail!(
                    "--verdict {} and --property VERDICT={} both set the same VERDICT property on this \
                     close: pass exactly one (--verdict <{}> for the canonical vocabulary, \
                     --property VERDICT=<value> for a legacy free-text spelling)",
                    flag.as_str(),
                    property_value,
                    ReviewVerdict::value_list()
                );
            }
        }
    }
    Ok(())
}

/// Refuse a `--fix-round-final` that cannot mean what it says.
///
/// orgasmic:TASK-4WKNX — `--reason` is required on the same argument that makes
/// `--no-review-required` require one: this is a bypass of a safety default,
/// and a bypass nobody has to justify is a bypass nobody can audit afterwards.
/// The non-fix-subtask refusal exists because there the flag would be a silent
/// no-op — that close already lands `in_review` — and a flag that quietly does
/// nothing is how an operator comes to believe it did something.
fn validate_fix_round_final(
    project_root: &Path,
    tasks: &[String],
    args: &DispatchCloseArgs,
    tx_type: &str,
) -> Result<()> {
    if !args.fix_round_final {
        return Ok(());
    }
    if !(args.status == DispatchCloseStatus::Done && tx_type == "implementer.done") {
        bail!(
            "--fix-round-final is valid only when closing an implementer dispatch as done: it \
             opts a fix round out of its own REVIEW ROUND. (The default-branch merge gate is a \
             different thing with a different flag: --no-review-required.)"
        );
    }
    if args
        .reason
        .as_ref()
        .map(|reason| sanitize_tx_value(reason))
        .filter(|reason| !reason.is_empty())
        .is_none()
    {
        bail!("--fix-round-final requires --reason so the skipped review is auditable");
    }
    let mut not_fix_rounds = Vec::new();
    for task in tasks {
        if !read_task_lifecycle(project_root, task)?.fix_subtask {
            not_fix_rounds.push(task.clone());
        }
    }
    if !not_fix_rounds.is_empty() {
        bail!(
            "--fix-round-final is valid only for a task carrying :FIX_SUBTASK:; {} does not, and \
             its close already lands in_review",
            not_fix_rounds.join(" ")
        );
    }
    Ok(())
}

fn reviewer_done_stage(args: &DispatchCloseArgs) -> LifecycleStage {
    // orgasmic:TASK-YN5FJ.1 — a pure superset of the legacy mapping: `approve`
    // joins the free-text `clean`/`ship` as clean, and `approve-with-follow-ups`
    // / `reject` land where every other non-clean value already lands. This,
    // not the default-branch gate, is where a bad verdict has its consequence
    // (RULING 1): the gate asks whether a review happened, this asks what it
    // said.
    let verdict_clean = close_verdict_value(args)
        .map(|value| value == "clean" || value == "ship" || value == "approve")
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

/// The `VERDICT` this close records, whichever spelling wrote it. Only one can
/// be present: `dispatch_close` refuses a close that passes both (RULING 3).
fn close_verdict_value(args: &DispatchCloseArgs) -> Option<&str> {
    args.verdict
        .map(ReviewVerdict::as_str)
        .or_else(|| close_property_value(args, "VERDICT"))
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
    closed_tx: &str,
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
                "repair_closed_tx": closed_tx,
                "request_id": request_id,
            }),
        )
        .await
}

/// Finish a legacy `dispatch-close` whose lifecycle leg never landed.
///
/// orgasmic:task_EP3H1 — a close appends its tx and then transitions the task
/// in a second daemon request. Under load the second one times out
/// client-side (measured at load average ~190 on 2026-07-29) and the operator
/// is left with a closed dispatch and a task stranded at its pre-close stage.
/// New closes append that tx and rewrite node.org in one `transaction_multi`,
/// so only pre-AP971 closes can reach this repair. The close tx records the
/// transition it intended, so the repair is decidable
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
            &started_tx,
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
            // The manager dispatch path writes the lifecycle transition before
            // the daemon appends manager.dispatch_started. The reviewer-chain
            // integration test and the unit case below both require this as
            // the durable evidence that an older close no longer needs repair.
            "task.state_transitioned" | "manager.dispatch_started" => {
                pending.retain(|(_, pending)| pending.task != task);
            }
            _ => {}
        }
    }
    Ok(pending)
}

/// Already-closed tasks whose pre-atomic terminal tx still needs the legacy
/// replay performed by `dispatch-close`.
///
/// The ledger match is deliberately exact on both generation (`CLOSED_TX`) and
/// task. A close carrying lifecycle metadata belongs to the atomic/reconciler
/// path and is never replayed here. An old-format close is eligible only while
/// the task remains at the active stage established when that dispatch kind was
/// opened; once an operator or later workflow moves it, the old close loses the
/// right to mutate it.
fn legacy_close_replay_tasks(
    project_root: &Path,
    open: &DispatchRecord,
    tasks: &[String],
) -> Result<BTreeSet<String>> {
    let Some(active_stage) = legacy_close_active_stage(&open.kind) else {
        return Ok(BTreeSet::new());
    };
    let entries = read_tx_entries(project_root)?;
    let mut eligible = BTreeSet::new();
    for task in tasks
        .iter()
        .filter(|task| open.closed_tasks.contains(*task))
    {
        let close = entries.iter().rev().find(|entry| {
            matches!(
                entry.ty.as_str(),
                "implementer.done"
                    | "reviewer.done"
                    | "architector.done"
                    | "manager.dispatch_aborted"
            ) && extra(entry, "CLOSED_TX") == Some(open.tx_id.as_str())
                && entry.task.as_deref() == Some(task.as_str())
        });
        let Some(close) = close else {
            continue;
        };
        if extra(close, LIFECYCLE_FROM_KEY).is_some() || extra(close, LIFECYCLE_TO_KEY).is_some() {
            continue;
        }
        if read_task_lifecycle(project_root, task)
            .map(|info| info.stage == active_stage)
            .unwrap_or(false)
        {
            eligible.insert(task.clone());
        }
    }
    Ok(eligible)
}

fn legacy_close_active_stage(kind: &str) -> Option<LifecycleStage> {
    match kind {
        "implementer" | "architector" => Some(LifecycleStage::InProgress),
        "reviewer" => Some(LifecycleStage::InReview),
        _ => None,
    }
}

fn dispatchable_stage(kind: DispatchKind, stage: LifecycleStage) -> bool {
    match kind {
        DispatchKind::Implementer => {
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
    }
}

#[derive(Debug)]
struct VerifiedMergeEvidence {
    sha: String,
    worker_commit: Option<String>,
}

fn verify_merge_evidence(
    project_root: &Path,
    merge_sha: &str,
    worker_commit: Option<&str>,
) -> Result<VerifiedMergeEvidence> {
    let merge_sha = resolve_commit(project_root, merge_sha).map_err(|_| {
        anyhow::anyhow!(
            "--merge-sha `{}` does not resolve to a commit in {}",
            merge_sha,
            project_root.display()
        )
    })?;
    let parents = Command::new("git")
        .args(["rev-list", "--parents", "-n", "1", &merge_sha])
        .current_dir(project_root)
        .output()
        .context("git rev-list merge parents")?;
    if !parents.status.success() {
        bail!(
            "cannot inspect --merge-sha `{merge_sha}` parents: {}{}",
            String::from_utf8_lossy(&parents.stderr),
            String::from_utf8_lossy(&parents.stdout)
        );
    }
    let parent_oids: Vec<String> = String::from_utf8_lossy(&parents.stdout)
        .split_whitespace()
        .map(str::to_string)
        .collect();
    if parent_oids.len() < 3 {
        bail!("--merge-sha `{merge_sha}` is not a merge commit");
    }
    verify_merged_gitlinks_reach_their_origin(project_root, &merge_sha, &parent_oids[1])?;

    let worker_commit = worker_commit
        .map(|worker_commit| {
            resolve_commit(project_root, worker_commit).map_err(|_| {
                anyhow::anyhow!(
                    "--worker-commit `{worker_commit}` does not resolve to a commit in {}",
                    project_root.display()
                )
            })
        })
        .transpose()?;
    if let Some(worker_commit) = worker_commit.as_deref() {
        let contained = Command::new("git")
            .args(["merge-base", "--is-ancestor", worker_commit, &merge_sha])
            .current_dir(project_root)
            .status()
            .context("git merge-base --is-ancestor for --worker-commit")?;
        match contained.code() {
            Some(0) => {}
            Some(1) => bail!(
                "--merge-sha `{merge_sha}` does not contain --worker-commit `{worker_commit}`"
            ),
            _ => bail!(
                "cannot verify whether --merge-sha `{merge_sha}` contains --worker-commit \
                 `{worker_commit}` (git merge-base exit {})",
                contained
            ),
        }
    }

    Ok(VerifiedMergeEvidence {
        sha: merge_sha,
        worker_commit,
    })
}

/// A merged gitlink bump must point at a commit the submodule's ORIGIN already
/// has. The dispatch worktree's submodule store is private and settled away at
/// cleanup (`settle_as_initialized_submodules`), so a commit that exists only
/// there would leave the default branch pointing at an oid no other machine —
/// or CI — can fetch. Reachability is read from the MAIN checkout's submodule:
/// a remote-tracking ref containing the oid means the sub-repo's own
/// merge-and-push already happened, which is the ordered two-step this model
/// prescribes (the sub-repo project merges first, the parent bumps second).
/// Unverifiable states — submodule not initialized in the main checkout, or an
/// oid the local clone has never fetched — refuse with the step that fixes
/// them rather than pass.
fn verify_merged_gitlinks_reach_their_origin(
    project_root: &Path,
    merge_sha: &str,
    first_parent: &str,
) -> Result<()> {
    let raw = Command::new("git")
        .args(["diff", "--raw", first_parent, merge_sha])
        .current_dir(project_root)
        .output()
        .context("git diff --raw for gitlink verification")?;
    if !raw.status.success() {
        bail!(
            "cannot diff --merge-sha `{merge_sha}` against its first parent for gitlink \
             verification: {}{}",
            String::from_utf8_lossy(&raw.stderr),
            String::from_utf8_lossy(&raw.stdout)
        );
    }
    for line in String::from_utf8_lossy(&raw.stdout).lines() {
        // `:<old_mode> <new_mode> <old_oid> <new_oid> <status>\t<path>`
        let Some((meta, path)) = line.split_once('\t') else {
            continue;
        };
        let fields: Vec<&str> = meta.split_whitespace().collect();
        if fields.len() < 5 || fields[1] != "160000" {
            continue;
        }
        let new_oid = fields[3];
        let sub = project_root.join(path);
        if !sub.join(".git").exists() {
            bail!(
                "--merge-sha `{merge_sha}` bumps submodule `{path}` to `{new_oid}`, but the \
                 submodule is not initialized in the main checkout, so the bump cannot be \
                 verified against its origin. `git submodule update --init -- {path}`, then \
                 re-run"
            );
        }
        let reachable = Command::new("git")
            .args(["branch", "-r", "--contains", new_oid])
            .current_dir(&sub)
            .output()
            .with_context(|| format!("git branch -r --contains in submodule {path}"))?;
        if !reachable.status.success()
            || String::from_utf8_lossy(&reachable.stdout).trim().is_empty()
        {
            bail!(
                "--merge-sha `{merge_sha}` bumps submodule `{path}` to `{new_oid}`, which is not \
                 reachable from any remote-tracking ref of the submodule checkout at {} — a \
                 commit that exists only in a dispatch worktree's private store would leave the \
                 default branch pointing at an oid no other machine can fetch. Land it in the \
                 submodule's own repository first (its own task, merged and pushed), then \
                 `git -C {} fetch` and re-run",
                sub.display(),
                sub.display()
            );
        }
    }
    Ok(())
}

fn merge_lands_on_default_branch(
    home: &Home,
    project_id: &str,
    project_root: &Path,
    merge_sha: &str,
) -> Result<bool> {
    let default_branch = projects::read_board(home)
        .context("read project board for default-branch merge verification")?
        .into_iter()
        .find(|entry| entry.id == project_id)
        .map(|entry| entry.branch)
        .filter(|branch| !branch.trim().is_empty())
        .ok_or_else(|| {
            anyhow::anyhow!(
                "cannot verify --merge-sha against the default branch: project {project_id} \
                 has no BRANCH in the project board"
            )
        })?;
    let local_ref = format!("refs/heads/{default_branch}");
    let remote_ref = format!("refs/remotes/origin/{default_branch}");
    let default_ref = if resolve_commit(project_root, &local_ref).is_ok() {
        local_ref
    } else if resolve_commit(project_root, &remote_ref).is_ok() {
        remote_ref
    } else {
        bail!(
            "cannot verify --merge-sha against default branch `{default_branch}`: neither \
             `{local_ref}` nor `{remote_ref}` resolves"
        );
    };
    let contained = Command::new("git")
        .args(["merge-base", "--is-ancestor", merge_sha, &default_ref])
        .current_dir(project_root)
        .status()
        .with_context(|| format!("git merge-base --is-ancestor {merge_sha} {default_ref}"))?;
    match contained.code() {
        Some(0) => Ok(true),
        Some(1) => Ok(false),
        _ => bail!(
            "cannot verify whether --merge-sha `{merge_sha}` is on default branch \
             `{default_branch}` (git merge-base exit {contained})"
        ),
    }
}

fn reviewer_verdict_exists(
    project_root: &Path,
    tasks: &[String],
    reviewed_dispatch_tx: &str,
) -> Result<bool> {
    let entries = read_tx_entries(project_root)?;
    Ok(tasks.iter().all(|task| {
        let linked_reviewers = entries
            .iter()
            .filter(|entry| entry.ty == "manager.dispatch_started")
            .filter(|entry| extra(entry, "KIND") == Some("reviewer"))
            .filter(|entry| {
                entry
                    .task
                    .as_deref()
                    .map(split_task_list)
                    .unwrap_or_default()
                    .iter()
                    .any(|reviewed_task| reviewed_task == task)
            })
            .filter(|entry| {
                extra(entry, "REVIEWS_TX")
                    .map(|value| {
                        value
                            .split_whitespace()
                            .any(|tx| tx == reviewed_dispatch_tx)
                    })
                    .unwrap_or(false)
            })
            .map(|entry| entry.tx_id.as_str())
            .collect::<BTreeSet<_>>();
        entries.iter().any(|entry| {
            entry.ty == "reviewer.done"
                && entry
                    .task
                    .as_deref()
                    .map(split_task_list)
                    .unwrap_or_default()
                    .iter()
                    .any(|reviewed_task| reviewed_task == task)
                && extra(entry, "CLOSED_TX")
                    .map(|closed_tx| linked_reviewers.contains(closed_tx))
                    .unwrap_or(false)
                && extra(entry, "VERDICT")
                    .map(|verdict| !verdict.trim().is_empty())
                    .unwrap_or(false)
        })
    }))
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

/// Per-kind directory name of a managed worktree. Each dispatch kind owns a
/// distinct one so `cmd_dispatch`'s cross-kind reuse guard has something to
/// compare.
fn worktree_stem(task: &str, kind: DispatchKind) -> String {
    let slug = task_slug(task);
    match kind {
        DispatchKind::Implementer => slug,
        DispatchKind::Reviewer => format!("{slug}-review"),
    }
}

/// Root of this project's MANAGED worktrees: `<home>/worktrees/<project-id>/`.
///
/// Deliberately OUTSIDE the project. macOS pins a TCC grant for a linker- or
/// ad-hoc-signed binary to that binary's CDHASH, so a worker that builds and
/// then runs a binary inside a guarded project (`~/Documents`, `~/Desktop`,
/// `~/Downloads`, iCloud Drive) earns a grant that dies at its very next
/// rebuild. TASK-3X5AQ measured both halves — five dead grants for five dead
/// worktrees, and two cdhashes for two builds at one stable path — so no path
/// scheme can make a grant survive. The durable answer is to stop needing one:
/// `~/.orgasmic` is unguarded.
///
/// Keyed on project ID rather than display name: the id is what the CLI and
/// daemon already address projects by, and names collide and change.
///
/// Universal, not macOS-gated. Linux has no TCC, but the same move keeps
/// multi-GB build trees out of the repo and leaves `git status` clean, and one
/// code path beats a platform branch.
// orgasmic:TASK-M47E5
fn managed_worktree_root(home: &Home, project_id: &str) -> Result<PathBuf> {
    let id = project_id.trim();
    if id.is_empty()
        || id == "."
        || id == ".."
        || id.contains('/')
        || id.contains('\\')
        || id.contains('\0')
    {
        bail!("project id {project_id:?} cannot name a managed worktree directory");
    }
    Ok(home.root.join("worktrees").join(id))
}

/// Managed default worktree path: `<home>/worktrees/<project-id>/<task-slug>`.
/// Only the SCRATCH moves — the dispatch record (brief, `last.txt`, stdout log)
/// stays under `<project>/.orgasmic/tmp/dispatch/<stem>/`, because it is small,
/// durable, and written by the already-granted daemon.
// orgasmic:TASK-M47E5
fn default_worktree(
    home: &Home,
    project_id: &str,
    task: &str,
    kind: DispatchKind,
) -> Result<PathBuf> {
    Ok(managed_worktree_root(home, project_id)?.join(worktree_stem(task, kind)))
}

fn default_branch(task: &str, kind: DispatchKind) -> String {
    let slug = task_slug(task);
    match kind {
        DispatchKind::Implementer => format!("{slug}-impl"),
        DispatchKind::Reviewer => format!("{slug}-review"),
    }
}

fn dispatch_artifact_stem(brief_path: &Path) -> Result<(String, String)> {
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
    // orgasmic:TASK-M47E5.1.1.1
    // These names would make `join(stem)` equal to the dispatch root (or its
    // parent), colliding artifacts across dispatches before the adapter ever
    // sees the resulting config.
    if stem.is_empty() || stem == "." || stem == ".." {
        bail!(
            "refusing brief name {file_name:?}: dispatch artifact stem must not be empty, '.' or '..' (derived {stem:?})"
        );
    }
    Ok((file_name, stem))
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
                dispatch_artifact_paths_for_attempt(project_root, brief_path, &attempt_id)?;
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
) -> Result<(PathBuf, PathBuf, PathBuf)> {
    let (file_name, stem) = dispatch_artifact_stem(brief_path)?;
    let dir = project_dispatch_dir(project_root).join(&stem);
    Ok((
        dir.join(file_name),
        dir.join(format!("{stem}-{attempt_id}-last.txt")),
        dir.join(format!("{stem}-{attempt_id}-stdout.log")),
    ))
}

/// Derive the last/stdout paths as siblings of an already-resolved brief when
/// the attempt id is known (e.g. from a live run's recorded `last_path`).
fn dispatch_sibling_artifact_paths(brief_path: &Path) -> Option<(PathBuf, PathBuf)> {
    let parent = brief_path.parent().unwrap_or_else(|| Path::new("."));
    let (_, stem) = dispatch_artifact_stem(brief_path).ok()?;
    Some((
        parent.join(format!("{stem}-last.txt")),
        parent.join(format!("{stem}-stdout.log")),
    ))
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

fn open_dispatches_overlapping_tasks(
    project_root: &Path,
    tasks: &[String],
) -> Result<Vec<DispatchRecord>> {
    let open = scan_open_dispatches(project_root)?;
    Ok(open
        .into_iter()
        .filter(|record| {
            record
                .tasks
                .iter()
                .any(|task| tasks.iter().any(|requested| requested == task))
        })
        .collect())
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
    Ok(fold_dispatches(&read_tx_entries(project_root)?)
        .into_iter()
        .map(|dispatch| {
            let mut record = dispatch_record_from_fold(dispatch);
            // Tx records store project-relative paths (no user-specific
            // prefixes in committed files); resolve them back against the
            // project root for local use (ps matching, cleanup).
            for path in [&mut record.worktree, &mut record.brief_path] {
                if let Some(path) = path.as_mut().filter(|path| path.is_relative()) {
                    *path = project_root.join(&path);
                }
            }
            record
        })
        .collect())
}

fn dispatch_record_from_fold(dispatch: DispatchFold) -> DispatchRecord {
    let started = &dispatch.started;
    let run = dispatch.run.as_ref();
    let run_extra = |key| run.and_then(|entry| extra(entry, key));
    DispatchRecord {
        tx_id: started.tx_id.clone(),
        tasks: started
            .task
            .as_deref()
            .map(split_task_list)
            .unwrap_or_default(),
        kind: extra(started, "KIND").unwrap_or_default().to_string(),
        worktree: extra(started, "WORKTREE").map(PathBuf::from),
        branch: extra(started, "BRANCH").map(str::to_string),
        model: extra_compat(started, "MODEL", "CODEX_MODEL").map(str::to_string),
        effort: extra_compat(started, "EFFORT", "CODEX_EFFORT").map(str::to_string),
        brief_path: extra_compat(started, "BRIEF_PATH", "CODEX_BRIEF_PATH").map(PathBuf::from),
        last_path: run_extra("LAST_PATH").map(PathBuf::from),
        stdout_path: run_extra("STDOUT_PATH").map(PathBuf::from),
        dispatch_attempt_token: run_extra("DISPATCH_ATTEMPT").map(str::to_string),
        run_id: dispatch.addressed_run_id,
        run_ids: dispatch.run_ids,
        worker_id: run_extra("WORKER").map(str::to_string),
        driver: run_extra("DRIVER").map(str::to_string),
        harness: run_extra("HARNESS").map(str::to_string),
        pid: run_extra("PID").and_then(|pid| pid.parse().ok()),
        started_at: extra(started, "STARTED_AT")
            .map(str::to_string)
            .or_else(|| Some(started.time.clone())),
        worker_pid: extra_compat(started, "WORKER_PID", "CODEX_PID")
            .and_then(|pid| pid.parse().ok()),
        goal_id: extra(started, "GOAL_ID").map(str::to_string),
        closed_tasks: dispatch.closed_tasks,
        cleanup_already_run: dispatch.cleanup_already_run,
        reported: dispatch.reported,
        closed: dispatch.closed,
    }
}

fn scan_open_dispatches(project_root: &Path) -> Result<Vec<DispatchRecord>> {
    Ok(scan_dispatches(project_root)?
        .into_iter()
        .filter(|record| !record.closed)
        .collect())
}

fn read_tx_entries(project_root: &Path) -> Result<Vec<TxEntry>> {
    // Project tx lives in the legacy `.orgasmic/tx/` and, since TASK-MSYN4, in
    // per-machine `.orgasmic/machines/<machine-id>/tx/`. This fold backs
    // dispatch-status/wait/close, so reading only the legacy directory makes
    // every dispatch invisible on a machine writing the new layout.
    let dotorg = project_root.join(".orgasmic");
    let mut tx_dirs = vec![dotorg.join("tx")];
    if let Ok(machines) = std::fs::read_dir(dotorg.join("machines")) {
        for machine in machines.flatten() {
            tx_dirs.push(machine.path().join("tx"));
        }
    }
    let mut paths = Vec::new();
    for tx_dir in tx_dirs {
        if !tx_dir.is_dir() {
            continue;
        }
        for entry in
            std::fs::read_dir(&tx_dir).with_context(|| format!("read {}", tx_dir.display()))?
        {
            let entry = entry.with_context(|| format!("read entry in {}", tx_dir.display()))?;
            let path = entry.path();
            if path.extension().and_then(|s| s.to_str()) == Some("org") {
                paths.push(path);
            }
        }
    }
    if paths.is_empty() {
        return Ok(Vec::new());
    }
    // Sort by file name, not full path: the month file is the ordering key, and
    // the same month from two machines must interleave by name, not by root.
    paths.sort_by(|a, b| {
        a.file_name()
            .cmp(&b.file_name())
            .then_with(|| a.as_path().cmp(b.as_path()))
    });
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

/// `[pid-alive]` / `[pid-gone]`, or nothing when no pid is known: a driver
/// that cannot report a pid must not read as a dead worker.
fn pid_flag(health: &DispatchHealth) -> &'static str {
    match (health.pid, health.pid_alive) {
        (None, _) => "",
        (Some(_), true) => "[pid-alive]",
        (Some(_), false) => "[pid-gone]",
    }
}

fn dispatch_health(record: &DispatchRecord, live_runs: &[RunSummary]) -> DispatchHealth {
    let worktree_exists = record
        .worktree
        .as_ref()
        .map(|path| path.exists())
        .unwrap_or(false);
    // pid 0 is "the driver could not report one" (ws transports), not a dead
    // process; keep it unknown so the status line omits the pid flag rather
    // than printing a [pid-gone] that is always true for a live ws run.
    let derived_pid = match record.worker_pid {
        Some(pid) => Some(pid),
        None if record.pid.is_some() => record.pid,
        None => derive_worker_pid(record),
    }
    .filter(|pid| *pid != 0);
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
    let (last_path, _) = match record.last_path.as_ref() {
        Some(path) => dispatch_sibling_artifact_paths_from_last(path),
        None => dispatch_sibling_artifact_paths(brief_path)?,
    };
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
        // orgasmic:task_ZKZBF — tx property readers (`extra` and friends) match
        // keys byte for byte, same as Heading::property, so a miscased key
        // would be recorded on the close tx and never read. Name the canonical
        // spelling when one exists.
        let canonical = key.to_ascii_uppercase();
        if is_uppercase_snake_key(&canonical) {
            return Err(format!(
                "property key `{key}` is not the canonical spelling; close-tx readers match keys \
                 byte for byte, so `:{key}:` would be recorded and never read — use `{canonical}`"
            ));
        }
        return Err("property key must match [A-Z][A-Z0-9_]*".to_string());
    }
    Ok((key.to_string(), raw_value.to_string()))
}

fn sanitize_tx_value(value: &str) -> String {
    value
        .chars()
        .map(|ch| if ch == '\n' || ch == '\r' { ' ' } else { ch })
        .collect::<String>()
        .trim()
        .to_string()
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

    #[test]
    fn dispatch_wait_timeout_parser_is_explicit_and_nonzero() {
        assert_eq!(parse_wait_duration("30s").unwrap(), Duration::from_secs(30));
        assert_eq!(parse_wait_duration("2m").unwrap(), Duration::from_secs(120));
        assert!(parse_wait_duration("0s").is_err());
        assert!(parse_wait_duration("30").is_err());
    }

    #[test]
    fn unknown_pid_prints_no_flag_and_zero_counts_as_unknown() {
        let mut record = architector_record();
        record.worker_pid = Some(0);
        record.pid = None;
        record.harness = Some("hermes".to_string());
        let health = dispatch_health(&record, &[]);
        assert_eq!(health.pid, None);
        assert_eq!(pid_flag(&health), "");
        record.worker_pid = Some(std::process::id());
        let health = dispatch_health(&record, &[]);
        assert_eq!(pid_flag(&health), "[pid-alive]");
    }

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
    fn future_manager_close_extra_wins_without_a_reserved_table_entry() {
        const FUTURE_MANAGER_KEY: &str = "FUTURE_MANAGER_CLOSE_FACT";
        assert!(!MANAGER_OWNED_CLOSE_PROPERTIES
            .iter()
            .any(|property| property.key == FUTURE_MANAGER_KEY));

        let properties = vec![(FUTURE_MANAGER_KEY.to_string(), "forged".to_string())];
        let mut close = TxEntry::new("tx-close", "implementer.done", "now", "agent", "host");
        close.extra = finish_close_tx_extras(
            vec![(FUTURE_MANAGER_KEY.to_string(), "authoritative".to_string())],
            &properties,
        );

        assert_eq!(extra(&close, FUTURE_MANAGER_KEY), Some("authoritative"));
    }

    #[test]
    fn derives_slug_defaults_for_task_paths() {
        let home = Home::at("/home/.orgasmic");
        assert_eq!(task_slug("TASK-047.5.1"), "task-047.5.1");
        // TASK-M47E5: the managed default lives under the HOME, keyed on the
        // project id — never under the project, which may sit in a
        // TCC-guarded directory.
        assert_eq!(
            default_worktree(&home, "orgasmic", "TASK-047.5.1", DispatchKind::Implementer).unwrap(),
            PathBuf::from("/home/.orgasmic/worktrees/orgasmic/task-047.5.1")
        );
        assert_eq!(
            default_worktree(&home, "orgasmic", "TASK-047.5.1", DispatchKind::Reviewer).unwrap(),
            PathBuf::from("/home/.orgasmic/worktrees/orgasmic/task-047.5.1-review")
        );
        assert_eq!(
            default_branch("TASK-047.5.1", DispatchKind::Implementer),
            "task-047.5.1-impl"
        );
        assert_eq!(
            default_branch("TASK-047.5.1", DispatchKind::Reviewer),
            "task-047.5.1-review"
        );
        assert_ne!(
            default_worktree(&home, "orgasmic", "TASK-086", DispatchKind::Reviewer).unwrap(),
            default_worktree(&home, "orgasmic", "TASK-086", DispatchKind::Implementer).unwrap()
        );
        // Two projects never share a managed worktree path, even for one task.
        assert_ne!(
            default_worktree(&home, "orgasmic", "TASK-086", DispatchKind::Implementer).unwrap(),
            default_worktree(&home, "other", "TASK-086", DispatchKind::Implementer).unwrap()
        );
        assert_ne!(
            default_branch("TASK-086", DispatchKind::Reviewer),
            default_branch("TASK-086", DispatchKind::Implementer)
        );
    }

    /// TASK-79VKP.6's negative control in the production shape: two linked
    /// Cargo worktrees must compile and run their own changed binary.  The
    /// private policy leaves no manager-owned target artifacts to reuse.
    #[test]
    fn empty_private_targets_never_run_another_worktrees_binary() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("manager");
        std::fs::create_dir_all(root.join("src")).unwrap();
        std::fs::write(
            root.join("Cargo.toml"),
            "[package]\nname = \"private-target-probe\"\nversion = \"0.1.0\"\nedition = \"2021\"\n\n[dependencies]\nanyhow = \"1\"\n",
        )
        .unwrap();
        std::fs::write(
            root.join("src/main.rs"),
            "fn main() { let _: anyhow::Result<()> = Ok(()); println!(\"BASE\"); }\n",
        )
        .unwrap();

        let base_build = Command::new("cargo")
            .args(["build", "--quiet"])
            .current_dir(&root)
            .output()
            .unwrap();
        assert!(
            base_build.status.success(),
            "base build: {}",
            String::from_utf8_lossy(&base_build.stderr)
        );

        let git = |args: &[&str]| {
            let output = Command::new("git")
                .args(args)
                .current_dir(&root)
                .output()
                .unwrap();
            assert!(
                output.status.success(),
                "git {args:?}: {}",
                String::from_utf8_lossy(&output.stderr)
            );
        };
        git(&["init", "-q", "-b", "main"]);
        git(&["config", "user.email", "tests@example.com"]);
        git(&["config", "user.name", "Tests"]);
        git(&["add", "Cargo.toml", "Cargo.lock", "src/main.rs"]);
        git(&["commit", "-qm", "base"]);

        let first = temp.path().join("first");
        let second = temp.path().join("second");
        create_worktree(&root, &first, "first-branch", "HEAD").unwrap();
        create_worktree(&root, &second, "second-branch", "HEAD").unwrap();
        std::fs::write(
            first.join("src/main.rs"),
            "fn main() { let _: anyhow::Result<()> = Ok(()); println!(\"FIRST\"); }\n",
        )
        .unwrap();
        std::fs::write(
            second.join("src/main.rs"),
            "fn main() { let _: anyhow::Result<()> = Ok(()); println!(\"SECOND\"); }\n",
        )
        .unwrap();

        assert!(matches!(
            private_worktree_target_policy(&root, &first),
            WorktreeTargetSeed::Skipped {
                reason: "empty-private-target",
                ..
            }
        ));
        assert!(matches!(
            private_worktree_target_policy(&root, &second),
            WorktreeTargetSeed::Skipped {
                reason: "empty-private-target",
                ..
            }
        ));

        let first_for_build = first.clone();
        let second_for_build = second.clone();
        let (first_build, second_build) = std::thread::scope(|scope| {
            let first = scope.spawn(|| {
                Command::new("cargo")
                    .arg("build")
                    .current_dir(&first_for_build)
                    .output()
                    .unwrap()
            });
            let second = scope.spawn(|| {
                Command::new("cargo")
                    .arg("build")
                    .current_dir(&second_for_build)
                    .output()
                    .unwrap()
            });
            (first.join().unwrap(), second.join().unwrap())
        });
        assert!(
            first_build.status.success(),
            "first build: {}",
            String::from_utf8_lossy(&first_build.stderr)
        );
        assert!(
            second_build.status.success(),
            "second build: {}",
            String::from_utf8_lossy(&second_build.stderr)
        );
        let run = |worktree: &Path| {
            let output = Command::new(worktree.join("target/debug/private-target-probe"))
                .output()
                .unwrap();
            assert!(output.status.success());
            String::from_utf8(output.stdout).unwrap()
        };
        assert_eq!(run(&first), "FIRST\n");
        assert_eq!(run(&second), "SECOND\n");
    }

    /// A project id is a path segment in the managed root, so anything that
    /// could escape it is refused rather than joined.
    // orgasmic:TASK-M47E5
    #[test]
    fn managed_worktree_root_refuses_a_project_id_that_could_escape_it() {
        let home = Home::at("/home/.orgasmic");
        for id in ["..", ".", "", "  ", "a/b", "a\\b"] {
            assert!(
                managed_worktree_root(&home, id).is_err(),
                "project id {id:?} should be refused"
            );
        }
        assert_eq!(
            managed_worktree_root(&home, "orgasmic").unwrap(),
            PathBuf::from("/home/.orgasmic/worktrees/orgasmic")
        );
    }

    /// TASK-M47E5.2 finding 1, the half an integration test cannot drive: a
    /// root RENAMED between the anchor and the removal.
    ///
    /// This is the difference between anchoring and checking. A check-then-act
    /// form validates the root at time T and removes at time T+n through the
    /// same path; swap the path in between and the removal lands in the victim.
    /// The anchor holds the directory INODE open, so `unlinkat` never consults
    /// the path again.
    // orgasmic:TASK-M47E5.2
    #[cfg(unix)]
    #[test]
    fn an_anchored_root_renamed_under_the_prune_still_removes_only_from_the_real_root() {
        let tmp = tempfile::tempdir().unwrap();
        let home = Home::at(tmp.path().join("home"));
        let real = home.root.join("worktrees/orgasmic");
        let victim = tmp.path().join("victim");
        std::fs::create_dir_all(real.join("task-a/nested")).unwrap();
        std::fs::write(real.join("task-a/nested/doomed.txt"), "doomed").unwrap();
        std::fs::create_dir_all(victim.join("task-a/nested")).unwrap();
        std::fs::write(victim.join("task-a/nested/sentinel.txt"), "sentinel").unwrap();

        let anchor = AnchoredManagedRoot::open(&home, "orgasmic")
            .unwrap()
            .expect("anchored");
        assert_eq!(
            anchor.child_names().unwrap(),
            vec![std::ffi::OsString::from("task-a")]
        );
        let identity = anchor
            .open_child(std::ffi::OsStr::new("task-a"))
            .unwrap()
            .expect("task-a is a real directory")
            .identity;

        // The adversarial move: the path the anchor was opened through now
        // names the victim instead.
        std::fs::rename(&real, tmp.path().join("moved-aside")).unwrap();
        std::fs::rename(&victim, &real).unwrap();

        anchor
            .remove_child(std::ffi::OsStr::new("task-a"), identity)
            .map_err(|failure| failure.error)
            .expect("removal must still succeed against the anchored inode");
        assert!(
            real.join("task-a/nested/sentinel.txt").is_file(),
            "the directory now at the root path must be untouched"
        );
        assert!(
            !tmp.path().join("moved-aside/task-a").exists(),
            "the entry that was removed must be the one under the anchored inode"
        );
    }

    /// TASK-RMA18 finding 5: the identity classified is the identity deleted.
    ///
    /// The removal is asked for by NAME, and a name is exactly what an adversary
    /// can rebind. Substituting a different directory at the same name between
    /// the classification and the removal must stop the removal dead rather than
    /// destroy whatever now answers to that name — and the substituted tree's
    /// sentinel is what proves it did.
    // orgasmic:TASK-RMA18
    #[cfg(unix)]
    #[test]
    fn a_child_substituted_between_classification_and_removal_is_refused() {
        let tmp = tempfile::tempdir().unwrap();
        let home = Home::at(tmp.path().join("home"));
        let root = home.root.join("worktrees/orgasmic");
        std::fs::create_dir_all(root.join("task-a/nested")).unwrap();
        std::fs::write(root.join("task-a/nested/doomed.txt"), "doomed").unwrap();
        let impostor = tmp.path().join("impostor");
        std::fs::create_dir_all(impostor.join("nested")).unwrap();
        std::fs::write(impostor.join("nested/sentinel.txt"), "sentinel").unwrap();

        let anchor = AnchoredManagedRoot::open(&home, "orgasmic")
            .unwrap()
            .expect("anchored");
        let classified = anchor
            .open_child(std::ffi::OsStr::new("task-a"))
            .unwrap()
            .expect("task-a is a real directory")
            .identity;

        // The substitution: the same NAME, a different inode.
        std::fs::rename(root.join("task-a"), tmp.path().join("moved-aside")).unwrap();
        std::fs::rename(&impostor, root.join("task-a")).unwrap();

        let failure = anchor
            .remove_child(std::ffi::OsStr::new("task-a"), classified)
            .expect_err("a substituted child must be refused");
        assert!(
            !failure.touched,
            "a refusal before any removal must report that it touched nothing: {}",
            failure.error
        );
        let message = failure.error.to_string();
        assert!(
            message.contains("different directory"),
            "the refusal must say the entry changed identity: {message}"
        );
        assert!(
            root.join("task-a/nested/sentinel.txt").is_file(),
            "the substituted tree must survive untouched"
        );

        // And the path fence used before handing anything to `git` refuses the
        // same substitution, for the same reason.
        let err = anchor
            .assert_path_names(&root.join("task-a"), classified)
            .expect_err("the path fence must refuse a rebound path")
            .to_string();
        assert!(err.contains("different directory"), "{err}");
    }

    /// TASK-RMA18 finding 4: `O_NOFOLLOW` guards only the FINAL component.
    ///
    /// Opening `<home>/worktrees/<project-id>` in one syscall makes the kernel
    /// resolve `<home>/worktrees` by pathname. Replace that ANCESTOR with a
    /// symlink and the handle anchors a victim directory with every downstream
    /// fd-relative guarantee intact and pointed at the wrong tree. The round-1
    /// regression replaced only the final component, so it could not catch this.
    // orgasmic:TASK-RMA18
    #[cfg(unix)]
    #[test]
    fn an_ancestor_symlink_above_the_managed_root_is_refused_by_name() {
        let tmp = tempfile::tempdir().unwrap();
        let home = Home::at(tmp.path().join("home"));
        std::fs::create_dir_all(&home.root).unwrap();
        let victim = tmp.path().join("victim");
        std::fs::create_dir_all(victim.join("orgasmic/task-precious")).unwrap();
        std::fs::write(
            victim.join("orgasmic/task-precious/keep-me.txt"),
            "sentinel",
        )
        .unwrap();

        // The ANCESTOR, not the root: `<home>/worktrees` is the symlink.
        std::os::unix::fs::symlink(&victim, home.root.join("worktrees")).unwrap();

        let err = AnchoredManagedRoot::open(&home, "orgasmic")
            .expect_err("an ancestor symlink must be refused")
            .to_string();
        assert!(err.contains("managed worktree root"), "{err}");
        assert!(err.contains("symlink"), "{err}");
        assert!(
            err.contains("worktrees"),
            "the refusal must name the component that is the symlink: {err}"
        );
        assert!(
            victim.join("orgasmic/task-precious/keep-me.txt").is_file(),
            "nothing may be scanned or removed through a followed ancestor"
        );
    }

    /// A symlinked root is refused at the anchor, and the refusal names both the
    /// root and the shape, because "ELOOP" tells an operator nothing.
    // orgasmic:TASK-M47E5.2
    #[cfg(unix)]
    #[test]
    fn a_symlinked_managed_root_is_refused_by_name() {
        let tmp = tempfile::tempdir().unwrap();
        let home = Home::at(tmp.path().join("home"));
        let victim = tmp.path().join("victim");
        std::fs::create_dir_all(&victim).unwrap();
        let root = home.root.join("worktrees/orgasmic");
        std::fs::create_dir_all(root.parent().unwrap()).unwrap();
        std::os::unix::fs::symlink(&victim, &root).unwrap();

        let err = AnchoredManagedRoot::open(&home, "orgasmic")
            .expect_err("a symlinked root must be refused")
            .to_string();
        assert!(err.contains("managed worktree root"), "{err}");
        assert!(err.contains("symlink"), "{err}");

        // A root that simply does not exist is not an error — there is nothing
        // to scan and nothing to refuse.
        let absent = Home::at(tmp.path().join("absent-home"));
        std::fs::create_dir_all(&absent.root).unwrap();
        assert!(AnchoredManagedRoot::open(&absent, "orgasmic")
            .unwrap()
            .is_none());
    }

    /// Does `hook` actually PARK when its env var names a file that exists?
    ///
    /// Answered by running it, never by reading a `cfg`. `pause_until_file_is_removed`
    /// writes a `<pause>.reached` sidecar immediately before it starts sleeping,
    /// so that file appearing is proof the body ran and parked; a compiled-out
    /// hook returns at once and writes nothing. Bounded either way: the pause
    /// file is removed once the sidecar shows up, which is what lets the parked
    /// thread finish and be joined.
    // orgasmic:TASK-RMA18.1
    fn pause_hook_parks(hook: fn(), env_var: &str, dir: &Path) -> bool {
        let pause = dir.join(format!("{env_var}.pause"));
        let reached = pause.with_extension("reached");
        let _ = std::fs::remove_file(&reached);
        std::fs::write(&pause, "1").unwrap();
        std::env::set_var(env_var, &pause);

        let worker = std::thread::spawn(hook);
        // Generous: this only has to outlast thread start-up, and the loop it
        // is racing sleeps forever, so a slow machine cannot make this flake
        // in the "it parked" direction.
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        let mut parked = false;
        while std::time::Instant::now() < deadline {
            if reached.exists() {
                parked = true;
                break;
            }
            if worker.is_finished() {
                break;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        // Release a parked thread; a no-op hook has already returned.
        let _ = std::fs::remove_file(&pause);
        worker.join().expect("pause hook thread");
        std::env::remove_var(env_var);
        parked
    }

    /// TASK-RMA18 / TASK-RMA18.1 finding 3: NEITHER test-only pause rendezvous
    /// may exist in a release build. Each parks the process indefinitely while
    /// its caller holds the global dispatch cleanup lock AND the daemon's
    /// worktree reservation, so in a shipped binary a stray environment variable
    /// — inherited by any child of a shell that once ran the suite — wedges the
    /// verb and blocks every acquire into that worktree until it is killed.
    ///
    /// THIS TEST MEASURES, IT DOES NOT RESTATE. TASK-RMA18 asserted
    /// `hook_is_compiled() == cfg!(debug_assertions)` where the function's body
    /// WAS `cfg!(debug_assertions)`; deleting the `#[cfg]` from the hook left it
    /// green. Here the left-hand side comes from calling the hook and watching
    /// for its `.reached` sidecar, so deleting the `#[cfg]` makes the release
    /// leg go red.
    ///
    /// That leg is not a flag (TASK-RMA18.1.1 finding 4: there is no
    /// `--release-gates`, and `run-tests.sh` forwards unknown arguments to
    /// cargo, so following that name got a cargo argument error). It runs
    /// AUTOMATICALLY for any invocation whose scope covers `orgasmic-cli` —
    /// `scripts/run-tests.sh -p orgasmic-cli` or `scripts/run-tests.sh
    /// --workspace` — as a second `cargo test -p orgasmic-cli --bin orgasmic`
    /// under `RUSTFLAGS=-C debug-assertions=off` in its own target directory.
    /// It is the only leg where the claim below is live.
    // orgasmic:TASK-RMA18,TASK-RMA18.1,TASK-RMA18.1.1
    #[test]
    fn the_pause_rendezvous_hooks_park_only_in_debug_builds() {
        let tmp = tempfile::tempdir().unwrap();
        for (verb, hook, env_var) in [
            (
                "worktree-prune",
                worktree_prune_pause_after_guard as fn(),
                "ORGASMIC_WORKTREE_PRUNE_PAUSE_FILE",
            ),
            (
                "dispatch-close",
                dispatch_close_pause_after_guard as fn(),
                "ORGASMIC_DISPATCH_CLOSE_PAUSE_FILE",
            ),
        ] {
            assert_eq!(
                pause_hook_parks(hook, env_var, tmp.path()),
                cfg!(debug_assertions),
                "{verb}'s pause rendezvous must park in a debug build and be COMPILED OUT of a \
                 release one; measured by running it with {env_var} set to an existing file"
            );
        }
    }

    /// TASK-M47E5.2 finding 3: only ABSENCE may conclude the repository is gone.
    // orgasmic:TASK-M47E5.2
    #[cfg(unix)]
    #[test]
    fn only_a_not_found_git_link_classifies_as_repo_gone() {
        use std::os::unix::fs::PermissionsExt;

        let tmp = tempfile::tempdir().unwrap();
        let worktree = tmp.path().join("wt");
        std::fs::create_dir_all(&worktree).unwrap();
        // The classifier reads through a HANDLE on the worktree, so the fixture
        // opens one exactly as the scan does.
        let state = || {
            let dir = anchored_dir::open_trust_root(&worktree).unwrap();
            worktree_repo_state(&dir, &worktree)
        };

        // No `.git` at all: proven absent.
        assert!(matches!(state(), WorktreeRepoState::Gone(_)));

        // A `.git` naming an admin directory that is gone: also proven absent.
        std::fs::write(worktree.join(".git"), "gitdir: ../nowhere\n").unwrap();
        assert!(matches!(state(), WorktreeRepoState::Gone(_)));

        // The same link, now resolving.
        std::fs::create_dir_all(tmp.path().join("nowhere")).unwrap();
        assert!(matches!(state(), WorktreeRepoState::Present));

        // Unreadable for a reason that is NOT absence: undetermined, and the
        // reason travels with it. This is the case that used to select
        // `RepoGone` — the one disposition that deletes without salvaging.
        let dot_git = worktree.join(".git");
        std::fs::set_permissions(&dot_git, std::fs::Permissions::from_mode(0o000)).unwrap();
        if std::fs::read_to_string(&dot_git).is_ok() {
            // Running as root; the case is unreachable here rather than absent.
            return;
        }
        match state() {
            WorktreeRepoState::Undetermined(detail) => {
                assert!(detail.contains(".git"), "{detail}");
                assert!(
                    detail.to_lowercase().contains("permission denied"),
                    "{detail}"
                );
            }
            other => panic!("an unreadable .git must never classify as repo-gone: {other:?}"),
        }
        std::fs::set_permissions(&dot_git, std::fs::Permissions::from_mode(0o644)).unwrap();
    }

    /// The reservation key a managed worktree name yields.
    // orgasmic:TASK-M47E5.2
    #[test]
    fn a_managed_worktree_name_yields_the_task_it_encodes() {
        assert_eq!(worktree_reservation_task_id("task-m47e5"), "TASK-M47E5");
        assert_eq!(
            worktree_reservation_task_id("task-m47e5-review"),
            "TASK-M47E5"
        );
        // A name outside the scheme still produces a stable, non-empty segment:
        // mutual exclusion and the liveness scan are keyed on the worktree, not
        // on this.
        assert_eq!(worktree_reservation_task_id("stray"), "stray");
        assert_eq!(worktree_reservation_task_id("task-"), "task-");
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

        // orgasmic:TASK-RB1ZN — the daemon used to answer this same 404 for a
        // run that was live with a release already running for it. It answers
        // 409 now, and that must NOT take the rescue branch: the branch's whole
        // premise is that the run is already released and its tombstone is on
        // disk to be read (TASK-37TAF's ordering note). A wedged run has no
        // tombstone yet, so treating it as "already released" would either read
        // an absent tombstone or emit a terminal tx for a run whose release
        // never happened. Hard-erroring hands the operator the daemon's own
        // detail, which names the drain budget and says to retry after it.
        let release_in_progress = anyhow::anyhow!(
            "daemon returned 409 Conflict: {{\"error\":\"release already in progress\",\"run_id\":\"run-x\",\"detail\":\"run run-x is live and a release is already running for it, so this call has nothing to add. TASK-HAREX bounds that drain at 20s; retry after it.\"}}"
        );
        assert!(!is_release_run_not_found_error(&release_in_progress));
        assert!(
            is_release_in_progress_error(&release_in_progress),
            "the live-but-releasing conflict needs its own branch, not the \
             already-released one"
        );
        assert!(!is_release_in_progress_error(&not_found));
        assert!(!is_release_in_progress_error(&ownership_conflict));

        let unreachable =
            anyhow::anyhow!("daemon request failed: connection refused — is the daemon reachable?");
        assert!(!is_release_run_not_found_error(&unreachable));
    }

    /// dec_HBK6A stage A: the architector dispatch VERB is gone, but the string
    /// is still on disk in every project's tx log. The removal is only correct
    /// if a persisted `manager.dispatch_started` row recorded as `architector`
    /// still parses off the ledger and still resolves its whole close
    /// vocabulary — otherwise a dispatch opened before the excision is
    /// unclosable and its history is unreadable.
    ///
    /// This is the load-bearing test of stage A. Stage D deleted
    /// `WorkerKind::Architector`; this must stay green because the ledger
    /// stores KIND as a free string and never calls WorkerKind::from_str.
    #[test]
    fn historical_architector_ledger_row_still_parses_and_closes() {
        // Nothing can START one any more.
        assert!(<DispatchKind as ValueEnum>::from_str("architector", false).is_err());

        let tmp = tempfile::tempdir().unwrap();
        let tx_dir = tmp.path().join(".orgasmic/tx");
        std::fs::create_dir_all(&tx_dir).unwrap();
        // Verbatim shape of a real pre-excision row (`:KIND: architector`),
        // followed by the worker's own report, which must leave it OPEN.
        std::fs::write(
            tx_dir.join("2026-05.org"),
            "#+title: tx\n#+orgasmic_version: 1\n\n* TX 2026-05-23 Sat 10:00:00 manager.dispatch_started TASK-086\n:PROPERTIES:\n:TX_ID:        tx-start-arch\n:TIME:         [2026-05-23 Sat 10:00:00]\n:TYPE:         manager.dispatch_started\n:ACTOR:        a@example.com\n:MACHINE:      host\n:PROJECT:      orgasmic\n:TASK:         TASK-086\n:KIND:         architector\n:WORKTREE:     /tmp/orgasmic-worktrees/task-086-arch\n:BRANCH:       task-086-arch\n:STARTED_AT:   [2026-05-23 Sat 10:00:00]\n:END:\n\n* TX 2026-05-23 Sat 10:10:00 architector.reported TASK-086\n:PROPERTIES:\n:TX_ID:        tx-reported-arch\n:TIME:         [2026-05-23 Sat 10:10:00]\n:TYPE:         architector.reported\n:ACTOR:        agent.architector\n:MACHINE:      host\n:PROJECT:      orgasmic\n:TASK:         TASK-086\n:END:\n",
        )
        .unwrap();

        let open = scan_open_dispatches(tmp.path()).unwrap();
        assert_eq!(open.len(), 1, "the historical architector row must parse");
        assert_eq!(open[0].tx_id, "tx-start-arch");
        assert_eq!(open[0].kind, "architector");
        assert!(
            open[0].reported,
            "`architector.reported` must still mark the dispatch reported"
        );
        assert!(!open[0].closed, "a report does not close the dispatch");

        // The close vocabulary the manager needs to finish it off.
        assert_eq!(done_tx_type(&open[0]).unwrap(), "architector.done");
        assert_eq!(
            finalize_tx_type_for_kind("architector").unwrap(),
            "architector.reported"
        );

        // And the manager's `*.done` still closes it.
        let closed_file = tx_dir.join("2026-06.org");
        std::fs::write(
            &closed_file,
            "#+title: tx\n#+orgasmic_version: 1\n\n* TX 2026-06-01 Mon 10:00:00 architector.done TASK-086\n:PROPERTIES:\n:TX_ID:        tx-done-arch\n:TIME:         [2026-06-01 Mon 10:00:00]\n:TYPE:         architector.done\n:ACTOR:        a@example.com\n:MACHINE:      host\n:PROJECT:      orgasmic\n:TASK:         TASK-086\n:CLOSED_TX:    tx-start-arch\n:END:\n",
        )
        .unwrap();
        assert!(
            scan_open_dispatches(tmp.path()).unwrap().is_empty(),
            "`architector.done` must still close the historical dispatch"
        );
        let closed = latest_closed_dispatch_for_tasks(tmp.path(), &["TASK-086".to_string()])
            .unwrap()
            .expect("a closed architector dispatch must still resolve");
        assert_eq!(closed.tx_id, "tx-start-arch");
        assert!(closed.closed);
    }

    #[test]
    fn successful_close_deletes_branch_by_default_but_aborted_close_retains_it() {
        let args_for = |status| DispatchCloseArgs {
            task: vec!["TASK-086".to_string()],
            started_tx: Some("tx-start".to_string()),
            status,
            merge_sha: None,
            worker_commit: None,
            worker_session: None,
            reviewed_diff: None,
            properties: Vec::new(),
            verdict: None,
            tokens: None,
            wall: None,
            reason: Some("test".to_string()),
            no_review_required: false,
            fix_round_final: false,
            worktree_remove: true,
            no_worktree_remove: false,
            branch_delete: false,
            no_branch_delete: false,
        };

        assert!(dispatch_close_deletes_branch(&args_for(
            DispatchCloseStatus::Done
        )));
        assert!(!dispatch_close_deletes_branch(&args_for(
            DispatchCloseStatus::Aborted
        )));
        let mut opted_out = args_for(DispatchCloseStatus::Done);
        opted_out.no_branch_delete = true;
        assert!(!dispatch_close_deletes_branch(&opted_out));
        let mut retained_worktree = args_for(DispatchCloseStatus::Done);
        retained_worktree.no_worktree_remove = true;
        assert!(!dispatch_close_deletes_branch(&retained_worktree));
        let mut explicit_abort = args_for(DispatchCloseStatus::Aborted);
        explicit_abort.branch_delete = true;
        assert!(dispatch_close_deletes_branch(&explicit_abort));
    }

    #[test]
    fn closes_architector_lifecycle_to_done() {
        let tmp = tempfile::tempdir().unwrap();
        let in_progress = tmp.path().join(".orgasmic/tasks/TASK-086/node.org");
        std::fs::create_dir_all(in_progress.parent().unwrap()).unwrap();
        std::fs::write(
            &in_progress,
            "#+title: orgasmic task TASK-086\n#+orgasmic_version: 2\n\n* IN_PROGRESS TASK-086 Architecture run\n:PROPERTIES:\n:ID:               TASK-086\n:END:\n",
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
            verdict: None,
            tokens: None,
            wall: None,
            reason: None,
            no_review_required: false,
            fix_round_final: false,
            worktree_remove: true,
            no_worktree_remove: false,
            branch_delete: false,
            no_branch_delete: false,
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

    #[test]
    fn fix_round_final_fence_names_aborted_and_architector_closes() {
        let args_for = |status| DispatchCloseArgs {
            task: vec!["TASK-086".to_string()],
            started_tx: Some("tx-start-arch".to_string()),
            status,
            merge_sha: Some("abc123".to_string()),
            worker_commit: None,
            worker_session: None,
            reviewed_diff: None,
            properties: Vec::new(),
            verdict: None,
            tokens: None,
            wall: None,
            reason: Some("not an implementer.done close".to_string()),
            no_review_required: false,
            fix_round_final: true,
            worktree_remove: true,
            no_worktree_remove: false,
            branch_delete: false,
            no_branch_delete: false,
        };
        let tasks = vec!["TASK-086".to_string()];
        let expected =
            "--fix-round-final is valid only when closing an implementer dispatch as done";

        let aborted = validate_fix_round_final(
            Path::new("/unused"),
            &tasks,
            &args_for(DispatchCloseStatus::Aborted),
            "manager.dispatch_aborted",
        )
        .unwrap_err()
        .to_string();
        assert!(
            aborted.contains(expected),
            "aborted close must reach the named fence: {aborted}"
        );

        let architector = validate_fix_round_final(
            Path::new("/unused"),
            &tasks,
            &args_for(DispatchCloseStatus::Done),
            "architector.done",
        )
        .unwrap_err()
        .to_string();
        assert!(
            architector.contains(expected),
            "architector.done close must reach the named fence: {architector}"
        );
    }

    /// orgasmic:TASK-4WKNX — the whole rule table in one place, so the answer
    /// to "does a fix round get its own review" is readable without
    /// reconstructing it from a dispatch integration test.
    #[test]
    fn implementer_done_stage_reviews_fix_rounds_unless_declared_final() {
        // Not a fix round: unchanged, `in_review`, with or without the flag.
        assert_eq!(
            implementer_done_stage(false, false),
            LifecycleStage::InReview
        );
        assert_eq!(
            implementer_done_stage(false, true),
            LifecycleStage::InReview
        );
        // A fix round is reviewed by default…
        assert_eq!(
            implementer_done_stage(true, false),
            LifecycleStage::InReview
        );
        // …and only the explicit opt-out closes it straight to done.
        assert_eq!(implementer_done_stage(true, true), LifecycleStage::Done);
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
        let dispatched = |task: &str| {
            format!(
                "* TX 2026-07-29 Wed 11:00:00 manager.dispatch_started {task}\n:PROPERTIES:\n:TX_ID:        tx-next-{task}\n:TIME:         [2026-07-29 Wed 11:00:00]\n:TYPE:         manager.dispatch_started\n:ACTOR:        a@example.com\n:MACHINE:      host\n:PROJECT:      orgasmic\n:TASK:         {task}\n:KIND:         reviewer\n:END:\n"
            )
        };
        std::fs::write(
            tx_dir.join("2026-07.org"),
            format!(
                "#+title: tx\n#+orgasmic_version: 1\n\n{}\n{}\n{}\n{}\n{}\n{}",
                close("tx-1", "TASK-TORN", "in_progress", "in_review"),
                close("tx-2", "TASK-LANDED", "in_progress", "in_review"),
                transitioned("TASK-LANDED"),
                close("tx-4", "TASK-REDISPATCHED", "in_progress", "in_review"),
                dispatched("TASK-REDISPATCHED"),
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

    // orgasmic:TASK-M47E5.1.1.1
    #[test]
    fn dispatch_artifact_reservation_refuses_degenerate_brief_names_before_creating_artifacts() {
        let tmp = tempfile::tempdir().unwrap();
        let project_root = tmp.path().join("repo");

        for (file_name, stem) in [
            ("-brief.md", ""),
            (".-brief.md", "."),
            ("..-brief.md", ".."),
        ] {
            let brief = tmp.path().join(file_name);
            let error = match DispatchArtifactReservation::reserve(&project_root, &brief) {
                Ok(_) => {
                    panic!("degenerate brief name unexpectedly reserved artifacts: {file_name}")
                }
                Err(error) => error,
            };
            let message = error.to_string();
            assert!(
                message.contains(&format!("brief name {file_name:?}")),
                "error must refuse the brief by name: {message}"
            );
            assert!(
                message.contains(&format!("derived {stem:?}")),
                "error must name the rejected stem: {message}"
            );
            assert!(
                !project_dispatch_dir(&project_root).exists(),
                "invalid brief {file_name} must be refused before creating dispatch artifacts"
            );
        }
    }

    #[test]
    fn reserve_dispatch_artifact_pair_preserves_preexisting_collider() {
        let tmp = tempfile::tempdir().unwrap();
        let project_root = tmp.path().join("repo");
        let brief = project_root.join("task-reserve-brief.md");
        let attempt = "aaaa1111bbbb2222cccc3333dddd4444";
        let (_, last, stdout) =
            dispatch_artifact_paths_for_attempt(&project_root, &brief, attempt).unwrap();
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
        let (_, last, stdout) =
            dispatch_artifact_paths_for_attempt(&project_root, &brief, attempt).unwrap();
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
            dispatch_artifact_paths_for_attempt(&project_root, &brief, "a1b2c3d4").unwrap();
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
        let (_, last1, _) =
            dispatch_artifact_paths_for_attempt(&project_root, &brief, "attempt1").unwrap();
        let (_, last2, _) =
            dispatch_artifact_paths_for_attempt(&project_root, &brief, "attempt2").unwrap();
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
            dispatch_artifact_paths_for_attempt(&root, &brief, "aaaaaaaa11111111bbbbbbbb22222222")
                .unwrap();
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
        open.tx_id = format!("tx-start-{}", task.to_lowercase());
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
        // orgasmic:TASK-QGWK7 — the report survives close at the path the tx names.
        let report_path = cleanup
            .report_path
            .as_deref()
            .expect("close must name a promoted REPORT_PATH");
        assert_eq!(
            report_path,
            ".orgasmic/tasks/TASK-CLEAN/dispatches/tx-start-task-clean/report.md"
        );
        assert!(
            fixture.root.join(report_path).exists(),
            "after close the report must still be readable from the path the tx names"
        );
        assert!(!fixture.last.exists(), "tmp last.txt must be promoted away");
        assert!(
            !fixture.stdout.exists(),
            "tmp stdout.log must be promoted away"
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
        assert_eq!(
            close
                .extra
                .iter()
                .find(|(key, _)| key == "REPORT_PATH")
                .map(|(_, value)| value.as_str()),
            Some(report_path)
        );
    }

    // orgasmic:TASK-QGWK7
    #[test]
    fn dispatch_close_promotes_report_readable_from_path_tx_names() {
        let fixture = dispatch_cleanup_fixture("task-qgwk7");
        std::fs::write(&fixture.last, "worker report survives close\n").unwrap();
        let open = dispatch_cleanup_record(&fixture, "TASK-QGWK7");

        let cleanup = cleanup_dispatch(&fixture.root, &open, true, true);
        assert_eq!(cleanup.status, CleanupStatus::Ok, "{:?}", cleanup.error);
        let report_path = cleanup
            .report_path
            .as_deref()
            .expect("close must name a promoted REPORT_PATH");
        assert!(
            fixture.root.join(report_path).exists(),
            "after close the report must still be readable from the path the tx names"
        );
        assert_eq!(
            std::fs::read_to_string(fixture.root.join(report_path)).unwrap(),
            "worker report survives close\n"
        );
        assert!(!fixture.last.exists());
        assert!(!fixture.worktree.exists());
        assert!(resolve_branch_oid(&fixture.root, &fixture.branch)
            .unwrap()
            .is_none());
    }

    // orgasmic:TASK-QGWK7.1
    #[test]
    fn dispatch_close_stages_promoted_record_in_git_index() {
        let fixture = dispatch_cleanup_fixture("task-stage");
        std::fs::write(&fixture.last, "staged report\n").unwrap();
        let open = dispatch_cleanup_record(&fixture, "TASK-STAGE");

        let cleanup = cleanup_dispatch(&fixture.root, &open, true, false);
        assert_eq!(cleanup.status, CleanupStatus::Ok, "{:?}", cleanup.error);
        let report_path = cleanup
            .report_path
            .as_deref()
            .expect("close must name a promoted REPORT_PATH");
        let dest_dir = fixture
            .root
            .join(report_path)
            .parent()
            .expect("report path has parent")
            .to_path_buf();
        let output = Command::new("git")
            .args(["ls-files", "--stage", "--"])
            .arg(&dest_dir)
            .current_dir(&fixture.root)
            .output()
            .unwrap();
        assert!(output.status.success());
        let listed = String::from_utf8_lossy(&output.stdout);
        assert!(
            listed.contains("report.md"),
            "promoted record must be in the git index after close so durability does not depend on which git add form a manager uses; git ls-files --stage:\n{listed}"
        );
    }

    fn git_ok(root: &Path, args: &[&str]) {
        let output = Command::new("git")
            .args(args)
            .current_dir(root)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "git {args:?}: {}{}",
            String::from_utf8_lossy(&output.stderr),
            String::from_utf8_lossy(&output.stdout)
        );
    }

    /// TASK-QGWK7.1.1 M-0. TASK-QGWK7.1 met "the record is in the index" with
    /// `git add` alone, and a staged path — any staged path — makes `git merge`
    /// refuse. That broke the exact order the review gate's refusal message
    /// prescribes: close the reviewer, then merge. A record in HISTORY is what
    /// a fresh clone can read AND what leaves the following merge able to run.
    // orgasmic:TASK-QGWK7.1.1
    #[test]
    fn dispatch_close_commits_promoted_record_so_the_next_merge_still_runs() {
        let fixture = dispatch_cleanup_fixture("task-merge");
        let base = String::from_utf8_lossy(
            &Command::new("git")
                .args(["rev-parse", "--abbrev-ref", "HEAD"])
                .current_dir(&fixture.root)
                .output()
                .unwrap()
                .stdout,
        )
        .trim()
        .to_string();
        git_ok(&fixture.root, &["checkout", "-q", "-b", "task-merge-side"]);
        std::fs::write(fixture.root.join("side.txt"), "side\n").unwrap();
        git_ok(&fixture.root, &["add", "side.txt"]);
        git_ok(&fixture.root, &["commit", "-qm", "side"]);
        git_ok(&fixture.root, &["checkout", "-q", &base]);
        std::fs::write(&fixture.last, "merge-safe report\n").unwrap();
        let open = dispatch_cleanup_record(&fixture, "TASK-MERGE");

        let cleanup = cleanup_dispatch(&fixture.root, &open, true, false);
        assert_eq!(cleanup.status, CleanupStatus::Ok, "{:?}", cleanup.error);
        let report_path = cleanup
            .report_path
            .as_deref()
            .expect("close must name a promoted REPORT_PATH");

        // Criterion 2 of the parent: a FRESH CLONE can read the record, which
        // needs it in a commit reachable from the branch, not merely staged.
        let in_history = Command::new("git")
            .args(["cat-file", "-e", &format!("HEAD:{report_path}")])
            .current_dir(&fixture.root)
            .output()
            .unwrap();
        assert!(
            in_history.status.success(),
            "the promoted record must be committed so a fresh clone can read it: {}",
            String::from_utf8_lossy(&in_history.stderr)
        );

        let merge = Command::new("git")
            .args(["merge", "--no-ff", "-m", "merge worker", "task-merge-side"])
            .current_dir(&fixture.root)
            .output()
            .unwrap();
        assert!(
            merge.status.success(),
            "a close must leave the merge that follows it able to run; git merge said: {}{}",
            String::from_utf8_lossy(&merge.stderr),
            String::from_utf8_lossy(&merge.stdout)
        );
    }

    /// Fixture for the gitlink merge guard: a submodule origin at v1 and a
    /// main repo with it added at `vendor/sub`, both committed.
    fn gitlink_fixture(tmp: &Path) -> (PathBuf, PathBuf) {
        let sub_origin = tmp.join("sub");
        std::fs::create_dir_all(&sub_origin).unwrap();
        git_ok(&sub_origin, &["init", "-qb", "main"]);
        git_ok(&sub_origin, &["config", "user.email", "t@example.com"]);
        git_ok(&sub_origin, &["config", "user.name", "T"]);
        std::fs::write(sub_origin.join("lib.txt"), "v1\n").unwrap();
        git_ok(&sub_origin, &["add", "."]);
        git_ok(&sub_origin, &["commit", "-qm", "sub v1"]);

        let root = tmp.join("main");
        std::fs::create_dir_all(&root).unwrap();
        git_ok(&root, &["init", "-qb", "main"]);
        git_ok(&root, &["config", "user.email", "t@example.com"]);
        git_ok(&root, &["config", "user.name", "T"]);
        std::fs::write(root.join("x.txt"), "x\n").unwrap();
        git_ok(&root, &["add", "."]);
        git_ok(&root, &["commit", "-qm", "init"]);
        git_ok(
            &root,
            &[
                "-c",
                "protocol.file.allow=always",
                "submodule",
                "add",
                sub_origin.to_str().unwrap(),
                "vendor/sub",
            ],
        );
        git_ok(&root, &["commit", "-qm", "add sub"]);
        (root, sub_origin)
    }

    fn bump_gitlink_and_merge(root: &Path) {
        git_ok(root, &["checkout", "-qb", "task-side"]);
        git_ok(root, &["add", "vendor/sub"]);
        git_ok(root, &["commit", "-qm", "bump gitlink"]);
        git_ok(root, &["checkout", "-q", "main"]);
        git_ok(root, &["merge", "--no-ff", "-qm", "merge side", "task-side"]);
    }

    /// The worker-shaped mistake: a commit made only inside the checkout,
    /// gitlink bumped to it, merged. The oid exists nowhere the submodule's
    /// origin can serve it from, so close must refuse the evidence.
    #[test]
    fn merge_evidence_refuses_a_gitlink_bump_the_submodule_origin_lacks() {
        let tmp = tempfile::tempdir().unwrap();
        let (root, _sub_origin) = gitlink_fixture(tmp.path());
        let sub = root.join("vendor/sub");
        git_ok(&sub, &["config", "user.email", "t@example.com"]);
        git_ok(&sub, &["config", "user.name", "T"]);
        std::fs::write(sub.join("lib.txt"), "local only\n").unwrap();
        git_ok(&sub, &["commit", "-qam", "local-only"]);
        bump_gitlink_and_merge(&root);

        let err = verify_merge_evidence(&root, "HEAD", None)
            .unwrap_err()
            .to_string();
        assert!(err.contains("vendor/sub"), "{err}");
        assert!(
            err.contains("not reachable from any remote-tracking ref"),
            "{err}"
        );
    }

    /// The prescribed order: the sub-repo lands the change first, the parent
    /// fetches and bumps second. That evidence passes.
    #[test]
    fn merge_evidence_accepts_a_gitlink_bump_the_submodule_origin_has() {
        let tmp = tempfile::tempdir().unwrap();
        let (root, sub_origin) = gitlink_fixture(tmp.path());
        std::fs::write(sub_origin.join("lib.txt"), "v2\n").unwrap();
        git_ok(&sub_origin, &["commit", "-qam", "sub v2"]);
        let sub = root.join("vendor/sub");
        git_ok(
            &sub,
            &["-c", "protocol.file.allow=always", "fetch", "-q", "origin"],
        );
        git_ok(&sub, &["checkout", "-q", "origin/main"]);
        bump_gitlink_and_merge(&root);

        verify_merge_evidence(&root, "HEAD", None).expect("origin-reachable bump must pass");
    }

    /// TASK-QGWK7.1.1 M-1. A `git add` that loses to a concurrent state chore
    /// holding `.git/index.lock` used to set `worktree_failed` for a worktree
    /// that WAS removed, and every `--branch-delete` arm requires
    /// `!worktree_failed` — so no arm fired, the branch was silently retained,
    /// and `branch_failed` stayed false with no error naming it.
    // orgasmic:TASK-QGWK7.1.1
    #[test]
    fn record_persist_failure_is_reported_without_mis_classifying_the_close() {
        let fixture = dispatch_cleanup_fixture("task-lockout");
        let open = dispatch_cleanup_record(&fixture, "TASK-LOCKOUT");
        // The concrete trigger from the review: a lock this close does not own.
        std::fs::write(fixture.root.join(".git/index.lock"), "").unwrap();

        let cleanup = cleanup_dispatch(&fixture.root, &open, true, true);

        let error = cleanup.error.clone().unwrap_or_default();
        assert_ne!(
            cleanup.status,
            CleanupStatus::WorktreeFailed,
            "a record-persist failure must not be reported as a worktree failure: {error}"
        );
        assert_eq!(cleanup.status, CleanupStatus::Partial, "{error}");
        assert!(
            error.starts_with("report:"),
            "both cleanup arms report this class of failure as `report:`: {error}"
        );
        assert!(
            !fixture.worktree.exists(),
            "the worktree really was removed; the close must say so"
        );
        assert!(
            resolve_branch_oid(&fixture.root, &fixture.branch)
                .unwrap()
                .is_none(),
            "--branch-delete must still fire: silently keeping the branch is what M-1 cost"
        );
        assert!(
            fixture
                .root
                .join(".orgasmic/tasks/TASK-LOCKOUT/dispatches/tx-start-task-lockout/report.md")
                .exists(),
            "the report itself is never the casualty of a failed persist"
        );
    }

    fn git_capture_ok(root: &Path, args: &[&str]) -> String {
        let output = Command::new("git")
            .args(args)
            .current_dir(root)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "git {args:?}: {}{}",
            String::from_utf8_lossy(&output.stderr),
            String::from_utf8_lossy(&output.stdout)
        );
        String::from_utf8_lossy(&output.stdout).trim().to_string()
    }

    fn staged_record_paths(root: &Path, started_tx: &str) -> String {
        let dest_dir = root.join(".orgasmic/tasks");
        let _ = started_tx;
        String::from_utf8_lossy(
            &Command::new("git")
                .args(["ls-files", "--stage", "--"])
                .arg(&dest_dir)
                .current_dir(root)
                .output()
                .unwrap()
                .stdout,
        )
        .trim()
        .to_string()
    }

    /// TASK-QGWK7.1.1.1 F-3. A record commit written inside a conflicted
    /// sequencer operation is discarded by `git rebase --abort`, which resets
    /// HEAD to `orig-head` and takes the promoted FILE with it (measured; the
    /// staged-only baseline loses it identically, so the class is pre-existing).
    /// Persistence must stand down while the sequencer owns HEAD, keep the
    /// files, and say why — never stage and never commit.
    // orgasmic:TASK-QGWK7.1.1.1
    #[test]
    fn a_close_inside_a_conflicted_merge_keeps_the_record_and_persists_nothing() {
        let fixture = dispatch_cleanup_fixture("task-seq");
        let base = git_capture_ok(&fixture.root, &["rev-parse", "--abbrev-ref", "HEAD"]);
        git_ok(&fixture.root, &["checkout", "-q", "-b", "task-seq-side"]);
        std::fs::write(fixture.root.join("base.txt"), "side\n").unwrap();
        git_ok(&fixture.root, &["commit", "-qam", "side"]);
        git_ok(&fixture.root, &["checkout", "-q", &base]);
        std::fs::write(fixture.root.join("base.txt"), "trunk\n").unwrap();
        git_ok(&fixture.root, &["commit", "-qam", "trunk"]);
        let conflicted = Command::new("git")
            .args(["merge", "task-seq-side"])
            .current_dir(&fixture.root)
            .output()
            .unwrap();
        assert!(
            !conflicted.status.success() && fixture.root.join(".git/MERGE_HEAD").exists(),
            "the fixture must really be mid-merge: {}",
            String::from_utf8_lossy(&conflicted.stdout)
        );
        let tip = git_capture_ok(&fixture.root, &["rev-parse", "HEAD"]);
        let open = dispatch_cleanup_record(&fixture, "TASK-SEQ");

        let cleanup = cleanup_dispatch(&fixture.root, &open, false, false);

        let error = cleanup.error.clone().unwrap_or_default();
        assert_eq!(cleanup.status, CleanupStatus::Partial, "{error}");
        assert!(
            error.contains("a git merge is in progress"),
            "the refusal must name the sequencer operation that caused it: {error}"
        );
        assert!(
            fixture
                .root
                .join(".orgasmic/tasks/TASK-SEQ/dispatches/tx-start-task-seq/report.md")
                .exists(),
            "the promoted report is never the casualty: it stays on disk for the re-run"
        );
        assert_eq!(
            staged_record_paths(&fixture.root, "tx-start-task-seq"),
            "",
            "a refusal must stage nothing — a staged path is what blocks the resolve+commit"
        );
        assert_eq!(
            git_capture_ok(&fixture.root, &["rev-parse", "HEAD"]),
            tip,
            "the close must not move HEAD out from under the merge"
        );
        assert!(
            fixture.root.join(".git/MERGE_HEAD").exists(),
            "the close must leave the operator's merge intact"
        );
    }

    /// Put `base.txt` through three commits so a revert of the middle one
    /// conflicts against the tip. Returns nothing: every caller reads the repo.
    fn stack_three_revisions(root: &Path) {
        for content in ["two\n", "three\n"] {
            std::fs::write(root.join("base.txt"), content).unwrap();
            git_ok(root, &["commit", "-qam", content.trim()]);
        }
    }

    /// TASK-QGWK7.1.1.1.1 B-1. `git revert` is the same sequencer machinery as
    /// `cherry-pick`, and the guard did not check `REVERT_HEAD`, so a close
    /// inside a revert committed the record onto the branch and reported `ok`.
    ///
    /// The DISCRIMINATING assertion is `CleanupStatus::Partial` — that is what
    /// the missing marker cost, and it is what the verify artifact's red run
    /// trips on. The `git revert --abort` assertion at the end is a REGRESSION
    /// FENCE, not the discriminator: re-measured in the production shape, the
    /// abort exits 0 with the marker or without it (TASK-QGWK7.1.1.1.1.1 C-1)
    /// — the single-pick abort is a `reset --merge` to the CURRENT HEAD, which
    /// IS the record commit, so it rewinds without touching the record
    /// (TASK-QGWK7.1.1.1.1.1.1 D-3; the manager's conflict resolution does not
    /// survive it). It stays because a close that ever starts wedging the
    /// operator's own abort must fail something.
    ///
    /// The name predates C-1 and is kept only because
    /// `verify/TASK-QGWK7.1.1.1.1/expect-red` pins it verbatim and is immutable
    /// (TASK-QGWK7.1.1.1.1.1, reported).
    // orgasmic:TASK-QGWK7.1.1.1.1,TASK-QGWK7.1.1.1.1.1
    #[test]
    fn a_close_inside_a_conflicted_revert_keeps_the_record_and_leaves_the_abort_working() {
        let fixture = dispatch_cleanup_fixture("task-revert");
        stack_three_revisions(&fixture.root);
        let conflicted = Command::new("git")
            .args(["revert", "--no-edit", "HEAD~1"])
            .current_dir(&fixture.root)
            .output()
            .unwrap();
        assert!(
            !conflicted.status.success() && fixture.root.join(".git/REVERT_HEAD").exists(),
            "the fixture must really be mid-revert: {}{}",
            String::from_utf8_lossy(&conflicted.stdout),
            String::from_utf8_lossy(&conflicted.stderr)
        );
        let tip = git_capture_ok(&fixture.root, &["rev-parse", "HEAD"]);
        let open = dispatch_cleanup_record(&fixture, "TASK-REVERT");

        let cleanup = cleanup_dispatch(&fixture.root, &open, false, false);

        let error = cleanup.error.clone().unwrap_or_default();
        assert_eq!(
            cleanup.status,
            CleanupStatus::Partial,
            "a close inside a revert must stand down instead of committing: {error}"
        );
        assert!(
            error.contains("a git revert is in progress"),
            "the refusal must name the sequencer operation that caused it: {error}"
        );
        let promoted = fixture
            .root
            .join(".orgasmic/tasks/TASK-REVERT/dispatches/tx-start-task-revert/report.md");
        assert!(
            promoted.exists(),
            "the promoted report is never the casualty: it stays on disk for the re-run"
        );
        assert_eq!(
            staged_record_paths(&fixture.root, "tx-start-task-revert"),
            "",
            "a refusal must stage nothing — a staged path is what blocks the resolve+commit"
        );
        assert_eq!(
            git_capture_ok(&fixture.root, &["rev-parse", "HEAD"]),
            tip,
            "the close must not move HEAD out from under the revert"
        );
        assert!(
            fixture.root.join(".git/REVERT_HEAD").exists(),
            "the close must leave the operator's revert intact"
        );

        // A fence, not the discriminator (see the doc comment): the operator's
        // own abort works either way, and must never stop working.
        let aborted = Command::new("git")
            .args(["revert", "--abort"])
            .current_dir(&fixture.root)
            .output()
            .unwrap();
        assert!(
            aborted.status.success(),
            "a close must never wedge `git revert --abort`: {}{}",
            String::from_utf8_lossy(&aborted.stdout),
            String::from_utf8_lossy(&aborted.stderr)
        );
        assert!(
            !fixture.root.join(".git/REVERT_HEAD").exists(),
            "the abort must really have finished the revert"
        );
        assert!(
            promoted.exists(),
            "and it must not take the promoted record with it — the re-run needs it"
        );
    }

    /// TASK-QGWK7.1.1.1.1 B-1, the wider half. The exposure is not only a
    /// CONFLICTED revert: a clean `git revert -n` — the ordinary way to stage a
    /// revert before editing it — leaves `REVERT_HEAD` present and
    /// `CHERRY_PICK_HEAD` absent (measured), so an ordinary manager move put a
    /// close in the unguarded state.
    // orgasmic:TASK-QGWK7.1.1.1.1
    #[test]
    fn a_close_with_a_cleanly_staged_revert_persists_nothing() {
        let fixture = dispatch_cleanup_fixture("task-revert-n");
        stack_three_revisions(&fixture.root);
        git_ok(&fixture.root, &["revert", "-n", "--no-edit", "HEAD"]);
        assert!(
            fixture.root.join(".git/REVERT_HEAD").exists()
                && !fixture.root.join(".git/CHERRY_PICK_HEAD").exists(),
            "the fixture must be the CLEAN staged revert: REVERT_HEAD alone"
        );
        let tip = git_capture_ok(&fixture.root, &["rev-parse", "HEAD"]);
        let open = dispatch_cleanup_record(&fixture, "TASK-REVERT-N");

        let cleanup = cleanup_dispatch(&fixture.root, &open, false, false);

        let error = cleanup.error.clone().unwrap_or_default();
        assert_eq!(
            cleanup.status,
            CleanupStatus::Partial,
            "a close inside a staged revert must stand down instead of committing: {error}"
        );
        assert!(
            error.contains("a git revert is in progress"),
            "a staged revert is refused by the same name as a conflicted one: {error}"
        );
        assert!(
            fixture
                .root
                .join(".orgasmic/tasks/TASK-REVERT-N/dispatches/tx-start-task-revert-n/report.md")
                .exists(),
            "the promoted report is never the casualty: it stays on disk for the re-run"
        );
        assert_eq!(
            staged_record_paths(&fixture.root, "tx-start-task-revert-n"),
            "",
            "the record must not join the manager's staged revert in the index"
        );
        assert_eq!(
            git_capture_ok(&fixture.root, &["rev-parse", "HEAD"]),
            tip,
            "the close must not commit on top of a revert the manager has not finished"
        );
    }

    /// TASK-QGWK7.1.1.1.1 B-1, narrowed by TASK-QGWK7.1.1.1.1.1 C-2 and
    /// corrected back by TASK-QGWK7.1.1.1.1.1.1 D-1. A multi-commit
    /// revert/cherry-pick range stopped between picks keeps its todo list in
    /// `.git/sequencer`, and USUALLY its `*_HEAD` marker too — but not always:
    /// resolve the conflict and `git commit` instead of `git revert --continue`
    /// and git clears the marker while leaving the todo list, from which
    /// `--continue` still resumes the range (measured, git 2.52.0). C-2 dropped
    /// the `sequencer` entry on the belief that no such state exists. It does,
    /// so the entry is back — checked last, so an ordinary stopped pick is
    /// still reported by its own marker.
    ///
    /// So this pins four states: a real stopped range is refused by its marker,
    /// a completed range is not refused, a range whose conflict was committed
    /// by hand IS refused by the todo list alone, and an abandoned range is
    /// refused too (its remedy is `git revert --quit`, pinned through the close
    /// in `a_close_after_an_abandoned_pick_range_is_refused_and_names_quit`).
    // orgasmic:TASK-QGWK7.1.1.1.1,TASK-QGWK7.1.1.1.1.1,TASK-QGWK7.1.1.1.1.1.1
    #[test]
    fn an_interrupted_pick_sequence_is_refused_and_a_finished_one_is_not() {
        let fixture = dispatch_cleanup_fixture("task-sequencer");
        let git_dir = fixture.root.join(".git");
        stack_three_revisions(&fixture.root);
        let stacked = git_capture_ok(&fixture.root, &["rev-parse", "HEAD"]);
        assert_eq!(
            sequencer_operation_in_progress(&git_dir),
            None,
            "a clean worktree must not look like an operation in progress"
        );

        // A real interrupted range: the first pick conflicts, and git writes
        // both the marker and the todo list.
        let interrupted = Command::new("git")
            .args(["revert", "--no-edit", "HEAD~1", "HEAD~2"])
            .current_dir(&fixture.root)
            .output()
            .unwrap();
        assert!(
            !interrupted.status.success()
                && git_dir.join("sequencer").is_dir()
                && git_dir.join("REVERT_HEAD").exists(),
            "the fixture must really be a stopped multi-pick sequence, and git must really \
             pair the todo list with the marker: {}{}",
            String::from_utf8_lossy(&interrupted.stdout),
            String::from_utf8_lossy(&interrupted.stderr)
        );
        assert_eq!(
            sequencer_operation_in_progress(&git_dir),
            Some("revert"),
            "a stopped range carries its own marker, and `sequencer` is checked last so the \
             refusal names the pick, not the range"
        );

        // TASK-QGWK7.1.1.1.1.1.1 D-1: resolving the conflict and committing by
        // hand — an ordinary, documented route — clears `REVERT_HEAD` and
        // leaves the todo list. The range is LIVE: `git revert --continue`
        // resumes it from here. Nothing but the `sequencer` entry refuses this.
        std::fs::write(fixture.root.join("base.txt"), "resolved\n").unwrap();
        git_ok(&fixture.root, &["add", "base.txt"]);
        git_ok(&fixture.root, &["commit", "-qm", "resolved by hand"]);
        assert!(
            git_dir.join("sequencer").is_dir()
                && !git_dir.join("REVERT_HEAD").exists()
                && !git_dir.join("CHERRY_PICK_HEAD").exists(),
            "the fixture must be the hand-committed stopped range: todo list, no marker"
        );
        assert_eq!(
            sequencer_operation_in_progress(&git_dir),
            Some(crate::sequencer_markers::STOPPED_PICK_RANGE),
            "a stopped range whose conflict was committed by hand keeps NO `*_HEAD` marker, \
             so only the todo list can refuse it"
        );

        git_ok(&fixture.root, &["revert", "--quit"]);
        assert_eq!(
            sequencer_operation_in_progress(&git_dir),
            None,
            "`--quit` clears the sequencer state, so the guard must let go of it"
        );

        // A range that completes cleanly leaves nothing behind either: no close
        // may be refused after a normal multi-commit revert.
        git_ok(&fixture.root, &["reset", "-q", "--hard", "HEAD"]);
        git_ok(&fixture.root, &["revert", "--no-edit", "HEAD", "HEAD~1"]);
        assert_eq!(
            sequencer_operation_in_progress(&git_dir),
            None,
            "a COMPLETED sequence must not look like one in progress"
        );

        // TASK-QGWK7.1.1.1.1.1 C-2, the latch, kept by TASK-QGWK7.1.1.1.1.1.1
        // D-1. Abandoning a stopped range with `git reset --hard` clears the
        // marker and leaves `.git/sequencer` behind (measured), and this state
        // is indistinguishable from the hand-committed LIVE range above — so
        // the guard refuses both, and the refusal names `git revert --quit` as
        // the one-command way out. Back to the stacked tip first, so this is
        // the SAME stopped range the top of the test built.
        git_ok(&fixture.root, &["reset", "-q", "--hard", &stacked]);
        let stopped = Command::new("git")
            .args(["revert", "--no-edit", "HEAD~1", "HEAD~2"])
            .current_dir(&fixture.root)
            .output()
            .unwrap();
        assert!(
            !stopped.status.success(),
            "the second fixture must really stop between picks: {}{}",
            String::from_utf8_lossy(&stopped.stdout),
            String::from_utf8_lossy(&stopped.stderr)
        );
        git_ok(&fixture.root, &["reset", "-q", "--hard"]);
        assert!(
            git_dir.join("sequencer").is_dir() && !git_dir.join("REVERT_HEAD").exists(),
            "the fixture must be the ABANDONED range: todo list left, marker gone"
        );
        assert_eq!(
            sequencer_operation_in_progress(&git_dir),
            Some(crate::sequencer_markers::STOPPED_PICK_RANGE),
            "an abandoned range is not distinguishable from a live hand-committed one, so it \
             is refused too — with `git revert --quit` named as the way out"
        );
        git_ok(&fixture.root, &["revert", "--quit"]);
        assert_eq!(
            sequencer_operation_in_progress(&git_dir),
            None,
            "and `git revert --quit` — the remedy the refusal names — really does clear it"
        );
    }

    /// TASK-QGWK7.1.1.1.1.1 C-2, through the close rather than the predicate,
    /// re-aimed by TASK-QGWK7.1.1.1.1.1.1 D-1. `git reset --hard` after a
    /// stopped range leaves `.git/sequencer` on disk for good — `git status`
    /// says nothing, an ordinary commit does not clear it, and only
    /// `git revert --quit` does (measured). C-2 read that as a reason to stop
    /// refusing the state; but on disk it is the SAME state as a live range
    /// whose conflict was committed by hand, which must be refused, so the
    /// guard cannot tell them apart and refuses both.
    ///
    /// What makes that acceptable is the MESSAGE. This pins it: the refusal
    /// must name `git revert --quit`, or an abandoned range becomes a permanent
    /// unexplained refusal of every close — which is the trap C-2 was right
    /// about.
    // orgasmic:TASK-QGWK7.1.1.1.1.1,TASK-QGWK7.1.1.1.1.1.1
    #[test]
    fn a_close_after_an_abandoned_pick_range_is_refused_and_names_quit() {
        let fixture = dispatch_cleanup_fixture("task-abandoned");
        stack_three_revisions(&fixture.root);
        let stopped = Command::new("git")
            .args(["revert", "--no-edit", "HEAD~1", "HEAD~2"])
            .current_dir(&fixture.root)
            .output()
            .unwrap();
        assert!(
            !stopped.status.success(),
            "the fixture must really stop between picks: {}{}",
            String::from_utf8_lossy(&stopped.stdout),
            String::from_utf8_lossy(&stopped.stderr)
        );
        git_ok(&fixture.root, &["reset", "-q", "--hard"]);
        assert!(
            fixture.root.join(".git/sequencer").is_dir()
                && !fixture.root.join(".git/REVERT_HEAD").exists(),
            "the fixture must be the abandoned range: the leftover todo list with no marker"
        );
        let open = dispatch_cleanup_record(&fixture, "TASK-ABANDONED");

        let cleanup = cleanup_dispatch(&fixture.root, &open, false, false);

        let error = cleanup.error.clone().unwrap_or_default();
        assert_eq!(
            cleanup.status,
            CleanupStatus::Partial,
            "a leftover todo list is a range `git revert --continue` still resumes, so the \
             close must stand down: {error}"
        );
        assert!(
            error.contains("git revert --quit"),
            "and it must name the ONE command that clears a todo list `git status` does not \
             even show, or the refusal is permanent and unexplained: {error}"
        );
        assert!(
            fixture
                .root
                .join(".orgasmic/tasks/TASK-ABANDONED/dispatches/tx-start-task-abandoned/report.md")
                .exists(),
            "the promoted report is never the casualty: it stays on disk for the re-run"
        );
        assert_eq!(
            staged_record_paths(&fixture.root, "tx-start-task-abandoned"),
            "",
            "a refusal must stage nothing"
        );

        // The remedy the message names must actually unblock the persist the
        // close was standing down from — otherwise the advice is a dead end.
        // Straight through the guarded function, since the promoted files are
        // already on disk and that is all it needs.
        git_ok(&fixture.root, &["revert", "--quit"]);
        commit_promoted_dispatch_record(&fixture.root, "TASK-ABANDONED", "tx-start-task-abandoned")
            .expect("`git revert --quit` must leave the record persistable");
        let in_history = Command::new("git")
            .args([
                "cat-file",
                "-e",
                "HEAD:.orgasmic/tasks/TASK-ABANDONED/dispatches/tx-start-task-abandoned/report.md",
            ])
            .current_dir(&fixture.root)
            .output()
            .unwrap();
        assert!(
            in_history.status.success(),
            "and after `--quit` the record must really reach history, not merely stop being \
             refused: {}",
            String::from_utf8_lossy(&in_history.stderr)
        );
    }

    /// TASK-QGWK7.1.1.1.1.1.1 D-1, through the close. The state that made the
    /// `sequencer` entry load-bearing, built the ordinary way: a stopped
    /// multi-pick range whose conflict the manager resolved and committed with
    /// `git commit` rather than `git revert --continue`. git clears
    /// `REVERT_HEAD` and leaves the todo list, so NO `*_HEAD` marker refuses
    /// this — and the range is live, because `git revert --continue` resumes it
    /// from here (measured, git 2.52.0).
    ///
    /// Without the `sequencer` entry the close commits the record mid-range, at
    /// a point the manager did not choose; if they then abandon the range with
    /// `git reset --hard <pre-range>` the record dies with it.
    // orgasmic:TASK-QGWK7.1.1.1.1.1.1
    #[test]
    fn a_close_inside_a_hand_committed_pick_range_is_refused() {
        let fixture = dispatch_cleanup_fixture("task-handcommit");
        stack_three_revisions(&fixture.root);
        let stopped = Command::new("git")
            .args(["revert", "--no-edit", "HEAD~1", "HEAD~2"])
            .current_dir(&fixture.root)
            .output()
            .unwrap();
        assert!(
            !stopped.status.success(),
            "the fixture must really stop between picks: {}{}",
            String::from_utf8_lossy(&stopped.stdout),
            String::from_utf8_lossy(&stopped.stderr)
        );
        // Resolve and commit by hand — not `git revert --continue`.
        std::fs::write(fixture.root.join("base.txt"), "resolved\n").unwrap();
        git_ok(&fixture.root, &["add", "base.txt"]);
        git_ok(&fixture.root, &["commit", "-qm", "resolved by hand"]);
        assert!(
            fixture.root.join(".git/sequencer").is_dir()
                && !fixture.root.join(".git/REVERT_HEAD").exists()
                && !fixture.root.join(".git/CHERRY_PICK_HEAD").exists(),
            "the fixture must be the live range with NO marker: the todo list alone"
        );
        let tip = git_capture_ok(&fixture.root, &["rev-parse", "HEAD"]);
        let open = dispatch_cleanup_record(&fixture, "TASK-HANDCOMMIT");

        let cleanup = cleanup_dispatch(&fixture.root, &open, false, false);

        let error = cleanup.error.clone().unwrap_or_default();
        assert_eq!(
            cleanup.status,
            CleanupStatus::Partial,
            "a range `git revert --continue` still resumes must refuse the close, marker or \
             no marker: {error}"
        );
        assert!(
            error.contains("revert or cherry-pick sequence"),
            "the refusal must name the leftover range, not a pick that is not stopped: {error}"
        );
        assert!(
            error.contains("git revert --continue"),
            "and it must name the command that finishes this range: {error}"
        );
        assert!(
            fixture
                .root
                .join(
                    ".orgasmic/tasks/TASK-HANDCOMMIT/dispatches/tx-start-task-handcommit/report.md"
                )
                .exists(),
            "the promoted report is never the casualty: it stays on disk for the re-run"
        );
        assert_eq!(
            staged_record_paths(&fixture.root, "tx-start-task-handcommit"),
            "",
            "a refusal must stage nothing"
        );
        assert_eq!(
            git_capture_ok(&fixture.root, &["rev-parse", "HEAD"]),
            tip,
            "and the close must not commit into the middle of the range"
        );
    }

    /// TASK-QGWK7.1.1.1 F-5. On a detached HEAD `update-ref HEAD` moves only the
    /// detached HEAD: the record commit is on NO branch, the manager's next
    /// checkout orphans it, and the close reported `ok`. Unborn HEAD was already
    /// safe (`rev-parse --verify HEAD^{commit}` fails before anything is
    /// staged); this makes the two unusual shapes symmetric.
    // orgasmic:TASK-QGWK7.1.1.1
    #[test]
    fn a_close_on_a_detached_head_refuses_to_persist_rather_than_orphan_the_record() {
        let fixture = dispatch_cleanup_fixture("task-detached");
        let base = git_capture_ok(&fixture.root, &["rev-parse", "--abbrev-ref", "HEAD"]);
        let branch_tip = git_capture_ok(&fixture.root, &["rev-parse", &base]);
        git_ok(&fixture.root, &["checkout", "-q", "--detach"]);
        let open = dispatch_cleanup_record(&fixture, "TASK-DETACHED");

        let cleanup = cleanup_dispatch(&fixture.root, &open, false, false);

        let error = cleanup.error.clone().unwrap_or_default();
        assert_eq!(cleanup.status, CleanupStatus::Partial, "{error}");
        assert!(
            error.contains("HEAD is detached"),
            "a detached HEAD must be refused by name, not reported as ok: {error}"
        );
        assert!(
            fixture
                .root
                .join(".orgasmic/tasks/TASK-DETACHED/dispatches/tx-start-task-detached/report.md")
                .exists(),
            "the promoted report is never the casualty: it stays on disk for the re-run"
        );
        assert_eq!(
            staged_record_paths(&fixture.root, "tx-start-task-detached"),
            "",
            "the refusal lands before the real `git add`, so nothing is staged"
        );
        assert_eq!(
            git_capture_ok(&fixture.root, &["rev-parse", &base]),
            branch_tip,
            "no branch may move for a close that could not resolve one"
        );
    }

    /// TASK-QGWK7.1.1 M-2. `git add -- <dir>` exits 0 on a directory that is
    /// only PARTIALLY ignored and stages the rest, so a rule as ordinary as
    /// `*.log` would thin the record while the close reported success.
    // orgasmic:TASK-QGWK7.1.1
    #[test]
    fn partially_ignored_record_directory_is_reported_not_silently_thinned() {
        let fixture = dispatch_cleanup_fixture("task-ignored");
        std::fs::write(fixture.root.join(".gitignore"), "*.log\n").unwrap();
        git_ok(&fixture.root, &["add", ".gitignore"]);
        git_ok(&fixture.root, &["commit", "-qm", "ignore logs"]);
        let open = dispatch_cleanup_record(&fixture, "TASK-IGNORED");

        let cleanup = cleanup_dispatch(&fixture.root, &open, true, false);

        let error = cleanup.error.clone().unwrap_or_default();
        assert_eq!(cleanup.status, CleanupStatus::Partial, "{error}");
        assert!(
            error.contains("stdout.log") && error.contains("ignore"),
            "the close must name the entry the ignore rule dropped: {error}"
        );
        // What did land is still committed — a thinned record is worth keeping
        // and worth naming, not worth discarding.
        let in_history = Command::new("git")
            .args([
                "cat-file",
                "-e",
                "HEAD:.orgasmic/tasks/TASK-IGNORED/dispatches/tx-start-task-ignored/report.md",
            ])
            .current_dir(&fixture.root)
            .output()
            .unwrap();
        assert!(
            in_history.status.success(),
            "the entries that were not ignored must still reach history"
        );
    }

    /// TASK-QGWK7.1.1 M-5: `:REPORT_PATH:` is committed to the tx log, so the
    /// manager-supplied route must be project-relative like the other three.
    // orgasmic:TASK-QGWK7.1.1
    #[test]
    fn manager_supplied_report_path_property_is_project_relative() {
        let project_root = PathBuf::from("/tmp/orgasmic-report-path-probe");
        let mut inside = vec![(
            "REPORT_PATH".to_string(),
            project_root
                .join(".orgasmic/tasks/TASK-X/dispatches/tx-1/report.md")
                .display()
                .to_string(),
        )];
        normalize_report_path_property(&project_root, &mut inside).unwrap();
        assert_eq!(
            inside[0].1, ".orgasmic/tasks/TASK-X/dispatches/tx-1/report.md",
            "an absolute path under the project must be relativized, not committed as-is"
        );

        let mut already_relative = vec![("REPORT_PATH".to_string(), "docs/review.md".to_string())];
        normalize_report_path_property(&project_root, &mut already_relative).unwrap();
        assert_eq!(already_relative[0].1, "docs/review.md");

        let mut outside = vec![(
            "REPORT_PATH".to_string(),
            "/var/tmp/elsewhere/report.md".to_string(),
        )];
        let err = normalize_report_path_property(&project_root, &mut outside)
            .expect_err("a path with no project-relative form must be refused, not committed");
        assert!(
            err.to_string().contains("project-relative"),
            "the refusal must say why: {err}"
        );
    }

    // orgasmic:TASK-QGWK7.1
    #[test]
    fn no_worktree_remove_with_artifacts_needs_close_guard() {
        let fixture = dispatch_cleanup_fixture("task-fence");
        let open = dispatch_cleanup_record(&fixture, "TASK-FENCE");
        assert!(
            close_needs_artifact_fence(false, &open),
            "--no-worktree-remove with LAST_PATH/STDOUT_PATH must take the close guard"
        );
        assert!(close_needs_artifact_fence(true, &open));
        let mut bare = open.clone();
        bare.last_path = None;
        bare.stdout_path = None;
        assert!(
            !close_needs_artifact_fence(false, &bare),
            "a no-op close with nothing to promote must not take the guard"
        );
        // TASK-QGWK7.1.1 M-6: the guard reserves a WORKTREE, so a record
        // without one cannot have it. Claiming the fence for that shape made
        // it requested and silently absent.
        let mut worktreeless = open.clone();
        worktreeless.worktree = None;
        assert!(
            !close_needs_artifact_fence(false, &worktreeless),
            "the predicate must not claim a fence reserve_close_guard cannot take"
        );
        assert!(!close_needs_artifact_fence(true, &worktreeless));
    }

    // orgasmic:TASK-QGWK7.1
    #[test]
    fn no_worktree_remove_promote_takes_cleanup_lock() {
        let fixture = dispatch_cleanup_fixture("task-lock");
        let open = dispatch_cleanup_record(&fixture, "TASK-LOCK");
        let held = acquire_dispatch_cleanup_lock(&fixture.root).unwrap();
        let root = fixture.root.clone();
        let worktree = open.worktree.clone();
        let last = fixture.last.clone();
        let stdout = fixture.stdout.clone();
        let tx_id = open.tx_id.clone();
        let (started_tx, started_rx) = std::sync::mpsc::channel();
        let (done_tx, done_rx) = std::sync::mpsc::channel();
        let join = std::thread::spawn(move || {
            started_tx.send(()).unwrap();
            let result = promote_dispatch_artifacts_in_place(
                &root,
                worktree.as_deref(),
                &last,
                &stdout,
                "TASK-LOCK",
                &tx_id,
            );
            done_tx.send(result).unwrap();
        });
        started_rx.recv().unwrap();
        // While we hold the lock, the promote must not finish.
        assert!(
            done_rx
                .recv_timeout(std::time::Duration::from_millis(150))
                .is_err(),
            "promote_dispatch_artifacts_in_place must take the cleanup lock"
        );
        drop(held);
        let outcome = done_rx
            .recv_timeout(std::time::Duration::from_secs(5))
            .expect("promote should finish after lock release")
            .expect("promote ok");
        assert!(outcome.report_path.is_some());
        join.join().unwrap();
    }

    // orgasmic:TASK-QGWK7.1
    #[test]
    fn no_worktree_remove_promote_against_missing_worktree_is_ok() {
        let fixture = dispatch_cleanup_fixture("task-missing-wt");
        std::fs::write(&fixture.last, "rescued report\n").unwrap();
        // Reclaim the worktree the way prune would, leaving tmp artifacts.
        assert!(Command::new("git")
            .args(["worktree", "remove"])
            .arg(&fixture.worktree)
            .current_dir(&fixture.root)
            .status()
            .unwrap()
            .success());
        assert!(!fixture.worktree.exists());
        let open = dispatch_cleanup_record(&fixture, "TASK-MISSING-WT");

        let cleanup = cleanup_dispatch(&fixture.root, &open, false, false);
        assert_eq!(
            cleanup.status,
            CleanupStatus::Ok,
            "promote against an already-reclaimed worktree must report ok, not partial: {:?}",
            cleanup.error
        );
        let report_path = cleanup
            .report_path
            .as_deref()
            .expect("close must name a promoted REPORT_PATH");
        assert!(fixture.root.join(report_path).exists());
        assert!(!fixture.last.exists());
    }

    // orgasmic:TASK-QGWK7.1
    #[test]
    fn finalize_report_path_fallback_is_project_relative() {
        let tmp = tempfile::tempdir().unwrap();
        let home = Home::at(tmp.path().join("home"));
        home.ensure().unwrap();
        let project_root = tmp.path().join("project");
        std::fs::create_dir_all(project_root.join(".orgasmic")).unwrap();
        std::fs::write(
            home.board(),
            format!(
                "#+title: orgasmic board\n#+orgasmic_version: 1\n\n* PROJECT demo\n:PROPERTIES:\n:ID:               demo\n:PATH:             {}\n:BRANCH:           main\n:STATUS:           active\n:END:\n",
                project_root.display()
            ),
        )
        .unwrap();
        let abs = project_root.join(".orgasmic/tmp/dispatch/task-x/task-x-last.txt");
        let rel = project_relative_report_path_fallback(&home, Some("demo"), Some(&abs))
            .expect("absolute path under project root must relativize");
        assert_eq!(rel, ".orgasmic/tmp/dispatch/task-x/task-x-last.txt");
        assert!(!rel.starts_with('/'));
        let outside = tmp.path().join("elsewhere/last.txt");
        assert_eq!(
            project_relative_report_path_fallback(&home, Some("demo"), Some(&outside)),
            None,
            "paths outside the project must not become REPORT_PATH"
        );
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
            Some("tx-start-salvage-late-writer"),
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

    /// A repository with one ordinary file and one GITLINK, written by git
    /// itself. `extra` goes to `git init`, `index_args` to `update-index`, so
    /// the object format and index version are git's to choose rather than this
    /// test's to encode. Returns the index bytes.
    #[cfg(unix)]
    fn git_written_index(extra: &[&str], index_args: &[&str]) -> Vec<u8> {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        let git = |args: &[&str]| {
            let output = Command::new("git")
                .args(args)
                .current_dir(root)
                .output()
                .expect("run git");
            assert!(
                output.status.success(),
                "git {args:?} failed: {}",
                String::from_utf8_lossy(&output.stderr)
            );
            String::from_utf8_lossy(&output.stdout).trim().to_string()
        };
        let mut init = vec!["init", "-q", "-b", "main"];
        init.extend_from_slice(extra);
        git(&init);
        git(&["config", "user.email", "tester@example.com"]);
        git(&["config", "user.name", "Test User"]);
        std::fs::write(root.join("a.txt"), "ordinary file").unwrap();
        git(&["add", "a.txt"]);
        git(&["commit", "-m", "init"]);
        let head = git(&["rev-parse", "HEAD"]);
        let mut update = vec!["update-index"];
        update.extend_from_slice(index_args);
        let cacheinfo = format!("160000,{head},vendor/sub");
        update.extend_from_slice(&["--add", "--cacheinfo", &cacheinfo]);
        git(&update);
        std::fs::read(root.join(".git/index")).unwrap()
    }

    /// TASK-RMA18.1.1 finding 1: the delete-path predicate now reads the INDEX,
    /// so its parser is checked against indexes GIT WROTE rather than against
    /// bytes this test encoded — including the two layouts that change the
    /// entry stride, a SHA-256 object format (32-byte object ids in the same
    /// layout, with nothing in the header to announce it) and index version 4
    /// (prefix-compressed, unpadded paths).
    // orgasmic:TASK-RMA18.1.1
    #[cfg(unix)]
    #[test]
    fn index_gitlink_paths_reads_the_gitlinks_git_wrote() {
        for (label, init, index_args) in [
            ("default", [].as_slice(), [].as_slice()),
            (
                "sha256",
                ["--object-format=sha256"].as_slice(),
                [].as_slice(),
            ),
            ("v4", [].as_slice(), ["--index-version", "4"].as_slice()),
        ] {
            let bytes = git_written_index(init, index_args);
            assert_eq!(
                index_gitlink_paths(&bytes).unwrap_or_else(|err| panic!("{label}: {err}")),
                vec!["vendor/sub".to_string()],
                "{label}: the mode-160000 entry is the only gitlink, and `a.txt` is not one"
            );
        }
    }

    /// UNKNOWN MEANS KEEP. Every one of these is an index this cannot trust, and
    /// each must be an ERROR rather than "no submodules" — the caller turns the
    /// error into a refusal, and turning it into an empty answer is exactly the
    /// fail-open this task exists to close.
    // orgasmic:TASK-RMA18.1.1
    #[cfg(unix)]
    #[test]
    fn index_gitlink_paths_fails_closed_on_anything_it_cannot_trust() {
        let good = git_written_index(&[], &[]);
        assert!(index_gitlink_paths(&good).is_ok(), "control must parse");

        for (label, bytes) in [
            ("empty", Vec::new()),
            ("not an index", b"NOTDIRC and then some".to_vec()),
            ("truncated mid-entry", good[..good.len() / 2].to_vec()),
            ("trailing hash chopped", good[..good.len() - 4].to_vec()),
            ("entry count overstated", {
                let mut bytes = good.clone();
                bytes[8..12].copy_from_slice(&99u32.to_be_bytes());
                bytes
            }),
            ("unsupported version", {
                let mut bytes = good.clone();
                bytes[4..8].copy_from_slice(&9u32.to_be_bytes());
                bytes
            }),
        ] {
            assert!(
                index_gitlink_paths(&bytes).is_err(),
                "{label}: an index that cannot be trusted must be an error, not an empty answer"
            );
        }
    }

    /// The predicate must fire on a POPULATED gitlink checkout and NOT on an
    /// uninitialized placeholder — git's own rule is `!is_empty_dir`, and a
    /// refusal that fired on empty placeholders would make the verb useless for
    /// every worktree of a repository that has any submodule at all.
    ///
    /// Both halves are measured on ONE linked worktree, through the same
    /// anchored handle the delete path uses, with the submodule recorded ONLY in
    /// the index — no `.gitmodules`, no admin `modules/` directory — so it is
    /// the index-derived branch that is under test in both directions.
    // orgasmic:TASK-RMA18.1.1
    #[cfg(unix)]
    #[test]
    fn worktree_submodule_refusal_fires_on_a_populated_gitlink_and_not_a_placeholder() {
        let tmp = tempfile::tempdir().unwrap();
        let project = tmp.path().join("project");
        std::fs::create_dir_all(&project).unwrap();
        let git = |root: &Path, args: &[&str]| {
            let output = Command::new("git")
                .args(args)
                .current_dir(root)
                .output()
                .expect("run git");
            assert!(
                output.status.success(),
                "git {args:?} failed: {}",
                String::from_utf8_lossy(&output.stderr)
            );
            String::from_utf8_lossy(&output.stdout).trim().to_string()
        };
        git(&project, &["init", "-q", "-b", "main"]);
        git(&project, &["config", "user.email", "tester@example.com"]);
        git(&project, &["config", "user.name", "Test User"]);
        std::fs::write(project.join("a.txt"), "ordinary file").unwrap();
        git(&project, &["add", "a.txt"]);
        git(&project, &["commit", "-m", "init"]);
        let head = git(&project, &["rev-parse", "HEAD"]);
        git(
            &project,
            &[
                "update-index",
                "--add",
                "--cacheinfo",
                &format!("160000,{head},vendor/sub"),
            ],
        );
        git(&project, &["commit", "-m", "gitlink, no .gitmodules"]);

        let worktree = tmp.path().join("wt");
        git(
            &project,
            &["worktree", "add", "-q", worktree.to_str().unwrap(), "HEAD"],
        );
        assert!(
            !worktree.join(".gitmodules").exists(),
            "fixture premise: nothing but the index records this submodule"
        );
        let handle = std::fs::File::open(&worktree).unwrap();

        // `git worktree add` leaves an EMPTY placeholder for an uninitialized
        // submodule, and git removes such a worktree without --force.
        assert!(worktree.join("vendor/sub").is_dir());
        assert_eq!(
            worktree_submodule_refusal(&handle, &worktree, None),
            None,
            "an uninitialized submodule placeholder must stay removable"
        );

        std::fs::write(worktree.join("vendor/sub/lib.txt"), "checked out").unwrap();
        let reason = worktree_submodule_refusal(&handle, &worktree, None)
            .expect("a populated gitlink checkout must be refused");
        assert!(
            reason.contains("vendor/sub"),
            "the refusal must name the submodule it found, got: {reason}"
        );
    }

    /// A SPLIT INDEX holds a delta against a shared index this never opens, so
    /// the gitlinks visible in it are not the whole set. git writes one on
    /// demand, so the fixture is git's own, and the answer must be a refusal
    /// rather than the partial list.
    // orgasmic:TASK-RMA18.1.1
    #[cfg(unix)]
    #[test]
    fn index_gitlink_paths_refuses_a_split_index() {
        let bytes = git_written_index(&[], &["--split-index"]);
        let err = index_gitlink_paths(&bytes)
            .expect_err("a split index must not answer for the shared index it defers to");
        assert!(
            err.contains("SPLIT INDEX"),
            "the refusal must name why it refused, got: {err}"
        );
        assert!(
            bytes.windows(4).any(|window| window == b"link"),
            "fixture premise: git must actually have written a split index here"
        );
    }

    #[cfg(unix)]
    struct PermissionRestore {
        path: PathBuf,
        mode: u32,
    }

    #[cfg(unix)]
    impl PermissionRestore {
        fn new(path: &Path, mode: u32) -> Self {
            Self {
                path: path.to_path_buf(),
                mode,
            }
        }
    }

    #[cfg(unix)]
    impl Drop for PermissionRestore {
        fn drop(&mut self) {
            use std::os::unix::fs::PermissionsExt;

            let _ =
                std::fs::set_permissions(&self.path, std::fs::Permissions::from_mode(self.mode));
        }
    }

    /// TASK-GRCWC: the cheap parity fixture. The preliminary walk and the
    /// destructive traversal must reject the same unreadable descendant, and
    /// the removal-side proof must show it rejected before touching anything.
    #[cfg(unix)]
    #[test]
    fn unreadable_descendant_is_refused_by_both_walk_and_removal_untouched() {
        use std::os::unix::fs::PermissionsExt;

        let tmp = tempfile::tempdir().unwrap();
        let worktree = tmp.path().join("wt");
        let blocked = worktree.join("blocked");
        std::fs::create_dir_all(&blocked).unwrap();
        let survivor = blocked.join("survivor.txt");
        std::fs::write(&survivor, "must survive").unwrap();
        let _restore = PermissionRestore::new(&blocked, 0o755);
        std::fs::set_permissions(&blocked, std::fs::Permissions::from_mode(0o000)).unwrap();
        assert!(
            std::fs::File::open(&blocked).is_err(),
            "mode 000 must actually make the directory unreadable; a privileged test process \
             must fail this fixture rather than report a meaningless pass"
        );

        let worktree_handle = std::fs::File::open(&worktree).unwrap();
        let walk_error = walk_worktree(&worktree_handle)
            .expect_err("the preliminary walk must reject the unreadable descendant");
        assert!(
            walk_error.to_string().contains("blocked"),
            "the walk refusal must name the descendant: {walk_error:#}"
        );

        let parent = std::fs::File::open(tmp.path()).unwrap();
        let expected = anchored_dir::identity_at(&parent, std::ffi::OsStr::new("wt")).unwrap();
        let failure =
            anchored_dir::remove_dir_all_at(&parent, std::ffi::OsStr::new("wt"), expected)
                .expect_err("the removal traversal must reject the same unreadable descendant");
        assert!(
            !failure.touched,
            "the pure unreadable fixture has no earlier sibling to remove, so refusal must be \
             untouched: {:#}",
            failure.error
        );
        assert!(
            failure.error.to_string().contains("blocked"),
            "the removal refusal must name the same unreadable descendant: {:#}",
            failure.error
        );
        std::fs::set_permissions(&blocked, std::fs::Permissions::from_mode(0o755)).unwrap();
        assert!(
            survivor.is_file(),
            "the removal traversal must not report success or destroy the unreadable tree"
        );
    }

    /// TASK-GRCWC: the expensive parity fixture is generated to stay coupled
    /// to the production bound. A pure chain means reaching the depth error
    /// cannot have removed a sibling on the way down.
    #[cfg(unix)]
    #[test]
    fn over_depth_descendant_is_refused_by_both_walk_and_removal_untouched() {
        let tmp = tempfile::tempdir().unwrap();
        let worktree = tmp.path().join("wt");
        std::fs::create_dir_all(&worktree).unwrap();
        let mut deepest = worktree.clone();
        for _ in 0..=anchored_dir::MAX_DEPTH {
            deepest.push("d");
            std::fs::create_dir(&deepest).unwrap();
        }
        let survivor = deepest.join("survivor.txt");
        std::fs::write(&survivor, "must survive").unwrap();

        let worktree_handle = std::fs::File::open(&worktree).unwrap();
        let walk_error = walk_worktree(&worktree_handle)
            .expect_err("the preliminary walk must reject a tree beyond MAX_DEPTH");
        assert!(
            walk_error
                .to_string()
                .contains(&anchored_dir::MAX_DEPTH.to_string()),
            "the walk refusal must name the production bound: {walk_error:#}"
        );

        let parent = std::fs::File::open(tmp.path()).unwrap();
        let expected = anchored_dir::identity_at(&parent, std::ffi::OsStr::new("wt")).unwrap();
        let failure =
            anchored_dir::remove_dir_all_at(&parent, std::ffi::OsStr::new("wt"), expected)
                .expect_err("the removal traversal must reject the same over-depth tree");
        assert!(
            !failure.touched,
            "a pure chain cannot be partially removed before the depth refusal: {:#}",
            failure.error
        );
        assert!(
            failure
                .error
                .to_string()
                .contains(&anchored_dir::MAX_DEPTH.to_string()),
            "the removal refusal must name the same production bound: {:#}",
            failure.error
        );
        assert!(
            survivor.is_file(),
            "the removal traversal must not report success or destroy the deep tree"
        );
    }

    /// TASK-RMA18.1.1.1 finding A, at the predicate: with NO REPOSITORY behind
    /// the worktree the disk is the only witness, and the witness is a nested
    /// `.git` OF ANY TYPE.
    ///
    /// THE TYPE IS THE WHOLE POINT (the reviewer's first correction to the C1
    /// ruling). This fixture uses the shape `git submodule update --init` writes
    /// inside a linked worktree — `.git` as a FILE holding
    /// `gitdir: ../../.git/modules/<name>` — pointed at an admin directory that
    /// does not exist, which is exactly what a repo-gone submodule looks like.
    /// A "populated `.git` DIRECTORY" predicate returns false on it.
    ///
    /// Three other things are pinned here because each one would ship a
    /// different defect: the refusal NAMES the offending path so the operator
    /// can act on it; it does NOT offer `git worktree remove --force`, which
    /// cannot run once the repository is gone and which this verb does not have;
    /// and an ordinary directory — including an EMPTY submodule placeholder — is
    /// not a signal, so `worktree_prune_removes_a_worktree_whose_repo_is_gone`
    /// keeps its meaning.
    // orgasmic:TASK-RMA18.1.1.1
    #[cfg(unix)]
    #[test]
    fn a_repo_gone_worktree_is_refused_over_a_nested_git_of_any_type() {
        let tmp = tempfile::tempdir().unwrap();
        let worktree = tmp.path().join("wt");
        // An empty placeholder and an ordinary populated directory. Neither is a
        // repository, and neither may refuse.
        std::fs::create_dir_all(worktree.join("vendor/placeholder")).unwrap();
        std::fs::create_dir_all(worktree.join("src")).unwrap();
        std::fs::write(worktree.join("src/main.rs"), "fn main() {}").unwrap();
        let handle = std::fs::File::open(&worktree).unwrap();

        let walk = walk_worktree(&handle).unwrap();
        assert_eq!(
            walk.nested_git, None,
            "an empty placeholder and an ordinary tree hold no repository"
        );
        assert_eq!(
            worktree_submodule_refusal(&handle, &worktree, walk.nested_git.as_deref()),
            None,
            "a repo-gone worktree with nothing nested inside it must stay removable"
        );

        // Now the shape `git submodule update --init` leaves behind, with the
        // admin directory it names already gone: a `.git` FILE.
        std::fs::create_dir_all(worktree.join("vendor/sub")).unwrap();
        std::fs::write(
            worktree.join("vendor/sub/.git"),
            "gitdir: ../../.git/modules/sub\n",
        )
        .unwrap();
        std::fs::write(worktree.join("vendor/sub/lib.txt"), "worker output").unwrap();
        assert!(
            worktree.join("vendor/sub/.git").is_file()
                && !worktree.join("vendor/sub/.git").is_dir(),
            "fixture premise: the nested `.git` must be a FILE, not a directory"
        );

        let walk = walk_worktree(&handle).unwrap();
        assert_eq!(
            walk.nested_git.as_deref(),
            Some("vendor/sub/.git"),
            "the walk must report the nested `.git` by its path inside the worktree"
        );
        let reason = worktree_submodule_refusal(&handle, &worktree, walk.nested_git.as_deref())
            .expect("a repo-gone worktree holding a nested repository must be refused");
        assert!(
            reason.contains("vendor/sub/.git"),
            "the refusal must NAME what to clear by hand, got: {reason}"
        );
        assert!(
            !reason.contains("or remove the worktree with `git worktree remove --force`"),
            "the repository is gone, so `git worktree remove --force` cannot run and the \
             remedy must not offer it, got: {reason}"
        );
        assert!(
            reason.contains("CANNOT run") && reason.contains("delete vendor/sub/.git yourself"),
            "the refusal must say the --force escape is unavailable and name what to clear by \
             hand, got: {reason}"
        );
    }

    /// The worktree's OWN `.git` is not its own refusal — and on the `RepoGone`
    /// path it is always there and always dangling, since a dangling `.git` is
    /// precisely what classified the worktree `RepoGone`. A depth-blind
    /// "any entry named `.git`" predicate would refuse EVERY repo-gone worktree
    /// and delete the verb's reason to exist.
    // orgasmic:TASK-RMA18.1.1.1
    #[cfg(unix)]
    #[test]
    fn the_worktrees_own_dangling_git_link_is_not_a_nested_repository() {
        let tmp = tempfile::tempdir().unwrap();
        let worktree = tmp.path().join("wt");
        std::fs::create_dir_all(&worktree).unwrap();
        std::fs::write(
            worktree.join(".git"),
            "gitdir: /nowhere/.git/worktrees/task-gone\n",
        )
        .unwrap();
        std::fs::write(worktree.join("a.txt"), "ordinary file").unwrap();
        let handle = std::fs::File::open(&worktree).unwrap();

        let walk = walk_worktree(&handle).unwrap();
        assert_eq!(walk.nested_git, None, "depth 1 is the worktree's own link");
        assert!(walk.bytes > 0, "the size walk must still have counted");
        assert_eq!(
            worktree_submodule_refusal(&handle, &worktree, walk.nested_git.as_deref()),
            None,
            "a repo-gone worktree must stay reclaimable on the strength of its own `.git`"
        );
    }

    #[test]
    fn dispatch_wait_response_classifier_preserves_the_documented_exit_contract() {
        let generation = |status: &str| ManagerDispatchWaitGeneration {
            started_tx: "tx-1".into(),
            status: status.into(),
            run_id: Some("run-1".into()),
        };
        assert!(matches!(
            classify_dispatch_wait_round(&[generation("reported")]),
            DispatchWaitRound::Reported
        ));
        assert!(matches!(
            classify_dispatch_wait_round(&[generation("closed")]),
            DispatchWaitRound::Reported
        ));
        assert!(matches!(
            classify_dispatch_wait_round(&[generation("died")]),
            DispatchWaitRound::Died(_)
        ));
        assert!(matches!(
            classify_dispatch_wait_round(&[generation("unknown")]),
            DispatchWaitRound::Unknown(_)
        ));
        assert!(matches!(
            classify_dispatch_wait_round(&[generation("waiting")]),
            DispatchWaitRound::Waiting
        ));
    }
}
