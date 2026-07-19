// arch: arch_A53QX.3
// orgasmic:arch_A53QX, dec_ASB1A
//! Cursor Agent harness adapter for the `cursor-agent` CLI stream-json bridge.
//!
//! Cursor Agent's headless wire is:
//! `cursor-agent --print --output-format stream-json --stream-partial-output`.
//! The subprocess-stream-json mode writes the orgasmic prompt bundle to stdin and translates each
//! JSON line from stdout into standard [`orgasmic_core::DriverEvent`] values.
//!
//! As with the other production drivers, an unconfigured endpoint keeps a
//! deterministic simulated mode for CI and supervisor smoke tests. Configure
//! `endpoint = "stdio"` to request the real CLI bridge.

use std::collections::{BTreeMap, HashSet};
use std::path::{Path, PathBuf};
use std::process::{Command as StdCommand, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use async_trait::async_trait;
use serde::Deserialize;
use serde_json::{json, Value};
use tokio::sync::mpsc;

use orgasmic_core::{BabysitterTool, DriverEvent, TextStream};

use crate::r#trait::{
    implementer_tool_is_allowed, BabysitterRequest, DriverConfig, DriverContext, DriverError,
    HarnessControlOutcome, HarnessEventAdapter, HarnessRequest, RunKind, TransitionRequest,
};

const TRANSPORT: &str = "cursor-agent";
// orgasmic:TASK-SZEWA, dec_WDR5K — no orgasmic-owned model default; omit --model
// when dispatch does not pass an explicit override (harness default applies).
const DEFAULT_SANDBOX: &str = "enabled";
/// Trailing system chunks kept for synthetic subprocess-exit summaries.
pub(crate) const SUBPROCESS_EXIT_SYSTEM_TAIL_CHUNKS: usize = 8;

/// Build a subprocess-exit summary: full assistant text when present, otherwise
/// the trailing system-chunk tail (not the full thinking stream).
pub(crate) fn distill_subprocess_exit_summary(
    assistant_text: &str,
    system_chunks: &[String],
) -> Option<String> {
    if !assistant_text.is_empty() {
        return Some(assistant_text.to_string());
    }
    if system_chunks.is_empty() {
        return None;
    }
    let start = system_chunks
        .len()
        .saturating_sub(SUBPROCESS_EXIT_SYSTEM_TAIL_CHUNKS);
    let summary = system_chunks[start..].concat();
    if summary.is_empty() {
        None
    } else {
        Some(summary)
    }
}

pub struct CursorAdapter {
    translator: Option<CursorAgentTranslator>,
    terminal_emitted: Arc<AtomicBool>,
}

impl CursorAdapter {
    pub fn new() -> Self {
        Self {
            translator: None,
            terminal_emitted: Arc::new(AtomicBool::new(false)),
        }
    }

    fn set_translator(&mut self, cfg: &CursorAgentConfig, ctx: &DriverContext) {
        let terminal_emitted = Arc::new(AtomicBool::new(false));
        self.terminal_emitted = terminal_emitted.clone();
        self.translator = Some(CursorAgentTranslator::new(
            ctx.run_kind,
            ctx.worktree.clone(),
            cfg,
            terminal_emitted,
        ));
    }

    async fn collect<F>(&mut self, f: F) -> Vec<DriverEvent>
    where
        F: for<'a> FnOnce(
            &'a mut CursorAgentTranslator,
            mpsc::Sender<DriverEvent>,
        )
            -> std::pin::Pin<Box<dyn std::future::Future<Output = ()> + Send + 'a>>,
    {
        let Some(translator) = self.translator.as_mut() else {
            return Vec::new();
        };
        let (tx, mut rx) = mpsc::channel(32);
        f(translator, tx.clone()).await;
        drop(tx);
        let mut events = Vec::new();
        while let Ok(event) = rx.try_recv() {
            events.push(event);
        }
        events
    }
}

impl Default for CursorAdapter {
    fn default() -> Self {
        Self::new()
    }
}

#[derive(Debug, Clone, Deserialize, Default)]
struct CursorAgentConfig {
    #[serde(default)]
    endpoint: Option<String>,
    #[serde(default)]
    model: Option<String>,
    #[serde(default)]
    api_key_env: Option<String>,
    #[serde(default)]
    sandbox: Option<String>,
    #[serde(default)]
    force: Option<bool>,
    #[serde(default)]
    prompt_bundle_text: Option<String>,
    #[serde(default)]
    project_root: Option<PathBuf>,
}

impl CursorAgentConfig {
    fn configured_endpoint(&self) -> Option<&str> {
        self.endpoint.as_deref().filter(|value| !value.is_empty())
    }

    fn model(&self) -> Option<&str> {
        self.model
            .as_deref()
            .map(str::trim)
            .filter(|value| !value.is_empty())
    }

    fn sandbox(&self) -> &str {
        self.sandbox
            .as_deref()
            .filter(|value| !value.is_empty())
            .unwrap_or(DEFAULT_SANDBOX)
    }

    fn force(&self) -> bool {
        self.force.unwrap_or(true)
    }
}

#[async_trait]
impl HarnessEventAdapter for CursorAdapter {
    fn harness(&self) -> &'static str {
        TRANSPORT
    }

    fn clone_box(&self) -> Box<dyn HarnessEventAdapter> {
        Box::new(CursorAdapter::new())
    }

    async fn parse_event(&mut self, raw: Value) -> Vec<DriverEvent> {
        self.collect(|translator, tx| {
            Box::pin(async move {
                translator.translate_value(&tx, &raw).await;
            })
        })
        .await
    }

    async fn parse_stdout_line(&mut self, line: &str) -> Vec<DriverEvent> {
        let line = line.to_string();
        self.collect(|translator, tx| {
            Box::pin(async move {
                translator.translate_stdout_line(&tx, &line).await;
            })
        })
        .await
    }

    fn next_seq(&mut self) -> u64 {
        self.translator
            .as_mut()
            .map(CursorAgentTranslator::next_seq)
            .unwrap_or(0)
    }

    fn terminal_emitted(&self) -> bool {
        self.terminal_emitted.load(Ordering::SeqCst)
    }

    async fn emit_run_complete_once(
        &mut self,
        events: &mpsc::Sender<DriverEvent>,
        summary: Option<String>,
    ) {
        if self.terminal_emitted.swap(true, Ordering::SeqCst) {
            return;
        }
        let _ = events.send(DriverEvent::RunComplete { summary }).await;
    }

    fn validate_config(&self, config: &DriverConfig) -> Result<(), DriverError> {
        let cfg: CursorAgentConfig = serde_json::from_value(config.0.clone())
            .map_err(|e| DriverError::InvalidConfig(e.to_string()))?;
        if let Some(env_name) = cfg.api_key_env.as_deref() {
            if std::env::var(env_name).is_err() && cfg.endpoint.is_some() {
                return Err(DriverError::InvalidConfig(format!(
                    "api_key_env '{env_name}' not set but endpoint is configured"
                )));
            }
        }
        match cfg.sandbox() {
            "enabled" | "disabled" => {}
            value => {
                return Err(DriverError::InvalidConfig(format!(
                    "sandbox must be enabled or disabled: {value}"
                )));
            }
        }
        if let Some(endpoint) = cfg.configured_endpoint() {
            if endpoint != "stdio" {
                return Err(DriverError::InvalidConfig(format!(
                    "endpoint must be 'stdio' or empty: {endpoint}"
                )));
            }
        }
        Ok(())
    }

    fn compose_request(
        &mut self,
        ctx: &DriverContext,
        config: &DriverConfig,
    ) -> Result<HarnessRequest, DriverError> {
        let cfg: CursorAgentConfig = serde_json::from_value(config.0.clone())
            .map_err(|e| DriverError::InvalidConfig(e.to_string()))?;
        self.validate_config(config)?;
        self.set_translator(&cfg, ctx);
        let simulated = cfg.configured_endpoint().is_none() || !cursor_agent_available();
        if simulated {
            self.terminal_emitted.store(true, Ordering::SeqCst);
            return Ok(HarnessRequest::Simulated {
                events: simulated_start_events(ctx, &cfg),
            });
        }
        let mut args = vec![
            "--print".to_string(),
            "--output-format".to_string(),
            "stream-json".to_string(),
            "--stream-partial-output".to_string(),
            "--sandbox".to_string(),
            cfg.sandbox().to_string(),
        ];
        if let Some(model) = cfg.model() {
            args.push("--model".to_string());
            args.push(model.to_string());
        }
        if cfg.force() {
            args.push("--force".to_string());
        }
        let mut env = BTreeMap::new();
        if let Some(env_name) = cfg.api_key_env.as_deref() {
            let api_key = std::env::var(env_name).map_err(|_| {
                DriverError::InvalidConfig(format!(
                    "api_key_env '{env_name}' not set but endpoint is configured"
                ))
            })?;
            env.insert("CURSOR_API_KEY".into(), api_key);
        }
        Ok(HarnessRequest::Subprocess {
            binary: "cursor-agent".into(),
            args,
            env,
            cwd: ctx.worktree.clone(),
            stdin_payload: Some(build_spawn_prompt(ctx, &cfg).into_bytes()),
            close_stdin: true,
        })
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
            call_id: format!("cursor-bs-{}", uuid::Uuid::new_v4()),
            name: req.tool.as_str().into(),
            args: req.payload,
            seq: self.next_seq(),
        }))
    }

    async fn release(&mut self, reason: String) -> Result<HarnessControlOutcome, DriverError> {
        let _ = reason;
        Ok(HarnessControlOutcome {
            close: true,
            ..HarnessControlOutcome::default()
        })
    }
}

fn cursor_agent_available() -> bool {
    StdCommand::new("which")
        .arg("cursor-agent")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map(|status| status.success())
        .unwrap_or(false)
}

fn build_spawn_prompt(ctx: &DriverContext, cfg: &CursorAgentConfig) -> String {
    let payload = json!({
        "transport": TRANSPORT,
        "wire": "cursor-agent-stdio-stream-json",
        "endpoint": cfg.endpoint,
        "model": cfg.model(),
        "sandbox": cfg.sandbox(),
        "force": cfg.force(),
        "run": {
            "kind": ctx.run_kind,
            "task_id": ctx.task_id,
            "worker_id": ctx.worker_id,
            "project_id": ctx.project_id,
            "worktree": ctx.worktree.as_ref().map(|p| p.display().to_string()),
            "babysitter_target": ctx.babysitter_target,
        }
    });
    let pretty = serde_json::to_string_pretty(&payload).unwrap_or_else(|_| payload.to_string());
    let mut prompt = String::new();
    if let Some(bundle) = cfg.prompt_bundle_text.as_deref() {
        if !bundle.trim().is_empty() {
            prompt.push_str(bundle.trim());
            prompt.push_str("\n\n");
        }
    }
    prompt.push_str(
        "orgasmic runtime context follows. Treat this as runtime metadata for the worker run.\n\n```json\n",
    );
    prompt.push_str(&pretty);
    prompt.push_str("\n```\n");
    prompt
}

fn simulated_start_events(ctx: &DriverContext, cfg: &CursorAgentConfig) -> Vec<DriverEvent> {
    let mut events = vec![
        DriverEvent::Ready {
            protocol_version: "cursor-agent-stream-json/1".into(),
            capabilities: json!({
                "simulated": true,
                "kind": ctx.run_kind,
                "model": cfg.model(),
                "endpoint": cfg.endpoint,
                "sandbox": cfg.sandbox(),
                "force": cfg.force(),
            }),
        },
        DriverEvent::TextChunk {
            stream: TextStream::System,
            chunk: format!(
                "cursor-agent simulated run task={} template={}",
                ctx.task_id, ctx.worker_id
            ),
            seq: 0,
        },
    ];
    events.push(DriverEvent::TextChunk {
        stream: TextStream::Assistant,
        chunk: "cursor-agent simulated response".into(),
        seq: 2,
    });
    events.push(DriverEvent::AgentTurnComplete { seq: 3 });
    events.push(DriverEvent::RunComplete {
        summary: Some("cursor-agent simulated complete".into()),
    });
    events
}

struct CursorAgentTranslator {
    seq: u64,
    endpoint: Option<String>,
    kind: RunKind,
    worktree: Option<PathBuf>,
    configured_model: Option<String>,
    sandbox: Option<String>,
    force: Option<bool>,
    project_root: Option<PathBuf>,
    terminal_emitted: Arc<AtomicBool>,
    allowed_tool_calls: HashSet<String>,
    assistant_text_so_far: String,
    last_assistant_text: Option<String>,
}

impl CursorAgentTranslator {
    fn new(
        kind: RunKind,
        worktree: Option<PathBuf>,
        cfg: &CursorAgentConfig,
        terminal_emitted: Arc<AtomicBool>,
    ) -> Self {
        Self {
            seq: 0,
            endpoint: cfg.endpoint.clone(),
            kind,
            worktree,
            configured_model: cfg.model.clone(),
            sandbox: cfg.sandbox.clone(),
            force: cfg.force,
            project_root: cfg.project_root.clone(),
            terminal_emitted,
            allowed_tool_calls: HashSet::new(),
            assistant_text_so_far: String::new(),
            last_assistant_text: None,
        }
    }

    fn next_seq(&mut self) -> u64 {
        let seq = self.seq;
        self.seq += 1;
        seq
    }

    async fn translate_stdout_line(&mut self, events: &mpsc::Sender<DriverEvent>, line: &str) {
        match serde_json::from_str::<Value>(line) {
            Ok(value) => self.translate_value(events, &value).await,
            Err(_) => {
                let _ = events
                    .send(DriverEvent::TextChunk {
                        stream: TextStream::Stdout,
                        chunk: line.to_string(),
                        seq: self.next_seq(),
                    })
                    .await;
            }
        }
    }

    async fn translate_value(&mut self, events: &mpsc::Sender<DriverEvent>, value: &Value) {
        match value.get("type").and_then(Value::as_str) {
            Some("system") => self.translate_system(events, value).await,
            Some("assistant") => self.translate_assistant(events, value).await,
            Some("text") => self.translate_text_event(events, value).await,
            Some("tool_use") => self.translate_tool_use(events, value).await,
            Some("tool_call") => self.translate_tool_call(events, value).await,
            Some("result") => self.translate_result(events, value).await,
            Some("error") => self.translate_error(events, value).await,
            Some("thinking") => self.translate_thinking(events, value).await,
            Some("user") => {}
            Some(other) => {
                let _ = events
                    .send(DriverEvent::TextChunk {
                        stream: TextStream::System,
                        chunk: format!("cursor-agent event {other}: {value}"),
                        seq: self.next_seq(),
                    })
                    .await;
            }
            None => {
                let _ = events
                    .send(DriverEvent::TextChunk {
                        stream: TextStream::System,
                        chunk: format!("cursor-agent event: {value}"),
                        seq: self.next_seq(),
                    })
                    .await;
            }
        }
    }

    async fn translate_system(&mut self, events: &mpsc::Sender<DriverEvent>, value: &Value) {
        match value.get("subtype").and_then(Value::as_str) {
            Some("init") => {
                let observed_model = value
                    .get("model")
                    .and_then(Value::as_str)
                    .or(self.configured_model.as_deref());
                let _ = events
                    .send(DriverEvent::Ready {
                        protocol_version: "cursor-agent-stream-json/1".into(),
                        capabilities: json!({
                            "simulated": false,
                            "kind": self.kind,
                            "wire": "stdio-stream-json",
                            "endpoint": self.endpoint,
                            "model": observed_model,
                            "sandbox": self.sandbox.as_deref().unwrap_or(DEFAULT_SANDBOX),
                            "force": self.force.unwrap_or(true),
                            "session_id": value.get("session_id").cloned().unwrap_or(Value::Null),
                            "api_key_source": value.get("apiKeySource").cloned().unwrap_or(Value::Null),
                            "permission_mode": value.get("permissionMode").cloned().unwrap_or(Value::Null),
                        }),
                    })
                    .await;
            }
            Some(subtype) => {
                let _ = events
                    .send(DriverEvent::TextChunk {
                        stream: TextStream::System,
                        chunk: format!("cursor-agent system {subtype}: {value}"),
                        seq: self.next_seq(),
                    })
                    .await;
            }
            None => {
                let _ = events
                    .send(DriverEvent::TextChunk {
                        stream: TextStream::System,
                        chunk: format!("cursor-agent system: {value}"),
                        seq: self.next_seq(),
                    })
                    .await;
            }
        }
    }

    async fn translate_assistant(&mut self, events: &mpsc::Sender<DriverEvent>, value: &Value) {
        if let Some(content) = value.pointer("/message/content") {
            self.translate_message_content(events, content).await;
        }
        if let Some(error) = value.get("error").and_then(Value::as_str) {
            let _ = events
                .send(DriverEvent::DriverError {
                    fatal: true,
                    message: format!("cursor-agent assistant error: {error}"),
                })
                .await;
        }
    }

    async fn translate_message_content(
        &mut self,
        events: &mpsc::Sender<DriverEvent>,
        content: &Value,
    ) {
        if let Some(text) = content.as_str() {
            self.emit_assistant_text(events, text).await;
            return;
        }
        let Some(items) = content.as_array() else {
            return;
        };
        for item in items {
            match item.get("type").and_then(Value::as_str) {
                Some("text") => {
                    if let Some(text) = item.get("text").and_then(Value::as_str) {
                        self.emit_assistant_text(events, text).await;
                    }
                }
                Some("tool_use") => {
                    self.translate_tool_use(events, item).await;
                }
                _ => {}
            }
        }
    }

    async fn translate_text_event(&mut self, events: &mpsc::Sender<DriverEvent>, value: &Value) {
        if let Some(text) = value
            .get("content")
            .or_else(|| value.get("text"))
            .and_then(Value::as_str)
        {
            self.emit_assistant_text(events, text).await;
        }
    }

    async fn emit_assistant_text(&mut self, events: &mpsc::Sender<DriverEvent>, text: &str) {
        if text.is_empty() {
            return;
        }
        if self.last_assistant_text.as_deref() == Some(text) || self.assistant_text_so_far == text {
            self.last_assistant_text = Some(text.to_string());
            return;
        }
        let prior_all = self.assistant_text_so_far.as_str();
        let prior_raw = self.last_assistant_text.as_deref().unwrap_or("");
        let chunk = if !prior_all.is_empty() && text.starts_with(prior_all) {
            &text[prior_all.len()..]
        } else if !prior_raw.is_empty() && text.starts_with(prior_raw) {
            &text[prior_raw.len()..]
        } else {
            text
        };
        if chunk.is_empty() {
            self.last_assistant_text = Some(text.to_string());
            return;
        }
        if !prior_all.is_empty() && text.starts_with(prior_all) {
            self.assistant_text_so_far = text.to_string();
        } else {
            self.assistant_text_so_far.push_str(chunk);
        }
        self.last_assistant_text = Some(text.to_string());
        let _ = events
            .send(DriverEvent::TextChunk {
                stream: TextStream::Assistant,
                chunk: chunk.to_string(),
                seq: self.next_seq(),
            })
            .await;
    }

    async fn translate_tool_use(&mut self, events: &mpsc::Sender<DriverEvent>, value: &Value) {
        let body = value.get("tool_use").unwrap_or(value);
        let call_id = body
            .get("id")
            .or_else(|| body.get("call_id"))
            .and_then(Value::as_str)
            .map(ToString::to_string)
            .unwrap_or_else(|| format!("cursor-tool-{}", uuid::Uuid::new_v4()));
        let name = body
            .get("name")
            .or_else(|| body.get("tool_name"))
            .and_then(Value::as_str)
            .map(normalize_tool_name)
            .unwrap_or_else(|| "unknown".into());
        let args = body
            .get("input")
            .or_else(|| body.get("args"))
            .cloned()
            .unwrap_or(Value::Null);
        self.emit_tool_call_or_telemetry(events, call_id, name, args)
            .await;
    }

    async fn translate_tool_call(&mut self, events: &mpsc::Sender<DriverEvent>, value: &Value) {
        let call_id = value
            .get("call_id")
            .and_then(Value::as_str)
            .map(ToString::to_string)
            .or_else(|| {
                tool_call_args(value)
                    .and_then(|args| args.get("toolCallId"))
                    .and_then(Value::as_str)
                    .map(ToString::to_string)
            })
            .unwrap_or_else(|| format!("cursor-tool-{}", uuid::Uuid::new_v4()));
        let name = tool_call_name(value).unwrap_or_else(|| "unknown".into());
        let args = tool_call_args(value).cloned().unwrap_or(Value::Null);
        match value.get("subtype").and_then(Value::as_str) {
            Some("completed") => {
                let output = tool_call_result(value)
                    .cloned()
                    .unwrap_or_else(|| value.clone());
                if self.allowed_tool_calls.remove(&call_id) {
                    let ok = tool_call_success(value);
                    let _ = events
                        .send(DriverEvent::ToolResult {
                            call_id,
                            ok,
                            output,
                            seq: self.next_seq(),
                        })
                        .await;
                } else {
                    self.emit_tool_telemetry(events, &name, "completed", output)
                        .await;
                }
            }
            _ => {
                self.emit_tool_call_or_telemetry(events, call_id, name, args)
                    .await;
            }
        }
    }

    async fn emit_tool_call_or_telemetry(
        &mut self,
        events: &mpsc::Sender<DriverEvent>,
        call_id: String,
        name: String,
        args: Value,
    ) {
        if self.tool_allowed_for_kind(&name, &args) {
            self.allowed_tool_calls.insert(call_id.clone());
            let _ = events
                .send(DriverEvent::ToolCall {
                    call_id,
                    name,
                    args,
                    seq: self.next_seq(),
                })
                .await;
            return;
        }
        self.emit_tool_telemetry(events, &name, "ignored by orgasmic tool policy", args)
            .await;
    }

    async fn emit_tool_telemetry(
        &mut self,
        events: &mpsc::Sender<DriverEvent>,
        name: &str,
        status: &str,
        payload: Value,
    ) {
        let _ = events
            .send(DriverEvent::TextChunk {
                stream: TextStream::System,
                chunk: format!("cursor-agent tool {name} {status}: {payload}"),
                seq: self.next_seq(),
            })
            .await;
    }

    fn tool_allowed_for_kind(&self, name: &str, args: &Value) -> bool {
        match self.kind {
            RunKind::Worker => self.implementer_tool_allowed(name, args),
            RunKind::Babysitter => BabysitterTool::parse(name).is_some(),
        }
    }

    fn implementer_tool_allowed(&self, name: &str, args: &Value) -> bool {
        if implementer_tool_is_allowed(name) {
            return true;
        }

        // Cursor-agent emits its own native tools (read/shell/grep/find/await)
        // in the same stream as orgasmic control tools. The original policy
        // admitted only WorkerTool names (currently transition_state), which
        // protected the daemon control surface but also hid legitimate
        // verify-style worker activity. Keep transition_state as the only
        // orgasmic control tool, and gate cursor-native filesystem/command tools
        // by path:
        // - read: worker worktree or project-scoped dispatch briefs
        // - shell git: `git -C <worktree> ...`
        // - shell grep/find or native grep/find: search root under worktree
        // - kill/pkill: allowed for worker cleanup; shell redirection is limited to 2>/dev/null
        // - await: always allowed; it only waits for a prior cursor task
        match name {
            "await" | "structured_await" => true,
            "kill" | "pkill" => true,
            "read" | "read_file" | "readfile" => self.read_args_allowed(args),
            "grep" | "grep_search" | "find" => self.search_args_allowed(args),
            "shell" | "bash" => self.shell_command_allowed(args),
            _ => false,
        }
    }

    fn read_args_allowed(&self, args: &Value) -> bool {
        let paths = schema_target_paths(args, &["path"]);
        !paths.is_empty()
            && paths.iter().all(|path| {
                self.path_under_dispatch_artifacts(path) || self.path_under_worktree(path)
            })
    }

    fn search_args_allowed(&self, args: &Value) -> bool {
        let paths = schema_target_paths(args, &["path", "root", "directory"]);
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
        self.normalized_project_root()
            .map(|root| path_is_under(path, &root.join(".orgasmic/tmp/dispatch")))
            .unwrap_or(false)
    }

    fn path_is_worktree(&self, path: &Path) -> bool {
        self.normalized_worktree()
            .map(|worktree| resolve_policy_path(path) == worktree)
            .unwrap_or(false)
    }

    fn path_under_worktree(&self, path: &Path) -> bool {
        self.normalized_worktree()
            .map(|worktree| path_is_under(path, &worktree))
            .unwrap_or(false)
    }

    fn normalized_worktree(&self) -> Option<PathBuf> {
        self.worktree
            .as_deref()
            .filter(|path| path.is_absolute())
            .map(resolve_policy_path)
    }

    fn normalized_project_root(&self) -> Option<PathBuf> {
        self.project_root
            .as_deref()
            .filter(|path| path.is_absolute())
            .map(resolve_policy_path)
    }

    async fn translate_result(&mut self, events: &mpsc::Sender<DriverEvent>, value: &Value) {
        let summary = value
            .get("result")
            .and_then(Value::as_str)
            .map(ToString::to_string);
        if value
            .get("is_error")
            .and_then(Value::as_bool)
            .unwrap_or(false)
        {
            let code = value
                .get("subtype")
                .and_then(Value::as_str)
                .map(|subtype| format!("cursor_result_{subtype}"))
                .unwrap_or_else(|| "cursor_result_error".into());
            let message = summary.unwrap_or_else(|| value.to_string());
            self.emit_run_fail_once(events, code, message).await;
        } else {
            self.emit_run_complete_once(events, summary).await;
        }
    }

    async fn emit_run_complete_once(
        &mut self,
        events: &mpsc::Sender<DriverEvent>,
        summary: Option<String>,
    ) {
        if self.terminal_emitted.swap(true, Ordering::SeqCst) {
            return;
        }
        let _ = events.send(DriverEvent::RunComplete { summary }).await;
    }

    async fn emit_run_fail_once(
        &mut self,
        events: &mpsc::Sender<DriverEvent>,
        error_code: String,
        error_markdown: String,
    ) {
        if self.terminal_emitted.swap(true, Ordering::SeqCst) {
            return;
        }
        let _ = events
            .send(DriverEvent::RunFail {
                error_code,
                error_markdown,
            })
            .await;
    }

    async fn translate_error(&mut self, events: &mpsc::Sender<DriverEvent>, value: &Value) {
        let message = value
            .get("message")
            .or_else(|| value.get("error"))
            .and_then(Value::as_str)
            .map(ToString::to_string)
            .unwrap_or_else(|| value.to_string());
        let _ = events
            .send(DriverEvent::DriverError {
                fatal: true,
                message,
            })
            .await;
    }

    async fn translate_thinking(&mut self, events: &mpsc::Sender<DriverEvent>, value: &Value) {
        let subtype = value
            .get("subtype")
            .and_then(Value::as_str)
            .unwrap_or("unknown");
        let _ = events
            .send(DriverEvent::TextChunk {
                stream: TextStream::System,
                chunk: format!("cursor-agent thinking {subtype}"),
                seq: self.next_seq(),
            })
            .await;
    }
}

fn tool_call_body(value: &Value) -> Option<(&str, &Value)> {
    let body = value.get("tool_call")?.as_object()?;
    if let Some(name) = body.get("name").and_then(Value::as_str) {
        return Some((name, value.get("tool_call").unwrap()));
    }
    body.iter()
        .find(|(_, tool)| tool.is_object())
        .map(|(name, tool)| (name.as_str(), tool))
}

fn tool_call_name(value: &Value) -> Option<String> {
    value
        .get("name")
        .and_then(Value::as_str)
        .map(ToString::to_string)
        .or_else(|| tool_call_body(value).map(|(name, _)| normalize_tool_name(name)))
}

fn tool_call_args(value: &Value) -> Option<&Value> {
    let (_, body) = tool_call_body(value)?;
    body.get("args").or_else(|| body.get("input"))
}

fn tool_call_result(value: &Value) -> Option<&Value> {
    let (_, body) = tool_call_body(value)?;
    body.get("result")
}

fn tool_call_success(value: &Value) -> bool {
    let Some(result) = tool_call_result(value) else {
        return true;
    };
    if result.get("error").is_some() {
        return false;
    }
    if result.get("success").is_some() {
        return true;
    }
    !result
        .get("is_error")
        .and_then(Value::as_bool)
        .unwrap_or(false)
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

/// Simulated config used by supervisor smoke tests and CI runs without a
/// live `cursor-agent` binary.
pub fn simulated_config() -> DriverConfig {
    DriverConfig::from_value(Value::Object(Default::default()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{AttachOutcome, CursorAgentDriver, WorkerDriver};
    use orgasmic_core::RuntimeIdentity;

    fn ctx(id: &str, kind: RunKind) -> DriverContext {
        DriverContext {
            identity: RuntimeIdentity::new(id, "boot-test"),
            run_kind: kind,
            task_id: "TASK-056".into(),
            worker_id: "implementer-composer".into(),
            project_id: Some("orgasmic".into()),
            worktree: None,
            babysitter_target: None,
        }
    }

    fn terminal_flag() -> Arc<AtomicBool> {
        Arc::new(AtomicBool::new(false))
    }

    fn test_translator(kind: RunKind) -> CursorAgentTranslator {
        CursorAgentTranslator::new(kind, None, &CursorAgentConfig::default(), terminal_flag())
    }

    #[tokio::test]
    async fn stream_json_init_emits_ready() {
        let (tx, mut rx) = mpsc::channel(8);
        let cfg = CursorAgentConfig {
            endpoint: Some("stdio".into()),
            model: Some("fixture-model".into()),
            sandbox: Some(DEFAULT_SANDBOX.into()),
            force: Some(true),
            ..CursorAgentConfig::default()
        };
        let mut translator =
            CursorAgentTranslator::new(RunKind::Worker, None, &cfg, terminal_flag());
        translator
            .translate_value(
                &tx,
                &json!({
                    "type": "system",
                    "subtype": "init",
                    "session_id": "sess-1",
                    "model": "Composer 2.5 Fast",
                    "apiKeySource": "login",
                    "permissionMode": "default"
                }),
            )
            .await;
        let ev = rx.recv().await.unwrap();
        let DriverEvent::Ready {
            protocol_version,
            capabilities,
        } = ev
        else {
            panic!("expected Ready");
        };
        assert_eq!(protocol_version, "cursor-agent-stream-json/1");
        assert_eq!(capabilities["simulated"], false);
        assert_eq!(capabilities["wire"], "stdio-stream-json");
        assert_eq!(capabilities["session_id"], "sess-1");
        assert_eq!(capabilities["model"], "Composer 2.5 Fast");
    }

    #[tokio::test]
    async fn stream_json_assistant_text_maps_to_chunk() {
        let (tx, mut rx) = mpsc::channel(8);
        let mut translator = test_translator(RunKind::Worker);
        translator
            .translate_value(
                &tx,
                &json!({
                    "type": "assistant",
                    "message": {
                        "role": "assistant",
                        "content": [{"type": "text", "text": "hello"}]
                    }
                }),
            )
            .await;
        let ev = rx.recv().await.unwrap();
        assert_eq!(
            ev,
            DriverEvent::TextChunk {
                stream: TextStream::Assistant,
                chunk: "hello".into(),
                seq: 0,
            }
        );
    }

    #[tokio::test]
    async fn top_level_tool_use_obeys_implementer_policy() {
        let (tx, mut rx) = mpsc::channel(8);
        let mut translator = test_translator(RunKind::Worker);
        translator
            .translate_value(
                &tx,
                &json!({
                    "type": "tool_use",
                    "id": "tool-transition",
                    "name": "transition_state",
                    "input": {"from": "implement", "to": "done", "reason": "complete"}
                }),
            )
            .await;

        let ev = rx.recv().await.unwrap();
        assert_eq!(
            ev,
            DriverEvent::ToolCall {
                call_id: "tool-transition".into(),
                name: "transition_state".into(),
                args: json!({"from": "implement", "to": "done", "reason": "complete"}),
                seq: 0,
            }
        );
    }

    #[tokio::test]
    async fn native_cursor_tool_use_is_system_telemetry_not_tool_call() {
        let (tx, mut rx) = mpsc::channel(8);
        let mut translator = test_translator(RunKind::Worker);
        translator
            .translate_value(
                &tx,
                &json!({
                    "type": "tool_use",
                    "id": "tool-shell",
                    "name": "shell",
                    "input": {"command": "echo hello"}
                }),
            )
            .await;

        let ev = rx.recv().await.unwrap();
        assert!(matches!(
            ev,
            DriverEvent::TextChunk {
                stream: TextStream::System,
                chunk,
                seq: 0,
            } if chunk.contains("cursor-agent tool shell ignored by orgasmic tool policy")
        ));
    }

    #[tokio::test]
    async fn babysitter_tool_use_obeys_babysitter_policy() {
        let (tx, mut rx) = mpsc::channel(8);
        let mut translator = test_translator(RunKind::Babysitter);
        translator
            .translate_value(
                &tx,
                &json!({
                    "type": "tool_use",
                    "id": "tool-poke",
                    "name": "poke",
                    "input": {"target_run": "run-worker"}
                }),
            )
            .await;
        translator
            .translate_value(
                &tx,
                &json!({
                    "type": "tool_use",
                    "id": "tool-transition",
                    "name": "transition_state",
                    "input": {"to": "done"}
                }),
            )
            .await;

        let allowed = rx.recv().await.unwrap();
        assert!(matches!(
            allowed,
            DriverEvent::ToolCall {
                name,
                seq: 0,
                ..
            } if name == "poke"
        ));
        let blocked = rx.recv().await.unwrap();
        assert!(matches!(
            blocked,
            DriverEvent::TextChunk {
                stream: TextStream::System,
                chunk,
                seq: 1,
            } if chunk.contains("cursor-agent tool transition_state ignored by orgasmic tool policy")
        ));
    }

    #[tokio::test]
    async fn stream_json_tool_call_filters_native_started_and_completed() {
        let (tx, mut rx) = mpsc::channel(8);
        let mut translator = test_translator(RunKind::Worker);
        translator
            .translate_value(
                &tx,
                &json!({
                    "type": "tool_call",
                    "subtype": "started",
                    "call_id": "tool-1",
                    "tool_call": {
                        "shellToolCall": {
                            "args": {"command": "echo hello"}
                        }
                    }
                }),
            )
            .await;
        translator
            .translate_value(
                &tx,
                &json!({
                    "type": "tool_call",
                    "subtype": "completed",
                    "call_id": "tool-1",
                    "tool_call": {
                        "shellToolCall": {
                            "result": {
                                "success": {
                                    "stdout": "hello\n",
                                    "exitCode": 0
                                }
                            }
                        }
                    }
                }),
            )
            .await;

        let started = rx.recv().await.unwrap();
        assert!(matches!(
            started,
            DriverEvent::TextChunk {
                stream: TextStream::System,
                chunk,
                seq: 0,
            } if chunk.contains("cursor-agent tool shell ignored by orgasmic tool policy")
        ));
        let completed = rx.recv().await.unwrap();
        assert!(matches!(
            completed,
            DriverEvent::TextChunk {
                stream: TextStream::System,
                chunk,
                seq: 1,
            } if chunk.contains("cursor-agent tool shell completed")
        ));
    }

    #[tokio::test]
    async fn stream_json_partial_fixture_emits_deltas_and_one_terminal() {
        let fixture = include_str!("../../tests/fixtures/cursor_agent_partial.jsonl");
        let (tx, mut rx) = mpsc::channel(32);
        let mut translator = test_translator(RunKind::Worker);

        for line in fixture.lines().filter(|line| !line.trim().is_empty()) {
            translator.translate_stdout_line(&tx, line).await;
        }

        let mut chunks = Vec::new();
        let mut terminal_count = 0;
        while let Ok(ev) = rx.try_recv() {
            match ev {
                DriverEvent::TextChunk {
                    stream: TextStream::Assistant,
                    chunk,
                    ..
                } => chunks.push(chunk),
                DriverEvent::RunComplete { .. } | DriverEvent::RunFail { .. } => {
                    terminal_count += 1;
                }
                _ => {}
            }
        }

        assert_eq!(
            chunks,
            vec![
                "Hi",
                " \u{2014} good",
                " to meet you.",
                " What would",
                " you like to work",
                " on?",
            ]
        );
        assert_eq!(terminal_count, 1);
    }

    #[tokio::test]
    async fn attach_is_not_reattachable() {
        let d = CursorAgentDriver;
        let outcome = d
            .attach(
                ctx("run-cursor-attach", RunKind::Worker),
                simulated_config(),
            )
            .await
            .unwrap();
        assert!(matches!(outcome, AttachOutcome::NotReattachable));
    }
}
