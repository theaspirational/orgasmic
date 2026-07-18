// orgasmic:task_GYMSP, arch_A53QX, dec_ASB1A
//! Cursor Agent ACP stdio adapter (`cursor-agent acp`).
//!
//! This runs alongside the legacy `cursor-agent --print --sandbox enabled`
//! stream-json path in `cursor.rs`. ACP exposes no `--sandbox` flag: filesystem
//! isolation is therefore delegated to ACP's pre-execution
//! `session/request_permission` gate. Do not remove the legacy path here; the
//! deprecation decision is intentionally deferred until the ACP path has live
//! mileage.

use std::collections::{BTreeMap, HashSet};
use std::path::{Path, PathBuf};

use async_trait::async_trait;
use serde::Deserialize;
use serde_json::{json, Value};
use tokio::sync::mpsc;

use orgasmic_core::{BabysitterTool, DriverEvent, SandboxAllowlist, TextStream};

use crate::r#trait::{
    implementer_tool_is_allowed, BabysitterRequest, DriverConfig, DriverContext, DriverError,
    HarnessControlOutcome, HarnessEventAdapter, HarnessRequest, RunKind, StdioSpawn,
    TransitionRequest, UserInputRequest, WireMessage,
};
use crate::runtime_options::{
    RuntimeModelOption, RuntimeOptionsCatalog, RuntimeOptionsRequest, RuntimeOptionsState,
};
use crate::sandbox::ApprovalResponse;

const HARNESS: &str = "cursor-agent";
// orgasmic:TASK-SZEWA, dec_WDR5K — no orgasmic-owned model default.

pub struct CursorAcpAdapter {
    ctx: Option<DriverContext>,
    cfg: Option<CursorAcpConfig>,
    seq: u64,
    session_id: Option<String>,
    terminal_emitted: bool,
    active_tools: HashSet<String>,
    /// Structured model catalog from the live `session/new` JSON-RPC result.
    runtime_catalog: Option<RuntimeOptionsCatalog>,
}

impl CursorAcpAdapter {
    pub fn new() -> Self {
        Self {
            ctx: None,
            cfg: None,
            seq: 0,
            session_id: None,
            terminal_emitted: false,
            active_tools: HashSet::new(),
            runtime_catalog: None,
        }
    }

    async fn collect<F>(&mut self, f: F) -> Vec<DriverEvent>
    where
        F: for<'a> FnOnce(
            &'a mut CursorAcpAdapter,
            mpsc::Sender<DriverEvent>,
        )
            -> std::pin::Pin<Box<dyn std::future::Future<Output = ()> + Send + 'a>>,
    {
        let (tx, mut rx) = mpsc::channel(32);
        f(self, tx.clone()).await;
        drop(tx);
        let mut events = Vec::new();
        while let Ok(event) = rx.try_recv() {
            events.push(event);
        }
        events
    }

    fn cfg_model(&self) -> Option<String> {
        self.cfg
            .as_ref()
            .and_then(|cfg| cfg.model.clone())
            .map(|model| model.trim().to_string())
            .filter(|model| !model.is_empty())
    }

    fn worktree(&self) -> Option<PathBuf> {
        self.ctx.as_ref().and_then(|ctx| ctx.worktree.clone())
    }

    fn project_root(&self) -> Option<PathBuf> {
        self.cfg
            .as_ref()
            .and_then(|cfg| cfg.project_root.clone())
            .or_else(|| {
                self.worktree()
                    .and_then(|p| p.parent().map(Path::to_path_buf))
            })
    }
}

impl Default for CursorAcpAdapter {
    fn default() -> Self {
        Self::new()
    }
}

#[derive(Debug, Clone, Deserialize, Default)]
struct CursorAcpConfig {
    #[serde(default)]
    model: Option<String>,
    #[serde(default)]
    api_key_env: Option<String>,
    #[serde(default)]
    prompt_bundle_text: Option<String>,
    #[serde(default)]
    project_root: Option<PathBuf>,
    #[serde(default = "default_auto_start_turn")]
    auto_start_turn: bool,
}

fn default_auto_start_turn() -> bool {
    true
}

#[async_trait]
impl HarnessEventAdapter for CursorAcpAdapter {
    fn harness(&self) -> &'static str {
        HARNESS
    }

    fn clone_box(&self) -> Box<dyn HarnessEventAdapter> {
        Box::new(CursorAcpAdapter {
            ctx: self.ctx.clone(),
            cfg: self.cfg.clone(),
            seq: self.seq,
            session_id: self.session_id.clone(),
            terminal_emitted: self.terminal_emitted,
            active_tools: self.active_tools.clone(),
            runtime_catalog: self.runtime_catalog.clone(),
        })
    }

    fn validate_config(&self, config: &DriverConfig) -> Result<(), DriverError> {
        let cfg: CursorAcpConfig = serde_json::from_value(config.0.clone())
            .map_err(|e| DriverError::InvalidConfig(e.to_string()))?;
        if let Some(env_name) = cfg.api_key_env.as_deref() {
            if std::env::var(env_name).is_err() {
                return Err(DriverError::InvalidConfig(format!(
                    "api_key_env '{env_name}' not set for cursor-agent ACP"
                )));
            }
        }
        Ok(())
    }

    fn stdio_spawn(&self) -> Option<StdioSpawn> {
        Some(StdioSpawn {
            command: "cursor-agent".into(),
            args: vec!["acp".into()],
            cwd: None,
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
        let cfg: CursorAcpConfig = serde_json::from_value(config.0.clone())
            .map_err(|e| DriverError::InvalidConfig(e.to_string()))?;
        self.validate_config(config)?;
        self.ctx = Some(ctx.clone());
        self.cfg = Some(cfg.clone());
        let mut post_session = Vec::new();
        // Explicit model override only — never invent a default (dec_WDR5K item 9).
        if let Some(model) = cfg
            .model
            .as_deref()
            .map(str::trim)
            .filter(|m| !m.is_empty())
        {
            post_session.push(json!({
                "method": "session/set_config_option",
                "params": {
                    "configId": "model",
                    "value": model,
                }
            }));
        }
        Ok(json!({
            "initialize": initialize_params(),
            "thread_start": session_new_params(ctx),
            "post_session": post_session,
            "auto_turn": cfg.auto_start_turn,
        }))
    }

    fn compose_request(
        &mut self,
        ctx: &DriverContext,
        config: &DriverConfig,
    ) -> Result<HarnessRequest, DriverError> {
        let cfg: CursorAcpConfig = serde_json::from_value(config.0.clone())
            .map_err(|e| DriverError::InvalidConfig(e.to_string()))?;
        self.validate_config(config)?;
        self.ctx = Some(ctx.clone());
        self.cfg = Some(cfg.clone());
        let mut env: BTreeMap<String, String> = BTreeMap::new();
        if let Some(env_name) = cfg.api_key_env.as_deref() {
            let api_key = std::env::var(env_name).map_err(|_| {
                DriverError::InvalidConfig(format!(
                    "api_key_env '{env_name}' not set for cursor-agent ACP"
                ))
            })?;
            env.insert("CURSOR_API_KEY".into(), api_key);
        }
        // acp-stdio upgrades this simulated request to `cursor-agent acp` when
        // the binary is present; otherwise this keeps CI deterministic.
        Ok(HarnessRequest::Simulated {
            events: vec![DriverEvent::Ready {
                protocol_version: "cursor-acp/1".into(),
                capabilities: json!({
                    "simulated": true,
                    "kind": ctx.run_kind,
                    "model": cfg.model,
                    "env": env.keys().cloned().collect::<Vec<_>>(),
                }),
            }],
        })
    }

    fn jsonrpc_session_start_method(&self) -> &'static str {
        "session/new"
    }

    fn jsonrpc_turn_start_method(&self) -> &'static str {
        "session/prompt"
    }

    async fn on_ws_thread_started(
        &mut self,
        endpoint: &str,
        thread_response: &Value,
    ) -> Result<Vec<DriverEvent>, DriverError> {
        let session_id = thread_response
            .get("sessionId")
            .or_else(|| thread_response.get("session_id"))
            .and_then(Value::as_str)
            .ok_or_else(|| DriverError::Transport("session/new response missing sessionId".into()))?
            .to_string();
        self.session_id = Some(session_id.clone());
        // Live cursor-agent ACP returns a structured models block on session/new
        // (not documented as a discovery method; verified against the binary).
        self.runtime_catalog =
            catalog_from_session_new(thread_response, self.cfg_model().as_deref());
        let ctx = self
            .ctx
            .as_ref()
            .ok_or_else(|| DriverError::Other("cursor ACP ctx missing".into()))?;
        Ok(vec![DriverEvent::Ready {
            protocol_version: "cursor-acp/1".into(),
            capabilities: json!({
                "simulated": false,
                "kind": ctx.run_kind,
                "wire": "stdio-jsonrpc",
                "endpoint": endpoint,
                "session_id": session_id,
                "model": self.cfg_model(),
                "catalog_source": self.runtime_catalog.as_ref().map(|c| c.source.clone()),
            }),
        }])
    }

    fn ws_turn_start_params(&mut self) -> Result<Value, DriverError> {
        let session_id = self
            .session_id
            .as_deref()
            .ok_or_else(|| DriverError::Transport("cursor ACP session missing".into()))?;
        let cfg = self
            .cfg
            .as_ref()
            .ok_or_else(|| DriverError::Other("cursor ACP config missing".into()))?;
        Ok(session_prompt_params(session_id, cfg))
    }

    async fn on_ws_response(
        &mut self,
        method: &str,
        response: Value,
    ) -> Result<Vec<DriverEvent>, DriverError> {
        if method == "session/prompt" {
            Ok(self.stop_response_events(&response))
        } else {
            Ok(Vec::new())
        }
    }

    async fn try_handle_approval(
        &mut self,
        method: &str,
        params: &Value,
        allowlist: &SandboxAllowlist,
    ) -> Option<ApprovalResponse> {
        if method != "session/request_permission" {
            return None;
        }
        let allowed = self.permission_allowed(params, allowlist);
        Some(ApprovalResponse::Selected {
            option_id: if allowed { "allow_once" } else { "reject_once" }.into(),
        })
    }

    async fn parse_event(&mut self, raw: Value) -> Vec<DriverEvent> {
        self.collect(|adapter, tx| {
            Box::pin(async move {
                adapter.translate_value(&tx, &raw).await;
            })
        })
        .await
    }

    async fn transition_state(
        &mut self,
        req: TransitionRequest,
    ) -> Result<HarnessControlOutcome, DriverError> {
        let text = format!(
            "orgasmic transition_state request\nfrom: {}\nto: {}\nreason: {}",
            req.from, req.to, req.reason
        );
        Ok(HarnessControlOutcome {
            events: vec![DriverEvent::TransitionState {
                from: req.from,
                to: req.to,
                reason: req.reason,
            }],
            wire_messages: self.prompt_messages(&text),
            ..HarnessControlOutcome::default()
        })
    }

    async fn babysitter_action(
        &mut self,
        req: BabysitterRequest,
    ) -> Result<HarnessControlOutcome, DriverError> {
        let text = format!(
            "orgasmic babysitter action\nrun: {}\ntool: {}\npayload: {}",
            req.target_run,
            req.tool.as_str(),
            req.payload
        );
        Ok(HarnessControlOutcome {
            events: vec![DriverEvent::ToolCall {
                call_id: format!("cursor-acp-bs-{}", uuid::Uuid::new_v4()),
                name: req.tool.as_str().into(),
                args: req.payload,
                seq: self.next_seq(),
            }],
            wire_messages: self.prompt_messages(&text),
            ..HarnessControlOutcome::default()
        })
    }

    async fn send_input(
        &mut self,
        req: UserInputRequest,
    ) -> Result<HarnessControlOutcome, DriverError> {
        Ok(HarnessControlOutcome {
            events: vec![DriverEvent::TextChunk {
                stream: TextStream::User,
                chunk: req.input.clone(),
                seq: self.next_seq(),
            }],
            wire_messages: self.prompt_messages(&req.input),
            ..HarnessControlOutcome::default()
        })
    }

    async fn switch_runtime_options(
        &mut self,
        req: RuntimeOptionsRequest,
    ) -> Result<HarnessControlOutcome, DriverError> {
        let session_id = self
            .session_id
            .clone()
            .ok_or_else(|| DriverError::Transport("cursor ACP session missing".into()))?;
        let model = req
            .model
            .as_deref()
            .map(str::trim)
            .filter(|m| !m.is_empty())
            .ok_or_else(|| DriverError::Unsupported("cursor ACP runtime options require model"))?
            .to_string();
        if let Some(cfg) = self.cfg.as_mut() {
            cfg.model = Some(model.clone());
        }
        if let Some(catalog) = self.runtime_catalog.as_mut() {
            catalog.current.model = Some(model.clone());
            for entry in &mut catalog.models {
                entry.current = entry.id == model;
            }
        }
        Ok(HarnessControlOutcome {
            events: vec![DriverEvent::TextChunk {
                stream: TextStream::System,
                chunk: format!("runtime options updated for cursor ACP session: model={model}"),
                seq: self.next_seq(),
            }],
            wire_messages: vec![WireMessage::JsonRpc {
                method: "session/set_config_option".into(),
                params: json!({
                    "sessionId": session_id,
                    "configId": "model",
                    "value": model,
                }),
            }],
            ..HarnessControlOutcome::default()
        })
    }

    async fn runtime_options_catalog(&mut self) -> Result<RuntimeOptionsCatalog, DriverError> {
        self.runtime_catalog.clone().ok_or_else(|| {
            DriverError::Unsupported("cursor ACP runtime catalog unavailable before session/new")
        })
    }

    async fn release(&mut self, reason: String) -> Result<HarnessControlOutcome, DriverError> {
        self.terminal_emitted = true;
        let mut wire_messages = Vec::new();
        if let Some(session_id) = self.session_id.as_deref() {
            wire_messages.push(WireMessage::Json(json!({
                "jsonrpc": "2.0",
                "method": "session/cancel",
                "params": { "sessionId": session_id },
            })));
        }
        Ok(HarnessControlOutcome {
            events: vec![DriverEvent::RunComplete {
                summary: Some(reason),
            }],
            wire_messages,
            close: true,
            ..HarnessControlOutcome::default()
        })
    }

    fn terminal_emitted(&self) -> bool {
        self.terminal_emitted
    }

    fn next_seq(&mut self) -> u64 {
        let seq = self.seq;
        self.seq += 1;
        seq
    }
}

impl CursorAcpAdapter {
    async fn translate_value(&mut self, events: &mpsc::Sender<DriverEvent>, raw: &Value) {
        if raw.get("id").is_some() && raw.get("result").is_some() {
            return;
        }
        if raw.get("id").is_some() && raw.get("error").is_some() {
            let message = raw
                .get("error")
                .and_then(|e| e.get("message"))
                .and_then(Value::as_str)
                .unwrap_or("cursor ACP request failed")
                .to_string();
            let _ = events
                .send(DriverEvent::DriverError {
                    fatal: false,
                    message,
                })
                .await;
            return;
        }
        if raw.get("method").and_then(Value::as_str) != Some("session/update") {
            return;
        }
        let update = raw
            .get("params")
            .and_then(|params| params.get("update"))
            .unwrap_or(&Value::Null);
        match session_update_kind(update) {
            Some("plan") => self.translate_plan(events, update).await,
            Some("agent_message_chunk") => self.translate_message_chunk(events, update).await,
            Some("tool_call") => self.translate_tool_call(events, update).await,
            Some("tool_call_update") => self.translate_tool_call_update(events, update).await,
            Some("agent_thought_chunk") | Some("user_message_chunk") => {
                self.translate_message_chunk(events, update).await
            }
            _ => {
                let _ = events
                    .send(DriverEvent::TextChunk {
                        stream: TextStream::System,
                        chunk: format!("cursor ACP session/update: {update}"),
                        seq: self.next_seq(),
                    })
                    .await;
            }
        }
    }

    async fn translate_plan(&mut self, events: &mpsc::Sender<DriverEvent>, update: &Value) {
        let plan = update
            .get("plan")
            .or_else(|| update.get("entries"))
            .or_else(|| update.get("content"))
            .cloned()
            .unwrap_or_else(|| update.clone());
        let _ = events
            .send(DriverEvent::TextChunk {
                stream: TextStream::System,
                chunk: format!("cursor ACP plan: {plan}"),
                seq: self.next_seq(),
            })
            .await;
    }

    async fn translate_message_chunk(
        &mut self,
        events: &mpsc::Sender<DriverEvent>,
        update: &Value,
    ) {
        let text = update_text(update);
        if !text.is_empty() {
            let stream = match session_update_kind(update) {
                Some("user_message_chunk") => TextStream::User,
                Some("agent_thought_chunk") => TextStream::System,
                _ => TextStream::Assistant,
            };
            let _ = events
                .send(DriverEvent::TextChunk {
                    stream,
                    chunk: text,
                    seq: self.next_seq(),
                })
                .await;
        }
    }

    async fn translate_tool_call(&mut self, events: &mpsc::Sender<DriverEvent>, update: &Value) {
        let tool = tool_payload(update);
        let call_id = tool_call_id(tool);
        let name = tool_call_name(tool);
        let args = tool_call_args(tool);
        self.active_tools.insert(call_id.clone());
        let _ = events
            .send(DriverEvent::ToolCall {
                call_id,
                name,
                args,
                seq: self.next_seq(),
            })
            .await;
    }

    async fn translate_tool_call_update(
        &mut self,
        events: &mpsc::Sender<DriverEvent>,
        update: &Value,
    ) {
        let tool = tool_payload(update);
        let call_id = tool_call_id(tool);
        let status = tool_status(tool);
        let terminal = matches!(
            status.as_deref(),
            Some("completed" | "failed" | "cancelled")
        );
        if terminal {
            self.active_tools.remove(&call_id);
            let ok = status.as_deref() == Some("completed");
            let _ = events
                .send(DriverEvent::ToolResult {
                    call_id,
                    ok,
                    output: tool.clone(),
                    seq: self.next_seq(),
                })
                .await;
        } else {
            let _ = events
                .send(DriverEvent::TextChunk {
                    stream: TextStream::System,
                    chunk: format!(
                        "cursor ACP tool {call_id} status: {}",
                        status.unwrap_or_else(|| "unknown".into())
                    ),
                    seq: self.next_seq(),
                })
                .await;
        }
    }

    fn stop_response_events(&mut self, response: &Value) -> Vec<DriverEvent> {
        let reason = response
            .get("stopReason")
            .or_else(|| response.get("stop_reason"))
            .and_then(Value::as_str)
            .unwrap_or("end_turn");
        match reason {
            "end_turn" | "max_tokens" => {
                self.terminal_emitted = true;
                vec![DriverEvent::RunComplete {
                    summary: response
                        .get("summary")
                        .and_then(Value::as_str)
                        .map(ToString::to_string),
                }]
            }
            "cancelled" => {
                self.terminal_emitted = true;
                vec![DriverEvent::RunComplete {
                    summary: Some("cursor ACP turn cancelled".into()),
                }]
            }
            "refusal" | "max_turn_requests" => {
                self.terminal_emitted = true;
                // ACP StopReason::refusal and ::max_turn_requests are terminal
                // non-success outcomes for an orgasmic worker run: map them to
                // RunFail with a typed code so supervisors do not treat a model
                // refusal or turn-budget exhaustion as accepted completion.
                vec![DriverEvent::RunFail {
                    error_code: format!("cursor_acp_stop_reason_{reason}"),
                    error_markdown: format!(
                        "cursor ACP stopped with StopReason::{reason}: {response}"
                    ),
                }]
            }
            other => vec![DriverEvent::DriverError {
                fatal: false,
                message: format!("cursor ACP unknown stopReason {other}: {response}"),
            }],
        }
    }

    fn prompt_messages(&self, text: &str) -> Vec<WireMessage> {
        self.session_id
            .as_deref()
            .map(|session_id| {
                vec![WireMessage::JsonRpc {
                    method: "session/prompt".into(),
                    params: session_prompt_text_params(session_id, text),
                }]
            })
            .unwrap_or_default()
    }

    fn permission_allowed(&self, params: &Value, allowlist: &SandboxAllowlist) -> bool {
        let kind = params
            .get("kind")
            .or_else(|| params.pointer("/toolCall/kind"))
            .or_else(|| params.pointer("/tool_call/kind"))
            .and_then(Value::as_str)
            .unwrap_or("other");
        if !permission_kind_allowed(kind, allowlist) {
            return false;
        }
        if matches!(kind, "edit" | "delete" | "move") {
            return self.write_permission_allowed(params, allowlist);
        }
        let name = params
            .get("toolName")
            .or_else(|| params.get("tool_name"))
            .or_else(|| params.pointer("/toolCall/title"))
            .or_else(|| params.pointer("/tool_call/title"))
            .and_then(Value::as_str)
            .unwrap_or(kind);
        let args = params
            .get("input")
            .or_else(|| params.get("args"))
            .or_else(|| params.pointer("/toolCall/input"))
            .or_else(|| params.pointer("/tool_call/input"))
            .unwrap_or(params);
        match self
            .ctx
            .as_ref()
            .map(|ctx| ctx.run_kind)
            .unwrap_or(RunKind::Worker)
        {
            RunKind::Worker => self.implementer_tool_allowed(name, kind, args),
            RunKind::Babysitter => BabysitterTool::parse(name).is_some(),
        }
    }

    fn implementer_tool_allowed(&self, name: &str, kind: &str, args: &Value) -> bool {
        if implementer_tool_is_allowed(name) {
            return true;
        }
        match normalize_tool_name(name).as_str() {
            "await" | "structured_await" => true,
            "kill" | "pkill" => true,
            "read" | "read_file" | "readfile" => self.read_args_allowed(args),
            "grep" | "grep_search" | "search" | "find" => self.search_args_allowed(args),
            "shell" | "bash" | "execute" => self.shell_command_allowed(args),
            _ => match kind {
                "read" => self.read_args_allowed(args),
                "search" => self.search_args_allowed(args),
                "execute" => self.shell_command_allowed(args),
                "think" | "fetch" | "other" => true,
                _ => false,
            },
        }
    }

    fn write_permission_allowed(&self, params: &Value, allowlist: &SandboxAllowlist) -> bool {
        if !allowlist.allow_patch {
            return false;
        }
        if allowlist.allow_writes_outside_cwd {
            return true;
        }
        let paths = schema_target_paths(
            params
                .get("input")
                .or_else(|| params.get("args"))
                .or_else(|| params.pointer("/toolCall/input"))
                .or_else(|| params.pointer("/tool_call/input"))
                .unwrap_or(params),
            &[
                "path", "file", "uri", "from", "to", "oldPath", "newPath", "old_path", "new_path",
            ],
        );
        !paths.is_empty() && paths.iter().all(|path| self.path_under_worktree(path))
    }

    fn read_args_allowed(&self, args: &Value) -> bool {
        let paths = schema_target_paths(args, &["path", "file", "uri"]);
        !paths.is_empty()
            && paths.iter().all(|path| {
                self.path_under_dispatch_artifacts(path) || self.path_under_worktree(path)
            })
    }

    fn search_args_allowed(&self, args: &Value) -> bool {
        let paths = schema_target_paths(args, &["path", "root", "directory", "cwd"]);
        !paths.is_empty() && paths.iter().all(|path| self.path_under_worktree(path))
    }

    fn shell_command_allowed(&self, args: &Value) -> bool {
        let Some(command) = shell_command_arg(args) else {
            return false;
        };
        let words = split_policy_command(command);
        if words.is_empty() {
            return false;
        }
        match words[0].as_str() {
            "kill" | "pkill" => {
                !command_has_non_redirect_shell_control(command) && kill_command_allowed(&words)
            }
            _ => {
                if command_has_shell_control(command) {
                    return false;
                }
                match words[0].as_str() {
                    "git" => self.git_c_command_allowed(&words),
                    "grep" | "rg" => self.grep_command_allowed(&words),
                    "find" => self.find_command_allowed(&words),
                    _ => false,
                }
            }
        }
    }

    fn git_c_command_allowed(&self, words: &[String]) -> bool {
        if words.len() < 4 || words.get(1).map(String::as_str) != Some("-C") {
            return false;
        }
        let Some(path) = policy_path(&words[2]) else {
            return false;
        };
        self.path_is_worktree(&path)
    }

    fn grep_command_allowed(&self, words: &[String]) -> bool {
        command_option_path(words, "--path")
            .map(|path| self.path_under_worktree(&path))
            .unwrap_or(false)
    }

    fn find_command_allowed(&self, words: &[String]) -> bool {
        if let Some(path) = command_option_path(words, "--path") {
            return self.path_under_worktree(&path);
        }
        words
            .iter()
            .skip(1)
            .find(|word| !word.starts_with('-'))
            .and_then(|word| policy_path(word))
            .map(|path| self.path_under_worktree(&path))
            .unwrap_or(false)
    }

    fn path_under_dispatch_artifacts(&self, path: &Path) -> bool {
        self.project_root()
            .map(|root| path_is_under(path, &root.join(".orgasmic/tmp/dispatch")))
            .unwrap_or(false)
    }

    fn path_is_worktree(&self, path: &Path) -> bool {
        self.worktree()
            .filter(|path| path.is_absolute())
            .map(|worktree| resolve_policy_path(path) == resolve_policy_path(&worktree))
            .unwrap_or(false)
    }

    fn path_under_worktree(&self, path: &Path) -> bool {
        self.worktree()
            .filter(|path| path.is_absolute())
            .map(|worktree| path_is_under(path, &worktree))
            .unwrap_or(false)
    }
}

fn initialize_params() -> Value {
    json!({
        "protocolVersion": 1,
        "clientCapabilities": {
            "fs": { "readTextFile": true, "writeTextFile": true },
            "terminal": true,
        },
        "clientInfo": {
            "name": "orgasmic",
            "title": "orgasmic",
            "version": env!("CARGO_PKG_VERSION"),
        },
    })
}

fn session_new_params(ctx: &DriverContext) -> Value {
    json!({
        "cwd": ctx.worktree.as_ref().map(|p| p.display().to_string()).unwrap_or_else(|| ".".into()),
        "mcpServers": [],
    })
}

/// Parse the structured `models` / `configOptions` block from a live
/// `session/new` result. Never scrapes CLI text.
fn catalog_from_session_new(
    thread_response: &Value,
    override_model: Option<&str>,
) -> Option<RuntimeOptionsCatalog> {
    let models_block = thread_response.get("models")?;
    let available = models_block
        .get("availableModels")
        .and_then(Value::as_array)?;
    let current_from_session = models_block
        .get("currentModelId")
        .and_then(Value::as_str)
        .map(str::to_string);
    let current_model = override_model
        .map(str::to_string)
        .or(current_from_session.clone());
    let mut models = Vec::new();
    for entry in available {
        let Some(id) = entry
            .get("modelId")
            .or_else(|| entry.get("id"))
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|s| !s.is_empty())
        else {
            continue;
        };
        let label = entry
            .get("name")
            .or_else(|| entry.get("label"))
            .and_then(Value::as_str)
            .unwrap_or(id)
            .to_string();
        let current = current_model
            .as_deref()
            .map(|cur| cur == id)
            .unwrap_or(false);
        models.push(RuntimeModelOption {
            id: id.to_string(),
            label,
            provider: None,
            current,
            reasoning_efforts: Vec::new(),
            speeds: Vec::new(),
            default_reasoning_effort: None,
        });
    }
    if models.is_empty() {
        return None;
    }
    Some(RuntimeOptionsCatalog {
        source: "cursor-acp:session/new".into(),
        provider_switching: false,
        live_switching: false,
        current: RuntimeOptionsState {
            provider: None,
            model: current_model,
            reasoning_effort: None,
            speed: None,
        },
        providers: Vec::new(),
        models,
        efforts: Vec::new(),
        speeds: Vec::new(),
    })
}

fn session_prompt_params(session_id: &str, cfg: &CursorAcpConfig) -> Value {
    session_prompt_text_params(session_id, cfg.prompt_bundle_text.as_deref().unwrap_or(""))
}

fn session_prompt_text_params(session_id: &str, text: &str) -> Value {
    json!({
        "sessionId": session_id,
        "prompt": [{ "type": "text", "text": text }],
    })
}

fn permission_kind_allowed(kind: &str, allowlist: &SandboxAllowlist) -> bool {
    match kind {
        "execute" => allowlist.allow_exec,
        "edit" | "delete" | "move" => allowlist.allow_patch,
        "fetch" => allowlist.allow_network,
        "read" | "search" | "think" | "other" => true,
        _ => false,
    }
}

fn session_update_kind(update: &Value) -> Option<&str> {
    update
        .get("sessionUpdate")
        .or_else(|| update.get("session_update"))
        .or_else(|| update.get("kind"))
        .or_else(|| update.get("type"))
        .and_then(Value::as_str)
}

fn update_text(update: &Value) -> String {
    update
        .get("content")
        .or_else(|| update.get("text"))
        .or_else(|| update.get("delta"))
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string()
}

fn tool_payload(update: &Value) -> &Value {
    update
        .get("toolCall")
        .or_else(|| update.get("tool_call"))
        .unwrap_or(update)
}

fn tool_call_id(tool: &Value) -> String {
    tool.get("id")
        .or_else(|| tool.get("toolCallId"))
        .or_else(|| tool.get("tool_call_id"))
        .or_else(|| tool.get("callId"))
        .or_else(|| tool.get("call_id"))
        .and_then(Value::as_str)
        .map(ToString::to_string)
        .unwrap_or_else(|| format!("cursor-acp-tool-{}", uuid::Uuid::new_v4()))
}

fn tool_call_name(tool: &Value) -> String {
    tool.get("title")
        .or_else(|| tool.get("name"))
        .or_else(|| tool.get("toolName"))
        .or_else(|| tool.get("tool_name"))
        .or_else(|| tool.get("kind"))
        .and_then(Value::as_str)
        .map(normalize_tool_name)
        .unwrap_or_else(|| "unknown".into())
}

fn tool_call_args(tool: &Value) -> Value {
    tool.get("input")
        .or_else(|| tool.get("args"))
        .cloned()
        .unwrap_or(Value::Null)
}

fn tool_status(tool: &Value) -> Option<String> {
    tool.get("status")
        .and_then(Value::as_str)
        .map(ToString::to_string)
}

fn normalize_tool_name(name: &str) -> String {
    let base = name.strip_suffix("ToolCall").unwrap_or(name);
    let mut out = String::new();
    for (idx, ch) in base.chars().enumerate() {
        if ch.is_ascii_uppercase() {
            if idx > 0 {
                out.push('_');
            }
            out.push(ch.to_ascii_lowercase());
        } else if ch == ' ' || ch == '-' {
            out.push('_');
        } else {
            out.push(ch);
        }
    }
    out
}

fn shell_command_arg(args: &Value) -> Option<&str> {
    args.as_str().or_else(|| {
        args.get("command")
            .or_else(|| args.get("cmd"))
            .or_else(|| args.get("script"))
            .and_then(Value::as_str)
    })
}

fn command_has_shell_control(command: &str) -> bool {
    command_has_non_redirect_shell_control(command)
        || command.contains('<')
        || command.contains('>')
}

fn command_has_non_redirect_shell_control(command: &str) -> bool {
    command.contains(';')
        || command.contains('|')
        || command.contains('&')
        || command.contains('\n')
        || command.contains('`')
        || command.contains("$(")
}

fn kill_command_allowed(words: &[String]) -> bool {
    words.len() > 1
        && words
            .iter()
            .skip(1)
            .all(|word| word == "2>/dev/null" || (!word.contains('<') && !word.contains('>')))
}

fn schema_target_paths(args: &Value, keys: &[&str]) -> Vec<PathBuf> {
    let mut paths = Vec::new();
    match args {
        Value::String(raw) => {
            if let Some(path) = policy_path(raw) {
                paths.push(path);
            }
        }
        Value::Object(map) => {
            for key in keys {
                if let Some(value) = map.get(*key) {
                    collect_schema_path_values(value, &mut paths);
                }
            }
        }
        _ => {}
    }
    paths
}

fn collect_schema_path_values(value: &Value, paths: &mut Vec<PathBuf>) {
    match value {
        Value::String(raw) => {
            if let Some(path) = policy_path(raw) {
                paths.push(path);
            }
        }
        Value::Array(values) => {
            for value in values {
                collect_schema_path_values(value, paths);
            }
        }
        _ => {}
    }
}

fn policy_path(raw: &str) -> Option<PathBuf> {
    let path = Path::new(raw);
    if path.is_absolute() {
        Some(path.to_path_buf())
    } else {
        None
    }
}

fn normalize_policy_path(path: &Path) -> PathBuf {
    let mut raw = path.to_string_lossy().to_string();
    if let Some(rest) = raw.strip_prefix("/private/tmp/") {
        raw = format!("/tmp/{rest}");
    } else if raw == "/private/tmp" {
        raw = "/tmp".into();
    } else if let Some(rest) = raw.strip_prefix("/private/var/") {
        raw = format!("/var/{rest}");
    } else if raw == "/private/var" {
        raw = "/var".into();
    }

    let mut normalized = PathBuf::new();
    for component in Path::new(&raw).components() {
        match component {
            std::path::Component::CurDir => {}
            std::path::Component::ParentDir => {
                normalized.pop();
            }
            _ => normalized.push(component.as_os_str()),
        }
    }
    normalized
}

fn resolve_policy_path(path: &Path) -> PathBuf {
    if let Ok(canonical) = path.canonicalize() {
        return normalize_policy_path(&canonical);
    }
    if let (Some(parent), Some(file_name)) = (path.parent(), path.file_name()) {
        if parent != path {
            let mut resolved = resolve_policy_path(parent);
            resolved.push(file_name);
            return normalize_policy_path(&resolved);
        }
    }
    normalize_policy_path(path)
}

fn path_is_under(path: &Path, root: &Path) -> bool {
    let path = resolve_policy_path(path);
    let root = resolve_policy_path(root);
    path == root || path.starts_with(root)
}

fn split_policy_command(command: &str) -> Vec<String> {
    let mut words = Vec::new();
    let mut current = String::new();
    let mut quote = None;
    let mut escaped = false;

    for ch in command.chars() {
        if escaped {
            current.push(ch);
            escaped = false;
            continue;
        }
        if ch == '\\' {
            escaped = true;
            continue;
        }
        if let Some(q) = quote {
            if ch == q {
                quote = None;
            } else {
                current.push(ch);
            }
            continue;
        }
        match ch {
            '\'' | '"' => quote = Some(ch),
            ch if ch.is_whitespace() => {
                if !current.is_empty() {
                    words.push(current);
                    current = String::new();
                }
            }
            _ => current.push(ch),
        }
    }
    if escaped {
        current.push('\\');
    }
    if !current.is_empty() {
        words.push(current);
    }
    words
}

fn command_option_path(words: &[String], option: &str) -> Option<PathBuf> {
    for idx in 1..words.len() {
        let word = &words[idx];
        if word == option {
            return words.get(idx + 1).and_then(|value| policy_path(value));
        }
        let prefix = format!("{option}=");
        if let Some(value) = word.strip_prefix(&prefix) {
            return policy_path(value);
        }
    }
    None
}

pub fn simulated_config() -> DriverConfig {
    DriverConfig::from_value(Value::Object(Default::default()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use orgasmic_core::RuntimeIdentity;

    fn ctx() -> DriverContext {
        DriverContext {
            identity: RuntimeIdentity::new("run-cursor-acp", "boot-test"),
            run_kind: RunKind::Worker,
            task_id: "TASK-GYMSP".into(),
            worker_id: "implementer-composer-acp".into(),
            project_id: Some("orgasmic".into()),
            worktree: Some(std::env::current_dir().unwrap()),
            babysitter_target: None,
        }
    }

    #[test]
    fn session_init_envelope_uses_acp_methods_shape() {
        let mut adapter = CursorAcpAdapter::new();
        let init = adapter
            .stdio_session_init(&ctx(), &DriverConfig::empty())
            .unwrap();
        assert_eq!(init["initialize"]["protocolVersion"], 1);
        assert_eq!(init["initialize"]["clientCapabilities"]["terminal"], true);
        assert_eq!(init["thread_start"]["mcpServers"], json!([]));
        assert_eq!(init["post_session"], json!([]));
        assert_eq!(adapter.jsonrpc_session_start_method(), "session/new");
        assert_eq!(adapter.jsonrpc_turn_start_method(), "session/prompt");
    }

    #[test]
    fn session_init_posts_set_config_only_for_explicit_model() {
        let mut adapter = CursorAcpAdapter::new();
        let init = adapter
            .stdio_session_init(
                &ctx(),
                &DriverConfig::from_value(json!({ "model": "fixture-model" })),
            )
            .unwrap();
        assert_eq!(
            init["post_session"],
            json!([{
                "method": "session/set_config_option",
                "params": { "configId": "model", "value": "fixture-model" }
            }])
        );
    }

    #[tokio::test]
    async fn session_new_models_block_becomes_runtime_catalog() {
        let mut adapter = CursorAcpAdapter::new();
        adapter.ctx = Some(ctx());
        let events = adapter
            .on_ws_thread_started(
                "stdio",
                &json!({
                    "sessionId": "sess-1",
                    "models": {
                        "currentModelId": "fixture-a",
                        "availableModels": [
                            { "modelId": "fixture-a", "name": "A" },
                            { "modelId": "fixture-b", "name": "B" }
                        ]
                    }
                }),
            )
            .await
            .unwrap();
        assert!(matches!(events[0], DriverEvent::Ready { .. }));
        let catalog = adapter.runtime_options_catalog().await.unwrap();
        assert_eq!(catalog.source, "cursor-acp:session/new");
        assert_eq!(catalog.models.len(), 2);
        assert_eq!(catalog.current.model.as_deref(), Some("fixture-a"));
    }

    #[tokio::test]
    async fn tool_call_event_translation() {
        let mut adapter = CursorAcpAdapter::new();
        adapter.ctx = Some(ctx());
        let events = adapter
            .parse_event(json!({
                "jsonrpc": "2.0",
                "method": "session/update",
                "params": {"update": {
                    "sessionUpdate": "tool_call",
                    "toolCall": {"id": "tool-1", "kind": "execute", "title": "execute", "input": {"command": "git -C /tmp status"}}
                }}
            }))
            .await;
        assert!(
            matches!(&events[0], DriverEvent::ToolCall { call_id, name, .. } if call_id == "tool-1" && name == "execute")
        );
    }

    #[tokio::test]
    async fn tool_call_update_status_transitions() {
        let mut adapter = CursorAcpAdapter::new();
        let progress = adapter
            .parse_event(json!({
                "method": "session/update",
                "params": {"update": {"sessionUpdate": "tool_call_update", "toolCall": {"id": "tool-1", "status": "in_progress"}}}
            }))
            .await;
        assert!(matches!(
            &progress[0],
            DriverEvent::TextChunk {
                stream: TextStream::System,
                ..
            }
        ));
        let completed = adapter
            .parse_event(json!({
                "method": "session/update",
                "params": {"update": {"sessionUpdate": "tool_call_update", "toolCall": {"id": "tool-1", "status": "completed", "content": "ok"}}}
            }))
            .await;
        assert!(
            matches!(&completed[0], DriverEvent::ToolResult { call_id, ok: true, .. } if call_id == "tool-1")
        );
    }

    #[tokio::test]
    async fn permission_request_allow_and_reject_paths() {
        let mut adapter = CursorAcpAdapter::new();
        adapter.ctx = Some(ctx());
        let cwd = std::env::current_dir().unwrap();
        let allow = adapter
            .try_handle_approval(
                "session/request_permission",
                &json!({"kind": "read", "input": {"path": cwd.display().to_string()}}),
                &SandboxAllowlist::default(),
            )
            .await;
        assert_eq!(
            allow,
            Some(ApprovalResponse::Selected {
                option_id: "allow_once".into()
            })
        );
        let reject = adapter
            .try_handle_approval(
                "session/request_permission",
                &json!({"kind": "execute", "input": {"command": "rm -rf /"}}),
                &SandboxAllowlist {
                    allow_exec: false,
                    ..SandboxAllowlist::default()
                },
            )
            .await;
        assert_eq!(
            reject,
            Some(ApprovalResponse::Selected {
                option_id: "reject_once".into()
            })
        );
    }

    #[tokio::test]
    async fn edit_permission_allows_worktree_but_rejects_outside_when_outside_writes_disabled() {
        let mut adapter = CursorAcpAdapter::new();
        adapter.ctx = Some(ctx());
        let worktree = std::env::current_dir().unwrap();
        let restricted = SandboxAllowlist {
            allow_writes_outside_cwd: false,
            ..SandboxAllowlist::default()
        };
        let allow = adapter
            .try_handle_approval(
                "session/request_permission",
                &json!({"kind": "edit", "input": {"path": worktree.join("src/lib.rs").display().to_string()}}),
                &restricted,
            )
            .await;
        assert_eq!(
            allow,
            Some(ApprovalResponse::Selected {
                option_id: "allow_once".into()
            })
        );
        let reject = adapter
            .try_handle_approval(
                "session/request_permission",
                &json!({"kind": "edit", "input": {"path": "/tmp/outside.rs"}}),
                &restricted,
            )
            .await;
        assert_eq!(
            reject,
            Some(ApprovalResponse::Selected {
                option_id: "reject_once".into()
            })
        );
    }

    #[tokio::test]
    async fn stop_reason_refusal_and_max_turn_requests_fail() {
        let mut adapter = CursorAcpAdapter::new();
        for reason in ["refusal", "max_turn_requests"] {
            let events = adapter
                .on_ws_response("session/prompt", json!({"stopReason": reason}))
                .await
                .unwrap();
            assert!(
                matches!(&events[0], DriverEvent::RunFail { error_code, .. } if error_code == &format!("cursor_acp_stop_reason_{reason}"))
            );
            adapter.terminal_emitted = false;
        }
    }
}
