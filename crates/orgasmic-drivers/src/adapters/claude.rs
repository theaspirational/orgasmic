// arch: arch_A53QX.3
// orgasmic:arch_A53QX, dec_ASB1A, task_TGGAJ
//! Claude Code harness adapter for ACP-like stdio stream-json.
//!
//! Claude Code's exposed programmatic wire is the Agent SDK JSONL stream:
//! `claude -p --input-format stream-json --output-format stream-json`.
//! The `endpoint` field in config is retained in capabilities for audit only.
//! An empty endpoint is normal for the ACP-stdio pairing, where the mode
//! upgrades this adapter's simulated request into `stdio_spawn`; ACP-WS keeps
//! the simulated request because a WebSocket URL is required there.
//!
//! Simulation is also used when `claude` is not detectable on PATH, or when
//! `ORGASMIC_DRIVER_SIMULATE=1` is set explicitly.  Both cases emit a WARN
//! log naming which check caused the fallback so the next operator can debug
//! without reading source.

use std::collections::BTreeMap;
use std::process::{Command as StdCommand, Stdio};

use async_trait::async_trait;
use serde::Deserialize;
use serde_json::{json, Value};
use tokio::sync::mpsc;

use orgasmic_core::{DriverEvent, TextStream};

use crate::r#trait::{
    BabysitterRequest, DriverConfig, DriverContext, DriverError, HarnessControlOutcome,
    HarnessEventAdapter, HarnessRequest, RunKind, StdioSpawn, TransitionRequest,
};

const TRANSPORT: &str = "claude-acp";

pub struct ClaudeAdapter {
    translator: Option<AcpTranslator>,
}

impl ClaudeAdapter {
    pub fn new() -> Self {
        Self { translator: None }
    }

    async fn collect<F>(&mut self, f: F) -> Vec<DriverEvent>
    where
        F: for<'a> FnOnce(
            &'a mut AcpTranslator,
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

impl Default for ClaudeAdapter {
    fn default() -> Self {
        Self::new()
    }
}

#[derive(Debug, Clone, Deserialize, Default)]
struct ClaudeAcpConfig {
    #[serde(default)]
    endpoint: Option<String>,
    #[serde(default)]
    model: Option<String>,
    #[serde(default)]
    api_key_env: Option<String>,
    #[serde(default)]
    prompt_bundle_text: Option<String>,
}

#[async_trait]
impl HarnessEventAdapter for ClaudeAdapter {
    fn harness(&self) -> &'static str {
        "claude"
    }

    fn clone_box(&self) -> Box<dyn HarnessEventAdapter> {
        Box::new(ClaudeAdapter::new())
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
            .map(AcpTranslator::next_seq)
            .unwrap_or(0)
    }

    fn validate_config(&self, config: &DriverConfig) -> Result<(), DriverError> {
        let cfg: ClaudeAcpConfig = serde_json::from_value(config.0.clone())
            .map_err(|e| DriverError::InvalidConfig(e.to_string()))?;
        if let Some(env_name) = cfg.api_key_env.as_deref() {
            if std::env::var(env_name).is_err() && cfg.endpoint.is_some() {
                return Err(DriverError::InvalidConfig(format!(
                    "api_key_env '{env_name}' not set but endpoint is configured"
                )));
            }
        }
        Ok(())
    }

    fn stdio_spawn(&self) -> Option<StdioSpawn> {
        Some(StdioSpawn {
            command: "claude".into(),
            args: vec![
                "--bare".to_string(),
                "-p".to_string(),
                "--input-format".to_string(),
                "stream-json".to_string(),
                "--output-format".to_string(),
                "stream-json".to_string(),
                "--include-partial-messages".to_string(),
                "--verbose".to_string(),
                "--no-session-persistence".to_string(),
            ],
            cwd: None,
            env: Vec::new(),
        })
    }

    fn upgrades_simulated_to_subprocess(&self) -> bool {
        std::env::var("ORGASMIC_DRIVER_SIMULATE")
            .map(|v| v != "1")
            .unwrap_or(true)
    }

    fn stdio_initial_payload(
        &mut self,
        ctx: &DriverContext,
        config: &DriverConfig,
    ) -> Result<Option<Vec<u8>>, DriverError> {
        let cfg: ClaudeAcpConfig = serde_json::from_value(config.0.clone())
            .map_err(|e| DriverError::InvalidConfig(e.to_string()))?;
        self.translator = Some(AcpTranslator::new(
            cfg.endpoint.clone(),
            ctx.run_kind,
            cfg.model.clone(),
        ));
        Ok(Some(json_line_bytes(&claude_user_message(
            build_spawn_prompt(ctx, &cfg),
        ))?))
    }

    fn compose_request(
        &mut self,
        ctx: &DriverContext,
        config: &DriverConfig,
    ) -> Result<HarnessRequest, DriverError> {
        let cfg: ClaudeAcpConfig = serde_json::from_value(config.0.clone())
            .map_err(|e| DriverError::InvalidConfig(e.to_string()))?;
        self.validate_config(config)?;
        let explicit_simulate = std::env::var("ORGASMIC_DRIVER_SIMULATE")
            .map(|v| v == "1")
            .unwrap_or(false);
        let endpoint_empty = cfg.endpoint.as_deref().map(str::is_empty).unwrap_or(true);
        let simulated = if explicit_simulate {
            tracing::warn!(
                "claude-acp: ORGASMIC_DRIVER_SIMULATE=1 is set; using simulated mode (explicit override)"
            );
            true
        } else if !claude_available() {
            tracing::warn!(
                "claude-acp: 'claude' binary not found on PATH; using simulated mode (binary not detectable)"
            );
            true
        } else {
            endpoint_empty
        };
        if simulated {
            return Ok(HarnessRequest::Simulated {
                events: simulated_start_events(ctx, &cfg),
            });
        }

        self.translator = Some(AcpTranslator::new(
            cfg.endpoint.clone(),
            ctx.run_kind,
            cfg.model.clone(),
        ));
        let spawn = self
            .stdio_spawn()
            .expect("claude adapter always exposes stdio_spawn");
        let mut args = spawn.args.clone();
        if let Some(model) = cfg.model.as_deref() {
            if !model.is_empty() {
                args.push("--model".into());
                args.push(model.to_string());
            }
        }
        let mut env = BTreeMap::new();
        if let Some(env_name) = cfg.api_key_env.as_deref() {
            let api_key = std::env::var(env_name).map_err(|_| {
                DriverError::InvalidConfig(format!(
                    "api_key_env '{env_name}' not set but endpoint is configured"
                ))
            })?;
            env.insert("ANTHROPIC_API_KEY".into(), api_key);
        }
        Ok(HarnessRequest::Subprocess {
            binary: spawn.command,
            args,
            env,
            cwd: spawn.cwd.clone().or_else(|| ctx.worktree.clone()),
            stdin_payload: Some(json_line_bytes(&claude_user_message(build_spawn_prompt(
                ctx, &cfg,
            )))?),
            close_stdin: false,
        })
    }

    async fn transition_state(
        &mut self,
        req: TransitionRequest,
    ) -> Result<HarnessControlOutcome, DriverError> {
        let text = format!(
            "orgasmic control: transition_state requested\nfrom: {}\nto: {}\nreason: {}",
            req.from, req.to, req.reason
        );
        Ok(HarnessControlOutcome {
            events: vec![DriverEvent::TransitionState {
                from: req.from,
                to: req.to,
                reason: req.reason,
            }],
            stdin_payloads: vec![json_line_bytes(&claude_user_message(text))?],
            ..HarnessControlOutcome::default()
        })
    }

    async fn babysitter_action(
        &mut self,
        req: BabysitterRequest,
    ) -> Result<HarnessControlOutcome, DriverError> {
        let call_id = format!("acp-bs-{}", uuid::Uuid::new_v4());
        let payload = json!({
            "tool": req.tool.as_str(),
            "target_run": req.target_run,
            "payload": req.payload,
        });
        let text = format!(
            "orgasmic babysitter control action:\n```json\n{}\n```",
            serde_json::to_string_pretty(&payload).unwrap_or_else(|_| payload.to_string())
        );
        Ok(HarnessControlOutcome {
            events: vec![DriverEvent::ToolCall {
                call_id,
                name: payload["tool"].as_str().unwrap_or("unknown").into(),
                args: payload["payload"].clone(),
                seq: self.next_seq(),
            }],
            stdin_payloads: vec![json_line_bytes(&claude_user_message(text))?],
            ..HarnessControlOutcome::default()
        })
    }

    async fn release(&mut self, reason: String) -> Result<HarnessControlOutcome, DriverError> {
        let text = format!("orgasmic control: release requested\nreason: {reason}");
        Ok(HarnessControlOutcome {
            events: crate::r#trait::turn_boundary_events(
                self.next_seq(),
                DriverEvent::RunComplete {
                    summary: Some(reason),
                },
            ),
            stdin_payloads: vec![json_line_bytes(&claude_user_message(text))?],
            close: true,
            ..HarnessControlOutcome::default()
        })
    }
}

fn claude_available() -> bool {
    StdCommand::new("claude")
        .arg("--version")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

fn build_spawn_prompt(ctx: &DriverContext, cfg: &ClaudeAcpConfig) -> String {
    let payload = json!({
        "transport": TRANSPORT,
        "wire": "claude-code-stdio-stream-json",
        "endpoint": cfg.endpoint,
        "model": cfg.model,
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

fn claude_user_message(text: String) -> Value {
    json!({
        "type": "user",
        "message": {
            "role": "user",
            "content": [
                {
                    "type": "text",
                    "text": text,
                }
            ],
        },
        "parent_tool_use_id": null,
    })
}

fn json_line_bytes(value: &Value) -> Result<Vec<u8>, DriverError> {
    let mut line = serde_json::to_vec(value).map_err(|e| DriverError::Other(e.to_string()))?;
    line.push(b'\n');
    Ok(line)
}

fn simulated_start_events(ctx: &DriverContext, cfg: &ClaudeAcpConfig) -> Vec<DriverEvent> {
    vec![DriverEvent::Ready {
        protocol_version: "acp/1".into(),
        capabilities: json!({
            "simulated": true,
            "kind": ctx.run_kind,
            "model": cfg.model,
        }),
    }]
}

struct StreamingTool {
    call_id: String,
    name: String,
    initial_input: Value,
    partial_json: String,
}

struct AcpTranslator {
    seq: u64,
    endpoint: Option<String>,
    kind: RunKind,
    configured_model: Option<String>,
    streaming_tools: BTreeMap<u64, StreamingTool>,
    saw_partial_text: bool,
}

impl AcpTranslator {
    fn new(endpoint: Option<String>, kind: RunKind, configured_model: Option<String>) -> Self {
        Self {
            seq: 0,
            endpoint,
            kind,
            configured_model,
            streaming_tools: BTreeMap::new(),
            saw_partial_text: false,
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
            Some("stream_event") => self.translate_stream_event(events, value).await,
            Some("result") => self.translate_result(events, value).await,
            Some("user") => self.translate_user(events, value).await,
            Some(other) => {
                let _ = events
                    .send(DriverEvent::TextChunk {
                        stream: TextStream::System,
                        chunk: format!("claude event {other}: {value}"),
                        seq: self.next_seq(),
                    })
                    .await;
            }
            None => {
                let _ = events
                    .send(DriverEvent::TextChunk {
                        stream: TextStream::System,
                        chunk: format!("claude event: {value}"),
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
                        protocol_version: "claude-code-stream-json/1".into(),
                        capabilities: json!({
                            "simulated": false,
                            "kind": self.kind,
                            "wire": "stdio-stream-json",
                            "endpoint": self.endpoint,
                            "model": observed_model,
                            "session_id": value.get("session_id").cloned().unwrap_or(Value::Null),
                            "claude_code_version": value.get("claude_code_version").cloned().unwrap_or(Value::Null),
                        }),
                    })
                    .await;
            }
            Some("status") => {
                let status = value
                    .get("status")
                    .and_then(Value::as_str)
                    .unwrap_or("unknown");
                let _ = events
                    .send(DriverEvent::TextChunk {
                        stream: TextStream::System,
                        chunk: format!("claude status: {status}"),
                        seq: self.next_seq(),
                    })
                    .await;
            }
            Some(subtype) => {
                let _ = events
                    .send(DriverEvent::TextChunk {
                        stream: TextStream::System,
                        chunk: format!("claude system {subtype}: {value}"),
                        seq: self.next_seq(),
                    })
                    .await;
            }
            None => {
                let _ = events
                    .send(DriverEvent::TextChunk {
                        stream: TextStream::System,
                        chunk: format!("claude system: {value}"),
                        seq: self.next_seq(),
                    })
                    .await;
            }
        }
    }

    async fn translate_assistant(&mut self, events: &mpsc::Sender<DriverEvent>, value: &Value) {
        if let Some(content) = value.pointer("/message/content") {
            self.translate_content(events, content, !self.saw_partial_text)
                .await;
        }
        if let Some(error) = value.get("error").and_then(Value::as_str) {
            let text = value
                .pointer("/message/content/0/text")
                .and_then(Value::as_str)
                .unwrap_or(error);
            let _ = events
                .send(DriverEvent::DriverError {
                    fatal: true,
                    message: format!("claude {error}: {text}"),
                })
                .await;
        }
    }

    async fn translate_user(&mut self, events: &mpsc::Sender<DriverEvent>, value: &Value) {
        if let Some(content) = value.pointer("/message/content") {
            self.translate_tool_results(events, content).await;
        }
    }

    async fn translate_content(
        &mut self,
        events: &mpsc::Sender<DriverEvent>,
        content: &Value,
        emit_text: bool,
    ) {
        if let Some(text) = content.as_str() {
            if emit_text && !text.is_empty() {
                let _ = events
                    .send(DriverEvent::TextChunk {
                        stream: TextStream::Assistant,
                        chunk: text.to_string(),
                        seq: self.next_seq(),
                    })
                    .await;
            }
            return;
        }
        let Some(items) = content.as_array() else {
            return;
        };
        for item in items {
            match item.get("type").and_then(Value::as_str) {
                Some("text") if emit_text => {
                    if let Some(text) = item.get("text").and_then(Value::as_str) {
                        if !text.is_empty() {
                            let _ = events
                                .send(DriverEvent::TextChunk {
                                    stream: TextStream::Assistant,
                                    chunk: text.to_string(),
                                    seq: self.next_seq(),
                                })
                                .await;
                        }
                    }
                }
                Some("tool_use") => {
                    let call_id = item
                        .get("id")
                        .and_then(Value::as_str)
                        .map(ToString::to_string)
                        .unwrap_or_else(|| format!("claude-tool-{}", uuid::Uuid::new_v4()));
                    let name = item
                        .get("name")
                        .and_then(Value::as_str)
                        .unwrap_or("unknown")
                        .to_string();
                    let args = item.get("input").cloned().unwrap_or(Value::Null);
                    let _ = events
                        .send(DriverEvent::ToolCall {
                            call_id,
                            name,
                            args,
                            seq: self.next_seq(),
                        })
                        .await;
                }
                _ => {}
            }
        }
    }

    async fn translate_tool_results(
        &mut self,
        events: &mpsc::Sender<DriverEvent>,
        content: &Value,
    ) {
        let Some(items) = content.as_array() else {
            return;
        };
        for item in items {
            if item.get("type").and_then(Value::as_str) != Some("tool_result") {
                continue;
            }
            let call_id = item
                .get("tool_use_id")
                .and_then(Value::as_str)
                .map(ToString::to_string)
                .unwrap_or_else(|| format!("claude-tool-result-{}", uuid::Uuid::new_v4()));
            let ok = !item
                .get("is_error")
                .and_then(Value::as_bool)
                .unwrap_or(false);
            let _ = events
                .send(DriverEvent::ToolResult {
                    call_id,
                    ok,
                    output: item.clone(),
                    seq: self.next_seq(),
                })
                .await;
        }
    }

    async fn translate_stream_event(&mut self, events: &mpsc::Sender<DriverEvent>, value: &Value) {
        let Some(event) = value.get("event") else {
            return;
        };
        match event.get("type").and_then(Value::as_str) {
            Some("content_block_start") => {
                let index = event.get("index").and_then(Value::as_u64).unwrap_or(0);
                if let Some(block) = event.get("content_block") {
                    if block.get("type").and_then(Value::as_str) == Some("tool_use") {
                        self.streaming_tools.insert(
                            index,
                            StreamingTool {
                                call_id: block
                                    .get("id")
                                    .and_then(Value::as_str)
                                    .map(ToString::to_string)
                                    .unwrap_or_else(|| {
                                        format!("claude-tool-{}", uuid::Uuid::new_v4())
                                    }),
                                name: block
                                    .get("name")
                                    .and_then(Value::as_str)
                                    .unwrap_or("unknown")
                                    .to_string(),
                                initial_input: block.get("input").cloned().unwrap_or(Value::Null),
                                partial_json: String::new(),
                            },
                        );
                    }
                }
            }
            Some("content_block_delta") => {
                let delta = event.get("delta").unwrap_or(&Value::Null);
                match delta.get("type").and_then(Value::as_str) {
                    Some("text_delta") => {
                        if let Some(text) = delta.get("text").and_then(Value::as_str) {
                            self.saw_partial_text = true;
                            if !text.is_empty() {
                                let _ = events
                                    .send(DriverEvent::TextChunk {
                                        stream: TextStream::Assistant,
                                        chunk: text.to_string(),
                                        seq: self.next_seq(),
                                    })
                                    .await;
                            }
                        }
                    }
                    Some("input_json_delta") => {
                        let index = event.get("index").and_then(Value::as_u64).unwrap_or(0);
                        if let Some(tool) = self.streaming_tools.get_mut(&index) {
                            if let Some(partial) = delta.get("partial_json").and_then(Value::as_str)
                            {
                                tool.partial_json.push_str(partial);
                            }
                        }
                    }
                    _ => {}
                }
            }
            Some("content_block_stop") => {
                let index = event.get("index").and_then(Value::as_u64).unwrap_or(0);
                if let Some(tool) = self.streaming_tools.remove(&index) {
                    let args = if tool.partial_json.trim().is_empty() {
                        tool.initial_input
                    } else {
                        serde_json::from_str(&tool.partial_json)
                            .unwrap_or_else(|_| json!({"partial_json": tool.partial_json}))
                    };
                    let _ = events
                        .send(DriverEvent::ToolCall {
                            call_id: tool.call_id,
                            name: tool.name,
                            args,
                            seq: self.next_seq(),
                        })
                        .await;
                }
            }
            _ => {}
        }
    }

    async fn translate_result(&mut self, events: &mpsc::Sender<DriverEvent>, value: &Value) {
        let summary = value
            .get("result")
            .and_then(Value::as_str)
            .map(ToString::to_string);
        let seq = self.next_seq();
        let _ = events.send(DriverEvent::AgentTurnComplete { seq }).await;
        if value
            .get("is_error")
            .and_then(Value::as_bool)
            .unwrap_or(false)
        {
            let code = value
                .get("api_error_status")
                .and_then(Value::as_i64)
                .map(|s| format!("claude_api_error_{s}"))
                .unwrap_or_else(|| "claude_result_error".into());
            let message = summary.unwrap_or_else(|| value.to_string());
            let _ = events
                .send(DriverEvent::RunFail {
                    error_code: code,
                    error_markdown: message,
                })
                .await;
        } else {
            let _ = events.send(DriverEvent::RunComplete { summary }).await;
        }
    }
}

/// Simulated config used by supervisor smoke tests and CI runs without a
/// live `claude` binary.
pub fn simulated_config() -> DriverConfig {
    DriverConfig::from_value(Value::Object(Default::default()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{AcpStdioDriver, AcpWsDriver, AttachOutcome, ClaudeAcpDriver, WorkerDriver};
    use orgasmic_core::RuntimeIdentity;
    use tokio::time::{timeout, Duration};

    fn ctx(id: &str, kind: RunKind) -> DriverContext {
        DriverContext {
            identity: RuntimeIdentity::new(id, "boot-test"),
            run_kind: kind,
            task_id: "TASK-006".into(),
            worker_id: "implementer-claude".into(),
            project_id: Some("orgasmic".into()),
            worktree: None,
            babysitter_target: None,
        }
    }

    #[tokio::test]
    async fn simulated_acquire_emits_ready_and_release() {
        // Explicit simulate override so the test is not affected by whether
        // the `claude` binary is on PATH on the host running the suite.
        // tokio::sync::MutexGuard may be held across await points; clippy's
        // `await_holding_lock` lint targets only std::sync::MutexGuard.
        let _guard = env_lock().lock().await;
        std::env::set_var("ORGASMIC_DRIVER_SIMULATE", "1");

        let d = ClaudeAcpDriver;
        let mut s = d
            .acquire(ctx("run-c", RunKind::Worker), simulated_config())
            .await
            .unwrap();
        let ev = s.events.recv().await.unwrap();
        assert!(matches!(ev, DriverEvent::Ready { .. }));
        s.control.release("done").await.unwrap();
        let turn = s.events.recv().await.unwrap();
        assert!(matches!(turn, DriverEvent::AgentTurnComplete { .. }));
        let last = s.events.recv().await.unwrap();
        std::env::remove_var("ORGASMIC_DRIVER_SIMULATE");
        assert!(matches!(last, DriverEvent::RunComplete { .. }));
    }

    #[tokio::test]
    async fn missing_api_key_with_endpoint_fails_validation() {
        let d = ClaudeAcpDriver;
        std::env::remove_var("ORGASMIC_TEST_MISSING_ACP_KEY");
        let cfg = DriverConfig::from_value(json!({
            "endpoint": "https://example.com",
            "api_key_env": "ORGASMIC_TEST_MISSING_ACP_KEY"
        }));
        assert!(d.validate(&cfg).is_err());
    }

    #[test]
    fn transport_name_is_stable() {
        assert_eq!(ClaudeAcpDriver.transport(), "claude-acp");
    }

    #[tokio::test]
    async fn attach_is_not_reattachable() {
        let d = ClaudeAcpDriver;
        let outcome = d
            .attach(ctx("run-acp-attach", RunKind::Worker), simulated_config())
            .await
            .unwrap();
        assert!(matches!(outcome, AttachOutcome::NotReattachable));
    }

    #[tokio::test]
    async fn stream_json_init_emits_ready() {
        let (tx, mut rx) = mpsc::channel(8);
        let mut translator =
            AcpTranslator::new(Some("stdio".into()), RunKind::Worker, Some("sonnet".into()));
        translator
            .translate_value(
                &tx,
                &json!({
                    "type": "system",
                    "subtype": "init",
                    "session_id": "sess-1",
                    "model": "claude-sonnet-4-6",
                    "claude_code_version": "2.1.147"
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
        assert_eq!(protocol_version, "claude-code-stream-json/1");
        assert_eq!(capabilities["simulated"], false);
        assert_eq!(capabilities["wire"], "stdio-stream-json");
        assert_eq!(capabilities["session_id"], "sess-1");
    }

    #[tokio::test]
    async fn stream_json_text_delta_maps_to_assistant_chunk() {
        let (tx, mut rx) = mpsc::channel(8);
        let mut translator = AcpTranslator::new(None, RunKind::Worker, None);
        translator
            .translate_value(
                &tx,
                &json!({
                    "type": "stream_event",
                    "event": {
                        "type": "content_block_delta",
                        "index": 0,
                        "delta": {
                            "type": "text_delta",
                            "text": "hello"
                        }
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
    async fn stream_json_tool_delta_maps_to_tool_call() {
        let (tx, mut rx) = mpsc::channel(8);
        let mut translator = AcpTranslator::new(None, RunKind::Worker, None);
        translator
            .translate_value(
                &tx,
                &json!({
                    "type": "stream_event",
                    "event": {
                        "type": "content_block_start",
                        "index": 1,
                        "content_block": {
                            "type": "tool_use",
                            "id": "toolu_1",
                            "name": "Bash",
                            "input": {}
                        }
                    }
                }),
            )
            .await;
        translator
            .translate_value(
                &tx,
                &json!({
                    "type": "stream_event",
                    "event": {
                        "type": "content_block_delta",
                        "index": 1,
                        "delta": {
                            "type": "input_json_delta",
                            "partial_json": "{\"command\":\"git status\"}"
                        }
                    }
                }),
            )
            .await;
        translator
            .translate_value(
                &tx,
                &json!({
                    "type": "stream_event",
                    "event": {
                        "type": "content_block_stop",
                        "index": 1
                    }
                }),
            )
            .await;
        let ev = rx.recv().await.unwrap();
        assert_eq!(
            ev,
            DriverEvent::ToolCall {
                call_id: "toolu_1".into(),
                name: "Bash".into(),
                args: json!({"command": "git status"}),
                seq: 0,
            }
        );
    }

    // Serialize tests that mutate process env.
    //
    // A single tokio::sync::Mutex is shared by both sync tests (use
    // blocking_lock()) and async tests (use .lock().await). tokio's MutexGuard
    // may be held across await points without triggering the
    // `await_holding_lock` clippy lint — that lint targets only std::sync::Mutex.
    fn env_lock() -> &'static tokio::sync::Mutex<()> {
        static LOCK: std::sync::OnceLock<tokio::sync::Mutex<()>> = std::sync::OnceLock::new();
        LOCK.get_or_init(|| tokio::sync::Mutex::new(()))
    }

    fn make_claude_stub(dir: &std::path::Path) {
        let stub = dir.join("claude");
        std::fs::write(
            &stub,
            r#"#!/bin/sh
if [ "$1" = "--version" ]; then
  exit 0
fi
printf '%s\n' '{"type":"system","subtype":"init","session_id":"stub-session","model":"stub-model","claude_code_version":"stub"}'
printf '%s\n' '{"type":"result","subtype":"success","result":"stub complete"}'
"#,
        )
        .expect("write claude stub");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mut perms = std::fs::metadata(&stub).unwrap().permissions();
            perms.set_mode(0o755);
            std::fs::set_permissions(&stub, perms).unwrap();
        }
    }

    /// Endpoint-empty + ACP-stdio + detectable `claude` (via stub) must take
    /// the real stdio path. ACP-stdio is the discriminator: it upgrades the
    /// adapter's empty-endpoint Simulated request through `stdio_spawn`.
    #[tokio::test]
    async fn gate_real_wire_for_empty_endpoint_stdio_when_claude_stub_on_path() {
        let dir = tempfile::tempdir().expect("tempdir");
        make_claude_stub(dir.path());

        let _guard = env_lock().lock().await;
        let saved_path = std::env::var("PATH").unwrap_or_default();
        let new_path = format!("{}:{}", dir.path().display(), saved_path);
        std::env::set_var("PATH", &new_path);
        std::env::remove_var("ORGASMIC_DRIVER_SIMULATE");

        let driver = AcpStdioDriver::new(Box::new(ClaudeAdapter::new()));
        let mut session = driver
            .acquire(ctx("run-gate-real", RunKind::Worker), simulated_config())
            .await
            .expect("stdio acquire should spawn claude stub");
        std::env::set_var("PATH", &saved_path);

        let event = timeout(Duration::from_secs(5), session.events.recv())
            .await
            .expect("timed out waiting for stub Ready")
            .expect("event stream closed before Ready");
        match event {
            DriverEvent::Ready { capabilities, .. } => assert_eq!(
                capabilities["simulated"],
                serde_json::Value::Bool(false),
                "stdio + available claude must emit simulated:false"
            ),
            other => panic!("expected Ready, got {other:?}"),
        }
        let _ = session.control.release("test cleanup").await;
    }

    /// Endpoint-empty + ACP-WS + detectable `claude` remains simulated because
    /// WS mode has no `stdio_spawn` upgrade and requires a configured URL.
    #[tokio::test]
    async fn gate_simulates_for_empty_endpoint_ws_when_claude_stub_on_path() {
        let dir = tempfile::tempdir().expect("tempdir");
        make_claude_stub(dir.path());

        let _guard = env_lock().lock().await;
        let saved_path = std::env::var("PATH").unwrap_or_default();
        let new_path = format!("{}:{}", dir.path().display(), saved_path);
        std::env::set_var("PATH", &new_path);
        std::env::remove_var("ORGASMIC_DRIVER_SIMULATE");

        let driver = AcpWsDriver::new(Box::new(ClaudeAdapter::new()));
        let mut session = driver
            .acquire(ctx("run-gate-ws-sim", RunKind::Worker), simulated_config())
            .await
            .expect("ws acquire should stay simulated for empty endpoint");
        std::env::set_var("PATH", &saved_path);

        let event = session.events.recv().await.expect("simulated Ready");
        match event {
            DriverEvent::Ready { capabilities, .. } => assert_eq!(
                capabilities["simulated"],
                serde_json::Value::Bool(true),
                "ws + empty endpoint must remain simulated"
            ),
            other => panic!("expected Ready, got {other:?}"),
        }
    }

    /// Endpoint-empty + ACP-stdio + missing `claude` remains simulated because
    /// the stdio upgrade only fires when the spawn command is available.
    #[tokio::test]
    async fn gate_simulates_for_empty_endpoint_stdio_when_claude_missing() {
        let _guard = env_lock().lock().await;
        let saved_path = std::env::var("PATH").unwrap_or_default();
        std::env::set_var("PATH", "");
        std::env::remove_var("ORGASMIC_DRIVER_SIMULATE");

        let driver = AcpStdioDriver::new(Box::new(ClaudeAdapter::new()));
        let mut session = driver
            .acquire(ctx("run-gate-missing", RunKind::Worker), simulated_config())
            .await
            .expect("stdio acquire should fall back to simulated when claude is missing");
        std::env::set_var("PATH", &saved_path);

        let event = session.events.recv().await.expect("simulated Ready");
        match event {
            DriverEvent::Ready { capabilities, .. } => assert_eq!(
                capabilities["simulated"],
                serde_json::Value::Bool(true),
                "stdio + missing claude must remain simulated"
            ),
            other => panic!("expected Ready, got {other:?}"),
        }
    }

    /// ORGASMIC_DRIVER_SIMULATE=1 must force simulated mode even when claude
    /// would be detectable on PATH.
    #[test]
    fn gate_simulate_when_env_var_set() {
        let _guard = env_lock().blocking_lock();
        std::env::set_var("ORGASMIC_DRIVER_SIMULATE", "1");

        let mut adapter = ClaudeAdapter::new();
        let result =
            adapter.compose_request(&ctx("run-gate-sim", RunKind::Worker), &simulated_config());

        std::env::remove_var("ORGASMIC_DRIVER_SIMULATE");

        let request = result.expect("compose_request should succeed");
        match request {
            HarnessRequest::Simulated { events } => {
                let ready = events
                    .into_iter()
                    .find(|e| matches!(e, DriverEvent::Ready { .. }));
                let DriverEvent::Ready { capabilities, .. } =
                    ready.expect("Simulated must emit Ready")
                else {
                    panic!("first event is not Ready");
                };
                assert_eq!(
                    capabilities["simulated"],
                    serde_json::Value::Bool(true),
                    "simulated mode must set capabilities.simulated=true"
                );
            }
            other => panic!("expected Simulated, got {other:?}"),
        }
    }

    /// Real-Claude smoke. Skipped on hosts without `claude` on PATH so CI
    /// without Claude Code still exercises simulated mode only. When
    /// present, use an invalid API key in `--bare` mode so the bridge
    /// verifies stdio JSONL parsing without spending tokens.
    #[tokio::test]
    async fn real_claude_stream_json_bridge_reports_auth_error() {
        if !claude_available() {
            eprintln!(
                "skipping real_claude_stream_json_bridge_reports_auth_error: claude not on PATH"
            );
            return;
        }
        // Hold the env lock to prevent simulated_acquire_emits_ready_and_release
        // from setting ORGASMIC_DRIVER_SIMULATE=1 while this test is running.
        let _guard = env_lock().lock().await;
        std::env::set_var("ORGASMIC_TEST_CLAUDE_ACP_KEY", "invalid");
        let d = ClaudeAcpDriver;
        let cfg = DriverConfig::from_value(json!({
            "endpoint": "stdio",
            "api_key_env": "ORGASMIC_TEST_CLAUDE_ACP_KEY",
            "model": "sonnet"
        }));
        let mut s = d
            .acquire(ctx("run-real-claude", RunKind::Worker), cfg)
            .await
            .unwrap();

        let mut saw_ready = false;
        let mut saw_failure = false;
        for _ in 0..10 {
            let ev = timeout(Duration::from_secs(15), s.events.recv())
                .await
                .expect("real claude smoke timed out")
                .expect("event stream closed before auth failure");
            match ev {
                DriverEvent::Ready { capabilities, .. } => {
                    saw_ready = capabilities["simulated"] == false;
                }
                DriverEvent::RunFail { error_code, .. } => {
                    saw_failure = error_code.starts_with("claude_api_error_")
                        || error_code == "claude_result_error";
                }
                DriverEvent::DriverError { fatal, message } => {
                    saw_failure = saw_failure || (fatal && message.contains("authentication"));
                }
                _ => {}
            }
            if saw_ready && saw_failure {
                break;
            }
        }
        let _ = s.control.release("test cleanup").await;
        assert!(
            saw_ready,
            "real claude bridge should emit non-simulated Ready"
        );
        assert!(
            saw_failure,
            "invalid API key should surface as driver failure"
        );
    }
}
