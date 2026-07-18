// arch: arch_A53QX.4
// orgasmic:arch_A53QX, dec_ASB1A
//! Subprocess stream-json mode.
//!
//! This mode owns the process lifecycle and stdout/stderr plumbing. Harness
//! adapters own CLI arguments, prompt payloads, and event-shape translation.

use std::process::Stdio;

use async_trait::async_trait;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, ChildStderr, ChildStdin, ChildStdout, Command};
use tokio::sync::{mpsc, oneshot};
use tracing::info;

use orgasmic_core::{DriverEvent, TextStream};

use crate::adapters::cursor::distill_subprocess_exit_summary;
use crate::r#trait::{
    AttachOutcome, BabysitterAck, BabysitterRequest, DriverConfig, DriverContext, DriverControl,
    DriverError, DriverSession, HarnessControlOutcome, HarnessEventAdapter, HarnessRequest,
    RunKind, TransitionAck, TransitionRequest, UserInputAck, UserInputRequest, WorkerDriver,
};

const MODE: &str = "subprocess-stream-json";
// orgasmic:TASK-P4MGK — harness exit / synthesized RunComplete is not the
// dispatch success signal; `orgasmic dispatch finalize` is primary.

pub struct SubprocessStreamJsonDriver {
    adapter: Box<dyn HarnessEventAdapter>,
}

impl SubprocessStreamJsonDriver {
    pub fn new(adapter: Box<dyn HarnessEventAdapter>) -> Self {
        Self { adapter }
    }
}

#[async_trait]
impl WorkerDriver for SubprocessStreamJsonDriver {
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
        let (tx, rx) = mpsc::channel(64);

        let control = match request {
            HarnessRequest::Simulated { events } => {
                for event in events {
                    let _ = tx.send(event).await;
                }
                SubprocessControlMode::Simulated {
                    adapter,
                    events: tx,
                }
            }
            HarnessRequest::Subprocess {
                binary,
                args,
                env,
                cwd,
                stdin_payload,
                close_stdin,
            } => {
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
                let Some(mut stdin) = child.stdin.take() else {
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
                if let Some(payload) = stdin_payload {
                    if let Err(e) = stdin.write_all(&payload).await {
                        let _ = child.kill().await;
                        return Err(DriverError::Transport(format!(
                            "{binary} initial write: {e}"
                        )));
                    }
                }
                let stdin = if close_stdin {
                    let _ = stdin.shutdown().await;
                    None
                } else {
                    Some(stdin)
                };
                tokio::spawn(run_subprocess_stream_json(SubprocessRuntime {
                    binary,
                    child,
                    stdin,
                    stdout,
                    stderr,
                    command_rx,
                    events: tx,
                    adapter,
                }));
                SubprocessControlMode::Real { commands, pid }
            }
            _ => {
                return Err(DriverError::Unsupported(
                    "subprocess-stream-json request shape",
                ));
            }
        };

        Ok(DriverSession {
            identity: ctx.identity.clone(),
            pid: control.pid(),
            events: rx,
            control: Box::new(SubprocessStreamJsonControl {
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

#[cfg(unix)]
extern "C" {
    fn setsid() -> i32;
    fn kill(pid: i32, sig: i32) -> i32;
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

/// Grace window between the group TERM and the group KILL escalation.
#[cfg(unix)]
const GROUP_REAP_GRACE: std::time::Duration = std::time::Duration::from_secs(2);

/// Reap the whole process group rooted at the detached child, then `wait` the
/// direct child.
///
/// `detach_subprocess` spawns the harness under `setsid`, so the child is a
/// process-group leader (its pid == pgid) and any descendants it forks — e.g.
/// cursor-agent's node `worker-server` — inherit that group. `Child::kill`
/// signals only the direct child, orphaning those descendants on every release
/// (TASK-104.3). Here we signal the *group* (`kill(-pgid, …)`): a TERM for a
/// graceful exit, a short grace window, then a KILL to anything that survived.
/// The direct child is finally `wait`ed (reaping its zombie and surfacing the
/// exit status to `finalize_subprocess_exit`), preserving existing release
/// semantics — this only adds descendant reaping.
#[cfg(unix)]
pub(crate) async fn reap_process_group(
    child: &mut Child,
) -> Result<std::process::ExitStatus, std::io::Error> {
    const SIGTERM: i32 = 15;
    const SIGKILL: i32 = 9;

    if let Some(pid) = child.id() {
        let pgid = pid as i32;
        // TERM the whole group for a graceful shutdown.
        unsafe {
            kill(-pgid, SIGTERM);
        }
        // Give the group a short window to exit on its own. Poll the direct
        // child (the group leader) as the liveness proxy: once it is gone the
        // graceful path has done its job for the common single-generation case;
        // the unconditional group KILL below still sweeps any stragglers.
        let deadline = tokio::time::Instant::now() + GROUP_REAP_GRACE;
        loop {
            match child.try_wait() {
                Ok(Some(_)) => break,
                Ok(None) => {}
                Err(_) => break,
            }
            if tokio::time::Instant::now() >= deadline {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        }
        // KILL anything still alive in the group (the leader and/or descendants
        // that ignored TERM). Harmless if the group is already gone.
        unsafe {
            kill(-pgid, SIGKILL);
        }
    } else {
        // No pid (child already reaped): fall back to a direct kill.
        let _ = child.kill().await;
    }

    child.wait().await
}

/// Non-unix fallback: no process groups, so reap the direct child as before.
#[cfg(not(unix))]
pub(crate) async fn reap_process_group(
    child: &mut Child,
) -> Result<std::process::ExitStatus, std::io::Error> {
    let _ = child.kill().await;
    child.wait().await
}

struct SubprocessRuntime {
    binary: String,
    child: Child,
    stdin: Option<ChildStdin>,
    stdout: ChildStdout,
    stderr: ChildStderr,
    command_rx: mpsc::Receiver<SubprocessCommand>,
    events: mpsc::Sender<DriverEvent>,
    adapter: Box<dyn HarnessEventAdapter>,
}

#[derive(Default)]
pub(crate) struct SubprocessExitSummary {
    assistant_text: String,
    system_chunks: Vec<String>,
}

impl SubprocessExitSummary {
    pub(crate) fn record(&mut self, event: &DriverEvent) {
        match event {
            DriverEvent::TextChunk { stream, chunk, .. } if !chunk.is_empty() => match stream {
                TextStream::Assistant => self.assistant_text.push_str(chunk),
                TextStream::System => self.system_chunks.push(chunk.clone()),
                _ => {}
            },
            _ => {}
        }
    }

    fn distill(&self) -> Option<String> {
        distill_subprocess_exit_summary(&self.assistant_text, &self.system_chunks)
    }
}

pub(crate) async fn finalize_subprocess_exit(
    binary: &str,
    wait_status: Result<std::process::ExitStatus, std::io::Error>,
    adapter: &mut dyn HarnessEventAdapter,
    events: &mpsc::Sender<DriverEvent>,
    exit_summary: &SubprocessExitSummary,
) {
    let terminal_emitted = adapter.terminal_emitted();
    let distilled = exit_summary.distill();
    let exit_code = wait_status.as_ref().ok().and_then(|status| status.code());
    info!(
        binary,
        exit_code = ?exit_code,
        terminal_emitted,
        distill_is_some = distilled.is_some(),
        assistant_len = exit_summary.assistant_text.len(),
        system_chunks = exit_summary.system_chunks.len(),
        "subprocess-stream-json exit synthesis decision"
    );
    if terminal_emitted {
        return;
    }
    if let Some(summary) = distilled {
        if matches!(wait_status, Ok(status) if status.success()) {
            adapter.emit_run_complete_once(events, Some(summary)).await;
            return;
        }
    }
    match wait_status {
        Ok(status) if !status.success() => {
            let _ = events
                .send(DriverEvent::DriverError {
                    fatal: true,
                    message: format!("{binary} exited with status {status}"),
                })
                .await;
        }
        Err(e) => {
            let _ = events
                .send(DriverEvent::DriverError {
                    fatal: true,
                    message: format!("{binary} wait: {e}"),
                })
                .await;
        }
        Ok(_) => {}
    }
}

async fn run_subprocess_stream_json(runtime: SubprocessRuntime) {
    let SubprocessRuntime {
        binary,
        mut child,
        mut stdin,
        stdout,
        stderr,
        command_rx,
        events,
        mut adapter,
    } = runtime;
    let mut commands = command_rx;
    let mut stdout = BufReader::new(stdout).lines();
    let mut stderr = BufReader::new(stderr).lines();
    let mut stdout_open = true;
    let mut stderr_open = true;
    let mut released = false;
    let mut exit_summary = SubprocessExitSummary::default();

    while stdout_open || stderr_open {
        tokio::select! {
            line = stdout.next_line(), if stdout_open => {
                match line {
                    Ok(Some(line)) => {
                        let outgoing = adapter.parse_stdout_line(&line).await;
                        for event in &outgoing {
                            exit_summary.record(event);
                        }
                        emit_events(&events, outgoing).await;
                    }
                    Ok(None) => stdout_open = false,
                    Err(e) => {
                        if !adapter.terminal_emitted() {
                            let _ = events.send(DriverEvent::DriverError {
                                fatal: true,
                                message: format!("{binary} stdout read: {e}"),
                            }).await;
                        }
                        stdout_open = false;
                    }
                }
            }
            line = stderr.next_line(), if stderr_open => {
                match line {
                    Ok(Some(line)) => {
                        if adapter.ignores_stderr_line(&line) {
                            continue;
                        }
                        let event = adapter.stderr_event(line);
                        let _ = events.send(event).await;
                    }
                    Ok(None) => stderr_open = false,
                    Err(e) => {
                        if !adapter.terminal_emitted() {
                            let _ = events.send(DriverEvent::DriverError {
                                fatal: true,
                                message: format!("{binary} stderr read: {e}"),
                            }).await;
                        }
                        stderr_open = false;
                    }
                }
            }
            cmd = commands.recv() => {
                match cmd {
                    Some(cmd) => {
                        if handle_subprocess_command(
                            cmd,
                            &events,
                            stdin.as_mut(),
                            adapter.as_mut(),
                        )
                        .await {
                            released = true;
                            break;
                        }
                    }
                    None => {
                        released = true;
                        break;
                    }
                }
            }
        }
    }

    // On release, reap the whole setsid process group (direct child plus any
    // forked descendants), not just the direct child; otherwise wait the child
    // out as it exits on its own.
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

async fn handle_subprocess_command(
    cmd: SubprocessCommand,
    events: &mpsc::Sender<DriverEvent>,
    stdin: Option<&mut ChildStdin>,
    adapter: &mut dyn HarnessEventAdapter,
) -> bool {
    match cmd {
        SubprocessCommand::TransitionState { req, ack } => {
            let result = adapter.transition_state(req).await;
            let done = match result {
                Ok(outcome) => match apply_outcome(outcome, events, stdin).await {
                    Ok(done) => {
                        let _ = ack.send(Ok(TransitionAck {
                            accepted: true,
                            message: None,
                        }));
                        done
                    }
                    Err(e) => {
                        let _ = ack.send(Err(e));
                        false
                    }
                },
                Err(e) => {
                    let _ = ack.send(Err(e));
                    false
                }
            };
            done
        }
        SubprocessCommand::BabysitterAction { req, ack } => {
            let result = adapter.babysitter_action(req).await;
            let done = match result {
                Ok(outcome) => match apply_outcome(outcome, events, stdin).await {
                    Ok(done) => {
                        let _ = ack.send(Ok(BabysitterAck {
                            accepted: true,
                            message: None,
                        }));
                        done
                    }
                    Err(e) => {
                        let _ = ack.send(Err(e));
                        false
                    }
                },
                Err(e) => {
                    let _ = ack.send(Err(e));
                    false
                }
            };
            done
        }
        SubprocessCommand::SendInput { req, ack } => {
            let result = adapter.send_input(req).await;
            let done = match result {
                Ok(outcome) => match apply_outcome(outcome, events, stdin).await {
                    Ok(done) => {
                        let _ = ack.send(Ok(UserInputAck {
                            accepted: true,
                            message: None,
                        }));
                        done
                    }
                    Err(e) => {
                        let _ = ack.send(Err(e));
                        false
                    }
                },
                Err(e) => {
                    let _ = ack.send(Err(e));
                    false
                }
            };
            done
        }
        SubprocessCommand::Release { reason, ack } => {
            let result = adapter.release(reason).await;
            let done = match result {
                Ok(outcome) => match apply_outcome(outcome, events, stdin).await {
                    Ok(done) => {
                        let _ = ack.send(Ok(()));
                        done
                    }
                    Err(e) => {
                        let _ = ack.send(Err(e));
                        false
                    }
                },
                Err(e) => {
                    let _ = ack.send(Err(e));
                    false
                }
            };
            done
        }
    }
}

async fn apply_outcome(
    outcome: HarnessControlOutcome,
    events: &mpsc::Sender<DriverEvent>,
    mut stdin: Option<&mut ChildStdin>,
) -> Result<bool, DriverError> {
    for payload in outcome.stdin_payloads {
        let Some(stdin) = stdin.as_deref_mut() else {
            return Err(DriverError::Transport(
                "subprocess stdin unavailable for control write".into(),
            ));
        };
        stdin
            .write_all(&payload)
            .await
            .map_err(|e| DriverError::Transport(format!("subprocess control write: {e}")))?;
        stdin
            .flush()
            .await
            .map_err(|e| DriverError::Transport(format!("subprocess control flush: {e}")))?;
    }
    emit_events(events, outcome.events).await;
    if outcome.close {
        if let Some(stdin) = stdin {
            let _ = stdin.shutdown().await;
        }
    }
    Ok(outcome.close)
}

async fn emit_events(events: &mpsc::Sender<DriverEvent>, outgoing: Vec<DriverEvent>) {
    for event in outgoing {
        let _ = events.send(event).await;
    }
}

enum SubprocessCommand {
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
    Release {
        reason: String,
        ack: oneshot::Sender<Result<(), DriverError>>,
    },
}

enum SubprocessControlMode {
    Simulated {
        adapter: Box<dyn HarnessEventAdapter>,
        events: mpsc::Sender<DriverEvent>,
    },
    Real {
        commands: mpsc::Sender<SubprocessCommand>,
        pid: Option<u32>,
    },
}

impl SubprocessControlMode {
    fn pid(&self) -> Option<u32> {
        match self {
            Self::Real { pid, .. } => *pid,
            Self::Simulated { .. } => None,
        }
    }
}

struct SubprocessStreamJsonControl {
    mode: SubprocessControlMode,
    kind: RunKind,
    released: bool,
}

#[async_trait]
impl DriverControl for SubprocessStreamJsonControl {
    async fn transition_state(
        &mut self,
        req: TransitionRequest,
    ) -> Result<TransitionAck, DriverError> {
        if self.kind == RunKind::Babysitter {
            return Err(DriverError::WorkerToolBlocked("transition_state".into()));
        }
        match &mut self.mode {
            SubprocessControlMode::Simulated { adapter, events } => {
                let outcome = adapter.transition_state(req).await?;
                emit_events(events, outcome.events).await;
                Ok(TransitionAck {
                    accepted: true,
                    message: None,
                })
            }
            SubprocessControlMode::Real { commands, .. } => {
                let (ack, rx) = oneshot::channel();
                commands
                    .send(SubprocessCommand::TransitionState { req, ack })
                    .await
                    .map_err(|_| DriverError::Transport("subprocess task ended".into()))?;
                rx.await.map_err(|_| {
                    DriverError::Transport("subprocess transition ack dropped".into())
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
            SubprocessControlMode::Simulated { adapter, events } => {
                let outcome = adapter.babysitter_action(req).await?;
                emit_events(events, outcome.events).await;
                Ok(BabysitterAck {
                    accepted: true,
                    message: None,
                })
            }
            SubprocessControlMode::Real { commands, .. } => {
                let (ack, rx) = oneshot::channel();
                commands
                    .send(SubprocessCommand::BabysitterAction { req, ack })
                    .await
                    .map_err(|_| DriverError::Transport("subprocess task ended".into()))?;
                rx.await.map_err(|_| {
                    DriverError::Transport("subprocess babysitter ack dropped".into())
                })?
            }
        }
    }

    async fn send_input(&mut self, req: UserInputRequest) -> Result<UserInputAck, DriverError> {
        match &mut self.mode {
            SubprocessControlMode::Simulated { adapter, events } => {
                let outcome = adapter.send_input(req).await?;
                emit_events(events, outcome.events).await;
                Ok(UserInputAck {
                    accepted: true,
                    message: None,
                })
            }
            SubprocessControlMode::Real { commands, .. } => {
                let (ack, rx) = oneshot::channel();
                commands
                    .send(SubprocessCommand::SendInput { req, ack })
                    .await
                    .map_err(|_| DriverError::Transport("subprocess task ended".into()))?;
                rx.await
                    .map_err(|_| DriverError::Transport("subprocess input ack dropped".into()))?
            }
        }
    }

    async fn release(&mut self, reason: &str) -> Result<(), DriverError> {
        if self.released {
            return Ok(());
        }
        self.released = true;
        match &mut self.mode {
            SubprocessControlMode::Simulated { adapter, events } => {
                let outcome = adapter.release(reason.to_string()).await?;
                emit_events(events, outcome.events).await;
                Ok(())
            }
            SubprocessControlMode::Real { commands, .. } => {
                let (ack, rx) = oneshot::channel();
                if commands
                    .send(SubprocessCommand::Release {
                        reason: reason.to_string(),
                        ack,
                    })
                    .await
                    .is_err()
                {
                    return Ok(());
                }
                rx.await
                    .map_err(|_| DriverError::Transport("subprocess release ack dropped".into()))?
            }
        }
    }
}
