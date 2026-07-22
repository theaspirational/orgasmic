// arch: arch_A53QX.5, arch_BVH7M.2
// orgasmic:arch_BVH7M, arch_A53QX, dec_R75SW
//! Append-only JSONL session writer.
//!
//! One file descriptor per run. Each line is a self-contained JSON object.
//! No compression in v0.0.1 (`dec_006`). Schema is intentionally loose at
//! this layer: the worker drivers serialize their own native events into
//! the `event` field; the daemon only enforces the envelope.
//!
//! Driver event vocabulary ([`DriverEvent`], [`Lifecycle`], [`BabysitterTool`])
//! is shared between `orgasmic-drivers` and `orgasmic-daemon::supervisor` so
//! the supervisor can persist driver-emitted events as well-typed payloads
//! without each driver duplicating the JSON envelope shape (`arch_004`).

use std::fs::{File, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use thiserror::Error;
use uuid::Uuid;

#[derive(Debug, Error)]
pub enum SessionError {
    #[error("session io: {0}")]
    Io(#[from] std::io::Error),
    #[error("session serialize: {0}")]
    Serialize(#[from] serde_json::Error),
    #[error("invalid run sub-state {0}")]
    InvalidRunSubState(String),
}

/// One session JSONL line.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionEnvelope {
    pub seq: u64,
    pub time: DateTime<Utc>,
    pub run_id: String,
    pub runtime_id: String,
    pub boot_id: String,
    pub kind: SessionEventKind,
    pub event: Value,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum SessionEventKind {
    /// Driver-native event payload (Claude ACP, Codex appserver, etc.).
    DriverEvent,
    /// Lifecycle event (acquire, attach, release, transition, etc.).
    Lifecycle,
    /// Babysitter summary chunk handed to a stall detector.
    BabysitterSummary,
    /// Free-form note written by a supervisor or recovery path.
    Note,
}

/// Identity tuple used to disambiguate cleanup from a replacement runtime
/// after restart (`arch_010`).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RuntimeIdentity {
    pub run_id: String,
    pub runtime_id: String,
    pub boot_id: String,
}

impl RuntimeIdentity {
    pub fn new(run_id: impl Into<String>, boot_id: impl Into<String>) -> Self {
        Self {
            run_id: run_id.into(),
            runtime_id: Uuid::new_v4().to_string(),
            boot_id: boot_id.into(),
        }
    }

    /// Predeclared identity for crash-recoverable acquire (recovery claims).
    pub fn planned(
        run_id: impl Into<String>,
        runtime_id: impl Into<String>,
        boot_id: impl Into<String>,
    ) -> Self {
        Self {
            run_id: run_id.into(),
            runtime_id: runtime_id.into(),
            boot_id: boot_id.into(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RunSubState(String);

impl RunSubState {
    /// Validate a performer-owned `<performer>.<verb>` sub-state.
    ///
    /// The namespace is the performer: any lowercase worker kind token,
    /// `human`, or reserved `ci`. The verb is a non-empty lowercase token
    /// using underscores for multi-word actions.
    pub fn new(value: impl Into<String>) -> Result<Self, SessionError> {
        let value = value.into();
        let Some((namespace, verb)) = value.split_once('.') else {
            return Err(SessionError::InvalidRunSubState(value));
        };
        if !is_valid_sub_state_namespace(namespace)
            || verb.is_empty()
            || !verb.chars().all(|c| c.is_ascii_lowercase() || c == '_')
        {
            return Err(SessionError::InvalidRunSubState(value));
        }
        Ok(Self(value))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

fn is_valid_sub_state_namespace(namespace: &str) -> bool {
    let mut chars = namespace.chars();
    matches!(chars.next(), Some(c) if c.is_ascii_lowercase())
        && chars.all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_')
}

/// Append-only writer for one session JSONL file.
pub struct SessionWriter {
    path: PathBuf,
    file: File,
    identity: RuntimeIdentity,
    seq: u64,
}

impl SessionWriter {
    pub fn open(path: impl AsRef<Path>, identity: RuntimeIdentity) -> Result<Self, SessionError> {
        let path = path.as_ref().to_path_buf();
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let file = OpenOptions::new().create(true).append(true).open(&path)?;
        Ok(Self {
            path,
            file,
            identity,
            seq: 0,
        })
    }

    /// Construct a writer from an already-authorized append handle. Callers
    /// use this when pathname re-resolution would discard retained file
    /// identity across a security-sensitive boundary.
    pub fn from_file(path: PathBuf, file: File, identity: RuntimeIdentity) -> Self {
        Self {
            path,
            file,
            identity,
            seq: 0,
        }
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn identity(&self) -> &RuntimeIdentity {
        &self.identity
    }

    pub fn next_seq(&self) -> u64 {
        self.seq
    }

    /// Append one envelope and return its sequence number.
    pub fn append(&mut self, kind: SessionEventKind, event: Value) -> Result<u64, SessionError> {
        let envelope = SessionEnvelope {
            seq: self.seq,
            time: Utc::now(),
            run_id: self.identity.run_id.clone(),
            runtime_id: self.identity.runtime_id.clone(),
            boot_id: self.identity.boot_id.clone(),
            kind,
            event,
        };
        let line = serde_json::to_string(&envelope)?;
        self.file.write_all(line.as_bytes())?;
        self.file.write_all(b"\n")?;
        self.file.flush()?;
        self.file.sync_all()?;
        let seq = self.seq;
        self.seq += 1;
        Ok(seq)
    }
}

/// Driver-side events serialized into the per-run JSONL session.
///
/// The supervisor folds these into [`SessionEnvelope`] (kind = [`SessionEventKind::DriverEvent`])
/// without altering the payload shape, so the JSONL stream is the
/// authoritative source for replay, recovery, and UI rendering.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum DriverEvent {
    /// Driver finished its startup handshake. Supervisor unblocks acquire.
    Ready {
        protocol_version: String,
        capabilities: Value,
    },
    /// Free-form text chunk (assistant reply, stdout, stderr).
    TextChunk {
        stream: TextStream,
        chunk: String,
        seq: u64,
    },
    /// Worker invoked a tool/transition. `name` matches a [`WorkerTool`]
    /// variant for implementer runs and a [`BabysitterTool`] variant for
    /// babysitter runs.
    ToolCall {
        call_id: String,
        name: String,
        args: Value,
        seq: u64,
    },
    /// Result of a previous [`DriverEvent::ToolCall`].
    ToolResult {
        call_id: String,
        ok: bool,
        output: Value,
        seq: u64,
    },
    /// Worker explicitly transitioned the task state machine.
    TransitionState {
        from: String,
        to: String,
        reason: String,
    },
    /// Worker reported run completion. Supervisor moves the lease to
    /// `Released`.
    RunComplete { summary: Option<String> },
    /// Worker failed. Supervisor records the error and releases the lease.
    RunFail {
        error_code: String,
        error_markdown: String,
    },
    /// Driver-internal error (process died, transport broke, etc.).
    DriverError { fatal: bool, message: String },
    /// Protocol signal that one agent/model turn finished at a native harness
    /// boundary (for example codex `turn/completed`, ACP `session/prompt`
    /// stop, or a synthesized single-turn subprocess completion).
    ///
    /// Carries no content. The supervisor counts only this variant toward
    /// `max_iterations`; substantive events within the same turn (text chunks,
    /// tool calls, heartbeats) do not advance the iteration counter.
    AgentTurnComplete { seq: u64 },
    /// Lightweight liveness signal emitted while a session is active but the
    /// underlying harness has produced no substantive output for a while (for
    /// example codex `app-server` buffering a long `cargo test` subprocess).
    ///
    /// Carries no content. Its sole purpose is to reset the supervisor's
    /// stall detector (`last_driver_event_at`) so an actively-working run that
    /// happens to be quiet is not mistaken for a stall. It is distinguished by
    /// its `type` (`heartbeat`) so substantive views (evidence distillation,
    /// babysitter summaries, UI transcripts) filter it out.
    Heartbeat { seq: u64 },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TextStream {
    Stdout,
    Stderr,
    Assistant,
    User,
    System,
}

/// Lifecycle envelope (`kind = SessionEventKind::Lifecycle`).
///
/// Supervisor-side events the driver itself does not emit. Kept in
/// `orgasmic-core` so the on-disk JSONL is self-describing for boot
/// reconciliation in `arch_010`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "phase", rename_all = "snake_case")]
pub enum Lifecycle {
    Acquire {
        task_id: String,
        kind: String,
        worker_id: String,
    },
    /// Reattach metadata, written immediately after `Acquire` (boot
    /// auto-reattach). Carries enough to reconstruct the supervisor `reattach`
    /// call after a daemon restart: the driver transport/harness, project +
    /// worktree, and the exact `driver_config`. Kept a separate variant so
    /// pre-existing session JSONL (written before this event existed) still
    /// reconciles, and so the `Acquire` schema is untouched.
    RunMeta {
        transport: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        harness: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        project_id: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        worktree: Option<PathBuf>,
        /// Dispatch artifact paths (CLI-derived from the brief filename stem),
        /// so a boot reattach can reconstruct a `DispatchCompletion` and
        /// respawn the completion watcher that died with the old daemon
        /// process. `None` for non-dispatch runs (manager, recovery, stage
        /// launches) and for pre-upgrade session JSONL.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        last_path: Option<PathBuf>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        stdout_path: Option<PathBuf>,
        /// Full UUID attempt token for CLI dispatch cleanup fencing (TASK-ZGT1X).
        #[serde(default, skip_serializing_if = "Option::is_none")]
        dispatch_attempt_token: Option<String>,
        /// Worker role at acquire time (including `terminal` for custom bare
        /// terminals). Boot reattach restores this instead of inferring from
        /// `worker_id` alone (TASK-99W9C / dec_WDR5K item 6).
        #[serde(default, skip_serializing_if = "Option::is_none")]
        role: Option<String>,
        /// Whether this run advertised the universal finalize contract when
        /// acquired. Boot reattach restores this instead of recomputing from
        /// artifact paths alone (TASK-99W9C).
        #[serde(default, skip_serializing_if = "Option::is_none")]
        requires_worker_finalize: Option<bool>,
        driver_config: Value,
    },
    Attach,
    Release {
        reason: String,
        outcome: ReleaseOutcome,
        /// Set when the worker declared completion itself via
        /// `orgasmic dispatch finalize` (dec_3M7M0) before this release, over
        /// the same daemon channel used for every other write. The dispatch
        /// completion watcher treats this as authoritative and skips its
        /// scrollback-scrape fallback entirely. `#[serde(default)]` keeps
        /// pre-existing session JSONL (written before this field existed)
        /// parseable.
        #[serde(default)]
        finalized_by_worker: bool,
    },
    /// Historical auto-continuation envelope. No production path emits this
    /// after TASK-QPKCD; kept so older session JSONL still deserializes.
    Continuation {
        previous_run: String,
        previous_session_path: PathBuf,
        diff_summary: String,
        acceptance_criteria: Vec<String>,
    },
    BabysitterSpawned {
        target_run: String,
        babysitter_run: String,
    },
    /// A still-live runtime from a prior daemon boot was rehydrated into the
    /// current supervisor. The original `run_id`/`runtime_id` are preserved
    /// (carried by the envelope); this event records the *new* boot that
    /// reattached so the JSONL stays a complete per-run history.
    Reattach {
        reattached_boot: String,
        transport: String,
    },
    /// Harness-aware native runtime identity, captured at launch (or resume)
    /// time. For Claude, `session_id`/`session_path` are deterministic and
    /// `resume_argv` is the exact `claude --resume <id> --fork-session`
    /// command. Other harnesses populate only `launch_argv` until their
    /// native session semantics are known.
    NativeRuntime {
        provider: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        session_id: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        session_path: Option<PathBuf>,
        launch_argv: Vec<String>,
        #[serde(default)]
        resume_argv: Vec<String>,
    },
    /// Typed link from a replacement recovery run back to its Failed origin.
    /// Written into the replacement session after acquire succeeds so daemon
    /// session truth can verify committed recovery claims.
    RecoveryOrigin {
        project_id: String,
        origin_run_id: String,
        origin_session_path: PathBuf,
        request_id: String,
        replacement_run_id: String,
        replacement_session_path: PathBuf,
        action: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        target: Option<String>,
        /// Complete immutable recovery claim snapshot. The daemon writes this
        /// only after the replacement exists and uses it to reconstruct a
        /// deleted claim without letting path-selected JSONL self-authenticate.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        claim: Option<Value>,
    },
    /// A recovery prompt staged for the operator. `sent = false` means the
    /// draft is pending an explicit composer send.
    PromptDraft {
        text: String,
        sent: bool,
    },
    /// A durable record of an operator composer send into a run.
    ComposerSend {
        text: String,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReleaseOutcome {
    Completed,
    Failed,
    Interrupted,
    Cancelled,
}

/// Worker-callable tools on implementer runs (arch_004).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WorkerTool {
    TransitionState,
}

impl WorkerTool {
    pub fn as_str(self) -> &'static str {
        match self {
            WorkerTool::TransitionState => "transition_state",
        }
    }

    pub fn parse(s: &str) -> Option<WorkerTool> {
        match s {
            "transition_state" => Some(WorkerTool::TransitionState),
            _ => None,
        }
    }
}

/// Babysitter tool set per arch_004. Babysitters cannot edit code or invoke
/// arbitrary CLI commands; only these four actions are permitted.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BabysitterTool {
    Poke,
    Restart,
    Escalate,
    RecordFinding,
}

impl BabysitterTool {
    pub fn as_str(self) -> &'static str {
        match self {
            BabysitterTool::Poke => "poke_implementer",
            BabysitterTool::Restart => "restart_implementer",
            BabysitterTool::Escalate => "escalate_to_human",
            BabysitterTool::RecordFinding => "record_finding",
        }
    }

    pub fn parse(s: &str) -> Option<BabysitterTool> {
        match s {
            "poke" | "poke_implementer" => Some(BabysitterTool::Poke),
            "restart" | "restart_implementer" => Some(BabysitterTool::Restart),
            "escalate" | "escalate_to_human" => Some(BabysitterTool::Escalate),
            "record_finding" => Some(BabysitterTool::RecordFinding),
            _ => None,
        }
    }

    pub const ALL: [BabysitterTool; 4] = [
        BabysitterTool::Poke,
        BabysitterTool::Restart,
        BabysitterTool::Escalate,
        BabysitterTool::RecordFinding,
    ];
}

/// A summarized implementer event chunk fed to the babysitter (arch_004).
///
/// The supervisor coalesces driver events from the implementer's session into
/// a coarse summary before handing them to the babysitter, so the babysitter
/// reasons about stall/escalation signals instead of raw byte streams.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct BabysitterSummaryChunk {
    pub window_start_seq: u64,
    pub window_end_seq: u64,
    pub event_count: usize,
    /// Highest-level event observed in the window. `RunFail` outranks
    /// `RunComplete` outranks `ToolCall`, etc.; see implementation in
    /// `orgasmic-daemon::supervisor`.
    pub headline: String,
    /// Last assistant text, truncated. Empty if no text in window.
    pub last_text: String,
    /// Tool call names observed.
    pub tool_calls: Vec<String>,
}

/// Read every envelope from a JSONL file. Skips empty lines but returns an
/// error on the first malformed line. Used by boot reconciliation and tests.
pub fn read_session_file(path: impl AsRef<Path>) -> Result<Vec<SessionEnvelope>, SessionError> {
    let contents = std::fs::read_to_string(path)?;
    let mut out = Vec::new();
    for line in contents.lines() {
        if line.trim().is_empty() {
            continue;
        }
        out.push(serde_json::from_str(line)?);
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn writer_appends_and_reads_back() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("run-abc.jsonl");
        let id = RuntimeIdentity::new("run-abc", "boot-1");
        let mut writer = SessionWriter::open(&path, id).unwrap();
        writer
            .append(SessionEventKind::Lifecycle, json!({"type": "acquire"}))
            .unwrap();
        writer
            .append(
                SessionEventKind::DriverEvent,
                json!({"tool": "edit", "ok": true}),
            )
            .unwrap();
        drop(writer);

        let env = read_session_file(&path).unwrap();
        assert_eq!(env.len(), 2);
        assert_eq!(env[0].seq, 0);
        assert_eq!(env[1].seq, 1);
        assert_eq!(env[0].kind, SessionEventKind::Lifecycle);
    }

    #[test]
    fn driver_event_round_trips_through_envelope() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("run-de.jsonl");
        let id = RuntimeIdentity::new("run-de", "boot-1");
        let mut writer = SessionWriter::open(&path, id).unwrap();
        let evt = DriverEvent::ToolCall {
            call_id: "c1".into(),
            name: WorkerTool::TransitionState.as_str().into(),
            args: json!({"to": "in_progress"}),
            seq: 0,
        };
        writer
            .append(
                SessionEventKind::DriverEvent,
                serde_json::to_value(&evt).unwrap(),
            )
            .unwrap();
        drop(writer);
        let env = read_session_file(&path).unwrap();
        let parsed: DriverEvent = serde_json::from_value(env[0].event.clone()).unwrap();
        assert_eq!(parsed, evt);
    }

    #[test]
    fn babysitter_tool_set_is_closed() {
        for t in BabysitterTool::ALL {
            assert_eq!(BabysitterTool::parse(t.as_str()), Some(t));
        }
        assert_eq!(
            BabysitterTool::parse("poke_implementer"),
            Some(BabysitterTool::Poke)
        );
        assert_eq!(
            BabysitterTool::parse("restart_implementer"),
            Some(BabysitterTool::Restart)
        );
        assert_eq!(
            BabysitterTool::parse("escalate_to_human"),
            Some(BabysitterTool::Escalate)
        );
        assert!(BabysitterTool::parse("edit_file").is_none());
        assert!(BabysitterTool::parse("shell").is_none());
    }

    #[test]
    fn lifecycle_round_trip() {
        let lc = Lifecycle::Acquire {
            task_id: "TASK-006".into(),
            kind: "implementer".into(),
            worker_id: "implementer-claude-acp".into(),
        };
        let v = serde_json::to_value(&lc).unwrap();
        let back: Lifecycle = serde_json::from_value(v).unwrap();
        assert_eq!(lc, back);
    }

    #[test]
    fn run_sub_state_validates_namespace_and_verb() {
        assert_eq!(
            RunSubState::new("implementer.working").unwrap().as_str(),
            "implementer.working"
        );
        assert_eq!(
            RunSubState::new("weird.thing").unwrap().as_str(),
            "weird.thing"
        );
        for invalid in [
            "Agent.working",
            "agent-x.working",
            ".working",
            "agent.",
            "agent.UPPER",
        ] {
            assert!(RunSubState::new(invalid).is_err(), "{invalid}");
        }
    }

    #[test]
    fn appends_preserve_prior_lines() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("run-xyz.jsonl");
        {
            let mut w =
                SessionWriter::open(&path, RuntimeIdentity::new("run-xyz", "boot-1")).unwrap();
            w.append(SessionEventKind::Note, json!({"msg": "one"}))
                .unwrap();
        }
        {
            let mut w =
                SessionWriter::open(&path, RuntimeIdentity::new("run-xyz", "boot-1")).unwrap();
            w.append(SessionEventKind::Note, json!({"msg": "two"}))
                .unwrap();
        }
        let env = read_session_file(&path).unwrap();
        assert_eq!(env.len(), 2);
        assert_eq!(env[0].event["msg"], "one");
        assert_eq!(env[1].event["msg"], "two");
    }
}
