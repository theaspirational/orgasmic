// arch: arch_A53QX.3
// orgasmic:arch_A53QX, dec_ASB1A
//! Hermes harness adapter.
//!
//! Hermes speaks ACP over a WebSocket shape similar to Claude Code but with
//! its own session-token handshake header and capability set.

use std::path::PathBuf;
use std::time::Duration;

use async_trait::async_trait;
use serde::Deserialize;
use serde_json::{json, Value};
use tokio::process::Command;
use tokio::time::timeout;

use orgasmic_core::{DriverEvent, TextStream};

use crate::r#trait::{
    AcpWsProtocol, BabysitterRequest, DriverConfig, DriverContext, DriverError,
    HarnessControlOutcome, HarnessEventAdapter, HarnessRequest, RunKind, StdioSpawn,
    TransitionRequest, UserInputRequest, WireMessage,
};
use crate::runtime_options::{
    dedupe_non_empty, RuntimeModelOption, RuntimeOptionsCatalog, RuntimeOptionsRequest,
    RuntimeOptionsState, RuntimeProviderOption, RuntimeSpeed,
};

const TRANSPORT: &str = "hermes";
const HERMES_INVENTORY_TIMEOUT: Duration = Duration::from_secs(8);

pub struct HermesAdapter {
    seq: u64,
    run_kind: Option<RunKind>,
    terminal_emitted: bool,
    ctx: Option<DriverContext>,
    cfg: Option<HermesConfig>,
    session_id: Option<String>,
}

impl HermesAdapter {
    pub fn new() -> Self {
        Self {
            seq: 0,
            run_kind: None,
            terminal_emitted: false,
            ctx: None,
            cfg: None,
            session_id: None,
        }
    }

    fn record_terminal(&mut self, event: &DriverEvent) {
        if matches!(
            event,
            DriverEvent::RunComplete { .. } | DriverEvent::RunFail { .. }
        ) {
            self.terminal_emitted = true;
        }
    }

    fn take_seq(&mut self) -> u64 {
        let seq = self.seq;
        self.seq = self.seq.saturating_add(1);
        seq
    }
}

impl Default for HermesAdapter {
    fn default() -> Self {
        Self::new()
    }
}

#[derive(Debug, Clone, Deserialize, Default)]
struct HermesConfig {
    #[serde(default)]
    endpoint: Option<String>,
    #[serde(default)]
    session_token: Option<String>,
    #[serde(default)]
    provider: Option<String>,
    #[serde(default)]
    model: Option<String>,
    #[serde(default, alias = "effort")]
    reasoning_effort: Option<String>,
    #[serde(default)]
    speed: Option<RuntimeSpeed>,
    #[serde(default)]
    prompt_bundle_text: Option<String>,
    #[serde(default = "default_auto_start_turn")]
    auto_start_turn: bool,
}

fn default_auto_start_turn() -> bool {
    true
}

#[async_trait]
impl HarnessEventAdapter for HermesAdapter {
    fn harness(&self) -> &'static str {
        TRANSPORT
    }

    fn clone_box(&self) -> Box<dyn HarnessEventAdapter> {
        Box::new(HermesAdapter::new())
    }

    fn validate_config(&self, config: &DriverConfig) -> Result<(), DriverError> {
        let cfg: HermesConfig = serde_json::from_value(config.0.clone())
            .map_err(|e| DriverError::InvalidConfig(e.to_string()))?;
        let endpoint = configured_endpoint(&cfg);
        if endpoint.is_some()
            && cfg
                .session_token
                .as_deref()
                .map(str::is_empty)
                .unwrap_or(true)
        {
            return Err(DriverError::InvalidConfig(
                "session_token required when endpoint is set".into(),
            ));
        }
        if let Some(endpoint) = endpoint {
            websocket_endpoint(endpoint)?;
        }
        Ok(())
    }

    fn stdio_spawn(&self) -> Option<StdioSpawn> {
        let cwd = std::env::var_os("HOME").map(PathBuf::from);
        Some(StdioSpawn {
            command: "hermes".into(),
            args: vec!["acp".into()],
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
        let cfg: HermesConfig = serde_json::from_value(config.0.clone())
            .map_err(|e| DriverError::InvalidConfig(e.to_string()))?;
        self.ctx = Some(ctx.clone());
        self.cfg = Some(cfg.clone());
        self.run_kind = Some(ctx.run_kind);
        let cwd = ctx
            .worktree
            .as_ref()
            .map(|path| path.to_string_lossy().to_string())
            .unwrap_or_else(|| ".".to_string());
        Ok(json!({
            "initialize": {
                "protocolVersion": 1,
                "clientCapabilities": {},
                "clientInfo": {
                    "name": "orgasmic",
                    "version": env!("CARGO_PKG_VERSION"),
                },
            },
            "thread_start": {
                "cwd": cwd,
                "mcpServers": [],
                "_meta": {
                    "orgasmic": spawn_payload(ctx, &cfg),
                },
            },
            "auto_turn": cfg.auto_start_turn,
        }))
    }

    fn compose_request(
        &mut self,
        ctx: &DriverContext,
        config: &DriverConfig,
    ) -> Result<HarnessRequest, DriverError> {
        let cfg: HermesConfig = serde_json::from_value(config.0.clone())
            .map_err(|e| DriverError::InvalidConfig(e.to_string()))?;
        self.run_kind = Some(ctx.run_kind);
        self.ctx = Some(ctx.clone());
        self.cfg = Some(cfg.clone());
        let Some(endpoint) = configured_endpoint(&cfg) else {
            return Ok(HarnessRequest::Simulated {
                events: simulated_start_events(ctx, &cfg),
            });
        };
        self.validate_config(config)?;
        let session_token = cfg
            .session_token
            .clone()
            .filter(|s| !s.is_empty())
            .ok_or_else(|| {
                DriverError::InvalidConfig("session_token required when endpoint is set".into())
            })?;
        let mut headers = std::collections::BTreeMap::new();
        headers.insert("session_token".into(), session_token);
        Ok(HarnessRequest::AcpWs {
            endpoint: websocket_endpoint(endpoint)?,
            headers,
            protocol: AcpWsProtocol::RawJson,
            session_init: spawn_payload(ctx, &cfg),
        })
    }

    async fn on_ws_connected(&mut self, meta: Value) -> Result<Vec<DriverEvent>, DriverError> {
        Ok(vec![DriverEvent::Ready {
            protocol_version: "hermes/1".into(),
            capabilities: json!({
                "simulated": false,
                "kind": self.run_kind,
                "wire": "websocket",
                "status": meta.get("status").cloned().unwrap_or(Value::Null),
                "provider": self.cfg.as_ref().and_then(|cfg| cfg.provider.clone()),
                "model": self.cfg.as_ref().and_then(|cfg| cfg.model.clone()),
                "reasoning_effort": self.cfg.as_ref().and_then(|cfg| cfg.reasoning_effort.clone()),
                "speed": self.cfg.as_ref().and_then(|cfg| cfg.speed),
            }),
        }])
    }

    async fn parse_event(&mut self, raw: Value) -> Vec<DriverEvent> {
        if let Some(events) = self.acp_events_from_value(&raw) {
            return events;
        }
        match serde_json::from_value::<DriverEvent>(raw.clone())
            .or_else(|_| hermes_event_from_value(raw, &mut self.seq))
        {
            Ok(event) => {
                self.record_terminal(&event);
                vec![event]
            }
            Err(err) => vec![DriverEvent::DriverError {
                fatal: true,
                message: err.to_string(),
            }],
        }
    }

    async fn on_ws_thread_started(
        &mut self,
        endpoint: &str,
        thread_response: &Value,
    ) -> Result<Vec<DriverEvent>, DriverError> {
        if let Some(session_id) = thread_response
            .get("sessionId")
            .and_then(Value::as_str)
            .map(str::to_string)
        {
            self.session_id = Some(session_id.clone());
            return Ok(vec![DriverEvent::Ready {
                protocol_version: "hermes-acp/stdio".into(),
                capabilities: json!({
                    "simulated": false,
                    "kind": self.run_kind,
                    "wire": "stdio",
                    "endpoint": endpoint,
                    "session_id": session_id,
                    "provider": self.cfg.as_ref().and_then(|cfg| cfg.provider.clone()),
                    "model": self.cfg.as_ref().and_then(|cfg| cfg.model.clone()),
                    "reasoning_effort": self.cfg.as_ref().and_then(|cfg| cfg.reasoning_effort.clone()),
                    "speed": self.cfg.as_ref().and_then(|cfg| cfg.speed),
                }),
            }]);
        }
        Ok(Vec::new())
    }

    fn ws_turn_start_params(&mut self) -> Result<Value, DriverError> {
        let session_id = self
            .session_id
            .as_deref()
            .ok_or_else(|| DriverError::Transport("hermes ACP session missing".into()))?;
        let cfg = self
            .cfg
            .as_ref()
            .ok_or_else(|| DriverError::Other("hermes config missing".into()))?;
        Ok(hermes_prompt_params(
            session_id,
            cfg.prompt_bundle_text.as_deref().unwrap_or(""),
        ))
    }

    fn jsonrpc_session_start_method(&self) -> &'static str {
        "session/new"
    }

    fn jsonrpc_turn_start_method(&self) -> &'static str {
        "session/prompt"
    }

    async fn transition_state(
        &mut self,
        req: TransitionRequest,
    ) -> Result<HarnessControlOutcome, DriverError> {
        let control_text = format!(
            "Transition state from {} to {}. Reason: {}",
            req.from, req.to, req.reason
        );
        let wire_message = if let Some(session_id) = self.session_id.as_deref() {
            WireMessage::JsonRpc {
                method: "session/prompt".into(),
                params: hermes_prompt_params(session_id, &control_text),
            }
        } else {
            WireMessage::Json(json!({
                "type": "transition_state",
                "from": req.from,
                "to": req.to,
                "reason": req.reason,
            }))
        };
        Ok(HarnessControlOutcome {
            events: vec![DriverEvent::TransitionState {
                from: req.from.clone(),
                to: req.to.clone(),
                reason: req.reason.clone(),
            }],
            wire_messages: vec![wire_message],
            ..HarnessControlOutcome::default()
        })
    }

    async fn babysitter_action(
        &mut self,
        req: BabysitterRequest,
    ) -> Result<HarnessControlOutcome, DriverError> {
        let control_text = format!(
            "Babysitter action {} for run {}: {}",
            req.tool.as_str(),
            req.target_run,
            req.payload
        );
        let wire_message = if let Some(session_id) = self.session_id.as_deref() {
            WireMessage::JsonRpc {
                method: "session/prompt".into(),
                params: hermes_prompt_params(session_id, &control_text),
            }
        } else {
            WireMessage::Json(json!({
                "type": "babysitter_action",
                "tool": req.tool.as_str(),
                "target_run": req.target_run,
                "payload": req.payload,
            }))
        };
        Ok(HarnessControlOutcome {
            events: vec![DriverEvent::ToolCall {
                call_id: format!("hermes-bs-{}", uuid::Uuid::new_v4()),
                name: req.tool.as_str().into(),
                args: req.payload.clone(),
                seq: self.seq,
            }],
            wire_messages: vec![wire_message],
            ..HarnessControlOutcome::default()
        })
    }

    async fn send_input(
        &mut self,
        req: UserInputRequest,
    ) -> Result<HarnessControlOutcome, DriverError> {
        let input = req.input.trim().to_string();
        if input.is_empty() {
            return Err(DriverError::InvalidConfig("input must not be empty".into()));
        }
        let mut outcome = HarnessControlOutcome {
            events: vec![DriverEvent::TextChunk {
                stream: TextStream::User,
                chunk: input.clone(),
                seq: self.take_seq(),
            }],
            wire_messages: Vec::new(),
            ..HarnessControlOutcome::default()
        };
        if let Some(session_id) = self.session_id.as_deref() {
            outcome.wire_messages.push(WireMessage::JsonRpc {
                method: "session/prompt".into(),
                params: hermes_prompt_params(session_id, &input),
            });
        } else {
            outcome.wire_messages.push(WireMessage::Json(json!({
                "type": "user_input",
                "input": input,
            })));
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

        let mut commands = Vec::new();
        {
            let cfg = self.cfg.get_or_insert_with(HermesConfig::default);
            if req.model.is_some() || req.provider.is_some() {
                let model = req
                    .model
                    .clone()
                    .or_else(|| cfg.model.clone())
                    .ok_or_else(|| {
                        DriverError::InvalidConfig(
                            "Hermes provider switching requires a model".into(),
                        )
                    })?;
                let provider = req.provider.clone().or_else(|| cfg.provider.clone());
                let mut command = format!("/model {}", slash_arg(&model));
                if let Some(provider) = provider.as_deref() {
                    command.push_str(" --provider ");
                    command.push_str(&slash_arg(provider));
                }
                commands.push(command);
            }
            if let Some(effort) = req.reasoning_effort.as_deref() {
                commands.push(format!("/reasoning {}", slash_arg(effort)));
            }
            if let Some(speed) = req.speed {
                commands.push(format!("/fast {}", speed.as_str()));
            }

            if let Some(provider) = req.provider.as_ref() {
                cfg.provider = Some(provider.clone());
            }
            if let Some(model) = req.model.as_ref() {
                cfg.model = Some(model.clone());
            }
            if let Some(effort) = req.reasoning_effort.as_ref() {
                cfg.reasoning_effort = Some(effort.clone());
            }
            if let Some(speed) = req.speed {
                cfg.speed = Some(speed);
            }
        }

        let session_id = self.session_id.clone();
        Ok(HarnessControlOutcome {
            events: vec![DriverEvent::TextChunk {
                stream: TextStream::System,
                chunk: format!("runtime options updated for Hermes: {}", req.summary()),
                seq: self.take_seq(),
            }],
            wire_messages: commands
                .into_iter()
                .map(|command| hermes_command_message(session_id.as_deref(), &command))
                .collect(),
            ..HarnessControlOutcome::default()
        })
    }

    async fn runtime_options_catalog(&mut self) -> Result<RuntimeOptionsCatalog, DriverError> {
        Ok(hermes_runtime_options_catalog(self.cfg.as_ref()).await)
    }

    async fn release(&mut self, reason: String) -> Result<HarnessControlOutcome, DriverError> {
        self.terminal_emitted = true;
        let wire_messages = if self.session_id.is_some() {
            Vec::new()
        } else {
            vec![WireMessage::Json(json!({
                "type": "release",
                "reason": reason,
            }))]
        };
        Ok(HarnessControlOutcome {
            events: vec![DriverEvent::RunComplete {
                summary: Some(reason.clone()),
            }],
            wire_messages,
            close: true,
            ..HarnessControlOutcome::default()
        })
    }

    fn ignores_stderr_line(&self, line: &str) -> bool {
        is_hermes_info_log_line(line)
    }

    fn terminal_emitted(&self) -> bool {
        self.terminal_emitted
    }

    fn ws_connect_errors_emit_to_stream(&self) -> bool {
        true
    }
}

impl HermesAdapter {
    fn acp_events_from_value(&mut self, raw: &Value) -> Option<Vec<DriverEvent>> {
        if raw.get("id").is_some() && raw.get("result").is_some() {
            return Some(Vec::new());
        }
        if raw.get("id").is_some() && raw.get("error").is_some() {
            return Some(vec![DriverEvent::DriverError {
                fatal: false,
                message: raw
                    .get("error")
                    .and_then(|error| error.get("message"))
                    .and_then(Value::as_str)
                    .unwrap_or("hermes ACP request failed")
                    .to_string(),
            }]);
        }
        if raw.get("method").and_then(Value::as_str) != Some("session/update") {
            return None;
        }
        let update = raw.get("params").and_then(|params| params.get("update"))?;
        let session_update = update
            .get("sessionUpdate")
            .and_then(Value::as_str)
            .or_else(|| update.get("session_update").and_then(Value::as_str))?;
        match session_update {
            "agent_message_chunk" => Some(text_chunk_from_acp_update(
                update,
                TextStream::Assistant,
                &mut self.seq,
            )),
            "agent_thought_chunk" => Some(text_chunk_from_acp_update(
                update,
                TextStream::System,
                &mut self.seq,
            )),
            "user_message_chunk" => Some(text_chunk_from_acp_update(
                update,
                TextStream::User,
                &mut self.seq,
            )),
            "tool_call" => Some(vec![DriverEvent::ToolCall {
                call_id: string_field(update, &["toolCallId", "tool_call_id", "id"])
                    .unwrap_or_else(|| format!("hermes-tool-{}", uuid::Uuid::new_v4())),
                name: string_field(update, &["title", "name"]).unwrap_or_else(|| "tool".into()),
                args: update
                    .get("rawInput")
                    .or_else(|| update.get("raw_input"))
                    .cloned()
                    .unwrap_or(Value::Null),
                seq: self.take_seq(),
            }]),
            "tool_call_update" => Some(vec![DriverEvent::ToolResult {
                call_id: string_field(update, &["toolCallId", "tool_call_id", "id"])
                    .unwrap_or_default(),
                ok: update
                    .get("status")
                    .and_then(Value::as_str)
                    .map(|status| status != "failed")
                    .unwrap_or(true),
                output: update
                    .get("rawOutput")
                    .or_else(|| update.get("raw_output"))
                    .or_else(|| update.get("content"))
                    .cloned()
                    .unwrap_or(Value::Null),
                seq: self.take_seq(),
            }]),
            _ => Some(Vec::new()),
        }
    }
}

fn configured_endpoint(cfg: &HermesConfig) -> Option<&str> {
    cfg.endpoint.as_deref().filter(|s| !s.is_empty())
}

fn websocket_endpoint(endpoint: &str) -> Result<String, DriverError> {
    if endpoint.starts_with("ws://") || endpoint.starts_with("wss://") {
        return Ok(endpoint.to_string());
    }
    if let Some(rest) = endpoint.strip_prefix("http://") {
        return Ok(format!("ws://{rest}"));
    }
    if let Some(rest) = endpoint.strip_prefix("https://") {
        return Ok(format!("wss://{rest}"));
    }
    Err(DriverError::InvalidConfig(format!(
        "endpoint must start with ws://, wss://, http://, or https://: {endpoint}"
    )))
}

fn spawn_payload(ctx: &DriverContext, cfg: &HermesConfig) -> Value {
    json!({
        "type": "spawn",
        "protocol_version": "hermes/1",
        "transport": TRANSPORT,
        "prompt_bundle_text": cfg.prompt_bundle_text,
        "provider": cfg.provider,
        "model": cfg.model,
        "reasoning_effort": cfg.reasoning_effort,
        "speed": cfg.speed,
        "identity": ctx.identity,
        "kind": ctx.run_kind,
        "task_id": ctx.task_id,
        "worker_id": ctx.worker_id,
        "project_id": ctx.project_id,
        "worktree": ctx.worktree,
        "babysitter_target": ctx.babysitter_target,
    })
}

fn hermes_command_message(session_id: Option<&str>, command: &str) -> WireMessage {
    if let Some(session_id) = session_id {
        WireMessage::JsonRpc {
            method: "session/prompt".into(),
            params: hermes_prompt_params(session_id, command),
        }
    } else {
        WireMessage::Json(json!({
            "type": "user_input",
            "input": command,
        }))
    }
}

fn hermes_prompt_params(session_id: &str, text: &str) -> Value {
    json!({
        "sessionId": session_id,
        "prompt": [{
            "type": "text",
            "text": text,
        }],
    })
}

async fn hermes_runtime_options_catalog(cfg: Option<&HermesConfig>) -> RuntimeOptionsCatalog {
    let current = hermes_current_state(cfg);
    match load_hermes_inventory_payload().await {
        Ok(payload) => hermes_catalog_from_payload(&payload, &current, "hermes:inventory"),
        Err(_) => hermes_unavailable_catalog(current),
    }
}

async fn load_hermes_inventory_payload() -> Result<Value, DriverError> {
    let script = r#"
import json
from hermes_cli.inventory import build_models_payload, load_picker_context
payload = build_models_payload(
    load_picker_context(),
    picker_hints=True,
    canonical_order=True,
    capabilities=True,
    max_models=50,
)
print(json.dumps(payload))
"#;
    let mut last_error = "no Python candidates".to_string();
    for python in hermes_python_candidates() {
        if python.is_absolute() && !python.exists() {
            continue;
        }
        let output = match timeout(
            HERMES_INVENTORY_TIMEOUT,
            Command::new(&python).arg("-c").arg(script).output(),
        )
        .await
        {
            Err(_) => {
                last_error = format!("{} timed out", python.display());
                continue;
            }
            Ok(Err(err)) => {
                last_error = format!("{}: {err}", python.display());
                continue;
            }
            Ok(Ok(output)) => output,
        };
        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
            last_error = if stderr.is_empty() {
                format!("{} exited with {}", python.display(), output.status)
            } else {
                format!("{}: {stderr}", python.display())
            };
            continue;
        }
        return serde_json::from_slice(&output.stdout)
            .map_err(|err| DriverError::Transport(format!("Hermes inventory JSON: {err}")));
    }
    Err(DriverError::Transport(format!(
        "Hermes inventory unavailable: {last_error}"
    )))
}

fn hermes_python_candidates() -> Vec<PathBuf> {
    let mut candidates = Vec::new();
    if let Some(path) = std::env::var_os("HERMES_PYTHON") {
        push_python_candidate(&mut candidates, PathBuf::from(path));
    }
    if let Some(home) = std::env::var_os("HERMES_HOME") {
        let home = PathBuf::from(home);
        push_python_candidate(
            &mut candidates,
            home.join("hermes-agent").join("venv/bin/python"),
        );
        push_python_candidate(&mut candidates, home.join("venv/bin/python"));
    }
    if let Some(home) = std::env::var_os("HOME") {
        push_python_candidate(
            &mut candidates,
            PathBuf::from(home)
                .join(".hermes")
                .join("hermes-agent")
                .join("venv/bin/python"),
        );
    }
    push_python_candidate(&mut candidates, PathBuf::from("python3"));
    candidates
}

fn push_python_candidate(candidates: &mut Vec<PathBuf>, path: PathBuf) {
    if !candidates.iter().any(|candidate| candidate == &path) {
        candidates.push(path);
    }
}

fn hermes_catalog_from_payload(
    payload: &Value,
    current: &RuntimeOptionsState,
    source: &str,
) -> RuntimeOptionsCatalog {
    let current = RuntimeOptionsState {
        provider: current
            .provider
            .clone()
            .or_else(|| string_field(payload, &["provider"])),
        model: current
            .model
            .clone()
            .or_else(|| string_field(payload, &["model"])),
        reasoning_effort: current.reasoning_effort.clone(),
        speed: current.speed,
    };
    let providers = payload
        .get("providers")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|row| hermes_provider_option(row, &current))
        .collect::<Vec<_>>();
    let models = providers
        .iter()
        .flat_map(|provider| provider.models.iter().cloned())
        .collect::<Vec<_>>();
    RuntimeOptionsCatalog {
        source: source.into(),
        provider_switching: true,
        live_switching: true,
        current,
        efforts: aggregate_model_efforts(&models),
        speeds: aggregate_model_speeds(&models),
        providers,
        models,
    }
}

fn hermes_unavailable_catalog(current: RuntimeOptionsState) -> RuntimeOptionsCatalog {
    RuntimeOptionsCatalog {
        source: "hermes:unavailable".into(),
        provider_switching: true,
        live_switching: false,
        current,
        providers: Vec::new(),
        models: Vec::new(),
        efforts: Vec::new(),
        speeds: vec![RuntimeSpeed::Normal],
    }
}

fn hermes_current_state(cfg: Option<&HermesConfig>) -> RuntimeOptionsState {
    RuntimeOptionsState {
        provider: cfg.and_then(|cfg| cfg.provider.clone()),
        model: cfg.and_then(|cfg| cfg.model.clone()),
        reasoning_effort: cfg.and_then(|cfg| cfg.reasoning_effort.clone()),
        speed: cfg.and_then(|cfg| cfg.speed),
    }
}

fn hermes_provider_option(
    row: &Value,
    current: &RuntimeOptionsState,
) -> Option<RuntimeProviderOption> {
    if row.get("authenticated").and_then(Value::as_bool) == Some(false) {
        return None;
    }
    let id = string_field(row, &["slug"])?;
    let label = string_field(row, &["name"]).unwrap_or_else(|| id.clone());
    let capabilities = row.get("capabilities");
    let provider_current = current.provider.as_deref() == Some(id.as_str())
        || row
            .get("is_current")
            .and_then(Value::as_bool)
            .unwrap_or(false);
    let models = row
        .get("models")
        .and_then(Value::as_array)?
        .iter()
        .filter_map(Value::as_str)
        .map(str::trim)
        .filter(|model| !model.is_empty())
        .map(|model| hermes_model_option(&id, model, capabilities, current, provider_current))
        .collect::<Vec<_>>();
    if models.is_empty() {
        return None;
    }
    Some(RuntimeProviderOption {
        id,
        label,
        current: provider_current,
        authenticated: row.get("authenticated").and_then(Value::as_bool),
        models,
    })
}

fn hermes_model_option(
    provider: &str,
    model: &str,
    capabilities: Option<&Value>,
    current: &RuntimeOptionsState,
    provider_current: bool,
) -> RuntimeModelOption {
    let caps = capabilities.and_then(|caps| caps.get(model));
    let fast = caps
        .and_then(|caps| caps.get("fast"))
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let reasoning = caps
        .and_then(|caps| caps.get("reasoning"))
        .and_then(Value::as_bool)
        .unwrap_or(true);
    let mut speeds = vec![RuntimeSpeed::Normal];
    if fast {
        speeds.push(RuntimeSpeed::Fast);
    }
    let reasoning_efforts = if reasoning {
        caps.and_then(|caps| caps.get("reasoning_efforts"))
            .and_then(Value::as_array)
            .map(|entries| {
                entries
                    .iter()
                    .filter_map(Value::as_str)
                    .map(str::to_string)
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default()
    } else {
        vec!["none".into()]
    };
    RuntimeModelOption {
        id: model.into(),
        label: model.into(),
        provider: Some(provider.into()),
        current: provider_current && current.model.as_deref() == Some(model),
        reasoning_efforts,
        speeds,
        default_reasoning_effort: None,
    }
}

fn aggregate_model_efforts(models: &[RuntimeModelOption]) -> Vec<String> {
    dedupe_non_empty(
        models
            .iter()
            .flat_map(|model| model.reasoning_efforts.iter().cloned()),
    )
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

fn slash_arg(value: &str) -> String {
    if value
        .chars()
        .all(|ch| ch.is_ascii_alphanumeric() || matches!(ch, '-' | '_' | '.' | ':' | '/'))
    {
        value.to_string()
    } else {
        format!("{value:?}")
    }
}

fn is_hermes_info_log_line(line: &str) -> bool {
    let is_info_or_debug = line.contains(" [INFO] ") || line.contains(" [DEBUG] ");
    if !is_info_or_debug {
        return false;
    }
    [" acp_adapter.", " run_agent:", " agent."]
        .iter()
        .any(|logger| line.contains(logger))
}

fn text_chunk_from_acp_update(
    update: &Value,
    stream: TextStream,
    seq: &mut u64,
) -> Vec<DriverEvent> {
    let Some(text) = update
        .get("content")
        .and_then(|content| content.get("text"))
        .and_then(Value::as_str)
        .filter(|text| !text.is_empty())
    else {
        return Vec::new();
    };
    let n = *seq;
    *seq = seq.saturating_add(1);
    vec![DriverEvent::TextChunk {
        stream,
        chunk: text.to_string(),
        seq: n,
    }]
}

fn hermes_event_from_value(value: Value, seq: &mut u64) -> Result<DriverEvent, DriverError> {
    let ty = value
        .get("type")
        .and_then(Value::as_str)
        .ok_or_else(|| DriverError::Transport("hermes event missing type".into()))?;
    match ty {
        "session.ready" | "hermes.ready" => Ok(DriverEvent::Ready {
            protocol_version: string_field(&value, &["protocol_version"])
                .unwrap_or_else(|| "hermes/1".to_string()),
            capabilities: value
                .get("capabilities")
                .cloned()
                .unwrap_or_else(|| Value::Object(Default::default())),
        }),
        "text" | "text_delta" | "assistant_text" => Ok(DriverEvent::TextChunk {
            stream: text_stream_field(&value).unwrap_or(TextStream::Assistant),
            chunk: string_field(&value, &["chunk", "text", "content", "delta"]).unwrap_or_default(),
            seq: seq_field(&value, seq),
        }),
        "tool_call" | "tool.use" => Ok(DriverEvent::ToolCall {
            call_id: string_field(&value, &["call_id", "id"])
                .unwrap_or_else(|| format!("hermes-call-{}", uuid::Uuid::new_v4())),
            name: string_field(&value, &["name", "tool"]).unwrap_or_default(),
            args: value
                .get("args")
                .or_else(|| value.get("input"))
                .cloned()
                .unwrap_or_else(|| Value::Object(Default::default())),
            seq: seq_field(&value, seq),
        }),
        "tool_result" | "tool.result" => Ok(DriverEvent::ToolResult {
            call_id: string_field(&value, &["call_id", "id"]).unwrap_or_default(),
            ok: value.get("ok").and_then(Value::as_bool).unwrap_or(true),
            output: value
                .get("output")
                .or_else(|| value.get("result"))
                .cloned()
                .unwrap_or(Value::Null),
            seq: seq_field(&value, seq),
        }),
        "run_complete" | "session.complete" | "complete" => Ok(DriverEvent::RunComplete {
            summary: string_field(&value, &["summary", "message"]),
        }),
        "run_fail" | "session.fail" | "fail" => Ok(DriverEvent::RunFail {
            error_code: string_field(&value, &["error_code", "code"])
                .unwrap_or_else(|| "hermes_fail".to_string()),
            error_markdown: string_field(&value, &["error_markdown", "message", "error"])
                .unwrap_or_default(),
        }),
        "driver_error" | "error" => Ok(DriverEvent::DriverError {
            fatal: value.get("fatal").and_then(Value::as_bool).unwrap_or(false),
            message: string_field(&value, &["message", "error"]).unwrap_or_default(),
        }),
        other => Err(DriverError::Transport(format!(
            "unknown hermes event type: {other}"
        ))),
    }
}

fn string_field(value: &Value, keys: &[&str]) -> Option<String> {
    keys.iter().find_map(|key| {
        value
            .get(*key)
            .and_then(Value::as_str)
            .map(ToString::to_string)
    })
}

fn text_stream_field(value: &Value) -> Option<TextStream> {
    value
        .get("stream")
        .cloned()
        .and_then(|stream| serde_json::from_value(stream).ok())
}

fn seq_field(value: &Value, seq: &mut u64) -> u64 {
    if let Some(n) = value.get("seq").and_then(Value::as_u64) {
        *seq = n.saturating_add(1);
        n
    } else {
        let n = *seq;
        *seq = seq.saturating_add(1);
        n
    }
}

pub fn simulated_config() -> DriverConfig {
    DriverConfig::from_value(Value::Object(Default::default()))
}

fn simulated_start_events(ctx: &DriverContext, cfg: &HermesConfig) -> Vec<DriverEvent> {
    vec![DriverEvent::Ready {
        protocol_version: "hermes/1".into(),
        capabilities: json!({
            "simulated": true,
            "kind": ctx.run_kind,
            "provider": cfg.provider,
            "model": cfg.model,
            "reasoning_effort": cfg.reasoning_effort,
            "speed": cfg.speed,
        }),
    }]
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{driver_for_mode_harness, HermesDriver, WorkerDriver};
    use futures::{SinkExt, StreamExt};
    use orgasmic_core::RuntimeIdentity;
    use std::sync::{Arc, Mutex};
    use tokio::net::TcpListener;
    use tokio::time::{timeout, Duration};
    use tokio_tungstenite::{
        accept_async, accept_hdr_async,
        tungstenite::{
            handshake::server::{Request, Response},
            Message,
        },
    };

    const REAL_HERMES_FIXTURE: [&str; 3] = [
        r#"{"type":"text","stream":"assistant","text":"fixture hello"}"#,
        r#"{"type":"tool_call","call_id":"call-1","name":"transition_state","args":{"from":"implementing","to":"done","reason":"fixture"},"seq":7}"#,
        r#"{"type":"run_complete","summary":"fixture complete"}"#,
    ];

    fn ctx(id: &str, kind: RunKind) -> DriverContext {
        DriverContext {
            identity: RuntimeIdentity::new(id, "boot-test"),
            run_kind: kind,
            task_id: "TASK-006".into(),
            worker_id: "implementer-hermes".into(),
            project_id: None,
            worktree: None,
            babysitter_target: None,
        }
    }

    #[tokio::test]
    async fn simulated_acquire_emits_ready() {
        let d = driver_for_mode_harness("acp-ws", "hermes").expect("acp-ws hermes driver");
        let mut s = d
            .acquire(ctx("run-h", RunKind::Worker), simulated_config())
            .await
            .unwrap();
        let ev = s.events.recv().await.unwrap();
        assert!(matches!(ev, DriverEvent::Ready { .. }));
        s.control.release("done").await.unwrap();
    }

    #[tokio::test]
    async fn simulated_send_input_emits_user_text() {
        let d = driver_for_mode_harness("acp-ws", "hermes").expect("acp-ws hermes driver");
        let mut s = d
            .acquire(ctx("run-h-input", RunKind::Worker), simulated_config())
            .await
            .unwrap();
        let ready = s.events.recv().await.unwrap();
        assert!(matches!(ready, DriverEvent::Ready { .. }));

        let ack = s
            .control
            .send_input(crate::r#trait::UserInputRequest {
                input: "  fixture user input  ".into(),
            })
            .await
            .unwrap();
        assert!(ack.accepted);

        let event = s.events.recv().await.unwrap();
        assert_eq!(
            event,
            DriverEvent::TextChunk {
                stream: TextStream::User,
                chunk: "fixture user input".into(),
                seq: 0,
            }
        );
        s.control.release("done").await.unwrap();
    }

    #[test]
    fn endpoint_without_token_is_rejected() {
        let d = HermesDriver;
        let cfg = DriverConfig::from_value(json!({"endpoint": "https://hermes.local"}));
        assert!(d.validate(&cfg).is_err());
    }

    #[test]
    fn hermes_stdio_session_init_uses_standard_acp_methods_shape() {
        let mut adapter = HermesAdapter::new();
        let init = adapter
            .stdio_session_init(&ctx("run-h-stdio", RunKind::Worker), &DriverConfig::empty())
            .unwrap();

        assert_eq!(init["initialize"]["protocolVersion"], 1);
        assert_eq!(init["thread_start"]["cwd"], ".");
        assert_eq!(init["thread_start"]["mcpServers"], json!([]));
        assert_eq!(adapter.jsonrpc_session_start_method(), "session/new");
        assert_eq!(adapter.jsonrpc_turn_start_method(), "session/prompt");
    }

    #[test]
    fn hermes_stderr_filter_drops_adapter_info_logs() {
        let adapter = HermesAdapter::new();
        assert!(adapter.ignores_stderr_line(
            "2026-06-10 09:02:44 [INFO] acp_adapter.entry: Starting hermes-agent ACP adapter"
        ));
        assert!(adapter.ignores_stderr_line(
            "2026-06-10 09:02:47 [INFO] agent.auxiliary_client: Vision auto-detect: using main provider openai-codex (gpt-5.5)"
        ));
        assert!(adapter.ignores_stderr_line(
            "2026-06-10 09:02:45 [INFO] run_agent: OpenAI client created provider=openai-codex model=gpt-5.5"
        ));
        assert!(adapter.ignores_stderr_line(
            "2026-06-10 09:42:20 [INFO] agent.turn_context: conversation turn: session=99b30e17-fc4e-495c-a674-c7fdcb005e5a model=gpt-5.5 provider=openai-codex platform=acp history=0 msg='hi'"
        ));
        assert!(adapter.ignores_stderr_line(
            "2026-06-10 09:42:23 [INFO] agent.conversation_loop: Turn ended: reason=text_response(finish_reason=stop) model=gpt-5.5"
        ));
    }

    #[test]
    fn hermes_stderr_filter_keeps_warnings_and_plain_text() {
        let adapter = HermesAdapter::new();
        assert!(!adapter
            .ignores_stderr_line("2026-06-10 09:02:44 [WARN] acp_adapter.server: reconnecting"));
        assert!(!adapter.ignores_stderr_line("plain stderr from harness"));
    }

    #[tokio::test]
    async fn runtime_options_send_slash_commands_for_acp_session() {
        let mut adapter = HermesAdapter::new();
        adapter
            .stdio_session_init(
                &ctx("run-h-runtime", RunKind::Worker),
                &DriverConfig::from_value(json!({
                    "provider": "old-provider",
                    "model": "old-model",
                })),
            )
            .unwrap();
        adapter
            .on_ws_thread_started("stdio", &json!({"sessionId": "session-fixture"}))
            .await
            .unwrap();

        let outcome = adapter
            .switch_runtime_options(RuntimeOptionsRequest {
                provider: Some("openai-codex".into()),
                model: Some("gpt-5.5".into()),
                reasoning_effort: Some("low".into()),
                speed: Some(RuntimeSpeed::Fast),
            })
            .await
            .unwrap();

        let prompts = outcome
            .wire_messages
            .iter()
            .map(|message| match message {
                WireMessage::JsonRpc { method, params } => {
                    assert_eq!(method, "session/prompt");
                    params["prompt"][0]["text"].as_str().unwrap().to_string()
                }
                other => panic!("expected JSON-RPC prompt, got {other:?}"),
            })
            .collect::<Vec<_>>();
        assert_eq!(
            prompts,
            vec![
                "/model gpt-5.5 --provider openai-codex",
                "/reasoning low",
                "/fast fast",
            ]
        );
    }

    #[test]
    fn runtime_options_catalog_uses_inventory_capabilities() {
        let current = RuntimeOptionsState {
            provider: Some("openai-codex".into()),
            model: Some("gpt-5.5".into()),
            reasoning_effort: Some("high".into()),
            speed: Some(RuntimeSpeed::Fast),
        };
        let catalog = hermes_catalog_from_payload(
            &json!({
                "provider": "openai-codex",
                "model": "gpt-5.5",
                "providers": [
                    {
                        "slug": "openai-codex",
                        "name": "OpenAI Codex",
                        "authenticated": true,
                        "is_current": true,
                        "models": ["gpt-5.5", "gpt-basic"],
                        "capabilities": {
                            "gpt-5.5": {"fast": true, "reasoning": true},
                            "gpt-basic": {"fast": false, "reasoning": false}
                        }
                    },
                    {
                        "slug": "unauthenticated",
                        "name": "Unauthenticated",
                        "authenticated": false,
                        "models": ["not-selectable"]
                    }
                ]
            }),
            &current,
            "hermes:test",
        );

        assert!(catalog.provider_switching);
        assert_eq!(catalog.providers.len(), 1);
        assert_eq!(catalog.providers[0].id, "openai-codex");
        assert!(catalog.providers[0].current);
        assert_eq!(catalog.models.len(), 2);
        assert!(catalog.models[0].current);
        assert_eq!(
            catalog.models[0].speeds,
            vec![RuntimeSpeed::Normal, RuntimeSpeed::Fast]
        );
        assert_eq!(catalog.models[1].speeds, vec![RuntimeSpeed::Normal]);
        assert_eq!(catalog.models[1].reasoning_efforts, vec!["none"]);
    }

    #[tokio::test]
    async fn hermes_acp_session_update_maps_text_chunk() {
        let mut adapter = HermesAdapter::new();
        let events = adapter
            .parse_event(json!({
                "jsonrpc": "2.0",
                "method": "session/update",
                "params": {
                    "sessionId": "session-fixture",
                    "update": {
                        "sessionUpdate": "agent_message_chunk",
                        "content": {
                            "type": "text",
                            "text": "hello from acp"
                        }
                    }
                }
            }))
            .await;

        assert_eq!(
            events,
            vec![DriverEvent::TextChunk {
                stream: TextStream::Assistant,
                chunk: "hello from acp".into(),
                seq: 0,
            }]
        );
    }

    #[allow(clippy::result_large_err)]
    #[tokio::test]
    async fn real_hermes_websocket_fixture_roundtrip() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let seen_token = Arc::new(Mutex::new(None));
        let server_token = Arc::clone(&seen_token);

        let server = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let callback_token = Arc::clone(&server_token);
            let callback = move |req: &Request, response: Response| {
                let token = req
                    .headers()
                    .get("session_token")
                    .and_then(|v| v.to_str().ok())
                    .map(ToString::to_string);
                *callback_token.lock().unwrap() = token;
                Ok(response)
            };
            let mut ws = accept_hdr_async(stream, callback).await.unwrap();
            let spawn_text = ws.next().await.unwrap().unwrap().into_text().unwrap();
            let spawn: Value = serde_json::from_str(&spawn_text).unwrap();
            for line in REAL_HERMES_FIXTURE {
                ws.send(Message::Text(line.to_string())).await.unwrap();
            }
            spawn
        });

        let d = driver_for_mode_harness("acp-ws", "hermes").expect("acp-ws hermes driver");
        let cfg = DriverConfig::from_value(json!({
            "endpoint": format!("ws://{addr}/acp"),
            "session_token": "fixture-token",
        }));
        let mut s = d
            .acquire(ctx("run-h-real", RunKind::Worker), cfg)
            .await
            .unwrap();

        let ready = s.events.recv().await.unwrap();
        let DriverEvent::Ready {
            protocol_version,
            capabilities,
        } = ready
        else {
            panic!("expected Ready");
        };
        assert_eq!(protocol_version, "hermes/1");
        assert_eq!(capabilities["simulated"], false);
        assert_eq!(capabilities["wire"], "websocket");

        let text = s.events.recv().await.unwrap();
        assert!(matches!(
            text,
            DriverEvent::TextChunk {
                stream: TextStream::Assistant,
                ref chunk,
                seq: 0,
            } if chunk == "fixture hello"
        ));
        let tool = s.events.recv().await.unwrap();
        assert!(matches!(
            tool,
            DriverEvent::ToolCall {
                ref call_id,
                ref name,
                seq: 7,
                ..
            } if call_id == "call-1" && name == "transition_state"
        ));
        let done = s.events.recv().await.unwrap();
        assert!(matches!(
            done,
            DriverEvent::RunComplete { summary: Some(ref s) } if s == "fixture complete"
        ));

        let spawn = server.await.unwrap();
        assert_eq!(
            *seen_token.lock().unwrap(),
            Some("fixture-token".to_string())
        );
        assert_eq!(spawn["type"], "spawn");
        assert_eq!(spawn["transport"], "hermes");
        assert_eq!(spawn["kind"], "worker");
        assert_eq!(spawn["task_id"], "TASK-006");
        assert!(spawn.get("continuation").is_none() || spawn["continuation"].is_null());
    }

    #[allow(clippy::result_large_err)]
    #[tokio::test]
    async fn real_hermes_websocket_send_input_writes_user_frame() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();

        let server = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let mut ws = accept_async(stream).await.unwrap();
            let spawn_text = ws.next().await.unwrap().unwrap().into_text().unwrap();
            let spawn: Value = serde_json::from_str(&spawn_text).unwrap();
            let input_text = ws.next().await.unwrap().unwrap().into_text().unwrap();
            let input: Value = serde_json::from_str(&input_text).unwrap();
            (spawn, input)
        });

        let d = driver_for_mode_harness("acp-ws", "hermes").expect("acp-ws hermes driver");
        let cfg = DriverConfig::from_value(json!({
            "endpoint": format!("ws://{addr}/acp"),
            "session_token": "fixture-token",
        }));
        let mut s = d
            .acquire(ctx("run-h-input-real", RunKind::Worker), cfg)
            .await
            .unwrap();

        let ready = timeout(Duration::from_secs(2), s.events.recv())
            .await
            .unwrap()
            .unwrap();
        assert!(matches!(
            ready,
            DriverEvent::Ready {
                protocol_version,
                ..
            } if protocol_version == "hermes/1"
        ));

        let ack = s
            .control
            .send_input(crate::r#trait::UserInputRequest {
                input: "fixture live input".into(),
            })
            .await
            .unwrap();
        assert!(ack.accepted);

        let event = s.events.recv().await.unwrap();
        assert_eq!(
            event,
            DriverEvent::TextChunk {
                stream: TextStream::User,
                chunk: "fixture live input".into(),
                seq: 0,
            }
        );

        let (spawn, input) = timeout(Duration::from_secs(2), server)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(spawn["type"], "spawn");
        assert_eq!(input["type"], "user_input");
        assert_eq!(input["input"], "fixture live input");
    }

    #[test]
    fn transport_name_is_stable() {
        assert_eq!(HermesDriver.transport(), "hermes");
    }
}
