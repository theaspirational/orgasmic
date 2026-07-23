// arch: arch_A53QX.4
// orgasmic:arch_A53QX, dec_ASB1A
//! Transport-agnostic JSON-RPC request/response helpers for acp-ws and acp-stdio.

use async_trait::async_trait;
use serde_json::{json, Value};
use tokio::sync::mpsc;

use orgasmic_core::{DriverEvent, SandboxAllowlist};

use crate::r#trait::{DriverError, HarnessControlOutcome, HarnessEventAdapter, WireMessage};
use crate::sandbox::{approval_document_events, approval_result};

#[async_trait]
pub trait JsonRpcTransport: Send {
    async fn send_json(&mut self, value: Value) -> Result<(), DriverError>;
    async fn recv_json(&mut self) -> Result<Option<Value>, DriverError>;
}

#[derive(Debug, Clone, Default)]
pub struct RpcIds {
    next: u64,
}

impl RpcIds {
    pub fn new() -> Self {
        Self { next: 1 }
    }

    pub fn next_id(&mut self) -> u64 {
        let id = self.next;
        self.next += 1;
        id
    }
}

pub fn response_matches(value: &Value, id: u64) -> bool {
    value.get("id") == Some(&json!(id))
}

const RPC_ERROR_DATA_MAX_BYTES: usize = 3 * 1024;

/// Render server-provided JSON-RPC error data without allowing it to dominate
/// durable driver-error artifacts. The marker reports bytes omitted from the
/// serialized value; the retained prefix always ends at a UTF-8 boundary.
fn bounded_error_data(data: Option<&Value>) -> String {
    let serialized = data.unwrap_or(&Value::Null).to_string();
    if serialized.len() <= RPC_ERROR_DATA_MAX_BYTES {
        return serialized;
    }

    // The marker length depends on the omitted-byte count, so converge on the
    // prefix budget before slicing. It stabilizes once the digit count does.
    let mut marker_len = "[... 0 bytes omitted]".len();
    loop {
        let mut prefix_len = RPC_ERROR_DATA_MAX_BYTES.saturating_sub(marker_len);
        while !serialized.is_char_boundary(prefix_len) {
            prefix_len -= 1;
        }
        let omitted = serialized.len() - prefix_len;
        let marker = format!("[... {omitted} bytes omitted]");
        if marker.len() == marker_len {
            return format!("{}{marker}", &serialized[..prefix_len]);
        }
        marker_len = marker.len();
    }
}

pub fn rpc_result(method: &str, mut value: Value) -> Result<Value, DriverError> {
    if let Some(error) = value.get("error") {
        let code = error.get("code").cloned().unwrap_or(Value::Null);
        let message = error
            .get("message")
            .and_then(Value::as_str)
            .unwrap_or("JSON-RPC request failed");
        let data = bounded_error_data(error.get("data"));
        return Err(DriverError::Transport(format!(
            "{method} RPC error: code={code} message={message} data={data}"
        )));
    }
    Ok(value.get_mut("result").cloned().unwrap_or(Value::Null))
}

pub async fn emit_events(events: &mpsc::Sender<DriverEvent>, outgoing: Vec<DriverEvent>) {
    for event in outgoing {
        let _ = events.send(event).await;
    }
}

pub async fn send_driver_error(events: &mpsc::Sender<DriverEvent>, fatal: bool, message: String) {
    let _ = events
        .send(DriverEvent::DriverError { fatal, message })
        .await;
}

fn is_server_request(value: &Value) -> bool {
    value.get("method").is_some()
        && value.get("id").is_some()
        && value.get("result").is_none()
        && value.get("error").is_none()
}

/// Intercept codex sandbox approval server requests, auto-respond, and emit
/// documenting session events. Returns `true` when the message was handled.
pub async fn try_dispatch_approval(
    value: &Value,
    transport: &mut dyn JsonRpcTransport,
    adapter: &mut dyn HarnessEventAdapter,
    events: &mpsc::Sender<DriverEvent>,
    allowlist: &SandboxAllowlist,
) -> Result<bool, DriverError> {
    if !is_server_request(value) {
        return Ok(false);
    }
    let method = value
        .get("method")
        .and_then(Value::as_str)
        .unwrap_or_default();
    let params = value.get("params").unwrap_or(&Value::Null);
    let Some(decision) = adapter.try_handle_approval(method, params, allowlist).await else {
        return Ok(false);
    };
    let request_id = value.get("id").cloned().unwrap_or(Value::Null);
    let doc = approval_document_events(method, params, &request_id, decision.clone(), 0);
    emit_events(events, doc).await;
    let response = json!({
        "jsonrpc": "2.0",
        "id": request_id,
        "result": approval_result(decision),
    });
    transport
        .send_json(response)
        .await
        .map_err(|e| DriverError::Transport(format!("approval response: {e}")))?;
    Ok(true)
}

pub async fn dispatch_incoming_json(
    value: Value,
    transport: &mut dyn JsonRpcTransport,
    adapter: &mut dyn HarnessEventAdapter,
    events: &mpsc::Sender<DriverEvent>,
    allowlist: &SandboxAllowlist,
) -> Result<Vec<DriverEvent>, DriverError> {
    if try_dispatch_approval(&value, transport, adapter, events, allowlist).await? {
        return Ok(Vec::new());
    }
    Ok(adapter.parse_event(value).await)
}

pub async fn request_response(
    transport: &mut dyn JsonRpcTransport,
    ids: &mut RpcIds,
    method: &str,
    params: Value,
    events: &mpsc::Sender<DriverEvent>,
    adapter: &mut dyn HarnessEventAdapter,
    allowlist: &SandboxAllowlist,
) -> Result<Value, DriverError> {
    let id = ids.next_id();
    let request = json!({
        "jsonrpc": "2.0",
        "id": id,
        "method": method,
        "params": params,
    });
    transport
        .send_json(request)
        .await
        .map_err(|e| DriverError::Transport(format!("{method}: {e}")))?;
    loop {
        let Some(value) = transport
            .recv_json()
            .await
            .map_err(|e| DriverError::Transport(format!("{method}: {e}")))?
        else {
            return Err(DriverError::Transport(format!(
                "{method} closed before response"
            )));
        };
        if response_matches(&value, id) {
            return rpc_result(method, value);
        }
        let outgoing = dispatch_incoming_json(value, transport, adapter, events, allowlist).await?;
        emit_events(events, outgoing).await;
        if method == adapter.jsonrpc_turn_start_method() && adapter.terminal_emitted() {
            return Ok(Value::Null);
        }
    }
}

pub async fn handle_outcome(
    result: Result<HarnessControlOutcome, DriverError>,
    transport: &mut dyn JsonRpcTransport,
    events: &mpsc::Sender<DriverEvent>,
    ids: &mut RpcIds,
) -> Result<bool, DriverError> {
    let outcome = result?;
    for message in outcome.wire_messages {
        send_wire_message(transport, ids, message).await?;
    }
    emit_events(events, outcome.events).await;
    Ok(outcome.close)
}

pub async fn send_wire_message(
    transport: &mut dyn JsonRpcTransport,
    ids: &mut RpcIds,
    message: WireMessage,
) -> Result<(), DriverError> {
    let value = match message {
        WireMessage::Json(value) => value,
        WireMessage::JsonRpc { method, params } => json!({
            "jsonrpc": "2.0",
            "id": ids.next_id(),
            "method": method,
            "params": params,
        }),
    };
    transport
        .send_json(value)
        .await
        .map_err(|e| DriverError::Transport(format!("jsonrpc send: {e}")))
}

pub async fn run_jsonrpc_handshake(
    transport: &mut dyn JsonRpcTransport,
    ids: &mut RpcIds,
    peer_id: &str,
    session_init: Value,
    events: &mpsc::Sender<DriverEvent>,
    adapter: &mut dyn HarnessEventAdapter,
    allowlist: &SandboxAllowlist,
) -> Result<(), DriverError> {
    let initialize = session_init
        .get("initialize")
        .cloned()
        .unwrap_or_else(|| Value::Object(Default::default()));
    request_response(
        transport,
        ids,
        "initialize",
        initialize,
        events,
        adapter,
        allowlist,
    )
    .await?;

    let thread_start = session_init
        .get("thread_start")
        .cloned()
        .unwrap_or_else(|| Value::Object(Default::default()));
    let session_start_method = adapter.jsonrpc_session_start_method();
    let thread_response = request_response(
        transport,
        ids,
        session_start_method,
        thread_start,
        events,
        adapter,
        allowlist,
    )
    .await?;
    let outgoing = adapter
        .on_ws_thread_started(peer_id, &thread_response)
        .await?;
    emit_events(events, outgoing).await;

    // Optional post-session RPCs (e.g. cursor ACP `session/set_config_option`
    // for an explicit model override). Inject sessionId when the adapter omitted
    // it because the id is only known after session/new.
    if let Some(post_session) = session_init.get("post_session").and_then(Value::as_array) {
        let session_id = thread_response
            .get("sessionId")
            .or_else(|| thread_response.get("session_id"))
            .and_then(Value::as_str);
        for entry in post_session {
            let method = entry.get("method").and_then(Value::as_str).ok_or_else(|| {
                DriverError::Transport("post_session entry missing method".into())
            })?;
            let mut params = entry.get("params").cloned().unwrap_or_else(|| json!({}));
            if let (Some(session_id), Some(map)) = (session_id, params.as_object_mut()) {
                map.entry("sessionId".to_string())
                    .or_insert_with(|| json!(session_id));
            }
            let params = adapter.jsonrpc_post_session_params(method, params)?;
            let response =
                request_response(transport, ids, method, params, events, adapter, allowlist)
                    .await?;
            if let Ok(events_to_emit) = adapter.on_ws_response(method, response).await {
                emit_events(events, events_to_emit).await;
            }
        }
    }

    let auto_turn = session_init
        .get("auto_turn")
        .and_then(Value::as_bool)
        .unwrap_or(true);
    if !auto_turn {
        return Ok(());
    }

    let turn_start = adapter.ws_turn_start_params()?;
    let turn_start_method = adapter.jsonrpc_turn_start_method();
    let turn_response = request_response(
        transport,
        ids,
        turn_start_method,
        turn_start,
        events,
        adapter,
        allowlist,
    )
    .await?;
    if let Ok(events_to_emit) = adapter
        .on_ws_response(turn_start_method, turn_response)
        .await
    {
        emit_events(events, events_to_emit).await;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ApprovalResponse;

    struct MockAdapter;

    #[async_trait]
    impl HarnessEventAdapter for MockAdapter {
        fn harness(&self) -> &'static str {
            "mock"
        }

        fn clone_box(&self) -> Box<dyn HarnessEventAdapter> {
            Box::new(MockAdapter)
        }

        async fn parse_event(&mut self, _raw: Value) -> Vec<DriverEvent> {
            Vec::new()
        }

        fn compose_request(
            &mut self,
            _ctx: &crate::r#trait::DriverContext,
            _config: &crate::r#trait::DriverConfig,
        ) -> Result<crate::r#trait::HarnessRequest, DriverError> {
            Err(DriverError::Unsupported("mock compose_request"))
        }

        async fn try_handle_approval(
            &mut self,
            method: &str,
            _params: &Value,
            allowlist: &SandboxAllowlist,
        ) -> Option<ApprovalResponse> {
            if method == "exec_approval_request" && allowlist.allow_exec {
                Some(ApprovalResponse::Approved)
            } else {
                None
            }
        }
    }

    struct ChannelTransport {
        incoming: std::collections::VecDeque<Value>,
        outgoing: Vec<Value>,
    }

    #[async_trait]
    impl JsonRpcTransport for ChannelTransport {
        async fn send_json(&mut self, value: Value) -> Result<(), DriverError> {
            self.outgoing.push(value);
            Ok(())
        }

        async fn recv_json(&mut self) -> Result<Option<Value>, DriverError> {
            Ok(self.incoming.pop_front())
        }
    }

    #[tokio::test]
    async fn try_dispatch_approval_replies_with_matching_id() {
        let (tx, _rx) = mpsc::channel(8);
        let mut transport = ChannelTransport {
            incoming: std::collections::VecDeque::new(),
            outgoing: Vec::new(),
        };
        let mut adapter = MockAdapter;
        let request = json!({
            "jsonrpc": "2.0",
            "id": 42,
            "method": "exec_approval_request",
            "params": {"command": "cargo test"},
        });
        let handled = try_dispatch_approval(
            &request,
            &mut transport,
            &mut adapter,
            &tx,
            &SandboxAllowlist::default(),
        )
        .await
        .unwrap();
        assert!(handled);
        assert_eq!(transport.outgoing.len(), 1);
        assert_eq!(transport.outgoing[0]["id"], 42);
        assert_eq!(transport.outgoing[0]["result"]["decision"], "accept");
    }

    #[tokio::test]
    async fn handshake_can_start_idle_without_turn() {
        let (tx, _rx) = mpsc::channel(8);
        let mut transport = ChannelTransport {
            incoming: std::collections::VecDeque::from(vec![
                json!({"jsonrpc": "2.0", "id": 1, "result": {"userAgent": "fixture"}}),
                json!({"jsonrpc": "2.0", "id": 2, "result": {"thread": {"id": "thread-idle"}}}),
            ]),
            outgoing: Vec::new(),
        };
        let mut ids = RpcIds::new();
        let mut adapter = MockAdapter;

        run_jsonrpc_handshake(
            &mut transport,
            &mut ids,
            "fixture",
            json!({
                "auto_turn": false,
                "initialize": {},
                "thread_start": {},
            }),
            &tx,
            &mut adapter,
            &SandboxAllowlist::default(),
        )
        .await
        .unwrap();

        let methods = transport
            .outgoing
            .iter()
            .map(|value| value["method"].as_str().unwrap())
            .collect::<Vec<_>>();
        assert_eq!(methods, vec!["initialize", "thread/start"]);
    }

    #[test]
    fn rpc_error_attributes_method_and_preserves_structured_error_fields() {
        let error = rpc_result(
            "session/set_config_option",
            json!({
                "jsonrpc": "2.0",
                "id": 3,
                "error": {
                    "code": -32602,
                    "message": "Invalid params",
                    "data": {"message": "Invalid model value: cursor-grok-4.5-high-fast"}
                }
            }),
        )
        .unwrap_err();
        assert!(matches!(error, DriverError::Transport(message)
            if message == "session/set_config_option RPC error: code=-32602 message=Invalid params data={\"message\":\"Invalid model value: cursor-grok-4.5-high-fast\"}"));
    }

    #[test]
    fn bounded_error_data_preserves_small_json_values_and_null() {
        assert_eq!(bounded_error_data(Some(&json!("small"))), "\"small\"");
        assert_eq!(
            bounded_error_data(Some(&json!({"reason": "small"}))),
            "{\"reason\":\"small\"}"
        );
        assert_eq!(
            bounded_error_data(Some(&json!(["small", 1]))),
            "[\"small\",1]"
        );
        assert_eq!(bounded_error_data(Some(&Value::Null)), "null");
        assert_eq!(bounded_error_data(None), "null");
    }

    #[test]
    fn bounded_error_data_elides_oversized_ascii_object() {
        let rendered = bounded_error_data(Some(&json!({ "payload": "x".repeat(10_000) })));

        assert!(rendered.len() <= RPC_ERROR_DATA_MAX_BYTES);
        assert!(rendered.starts_with("{\"payload\":\""));
        assert!(rendered.ends_with("bytes omitted]"));
        assert!(rendered.contains("[... "));
    }

    #[test]
    fn bounded_error_data_elides_at_a_multibyte_boundary() {
        let rendered = bounded_error_data(Some(&json!("€".repeat(2_000))));
        let marker_start = rendered.find("[... ").unwrap();

        assert!(rendered.len() <= RPC_ERROR_DATA_MAX_BYTES);
        assert!(rendered[..marker_start].ends_with('€'));
        assert!(rendered.ends_with("bytes omitted]"));
    }

    #[tokio::test]
    async fn ambiguous_cursor_alias_fails_before_config_or_prompt() {
        let (tx, _rx) = mpsc::channel(8);
        let mut transport = ChannelTransport {
            incoming: std::collections::VecDeque::from(vec![
                json!({"jsonrpc": "2.0", "id": 1, "result": {}}),
                json!({
                    "jsonrpc": "2.0",
                    "id": 2,
                    "result": {
                        "sessionId": "sess-fixture",
                        "configOptions": [{
                            "id": "model",
                            "options": [
                                {
                                    "value": "grok-4.5[effort=high,fast=true]",
                                    "name": "grok-4.5"
                                },
                                {
                                    "value": "grok-4.5[mode=high,fast=true]",
                                    "name": "grok-4.5"
                                }
                            ]
                        }]
                    }
                }),
            ]),
            outgoing: Vec::new(),
        };
        let ctx = crate::r#trait::DriverContext {
            identity: orgasmic_core::RuntimeIdentity::new("run-fixture", "boot-fixture"),
            run_kind: crate::r#trait::RunKind::Worker,
            task_id: "TASK-YK3G8".into(),
            worker_id: "fixture".into(),
            project_id: Some("orgasmic".into()),
            worktree: None,
            babysitter_target: None,
        };
        let mut adapter = crate::adapters::CursorAcpAdapter::new();
        let init = adapter
            .stdio_session_init(
                &ctx,
                &crate::r#trait::DriverConfig::from_value(json!({
                    "model": "cursor-grok-4.5-high-fast"
                })),
            )
            .unwrap();
        let mut ids = RpcIds::new();

        let result = run_jsonrpc_handshake(
            &mut transport,
            &mut ids,
            "fixture",
            init,
            &tx,
            &mut adapter,
            &SandboxAllowlist::default(),
        )
        .await;

        assert!(
            matches!(result, Err(DriverError::InvalidConfig(_))),
            "unexpected result: {result:?}"
        );
        assert_eq!(
            transport
                .outgoing
                .iter()
                .map(|request| request["method"].as_str().unwrap())
                .collect::<Vec<_>>(),
            vec!["initialize", "session/new"]
        );
    }

    struct PostSessionAdapter;

    #[async_trait]
    impl HarnessEventAdapter for PostSessionAdapter {
        fn harness(&self) -> &'static str {
            "post-session-fixture"
        }

        fn clone_box(&self) -> Box<dyn HarnessEventAdapter> {
            Box::new(PostSessionAdapter)
        }

        async fn parse_event(&mut self, _raw: Value) -> Vec<DriverEvent> {
            Vec::new()
        }

        fn compose_request(
            &mut self,
            _ctx: &crate::r#trait::DriverContext,
            _config: &crate::r#trait::DriverConfig,
        ) -> Result<crate::r#trait::HarnessRequest, DriverError> {
            Err(DriverError::Unsupported("post-session fixture"))
        }

        fn jsonrpc_session_start_method(&self) -> &'static str {
            "session/new"
        }

        fn jsonrpc_turn_start_method(&self) -> &'static str {
            "session/prompt"
        }

        fn ws_turn_start_params(&mut self) -> Result<Value, DriverError> {
            Ok(
                json!({"sessionId": "sess-fixture", "prompt": [{"type": "text", "text": "fixture"}]}),
            )
        }

        fn jsonrpc_post_session_params(
            &mut self,
            method: &str,
            mut params: Value,
        ) -> Result<Value, DriverError> {
            if method == "session/set_config_option" {
                params["value"] = json!("grok-4.5[effort=high,fast=true]");
            }
            Ok(params)
        }
    }

    #[tokio::test]
    async fn post_session_model_is_applied_before_first_prompt() {
        let (tx, _rx) = mpsc::channel(8);
        let mut transport = ChannelTransport {
            incoming: std::collections::VecDeque::from(vec![
                json!({"jsonrpc": "2.0", "id": 1, "result": {}}),
                json!({"jsonrpc": "2.0", "id": 2, "result": {"sessionId": "sess-fixture"}}),
                json!({"jsonrpc": "2.0", "id": 3, "result": {}}),
                json!({"jsonrpc": "2.0", "id": 4, "result": {"stopReason": "end_turn"}}),
            ]),
            outgoing: Vec::new(),
        };
        let mut ids = RpcIds::new();
        let mut adapter = PostSessionAdapter;

        run_jsonrpc_handshake(
            &mut transport,
            &mut ids,
            "fixture",
            json!({
                "initialize": {},
                "thread_start": {},
                "post_session": [{
                    "method": "session/set_config_option",
                    "params": {"configId": "model", "value": "cursor-grok-4.5-high-fast"}
                }]
            }),
            &tx,
            &mut adapter,
            &SandboxAllowlist::default(),
        )
        .await
        .unwrap();

        assert_eq!(
            transport
                .outgoing
                .iter()
                .map(|request| request["method"].as_str().unwrap())
                .collect::<Vec<_>>(),
            vec![
                "initialize",
                "session/new",
                "session/set_config_option",
                "session/prompt"
            ]
        );
        assert_eq!(
            transport.outgoing[2]["params"],
            json!({
                "sessionId": "sess-fixture",
                "configId": "model",
                "value": "grok-4.5[effort=high,fast=true]"
            })
        );
    }
}
