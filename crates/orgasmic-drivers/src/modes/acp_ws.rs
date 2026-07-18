// arch: arch_A53QX.4
// orgasmic:arch_A53QX, dec_ASB1A
//! WebSocket mode for JSON-RPC and ACP-shaped harnesses.

use async_trait::async_trait;
use futures::{SinkExt, StreamExt};
use serde_json::{json, Value};
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::sync::{mpsc, oneshot};
use tokio::time::{timeout, Duration};
use tokio_tungstenite::{
    connect_async,
    tungstenite::{client::IntoClientRequest, http::HeaderValue, Message},
    WebSocketStream,
};

use orgasmic_core::DriverEvent;

use crate::modes::jsonrpc::{
    dispatch_incoming_json, handle_outcome, request_response, run_jsonrpc_handshake,
    send_driver_error, JsonRpcTransport, RpcIds,
};
use crate::r#trait::{
    AcpWsProtocol, AttachOutcome, BabysitterAck, BabysitterRequest, DriverConfig, DriverContext,
    DriverControl, DriverError, DriverSession, HarnessEventAdapter, HarnessRequest, RunKind,
    TransitionAck, TransitionRequest, UserInputAck, UserInputRequest, WorkerDriver,
};
use crate::runtime_options::{RuntimeOptionsAck, RuntimeOptionsCatalog, RuntimeOptionsRequest};
use crate::sandbox::allowlist_from_driver_config;

const MODE: &str = "acp-ws";
// orgasmic:TASK-P4MGK — protocol turn-end is not the dispatch success
// signal; `orgasmic dispatch finalize` is the primary end-of-run.

pub struct AcpWsDriver {
    adapter: Box<dyn HarnessEventAdapter>,
}

impl AcpWsDriver {
    pub fn new(adapter: Box<dyn HarnessEventAdapter>) -> Self {
        Self { adapter }
    }
}

#[async_trait]
impl WorkerDriver for AcpWsDriver {
    fn transport(&self) -> &'static str {
        MODE
    }

    fn harness(&self) -> Option<&'static str> {
        Some(self.adapter.harness())
    }

    fn validate(&self, config: &DriverConfig) -> Result<(), DriverError> {
        self.adapter.validate_config(config)
    }

    async fn acquire(
        &self,
        ctx: DriverContext,
        config: DriverConfig,
    ) -> Result<DriverSession, DriverError> {
        let mut adapter = self.adapter.clone_box();
        let request = adapter.compose_request(&ctx, &config)?;
        let allowlist = allowlist_from_driver_config(&config)
            .map_err(|e| crate::DriverError::InvalidConfig(format!("sandbox_permissions: {e}")))?;
        let (tx, rx) = mpsc::channel(64);

        let control = match request {
            HarnessRequest::Simulated { events } => {
                for event in events {
                    let _ = tx.send(event).await;
                }
                AcpWsControlMode::Simulated {
                    adapter,
                    events: tx,
                }
            }
            HarnessRequest::AcpWs {
                endpoint,
                headers,
                protocol,
                session_init,
            } => {
                let mut request = endpoint
                    .as_str()
                    .into_client_request()
                    .map_err(|e| DriverError::InvalidConfig(format!("websocket endpoint: {e}")))?;
                for (key, value) in headers {
                    let name =
                        tokio_tungstenite::tungstenite::http::header::HeaderName::from_bytes(
                            key.as_bytes(),
                        )
                        .map_err(|e| {
                            DriverError::InvalidConfig(format!("websocket header {key}: {e}"))
                        })?;
                    let value = HeaderValue::from_str(&value).map_err(|e| {
                        DriverError::InvalidConfig(format!("websocket header {key}: {e}"))
                    })?;
                    request.headers_mut().insert(name, value);
                }
                let (commands, command_rx) = mpsc::channel(16);
                if adapter.ws_connect_errors_emit_to_stream() {
                    let connect_endpoint = endpoint.clone();
                    tokio::spawn(async move {
                        match timeout(Duration::from_secs(5), connect_async(request)).await {
                            Err(_) => {
                                send_driver_error(
                                    &tx,
                                    true,
                                    format!("websocket connect timed out: {connect_endpoint}"),
                                )
                                .await;
                            }
                            Ok(Err(e)) => {
                                send_driver_error(
                                    &tx,
                                    true,
                                    format!("websocket connect {connect_endpoint}: {e}"),
                                )
                                .await;
                            }
                            Ok(Ok((ws, response))) => {
                                run_acp_ws(AcpWsRuntime {
                                    endpoint: connect_endpoint,
                                    ws,
                                    status: response.status().as_u16(),
                                    protocol,
                                    session_init,
                                    allowlist: allowlist.clone(),
                                    events: tx,
                                    command_rx,
                                    adapter,
                                })
                                .await;
                            }
                        }
                    });
                } else {
                    let (ws, response) = timeout(Duration::from_secs(5), connect_async(request))
                        .await
                        .map_err(|_| {
                            DriverError::Transport(format!(
                                "websocket connect timed out: {endpoint}"
                            ))
                        })?
                        .map_err(|e| {
                            DriverError::Transport(format!("websocket connect {endpoint}: {e}"))
                        })?;

                    tokio::spawn(run_acp_ws(AcpWsRuntime {
                        endpoint,
                        ws,
                        status: response.status().as_u16(),
                        protocol,
                        session_init,
                        allowlist,
                        events: tx,
                        command_rx,
                        adapter,
                    }));
                }
                AcpWsControlMode::Real { commands }
            }
            _ => return Err(DriverError::Unsupported("acp-ws request shape")),
        };

        Ok(DriverSession {
            identity: ctx.identity.clone(),
            pid: None,
            events: rx,
            control: Box::new(AcpWsControl {
                mode: control,
                kind: ctx.run_kind,
                released: false,
            }),
            native_runtime: None,
        })
    }

    async fn attach(
        &self,
        _ctx: DriverContext,
        _config: DriverConfig,
    ) -> Result<AttachOutcome, DriverError> {
        Ok(AttachOutcome::NotReattachable)
    }
}

struct AcpWsRuntime<S> {
    endpoint: String,
    ws: WebSocketStream<S>,
    status: u16,
    protocol: AcpWsProtocol,
    session_init: Value,
    allowlist: orgasmic_core::SandboxAllowlist,
    events: mpsc::Sender<DriverEvent>,
    command_rx: mpsc::Receiver<AcpWsCommand>,
    adapter: Box<dyn HarnessEventAdapter>,
}

async fn run_acp_ws<S>(runtime: AcpWsRuntime<S>)
where
    S: AsyncRead + AsyncWrite + Unpin + Send,
{
    let AcpWsRuntime {
        endpoint,
        mut ws,
        status,
        protocol,
        session_init,
        allowlist,
        events,
        command_rx,
        mut adapter,
    } = runtime;
    let mut commands = command_rx;
    let mut ids = RpcIds::new();
    let init_result = match protocol {
        AcpWsProtocol::RawJson => {
            if let Err(e) = ws.send(Message::Text(session_init.to_string())).await {
                send_driver_error(&events, true, format!("websocket session init: {e}")).await;
                return;
            }
            adapter.on_ws_connected(json!({ "status": status })).await
        }
        AcpWsProtocol::JsonRpc => {
            let mut transport = WsJsonRpcTransport { ws: &mut ws };
            if let Err(e) = run_jsonrpc_handshake(
                &mut transport,
                &mut ids,
                &endpoint,
                session_init,
                &events,
                adapter.as_mut(),
                &allowlist,
            )
            .await
            {
                send_driver_error(&events, true, e.to_string()).await;
                let _ = ws.close(None).await;
                return;
            }
            Ok(Vec::new())
        }
    };

    if let AcpWsProtocol::RawJson = protocol {
        match init_result {
            Ok(outgoing) => crate::modes::jsonrpc::emit_events(&events, outgoing).await,
            Err(e) => {
                send_driver_error(&events, true, e.to_string()).await;
                return;
            }
        }
    }

    loop {
        tokio::select! {
            command = commands.recv() => {
                let Some(command) = command else { break; };
                if handle_ws_command(
                    command,
                    &mut ws,
                    &events,
                    &mut ids,
                    adapter.as_mut(),
                    &allowlist,
                )
                .await
                {
                    break;
                }
            }
            message = ws.next() => {
                let Some(message) = message else { break; };
                match message {
                    Ok(message) => {
                        if message.is_close() {
                            break;
                        }
                        match message_to_json(message) {
                            Ok(Some(value)) => {
                                let mut transport = WsJsonRpcTransport { ws: &mut ws };
                                let outgoing = match dispatch_incoming_json(
                                    value,
                                    &mut transport,
                                    adapter.as_mut(),
                                    &events,
                                    &allowlist,
                                )
                                .await
                                {
                                    Ok(events) => events,
                                    Err(e) => {
                                        send_driver_error(&events, true, e.to_string()).await;
                                        break;
                                    }
                                };
                                crate::modes::jsonrpc::emit_events(&events, outgoing).await;
                                if adapter.terminal_emitted() {
                                    let _ = ws.close(None).await;
                                    break;
                                }
                            }
                            Ok(None) => {}
                            Err(e) => {
                                send_driver_error(&events, true, e.to_string()).await;
                                break;
                            }
                        }
                    }
                    Err(e) => {
                        send_driver_error(&events, true, format!("websocket read: {e}")).await;
                        break;
                    }
                }
            }
        }
    }
}

struct WsJsonRpcTransport<'a, S> {
    ws: &'a mut WebSocketStream<S>,
}

#[async_trait::async_trait]
impl<S> JsonRpcTransport for WsJsonRpcTransport<'_, S>
where
    S: AsyncRead + AsyncWrite + Unpin + Send,
{
    async fn send_json(&mut self, value: Value) -> Result<(), DriverError> {
        self.ws
            .send(Message::Text(value.to_string()))
            .await
            .map_err(|e| DriverError::Transport(format!("websocket send: {e}")))
    }

    async fn recv_json(&mut self) -> Result<Option<Value>, DriverError> {
        loop {
            let message = self
                .ws
                .next()
                .await
                .ok_or_else(|| DriverError::Transport("websocket closed".into()))?
                .map_err(|e| DriverError::Transport(format!("websocket read: {e}")))?;
            if let Some(value) = message_to_json(message)? {
                return Ok(Some(value));
            }
        }
    }
}

async fn handle_ws_command<S>(
    command: AcpWsCommand,
    ws: &mut WebSocketStream<S>,
    events: &mpsc::Sender<DriverEvent>,
    ids: &mut RpcIds,
    adapter: &mut dyn HarnessEventAdapter,
    allowlist: &orgasmic_core::SandboxAllowlist,
) -> bool
where
    S: AsyncRead + AsyncWrite + Unpin + Send,
{
    let mut transport = WsJsonRpcTransport { ws };
    match command {
        AcpWsCommand::TransitionState { req, ack } => {
            let result = adapter.transition_state(req).await;
            let done = handle_outcome(result, &mut transport, events, ids).await;
            let close = matches!(done, Ok(true));
            if close {
                let _ = transport.ws.close(None).await;
            }
            let _ = ack.send(done.map(|_| TransitionAck {
                accepted: true,
                message: None,
            }));
            close
        }
        AcpWsCommand::BabysitterAction { req, ack } => {
            let result = adapter.babysitter_action(req).await;
            let done = handle_outcome(result, &mut transport, events, ids).await;
            let close = matches!(done, Ok(true));
            if close {
                let _ = transport.ws.close(None).await;
            }
            let _ = ack.send(done.map(|_| BabysitterAck {
                accepted: true,
                message: None,
            }));
            close
        }
        AcpWsCommand::SendInput { req, ack } => {
            let result = adapter.send_input(req).await;
            let done = handle_outcome(result, &mut transport, events, ids).await;
            let close = matches!(done, Ok(true));
            if close {
                let _ = transport.ws.close(None).await;
            }
            let _ = ack.send(done.map(|_| UserInputAck {
                accepted: true,
                message: None,
            }));
            close
        }
        AcpWsCommand::SwitchRuntimeOptions { req, ack } => {
            let result = adapter.switch_runtime_options(req).await;
            let done = handle_outcome(result, &mut transport, events, ids).await;
            let close = matches!(done, Ok(true));
            if close {
                let _ = transport.ws.close(None).await;
            }
            let _ = ack.send(done.map(|_| RuntimeOptionsAck {
                accepted: true,
                message: None,
            }));
            close
        }
        AcpWsCommand::RuntimeOptionsCatalog { ack } => {
            let result = runtime_options_catalog_for_adapter(
                &mut transport,
                events,
                ids,
                adapter,
                allowlist,
            )
            .await;
            let _ = ack.send(result);
            false
        }
        AcpWsCommand::Release { reason, ack } => {
            let result = adapter.release(reason).await;
            let done = handle_outcome(result, &mut transport, events, ids).await;
            let close = matches!(done, Ok(true));
            if close {
                let _ = transport.ws.close(None).await;
            }
            let _ = ack.send(done.map(|_| ()));
            close
        }
    }
}

fn message_to_json(message: Message) -> Result<Option<Value>, DriverError> {
    match message {
        Message::Text(text) => serde_json::from_str(&text)
            .map(Some)
            .map_err(|e| DriverError::Transport(format!("invalid websocket JSON text: {e}"))),
        Message::Binary(bytes) => serde_json::from_slice(&bytes)
            .map(Some)
            .map_err(|e| DriverError::Transport(format!("invalid websocket JSON binary: {e}"))),
        Message::Close(_) => Err(DriverError::Transport("websocket closed".into())),
        Message::Ping(_) | Message::Pong(_) | Message::Frame(_) => Ok(None),
    }
}

enum AcpWsCommand {
    TransitionState {
        req: TransitionRequest,
        ack: oneshot::Sender<Result<TransitionAck, DriverError>>,
    },
    BabysitterAction {
        req: BabysitterRequest,
        ack: oneshot::Sender<Result<BabysitterAck, DriverError>>,
    },
    SendInput {
        req: UserInputRequest,
        ack: oneshot::Sender<Result<UserInputAck, DriverError>>,
    },
    SwitchRuntimeOptions {
        req: RuntimeOptionsRequest,
        ack: oneshot::Sender<Result<RuntimeOptionsAck, DriverError>>,
    },
    RuntimeOptionsCatalog {
        ack: oneshot::Sender<Result<RuntimeOptionsCatalog, DriverError>>,
    },
    Release {
        reason: String,
        ack: oneshot::Sender<Result<(), DriverError>>,
    },
}

async fn runtime_options_catalog_for_adapter(
    transport: &mut dyn JsonRpcTransport,
    events: &mpsc::Sender<DriverEvent>,
    ids: &mut RpcIds,
    adapter: &mut dyn HarnessEventAdapter,
    allowlist: &orgasmic_core::SandboxAllowlist,
) -> Result<RuntimeOptionsCatalog, DriverError> {
    if let Some(rpc) = adapter.runtime_options_catalog_rpc() {
        let response = request_response(
            transport,
            ids,
            &rpc.method,
            rpc.params,
            events,
            adapter,
            allowlist,
        )
        .await?;
        return adapter
            .runtime_options_catalog_from_response(response)
            .await;
    }
    adapter.runtime_options_catalog().await
}

enum AcpWsControlMode {
    Simulated {
        adapter: Box<dyn HarnessEventAdapter>,
        events: mpsc::Sender<DriverEvent>,
    },
    Real {
        commands: mpsc::Sender<AcpWsCommand>,
    },
}

struct AcpWsControl {
    mode: AcpWsControlMode,
    kind: RunKind,
    released: bool,
}

#[async_trait]
impl DriverControl for AcpWsControl {
    async fn transition_state(
        &mut self,
        req: TransitionRequest,
    ) -> Result<TransitionAck, DriverError> {
        if self.kind == RunKind::Babysitter {
            return Err(DriverError::WorkerToolBlocked("transition_state".into()));
        }
        match &mut self.mode {
            AcpWsControlMode::Simulated { adapter, events } => {
                let outcome = adapter.transition_state(req).await?;
                crate::modes::jsonrpc::emit_events(events, outcome.events).await;
                Ok(TransitionAck {
                    accepted: true,
                    message: None,
                })
            }
            AcpWsControlMode::Real { commands } => {
                let (ack, rx) = oneshot::channel();
                commands
                    .send(AcpWsCommand::TransitionState { req, ack })
                    .await
                    .map_err(|_| DriverError::Transport("websocket task ended".into()))?;
                rx.await.map_err(|_| {
                    DriverError::Transport("websocket transition ack dropped".into())
                })?
            }
        }
    }

    async fn babysitter_action(
        &mut self,
        req: BabysitterRequest,
    ) -> Result<BabysitterAck, DriverError> {
        if self.kind == RunKind::Worker {
            return Err(DriverError::BabysitterToolBlocked(req.tool.as_str().into()));
        }
        match &mut self.mode {
            AcpWsControlMode::Simulated { adapter, events } => {
                let outcome = adapter.babysitter_action(req).await?;
                crate::modes::jsonrpc::emit_events(events, outcome.events).await;
                Ok(BabysitterAck {
                    accepted: true,
                    message: None,
                })
            }
            AcpWsControlMode::Real { commands } => {
                let (ack, rx) = oneshot::channel();
                commands
                    .send(AcpWsCommand::BabysitterAction { req, ack })
                    .await
                    .map_err(|_| DriverError::Transport("websocket task ended".into()))?;
                rx.await.map_err(|_| {
                    DriverError::Transport("websocket babysitter ack dropped".into())
                })?
            }
        }
    }

    async fn send_input(&mut self, req: UserInputRequest) -> Result<UserInputAck, DriverError> {
        match &mut self.mode {
            AcpWsControlMode::Simulated { adapter, events } => {
                let outcome = adapter.send_input(req).await?;
                crate::modes::jsonrpc::emit_events(events, outcome.events).await;
                Ok(UserInputAck {
                    accepted: true,
                    message: None,
                })
            }
            AcpWsControlMode::Real { commands } => {
                let (ack, rx) = oneshot::channel();
                commands
                    .send(AcpWsCommand::SendInput { req, ack })
                    .await
                    .map_err(|_| DriverError::Transport("websocket task ended".into()))?;
                rx.await
                    .map_err(|_| DriverError::Transport("websocket input ack dropped".into()))?
            }
        }
    }

    async fn switch_runtime_options(
        &mut self,
        req: RuntimeOptionsRequest,
    ) -> Result<RuntimeOptionsAck, DriverError> {
        match &mut self.mode {
            AcpWsControlMode::Simulated { adapter, events } => {
                let outcome = adapter.switch_runtime_options(req).await?;
                crate::modes::jsonrpc::emit_events(events, outcome.events).await;
                Ok(RuntimeOptionsAck {
                    accepted: true,
                    message: None,
                })
            }
            AcpWsControlMode::Real { commands } => {
                let (ack, rx) = oneshot::channel();
                commands
                    .send(AcpWsCommand::SwitchRuntimeOptions { req, ack })
                    .await
                    .map_err(|_| DriverError::Transport("websocket task ended".into()))?;
                rx.await.map_err(|_| {
                    DriverError::Transport("websocket runtime options ack dropped".into())
                })?
            }
        }
    }

    async fn runtime_options_catalog(&mut self) -> Result<RuntimeOptionsCatalog, DriverError> {
        match &mut self.mode {
            AcpWsControlMode::Simulated { adapter, .. } => adapter.runtime_options_catalog().await,
            AcpWsControlMode::Real { commands } => {
                let (ack, rx) = oneshot::channel();
                commands
                    .send(AcpWsCommand::RuntimeOptionsCatalog { ack })
                    .await
                    .map_err(|_| DriverError::Transport("websocket task ended".into()))?;
                rx.await.map_err(|_| {
                    DriverError::Transport("websocket runtime options catalog dropped".into())
                })?
            }
        }
    }

    async fn release(&mut self, reason: &str) -> Result<(), DriverError> {
        if self.released {
            return Ok(());
        }
        self.released = true;
        match &mut self.mode {
            AcpWsControlMode::Simulated { adapter, events } => {
                let outcome = adapter.release(reason.to_string()).await?;
                crate::modes::jsonrpc::emit_events(events, outcome.events).await;
                Ok(())
            }
            AcpWsControlMode::Real { commands } => {
                let (ack, rx) = oneshot::channel();
                if commands
                    .send(AcpWsCommand::Release {
                        reason: reason.to_string(),
                        ack,
                    })
                    .await
                    .is_err()
                {
                    return Ok(());
                }
                rx.await
                    .map_err(|_| DriverError::Transport("websocket release ack dropped".into()))?
            }
        }
    }
}
