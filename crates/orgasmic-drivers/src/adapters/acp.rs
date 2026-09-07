//! One ACP client for native agents and the pinned Codex/Claude ACP translators.
//! Reuses Orgasmic's owned stdio JSON-RPC transport and sandbox policy.
use super::cursor_acp::CursorAcpAdapter;
use crate::r#trait::*;
use crate::runtime_options::*;
use crate::sandbox::{ApprovalResponse, SandboxAllowlist};
use async_trait::async_trait;
use orgasmic_core::{
    DriverEvent, ProviderDiagnosticPayload, ProviderRuntimeEvent, ProviderRuntimeEventKind,
    TextStream,
};
use serde_json::{json, Value};

pub const PROVIDERS: &[&str] = &["codex", "claude", "opencode", "cursor-agent", "hermes"];

pub struct AcpAdapter {
    provider: &'static str,
    chat: bool,
    config: DriverConfig,
    policy: CursorAcpAdapter,
    session: Option<String>,
    catalog: Value,
    seq: u64,
    finished: bool,
    released: bool,
}
impl AcpAdapter {
    pub fn new(provider: &str, chat: bool) -> Option<Self> {
        Some(Self {
            provider: *PROVIDERS.iter().find(|p| **p == provider)?,
            chat,
            config: DriverConfig::empty(),
            policy: CursorAcpAdapter::new(),
            session: None,
            catalog: Value::Null,
            seq: 0,
            finished: false,
            released: false,
        })
    }
    fn record(&self, method: &str, value: Value) -> DriverEvent {
        DriverEvent::Acp {
            provider: self.provider.into(),
            message: json!({"method":method,"params":value}),
        }
    }
    fn prompt(&self, text: &str) -> Result<Value, DriverError> {
        let session = self
            .session
            .as_ref()
            .ok_or_else(|| DriverError::Transport("ACP session is not ready".into()))?;
        Ok(json!({"sessionId":session,"prompt":[{"type":"text","text":text}]}))
    }
    fn options(&self) -> &[Value] {
        self.catalog["configOptions"]
            .as_array()
            .map(Vec::as_slice)
            .unwrap_or(&[])
    }
    fn option(&self, category: &str) -> Option<&Value> {
        self.options().iter().find(|o| {
            o["category"] == category
                || o["id"] == category
                || (category == "speed" && (o["id"] == "fast-mode" || o["id"] == "fast"))
        })
    }
    fn selection(&self, category: &str, requested: &str) -> Result<WireMessage, DriverError> {
        let session = self
            .session
            .as_ref()
            .ok_or_else(|| DriverError::Transport("ACP session is not ready".into()))?;
        if let Some(option) = self.option(category) {
            let requested = if category == "speed"
                && matches!(option["id"].as_str(), Some("fast-mode" | "fast"))
            {
                match requested {
                    "fast" => "on",
                    "normal" => "off",
                    other => other,
                }
            } else {
                requested
            };
            let choices = option_values(&option["options"]);
            let selected = choices
                .iter()
                .find(|o| o["value"] == requested || o["name"] == requested)
                .ok_or_else(|| {
                    DriverError::InvalidConfig(format!(
                        "ACP {} does not offer {category}={requested}",
                        self.provider
                    ))
                })?;
            return Ok(WireMessage::JsonRpc {
                method: "session/set_config_option".into(),
                params: json!({"sessionId":session,"configId":option["id"],"value":selected["value"]}),
            });
        }
        if category == "mode"
            && self.catalog["modes"]["availableModes"]
                .as_array()
                .is_some_and(|modes| modes.iter().any(|m| m["id"] == requested))
        {
            return Ok(WireMessage::JsonRpc {
                method: "session/set_mode".into(),
                params: json!({"sessionId":session,"modeId":requested}),
            });
        }
        if category == "model" {
            let offered = self.catalog["models"]["availableModels"].as_array();
            if offered.is_some_and(|models| models.iter().any(|m| m["modelId"] == requested)) {
                return Ok(WireMessage::JsonRpc {
                    method: "session/set_model".into(),
                    params: json!({"sessionId":session,"modelId":requested}),
                });
            }
        }
        Err(DriverError::InvalidConfig(format!(
            "ACP {} does not advertise {category}={requested}",
            self.provider
        )))
    }
}
fn option_values(value: &Value) -> Vec<&Value> {
    value
        .as_array()
        .into_iter()
        .flatten()
        .flat_map(|v| {
            if v.get("options").is_some() {
                option_values(&v["options"])
            } else {
                vec![v]
            }
        })
        .collect()
}
#[async_trait]
impl HarnessEventAdapter for AcpAdapter {
    fn harness(&self) -> &'static str {
        if self.chat {
            match self.provider {
                "codex" => "codex-chat",
                "claude" => "claude-sdk",
                "cursor-agent" => "cursor-acp-chat",
                "hermes" => "hermes-acp-chat",
                p => p,
            }
        } else {
            self.provider
        }
    }
    fn clone_box(&self) -> Box<dyn HarnessEventAdapter> {
        Box::new(Self::new(self.provider, self.chat).unwrap())
    }
    fn validate_config(&self, config: &DriverConfig) -> Result<(), DriverError> {
        crate::sandbox::allowlist_from_driver_config(config)
            .map_err(|e| DriverError::InvalidConfig(e.to_string()))?;
        Ok(())
    }
    fn stdio_spawn(&self) -> Option<StdioSpawn> {
        let (command, args) = match self.provider {
            "codex" => (
                "npx",
                vec![
                    "--yes",
                    "--package",
                    "@agentclientprotocol/codex-acp@1.10.0",
                    "codex-acp",
                ],
            ),
            "claude" => (
                "npx",
                vec![
                    "--yes",
                    "--package",
                    "@agentclientprotocol/claude-agent-acp@0.75.1",
                    "claude-agent-acp",
                ],
            ),
            provider => (provider, vec!["acp"]),
        };
        Some(StdioSpawn {
            command: command.into(),
            args: args.into_iter().map(str::to_string).collect(),
            cwd: None,
            env: Vec::new(),
        })
    }
    fn upgrades_simulated_to_subprocess(&self) -> bool {
        true
    }
    fn compose_request(
        &mut self,
        ctx: &DriverContext,
        config: &DriverConfig,
    ) -> Result<HarnessRequest, DriverError> {
        self.validate_config(config)?;
        self.config = config.clone();
        self.policy.stdio_session_init(ctx, config)?;
        let command = self.stdio_spawn().unwrap().command;
        if !super::claude::executable_on_path(&command) {
            return Err(DriverError::InvalidConfig(format!(
                "ACP launcher {command} is not installed"
            )));
        }
        Ok(HarnessRequest::Simulated { events: Vec::new() })
    }
    fn stdio_session_init(
        &mut self,
        ctx: &DriverContext,
        config: &DriverConfig,
    ) -> Result<Value, DriverError> {
        self.config = config.clone();
        self.policy.stdio_session_init(ctx, config)?;
        let mut post = Vec::new();
        if let Some(access) = config.0["access"].as_str() {
            let mode = match (self.provider, access) {
                ("codex", "full-access") => "agent-full-access",
                ("codex", "auto") => "agent",
                ("codex", "auto-accept-edits" | "supervised") => "read-only",
                ("claude", "full-access") => "bypassPermissions",
                ("claude", "auto") => "auto",
                ("claude", "auto-accept-edits") => "acceptEdits",
                ("claude", "supervised") => "default",
                ("hermes", "full-access") => "dont_ask",
                ("hermes", "auto") => "default",
                ("cursor-agent", "auto" | "full-access") => "agent",
                ("opencode", "auto" | "full-access") => "build",
                _ => {
                    return Err(DriverError::InvalidConfig(format!(
                        "Unsupported ACP access mode {access}"
                    )))
                }
            };
            post.push(json!({"method":"session/set_config_option","params":{"category":"mode","value":mode}}));
        }
        for (category, value) in [
            ("model", config.0["model"].as_str()),
            (
                "thought_level",
                config.0["reasoning_effort"]
                    .as_str()
                    .or(config.0["effort"].as_str()),
            ),
            (
                "speed",
                config.0["speed"]
                    .as_str()
                    .or(config.0["service_tier"].as_str()),
            ),
        ] {
            if let Some(value) = value.filter(|v| !v.is_empty()) {
                post.push(json!({"method":"session/set_config_option","params":{"category":category,"value":value}}));
            }
        }
        Ok(json!({
            "initialize":{"protocolVersion":1,"clientInfo":{"name":"orgasmic","version":env!("CARGO_PKG_VERSION")},"clientCapabilities":{}},
            "thread_start":{"cwd":ctx.worktree.as_ref().map(|p|p.display().to_string()).unwrap_or_else(||"/".into()),"mcpServers":[]},
            "post_session":post,"auto_turn":config.0["auto_start_turn"].as_bool().unwrap_or(true)
        }))
    }
    fn jsonrpc_session_start_method(&self) -> &'static str {
        "session/new"
    }
    fn jsonrpc_turn_start_method(&self) -> &'static str {
        "session/prompt"
    }
    async fn on_ws_thread_started(
        &mut self,
        _endpoint: &str,
        response: &Value,
    ) -> Result<Vec<DriverEvent>, DriverError> {
        self.session = Some(
            response["sessionId"]
                .as_str()
                .ok_or_else(|| DriverError::Transport("ACP session/new omitted sessionId".into()))?
                .into(),
        );
        self.catalog = response.clone();
        Ok(vec![
            DriverEvent::Ready {
                protocol_version: "acp/1".into(),
                capabilities: json!({"canonical_events":true,"provider":self.provider,"session_id":self.session,"acp":true}),
            },
            self.record("session/new", response.clone()),
        ])
    }
    fn jsonrpc_post_session_request(
        &mut self,
        _method: &str,
        params: Value,
    ) -> Result<(String, Value), DriverError> {
        let category = params["category"].as_str().unwrap_or("model");
        let value = params["value"].as_str().unwrap_or_default();
        match self.selection(category, value)? {
            WireMessage::JsonRpc { method, params } => Ok((method, params)),
            _ => unreachable!(),
        }
    }
    fn ws_turn_start_params(&mut self) -> Result<Value, DriverError> {
        self.prompt(
            self.config.0["prompt_bundle_text"]
                .as_str()
                .unwrap_or_default(),
        )
    }
    async fn parse_event(&mut self, raw: Value) -> Vec<DriverEvent> {
        let method = raw["method"].as_str().unwrap_or_default();
        if method.is_empty() {
            return Vec::new();
        }
        let mut events = Vec::new();
        let update = &raw["params"]["update"];
        let text = update["content"]["text"].as_str().unwrap_or_default();
        if method == "session/update"
            && update["content"]["type"] == "text"
            && matches!(
                update["sessionUpdate"].as_str(),
                Some("agent_message_chunk" | "agent_thought_chunk" | "user_message_chunk")
            )
            && text.len() > orgasmic_core::DRIVER_EVENT_PAYLOAD_CAP_BYTES / 8
        {
            // Keep all model prose even when a vendor emits one very large chunk.
            // Leave room for JSON escapes and enclosing objects. The session writer
            // still bounds tool payloads and unknown extensions.
            let mut remaining = text;
            while !remaining.is_empty() {
                let mut end = remaining
                    .len()
                    .min(orgasmic_core::DRIVER_EVENT_PAYLOAD_CAP_BYTES / 8);
                while !remaining.is_char_boundary(end) {
                    end -= 1;
                }
                let mut params = raw["params"].clone();
                params["update"]["content"]["text"] = json!(&remaining[..end]);
                events.push(self.record(method, params));
                remaining = &remaining[end..];
            }
        } else {
            events.push(self.record(method, raw["params"].clone()));
        }
        if method == "session/update" {
            if let Err(error) = serde_json::from_value::<
                agent_client_protocol_schema::v1::SessionNotification,
            >(raw["params"].clone())
            {
                events.push(DriverEvent::ProviderRuntime {
                    event: Box::new(ProviderRuntimeEvent {
                        event_id: uuid::Uuid::new_v4().to_string(),
                        provider: self.provider.into(),
                        thread_id: self.session.clone().unwrap_or_default(),
                        created_at: chrono::Utc::now().to_rfc3339(),
                        turn_id: None,
                        item_id: None,
                        request_id: None,
                        provider_refs: None,
                        raw: None,
                        kind: ProviderRuntimeEventKind::RuntimeWarning(ProviderDiagnosticPayload {
                            message: Some(format!(
                                "Unrecognized ACP update retained for inspection: {error}"
                            )),
                            ..Default::default()
                        }),
                    }),
                });
            }
            if raw["params"]["update"]["sessionUpdate"] == "config_option_update" {
                self.catalog["configOptions"] = raw["params"]["update"]["configOptions"].clone();
            }
        }
        events
    }
    async fn on_ws_response(
        &mut self,
        method: &str,
        response: Value,
    ) -> Result<Vec<DriverEvent>, DriverError> {
        if response.get("configOptions").is_some() {
            self.catalog["configOptions"] = response["configOptions"].clone();
        }
        let mut events = vec![self.record(method, response.clone())];
        if method == "session/prompt" {
            self.finished = true;
            events.push(crate::r#trait::agent_turn_complete(self.next_seq()));
            if !self.chat {
                events.push(match response["stopReason"].as_str() {
                    Some("end_turn" | "max_tokens") => DriverEvent::RunComplete { summary: None },
                    _ => DriverEvent::RunFail {
                        error_code: "acp_turn_stopped".into(),
                        error_markdown: format!("ACP turn stopped: {}", response["stopReason"]),
                    },
                });
            }
        }
        Ok(events)
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
        let allowed = if self.chat {
            self.policy.chat_permission_allowed(params, allowlist)
        } else {
            self.policy.permission_allowed(params, allowlist)
        };
        let desired = if allowed { "allow_once" } else { "reject_once" };
        let option = params["options"]
            .as_array()
            .into_iter()
            .flatten()
            .find(|o| o["kind"] == desired);
        Some(ApprovalResponse::Acp {
            option_id: option
                .and_then(|o| o["optionId"].as_str())
                .map(str::to_string),
            allowed,
        })
    }
    async fn send_input(
        &mut self,
        req: UserInputRequest,
    ) -> Result<HarnessControlOutcome, DriverError> {
        if req.input.trim().is_empty() {
            return Err(DriverError::InvalidConfig("input must not be empty".into()));
        }
        self.finished = false;
        Ok(HarnessControlOutcome {
            events: vec![DriverEvent::TextChunk {
                stream: TextStream::User,
                chunk: req.input.clone(),
                seq: self.next_seq(),
            }],
            wire_messages: vec![WireMessage::JsonRpc {
                method: "session/prompt".into(),
                params: self.prompt(&req.input)?,
            }],
            ..Default::default()
        })
    }
    async fn transition_state(
        &mut self,
        req: TransitionRequest,
    ) -> Result<HarnessControlOutcome, DriverError> {
        self.send_input(UserInputRequest {
            input: format!("Transition from {} to {}: {}", req.from, req.to, req.reason),
        })
        .await
    }
    async fn switch_runtime_options(
        &mut self,
        req: RuntimeOptionsRequest,
    ) -> Result<HarnessControlOutcome, DriverError> {
        if req.provider.as_deref().is_some_and(|p| p != self.provider) {
            return Err(DriverError::InvalidConfig(
                "Switch providers by starting a new ACP chat".into(),
            ));
        }
        if self.option("model").is_none() {
            return Err(DriverError::Unsupported(
                "live ACP configuration; start a new chat to choose another model",
            ));
        }
        let mut wire_messages = Vec::new();
        for (category, value) in [
            ("model", req.model),
            ("thought_level", req.reasoning_effort),
            ("speed", req.speed.map(|v| v.to_string())),
        ] {
            if let Some(value) = value {
                wire_messages.push(self.selection(category, &value)?);
            }
        }
        Ok(HarnessControlOutcome {
            wire_messages,
            ..Default::default()
        })
    }
    async fn runtime_options_catalog(&mut self) -> Result<RuntimeOptionsCatalog, DriverError> {
        let model = self.option("model");
        let current_model = model
            .and_then(|o| o["currentValue"].as_str())
            .or(self.config.0["model"].as_str())
            .or(self.catalog["models"]["currentModelId"].as_str())
            .map(str::to_string);
        let efforts: Vec<String> = self
            .option("thought_level")
            .map(|o| {
                option_values(&o["options"])
                    .iter()
                    .filter_map(|v| v["value"].as_str().map(str::to_string))
                    .collect()
            })
            .unwrap_or_default();
        let speeds = if self.option("speed").is_some() {
            vec![RuntimeSpeed::Normal, RuntimeSpeed::Fast]
        } else {
            vec![]
        };
        let speed = self
            .option("speed")
            .and_then(|o| match o["currentValue"].as_str() {
                Some("on" | "fast") => Some(RuntimeSpeed::Fast),
                Some("off" | "normal") => Some(RuntimeSpeed::Normal),
                _ => None,
            });
        let models = if let Some(option) = model {
            option_values(&option["options"])
        } else {
            self.catalog["models"]["availableModels"]
                .as_array()
                .map(|v| v.iter().collect())
                .unwrap_or_default()
        };
        Ok(RuntimeOptionsCatalog {
            source: format!("{}-acp:session/new", self.provider),
            provider_switching: false,
            live_switching: model.is_some(),
            current: RuntimeOptionsState {
                model: current_model.clone(),
                speed,
                reasoning_effort: self
                    .option("thought_level")
                    .and_then(|o| o["currentValue"].as_str())
                    .map(str::to_string),
                ..Default::default()
            },
            models: models
                .into_iter()
                .filter_map(|m| {
                    let id = m["value"].as_str().or(m["modelId"].as_str())?;
                    Some(RuntimeModelOption {
                        id: id.into(),
                        label: m["name"].as_str().unwrap_or(id).into(),
                        provider: None,
                        current: current_model.as_deref() == Some(id),
                        reasoning_efforts: efforts.clone(),
                        speeds: speeds.clone(),
                        default_reasoning_effort: None,
                    })
                })
                .collect(),
            efforts,
            speeds,
            ..Default::default()
        })
    }
    async fn release(&mut self, _reason: String) -> Result<HarnessControlOutcome, DriverError> {
        if self.released {
            return Ok(HarnessControlOutcome {
                close: true,
                ..Default::default()
            });
        }
        self.released = true;
        let wire_messages = self
            .session
            .as_ref()
            .map(|id| {
                vec![WireMessage::Json(
                    json!({"jsonrpc":"2.0","method":"session/cancel","params":{"sessionId":id}}),
                )]
            })
            .unwrap_or_default();
        Ok(HarnessControlOutcome {
            events: vec![DriverEvent::RunComplete { summary: None }],
            wire_messages,
            close: true,
            ..Default::default()
        })
    }
    fn terminal_emitted(&self) -> bool {
        self.released || (self.finished && !self.chat)
    }
    fn next_seq(&mut self) -> u64 {
        let seq = self.seq;
        self.seq += 1;
        seq
    }
    fn ignores_stderr_line(&self, _line: &str) -> bool {
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn acp_large_unicode_prose_survives_the_real_session_writer() {
        let mut adapter = AcpAdapter::new("hermes", true).unwrap();
        let text = "All words 👋\n".repeat(5000);
        let raw = json!({"method":"session/update","params":{"sessionId":"test","update":{"sessionUpdate":"agent_message_chunk","content":{"type":"text","text":text}}}});
        let events = adapter.parse_event(raw).await;
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("session.jsonl");
        let mut writer = orgasmic_core::SessionWriter::open(
            &path,
            orgasmic_core::RuntimeIdentity::new("test", "test"),
        )
        .unwrap();
        for event in events {
            writer
                .append(
                    orgasmic_core::SessionEventKind::DriverEvent,
                    serde_json::to_value(event).unwrap(),
                )
                .unwrap();
        }
        let saved = std::fs::read_to_string(path).unwrap();
        let actual: String = saved
            .lines()
            .map(|line| {
                serde_json::from_str::<Value>(line).unwrap()["event"]["message"]["params"]["update"]
                    ["content"]["text"]
                    .as_str()
                    .unwrap()
                    .to_string()
            })
            .collect();
        assert_eq!(actual, text);
    }

    #[tokio::test]
    async fn acp_preserves_unknown_updates_and_keeps_chat_alive_between_turns() {
        let mut adapter = AcpAdapter::new("codex", true).unwrap();
        adapter
            .on_ws_thread_started("", &json!({"sessionId":"test"}))
            .await
            .unwrap();
        let raw = json!({"method":"session/update","params":{"sessionId":"test","update":{"sessionUpdate":"future_update","extra":42}}});
        let events = adapter.parse_event(raw.clone()).await;
        assert!(
            matches!(&events[0], DriverEvent::Acp { message, .. } if message["params"] == raw["params"])
        );
        assert!(events.iter().any(|e| matches!(e, DriverEvent::ProviderRuntime { event } if matches!(event.kind, ProviderRuntimeEventKind::RuntimeWarning(_)))));
        for _ in 0..2 {
            adapter
                .send_input(UserInputRequest {
                    input: "hello".into(),
                })
                .await
                .unwrap();
            let events = adapter
                .on_ws_response("session/prompt", json!({"stopReason":"end_turn"}))
                .await
                .unwrap();
            assert!(events
                .iter()
                .any(|e| matches!(e, DriverEvent::AgentTurnComplete { .. })));
            assert!(!adapter.terminal_emitted());
            assert!(!events
                .iter()
                .any(|e| matches!(e, DriverEvent::RunComplete { .. })));
        }
        let closed = adapter.release("test close".into()).await.unwrap();
        assert!(closed.close && adapter.terminal_emitted());
        assert!(matches!(
            closed.events.as_slice(),
            [DriverEvent::RunComplete { .. }]
        ));
        assert!(adapter
            .release("again".into())
            .await
            .unwrap()
            .events
            .is_empty());
    }

    #[tokio::test]
    async fn acp_uses_advertised_option_ids_and_legacy_models() {
        let mut adapter = AcpAdapter::new("hermes", true).unwrap();
        adapter.session = Some("test".into());
        adapter.catalog = json!({"models":{"availableModels":[{"modelId":"cheap"}]},"modes":{"availableModes":[{"id":"default"}]},"configOptions":[{"id":"fast-mode","category":"model_config","options":[{"value":"on","name":"Fast"},{"value":"off","name":"Normal"}]}]});
        assert!(
            matches!(adapter.selection("model", "cheap").unwrap(), WireMessage::JsonRpc { method,params } if method == "session/set_model" && params["modelId"] == "cheap")
        );
        assert!(
            matches!(adapter.selection("mode", "default").unwrap(), WireMessage::JsonRpc { method,.. } if method == "session/set_mode")
        );
        assert!(
            matches!(adapter.selection("speed", "fast").unwrap(), WireMessage::JsonRpc { params,.. } if params["configId"] == "fast-mode" && params["value"] == "on")
        );
        assert!(adapter.selection("model", "invented").is_err());
        assert!(adapter
            .switch_runtime_options(RuntimeOptionsRequest {
                provider: Some("claude".into()),
                ..Default::default()
            })
            .await
            .is_err());
        let request = json!({"toolCall":{"kind":"execute","rawInput":{"command":"pwd"}},"options":[{"kind":"allow_once","optionId":"vendor-yes-42"},{"kind":"reject_once","optionId":"vendor-no-99"}]});
        let denied = adapter
            .try_handle_approval(
                "session/request_permission",
                &request,
                &SandboxAllowlist {
                    allow_exec: false,
                    ..Default::default()
                },
            )
            .await
            .unwrap();
        assert!(
            matches!(denied, ApprovalResponse::Acp { option_id:Some(ref id), allowed:false } if id == "vendor-no-99")
        );
        let allow = SandboxAllowlist {
            allow_exec: true,
            ..Default::default()
        };
        assert!(
            matches!(adapter.try_handle_approval("session/request_permission", &request, &allow).await.unwrap(), ApprovalResponse::Acp { option_id:Some(ref id), allowed:true } if id == "vendor-yes-42")
        );
    }
}
