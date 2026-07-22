// arch: arch_A53QX.3
// orgasmic:arch_A53QX, dec_ASB1A
//! Codex app-server harness adapter.
//!
//! Connects to a running `codex` app-server over the standard JSON-RPC port.
//! Same simulated-mode escape hatch as the Claude ACP driver: when no
//! endpoint is configured, the driver emits Ready + RunComplete on release,
//! which is enough for supervisor lease / JSONL tests on CI.

use std::path::PathBuf;

use async_trait::async_trait;
use serde::Deserialize;
use serde_json::{json, Value};
use tokio::sync::mpsc;

use orgasmic_core::{DriverEvent, SandboxAllowlist, TextStream};

use crate::r#trait::{
    AcpWsProtocol, BabysitterRequest, DriverConfig, DriverContext, DriverError,
    HarnessControlOutcome, HarnessEventAdapter, HarnessRequest, StdioSpawn, TransitionRequest,
    UserInputRequest, WireMessage,
};
use crate::runtime_options::{
    all_reasoning_efforts, dedupe_non_empty, RuntimeModelOption, RuntimeOptionsCatalog,
    RuntimeOptionsCatalogRpc, RuntimeOptionsRequest, RuntimeOptionsState, RuntimeSpeed,
};
use crate::sandbox::ApprovalResponse;

pub struct CodexAdapter {
    ctx: Option<DriverContext>,
    cfg: Option<CodexAppserverConfig>,
    seqs: EventSeqs,
    thread_id: Option<String>,
    active_turn_id: Option<String>,
    terminal_emitted: bool,
}

impl CodexAdapter {
    pub fn new() -> Self {
        Self {
            ctx: None,
            cfg: None,
            seqs: EventSeqs::default(),
            thread_id: None,
            active_turn_id: None,
            terminal_emitted: false,
        }
    }
}

impl Default for CodexAdapter {
    fn default() -> Self {
        Self::new()
    }
}

#[derive(Debug, Clone, Deserialize, Default)]
struct CodexAppserverConfig {
    #[serde(default)]
    endpoint: Option<String>,
    #[serde(default)]
    model: Option<String>,
    /// Reasoning budget hint forwarded to the app-server (advisory in
    /// simulated mode).
    #[serde(default)]
    reasoning_effort: Option<String>,
    /// Speed preference mapped to Codex app-server `serviceTier` on the next
    /// turn/start.
    #[serde(default)]
    speed: Option<RuntimeSpeed>,
    #[serde(default)]
    prompt_bundle_text: Option<String>,
    #[serde(default = "default_auto_start_turn")]
    auto_start_turn: bool,
}

#[async_trait]
impl HarnessEventAdapter for CodexAdapter {
    fn harness(&self) -> &'static str {
        "codex"
    }

    fn clone_box(&self) -> Box<dyn HarnessEventAdapter> {
        Box::new(CodexAdapter::new())
    }

    fn validate_config(&self, config: &DriverConfig) -> Result<(), DriverError> {
        let cfg: CodexAppserverConfig = serde_json::from_value(config.0.clone())
            .map_err(|e| DriverError::InvalidConfig(e.to_string()))?;
        if let Some(ep) = cfg.endpoint.as_deref() {
            if !ep.is_empty() && url_like(ep).is_err() {
                return Err(DriverError::InvalidConfig(format!(
                    "endpoint must look like ws://host:port or http://host:port: {ep}"
                )));
            }
        }
        Ok(())
    }

    fn stdio_spawn(&self) -> Option<StdioSpawn> {
        let cwd = std::env::var_os("HOME").map(PathBuf::from);
        Some(StdioSpawn {
            command: "codex".into(),
            args: vec!["app-server".into()],
            cwd,
            env: Vec::new(),
        })
    }

    fn upgrades_simulated_to_subprocess(&self) -> bool {
        true
    }

    fn stdio_session_init(
        &mut self,
        ctx: &DriverContext,
        config: &DriverConfig,
    ) -> Result<Value, DriverError> {
        let cfg: CodexAppserverConfig = serde_json::from_value(config.0.clone())
            .map_err(|e| DriverError::InvalidConfig(e.to_string()))?;
        self.ctx = Some(ctx.clone());
        self.cfg = Some(cfg.clone());
        Ok(json!({
            "initialize": initialize_params(),
            "thread_start": thread_start_params(ctx, &cfg)?,
            "auto_turn": cfg.auto_start_turn,
        }))
    }

    fn compose_request(
        &mut self,
        ctx: &DriverContext,
        config: &DriverConfig,
    ) -> Result<HarnessRequest, DriverError> {
        let cfg: CodexAppserverConfig = serde_json::from_value(config.0.clone())
            .map_err(|e| DriverError::InvalidConfig(e.to_string()))?;
        let Some(endpoint) = cfg.endpoint.as_deref().filter(|ep| !ep.is_empty()) else {
            return Ok(HarnessRequest::Simulated {
                events: simulated_start_events(ctx, &cfg),
            });
        };
        self.validate_config(config)?;
        let endpoint = websocket_endpoint(endpoint);
        self.ctx = Some(ctx.clone());
        self.cfg = Some(cfg.clone());
        Ok(HarnessRequest::AcpWs {
            endpoint,
            headers: Default::default(),
            protocol: AcpWsProtocol::JsonRpc,
            session_init: json!({
                "initialize": initialize_params(),
                "thread_start": thread_start_params(ctx, &cfg)?,
                "auto_turn": cfg.auto_start_turn,
            }),
        })
    }

    async fn on_ws_thread_started(
        &mut self,
        endpoint: &str,
        thread_response: &Value,
    ) -> Result<Vec<DriverEvent>, DriverError> {
        let thread_id = thread_response
            .get("thread")
            .and_then(|thread| thread.get("id"))
            .and_then(Value::as_str)
            .ok_or_else(|| {
                DriverError::Transport("thread/start response missing thread.id".into())
            })?
            .to_string();
        self.thread_id = Some(thread_id.clone());
        let ctx = self
            .ctx
            .as_ref()
            .ok_or_else(|| DriverError::Other("codex ctx missing".into()))?;
        let cfg = self
            .cfg
            .as_ref()
            .ok_or_else(|| DriverError::Other("codex config missing".into()))?
            .clone();
        Ok(vec![DriverEvent::Ready {
            protocol_version: "codex-appserver/1".into(),
            capabilities: json!({
                "simulated": false,
                "kind": ctx.run_kind,
                "model": cfg.model,
                "reasoning_effort": cfg.reasoning_effort,
                "speed": cfg.speed,
                "endpoint": endpoint,
                "thread_id": thread_id,
            }),
        }])
    }

    fn ws_turn_start_params(&mut self) -> Result<Value, DriverError> {
        let thread_id = self
            .thread_id
            .as_deref()
            .ok_or_else(|| DriverError::Transport("codex thread missing".into()))?;
        let ctx = self
            .ctx
            .as_ref()
            .ok_or_else(|| DriverError::Other("codex ctx missing".into()))?;
        let cfg = self
            .cfg
            .as_ref()
            .ok_or_else(|| DriverError::Other("codex config missing".into()))?;
        Ok(turn_start_params(thread_id, ctx, cfg))
    }

    async fn on_ws_response(
        &mut self,
        method: &str,
        response: Value,
    ) -> Result<Vec<DriverEvent>, DriverError> {
        if method == "turn/start" && self.active_turn_id.is_none() {
            self.active_turn_id = response
                .get("turn")
                .and_then(|turn| turn.get("id"))
                .and_then(Value::as_str)
                .map(str::to_string);
        }
        Ok(Vec::new())
    }

    /// Codex sandbox approval server requests (see codex app-server README
    /// "Approvals" and v2 aliases `exec_approval_request` /
    /// `apply_patch_approval_request` / `mcp_tool_call_approval_request`).
    async fn try_handle_approval(
        &mut self,
        method: &str,
        params: &Value,
        allowlist: &SandboxAllowlist,
    ) -> Option<ApprovalResponse> {
        let allowed = match method {
            "exec_approval_request" | "item/commandExecution/requestApproval" => {
                allowlist.allow_exec
            }
            "apply_patch_approval_request" | "item/fileChange/requestApproval" => {
                allowlist.allow_patch
            }
            "mcp_tool_call_approval_request" => allowlist.allow_exec,
            "item/permissions/requestApproval" => approval_permissions_allowed(params, allowlist),
            _ => return None,
        };
        Some(if allowed {
            ApprovalResponse::Approved
        } else {
            ApprovalResponse::Denied
        })
    }

    async fn parse_event(&mut self, raw: Value) -> Vec<DriverEvent> {
        let (tx, mut rx) = mpsc::channel(32);
        let done = if raw.get("error").is_some() {
            send_driver_error(&tx, false, rpc_error_message(&raw)).await;
            false
        } else if let Some((method, params)) = notification(&raw) {
            dispatch_notification(
                method,
                params,
                &tx,
                &mut self.seqs,
                &mut self.active_turn_id,
            )
            .await
            .unwrap_or(false)
        } else {
            false
        };
        if done {
            self.terminal_emitted = true;
        }
        drop(tx);
        let mut events = Vec::new();
        while let Ok(event) = rx.try_recv() {
            events.push(event);
        }
        events
    }

    async fn transition_state(
        &mut self,
        req: TransitionRequest,
    ) -> Result<HarnessControlOutcome, DriverError> {
        let mut outcome = HarnessControlOutcome::event(DriverEvent::TransitionState {
            from: req.from.clone(),
            to: req.to.clone(),
            reason: req.reason.clone(),
        });
        if let (Some(thread_id), Some(turn_id)) =
            (self.thread_id.as_ref(), self.active_turn_id.as_ref())
        {
            let input = format!(
                "orgasmic transition_state request\nfrom: {}\nto: {}\nreason: {}",
                req.from, req.to, req.reason
            );
            outcome.wire_messages.push(WireMessage::JsonRpc {
                method: "turn/steer".into(),
                params: json!({
                    "threadId": thread_id,
                    "expectedTurnId": turn_id,
                    "input": [text_input(input)],
                }),
            });
        }
        Ok(outcome)
    }

    async fn babysitter_action(
        &mut self,
        req: BabysitterRequest,
    ) -> Result<HarnessControlOutcome, DriverError> {
        let mut outcome = HarnessControlOutcome::event(DriverEvent::ToolCall {
            call_id: format!("codex-bs-{}", uuid::Uuid::new_v4()),
            name: req.tool.as_str().into(),
            args: req.payload.clone(),
            seq: self.seqs.next_tool(),
        });
        if let (Some(thread_id), Some(turn_id)) =
            (self.thread_id.as_ref(), self.active_turn_id.as_ref())
        {
            let input = format!(
                "orgasmic babysitter action\nrun: {}\ntool: {}\npayload: {}",
                req.target_run,
                req.tool.as_str(),
                req.payload
            );
            outcome.wire_messages.push(WireMessage::JsonRpc {
                method: "turn/steer".into(),
                params: json!({
                    "threadId": thread_id,
                    "expectedTurnId": turn_id,
                    "input": [text_input(input)],
                }),
            });
        }
        Ok(outcome)
    }

    async fn send_input(
        &mut self,
        req: UserInputRequest,
    ) -> Result<HarnessControlOutcome, DriverError> {
        let input = req.input.trim().to_string();
        if input.is_empty() {
            return Err(DriverError::InvalidConfig("input must not be empty".into()));
        }
        let thread_id = self
            .thread_id
            .as_ref()
            .ok_or_else(|| DriverError::Transport("codex thread missing".into()))?
            .clone();
        let cfg = self
            .cfg
            .as_ref()
            .ok_or_else(|| DriverError::Other("codex config missing".into()))?;
        let mut outcome = HarnessControlOutcome::event(DriverEvent::TextChunk {
            stream: TextStream::User,
            chunk: input.clone(),
            seq: self.seqs.next_text(TextStream::User),
        });
        if let Some(turn_id) = self.active_turn_id.as_ref() {
            outcome.wire_messages.push(WireMessage::JsonRpc {
                method: "turn/steer".into(),
                params: json!({
                    "threadId": thread_id,
                    "expectedTurnId": turn_id,
                    "input": [text_input(input)],
                }),
            });
        } else {
            outcome.wire_messages.push(WireMessage::JsonRpc {
                method: "turn/start".into(),
                params: turn_start_input_params(&thread_id, input, cfg),
            });
        }
        Ok(outcome)
    }

    async fn switch_runtime_options(
        &mut self,
        req: RuntimeOptionsRequest,
    ) -> Result<HarnessControlOutcome, DriverError> {
        let req = req.normalized().map_err(DriverError::InvalidConfig)?;
        if req.is_empty() {
            return Err(DriverError::InvalidConfig(
                "runtime options request must not be empty".into(),
            ));
        }
        if req.provider.is_some() {
            return Err(DriverError::Unsupported("codex provider switching"));
        }
        let cfg = self
            .cfg
            .as_mut()
            .ok_or_else(|| DriverError::Other("codex config missing".into()))?;
        if let Some(model) = req.model.as_ref() {
            cfg.model = Some(model.clone());
        }
        if let Some(effort) = req.reasoning_effort.as_ref() {
            cfg.reasoning_effort = Some(effort.clone());
        }
        if let Some(speed) = req.speed {
            cfg.speed = Some(speed);
        }
        Ok(HarnessControlOutcome::event(DriverEvent::TextChunk {
            stream: TextStream::System,
            chunk: format!(
                "runtime options updated for next Codex turn: {}",
                req.summary()
            ),
            seq: self.seqs.next_text(TextStream::System),
        }))
    }

    fn runtime_options_catalog_rpc(&self) -> Option<RuntimeOptionsCatalogRpc> {
        Some(RuntimeOptionsCatalogRpc {
            method: "model/list".into(),
            params: json!({
                "includeHidden": false,
                "limit": 500,
            }),
        })
    }

    async fn runtime_options_catalog_from_response(
        &mut self,
        response: Value,
    ) -> Result<RuntimeOptionsCatalog, DriverError> {
        Ok(codex_catalog_from_model_list(
            &response,
            self.cfg.as_ref(),
            "codex:model/list",
        ))
    }

    async fn runtime_options_catalog(&mut self) -> Result<RuntimeOptionsCatalog, DriverError> {
        Ok(codex_fallback_catalog(self.cfg.as_ref()))
    }

    async fn release(&mut self, reason: String) -> Result<HarnessControlOutcome, DriverError> {
        let _ = reason;
        let mut outcome = HarnessControlOutcome {
            close: true,
            ..HarnessControlOutcome::default()
        };
        if let (Some(thread_id), Some(turn_id)) =
            (self.thread_id.as_ref(), self.active_turn_id.as_ref())
        {
            outcome.wire_messages.push(WireMessage::JsonRpc {
                method: "turn/interrupt".into(),
                params: json!({
                    "threadId": thread_id,
                    "turnId": turn_id,
                }),
            });
        }
        Ok(outcome)
    }

    fn terminal_emitted(&self) -> bool {
        self.terminal_emitted
    }

    fn ignores_stderr_line(&self, line: &str) -> bool {
        if let Ok(value) = serde_json::from_str::<Value>(line) {
            let is_codex_diagnostic = value
                .get("target")
                .and_then(Value::as_str)
                .map(|target| target.starts_with("codex_"))
                .unwrap_or(false);
            if is_codex_diagnostic {
                return true;
            }
        }
        line.contains("codex_core_skills::loader")
            && line.contains("icon path must not contain '..'")
    }
}

fn default_auto_start_turn() -> bool {
    true
}

fn url_like(s: &str) -> Result<(), &'static str> {
    if s.starts_with("ws://")
        || s.starts_with("wss://")
        || s.starts_with("http://")
        || s.starts_with("https://")
    {
        Ok(())
    } else {
        Err("not a URL")
    }
}

fn websocket_endpoint(endpoint: &str) -> String {
    if let Some(rest) = endpoint.strip_prefix("http://") {
        format!("ws://{rest}")
    } else if let Some(rest) = endpoint.strip_prefix("https://") {
        format!("wss://{rest}")
    } else {
        endpoint.to_string()
    }
}

#[derive(Default)]
struct EventSeqs {
    assistant: u64,
    user: u64,
    stdout: u64,
    stderr: u64,
    system: u64,
    tool: u64,
}

impl EventSeqs {
    fn next_text(&mut self, stream: TextStream) -> u64 {
        let slot = match stream {
            TextStream::Stdout => &mut self.stdout,
            TextStream::Stderr => &mut self.stderr,
            TextStream::Assistant => &mut self.assistant,
            TextStream::User => &mut self.user,
            TextStream::System => &mut self.system,
        };
        let seq = *slot;
        *slot += 1;
        seq
    }

    fn next_tool(&mut self) -> u64 {
        let seq = self.tool;
        self.tool += 1;
        seq
    }
}

async fn dispatch_notification(
    method: &str,
    params: &Value,
    events: &mpsc::Sender<DriverEvent>,
    seqs: &mut EventSeqs,
    active_turn_id: &mut Option<String>,
) -> Result<bool, DriverError> {
    match method {
        "turn/started" => {
            if let Some(id) = params
                .get("turn")
                .and_then(|turn| turn.get("id"))
                .and_then(Value::as_str)
            {
                *active_turn_id = Some(id.to_string());
            }
        }
        "item/agentMessage/delta" => {
            if let Some(delta) = params.get("delta").and_then(Value::as_str) {
                send_text(events, seqs, TextStream::Assistant, delta.to_string()).await;
            }
        }
        "item/reasoning/textDelta" | "item/reasoning/summaryTextDelta" => {
            if let Some(delta) = params.get("delta").and_then(Value::as_str) {
                send_text(events, seqs, TextStream::System, delta.to_string()).await;
            }
        }
        "item/commandExecution/outputDelta" | "item/fileChange/outputDelta" => {
            if let Some(delta) = params.get("delta").and_then(Value::as_str) {
                send_text(events, seqs, TextStream::Stdout, delta.to_string()).await;
            }
        }
        "command/exec/outputDelta" | "process/outputDelta" => {
            let stream = match params.get("stream").and_then(Value::as_str) {
                Some("stderr") => TextStream::Stderr,
                _ => TextStream::Stdout,
            };
            if let Some(delta) = params.get("delta").and_then(Value::as_str) {
                send_text(events, seqs, stream, delta.to_string()).await;
            } else if let Some(delta) = params.get("deltaBase64").and_then(Value::as_str) {
                send_text(events, seqs, stream, format!("base64:{delta}")).await;
            }
        }
        "item/started" => {
            if let Some(item) = params.get("item") {
                emit_thread_item_tool_call(item, events, seqs).await;
            }
        }
        "rawResponseItem/completed" => {
            if let Some(item) = params.get("item") {
                emit_response_item(item, events, seqs).await;
            }
        }
        "turn/completed" => {
            emit_turn_completion(params, events).await;
            return Ok(true);
        }
        "thread/closed" => {
            // No fabricated summary: the daemon assembles the worker's real
            // report from the assistant text chunks when summary is None.
            let _ = events
                .send(DriverEvent::RunComplete { summary: None })
                .await;
            return Ok(true);
        }
        "error" => {
            let fatal = !params
                .get("willRetry")
                .and_then(Value::as_bool)
                .unwrap_or(false);
            send_driver_error(events, fatal, notification_error_message(params)).await;
            if fatal {
                return Ok(true);
            }
        }
        "warning" | "guardianWarning" | "configWarning" => {
            if let Some(message) = params
                .get("message")
                .or_else(|| params.get("summary"))
                .and_then(Value::as_str)
            {
                send_text(events, seqs, TextStream::System, message.to_string()).await;
            }
        }
        _ => {}
    }
    Ok(false)
}

async fn send_text(
    events: &mpsc::Sender<DriverEvent>,
    seqs: &mut EventSeqs,
    stream: TextStream,
    chunk: String,
) {
    let seq = seqs.next_text(stream);
    let _ = events
        .send(DriverEvent::TextChunk { stream, chunk, seq })
        .await;
}

async fn send_driver_error(events: &mpsc::Sender<DriverEvent>, fatal: bool, message: String) {
    let _ = events
        .send(DriverEvent::DriverError { fatal, message })
        .await;
}

async fn emit_turn_completion(params: &Value, events: &mpsc::Sender<DriverEvent>) {
    let turn = params.get("turn").unwrap_or(params);
    match turn.get("status").and_then(Value::as_str) {
        Some("failed") => {
            let error = turn
                .get("error")
                .map(notification_error_message)
                .unwrap_or_else(|| "codex turn failed".into());
            let _ = events
                .send(DriverEvent::RunFail {
                    error_code: "codex_turn_failed".into(),
                    error_markdown: error,
                })
                .await;
        }
        _ => {
            // turn/completed carries only a status field, never the turn's
            // output; a fabricated "codex turn <status>" marker would shadow
            // the real report assembled from assistant text chunks downstream.
            let _ = events
                .send(DriverEvent::RunComplete { summary: None })
                .await;
        }
    }
}

async fn emit_thread_item_tool_call(
    item: &Value,
    events: &mpsc::Sender<DriverEvent>,
    seqs: &mut EventSeqs,
) {
    let Some(item_type) = item.get("type").and_then(Value::as_str) else {
        return;
    };
    let name = match item_type {
        "commandExecution" => "command_execution".to_string(),
        "fileChange" => "file_change".to_string(),
        "mcpToolCall" => item
            .get("tool")
            .and_then(Value::as_str)
            .unwrap_or("mcp_tool_call")
            .to_string(),
        "dynamicToolCall" => {
            let tool = item
                .get("tool")
                .and_then(Value::as_str)
                .unwrap_or("dynamic_tool_call");
            match item.get("namespace").and_then(Value::as_str) {
                Some(ns) => format!("{ns}.{tool}"),
                None => tool.to_string(),
            }
        }
        "collabAgentToolCall" => item
            .get("tool")
            .and_then(Value::as_str)
            .unwrap_or("collab_agent_tool_call")
            .to_string(),
        "webSearch" => "web_search".to_string(),
        _ => return,
    };
    let call_id = item
        .get("id")
        .and_then(Value::as_str)
        .map(str::to_string)
        .unwrap_or_else(|| format!("codex-tool-{}", uuid::Uuid::new_v4()));
    let _ = events
        .send(DriverEvent::ToolCall {
            call_id,
            name,
            args: item.clone(),
            seq: seqs.next_tool(),
        })
        .await;
}

async fn emit_response_item(
    item: &Value,
    events: &mpsc::Sender<DriverEvent>,
    seqs: &mut EventSeqs,
) {
    let Some(item_type) = item.get("type").and_then(Value::as_str) else {
        return;
    };
    match item_type {
        "function_call" => {
            let call_id = item
                .get("call_id")
                .and_then(Value::as_str)
                .unwrap_or("codex-function-call")
                .to_string();
            let name = item
                .get("name")
                .and_then(Value::as_str)
                .unwrap_or("function_call")
                .to_string();
            let args = item
                .get("arguments")
                .and_then(Value::as_str)
                .map(parse_json_or_string)
                .unwrap_or(Value::Null);
            let _ = events
                .send(DriverEvent::ToolCall {
                    call_id,
                    name,
                    args,
                    seq: seqs.next_tool(),
                })
                .await;
        }
        "custom_tool_call" => {
            let call_id = item
                .get("call_id")
                .and_then(Value::as_str)
                .unwrap_or("codex-custom-tool-call")
                .to_string();
            let name = item
                .get("name")
                .and_then(Value::as_str)
                .unwrap_or("custom_tool_call")
                .to_string();
            let args = item
                .get("input")
                .and_then(Value::as_str)
                .map(parse_json_or_string)
                .unwrap_or(Value::Null);
            let _ = events
                .send(DriverEvent::ToolCall {
                    call_id,
                    name,
                    args,
                    seq: seqs.next_tool(),
                })
                .await;
        }
        "function_call_output" | "custom_tool_call_output" => {
            let call_id = item
                .get("call_id")
                .and_then(Value::as_str)
                .unwrap_or("codex-tool-output")
                .to_string();
            let _ = events
                .send(DriverEvent::ToolResult {
                    call_id,
                    ok: true,
                    output: item.get("output").cloned().unwrap_or(Value::Null),
                    seq: seqs.next_tool(),
                })
                .await;
        }
        "local_shell_call" => {
            let call_id = item
                .get("call_id")
                .and_then(Value::as_str)
                .unwrap_or("codex-local-shell-call")
                .to_string();
            let _ = events
                .send(DriverEvent::ToolCall {
                    call_id,
                    name: "local_shell_call".into(),
                    args: item.get("action").cloned().unwrap_or_else(|| item.clone()),
                    seq: seqs.next_tool(),
                })
                .await;
        }
        _ => {}
    }
}

fn thread_start_params(
    ctx: &DriverContext,
    cfg: &CodexAppserverConfig,
) -> Result<Value, DriverError> {
    let cwd = effective_cwd(ctx)?;
    Ok(json!({
        "model": cfg.model,
        "cwd": cwd,
        "runtimeWorkspaceRoots": [cwd],
        "baseInstructions": format!(
            "You are running under orgasmic for task {} using worker {}.",
            ctx.task_id, ctx.worker_id
        ),
        "experimentalRawEvents": true,
        "persistExtendedHistory": false,
    }))
}

fn initialize_params() -> Value {
    json!({
        "clientInfo": {
            "name": "orgasmic",
            "title": "orgasmic",
            "version": env!("CARGO_PKG_VERSION"),
        },
        "capabilities": {
            "experimentalApi": true,
            "requestAttestation": false,
        },
    })
}

fn turn_start_params(thread_id: &str, ctx: &DriverContext, cfg: &CodexAppserverConfig) -> Value {
    turn_start_input_params(thread_id, prompt_bundle(ctx, cfg), cfg)
}

fn turn_start_input_params(thread_id: &str, input: String, cfg: &CodexAppserverConfig) -> Value {
    let mut params = json!({
        "threadId": thread_id,
        "input": [text_input(input)],
        "model": cfg.model,
        "effort": cfg.reasoning_effort,
    });
    if let Some(speed) = cfg.speed {
        params["serviceTier"] = json!(codex_service_tier(speed));
    }
    params
}

fn codex_service_tier(speed: RuntimeSpeed) -> &'static str {
    match speed {
        RuntimeSpeed::Normal => "auto",
        RuntimeSpeed::Fast => "priority",
    }
}

fn codex_catalog_from_model_list(
    response: &Value,
    cfg: Option<&CodexAppserverConfig>,
    source: &str,
) -> RuntimeOptionsCatalog {
    let models = response
        .get("data")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|value| codex_model_option(value, cfg))
        .collect::<Vec<_>>();
    let efforts = aggregate_model_efforts(&models);
    let speeds = aggregate_model_speeds(&models);
    RuntimeOptionsCatalog {
        source: source.into(),
        provider_switching: false,
        current: codex_current_state(cfg),
        providers: Vec::new(),
        models,
        efforts,
        speeds,
    }
}

fn codex_fallback_catalog(cfg: Option<&CodexAppserverConfig>) -> RuntimeOptionsCatalog {
    let current = codex_current_state(cfg);
    let models = current
        .model
        .as_ref()
        .map(|model| {
            let mut speeds = vec![RuntimeSpeed::Normal];
            if current.speed == Some(RuntimeSpeed::Fast) {
                speeds.push(RuntimeSpeed::Fast);
            }
            RuntimeModelOption {
                id: model.clone(),
                label: model.clone(),
                provider: None,
                current: true,
                reasoning_efforts: current
                    .reasoning_effort
                    .clone()
                    .map(|effort| vec![effort])
                    .unwrap_or_else(all_reasoning_efforts),
                speeds,
                default_reasoning_effort: current.reasoning_effort.clone(),
            }
        })
        .into_iter()
        .collect::<Vec<_>>();
    RuntimeOptionsCatalog {
        source: "codex:fallback".into(),
        provider_switching: false,
        current,
        providers: Vec::new(),
        efforts: aggregate_model_efforts(&models),
        speeds: aggregate_model_speeds(&models),
        models,
    }
}

fn codex_current_state(cfg: Option<&CodexAppserverConfig>) -> RuntimeOptionsState {
    RuntimeOptionsState {
        provider: None,
        model: cfg.and_then(|cfg| cfg.model.clone()),
        reasoning_effort: cfg.and_then(|cfg| cfg.reasoning_effort.clone()),
        speed: cfg.and_then(|cfg| cfg.speed),
    }
}

fn codex_model_option(
    value: &Value,
    cfg: Option<&CodexAppserverConfig>,
) -> Option<RuntimeModelOption> {
    let id = string_field(value, &["model", "id"])?;
    let label = string_field(value, &["displayName", "id"]).unwrap_or_else(|| id.clone());
    let default_reasoning_effort = string_field(value, &["defaultReasoningEffort"]);
    let mut efforts = value
        .get("supportedReasoningEfforts")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|entry| string_field(entry, &["reasoningEffort"]))
        .collect::<Vec<_>>();
    if efforts.is_empty() {
        if let Some(default_effort) = default_reasoning_effort.clone() {
            efforts.push(default_effort);
        }
    }
    if efforts.is_empty() {
        efforts = all_reasoning_efforts();
    }
    efforts = dedupe_non_empty(efforts);
    let mut speeds = vec![RuntimeSpeed::Normal];
    if codex_model_supports_fast(value) {
        speeds.push(RuntimeSpeed::Fast);
    }
    let current = cfg
        .and_then(|cfg| cfg.model.as_deref())
        .map(|model| model == id)
        .unwrap_or_else(|| {
            value
                .get("isDefault")
                .and_then(Value::as_bool)
                .unwrap_or(false)
        });
    Some(RuntimeModelOption {
        id,
        label,
        provider: None,
        current,
        reasoning_efforts: efforts,
        speeds,
        default_reasoning_effort,
    })
}

fn codex_model_supports_fast(value: &Value) -> bool {
    value
        .get("serviceTiers")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|tier| string_field(tier, &["id"]))
        .any(|id| matches!(id.as_str(), "priority" | "fast"))
        || value
            .get("additionalSpeedTiers")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .filter_map(Value::as_str)
            .any(|id| matches!(id, "priority" | "fast"))
}

fn aggregate_model_efforts(models: &[RuntimeModelOption]) -> Vec<String> {
    let efforts = dedupe_non_empty(
        models
            .iter()
            .flat_map(|model| model.reasoning_efforts.iter().cloned()),
    );
    if efforts.is_empty() {
        all_reasoning_efforts()
    } else {
        efforts
    }
}

fn aggregate_model_speeds(models: &[RuntimeModelOption]) -> Vec<RuntimeSpeed> {
    let mut speeds = Vec::new();
    for speed in models.iter().flat_map(|model| model.speeds.iter().copied()) {
        if !speeds.contains(&speed) {
            speeds.push(speed);
        }
    }
    if speeds.is_empty() {
        speeds.push(RuntimeSpeed::Normal);
    }
    speeds
}

fn string_field(value: &Value, keys: &[&str]) -> Option<String> {
    keys.iter()
        .find_map(|key| value.get(*key).and_then(Value::as_str))
        .map(str::to_string)
}

fn text_input(text: String) -> Value {
    json!({
        "type": "text",
        "text": text,
        "text_elements": [],
    })
}

fn effective_cwd(ctx: &DriverContext) -> Result<String, DriverError> {
    let cwd = ctx
        .worktree
        .clone()
        .unwrap_or(std::env::current_dir().map_err(DriverError::Io)?);
    Ok(cwd.display().to_string())
}

fn prompt_bundle(ctx: &DriverContext, cfg: &CodexAppserverConfig) -> String {
    let mut prompt = String::new();
    if let Some(bundle) = cfg.prompt_bundle_text.as_deref() {
        if !bundle.trim().is_empty() {
            prompt.push_str(bundle.trim());
            prompt.push_str("\n\n");
        }
    }
    prompt.push_str(&format!(
        "orgasmic run\nrun_kind: {:?}\ntask_id: {}\nworker_id: {}",
        ctx.run_kind, ctx.task_id, ctx.worker_id
    ));
    if let Some(project_id) = ctx.project_id.as_ref() {
        prompt.push_str(&format!("\nproject_id: {project_id}"));
    }
    if let Some(worktree) = ctx.worktree.as_ref() {
        prompt.push_str(&format!("\nworktree: {}", worktree.display()));
    }
    if let Some(target) = ctx.babysitter_target.as_ref() {
        prompt.push_str(&format!("\nbabysitter_target: {target}"));
    }
    prompt
}

fn notification(value: &Value) -> Option<(&str, &Value)> {
    let method = value.get("method").and_then(Value::as_str)?;
    let params = value.get("params").unwrap_or(&Value::Null);
    Some((method, params))
}

fn approval_permissions_allowed(params: &Value, allowlist: &SandboxAllowlist) -> bool {
    if params.get("networkApprovalContext").is_some()
        || params
            .get("additionalPermissions")
            .and_then(|value| value.get("network"))
            .is_some()
    {
        return allowlist.allow_network;
    }
    if params
        .get("additionalPermissions")
        .and_then(|value| value.get("filesystem"))
        .is_some()
    {
        return allowlist.allow_writes_outside_cwd;
    }
    allowlist.allow_exec && allowlist.allow_patch
}

fn rpc_error_message(value: &Value) -> String {
    let error = value.get("error").unwrap_or(value);
    if let Some(message) = error.get("message").and_then(Value::as_str) {
        return message.to_string();
    }
    error.to_string()
}

fn notification_error_message(value: &Value) -> String {
    if let Some(message) = value
        .get("error")
        .and_then(|error| error.get("message"))
        .and_then(Value::as_str)
    {
        return message.to_string();
    }
    if let Some(message) = value.get("message").and_then(Value::as_str) {
        return message.to_string();
    }
    value.to_string()
}

fn parse_json_or_string(s: &str) -> Value {
    serde_json::from_str(s).unwrap_or_else(|_| Value::String(s.to_string()))
}

pub fn simulated_config() -> DriverConfig {
    DriverConfig::from_value(Value::Object(Default::default()))
}

fn simulated_start_events(ctx: &DriverContext, cfg: &CodexAppserverConfig) -> Vec<DriverEvent> {
    vec![DriverEvent::Ready {
        protocol_version: "codex-appserver/1".into(),
        capabilities: json!({
            "simulated": true,
            "kind": ctx.run_kind,
            "model": cfg.model,
            "reasoning_effort": cfg.reasoning_effort,
        }),
    }]
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{CodexAppserverDriver, RunKind, WorkerDriver};
    use futures::{SinkExt, StreamExt};
    use tokio::io::{AsyncRead, AsyncWrite};
    use tokio::net::TcpListener;
    use tokio_tungstenite::{accept_async, tungstenite::Message, WebSocketStream};

    use orgasmic_core::RuntimeIdentity;

    fn ctx(id: &str, kind: RunKind) -> DriverContext {
        DriverContext {
            identity: RuntimeIdentity::new(id, "boot-test"),
            run_kind: kind,
            task_id: "TASK-006".into(),
            worker_id: "implementer-codex".into(),
            project_id: None,
            worktree: None,
            babysitter_target: None,
        }
    }

    #[tokio::test]
    async fn simulated_acquire_emits_ready() {
        let d = CodexAppserverDriver;
        let mut s = d
            .acquire(ctx("run-cx", RunKind::Worker), simulated_config())
            .await
            .unwrap();
        let ev = s.events.recv().await.unwrap();
        assert!(matches!(ev, DriverEvent::Ready { .. }));
        s.control.release("done").await.unwrap();
    }

    #[test]
    fn validates_endpoint_shape() {
        let d = CodexAppserverDriver;
        let bad = DriverConfig::from_value(json!({"endpoint": "not-a-url"}));
        assert!(d.validate(&bad).is_err());
        let ok = DriverConfig::from_value(json!({"endpoint": "ws://localhost:8123"}));
        assert!(d.validate(&ok).is_ok());
    }

    #[tokio::test]
    async fn runtime_options_update_next_turn_params() {
        let mut adapter = CodexAdapter::new();
        adapter
            .stdio_session_init(
                &ctx("run-cx-runtime", RunKind::Worker),
                &DriverConfig::from_value(json!({
                    "model": "gpt-old",
                    "reasoning_effort": "low",
                })),
            )
            .unwrap();
        adapter
            .on_ws_thread_started("stdio", &json!({"thread": {"id": "thread-fixture"}}))
            .await
            .unwrap();

        let outcome = adapter
            .switch_runtime_options(RuntimeOptionsRequest {
                model: Some("gpt-new".into()),
                reasoning_effort: Some("high".into()),
                speed: Some(RuntimeSpeed::Fast),
                ..RuntimeOptionsRequest::default()
            })
            .await
            .unwrap();
        assert!(matches!(
            outcome.events.first(),
            Some(DriverEvent::TextChunk {
                stream: TextStream::System,
                ..
            })
        ));

        let params = adapter.ws_turn_start_params().unwrap();
        assert_eq!(params["model"], "gpt-new");
        assert_eq!(params["effort"], "high");
        assert_eq!(params["serviceTier"], "priority");
    }

    #[tokio::test]
    async fn runtime_options_catalog_uses_model_list_capabilities() {
        let mut adapter = CodexAdapter::new();
        adapter
            .stdio_session_init(
                &ctx("run-cx-catalog", RunKind::Worker),
                &DriverConfig::from_value(json!({
                    "model": "gpt-fast",
                    "reasoning_effort": "high",
                    "speed": "fast",
                })),
            )
            .unwrap();

        let catalog = adapter
            .runtime_options_catalog_from_response(json!({
                "data": [
                    {
                        "model": "gpt-fast",
                        "displayName": "GPT Fast",
                        "defaultReasoningEffort": "medium",
                        "supportedReasoningEfforts": [
                            {"reasoningEffort": "low"},
                            {"reasoningEffort": "high"}
                        ],
                        "serviceTiers": [
                            {"id": "priority", "name": "Priority", "description": "Fast"}
                        ]
                    },
                    {
                        "model": "gpt-normal",
                        "displayName": "GPT Normal",
                        "defaultReasoningEffort": "low",
                        "supportedReasoningEfforts": [
                            {"reasoningEffort": "low"}
                        ]
                    }
                ]
            }))
            .await
            .unwrap();

        assert!(!catalog.provider_switching);
        assert_eq!(catalog.current.model.as_deref(), Some("gpt-fast"));
        assert_eq!(catalog.models.len(), 2);
        assert!(catalog.models[0].current);
        assert_eq!(catalog.models[0].reasoning_efforts, vec!["low", "high"]);
        assert_eq!(
            catalog.models[0].speeds,
            vec![RuntimeSpeed::Normal, RuntimeSpeed::Fast]
        );
        assert_eq!(catalog.models[1].speeds, vec![RuntimeSpeed::Normal]);
    }

    #[tokio::test]
    async fn configured_unreachable_endpoint_errors() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        drop(listener);

        let d = CodexAppserverDriver;
        let cfg = DriverConfig::from_value(json!({"endpoint": format!("ws://{addr}")}));
        let result = d.acquire(ctx("run-down", RunKind::Worker), cfg).await;
        let Err(err) = result else {
            panic!("configured unreachable endpoint should fail acquire");
        };
        assert!(matches!(err, DriverError::Transport(_)));
    }

    #[tokio::test]
    async fn real_appserver_fixture_bridge_streams_events() {
        if !codex_available() {
            eprintln!("skipping real_appserver_fixture_bridge_streams_events: codex not on PATH");
            return;
        }

        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let mut ws = accept_async(stream).await.unwrap();

            let init = recv_json(&mut ws).await;
            assert_eq!(init["method"], "initialize");
            send_response(&mut ws, init["id"].clone(), json!({"userAgent": "fixture"})).await;

            let thread = recv_json(&mut ws).await;
            assert_eq!(thread["method"], "thread/start");
            assert_eq!(thread["params"]["model"], "gpt-fixture");
            assert_eq!(thread["params"]["experimentalRawEvents"], true);
            send_response(
                &mut ws,
                thread["id"].clone(),
                json!({"thread": {"id": "thread-fixture"}}),
            )
            .await;

            let turn = recv_json(&mut ws).await;
            assert_eq!(turn["method"], "turn/start");
            assert_eq!(turn["params"]["threadId"], "thread-fixture");
            assert_eq!(turn["params"]["effort"], "low");
            assert!(turn["params"]["input"][0]["text"]
                .as_str()
                .unwrap()
                .contains("task_id: TASK-006"));
            send_response(
                &mut ws,
                turn["id"].clone(),
                json!({"turn": {"id": "turn-fixture", "status": "inProgress", "items": []}}),
            )
            .await;

            ws.send(Message::Text(
                json!({
                    "jsonrpc": "2.0",
                    "method": "item/agentMessage/delta",
                    "params": {
                        "threadId": "thread-fixture",
                        "turnId": "turn-fixture",
                        "itemId": "item-1",
                        "delta": "hello",
                    },
                })
                .to_string(),
            ))
            .await
            .unwrap();
            ws.send(Message::Text(
                json!({
                    "jsonrpc": "2.0",
                    "method": "rawResponseItem/completed",
                    "params": {
                        "threadId": "thread-fixture",
                        "turnId": "turn-fixture",
                        "item": {
                            "type": "function_call",
                            "name": "transition_state",
                            "arguments": "{\"to\":\"done\"}",
                            "call_id": "call-1",
                        },
                    },
                })
                .to_string(),
            ))
            .await
            .unwrap();
            ws.send(Message::Text(
                json!({
                    "jsonrpc": "2.0",
                    "method": "turn/completed",
                    "params": {
                        "threadId": "thread-fixture",
                        "turn": {
                            "id": "turn-fixture",
                            "status": "completed",
                            "items": [],
                        },
                    },
                })
                .to_string(),
            ))
            .await
            .unwrap();
        });

        let d = CodexAppserverDriver;
        let cfg = DriverConfig::from_value(json!({
            "endpoint": format!("ws://{addr}"),
            "model": "gpt-fixture",
            "reasoning_effort": "low",
        }));
        let mut session = d
            .acquire(ctx("run-real-fixture", RunKind::Worker), cfg)
            .await
            .unwrap();

        let ready = session.events.recv().await.unwrap();
        let DriverEvent::Ready { capabilities, .. } = ready else {
            panic!("expected Ready, got {ready:?}");
        };
        assert_eq!(capabilities["simulated"], false);
        assert_eq!(capabilities["thread_id"], "thread-fixture");

        let text = session.events.recv().await.unwrap();
        assert!(matches!(
            text,
            DriverEvent::TextChunk {
                stream: TextStream::Assistant,
                chunk,
                seq: 0,
            } if chunk == "hello"
        ));

        let tool = session.events.recv().await.unwrap();
        assert!(matches!(
            tool,
            DriverEvent::ToolCall {
                call_id,
                name,
                args,
                seq: 0,
            } if call_id == "call-1" && name == "transition_state" && args["to"] == "done"
        ));

        let complete = session.events.recv().await.unwrap();
        assert!(matches!(complete, DriverEvent::RunComplete { .. }));
        server.await.unwrap();
    }

    #[test]
    fn transport_name_is_stable() {
        assert_eq!(CodexAppserverDriver.transport(), "codex-appserver");
    }

    #[tokio::test]
    async fn turn_completed_without_content_emits_summary_none() {
        let (tx, mut rx) = mpsc::channel(4);
        let mut seqs = EventSeqs::default();
        let mut active_turn_id = Some("t-1".to_string());
        let terminal = dispatch_notification(
            "turn/completed",
            &json!({"turn": {"id": "t-1", "status": "completed"}}),
            &tx,
            &mut seqs,
            &mut active_turn_id,
        )
        .await
        .unwrap();
        assert!(terminal);
        let event = rx.recv().await.unwrap();
        assert!(
            matches!(event, DriverEvent::RunComplete { summary: None }),
            "expected RunComplete with no fabricated summary, got {event:?}"
        );
    }

    #[tokio::test]
    async fn thread_closed_emits_summary_none() {
        let (tx, mut rx) = mpsc::channel(4);
        let mut seqs = EventSeqs::default();
        let mut active_turn_id = None;
        let terminal = dispatch_notification(
            "thread/closed",
            &json!({}),
            &tx,
            &mut seqs,
            &mut active_turn_id,
        )
        .await
        .unwrap();
        assert!(terminal);
        let event = rx.recv().await.unwrap();
        assert!(
            matches!(event, DriverEvent::RunComplete { summary: None }),
            "expected RunComplete with no fabricated summary, got {event:?}"
        );
    }

    #[tokio::test]
    async fn turn_failed_still_maps_to_run_fail() {
        let (tx, mut rx) = mpsc::channel(4);
        let mut seqs = EventSeqs::default();
        let mut active_turn_id = Some("t-1".to_string());
        let terminal = dispatch_notification(
            "turn/completed",
            &json!({"turn": {"id": "t-1", "status": "failed", "error": {"message": "boom"}}}),
            &tx,
            &mut seqs,
            &mut active_turn_id,
        )
        .await
        .unwrap();
        assert!(terminal);
        let event = rx.recv().await.unwrap();
        assert!(
            matches!(
                &event,
                DriverEvent::RunFail { error_code, error_markdown }
                    if error_code == "codex_turn_failed" && error_markdown.contains("boom")
            ),
            "expected RunFail preserved, got {event:?}"
        );
    }

    async fn recv_json<S>(ws: &mut WebSocketStream<S>) -> Value
    where
        S: AsyncRead + AsyncWrite + Unpin,
    {
        loop {
            let message = ws.next().await.unwrap().unwrap();
            let value = match message {
                Message::Text(text) => serde_json::from_str(&text).unwrap(),
                Message::Binary(bytes) => serde_json::from_slice(&bytes).unwrap(),
                Message::Ping(_) | Message::Pong(_) | Message::Frame(_) => Value::Null,
                Message::Close(_) => Value::Null,
            };
            if !value.is_null() {
                return value;
            }
        }
    }

    async fn send_response<S>(ws: &mut WebSocketStream<S>, id: Value, result: Value)
    where
        S: AsyncRead + AsyncWrite + Unpin,
    {
        ws.send(Message::Text(
            json!({
                "jsonrpc": "2.0",
                "id": id,
                "result": result,
            })
            .to_string(),
        ))
        .await
        .unwrap();
    }

    fn codex_available() -> bool {
        std::process::Command::new("which")
            .arg("codex")
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .map(|status| status.success())
            .unwrap_or(false)
    }
}
