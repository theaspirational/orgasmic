// orgasmic:arch_A53QX, dec_ASB1A
//! `WorkerDriver` trait — one typed execution surface for every runtime kind
//! orgasmic supports (claude-stream-json, codex-appserver, hermes, tmux-tui).
//!
//! Adapted from HAR's `src/drivers/`. Differences from HAR:
//!
//! - One trait covers acquire, attach, release, event stream, and transition.
//!   There is no separate `WorkerHandle` indirection
//!   because the supervisor owns the lease bookkeeping (see
//!   `orgasmic-daemon::supervisor`).
//! - Driver events are emitted as [`orgasmic_core::DriverEvent`] values so
//!   they land in the per-run JSONL session unchanged.

use std::collections::BTreeMap;
use std::path::PathBuf;

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use thiserror::Error;
use tokio::sync::mpsc;

use orgasmic_core::{DriverEvent, RuntimeIdentity, SandboxAllowlist, TextStream, WorkerTool};

use crate::catalog::TransportInteraction;
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
    /// the underlying runtime decides its own cwd.
    pub worktree: Option<PathBuf>,
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
    Ws {
        endpoint: String,
        headers: BTreeMap<String, String>,
        protocol: WsProtocol,
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
pub enum WsProtocol {
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

/// Subprocess invocation template for the stdio mode pairing.
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

    /// Harness-specific readiness probe backing [`WorkerDriver::preflight`].
    ///
    /// Readiness is a property of the harness and its credentials, not of the
    /// transport carrying them, so the mode drivers all delegate here rather
    /// than each reimplementing the same question. The rules that make a probe
    /// worth trusting — and its cost budget — are documented on
    /// [`WorkerDriver::preflight`]; read them before implementing this.
    ///
    /// An adapter that had to *observe* something to reach its verdict must
    /// return that observation in [`PreflightOutcome::plan`] rather than let the
    /// launch observe again — see the type's own docs for why.
    async fn preflight(
        &mut self,
        _ctx: &DriverContext,
        _config: &DriverConfig,
    ) -> PreflightOutcome {
        PreflightOutcome::default()
    }

    /// Base subprocess invocation for stdio adapters that ask the mode to
    /// construct or upgrade their request. Adapters that return a complete
    /// [`HarnessRequest::Subprocess`] from [`Self::compose_request`] do not
    /// need to duplicate that invocation here.
    fn stdio_spawn(&self) -> Option<StdioSpawn> {
        None
    }

    /// Native session identity for the run this adapter last composed, read by
    /// the mode when it builds the [`DriverSession`].
    ///
    /// Adapters that pin a harness-native session id at launch report it here
    /// so recovery can resume/fork the run and retro can locate its vendor
    /// transcript deterministically (dec_Y5MPK). `None` means this adapter
    /// establishes no native identity, and such runs have no resumable
    /// transcript — an accepted gap only where the harness offers none.
    fn native_runtime(&self) -> Option<NativeRuntimeMeta> {
        None
    }

    /// Returns true when this adapter wants the stdio mode to upgrade a
    /// [`HarnessRequest::Simulated`] to a real detached subprocess via
    /// [`Self::stdio_spawn`]. Default false preserves the Simulated
    /// short-circuit for adapters that emit Ready/run-complete events directly
    /// without a real subprocess.
    fn upgrades_simulated_to_subprocess(&self) -> bool {
        false
    }

    /// JSON-RPC session bootstrap for stdio when the harness speaks
    /// request/response over newline-delimited stdin/stdout (for example
    /// `codex app-server`). Default: unsupported — stdio uses plain
    /// subprocess stream-json for that adapter instead.
    fn stdio_session_init(
        &mut self,
        _ctx: &DriverContext,
        _config: &DriverConfig,
    ) -> Result<Value, DriverError> {
        Err(DriverError::Unsupported("stdio_session_init"))
    }

    /// When stdio upgrades a simulated acquire to a real subprocess, the
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

    /// JSON-RPC session-start method for session-oriented JSON-RPC runtimes.
    /// Codex app-server uses `thread/start`; real ACP agents use `session/new`.
    /// These are different protocols that happen to share a hook, not two
    /// dialects of one (TASK-XCJYC).
    fn jsonrpc_session_start_method(&self) -> &'static str {
        "thread/start"
    }

    /// JSON-RPC turn-start method for session-oriented JSON-RPC runtimes. Codex
    /// app-server uses `turn/start`; real ACP agents use `session/prompt`.
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

/// Run a mode driver's preflight against the harness adapter it carries.
///
/// Every mode holds a `Box<dyn HarnessEventAdapter>` and every mode's answer to
/// "could a worker start?" is the adapter's answer, so this exists once instead
/// of five times. The probe gets a fresh clone for the same reason `acquire`
/// does: the shared adapter is a template, and a probe must not leave state on
/// it that a later launch would inherit.
pub(crate) async fn preflight_via_adapter(
    adapter: &dyn HarnessEventAdapter,
    ctx: &DriverContext,
    config: &DriverConfig,
) -> PreflightOutcome {
    adapter.clone_box().preflight(ctx, config).await
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

    /// Whether a dispatch on this transport runs with nobody attached, or
    /// spawns an interactive terminal pane. It is the first thing a manager
    /// choosing a transport needs to know, and only the driver can answer it.
    ///
    /// The default is [`TransportInteraction::Undeclared`], never
    /// `Unattended`: a driver that never declared how it runs must not pass as
    /// safe to dispatch unattended. `catalog::tests` fails while any supported
    /// pair still answers `Undeclared`.
    fn interaction(&self) -> TransportInteraction {
        TransportInteraction::Undeclared
    }

    /// Validate configuration before any acquire. Returning `Err` here is
    /// the right place to surface a missing binary, an invalid command, or
    /// a malformed transport URL.
    fn validate(&self, _config: &DriverConfig) -> Result<(), DriverError> {
        Ok(())
    }

    // orgasmic:TASK-XC9N4
    /// Would `acquire` with this exact configuration run the in-memory stub —
    /// no process, no connection — instead of a harness? Composes the request
    /// on a fresh adapter clone and launches nothing. `false` by default: a
    /// mode that never simulates has nothing to declare.
    fn simulates(&self, _ctx: &DriverContext, _config: &DriverConfig) -> bool {
        false
    }

    /// Ask the harness, non-interactively and within a bounded time, whether a
    /// worker launched with this exact configuration could actually start.
    ///
    /// Called after configuration is resolved and before any lease, session, or
    /// dispatch record exists, so a definitive failure costs nothing to undo.
    ///
    /// Three rules make this worth doing at all:
    ///
    /// - The default is [`Preflight::Unsupported`], never a cheerful `Ok`. A
    ///   driver that has not implemented a probe must not be able to claim
    ///   readiness it never checked.
    /// - **An implementation must resolve the same credential mode the worker
    ///   will resolve, and then check the credential that mode actually
    ///   consumes.** This rule replaces an earlier, blunter one ("always
    ///   exercise the worker's exact argv"), which was written from a real
    ///   observation before its cause was understood. The observation:
    ///   `claude auth status` reported `loggedIn: true` while the same binary
    ///   invoked with the dispatch's own flags answered "Not logged in" in
    ///   39 ms. The cause, measured 2026-07-25: `auth status` reports only the
    ///   claude.ai/keychain login and is blind to `ANTHROPIC_API_KEY`, while
    ///   the dispatch's argv carried `--bare`, whose contract is that OAuth and
    ///   the keychain are never read. The two were answering about different
    ///   credentials, so they could disagree without either being wrong. Once
    ///   the probe resolves the mode first, `auth status` is exactly the right
    ///   question for native login and exactly the wrong one for `--bare`.
    /// - A probe runs on every dispatch, so its price is part of its design.
    ///   Measured on claude 2.1.220: submitting a real turn — the only way to
    ///   get the harness itself to rule on the credential — cost $0.0994 and
    ///   24.5k tokens for a one-character prompt, because the request writes
    ///   the harness's system prompt, tools and skills to cache before any
    ///   answer comes back. Cancelling early does not refund it: the failure
    ///   verdict and the outbound request are simultaneous (0.326 s vs
    ///   0.390 s). A check that silently bills a tenth of a dollar per dispatch
    ///   is not a check worth having, so prefer a local, zero-cost interrogation
    ///   of the resolved credential and return [`Preflight::Unsupported`] where
    ///   only a billed turn could reach a verdict.
    ///
    /// It must never prompt. A harness answering "run /login" is a definitive
    /// failure to report, not an interactive flow to enter: nobody is attached
    /// to a dispatched worker to answer it. Give any child process a null
    /// stdin so an interactive fallback cannot block instead of answering.
    ///
    /// A fourth rule follows from the first: **whatever the probe resolved to
    /// reach its verdict must come back in [`PreflightOutcome::plan`]**, so the
    /// launch applies that decision rather than making its own. A probe that
    /// admits a dispatch on an observation it then discards has ruled on a run
    /// that no longer exists by the time anything is spawned (TASK-KKBTP).
    async fn preflight(&self, _ctx: &DriverContext, _config: &DriverConfig) -> PreflightOutcome {
        PreflightOutcome::default()
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
    /// How this run authenticated, as a bare mode string (`bare_api_key` /
    /// `native_login`) and never any credential material. `None` for harnesses
    /// that do not resolve a credential mode.
    ///
    /// It travels here because this is the only per-run channel an adapter has
    /// to the supervisor, but its durable home is `Lifecycle::RunMeta`: the
    /// mode is reattach material, and a boot that rehydrates a run reads
    /// RunMeta, not the NativeRuntime event (TASK-S0QRM).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub credential_mode: Option<String>,
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
    /// Supervisor-side handle for transitions and release. Implementations
    /// are usually a thin wrapper around the
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
    /// Ask the worker to transition the task state machine.
    async fn transition_state(
        &mut self,
        req: TransitionRequest,
    ) -> Result<TransitionAck, DriverError>;

    /// Send user-authored input into a live interactive runtime.
    async fn send_input(&mut self, _req: UserInputRequest) -> Result<UserInputAck, DriverError> {
        Err(DriverError::Unsupported("send_input"))
    }

    /// Deliver an automated manager wake into a *claimed* provider pane.
    ///
    /// This is intentionally distinct from [`Self::send_input`]: the latter is
    /// the existing human-composer path, while this operation must prove the
    /// claimed provider is still the foreground process immediately before it
    /// pastes.  A bare terminal prompt is never sufficient proof.
    async fn send_manager_wake(
        &mut self,
        _req: ManagerWakeRequest,
    ) -> Result<UserInputAck, DriverError> {
        Err(DriverError::Unsupported("manager_wake"))
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
pub struct UserInputRequest {
    pub input: String,
}

/// The only bytes an automated manager wake may put in a pane.
///
/// This is intentionally a shell no-op as well as an entry-router resume
/// marker. A provider can consume it as a resume request; a provider exit in
/// the final tmux gap can only leave the shell executing `:` with one literal
/// argument. Never add user, task, or project content to this value.
pub const MANAGER_WAKE_MARKER: &str = ": 'ORGASMIC_MANAGER_WAKE_V1'";

/// A daemon-originated manager wake.
///
/// The payload is always [`MANAGER_WAKE_MARKER`] and is deliberately absent
/// from this request so no caller can alter the injected bytes. The tmux driver
/// discovers the actual foreground provider at delivery time; it is not a
/// caller-selected claim property.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ManagerWakeRequest {}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UserInputAck {
    pub accepted: bool,
    pub message: Option<String>,
}

/// Whether a worker launched with a given configuration could start.
///
/// Deliberately three-valued. Collapsing `Unsupported` into `Ready` would let
/// every driver that never implemented a probe report readiness it did not
/// check; collapsing it into `Fatal` would refuse every dispatch on drivers that
/// work fine. Callers must treat "we did not look" as its own answer.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub enum Preflight {
    /// The harness answered, using the worker's own execution context, that a
    /// worker could start.
    Ready,
    /// The harness cannot start a worker with this configuration, and trying
    /// would waste the resources the dispatch is about to create. `reason` is
    /// operator-facing and must name the remedy where one exists.
    Fatal { reason: String },
    /// This driver has no probe, or the probe could not reach a verdict (a
    /// timeout, a harness that does not answer). Dispatch proceeds as it did
    /// before preflight existed — the pre-existing failure modes still apply.
    #[default]
    Unsupported,
}

impl Preflight {
    /// Build a definitive rejection.
    pub fn fatal(reason: impl Into<String>) -> Self {
        Self::Fatal {
            reason: reason.into(),
        }
    }

    /// Only a definitive `Fatal` may reject a dispatch. An inconclusive probe
    /// must not.
    pub fn rejects_dispatch(&self) -> Option<&str> {
        match self {
            Self::Fatal { reason } => Some(reason.as_str()),
            Self::Ready | Self::Unsupported => None,
        }
    }
}

/// A probe's verdict together with the launch facts it had to resolve to reach
/// it.
///
/// The facts are the point. A probe runs *before* the dispatch owns a lease, a
/// session or a worktree; the launch runs after. If the probe observes a
/// credential, admits the dispatch on that observation and then throws it away,
/// the launch has no choice but to observe again — and a second observation of a
/// live harness can disagree with the first. Measured on the claude adapter
/// (TASK-KKBTP): `Present` at the probe and a timed-out `Unknown` at composition
/// resolved to two different credential modes, so a run launched with a
/// credential nothing had ruled on, after ownership was committed. Recording the
/// choice afterwards does not fix that; only pinning it beforehand does.
///
/// So a probe hands back a *plan*: a driver-config fragment the daemon merges
/// into the config it then gives [`WorkerDriver::acquire`], carrying the
/// decisions the launch must not re-derive. Two rules hold it honest:
///
/// - **Non-secret only.** The plan reaches durable run metadata. It may name the
///   environment variable a key comes from; it must never carry the key.
/// - **Decisions, not observations to redo.** A plan says "bare mode, key from
///   `ANTHROPIC_API_KEY`, neutralise nothing" — facts a launch can apply without
///   asking the harness anything.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PreflightOutcome {
    /// Whether the dispatch may proceed.
    pub verdict: Preflight,
    /// Driver-config fragment pinning what the probe resolved, or `None` when
    /// the probe resolved nothing a launch would otherwise re-derive.
    pub plan: Option<Value>,
}

impl PreflightOutcome {
    /// A verdict that pins nothing — the right answer for a probe that only
    /// classified, and for every driver that has no probe at all.
    pub fn verdict(verdict: Preflight) -> Self {
        Self {
            verdict,
            plan: None,
        }
    }

    /// Attach the launch facts this verdict was reached on.
    pub fn with_plan(mut self, plan: Value) -> Self {
        self.plan = Some(plan);
        self
    }

    /// Only a definitive `Fatal` may reject a dispatch (see [`Preflight`]).
    pub fn rejects_dispatch(&self) -> Option<&str> {
        self.verdict.rejects_dispatch()
    }

    /// Merge the pinned plan into the config the launch will receive.
    ///
    /// The daemon calls this between admitting the dispatch and acquiring, so
    /// `acquire` and every composition below it read the plan instead of
    /// re-observing. Merging into the driver config rather than into a
    /// side-channel is deliberate: the config is the one value that already
    /// travels the whole way — through `AcquireRequest`, through the mode
    /// driver's delegation, and into the persisted `RunMeta`, where the pinned
    /// plan becomes the diagnosable record of what the run was launched with.
    ///
    /// A plan overwrites any same-named key already present, so a stale or
    /// operator-supplied fragment cannot outrank the probe's own answer.
    pub fn pin_into(&self, config: &DriverConfig) -> DriverConfig {
        let (Some(Value::Object(plan)), Value::Object(base)) = (self.plan.as_ref(), &config.0)
        else {
            return config.clone();
        };
        let mut merged = base.clone();
        for (key, value) in plan {
            merged.insert(key.clone(), value.clone());
        }
        DriverConfig(Value::Object(merged))
    }
}

impl From<Preflight> for PreflightOutcome {
    fn from(verdict: Preflight) -> Self {
        Self::verdict(verdict)
    }
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
    #[error("manager wake provider does not match the claimed provider")]
    ManagerWakeProviderMismatch,
    #[error("manager wake pane is unavailable")]
    ManagerWakeUnavailable,
    #[error("worker tool '{0}' is not callable on this run kind")]
    WorkerToolBlocked(String),
    #[error("driver i/o: {0}")]
    Io(#[from] std::io::Error),
    #[error("driver input field not ready within {0:?}")]
    InputNotReady(std::time::Duration),
    #[error("driver: {0}")]
    Other(String),
}

/// True if `name` is callable on an implementer run.
pub fn implementer_tool_is_allowed(name: &str) -> bool {
    WorkerTool::parse(name).is_some()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::process::Command;

    #[test]
    fn manager_wake_marker_is_exact_and_a_real_bash_zsh_noop() {
        assert_eq!(
            MANAGER_WAKE_MARKER.as_bytes(),
            b": 'ORGASMIC_MANAGER_WAKE_V1'"
        );
        for shell in ["/bin/bash", "/bin/zsh"] {
            let output = Command::new(shell)
                .args(["-c", "set -e; : 'ORGASMIC_MANAGER_WAKE_V1'; printf '%s' ok"])
                .output()
                .unwrap_or_else(|error| panic!("run {shell}: {error}"));
            assert!(
                output.status.success(),
                "{shell} rejected fixed wake marker"
            );
            assert_eq!(
                output.stdout, b"ok",
                "{shell} marker changed shell behavior"
            );
        }
    }

    #[test]
    fn implementer_tools_are_closed() {
        assert!(implementer_tool_is_allowed("transition_state"));
        assert!(!implementer_tool_is_allowed("delete_repo"));
    }

    #[test]
    fn run_kind_round_trips() {
        let kind = RunKind::Worker;
        let json = serde_json::to_string(&kind).unwrap();
        let round_tripped: RunKind = serde_json::from_str(&json).unwrap();
        assert_eq!(round_tripped, kind);
    }
}
