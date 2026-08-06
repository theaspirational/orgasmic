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
use tracing::{info, warn};

use orgasmic_core::{DriverEvent, TextStream};

use crate::adapters::cursor::distill_subprocess_exit_summary;
use crate::catalog::TransportInteraction;
use crate::r#trait::{
    preflight_via_adapter, AttachOutcome, BabysitterAck, BabysitterRequest, DriverConfig,
    DriverContext, DriverControl, DriverError, DriverSession, HarnessControlOutcome,
    HarnessEventAdapter, HarnessRequest, PreflightOutcome, RunKind, TransitionAck,
    TransitionRequest, UserInputAck, UserInputRequest, WorkerDriver,
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

    /// Spawn a request that has **already been composed**, by the caller, on the
    /// adapter handed in here.
    ///
    /// The stdio mode delegates the plain-subprocess shape to this mode. It used to
    /// do so by handing over a fresh adapter clone and letting `acquire`
    /// compose a second time, which meant every stdio claude dispatch built
    /// its argv twice and — until the credential plan was pinned — detected its
    /// credentials twice, after the lease was already held (TASK-KKBTP). The
    /// request the mode spawns is now the request the caller composed, so there
    /// is one composition per dispatch and no way for the two to disagree.
    pub(crate) async fn acquire_composed(
        adapter: Box<dyn HarnessEventAdapter>,
        ctx: DriverContext,
        request: HarnessRequest,
        native_runtime: Option<crate::r#trait::NativeRuntimeMeta>,
    ) -> Result<DriverSession, DriverError> {
        spawn_composed(adapter, ctx, request, native_runtime).await
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

    /// A subprocess speaking stream-json over pipes: no terminal, no operator.
    fn interaction(&self) -> TransportInteraction {
        TransportInteraction::Unattended
    }

    fn validate(&self, config: &DriverConfig) -> Result<(), DriverError> {
        self.adapter.validate_config(config)
    }

    /// Readiness is the harness's question, not the transport's (see
    /// [`preflight_via_adapter`]).
    async fn preflight(&self, ctx: &DriverContext, config: &DriverConfig) -> PreflightOutcome {
        preflight_via_adapter(self.adapter.as_ref(), ctx, config).await
    }

    async fn acquire(
        &self,
        ctx: DriverContext,
        config: DriverConfig,
    ) -> Result<DriverSession, DriverError> {
        let mut adapter = self.adapter.clone_box();
        let request = adapter.compose_request(&ctx, &config)?;
        // Read straight after composing, before `adapter` is moved into the
        // control below. The adapter pins its harness-native session id while
        // building the argv; this used to be hardcoded `None` here, so a run
        // reaching this driver — which is every stdio claude run, via the
        // delegation in `StdioDriver::acquire` — recorded no NativeRuntime
        // lifecycle event at all, and recovery could never offer
        // `resume_native_fork` (TASK-VB9DQ item 3, TASK-SGRTX).
        let native_runtime = adapter.native_runtime();
        spawn_composed(adapter, ctx, request, native_runtime).await
    }

    async fn attach(
        &self,
        _ctx: DriverContext,
        _config: DriverConfig,
    ) -> Result<AttachOutcome, DriverError> {
        Ok(AttachOutcome::NotReattachable)
    }
}

/// Own the process lifecycle for an already-composed request.
async fn spawn_composed(
    adapter: Box<dyn HarnessEventAdapter>,
    ctx: DriverContext,
    request: HarnessRequest,
    native_runtime: Option<crate::r#trait::NativeRuntimeMeta>,
) -> Result<DriverSession, DriverError> {
    let (tx, rx) = mpsc::channel(64);

    let (control, producer) = match request {
        HarnessRequest::Simulated { events } => {
            for event in events {
                let _ = tx.send(event).await;
            }
            (
                SubprocessControlMode::Simulated {
                    adapter,
                    events: tx,
                },
                None,
            )
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
            let producer = tokio::spawn(run_subprocess_stream_json(SubprocessRuntime {
                binary,
                child,
                stdin,
                stdout,
                stderr,
                command_rx,
                events: tx,
                adapter,
            }));
            (
                SubprocessControlMode::Real { commands, pid },
                Some(producer),
            )
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
        producer,
        native_runtime,
    })
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
///
/// Kept as a bare millisecond count as well as a `Duration` because
/// [`RELEASE_DRAIN_BUDGET`] is derived from it in a `const` expression, and
/// `Duration` has no const subtraction.
pub(crate) const GROUP_REAP_GRACE_MS: u64 = 2_000;

/// Grace window between the group TERM and the group KILL escalation.
#[cfg(unix)]
const GROUP_REAP_GRACE: std::time::Duration = std::time::Duration::from_millis(GROUP_REAP_GRACE_MS);

/// What the supervisor gives this producer task after a release has been
/// acked, before it aborts the task outright.
///
/// This mirrors `DRIVER_RELEASE_TIMEOUT` in `orgasmic-daemon`'s
/// `supervisor.rs`: `stop_and_join_driver_producer` awaits `control.release`
/// under that budget and then joins *this* task under the same budget again.
/// `orgasmic-daemon` depends on `orgasmic-drivers`, not the other way round,
/// so the constant cannot be imported; it is named here so a change to it has
/// one place to look, and so the derivation below is arithmetic rather than
/// prose.
pub(crate) const PRODUCER_JOIN_BUDGET_MS: u64 = 5_000;

/// Slack reserved for [`finalize_subprocess_exit`] once the drain is done: its
/// synthesized `RunComplete`/`DriverError` go onto a bounded (64) event
/// channel whose receiver is the supervisor's own release drain.
pub(crate) const FINALIZE_SLACK_MS: u64 = 1_000;

/// How long the post-release drain may keep reading the harness's pipes.
///
/// orgasmic:TASK-SVKPN — derived, not chosen. Everything this task does after
/// the select loop breaks runs inside the producer join the supervisor bounds
/// at [`PRODUCER_JOIN_BUDGET_MS`], and exactly two of those steps can block:
/// the group reap ([`GROUP_REAP_GRACE_MS`] — TERM, grace, KILL) and this
/// drain. Reserving [`FINALIZE_SLACK_MS`] for the synthesis that follows
/// leaves the drain what is left, so the whole teardown fits the budget the
/// supervisor already assumes a release fits inside. Overrunning the drain
/// costs only the events still unread — `finalize_subprocess_exit` still runs
/// on what was drained; overrunning the *join* would cost the whole
/// synthesis, because the supervisor then aborts this task mid-finalize.
///
/// This is the driver-side sibling of TASK-HAREX's `DrainGate`, which bounds
/// the *supervisor's* wait on the other end of this channel. They compose —
/// a driver drain that fits here cannot push the supervisor's drain past
/// `RELEASE_FINALIZATION_DRAIN_TIMEOUT` — and they are not the same gate.
pub(crate) const RELEASE_DRAIN_BUDGET: std::time::Duration = std::time::Duration::from_millis(
    PRODUCER_JOIN_BUDGET_MS - GROUP_REAP_GRACE_MS - FINALIZE_SLACK_MS,
);

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

    // orgasmic:TASK-SVKPN — recover whatever the harness wrote before the break.
    // Only the release path needs this: the loop's own exit condition is "both
    // pipes are at EOF", so a loop that ended on its own has nothing left.
    if released
        && (stdout_open || stderr_open)
        && tokio::time::timeout(
            RELEASE_DRAIN_BUDGET,
            drain_child_streams(
                &binary,
                &mut stdout,
                &mut stdout_open,
                &mut stderr,
                &mut stderr_open,
                adapter.as_mut(),
                &events,
                &mut exit_summary,
            ),
        )
        .await
        .is_err()
    {
        warn!(
            binary,
            budget_ms = RELEASE_DRAIN_BUDGET.as_millis() as u64,
            "harness pipes did not reach EOF within the post-release drain \
             budget; synthesizing the exit from what was drained"
        );
    }

    finalize_subprocess_exit(
        &binary,
        wait_status,
        adapter.as_mut(),
        &events,
        &exit_summary,
    )
    .await;
}

/// Read whatever the harness already wrote but the select loop had not yet
/// consumed when the command branch broke it.
///
/// orgasmic:TASK-SVKPN. The loop above leaves on the command branch — an
/// explicit release, or the command channel closing — and `tokio::select!`
/// picks at random among ready branches, so the break can land with the
/// harness's entire output still sitting unread in the pipe. Measured
/// (TASK-Z7VQK): a harness that ran to completion in 9.5ms and printed all 16
/// of its lines had *zero* of them recorded, because the daemon's early-exit
/// watcher observed the pid gone and released while every line was still
/// pending. `finalize_subprocess_exit` then distilled an empty summary
/// (`distill_is_some=false assistant_len=0 system_chunks=0`), no `RunComplete`
/// was synthesized, and the run was orphaned as `protocol_end_without_finalize`
/// with an empty transcript — product-visible data loss, not a test artifact.
///
/// Note what this deliberately is *not*: a `biased;` in the loop above with
/// stdout first. That would make the loop prefer output over commands, which
/// fixes the ordering only for a harness that stops talking — and lets one
/// that never stops starve the release branch indefinitely, converting a lost
/// transcript into a wedged release. The bound belongs on the recovery, not on
/// the loop's fairness. `biased;` *here* is safe and wanted: there is no
/// command branch left to starve, and stdout carries the transcript.
///
/// Called after the child has been reaped, so no writer in the harness's
/// process group survives to hold the pipes open and EOF is the ordinary exit;
/// [`RELEASE_DRAIN_BUDGET`] covers the case where some unrelated process
/// inherited the write end. Data already in the pipe outlives its writer, so
/// reaping first costs nothing and additionally captures whatever the harness
/// flushed on the group TERM.
#[allow(clippy::too_many_arguments)]
async fn drain_child_streams(
    binary: &str,
    stdout: &mut tokio::io::Lines<BufReader<ChildStdout>>,
    stdout_open: &mut bool,
    stderr: &mut tokio::io::Lines<BufReader<ChildStderr>>,
    stderr_open: &mut bool,
    adapter: &mut dyn HarnessEventAdapter,
    events: &mpsc::Sender<DriverEvent>,
    exit_summary: &mut SubprocessExitSummary,
) {
    while *stdout_open || *stderr_open {
        tokio::select! {
            biased;
            line = stdout.next_line(), if *stdout_open => {
                match line {
                    Ok(Some(line)) => {
                        let outgoing = adapter.parse_stdout_line(&line).await;
                        for event in &outgoing {
                            exit_summary.record(event);
                        }
                        emit_events(events, outgoing).await;
                    }
                    Ok(None) => *stdout_open = false,
                    Err(e) => {
                        // A read error on a reaped child's pipe is the drain's
                        // boundary, not a run failure: the loop above would
                        // have emitted a fatal `DriverError` here, which after
                        // a release would only compete with the synthesis that
                        // follows. Stop reading and let it run.
                        warn!(binary, error = %e, "post-release stdout drain read error");
                        *stdout_open = false;
                    }
                }
            }
            line = stderr.next_line(), if *stderr_open => {
                match line {
                    Ok(Some(line)) => {
                        if adapter.ignores_stderr_line(&line) {
                            continue;
                        }
                        let event = adapter.stderr_event(line);
                        let _ = events.send(event).await;
                    }
                    Ok(None) => *stderr_open = false,
                    Err(e) => {
                        warn!(binary, error = %e, "post-release stderr drain read error");
                        *stderr_open = false;
                    }
                }
            }
        }
    }
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
