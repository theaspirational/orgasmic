//! Dedicated RunDock Chat adapters for the TypeScript provider SDK host.
//!
//! This module is intentionally separate from `claude.rs`: the latter is the
//! frozen worker/dispatch `claude -p` transport, while this adapter speaks a
//! small JSONL control protocol to the official Claude Agent SDK or OpenCode
//! SDK runtime.

use std::collections::BTreeMap;
use std::path::PathBuf;

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use orgasmic_core::{DriverEvent, ProviderRuntimeEvent, ProviderRuntimeEventKind, TextStream};

use crate::r#trait::{
    DriverConfig, DriverContext, DriverError, HarnessControlOutcome, HarnessEventAdapter,
    HarnessRequest, UserInputRequest,
};
use crate::runtime_options::{
    RuntimeModelOption, RuntimeOptionsCatalog, RuntimeOptionsRequest, RuntimeOptionsState,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChatSdkProvider {
    Claude,
    OpenCode,
}

impl ChatSdkProvider {
    pub fn id(self) -> &'static str {
        match self {
            Self::Claude => "claude",
            Self::OpenCode => "opencode",
        }
    }

    fn harness(self) -> &'static str {
        match self {
            Self::Claude => "claude-sdk",
            Self::OpenCode => "opencode",
        }
    }
}

#[derive(Debug, Clone)]
pub struct ProviderHostInvocation {
    pub binary: String,
    pub leading_args: Vec<String>,
}

/// Resolve the SDK host without requiring it to be installed globally.
///
/// Source builds use the checked-in TypeScript entrypoint (Node 22 strips its
/// erasable types). Runtime bundles carry a self-contained Bun sidecar under
/// `libexec`; `ORGASMIC_PROVIDER_HOST` remains an explicit override for custom
/// packaging.
pub fn provider_host_invocation() -> Result<ProviderHostInvocation, String> {
    if let Some(path) = std::env::var_os("ORGASMIC_PROVIDER_HOST") {
        return invocation_for_path(PathBuf::from(path));
    }

    if let Ok(executable) = std::env::current_exe() {
        if let Some(bin_dir) = executable.parent() {
            let bundled = bin_dir
                .parent()
                .unwrap_or(bin_dir)
                .join("libexec")
                .join(provider_host_sidecar_name());
            if bundled.is_file() {
                return invocation_for_path(bundled);
            }
        }
    }

    // Bundle installs copy the public CLI into `$ORGASMIC_HOME/bin`, while the
    // immutable runtime payload (including libexec) stays behind the `current`
    // symlink. Resolve that layout explicitly instead of assuming the copied
    // executable remains adjacent to its runtime.
    if let Ok(home) = orgasmic_core::Home::from_env() {
        let installed = installed_provider_host_path(&home);
        if installed.is_file() {
            return invocation_for_path(installed);
        }
    }

    let source = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../provider-host/src/index.ts");
    if source.is_file() {
        return invocation_for_path(source);
    }

    Err("Orgasmic Chat SDK host was not found; run `npm install` in provider-host or set ORGASMIC_PROVIDER_HOST".into())
}

fn provider_host_sidecar_name() -> &'static str {
    if cfg!(windows) {
        "orgasmic-provider-host.exe"
    } else {
        "orgasmic-provider-host"
    }
}

fn installed_provider_host_path(home: &orgasmic_core::Home) -> PathBuf {
    home.current_runtime()
        .join("libexec")
        .join(provider_host_sidecar_name())
}

fn invocation_for_path(path: PathBuf) -> Result<ProviderHostInvocation, String> {
    if !path.is_file() {
        return Err(format!("Chat SDK host does not exist: {}", path.display()));
    }
    if path.extension().and_then(|value| value.to_str()) == Some("ts")
        || path.extension().and_then(|value| value.to_str()) == Some("js")
        || path.extension().and_then(|value| value.to_str()) == Some("mjs")
    {
        return Ok(ProviderHostInvocation {
            binary: std::env::var("ORGASMIC_PROVIDER_HOST_NODE").unwrap_or_else(|_| "node".into()),
            leading_args: vec![path.display().to_string()],
        });
    }
    Ok(ProviderHostInvocation {
        binary: path.display().to_string(),
        leading_args: Vec::new(),
    })
}

#[derive(Debug, Clone, Deserialize, Default)]
struct ChatSdkConfig {
    #[serde(default)]
    cwd: Option<PathBuf>,
    #[serde(default)]
    model: Option<String>,
    #[serde(default)]
    reasoning_effort: Option<String>,
    #[serde(default)]
    access: Option<String>,
    #[serde(default)]
    service_tier: Option<String>,
    #[serde(default)]
    sandbox_permissions: Option<String>,
}

#[derive(Debug, Serialize)]
struct HostCommand<'a> {
    #[serde(rename = "type")]
    kind: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    text: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    reason: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    model: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    effort: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    access: Option<&'a str>,
    #[serde(rename = "serviceTier", skip_serializing_if = "Option::is_none")]
    service_tier: Option<&'a str>,
}

pub struct ChatSdkAdapter {
    provider: ChatSdkProvider,
    ready: bool,
    terminal_emitted: bool,
    text_seq: u64,
    turn_seq: u64,
    cfg: Option<ChatSdkConfig>,
}

impl ChatSdkAdapter {
    pub fn new(provider: ChatSdkProvider) -> Self {
        Self {
            provider,
            ready: false,
            terminal_emitted: false,
            text_seq: 0,
            turn_seq: 0,
            cfg: None,
        }
    }

    fn json_line<T: Serialize>(value: &T) -> Result<Vec<u8>, DriverError> {
        let mut payload = serde_json::to_vec(value)
            .map_err(|error| DriverError::Other(format!("serialize Chat SDK command: {error}")))?;
        payload.push(b'\n');
        Ok(payload)
    }

    fn events_for_runtime(&mut self, event: ProviderRuntimeEvent) -> Vec<DriverEvent> {
        let mut events = vec![DriverEvent::ProviderRuntime {
            event: Box::new(event.clone()),
        }];
        match &event.kind {
            ProviderRuntimeEventKind::SessionStarted(_) if !self.ready => {
                self.ready = true;
                events.insert(
                    0,
                    DriverEvent::Ready {
                        protocol_version: "orgasmic-provider-runtime/1".into(),
                        capabilities: json!({
                            "canonical_events": true,
                            "provider": self.provider.id(),
                            "sdk": true,
                        }),
                    },
                );
            }
            ProviderRuntimeEventKind::TurnCompleted(_) => {
                events.push(DriverEvent::AgentTurnComplete { seq: self.turn_seq });
                self.turn_seq += 1;
            }
            ProviderRuntimeEventKind::RuntimeError(payload)
                if !self.ready || payload.class.as_deref() == Some("transport_error") =>
            {
                events.push(DriverEvent::DriverError {
                    fatal: true,
                    message: payload
                        .message
                        .clone()
                        .unwrap_or_else(|| "Chat SDK provider failed to start".into()),
                });
                self.terminal_emitted = true;
            }
            _ => {}
        }
        events
    }
}

#[async_trait]
impl HarnessEventAdapter for ChatSdkAdapter {
    fn harness(&self) -> &'static str {
        self.provider.harness()
    }

    fn clone_box(&self) -> Box<dyn HarnessEventAdapter> {
        Box::new(Self::new(self.provider))
    }

    fn validate_config(&self, config: &DriverConfig) -> Result<(), DriverError> {
        serde_json::from_value::<ChatSdkConfig>(config.0.clone())
            .map_err(|error| DriverError::InvalidConfig(error.to_string()))?;
        provider_host_invocation().map_err(DriverError::InvalidConfig)?;
        Ok(())
    }

    fn compose_request(
        &mut self,
        ctx: &DriverContext,
        config: &DriverConfig,
    ) -> Result<HarnessRequest, DriverError> {
        self.validate_config(config)?;
        let cfg: ChatSdkConfig = serde_json::from_value(config.0.clone())
            .map_err(|error| DriverError::InvalidConfig(error.to_string()))?;
        self.cfg = Some(cfg.clone());
        let invocation = provider_host_invocation().map_err(DriverError::InvalidConfig)?;
        let cwd = cfg
            .cwd
            .or_else(|| ctx.worktree.clone())
            .unwrap_or(std::env::current_dir().map_err(DriverError::Io)?);
        let mut args = invocation.leading_args;
        args.extend([
            "session".into(),
            "--provider".into(),
            self.provider.id().into(),
            "--thread-id".into(),
            ctx.identity.runtime_id.clone(),
            "--cwd".into(),
            cwd.display().to_string(),
            "--access".into(),
            cfg.access.unwrap_or_else(|| "full-access".into()),
        ]);
        if let Some(model) = cfg.model {
            args.extend(["--model".into(), model]);
        }
        if let Some(effort) = cfg.reasoning_effort {
            args.extend(["--effort".into(), effort]);
        }
        if let Some(service_tier) = cfg.service_tier {
            args.extend(["--service-tier".into(), service_tier]);
        }
        if let Some(sandbox_permissions) = cfg.sandbox_permissions {
            args.extend(["--sandbox-permissions".into(), sandbox_permissions]);
        }
        Ok(HarnessRequest::Subprocess {
            binary: invocation.binary,
            args,
            env: BTreeMap::new(),
            cwd: Some(cwd),
            stdin_payload: None,
            close_stdin: false,
        })
    }

    async fn parse_event(&mut self, raw: Value) -> Vec<DriverEvent> {
        match serde_json::from_value::<ProviderRuntimeEvent>(raw) {
            Ok(event) if event.provider == self.provider.id() => self.events_for_runtime(event),
            Ok(event) => vec![DriverEvent::DriverError {
                fatal: true,
                message: format!(
                    "Chat SDK host emitted provider '{}' for '{}' session",
                    event.provider,
                    self.provider.id()
                ),
            }],
            Err(error) => vec![DriverEvent::DriverError {
                fatal: false,
                message: format!("invalid Chat SDK event: {error}"),
            }],
        }
    }

    async fn send_input(
        &mut self,
        req: UserInputRequest,
    ) -> Result<HarnessControlOutcome, DriverError> {
        let input = req.input.trim();
        if input.is_empty() {
            return Err(DriverError::InvalidConfig("input must not be empty".into()));
        }
        let payload = Self::json_line(&HostCommand {
            kind: "user_input",
            text: Some(input),
            reason: None,
            model: None,
            effort: None,
            access: None,
            service_tier: None,
        })?;
        let seq = self.text_seq;
        self.text_seq += 1;
        Ok(HarnessControlOutcome {
            events: vec![DriverEvent::TextChunk {
                stream: TextStream::User,
                chunk: input.to_string(),
                seq,
            }],
            stdin_payloads: vec![payload],
            ..HarnessControlOutcome::default()
        })
    }

    async fn switch_runtime_options(
        &mut self,
        req: RuntimeOptionsRequest,
    ) -> Result<HarnessControlOutcome, DriverError> {
        let req = req.normalized().map_err(DriverError::InvalidConfig)?;
        if let Some(cfg) = self.cfg.as_mut() {
            if let Some(model) = req.model.as_ref() {
                cfg.model = Some(model.clone());
            }
            if let Some(effort) = req.reasoning_effort.as_ref() {
                cfg.reasoning_effort = Some(effort.clone());
            }
        }
        let speed = req.speed.map(|speed| speed.as_str());
        Ok(HarnessControlOutcome {
            stdin_payloads: vec![Self::json_line(&HostCommand {
                kind: "set_options",
                text: None,
                reason: None,
                model: req.model.as_deref(),
                effort: req.reasoning_effort.as_deref(),
                access: None,
                service_tier: speed,
            })?],
            ..HarnessControlOutcome::default()
        })
    }

    async fn runtime_options_catalog(&mut self) -> Result<RuntimeOptionsCatalog, DriverError> {
        #[derive(Deserialize)]
        #[serde(rename_all = "camelCase")]
        struct HostModel {
            id: String,
            label: String,
            #[serde(default)]
            reasoning_efforts: Vec<String>,
        }
        #[derive(Deserialize)]
        struct HostCatalog {
            source: String,
            #[serde(default)]
            models: Vec<HostModel>,
        }

        let invocation = provider_host_invocation().map_err(DriverError::Other)?;
        let cfg = self.cfg.clone().unwrap_or_default();
        let cwd = cfg
            .cwd
            .unwrap_or(std::env::current_dir().map_err(DriverError::Io)?);
        let mut command = tokio::process::Command::new(invocation.binary);
        command.args(invocation.leading_args).args([
            "catalog",
            "--provider",
            self.provider.id(),
            "--cwd",
            &cwd.display().to_string(),
        ]);
        let output = tokio::time::timeout(std::time::Duration::from_secs(35), command.output())
            .await
            .map_err(|_| DriverError::Transport("Chat SDK catalog probe timed out".into()))?
            .map_err(|error| DriverError::Transport(format!("Chat SDK catalog probe: {error}")))?;
        if !output.status.success() {
            return Err(DriverError::Transport(
                String::from_utf8_lossy(&output.stderr).trim().to_string(),
            ));
        }
        let catalog: HostCatalog = serde_json::from_slice(&output.stdout).map_err(|error| {
            DriverError::Transport(format!("invalid Chat SDK catalog response: {error}"))
        })?;
        let models = catalog
            .models
            .into_iter()
            .map(|model| RuntimeModelOption {
                current: cfg.model.as_deref() == Some(model.id.as_str()),
                id: model.id,
                label: model.label,
                provider: Some(self.provider.id().into()),
                reasoning_efforts: model.reasoning_efforts,
                speeds: Vec::new(),
                default_reasoning_effort: None,
            })
            .collect::<Vec<_>>();
        let efforts = models
            .iter()
            .flat_map(|model| model.reasoning_efforts.iter().cloned())
            .fold(Vec::new(), |mut values, effort| {
                if !values.contains(&effort) {
                    values.push(effort);
                }
                values
            });
        Ok(RuntimeOptionsCatalog {
            source: catalog.source,
            provider_switching: false,
            live_switching: true,
            current: RuntimeOptionsState {
                provider: Some(self.provider.id().into()),
                model: cfg.model,
                reasoning_effort: cfg.reasoning_effort,
                speed: None,
            },
            providers: Vec::new(),
            models,
            efforts,
            speeds: Vec::new(),
        })
    }

    async fn release(&mut self, reason: String) -> Result<HarnessControlOutcome, DriverError> {
        if self.terminal_emitted {
            return Ok(HarnessControlOutcome {
                close: true,
                ..HarnessControlOutcome::default()
            });
        }
        self.terminal_emitted = true;
        Ok(HarnessControlOutcome {
            events: vec![DriverEvent::RunComplete { summary: None }],
            stdin_payloads: vec![Self::json_line(&HostCommand {
                kind: "stop",
                text: None,
                reason: Some(&reason),
                model: None,
                effort: None,
                access: None,
                service_tier: None,
            })?],
            close: true,
            ..HarnessControlOutcome::default()
        })
    }

    fn terminal_emitted(&self) -> bool {
        self.terminal_emitted
    }

    fn ignores_stderr_line(&self, _line: &str) -> bool {
        // Provider diagnostics are normalized on stdout as runtime.warning or
        // runtime.error. Native SDK/server logging is retained by the daemon
        // log and must not become transcript content.
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::modes::stdio::StdioComposeAdapter;
    use crate::r#trait::RunKind;
    use orgasmic_core::RuntimeIdentity;

    fn ctx() -> DriverContext {
        DriverContext {
            identity: RuntimeIdentity::new("run-chat-sdk", "boot-test"),
            run_kind: RunKind::Worker,
            task_id: "TASK-CHAT".into(),
            worker_id: "implementer-claude-sdk-stdio".into(),
            project_id: Some("orgasmic".into()),
            worktree: Some(PathBuf::from("/tmp/orgasmic-chat-sdk")),
        }
    }

    #[test]
    fn installed_provider_host_lives_under_current_runtime() {
        let home = orgasmic_core::Home::at("/tmp/orgasmic-home");
        assert_eq!(
            installed_provider_host_path(&home),
            home.current_runtime()
                .join("libexec")
                .join(provider_host_sidecar_name())
        );
    }

    #[test]
    fn compose_request_forwards_dispatch_sandbox_permissions() {
        let mut adapter = ChatSdkAdapter::new(ChatSdkProvider::Claude);
        let request = adapter
            .compose_request(
                &ctx(),
                &DriverConfig::from_value(json!({
                    "access": "supervised",
                    "sandbox_permissions": "read,patch"
                })),
            )
            .expect("compose Chat SDK request");

        let HarnessRequest::Subprocess { args, .. } = request else {
            panic!("Chat SDK request should spawn a subprocess");
        };
        assert!(args
            .windows(2)
            .any(|pair| { pair == ["--sandbox-permissions", "read,patch"] }));
    }

    #[test]
    fn stdio_wrapper_accepts_chat_sdk_composed_subprocess() {
        let mut adapter = StdioComposeAdapter {
            inner: Box::new(ChatSdkAdapter::new(ChatSdkProvider::OpenCode)),
            jsonrpc_session_init: None,
        };
        let request = adapter
            .compose_request(
                &ctx(),
                &DriverConfig::from_value(json!({
                    "model": "zai-coding-plan/glm-5.3",
                    "reasoning_effort": "high",
                    "access": "full-access"
                })),
            )
            .expect("stdio should accept the complete provider-host subprocess request");

        let HarnessRequest::Subprocess { args, .. } = request else {
            panic!("Chat SDK request should remain a subprocess");
        };
        assert!(args
            .windows(2)
            .any(|pair| { pair == ["--model", "zai-coding-plan/glm-5.3"] }));
    }

    #[tokio::test]
    async fn session_start_emits_ready_before_the_canonical_event() {
        let mut adapter = ChatSdkAdapter::new(ChatSdkProvider::Claude);
        let events = adapter
            .parse_event(json!({
                "eventId": "event-1",
                "provider": "claude",
                "threadId": "thread-1",
                "createdAt": "2026-08-15T00:00:00Z",
                "type": "session.started",
                "payload": { "message": "ready" }
            }))
            .await;

        assert!(matches!(events.first(), Some(DriverEvent::Ready { .. })));
        assert!(matches!(
            events.get(1),
            Some(DriverEvent::ProviderRuntime { event })
                if matches!(event.kind, ProviderRuntimeEventKind::SessionStarted(_))
        ));
    }

    #[tokio::test]
    async fn completed_turn_is_a_reusable_boundary_not_a_run_terminal() {
        let mut adapter = ChatSdkAdapter::new(ChatSdkProvider::OpenCode);
        let events = adapter
            .parse_event(json!({
                "eventId": "event-2",
                "provider": "opencode",
                "threadId": "thread-2",
                "turnId": "turn-1",
                "createdAt": "2026-08-15T00:00:01Z",
                "type": "turn.completed",
                "payload": { "state": "completed" }
            }))
            .await;

        assert!(matches!(
            events.first(),
            Some(DriverEvent::ProviderRuntime { event })
                if matches!(event.kind, ProviderRuntimeEventKind::TurnCompleted(_))
        ));
        assert!(matches!(
            events.get(1),
            Some(DriverEvent::AgentTurnComplete { seq: 0 })
        ));
        assert!(!events.iter().any(|event| matches!(
            event,
            DriverEvent::RunComplete { .. } | DriverEvent::RunFail { .. }
        )));
        assert!(!adapter.terminal_emitted());
    }

    #[tokio::test]
    async fn transport_error_after_ready_is_fatal() {
        let mut adapter = ChatSdkAdapter::new(ChatSdkProvider::OpenCode);
        let _ = adapter
            .parse_event(json!({
                "eventId": "event-ready",
                "provider": "opencode",
                "threadId": "thread-3",
                "createdAt": "2026-08-15T00:00:00Z",
                "type": "session.started",
                "payload": {}
            }))
            .await;
        let events = adapter
            .parse_event(json!({
                "eventId": "event-dead",
                "provider": "opencode",
                "threadId": "thread-3",
                "createdAt": "2026-08-15T00:00:01Z",
                "type": "runtime.error",
                "payload": { "message": "SSE ended", "class": "transport_error" }
            }))
            .await;

        assert!(events.iter().any(|event| matches!(
            event,
            DriverEvent::DriverError { fatal: true, message } if message == "SSE ended"
        )));
        assert!(adapter.terminal_emitted());
    }
}
