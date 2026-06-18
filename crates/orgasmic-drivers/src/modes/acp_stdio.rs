// arch: arch_A53QX.4
// orgasmic:arch_A53QX, dec_ASB1A
//! ACP-over-stdio mode.
//!
//! Claude Code uses line-delimited stream JSON via the subprocess-stream-json
//! plumbing inside this mode. Codex `app-server` uses newline-delimited JSON-RPC
//! with a dedicated runtime (`run_acp_stdio`) that mirrors `acp_ws`.

use std::collections::BTreeMap;
use std::process::{Command as StdCommand, Stdio};

use async_trait::async_trait;
use serde_json::Value;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, ChildStderr, ChildStdin, ChildStdout, Command};
use tokio::sync::{mpsc, oneshot};

use orgasmic_core::DriverEvent;

use crate::modes::jsonrpc::{
    dispatch_incoming_json, emit_events, request_response, run_jsonrpc_handshake,
    send_driver_error, JsonRpcTransport, RpcIds,
};
use crate::modes::subprocess_stream_json::{
    finalize_subprocess_exit, reap_process_group, SubprocessExitSummary, SubprocessStreamJsonDriver,
};
use crate::r#trait::{
    AttachOutcome, BabysitterAck, BabysitterRequest, DriverConfig, DriverContext, DriverControl,
    DriverError, DriverSession, HarnessControlOutcome, HarnessEventAdapter, HarnessRequest,
    RunKind, StdioSpawn, TransitionAck, TransitionRequest, UserInputAck, UserInputRequest,
    WorkerDriver,
};
use crate::runtime_options::{
    RuntimeOptionsAck, RuntimeOptionsCatalog, RuntimeOptionsCatalogRpc, RuntimeOptionsRequest,
};
use crate::sandbox::allowlist_from_driver_config;

const MODE: &str = "acp-stdio";

/// Cadence for liveness heartbeats emitted while the harness is quiet.
///
/// codex `app-server` buffers subprocess stdout (e.g. a long `cargo test`),
/// emitting zero driver events for many minutes and then flushing in one
/// burst. The supervisor's stall detector releases a run after
/// `:STALL_TIMEOUT_SECS:` (default 600s) without any driver event, so a
/// genuinely-working-but-quiet run gets SIGKILLed. A heartbeat every ~30s
/// keeps `last_driver_event_at` fresh (20 resets inside a 600s window) without
/// changing the harness's batching behaviour. See TASK-100.3.
const HEARTBEAT_INTERVAL: std::time::Duration = std::time::Duration::from_secs(30);

/// Floor for a configured `heartbeat_interval_ms`. A 0/1ms cadence is a
/// busy-spin: the select loop would wake on the heartbeat arm hundreds of times
/// a second to emit liveness pings that the supervisor only consumes to reset a
/// ~600s stall timer. Clamp any sub-floor override (including the 0 that would
/// otherwise be filtered out) up to this value so production configs cannot
/// request a pathological cadence. The test suite uses exactly this floor.
const HEARTBEAT_INTERVAL_FLOOR: std::time::Duration = std::time::Duration::from_millis(100);

/// Resolve the heartbeat cadence from driver config. Production omits the field
/// and uses [`HEARTBEAT_INTERVAL`] (30s); an optional `heartbeat_interval_ms`
/// (positive integer) overrides it — used by the regression suite to exercise
/// the cadence without 30s waits, and available as a per-worker tuning knob.
/// Any override below [`HEARTBEAT_INTERVAL_FLOOR`] (100ms) is clamped up to the
/// floor to avoid a busy-spin cadence.
fn heartbeat_interval(config: &DriverConfig) -> std::time::Duration {
    config
        .0
        .get("heartbeat_interval_ms")
        .and_then(Value::as_u64)
        .filter(|ms| *ms > 0)
        .map(std::time::Duration::from_millis)
        .map(|d| d.max(HEARTBEAT_INTERVAL_FLOOR))
        .unwrap_or(HEARTBEAT_INTERVAL)
}

pub struct AcpStdioDriver {
    adapter: Box<dyn HarnessEventAdapter>,
}

impl AcpStdioDriver {
    pub fn new(adapter: Box<dyn HarnessEventAdapter>) -> Self {
        Self { adapter }
    }
}

struct AcpStdioComposeAdapter {
    inner: Box<dyn HarnessEventAdapter>,
    jsonrpc_session_init: Option<Value>,
}

#[async_trait]
impl HarnessEventAdapter for AcpStdioComposeAdapter {
    fn harness(&self) -> &'static str {
        self.inner.harness()
    }

    fn clone_box(&self) -> Box<dyn HarnessEventAdapter> {
        Box::new(AcpStdioComposeAdapter {
            inner: self.inner.clone_box(),
            jsonrpc_session_init: None,
        })
    }

    fn stdio_spawn(&self) -> Option<StdioSpawn> {
        self.inner.stdio_spawn()
    }

    fn upgrades_simulated_to_subprocess(&self) -> bool {
        self.inner.upgrades_simulated_to_subprocess()
    }

    fn stdio_session_init(
        &mut self,
        ctx: &DriverContext,
        config: &DriverConfig,
    ) -> Result<Value, DriverError> {
        self.inner.stdio_session_init(ctx, config)
    }

    async fn parse_event(&mut self, raw: Value) -> Vec<orgasmic_core::DriverEvent> {
        self.inner.parse_event(raw).await
    }

    async fn parse_stdout_line(&mut self, line: &str) -> Vec<orgasmic_core::DriverEvent> {
        self.inner.parse_stdout_line(line).await
    }

    fn validate_config(&self, config: &DriverConfig) -> Result<(), DriverError> {
        self.inner.validate_config(config)
    }

    fn stdio_initial_payload(
        &mut self,
        ctx: &DriverContext,
        config: &DriverConfig,
    ) -> Result<Option<Vec<u8>>, DriverError> {
        self.inner.stdio_initial_payload(ctx, config)
    }

    fn stderr_event(&mut self, line: String) -> orgasmic_core::DriverEvent {
        self.inner.stderr_event(line)
    }

    fn text_event(
        &mut self,
        stream: orgasmic_core::TextStream,
        chunk: String,
    ) -> orgasmic_core::DriverEvent {
        self.inner.text_event(stream, chunk)
    }

    fn next_seq(&mut self) -> u64 {
        self.inner.next_seq()
    }

    async fn on_ws_connected(
        &mut self,
        meta: Value,
    ) -> Result<Vec<orgasmic_core::DriverEvent>, DriverError> {
        self.inner.on_ws_connected(meta).await
    }

    fn ws_connect_errors_emit_to_stream(&self) -> bool {
        self.inner.ws_connect_errors_emit_to_stream()
    }

    async fn on_ws_thread_started(
        &mut self,
        endpoint: &str,
        thread_response: &Value,
    ) -> Result<Vec<orgasmic_core::DriverEvent>, DriverError> {
        self.inner
            .on_ws_thread_started(endpoint, thread_response)
            .await
    }

    fn ws_turn_start_params(&mut self) -> Result<Value, DriverError> {
        self.inner.ws_turn_start_params()
    }

    fn jsonrpc_session_start_method(&self) -> &'static str {
        self.inner.jsonrpc_session_start_method()
    }

    fn jsonrpc_turn_start_method(&self) -> &'static str {
        self.inner.jsonrpc_turn_start_method()
    }

    async fn on_ws_response(
        &mut self,
        method: &str,
        response: Value,
    ) -> Result<Vec<orgasmic_core::DriverEvent>, DriverError> {
        self.inner.on_ws_response(method, response).await
    }

    async fn transition_state(
        &mut self,
        req: TransitionRequest,
    ) -> Result<HarnessControlOutcome, DriverError> {
        self.inner.transition_state(req).await
    }

    async fn babysitter_action(
        &mut self,
        req: BabysitterRequest,
    ) -> Result<HarnessControlOutcome, DriverError> {
        self.inner.babysitter_action(req).await
    }

    async fn release(&mut self, reason: String) -> Result<HarnessControlOutcome, DriverError> {
        self.inner.release(reason).await
    }

    async fn send_input(
        &mut self,
        req: UserInputRequest,
    ) -> Result<HarnessControlOutcome, DriverError> {
        self.inner.send_input(req).await
    }

    async fn switch_runtime_options(
        &mut self,
        req: RuntimeOptionsRequest,
    ) -> Result<HarnessControlOutcome, DriverError> {
        self.inner.switch_runtime_options(req).await
    }

    fn runtime_options_catalog_rpc(&self) -> Option<RuntimeOptionsCatalogRpc> {
        self.inner.runtime_options_catalog_rpc()
    }

    async fn runtime_options_catalog(&mut self) -> Result<RuntimeOptionsCatalog, DriverError> {
        self.inner.runtime_options_catalog().await
    }

    async fn runtime_options_catalog_from_response(
        &mut self,
        response: Value,
    ) -> Result<RuntimeOptionsCatalog, DriverError> {
        self.inner
            .runtime_options_catalog_from_response(response)
            .await
    }

    fn ignores_stderr_line(&self, line: &str) -> bool {
        self.inner.ignores_stderr_line(line)
    }

    async fn try_handle_approval(
        &mut self,
        method: &str,
        params: &serde_json::Value,
        allowlist: &orgasmic_core::SandboxAllowlist,
    ) -> Option<crate::sandbox::ApprovalResponse> {
        self.inner
            .try_handle_approval(method, params, allowlist)
            .await
    }

    fn terminal_emitted(&self) -> bool {
        self.inner.terminal_emitted()
    }

    fn compose_request(
        &mut self,
        ctx: &DriverContext,
        config: &DriverConfig,
    ) -> Result<HarnessRequest, DriverError> {
        let spawn = self.inner.stdio_spawn().ok_or(DriverError::Unsupported(
            "acp-stdio requires stdio_spawn adapter config",
        ))?;
        let request = self.inner.compose_request(ctx, config)?;
        let (request, session_init) =
            compose_stdio_request(&spawn, ctx, config, self.inner.as_mut(), request)?;
        self.jsonrpc_session_init = session_init;
        Ok(request)
    }
}

fn compose_stdio_request(
    spawn: &StdioSpawn,
    ctx: &DriverContext,
    config: &DriverConfig,
    adapter: &mut dyn HarnessEventAdapter,
    request: HarnessRequest,
) -> Result<(HarnessRequest, Option<Value>), DriverError> {
    if !spawn.cwd_is_absolute() {
        return Err(DriverError::InvalidConfig(
            "stdio_spawn cwd must be absolute when set".into(),
        ));
    }

    match request {
        HarnessRequest::Subprocess {
            stdin_payload,
            close_stdin,
            env: request_env,
            ..
        } => Ok((
            subprocess_from_stdio_spawn(spawn, ctx, request_env, stdin_payload, close_stdin),
            None,
        )),
        HarnessRequest::Simulated { .. }
            if adapter.upgrades_simulated_to_subprocess() && command_available(&spawn.command) =>
        {
            match adapter.stdio_session_init(ctx, config) {
                Ok(session_init) => Ok((
                    subprocess_from_stdio_spawn(spawn, ctx, BTreeMap::new(), None, false),
                    Some(session_init),
                )),
                Err(_) => {
                    let stdin_payload = adapter.stdio_initial_payload(ctx, config)?;
                    Ok((
                        subprocess_from_stdio_spawn(
                            spawn,
                            ctx,
                            BTreeMap::new(),
                            stdin_payload,
                            false,
                        ),
                        None,
                    ))
                }
            }
        }
        other => Ok((other, None)),
    }
}

fn subprocess_from_stdio_spawn(
    spawn: &StdioSpawn,
    ctx: &DriverContext,
    extra_env: BTreeMap<String, String>,
    stdin_payload: Option<Vec<u8>>,
    close_stdin: bool,
) -> HarnessRequest {
    let mut env: BTreeMap<String, String> = spawn.env.iter().cloned().collect();
    // adapter-composed env (e.g. api key) overrides spawn defaults
    env.extend(extra_env);
    HarnessRequest::Subprocess {
        binary: spawn.command.clone(),
        args: spawn.args.clone(),
        env,
        cwd: spawn.cwd.clone().or_else(|| ctx.worktree.clone()),
        stdin_payload,
        close_stdin,
    }
}

fn command_available(command: &str) -> bool {
    StdCommand::new("which")
        .arg(command)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map(|status| status.success())
        .unwrap_or(false)
}

#[async_trait]
impl WorkerDriver for AcpStdioDriver {
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
        let mut compose = AcpStdioComposeAdapter {
            inner: self.adapter.clone_box(),
            jsonrpc_session_init: None,
        };
        compose.validate_config(&config)?;
        let request = compose.compose_request(&ctx, &config)?;

        let (tx, rx) = mpsc::channel(64);
        let jsonrpc_init = compose.jsonrpc_session_init;
        let allowlist = allowlist_from_driver_config(&config)
            .map_err(|e| crate::DriverError::InvalidConfig(format!("sandbox_permissions: {e}")))?;
        let heartbeat_interval = heartbeat_interval(&config);
        let adapter = compose.inner;

        let control = match request {
            HarnessRequest::Simulated { events } => {
                for event in events {
                    let _ = tx.send(event).await;
                }
                AcpStdioControlMode::Simulated {
                    adapter,
                    events: tx,
                }
            }
            HarnessRequest::Subprocess {
                binary,
                args,
                env,
                cwd,
                stdin_payload: _,
                close_stdin: _,
            } if jsonrpc_init.is_some() => {
                let session_init = jsonrpc_init.expect("checked above");
                let (commands, command_rx) = mpsc::channel(16);
                let mut cmd = Command::new(&binary);
                cmd.args(args)
                    .stdin(Stdio::piped())
                    .stdout(Stdio::piped())
                    .stderr(Stdio::piped());
                if let Some(cwd) = cwd {
                    cmd.current_dir(cwd);
                }
                for (key, value) in env {
                    cmd.env(key, value);
                }
                detach_subprocess(&mut cmd);
                let mut child = cmd
                    .spawn()
                    .map_err(|e| DriverError::Transport(format!("{binary} spawn: {e}")))?;
                let pid = child.id();
                let Some(stdin) = child.stdin.take() else {
                    let _ = child.kill().await;
                    return Err(DriverError::Transport(format!(
                        "{binary} stdin unavailable"
                    )));
                };
                let Some(stdout) = child.stdout.take() else {
                    let _ = child.kill().await;
                    return Err(DriverError::Transport(format!(
                        "{binary} stdout unavailable"
                    )));
                };
                let Some(stderr) = child.stderr.take() else {
                    let _ = child.kill().await;
                    return Err(DriverError::Transport(format!(
                        "{binary} stderr unavailable"
                    )));
                };
                let peer_id = format!("stdio:{binary}");
                tokio::spawn(run_acp_stdio(AcpStdioRuntime {
                    binary,
                    child,
                    stdin,
                    stdout,
                    stderr,
                    peer_id,
                    session_init,
                    allowlist,
                    events: tx,
                    command_rx,
                    adapter,
                    heartbeat_interval,
                }));
                AcpStdioControlMode::JsonRpc { commands, pid }
            }
            HarnessRequest::Subprocess { .. } => {
                return SubprocessStreamJsonDriver::new(Box::new(AcpStdioComposeAdapter {
                    inner: self.adapter.clone_box(),
                    jsonrpc_session_init: None,
                }))
                .acquire(ctx, config)
                .await;
            }
            _ => return Err(DriverError::Unsupported("acp-stdio request shape")),
        };

        Ok(DriverSession {
            identity: ctx.identity.clone(),
            pid: control.pid(),
            events: rx,
            control: Box::new(AcpStdioControl {
                mode: control,
                kind: ctx.run_kind,
                released: false,
            }),
            native_runtime: None,
        })
    }

    async fn attach(
        &self,
        ctx: DriverContext,
        config: DriverConfig,
    ) -> Result<AttachOutcome, DriverError> {
        SubprocessStreamJsonDriver::new(Box::new(AcpStdioComposeAdapter {
            inner: self.adapter.clone_box(),
            jsonrpc_session_init: None,
        }))
        .attach(ctx, config)
        .await
    }
}

#[cfg(unix)]
extern "C" {
    fn setsid() -> i32;
}

#[cfg(unix)]
fn detach_subprocess(cmd: &mut Command) {
    unsafe {
        cmd.pre_exec(|| {
            if setsid() == -1 {
                Err(std::io::Error::last_os_error())
            } else {
                Ok(())
            }
        });
    }
}

#[cfg(not(unix))]
fn detach_subprocess(_cmd: &mut Command) {}

struct StdioJsonRpcTransport {
    stdin: ChildStdin,
    stdout: BufReader<ChildStdout>,
}

#[async_trait]
impl JsonRpcTransport for StdioJsonRpcTransport {
    async fn send_json(&mut self, value: Value) -> Result<(), DriverError> {
        let mut line = value.to_string();
        line.push('\n');
        self.stdin
            .write_all(line.as_bytes())
            .await
            .map_err(|e| DriverError::Transport(format!("stdio write: {e}")))?;
        self.stdin
            .flush()
            .await
            .map_err(|e| DriverError::Transport(format!("stdio flush: {e}")))
    }

    async fn recv_json(&mut self) -> Result<Option<Value>, DriverError> {
        let mut line = String::new();
        match self.stdout.read_line(&mut line).await {
            Ok(0) => Ok(None),
            Ok(_) => {
                let trimmed = line.trim();
                if trimmed.is_empty() {
                    return self.recv_json().await;
                }
                serde_json::from_str(trimmed)
                    .map(Some)
                    .map_err(|e| DriverError::Transport(format!("invalid stdio JSON: {e}")))
            }
            Err(e) => Err(DriverError::Transport(format!("stdio read: {e}"))),
        }
    }
}

struct AcpStdioRuntime {
    binary: String,
    child: Child,
    stdin: ChildStdin,
    stdout: ChildStdout,
    stderr: ChildStderr,
    peer_id: String,
    session_init: Value,
    allowlist: orgasmic_core::SandboxAllowlist,
    events: mpsc::Sender<DriverEvent>,
    command_rx: mpsc::Receiver<AcpStdioCommand>,
    adapter: Box<dyn HarnessEventAdapter>,
    heartbeat_interval: std::time::Duration,
}

async fn run_acp_stdio(runtime: AcpStdioRuntime) {
    let AcpStdioRuntime {
        binary,
        mut child,
        stdin,
        stdout,
        stderr,
        peer_id,
        session_init,
        allowlist,
        events,
        command_rx,
        mut adapter,
        heartbeat_interval,
    } = runtime;
    let mut commands = command_rx;
    let mut transport = StdioJsonRpcTransport {
        stdin,
        stdout: BufReader::new(stdout),
    };
    let mut ids = RpcIds::new();

    if let Err(e) = run_jsonrpc_handshake(
        &mut transport,
        &mut ids,
        &peer_id,
        session_init,
        &events,
        adapter.as_mut(),
        &allowlist,
    )
    .await
    {
        send_driver_error(&events, true, e.to_string()).await;
        // Reap the whole setsid group, not just the direct child: the harness
        // may already have forked descendants during the handshake.
        let _ = reap_process_group(&mut child).await;
        return;
    }

    let mut stderr = BufReader::new(stderr).lines();
    let mut stderr_open = true;
    let mut released = false;
    let mut exit_summary = SubprocessExitSummary::default();

    // Liveness heartbeats (TASK-100.3). The ticker is a branch of this select
    // loop, so it shares the loop task's lifetime exactly: when the loop ends
    // (release, terminal event, transport EOF, ...) the interval is dropped
    // with everything else — no detached JoinHandle to leak. `heartbeat_seq`
    // numbers the heartbeat stream; `quiet_since_last_tick` suppresses a
    // heartbeat on any tick where a real driver event already fired, so
    // heartbeats only appear during genuinely-silent stretches.
    let mut heartbeat = tokio::time::interval(heartbeat_interval);
    heartbeat.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    // First tick fires immediately; skip it so the first heartbeat is one full
    // interval into a quiet stretch rather than at t=0.
    heartbeat.tick().await;
    let mut heartbeat_seq: u64 = 0;
    let mut quiet_since_last_tick = true;
    // Once the child is observed dead we stop claiming liveness. The EOF /
    // termination path (transport `Ok(None)`, command-channel close, terminal
    // event) still runs its course and reaps the child below; this flag only
    // suppresses heartbeats so a wedged-but-piped grandchild can't keep the
    // stall timer reset after the actual peer has exited.
    let mut child_alive = true;

    loop {
        tokio::select! {
            command = commands.recv() => {
                match command {
                    Some(command) => {
                        quiet_since_last_tick = false;
                        if handle_stdio_command(
                            command,
                            &mut transport,
                            &events,
                            &mut ids,
                            adapter.as_mut(),
                            &allowlist,
                            &mut exit_summary,
                        )
                        .await
                        {
                            released = true;
                        }
                    }
                    None => {
                        released = true;
                        break;
                    }
                }
            }
            line = stderr.next_line(), if stderr_open => {
                match line {
                    Ok(Some(line)) => {
                        if adapter.ignores_stderr_line(&line) {
                            continue;
                        }
                        quiet_since_last_tick = false;
                        let event = adapter.stderr_event(line);
                        let _ = events.send(event).await;
                    }
                    Ok(None) => stderr_open = false,
                    Err(e) => {
                        send_driver_error(&events, true, format!("{binary} stderr read: {e}")).await;
                        stderr_open = false;
                    }
                }
            }
            value = transport.recv_json() => {
                match value {
                    Ok(Some(value)) => {
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
                        if !outgoing.is_empty() {
                            quiet_since_last_tick = false;
                        }
                        for event in &outgoing {
                            exit_summary.record(event);
                        }
                        emit_events(&events, outgoing).await;
                        if adapter.terminal_emitted() {
                            // Reap the whole group via the shared exit path
                            // below rather than the direct child only.
                            released = true;
                            break;
                        }
                    }
                    Ok(None) => break,
                    Err(e) => {
                        send_driver_error(&events, true, e.to_string()).await;
                        break;
                    }
                }
            }
            _ = heartbeat.tick(), if child_alive => {
                // Liveness gate: a heartbeat asserts the child is doing work, so
                // confirm it has not exited before emitting one. The transport
                // pipe can linger open past child exit (e.g. a grandchild holds
                // the fd), so EOF alone is not a reliable death signal — poll the
                // child directly. On exit, stop heartbeats and let the EOF /
                // termination path reap the child as usual.
                match child.try_wait() {
                    Ok(Some(_)) => {
                        child_alive = false;
                    }
                    Err(_) => {
                        // Can't determine status; treat as no-longer-live rather
                        // than keep asserting liveness we can't confirm.
                        child_alive = false;
                    }
                    Ok(None) => {
                        if quiet_since_last_tick {
                            let event = DriverEvent::Heartbeat { seq: heartbeat_seq };
                            heartbeat_seq += 1;
                            if events.send(event).await.is_err() {
                                break;
                            }
                        } else {
                            // Real activity covered this window; reset and only
                            // emit a heartbeat if the *next* full interval stays
                            // silent.
                            quiet_since_last_tick = true;
                        }
                    }
                }
            }
        }
    }

    // On release (explicit, command-channel close, or terminal event), reap the
    // whole setsid process group — direct child plus forked descendants such as
    // a JSON-RPC app-server's workers — not just the direct child. Otherwise the
    // child has exited on its own and we just wait it out.
    let wait_status = if released {
        reap_process_group(&mut child).await
    } else {
        child.wait().await
    };

    finalize_subprocess_exit(
        &binary,
        wait_status,
        adapter.as_mut(),
        &events,
        &exit_summary,
    )
    .await;
}

async fn handle_stdio_command(
    command: AcpStdioCommand,
    transport: &mut StdioJsonRpcTransport,
    events: &mpsc::Sender<DriverEvent>,
    ids: &mut RpcIds,
    adapter: &mut dyn HarnessEventAdapter,
    allowlist: &orgasmic_core::SandboxAllowlist,
    exit_summary: &mut SubprocessExitSummary,
) -> bool {
    match command {
        AcpStdioCommand::TransitionState { req, ack } => {
            let result = adapter.transition_state(req).await;
            let done =
                handle_outcome_with_summary(result, transport, events, ids, exit_summary).await;
            let close = matches!(done, Ok(true));
            let _ = ack.send(done.map(|_| TransitionAck {
                accepted: true,
                message: None,
            }));
            close
        }
        AcpStdioCommand::BabysitterAction { req, ack } => {
            let result = adapter.babysitter_action(req).await;
            let done =
                handle_outcome_with_summary(result, transport, events, ids, exit_summary).await;
            let close = matches!(done, Ok(true));
            let _ = ack.send(done.map(|_| BabysitterAck {
                accepted: true,
                message: None,
            }));
            close
        }
        AcpStdioCommand::SendInput { req, ack } => {
            let result = adapter.send_input(req).await;
            let done =
                handle_outcome_with_summary(result, transport, events, ids, exit_summary).await;
            let close = matches!(done, Ok(true));
            let _ = ack.send(done.map(|_| UserInputAck {
                accepted: true,
                message: None,
            }));
            close
        }
        AcpStdioCommand::SwitchRuntimeOptions { req, ack } => {
            let result = adapter.switch_runtime_options(req).await;
            let done =
                handle_outcome_with_summary(result, transport, events, ids, exit_summary).await;
            let close = matches!(done, Ok(true));
            let _ = ack.send(done.map(|_| RuntimeOptionsAck {
                accepted: true,
                message: None,
            }));
            close
        }
        AcpStdioCommand::RuntimeOptionsCatalog { ack } => {
            let result =
                runtime_options_catalog_for_adapter(transport, events, ids, adapter, allowlist)
                    .await;
            let _ = ack.send(result);
            false
        }
        AcpStdioCommand::Release { reason, ack } => {
            let result = adapter.release(reason).await;
            let done =
                handle_outcome_with_summary(result, transport, events, ids, exit_summary).await;
            let close = matches!(done, Ok(true));
            let _ = ack.send(done.map(|_| ()));
            close
        }
    }
}

async fn handle_outcome_with_summary(
    result: Result<HarnessControlOutcome, DriverError>,
    transport: &mut StdioJsonRpcTransport,
    events: &mpsc::Sender<DriverEvent>,
    ids: &mut RpcIds,
    exit_summary: &mut SubprocessExitSummary,
) -> Result<bool, DriverError> {
    let outcome = result?;
    for message in outcome.wire_messages {
        crate::modes::jsonrpc::send_wire_message(transport, ids, message).await?;
    }
    for event in &outcome.events {
        exit_summary.record(event);
    }
    emit_events(events, outcome.events).await;
    Ok(outcome.close)
}

enum AcpStdioCommand {
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

enum AcpStdioControlMode {
    Simulated {
        adapter: Box<dyn HarnessEventAdapter>,
        events: mpsc::Sender<DriverEvent>,
    },
    JsonRpc {
        commands: mpsc::Sender<AcpStdioCommand>,
        pid: Option<u32>,
    },
}

impl AcpStdioControlMode {
    fn pid(&self) -> Option<u32> {
        match self {
            Self::Simulated { .. } => None,
            Self::JsonRpc { pid, .. } => *pid,
        }
    }
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

struct AcpStdioControl {
    mode: AcpStdioControlMode,
    kind: RunKind,
    released: bool,
}

#[async_trait]
impl DriverControl for AcpStdioControl {
    async fn transition_state(
        &mut self,
        req: TransitionRequest,
    ) -> Result<TransitionAck, DriverError> {
        if self.kind == RunKind::Babysitter {
            return Err(DriverError::WorkerToolBlocked("transition_state".into()));
        }
        match &mut self.mode {
            AcpStdioControlMode::Simulated { adapter, events } => {
                let outcome = adapter.transition_state(req).await?;
                crate::modes::jsonrpc::emit_events(events, outcome.events).await;
                Ok(TransitionAck {
                    accepted: true,
                    message: None,
                })
            }
            AcpStdioControlMode::JsonRpc { commands, .. } => {
                let (ack, rx) = oneshot::channel();
                commands
                    .send(AcpStdioCommand::TransitionState { req, ack })
                    .await
                    .map_err(|_| DriverError::Transport("acp-stdio task ended".into()))?;
                rx.await.map_err(|_| {
                    DriverError::Transport("acp-stdio transition ack dropped".into())
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
            AcpStdioControlMode::Simulated { adapter, events } => {
                let outcome = adapter.babysitter_action(req).await?;
                crate::modes::jsonrpc::emit_events(events, outcome.events).await;
                Ok(BabysitterAck {
                    accepted: true,
                    message: None,
                })
            }
            AcpStdioControlMode::JsonRpc { commands, .. } => {
                let (ack, rx) = oneshot::channel();
                commands
                    .send(AcpStdioCommand::BabysitterAction { req, ack })
                    .await
                    .map_err(|_| DriverError::Transport("acp-stdio task ended".into()))?;
                rx.await.map_err(|_| {
                    DriverError::Transport("acp-stdio babysitter ack dropped".into())
                })?
            }
        }
    }

    async fn send_input(&mut self, req: UserInputRequest) -> Result<UserInputAck, DriverError> {
        match &mut self.mode {
            AcpStdioControlMode::Simulated { adapter, events } => {
                let outcome = adapter.send_input(req).await?;
                crate::modes::jsonrpc::emit_events(events, outcome.events).await;
                Ok(UserInputAck {
                    accepted: true,
                    message: None,
                })
            }
            AcpStdioControlMode::JsonRpc { commands, .. } => {
                let (ack, rx) = oneshot::channel();
                commands
                    .send(AcpStdioCommand::SendInput { req, ack })
                    .await
                    .map_err(|_| DriverError::Transport("acp-stdio task ended".into()))?;
                rx.await
                    .map_err(|_| DriverError::Transport("acp-stdio input ack dropped".into()))?
            }
        }
    }

    async fn switch_runtime_options(
        &mut self,
        req: RuntimeOptionsRequest,
    ) -> Result<RuntimeOptionsAck, DriverError> {
        match &mut self.mode {
            AcpStdioControlMode::Simulated { adapter, events } => {
                let outcome = adapter.switch_runtime_options(req).await?;
                crate::modes::jsonrpc::emit_events(events, outcome.events).await;
                Ok(RuntimeOptionsAck {
                    accepted: true,
                    message: None,
                })
            }
            AcpStdioControlMode::JsonRpc { commands, .. } => {
                let (ack, rx) = oneshot::channel();
                commands
                    .send(AcpStdioCommand::SwitchRuntimeOptions { req, ack })
                    .await
                    .map_err(|_| DriverError::Transport("acp-stdio task ended".into()))?;
                rx.await.map_err(|_| {
                    DriverError::Transport("acp-stdio runtime options ack dropped".into())
                })?
            }
        }
    }

    async fn runtime_options_catalog(&mut self) -> Result<RuntimeOptionsCatalog, DriverError> {
        match &mut self.mode {
            AcpStdioControlMode::Simulated { adapter, .. } => {
                adapter.runtime_options_catalog().await
            }
            AcpStdioControlMode::JsonRpc { commands, .. } => {
                let (ack, rx) = oneshot::channel();
                commands
                    .send(AcpStdioCommand::RuntimeOptionsCatalog { ack })
                    .await
                    .map_err(|_| DriverError::Transport("acp-stdio task ended".into()))?;
                rx.await.map_err(|_| {
                    DriverError::Transport("acp-stdio runtime options catalog dropped".into())
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
            AcpStdioControlMode::Simulated { adapter, events } => {
                let outcome = adapter.release(reason.to_string()).await?;
                crate::modes::jsonrpc::emit_events(events, outcome.events).await;
                Ok(())
            }
            AcpStdioControlMode::JsonRpc { commands, .. } => {
                let (ack, rx) = oneshot::channel();
                if commands
                    .send(AcpStdioCommand::Release {
                        reason: reason.to_string(),
                        ack,
                    })
                    .await
                    .is_err()
                {
                    return Ok(());
                }
                rx.await
                    .map_err(|_| DriverError::Transport("acp-stdio release ack dropped".into()))?
            }
        }
    }
}
