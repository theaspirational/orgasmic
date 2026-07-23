// arch: arch_A53QX.2
// orgasmic:arch_A53QX, dec_ASB1A
//! `WorkerDriver` trait — one typed execution surface for every runtime kind
//! orgasmic supports (claude-acp, codex-appserver, hermes, tmux-tui).
//!
//! Adapted from HAR's `src/drivers/`. Differences from HAR:
//!
//! - One trait covers acquire, attach, release, event stream, transition,
//!   and babysitter actions. There is no separate `WorkerHandle` indirection
//!   because the supervisor owns the lease bookkeeping (see
//!   `orgasmic-daemon::supervisor`).
//! - Driver events are emitted as [`orgasmic_core::DriverEvent`] values so
//!   they land in the per-run JSONL session unchanged.
//! - Babysitter runs reuse the same trait, but the spawning supervisor sets
//!   `kind = RunKind::Babysitter`, and drivers honor the restricted tool set
//!   from [`orgasmic_core::BabysitterTool`].

use std::collections::BTreeMap;
use std::path::PathBuf;

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use thiserror::Error;
use tokio::sync::mpsc;

use orgasmic_core::{
    BabysitterTool, DriverEvent, RuntimeIdentity, SandboxAllowlist, TextStream, WorkerTool,
};

use crate::runtime_options::{
    RuntimeOptionsAck, RuntimeOptionsCatalog, RuntimeOptionsCatalogRpc, RuntimeOptionsRequest,
};

/// Protocol signal for one completed agent/model turn.
pub fn agent_turn_complete(seq: u64) -> DriverEvent {
    DriverEvent::AgentTurnComplete { seq }
}

/// Prepend a turn boundary before a terminal run event.
pub fn turn_boundary_events(seq: u64, terminal: DriverEvent) -> Vec<DriverEvent> {
    vec![agent_turn_complete(seq), terminal]
}

/// What a driver instance is running.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RunKind {
    /// A normal worker run performing the task. The *role* (implementer,
    /// reviewer, …) is the resolved worker's kind, not part of RunKind.
    /// `alias` keeps pre-rename persisted sessions deserializable.
    #[serde(alias = "implementer")]
    Worker,
    /// A babysitter watching another run. Restricted tool set; cannot edit
    /// code or invoke arbitrary CLI commands (`arch_004`).
    Babysitter,
}

/// What spawning the driver needs from the supervisor.
#[derive(Debug, Clone)]
pub struct DriverContext {
    pub identity: RuntimeIdentity,
    pub run_kind: RunKind,
    pub task_id: String,
    pub worker_id: String,
    pub project_id: Option<String>,
    /// Worktree the driver should operate in. Drivers MAY ignore this when
    /// the underlying runtime decides its own cwd (e.g. babysitters that
    /// only call back into the daemon).
    pub worktree: Option<PathBuf>,
    /// Babysitters only: the run id they are observing. `None` for
    /// implementer runs.
    pub babysitter_target: Option<String>,
}

/// Per-driver configuration. Each driver decides its own shape; the
/// supervisor passes the raw JSON Value through so we don't have to grow
/// the trait when a new driver lands.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct DriverConfig(pub Value);

impl DriverConfig {
    pub fn empty() -> Self {
        Self(Value::Object(Default::default()))
    }
    pub fn from_value(v: Value) -> Self {
        Self(v)
    }
}

/// Mode-specific request shape composed by a harness adapter.
#[derive(Debug, Clone)]
pub enum HarnessRequest {
    /// No external process/connection. The mode returns these events and
    /// exposes an in-memory control surface.
    Simulated { events: Vec<DriverEvent> },
    /// Subprocess JSONL/stdout mode. `stdin_payload` is written once after
    /// spawn; `close_stdin` controls whether later control writes are allowed.
    Subprocess {
        binary: String,
        args: Vec<String>,
        env: BTreeMap<String, String>,
        cwd: Option<PathBuf>,
        stdin_payload: Option<Vec<u8>>,
        close_stdin: bool,
    },
    /// WebSocket mode. `session_init` is interpreted by the selected wire
    /// protocol, while all harness event mapping stays in the adapter.
    AcpWs {
        endpoint: String,
        headers: BTreeMap<String, String>,
        protocol: AcpWsProtocol,
        session_init: Value,
    },
    /// Tmux-pane mode.
    Tmux {
        binary: String,
        args: Vec<String>,
        env: BTreeMap<String, String>,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AcpWsProtocol {
    JsonRpc,
    RawJson,
}

#[derive(Debug, Clone)]
pub enum WireMessage {
    Json(Value),
    JsonRpc { method: String, params: Value },
}

#[derive(Debug, Clone, Default)]
pub struct HarnessControlOutcome {
    pub events: Vec<DriverEvent>,
    pub stdin_payloads: Vec<Vec<u8>>,
    pub wire_messages: Vec<WireMessage>,
    pub close: bool,
}

impl HarnessControlOutcome {
    pub fn event(event: DriverEvent) -> Self {
        Self {
            events: vec![event],
            ..Self::default()
        }
    }

    pub fn close_with(event: DriverEvent) -> Self {
        Self {
            events: vec![event],
            close: true,
            ..Self::default()
        }
    }
}

/// Subprocess invocation template for the acp-stdio mode pairing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StdioSpawn {
    pub command: String,
    pub args: Vec<String>,
    pub cwd: Option<PathBuf>,
    pub env: Vec<(String, String)>,
}

impl StdioSpawn {
    pub fn cwd_is_absolute(&self) -> bool {
        self.cwd
            .as_ref()
            .map(|path| path.is_absolute())
            .unwrap_or(true)
    }
}

/// Harness-specific event and request adapter used by the mode drivers.
#[async_trait]
pub trait HarnessEventAdapter: Send + Sync + 'static {
    /// Stable harness id, e.g. `codex`, `claude`, `cursor-agent`, `hermes`.
    fn harness(&self) -> &'static str;

    /// Clone a fresh per-session adapter. Mode drivers are reusable; adapter
    /// state is per acquired runtime.
    fn clone_box(&self) -> Box<dyn HarnessEventAdapter>;

    /// Translate one raw harness event into zero or more driver events.
    async fn parse_event(&mut self, raw: Value) -> Vec<DriverEvent>;

    /// Handle codex-style sandbox approval server requests before
    /// [`Self::parse_event`]. Default: not an approval method.
    async fn try_handle_approval(
        &mut self,
        _method: &str,
        _params: &Value,
        _allowlist: &SandboxAllowlist,
    ) -> Option<crate::sandbox::ApprovalResponse> {
        None
    }

    /// Translate one stdout JSONL line. Adapters may preserve state across
    /// lines for partial-output or streaming tool-call assembly.
    async fn parse_stdout_line(&mut self, line: &str) -> Vec<DriverEvent> {
        match serde_json::from_str::<Value>(line) {
            Ok(value) => self.parse_event(value).await,
            Err(_) => vec![self.text_event(TextStream::Stdout, line.to_string())],
        }
    }

    /// Compose the initial mode request for this harness.
    fn compose_request(
        &mut self,
        ctx: &DriverContext,
        config: &DriverConfig,
    ) -> Result<HarnessRequest, DriverError>;

    /// Optional harness-specific config validation hook.
    fn validate_config(&self, _config: &DriverConfig) -> Result<(), DriverError> {
        Ok(())
    }

    /// Subprocess invocation for the acp-stdio mode pairing.
    /// Returns `None` when this adapter does not participate in stdio mode.
    fn stdio_spawn(&self) -> Option<StdioSpawn> {
        None
    }

    /// Returns true when this adapter wants the acp-stdio mode to upgrade a
    /// [`HarnessRequest::Simulated`] to a real detached subprocess via
    /// [`Self::stdio_spawn`]. Default false preserves the Simulated
    /// short-circuit for adapters that emit Ready/run-complete events directly
    /// without a real subprocess.
    fn upgrades_simulated_to_subprocess(&self) -> bool {
        false
    }

    /// JSON-RPC session bootstrap for acp-stdio when the harness speaks
    /// request/response over newline-delimited stdin/stdout (for example
    /// `codex app-server`). Default: unsupported — acp-stdio uses plain
    /// subprocess stream-json for that adapter instead.
    fn stdio_session_init(
        &mut self,
        _ctx: &DriverContext,
        _config: &DriverConfig,
    ) -> Result<Value, DriverError> {
        Err(DriverError::Unsupported("stdio_session_init"))
    }

    /// When acp-stdio upgrades a simulated acquire to a real subprocess, the
    /// adapter may supply an initial stdin payload (for example JSON-RPC
    /// handshakes). Default: no initial write.
    fn stdio_initial_payload(
        &mut self,
        _ctx: &DriverContext,
        _config: &DriverConfig,
    ) -> Result<Option<Vec<u8>>, DriverError> {
        Ok(None)
    }

    /// Translate a stderr line from a subprocess mode.
    fn stderr_event(&mut self, line: String) -> DriverEvent {
        self.text_event(TextStream::Stderr, line)
    }

    /// Return true for noisy harness stderr lines that should not become
    /// transcript events.
    fn ignores_stderr_line(&self, _line: &str) -> bool {
        false
    }

    /// Translate plain text from a non-JSON stdout line.
    fn text_event(&mut self, stream: TextStream, chunk: String) -> DriverEvent {
        DriverEvent::TextChunk {
            stream,
            chunk,
            seq: self.next_seq(),
        }
    }

    /// Monotonic sequence for fallback text/control events.
    fn next_seq(&mut self) -> u64 {
        0
    }

    /// WebSocket connection hook. Raw-json protocols usually emit Ready here.
    async fn on_ws_connected(&mut self, _meta: Value) -> Result<Vec<DriverEvent>, DriverError> {
        Ok(Vec::new())
    }

    /// True when initial WebSocket connection failures should be emitted on
    /// the event stream instead of making `acquire()` return an error.
    fn ws_connect_errors_emit_to_stream(&self) -> bool {
        false
    }

    /// JSON-RPC hook after a successful `thread/start` response.
    async fn on_ws_thread_started(
        &mut self,
        _endpoint: &str,
        _thread_response: &Value,
    ) -> Result<Vec<DriverEvent>, DriverError> {
        Ok(Vec::new())
    }

    /// JSON-RPC `turn/start` params after the adapter has captured a thread.
    fn ws_turn_start_params(&mut self) -> Result<Value, DriverError> {
        Err(DriverError::Unsupported("ws_turn_start_params"))
    }

    /// JSON-RPC session-start method for ACP-like runtimes. Codex app-server
    /// uses `thread/start`; standard ACP agents use `session/new`.
    fn jsonrpc_session_start_method(&self) -> &'static str {
        "thread/start"
    }

    /// JSON-RPC turn-start method for ACP-like runtimes. Codex app-server uses
    /// `turn/start`; standard ACP agents use `session/prompt`.
    fn jsonrpc_turn_start_method(&self) -> &'static str {
        "turn/start"
    }

    /// Resolve a post-session JSON-RPC request after `session/new` has made
    /// runtime-provided configuration values available. Most adapters can use
    /// the static request from their session-init envelope unchanged.
    fn jsonrpc_post_session_params(
        &mut self,
        _method: &str,
        params: Value,
    ) -> Result<Value, DriverError> {
        Ok(params)
    }

    /// JSON-RPC response hook for non-handshake responses.
    async fn on_ws_response(
        &mut self,
        _method: &str,
        _response: Value,
    ) -> Result<Vec<DriverEvent>, DriverError> {
        Ok(Vec::new())
    }

    async fn transition_state(
        &mut self,
        req: TransitionRequest,
    ) -> Result<HarnessControlOutcome, DriverError> {
        Ok(HarnessControlOutcome::event(DriverEvent::TransitionState {
            from: req.from,
            to: req.to,
            reason: req.reason,
        }))
    }

    async fn babysitter_action(
        &mut self,
        req: BabysitterRequest,
    ) -> Result<HarnessControlOutcome, DriverError> {
        Ok(HarnessControlOutcome::event(DriverEvent::ToolCall {
            call_id: format!("{}-bs-{}", self.harness(), uuid::Uuid::new_v4()),
            name: req.tool.as_str().into(),
            args: req.payload,
            seq: self.next_seq(),
        }))
    }

    async fn send_input(
        &mut self,
        _req: UserInputRequest,
    ) -> Result<HarnessControlOutcome, DriverError> {
        Err(DriverError::Unsupported("send_input"))
    }

    async fn switch_runtime_options(
        &mut self,
        _req: RuntimeOptionsRequest,
    ) -> Result<HarnessControlOutcome, DriverError> {
        Err(DriverError::Unsupported("switch_runtime_options"))
    }

    /// Optional live catalog request when the transport itself exposes valid
    /// model/effort/speed options.
    fn runtime_options_catalog_rpc(&self) -> Option<RuntimeOptionsCatalogRpc> {
        None
    }

    /// Build a runtime-options catalog locally when no transport RPC is needed.
    async fn runtime_options_catalog(&mut self) -> Result<RuntimeOptionsCatalog, DriverError> {
        Err(DriverError::Unsupported("runtime_options_catalog"))
    }

    /// Convert a live transport response into the common catalog shape.
    async fn runtime_options_catalog_from_response(
        &mut self,
        _response: Value,
    ) -> Result<RuntimeOptionsCatalog, DriverError> {
        Err(DriverError::Unsupported(
            "runtime_options_catalog_from_response",
        ))
    }

    async fn release(&mut self, reason: String) -> Result<HarnessControlOutcome, DriverError> {
        Ok(HarnessControlOutcome {
            events: vec![DriverEvent::RunComplete {
                summary: Some(reason),
            }],
            close: true,
            ..HarnessControlOutcome::default()
        })
    }

    fn terminal_emitted(&self) -> bool {
        false
    }

    /// Emit a terminal run-complete once. Adapters with a `terminal_emitted`
    /// guard should override this so synthesis and natural terminals stay consistent.
    async fn emit_run_complete_once(
        &mut self,
        events: &mpsc::Sender<DriverEvent>,
        summary: Option<String>,
    ) {
        let _ = events.send(DriverEvent::RunComplete { summary }).await;
    }
}

impl Clone for Box<dyn HarnessEventAdapter> {
    fn clone(&self) -> Self {
        self.clone_box()
    }
}

/// A live driver instance the supervisor can talk to.
#[async_trait]
pub trait WorkerDriver: Send + Sync + 'static {
    /// Static transport id used by the supervisor registry. Matches the
    /// worker's `driver` field.
    fn transport(&self) -> &'static str;

    /// Stable harness id when the driver was built from a first-class
    /// `(mode, harness)` pair.
    fn harness(&self) -> Option<&'static str> {
        None
    }

    /// Validate configuration before any acquire. Returning `Err` here is
    /// the right place to surface a missing binary, an invalid command, or
    /// a malformed transport URL.
    fn validate(&self, _config: &DriverConfig) -> Result<(), DriverError> {
        Ok(())
    }

    /// Acquire the runtime and start the event stream. The supervisor
    /// has already created the session JSONL; the driver only emits events.
    async fn acquire(
        &self,
        ctx: DriverContext,
        config: DriverConfig,
    ) -> Result<DriverSession, DriverError>;

    /// Reattach to a runtime that an earlier daemon boot left behind.
    /// Default returns [`AttachOutcome::NotReattachable`] so each driver opts
    /// in explicitly. Implementations MUST verify the runtime is alive before
    /// returning an attached session (`arch_010`).
    async fn attach(
        &self,
        _ctx: DriverContext,
        _config: DriverConfig,
    ) -> Result<AttachOutcome, DriverError> {
        Ok(AttachOutcome::NotReattachable)
    }
}

/// Result of a provider-handle reattach attempt.
pub enum AttachOutcome {
    /// The driver proved the runtime handle still exists and returned a live
    /// session/control surface for it.
    Attached(Attached),
    /// The driver cannot prove a live runtime handle for this identity.
    NotReattachable,
}

/// Successful reattach payload.
pub struct Attached {
    pub session: Box<DriverSession>,
}

/// Harness-aware native runtime identity captured by a driver at launch or
/// resume time. The supervisor folds this into a typed
/// `Lifecycle::NativeRuntime` session event so recovery can later resume the
/// underlying native conversation deterministically.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct NativeRuntimeMeta {
    pub provider: String,
    pub session_id: Option<String>,
    pub session_path: Option<PathBuf>,
    pub launch_argv: Vec<String>,
    /// Exact argv to resume/fork this native session. Empty when the harness
    /// has no known resume semantics yet.
    pub resume_argv: Vec<String>,
}

/// What `acquire`/`attach` hands back to the supervisor.
pub struct DriverSession {
    /// Stable identity for this attempt. The supervisor pins ownership on
    /// `(run_id, runtime_id, boot_id)`.
    pub identity: RuntimeIdentity,
    /// OS pid of the runtime subprocess when this mode owns one. Websocket
    /// and tmux modes may not have a direct child process.
    pub pid: Option<u32>,
    /// Event stream from the driver. The supervisor folds each event into
    /// the per-run JSONL.
    pub events: mpsc::Receiver<DriverEvent>,
    /// Supervisor-side handle for transitions, babysitter actions, and
    /// release. Implementations are usually a thin wrapper around the
    /// driver's command channel.
    pub control: Box<dyn DriverControl>,
    /// Driver-owned event producer. The supervisor retains this handle so a
    /// release that cannot complete gracefully can abort and join the
    /// producer before it drains the receiver to closure.
    pub producer: Option<tokio::task::JoinHandle<()>>,
    /// Harness-aware native runtime identity, when the driver knows it.
    /// `None` for drivers/harnesses without native session semantics.
    pub native_runtime: Option<NativeRuntimeMeta>,
}

#[async_trait]
pub trait DriverControl: Send + Sync {
    /// Ask the worker to transition the task state machine. Only meaningful
    /// for `RunKind::Worker`. Babysitter drivers MAY reject this.
    async fn transition_state(
        &mut self,
        req: TransitionRequest,
    ) -> Result<TransitionAck, DriverError>;

    /// Invoke one of the four babysitter tools. Only meaningful for
    /// `RunKind::Babysitter`. Implementer drivers MUST reject this.
    async fn babysitter_action(
        &mut self,
        req: BabysitterRequest,
    ) -> Result<BabysitterAck, DriverError>;

    /// Send user-authored input into a live interactive runtime.
    async fn send_input(&mut self, _req: UserInputRequest) -> Result<UserInputAck, DriverError> {
        Err(DriverError::Unsupported("send_input"))
    }

    /// Change harness runtime options for subsequent prompts or turns.
    async fn switch_runtime_options(
        &mut self,
        _req: RuntimeOptionsRequest,
    ) -> Result<RuntimeOptionsAck, DriverError> {
        Err(DriverError::Unsupported("switch_runtime_options"))
    }

    /// Return the active run's valid provider/model/effort/speed choices.
    async fn runtime_options_catalog(&mut self) -> Result<RuntimeOptionsCatalog, DriverError> {
        Err(DriverError::Unsupported("runtime_options_catalog"))
    }

    /// Release the runtime. Idempotent: a second release is a no-op.
    async fn release(&mut self, reason: &str) -> Result<(), DriverError>;
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TransitionRequest {
    pub from: String,
    pub to: String,
    pub reason: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TransitionAck {
    pub accepted: bool,
    pub message: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BabysitterRequest {
    pub tool: BabysitterTool,
    pub target_run: String,
    pub payload: Value,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BabysitterAck {
    pub accepted: bool,
    pub message: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UserInputRequest {
    pub input: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UserInputAck {
    pub accepted: bool,
    pub message: Option<String>,
}

#[derive(Debug, Error)]
pub enum DriverError {
    #[error("driver config invalid: {0}")]
    InvalidConfig(String),
    #[error("driver transport unavailable: {0}")]
    Transport(String),
    #[error("runtime is not reattachable")]
    NotReattachable,
    #[error("operation not supported by this driver: {0}")]
    Unsupported(&'static str),
    #[error("babysitter tool '{0}' is not in the allowed set")]
    BabysitterToolBlocked(String),
    #[error("worker tool '{0}' is not callable on this run kind")]
    WorkerToolBlocked(String),
    #[error("driver i/o: {0}")]
    Io(#[from] std::io::Error),
    #[error("driver input field not ready within {0:?}")]
    InputNotReady(std::time::Duration),
    #[error("driver: {0}")]
    Other(String),
}

/// Build a [`BabysitterRequest`] with validation that the tool is in the
/// closed set. Drivers and the supervisor share this so the policy lives in
/// one place.
pub fn build_babysitter_request(
    tool: &str,
    target_run: String,
    payload: Value,
) -> Result<BabysitterRequest, DriverError> {
    let tool = BabysitterTool::parse(tool)
        .ok_or_else(|| DriverError::BabysitterToolBlocked(tool.to_string()))?;
    Ok(BabysitterRequest {
        tool,
        target_run,
        payload,
    })
}

/// True if `name` is callable on an implementer run.
pub fn implementer_tool_is_allowed(name: &str) -> bool {
    WorkerTool::parse(name).is_some()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn babysitter_tool_set_is_closed() {
        assert!(build_babysitter_request("poke", "run-x".into(), Value::Null).is_ok());
        assert!(build_babysitter_request("poke_implementer", "run-x".into(), Value::Null).is_ok());
        assert!(build_babysitter_request("restart", "run-x".into(), Value::Null).is_ok());
        assert!(
            build_babysitter_request("restart_implementer", "run-x".into(), Value::Null).is_ok()
        );
        assert!(build_babysitter_request("escalate", "run-x".into(), Value::Null).is_ok());
        assert!(build_babysitter_request("escalate_to_human", "run-x".into(), Value::Null).is_ok());
        assert!(build_babysitter_request("record_finding", "run-x".into(), Value::Null).is_ok());
        let err = build_babysitter_request("shell", "run-x".into(), Value::Null).unwrap_err();
        assert!(matches!(err, DriverError::BabysitterToolBlocked(_)));
    }

    #[test]
    fn implementer_tools_are_closed() {
        assert!(implementer_tool_is_allowed("transition_state"));
        assert!(!implementer_tool_is_allowed("delete_repo"));
    }

    #[test]
    fn run_kind_round_trips() {
        for k in [RunKind::Worker, RunKind::Babysitter] {
            let j = serde_json::to_string(&k).unwrap();
            let back: RunKind = serde_json::from_str(&j).unwrap();
            assert_eq!(back, k);
        }
    }
}
