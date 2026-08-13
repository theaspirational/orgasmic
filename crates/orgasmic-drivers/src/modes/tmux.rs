// orgasmic:arch_A53QX, arch_R3EPE, arch_QXS5W, dec_ASB1A
//! Tmux mode driver.
//!
//! Wraps any agentic CLI inside a tmux session and bridges the operator's
//! chat panel to it. The manager runs through this driver (`dec_011`); we
//! also use it as the smoke-test driver for the supervisor because it
//! doesn't require an external transport.
//!
//! In v0.0.1 the driver runs in **inert mode** unless a tmux binary is
//! available on `PATH`. Inert mode emits a synthetic `Ready` event, accepts
//! `transition_state` and `release`, and otherwise does nothing — that is
//! enough to drive supervisor lease / session-write tests on a CI box
//! without tmux. When tmux is available the driver spawns a real session
//! (`tmux new-session -d`), runs the configured command, and tears the
//! session down on `release`.

use std::fs::{File, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::process::{Command as StdCommand, Stdio};
use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc,
};
use std::time::Duration;

use async_trait::async_trait;
use serde::Deserialize;
use serde_json::json;
use tokio::io::AsyncWriteExt;
use tokio::process::Child;
use tokio::sync::mpsc;
use tokio::task::JoinHandle;

use orgasmic_core::{DriverEvent, RuntimeIdentity};

use crate::catalog::TransportInteraction;
use crate::r#trait::{
    preflight_via_adapter, AttachOutcome, Attached, DriverConfig, DriverContext, DriverControl,
    DriverError, DriverSession, HarnessEventAdapter, ManagerWakeRequest, NativeRuntimeMeta,
    PreflightOutcome, TransitionAck, TransitionRequest, UserInputAck, UserInputRequest,
    WorkerDriver,
};

const MODE: &str = "tmux";

pub struct TmuxDriver {
    adapter: Box<dyn HarnessEventAdapter>,
}

impl TmuxDriver {
    pub fn new(adapter: Box<dyn HarnessEventAdapter>) -> Self {
        Self { adapter }
    }
}

#[derive(Debug, Clone, Deserialize, Default)]
struct TmuxTuiConfig {
    /// Opaque daemon-minted capability for a bare app terminal. Never comes
    /// from user configuration and is stripped before the daemon persists
    /// RunMeta; it is exported only to the child pane.
    #[serde(default)]
    manager_terminal_capability: Option<String>,
    #[serde(default)]
    command: Option<String>,
    #[serde(default)]
    args: Vec<String>,
    #[serde(default)]
    cwd: Option<PathBuf>,
    #[serde(default)]
    harness: Option<String>,
    #[serde(default)]
    model: Option<String>,
    /// Extra argv appended verbatim to the harness CLI (worker
    /// `:HARNESS_ARGS:` / launch request). Appended before the guarded flag
    /// pushes in `build_spawn_plan`, so an explicit `--model` here wins.
    #[serde(default)]
    harness_args: Vec<String>,
    #[serde(default)]
    effort: Option<String>,
    #[serde(default)]
    reasoning_effort: Option<String>,
    /// Force inert mode (no real tmux interaction) even if `tmux` is on PATH.
    /// Test-only knob; production callers leave this unset.
    #[serde(default)]
    force_inert: bool,
    /// When true, launch argv is trusted resume/fork only — do not append a
    /// fresh `--session-id`, initial prompt bundle, or other fresh-launch flags.
    #[serde(default)]
    native_resume_mode: bool,
    /// Daemon-authenticated provider identity, independent of the executable
    /// target basename (Claude's real version target is named `2.x.y`).
    #[serde(default)]
    trusted_provider_identity: Option<String>,
    /// Daemon-pinned executable identity and the trusted orgasmic wrapper
    /// which opens, verifies, and executes a retained alias of that inode.
    #[serde(default)]
    pinned_executable: Option<PinnedExecutableIdentity>,
    /// Provider state root captured with the executable identity. Recovery
    /// never rediscovers this from ambient HOME at the launch boundary.
    #[serde(default)]
    provider_home: Option<PathBuf>,
    #[serde(default)]
    prompt_bundle_text: Option<String>,
    #[serde(
        default = "default_input_ready_timeout",
        deserialize_with = "deserialize_duration_secs"
    )]
    input_ready_timeout: Duration,
}

#[derive(Debug, Clone, Deserialize)]
struct PinnedExecutableIdentity {
    path: PathBuf,
    #[cfg(unix)]
    dev: u64,
    #[cfg(unix)]
    ino: u64,
    exec_wrapper: PathBuf,
    #[cfg(unix)]
    exec_wrapper_dev: u64,
    #[cfg(unix)]
    exec_wrapper_ino: u64,
}

pub(crate) fn default_input_ready_timeout() -> Duration {
    Duration::from_secs(10)
}

pub(crate) fn deserialize_duration_secs<'de, D>(deserializer: D) -> Result<Duration, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let secs = u64::deserialize(deserializer)?;
    Ok(Duration::from_secs(secs))
}

#[async_trait]
impl WorkerDriver for TmuxDriver {
    fn transport(&self) -> &'static str {
        MODE
    }

    fn harness(&self) -> Option<&'static str> {
        Some(self.adapter.harness())
    }

    /// The harness runs as its own TUI inside a tmux pane an operator can
    /// attach to; the pane runtime must exist for the run to start at all.
    fn interaction(&self) -> TransportInteraction {
        TransportInteraction::TerminalPane
    }

    fn validate(&self, config: &DriverConfig) -> Result<(), DriverError> {
        let cfg: TmuxTuiConfig = serde_json::from_value(config.0.clone())
            .map_err(|e| DriverError::InvalidConfig(e.to_string()))?;
        if let Some(cwd) = cfg.cwd.as_ref() {
            if !cwd.exists() {
                return Err(DriverError::InvalidConfig(format!(
                    "cwd does not exist: {}",
                    cwd.display()
                )));
            }
        }
        if cfg.trusted_provider_identity.as_deref() == Some("claude") {
            let pin = cfg.pinned_executable.as_ref().ok_or_else(|| {
                DriverError::InvalidConfig(
                    "trusted Claude execution requires pinned_executable".into(),
                )
            })?;
            if !pin.path.is_absolute()
                || !pin.exec_wrapper.is_absolute()
                || !cfg
                    .provider_home
                    .as_ref()
                    .is_some_and(|home| home.is_absolute())
            {
                return Err(DriverError::InvalidConfig(
                    "pinned executable, wrapper, and provider home paths must be absolute".into(),
                ));
            }
        } else if cfg.pinned_executable.is_some() {
            return Err(DriverError::InvalidConfig(
                "pinned executable requires trusted provider identity".into(),
            ));
        }
        Ok(())
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
        let cfg: TmuxTuiConfig = serde_json::from_value(config.0.clone())
            .map_err(|e| DriverError::InvalidConfig(e.to_string()))?;
        let (tx, rx) = mpsc::channel(64);
        let harness = cfg
            .harness
            .as_deref()
            .unwrap_or_else(|| self.adapter.harness());
        let spawn_plan = build_spawn_plan(&cfg, &ctx, harness);
        let inert_reason = inert_reason(&cfg, &spawn_plan.command);
        let inert = inert_reason.is_some();
        let session_name = tmux_session_name(&ctx.identity);
        let terminal_emitted = Arc::new(AtomicBool::new(false));
        let startup_cancel = Arc::new(AtomicBool::new(false));
        let send_child = SendChildOwner::new();
        let mut native_runtime = spawn_plan.native_runtime.clone();

        // orgasmic:TASK-AFE5Q,TASK-756WX
        let (lifecycle_task, startup_task, pane_activity_task) = if !inert {
            let launch_observation = spawn_tmux_session(&session_name, &spawn_plan).await?;
            if cfg.native_resume_mode
                && is_claude_harness_command(
                    harness,
                    &spawn_plan.command,
                    cfg.trusted_provider_identity.as_deref(),
                )
            {
                if let Some(resumed) = extract_resume_session_id(&spawn_plan.args) {
                    let observation = launch_observation.ok_or_else(|| {
                        DriverError::Transport(
                            "trusted Claude resume did not establish a launch boundary".into(),
                        )
                    })?;
                    let discovery = wait_for_claude_fork_session_id(
                        &resumed,
                        observation.since,
                        &observation.excluded,
                        &observation.directory,
                    )
                    .await;
                    match discovery {
                        ForkDiscoveryResult::Unique(fork_id) => {
                            native_runtime = Some(claude_native_runtime_with_home(
                                &fork_id,
                                &spawn_plan.cwd,
                                &spawn_plan.command,
                                &spawn_plan.args,
                                spawn_plan.provider_home.as_deref(),
                            ));
                        }
                        ForkDiscoveryResult::Ambiguous => {
                            kill_tmux_session(&session_name).await;
                            return Err(DriverError::Transport(
                                "ambiguous Claude fork session discovery".into(),
                            ));
                        }
                        ForkDiscoveryResult::NotFound => {
                            kill_tmux_session(&session_name).await;
                            return Err(DriverError::Transport(
                                "Claude fork session not discovered within launch bounds".into(),
                            ));
                        }
                    }
                }
            }
            let task = start_session_exit_watch(
                session_name.clone(),
                tx.clone(),
                terminal_emitted.clone(),
            );
            // orgasmic:TASK-4CSMY — the stall clock's pane channel. Started
            // next to the exit watch because both are per-live-session and
            // both end at release.
            let pane_activity_task = start_pane_activity_watch(
                session_name.clone(),
                tx.clone(),
                terminal_emitted.clone(),
            );
            // Paste fallback only (hermes/custom, or a harness without argv
            // delivery). Supported TUIs already received the prompt in argv.
            // Deliver in the background so `acquire` returns promptly.
            let startup_task = if let Some(prompt) = spawn_plan.paste_prompt.clone() {
                let session = session_name.clone();
                let command = spawn_plan.command.clone();
                let timeout = cfg.input_ready_timeout;
                let deliver_tx = tx.clone();
                let deliver_terminal = terminal_emitted.clone();
                let send_child = send_child.clone();
                let cancel = startup_cancel.clone();
                Some(tokio::spawn(async move {
                    deliver_prompt(
                        &session,
                        &command,
                        &prompt,
                        timeout,
                        &deliver_tx,
                        &deliver_terminal,
                        Some(send_child),
                        Some(cancel),
                    )
                    .await;
                }))
            } else if cursor_argv_needs_startup_trust(harness, &spawn_plan.paste_prompt) {
                // Cursor preserves argv across the workspace-trust gate, but
                // fresh worktrees block until `[a] Trust this workspace` is sent.
                let session = session_name.clone();
                let workspace = ctx
                    .worktree
                    .as_deref()
                    .map(|path| path.display().to_string())
                    .unwrap_or_default();
                let timeout = cfg.input_ready_timeout;
                let cancel = startup_cancel.clone();
                let send_child = send_child.clone();
                Some(tokio::spawn(async move {
                    if let Err(e) = accept_cursor_workspace_trust(
                        &session,
                        &workspace,
                        timeout,
                        Some(cancel),
                        Some(send_child),
                    )
                    .await
                    {
                        tracing::warn!(
                            ?e,
                            "cursor workspace trust gate not cleared within timeout"
                        );
                    }
                }))
            } else {
                None
            };
            (Some(task), startup_task, pane_activity_task)
        } else {
            if cfg.native_resume_mode
                && is_claude_harness_command(
                    harness,
                    &spawn_plan.command,
                    cfg.trusted_provider_identity.as_deref(),
                )
            {
                if let Some(resumed) = extract_resume_session_id(&spawn_plan.args) {
                    let fork_id = deterministic_inert_fork_session_id(&ctx.identity.runtime_id);
                    native_runtime = Some(claude_native_runtime_with_home(
                        &fork_id,
                        &spawn_plan.cwd,
                        &spawn_plan.command,
                        &spawn_plan.args,
                        spawn_plan.provider_home.as_deref(),
                    ));
                    let _ = resumed;
                }
            }
            (None, None, None)
        };

        let _ = tx
            .send(DriverEvent::Ready {
                protocol_version: "tmux-tui/1".into(),
                capabilities: json!({
                    "inert": inert,
                    "inert_reason": inert_reason,
                    "kind": ctx.run_kind,
                    "session": if inert { None::<String> } else { Some(session_name.clone()) },
                    "command": spawn_plan.command,
                    "args": spawn_plan.args,
                    "model": cfg.model,
                    "effort": cfg.effort.or(cfg.reasoning_effort),
                }),
            })
            .await;

        Ok(DriverSession {
            identity: ctx.identity.clone(),
            pid: None,
            events: rx,
            control: Box::new(TmuxTuiControl {
                events: Some(tx),
                session_name,
                inert,
                lifecycle_abort: lifecycle_task.as_ref().map(JoinHandle::abort_handle),
                pane_activity_abort: pane_activity_task.as_ref().map(JoinHandle::abort_handle),
                startup_task,
                startup_cancel,
                send_child,
                input_ready_timeout: cfg.input_ready_timeout,
                terminal_emitted,
                kill_on_drop: true,
                released: false,
            }),
            producer: lifecycle_task,
            native_runtime,
        })
    }

    async fn attach(
        &self,
        ctx: DriverContext,
        config: DriverConfig,
    ) -> Result<AttachOutcome, DriverError> {
        let cfg: TmuxTuiConfig = serde_json::from_value(config.0.clone())
            .map_err(|e| DriverError::InvalidConfig(e.to_string()))?;
        if cfg.force_inert {
            return Ok(AttachOutcome::NotReattachable);
        }

        // The async, kill-on-drop has-session command is both the availability
        // and liveness proof. Avoid the synchronous `tmux -V` discovery used
        // by acquisition so a bounded inventory probe cannot pin its executor.
        let session_name = tmux_session_name(&ctx.identity);
        if !has_tmux_session(&session_name).await? {
            return Ok(AttachOutcome::NotReattachable);
        }

        let (tx, rx) = mpsc::channel(64);
        let _ = tx
            .send(DriverEvent::Ready {
                protocol_version: "tmux-tui/1".into(),
                capabilities: json!({
                    "inert": false,
                    "reattached": true,
                    "kind": ctx.run_kind,
                    "session": session_name.clone(),
                }),
            })
            .await;
        let terminal_emitted = Arc::new(AtomicBool::new(false));
        let lifecycle_task =
            start_session_exit_watch(session_name.clone(), tx.clone(), terminal_emitted.clone());
        let lifecycle_abort = lifecycle_task.abort_handle();
        // orgasmic:TASK-4CSMY — a reattached run is stall-clocked like any
        // other, so it needs the pane channel too.
        let pane_activity_abort =
            start_pane_activity_watch(session_name.clone(), tx.clone(), terminal_emitted.clone())
                .as_ref()
                .map(JoinHandle::abort_handle);

        Ok(AttachOutcome::Attached(Attached {
            session: Box::new(DriverSession {
                identity: ctx.identity.clone(),
                pid: None,
                events: rx,
                control: Box::new(TmuxTuiControl {
                    events: Some(tx.clone()),
                    session_name: session_name.clone(),
                    inert: false,
                    lifecycle_abort: Some(lifecycle_abort),
                    pane_activity_abort,
                    startup_task: None,
                    startup_cancel: Arc::new(AtomicBool::new(false)),
                    send_child: SendChildOwner::new(),
                    input_ready_timeout: cfg.input_ready_timeout,
                    terminal_emitted,
                    kill_on_drop: false,
                    released: false,
                }),
                producer: Some(lifecycle_task),
                native_runtime: None,
            }),
        }))
    }
}

struct TmuxTuiControl {
    events: Option<mpsc::Sender<DriverEvent>>,
    session_name: String,
    inert: bool,
    /// Watches pane/process end only — never scrollback capture (TASK-AFE5Q).
    lifecycle_abort: Option<tokio::task::AbortHandle>,
    // orgasmic:TASK-4CSMY
    /// Counts pane output bytes into the coalesced `PaneActivity` liveness
    /// event. Aborted on release, which drops the FIFO with it.
    pane_activity_abort: Option<tokio::task::AbortHandle>,
    /// One-shot startup helper (prompt paste or Cursor trust gate).
    startup_task: Option<JoinHandle<()>>,
    startup_cancel: Arc<AtomicBool>,
    /// In-flight tmux CLI send child; killed/reaped before release returns.
    send_child: SendChildOwner,
    input_ready_timeout: Duration,
    terminal_emitted: Arc<AtomicBool>,
    kill_on_drop: bool,
    released: bool,
}

fn abort_driver_task(task: Option<JoinHandle<()>>) {
    if let Some(task) = task {
        task.abort();
    }
}

pub(crate) async fn cancel_and_join_driver_task(
    cancel: &AtomicBool,
    task: Option<JoinHandle<()>>,
    send_child: Option<&SendChildOwner>,
) {
    cancel.store(true, Ordering::SeqCst);
    if let Some(owner) = send_child {
        owner.kill_and_reap().await;
    }
    if let Some(task) = task {
        task.abort();
        let _ = task.await;
    }
}

/// Owns the in-flight tmux CLI child for send-keys and related verbs.
#[derive(Clone)]
pub(crate) struct SendChildOwner {
    active: Arc<std::sync::Mutex<Option<Child>>>,
}

impl SendChildOwner {
    pub(crate) fn new() -> Self {
        Self {
            active: Arc::new(std::sync::Mutex::new(None)),
        }
    }

    /// Check cancellation, spawn, and register under one lock boundary.
    pub(crate) async fn spawn_register_and_wait(
        &self,
        cancel: Option<&AtomicBool>,
        build: impl FnOnce() -> Result<tokio::process::Command, DriverError>,
    ) -> Result<(), DriverError> {
        if cancel.is_some_and(|flag| flag.load(Ordering::SeqCst)) {
            return Ok(());
        }
        {
            let mut guard = self.active.lock().unwrap();
            if cancel.is_some_and(|flag| flag.load(Ordering::SeqCst)) {
                return Ok(());
            }
            let mut cmd = build()?;
            cmd.kill_on_drop(true);
            let child = cmd
                .spawn()
                .map_err(|e| DriverError::Transport(format!("spawn: {e}")))?;
            *guard = Some(child);
        }
        wait_for_owned_send_child(self, cancel).await
    }

    pub(crate) async fn kill_and_reap(&self) {
        let child = self.active.lock().unwrap().take();
        if let Some(mut child) = child {
            let _ = child.kill().await;
            let _ = child.wait().await;
        }
    }
}

#[async_trait]
impl DriverControl for TmuxTuiControl {
    async fn transition_state(
        &mut self,
        req: TransitionRequest,
    ) -> Result<TransitionAck, DriverError> {
        if let Some(events) = self.events.as_ref() {
            let _ = events
                .send(DriverEvent::TransitionState {
                    from: req.from.clone(),
                    to: req.to.clone(),
                    reason: req.reason.clone(),
                })
                .await;
        }
        Ok(TransitionAck {
            accepted: true,
            message: None,
        })
    }

    async fn send_input(&mut self, req: UserInputRequest) -> Result<UserInputAck, DriverError> {
        if self.inert {
            return Err(DriverError::Unsupported("send_input"));
        }

        // Never paste into an active turn. A follow-up is accepted only after
        // the pane exposes its composer again; callers may retry a busy ack.
        if wait_for_input_ready(&self.session_name, self.input_ready_timeout)
            .await
            .is_err()
        {
            return Ok(UserInputAck {
                accepted: false,
                message: Some("harness busy".into()),
            });
        }

        paste_text_into_pane(
            &self.session_name,
            &req.input,
            Some(&self.send_child),
            Some(&self.startup_cancel),
        )
        .await?;
        send_keys(
            &self.session_name,
            &[String::from("Enter")],
            Some(&self.send_child),
            Some(&self.startup_cancel),
        )
        .await?;
        Ok(UserInputAck {
            accepted: true,
            message: None,
        })
    }

    async fn send_manager_wake(
        &mut self,
        _req: ManagerWakeRequest,
    ) -> Result<UserInputAck, DriverError> {
        if self.inert {
            return Err(DriverError::Unsupported("manager_wake"));
        }
        // This remains a targeting/UX proof: it refuses a busy, unknown, or
        // mismatched provider. It is not the byte-safety authority — tmux has
        // no conditional paste, so the fixed marker below is safe even when
        // the provider exits after this check.
        match tmux_provider_ready(&self.session_name).await? {
            true => {}
            false => {
                return Ok(UserInputAck {
                    accepted: false,
                    message: Some("claimed provider is busy or not at its composer".into()),
                });
            }
        }
        if let Err(error) = paste_manager_wake_marker_into_pane(
            &self.session_name,
            Some(&self.send_child),
            Some(&self.startup_cancel),
        )
        .await
        {
            return Err(normalize_manager_wake_target_loss(&self.session_name, error).await);
        }
        if let Err(error) = send_keys(
            &self.session_name,
            &[String::from("Enter")],
            Some(&self.send_child),
            Some(&self.startup_cancel),
        )
        .await
        {
            return Err(normalize_manager_wake_target_loss(&self.session_name, error).await);
        }
        Ok(UserInputAck {
            accepted: true,
            message: None,
        })
    }

    async fn release(&mut self, _reason: &str) -> Result<(), DriverError> {
        if self.released {
            return Ok(());
        }
        self.released = true;
        // A control-plane stop is not a provider terminal result. Mark the
        // lifecycle side terminal before killing the pane so teardown cannot
        // manufacture RunComplete and override the supervisor's frozen cause.
        self.terminal_emitted.store(true, Ordering::SeqCst);
        if let Some(abort) = self.lifecycle_abort.take() {
            abort.abort();
        }
        if let Some(abort) = self.pane_activity_abort.take() {
            abort.abort();
        }
        cancel_and_join_driver_task(
            &self.startup_cancel,
            self.startup_task.take(),
            Some(&self.send_child),
        )
        .await;
        // Receiver closure is the normal terminal authority. Dropping the
        // control-owned sender after producers stop lets the supervisor drain
        // every already-queued provider event and then converge.
        self.events.take();
        if !self.inert {
            kill_tmux_session(&self.session_name).await;
        }
        Ok(())
    }
}

impl Drop for TmuxTuiControl {
    fn drop(&mut self) {
        self.startup_cancel.store(true, Ordering::SeqCst);
        if let Some(abort) = self.lifecycle_abort.take() {
            abort.abort();
        }
        if let Some(abort) = self.pane_activity_abort.take() {
            abort.abort();
        }
        abort_driver_task(self.startup_task.take());
        if !self.released && self.kill_on_drop && !self.inert {
            kill_tmux_session_sync(&self.session_name);
        }
    }
}

// orgasmic:TASK-0RCRY
/// Names the tmux server every call site in this process talks to, as a `-L`
/// socket label.
///
/// Unset in production, deliberately: the daemon must reach the same server an
/// operator's own `tmux attach` reaches, or an attached pane would be
/// invisible. A test binary sets it (see [`own_tmux_server_for_tests`]) so its
/// sessions live on a server the run created and nothing else can reach.
pub const TMUX_SOCKET_ENV: &str = "ORGASMIC_TMUX_SOCKET";

/// Where [`own_tmux_server_for_tests`] records this process's own socket.
///
/// In-process rather than environment-only on purpose. Tests in this workspace
/// do mutate process-global environment (see `.orgasmic/gotchas.org`, "Tests
/// that set PATH break every other test in the binary"), and a run that lost
/// `ORGASMIC_TMUX_SOCKET` mid-flight would silently fall back to the shared
/// server — the exact failure this exists to prevent, in its hardest-to-see
/// form. The environment variable is still set alongside, so a child process
/// inherits the selection and so an operator can pin one by hand.
static OWNED_TMUX_SOCKET: std::sync::OnceLock<String> = std::sync::OnceLock::new();

/// The `-L` label for this process, or `None` for "whichever server the
/// environment selects".
///
/// Resolved on every call rather than cached: a test binary pins the socket
/// from its first tmux-gated test, and a cached `None` from some earlier
/// unrelated probe would strand the whole binary on the shared server.
fn tmux_socket() -> Option<String> {
    if let Some(socket) = OWNED_TMUX_SOCKET.get() {
        return Some(socket.clone());
    }
    std::env::var(TMUX_SOCKET_ENV)
        .ok()
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
}

// orgasmic:TASK-0RCRY
/// The tmux server this process talks to, and how that server was selected —
/// for failure messages.
///
/// `tmux new-session failed: server exited unexpectedly` is true and useless
/// when tmux is installed and working; the server identity is the part that
/// distinguishes "tmux is broken" from "something else is using this server".
#[must_use]
pub fn tmux_server_selection() -> String {
    match tmux_socket() {
        Some(socket) => format!("server '-L {socket}' (selected by {TMUX_SOCKET_ENV})"),
        None => {
            let tmpdir = std::env::var("TMUX_TMPDIR").unwrap_or_default();
            let via = if tmpdir.is_empty() {
                "no -L/-S and no TMUX_TMPDIR".to_string()
            } else {
                format!("no -L/-S, TMUX_TMPDIR={tmpdir}")
            };
            format!("the default shared server ({via})")
        }
    }
}

// orgasmic:TASK-0RCRY
/// The server-selection argv every tmux invocation carries — the whole of what
/// this task adds to a tmux command line.
///
/// Split from [`tmux_socket`] so the property "a pinned socket really reaches
/// the command line" is provable without mutating process-global environment
/// while other tests are spawning real tmux clients.
fn tmux_socket_args_for(socket: Option<&str>) -> Vec<String> {
    match socket {
        Some(socket) => vec!["-L".to_string(), socket.to_string()],
        None => Vec::new(),
    }
}

fn tmux_socket_args() -> Vec<String> {
    tmux_socket_args_for(tmux_socket().as_deref())
}

// orgasmic:TASK-0RCRY
/// Every synchronous tmux invocation in this crate is built here so the `-L`
/// selection cannot be forgotten at one call site.
///
/// Public because the daemon's test binaries create their fixture sessions
/// directly and must land on the same server as the production code they then
/// exercise.
#[must_use]
pub fn tmux_command() -> StdCommand {
    let mut command = StdCommand::new("tmux");
    command.args(tmux_socket_args());
    command
}

// orgasmic:TASK-0RCRY
/// [`tmux_command`] for the async call sites.
fn tmux_async_command() -> tokio::process::Command {
    let mut command = tokio::process::Command::new("tmux");
    command.args(tmux_socket_args());
    command
}

// orgasmic:TASK-0RCRY
/// Pin this process to a tmux server it owns, and hold that server open for as
/// long as the process lives. Returns the `-L` socket label.
///
/// Idempotent and safe to call from every tmux-gated test: the first caller
/// wins and every later caller gets the same label.
///
/// Two things are being bought here, and `-L` alone buys only the first:
///
/// 1. *Isolation.* Without `-L`, a test run reaches whichever server the
///    environment selects, which may be the operator's own server.
/// 2. *Stability.* A tmux server exits when its last session goes away, and
///    its socket outlives that decision by a moment. The live-mux tests are
///    serialized by the TASK-Z3093 flock, so the shared server repeatedly
///    drains to zero sessions between tests — and the next test's client,
///    arriving in that window, is told `server exited unexpectedly`. The
///    keepalive session below means the session count never reaches zero, so
///    that window never opens.
///
/// Reaping: nothing here needs an atexit hook. Each test still reaps its own
/// session through its existing drop-guard, and the keepalive pane runs a shell
/// loop that exits once this test process is gone — which removes the server's
/// last session, at which point tmux tears the server down and unlinks the
/// socket by itself. The loop is additionally capped so a recycled pid cannot
/// keep an orphan server alive indefinitely.
#[doc(hidden)]
pub fn own_tmux_server_for_tests() -> &'static str {
    OWNED_TMUX_SOCKET.get_or_init(|| {
        let pid = std::process::id();
        let socket = format!("orgasmic-test-{pid}");
        // The in-process record above is what every call site reads; this is
        // for child processes and for an operator pinning one by hand.
        std::env::set_var(TMUX_SOCKET_ENV, &socket);
        // Deliberately not `kill-server` first: killing a server is exactly
        // what produces `server exited unexpectedly` for a concurrent client,
        // and a per-pid socket cannot collide with a live one anyway.
        let keepalive = format!(
            "i=0; while [ $i -lt 21600 ] && kill -0 {pid} 2>/dev/null; do sleep 1; i=$((i+1)); done"
        );
        // Built from `socket` directly, NOT through `tmux_command()`: this runs
        // inside `get_or_init`, so the record every other call site reads is
        // not published yet and the keepalive would go to the shared server —
        // the one session that must never land there.
        let _ = StdCommand::new("tmux")
            .args(tmux_socket_args_for(Some(&socket)))
            .args([
                "new-session",
                "-d",
                "-s",
                "orgasmic-test-keepalive",
                "--",
                "/bin/sh",
                "-c",
                &keepalive,
            ])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();
        socket
    })
}

// orgasmic:TASK-FJCE9
/// Is tmux available at the resolved PATH location?
///
/// **A deliberate, documented copy.** The canonical rule is TASK-K4G1D's
/// `tmux_mode_availability_for` in `orgasmic-daemon`'s `api::tests`, and
/// TASK-VJ633 owns collapsing the two. It is copied rather than called for the
/// reason `test_tooling` already documents: an integration-test
/// crate cannot import a `#[cfg(test)]` library module, and
/// `crates/orgasmic-daemon/tests/recovery_fault_restart.rs` is exactly such a
/// crate — it must gate its live-tmux test on the same rule the daemon's own
/// tests use.
///
/// The copy is not free to drift: `daemon_and_driver_tmux_strictness_agree` in
/// `api::tests` asserts the two answer identically over the whole matrix, next
/// to the canonical rule so an edit there fails there.
///
#[doc(hidden)]
pub fn tmux_mode_availability_for(resolved: Option<&Path>) -> Result<(), String> {
    let Some(_) = resolved else {
        return Err("no tmux on PATH".to_string());
    };
    Ok(())
}

// orgasmic:TASK-FJCE9
/// [`tmux_mode_availability_for`] applied to the PATH lookup the drivers do —
/// the strict "is real tmux usable here?" a test binary gates on.
///
/// Deliberately based on the same PATH lookup used by the driver.
#[doc(hidden)]
#[must_use]
pub fn real_tmux_on_path() -> bool {
    let resolved = std::env::var_os("PATH").and_then(|paths| {
        std::env::split_paths(&paths)
            .map(|dir| dir.join("tmux"))
            .find(|candidate| candidate.is_file())
    });
    tmux_mode_availability_for(resolved.as_deref()).is_ok()
}

fn tmux_available() -> bool {
    // `-V` never contacts a server, so it needs no socket selection.
    StdCommand::new("tmux")
        .arg("-V")
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

pub fn tmux_session_name(identity: &RuntimeIdentity) -> String {
    format!("orgasmic-{}-{}", identity.run_id, identity.runtime_id)
}

/// What a synchronous `tmux has-session` probe actually established.
///
/// A non-zero tmux client invocation is not automatically evidence that the
/// session disappeared: a missing server, broken binary, or client error can
/// all use a non-zero exit. Recovery must only destroy/stale a durable claim
/// from the one explicit no-such-session answer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TmuxSessionObservation {
    Present,
    Absent,
    Unobserved,
}

/// Synchronous tmux session probe for crash-reconciliation paths that cannot
/// await driver I/O.
pub fn observe_tmux_session(session: &str) -> TmuxSessionObservation {
    match tmux_command()
        .args(["has-session", "-t", session])
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::piped())
        .output()
    {
        Ok(output) => classify_tmux_session_observation(
            output.status.success(),
            output.status.code(),
            &String::from_utf8_lossy(&output.stderr),
        ),
        Err(_) => TmuxSessionObservation::Unobserved,
    }
}

fn classify_tmux_session_observation(
    success: bool,
    code: Option<i32>,
    stderr: &str,
) -> TmuxSessionObservation {
    if success {
        return TmuxSessionObservation::Present;
    }
    // `has-session` emits this client error for the ordinary, decidable
    // no-such-session case. A bare exit 1 is intentionally insufficient: the
    // daemon must not turn an unobserved mux into a false-dead recovery.
    if code == Some(1) && stderr.to_ascii_lowercase().contains("can't find session") {
        TmuxSessionObservation::Absent
    } else {
        TmuxSessionObservation::Unobserved
    }
}

/// Boolean compatibility helper for consumers that only need positive
/// liveness. Recovery reconciliation uses [`observe_tmux_session`] directly
/// because absence and probe failure have different safety meanings there.
pub fn tmux_session_exists(session: &str) -> bool {
    matches!(
        observe_tmux_session(session),
        TmuxSessionObservation::Present
    )
}

async fn has_tmux_session(session: &str) -> Result<bool, DriverError> {
    let mut command = tmux_async_command();
    command.kill_on_drop(true);
    let status = command
        .args(["has-session", "-t", session])
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .await
        .map_err(|e| DriverError::Transport(format!("tmux has-session: {e}")))?;
    Ok(status.success())
}

#[derive(Debug, Clone)]
struct TmuxSpawnPlan {
    command: String,
    args: Vec<String>,
    cwd: PathBuf,
    /// Prompt to paste after spawn. `None` when the prompt was delivered via
    /// initial-prompt argv (claude/codex/cursor-agent) or when absent.
    paste_prompt: Option<String>,
    /// Harness-aware native runtime identity recorded into the session JSONL.
    /// `None` when the harness has no known native session semantics.
    native_runtime: Option<NativeRuntimeMeta>,
    /// This run's id, exported as `ORGASMIC_RUN_ID` into the spawned pane's
    /// environment so a manager session recognises "I am already supervised"
    /// (`orgasmic manager register`, dec_3Y2E1).
    run_id: String,
    runtime_id: String,
    boot_id: String,
    manager_terminal_capability: Option<String>,
    /// Harness-specific environment exported into the spawned pane. Carried on
    /// the plan (not applied at the tmux call site) so the stamp a transcript
    /// finder depends on is provable without spawning tmux.
    // orgasmic:TASK-GT91X
    harness_env: Vec<(String, String)>,
    native_resume_mode: bool,
    trusted_provider_identity: Option<String>,
    pinned_executable: Option<PinnedExecutableIdentity>,
    provider_home: Option<PathBuf>,
}

fn is_claude_harness_command(
    harness: &str,
    command: &str,
    trusted_provider_identity: Option<&str>,
) -> bool {
    trusted_provider_identity == Some("claude")
        || (harness == "claude"
            && (command == "claude"
                || Path::new(command)
                    .file_name()
                    .and_then(|name| name.to_str())
                    .is_some_and(|name| name == "claude")))
}

fn build_spawn_plan(cfg: &TmuxTuiConfig, ctx: &DriverContext, harness: &str) -> TmuxSpawnPlan {
    let cwd = cfg
        .cwd
        .clone()
        .or_else(|| ctx.worktree.clone())
        .unwrap_or_else(|| std::env::current_dir().unwrap_or_else(|_| PathBuf::from("/tmp")));
    // Trim only to detect emptiness; argv/paste delivery must preserve bytes.
    let prompt_text = if cfg.native_resume_mode {
        None
    } else {
        cfg.prompt_bundle_text
            .clone()
            .filter(|bundle| !bundle.trim().is_empty())
    };

    let (command, mut args) = if should_use_default_command(cfg, harness) {
        default_command_for_harness(harness, cfg)
    } else {
        (
            cfg.command.clone().unwrap_or_else(|| "sh".to_string()),
            cfg.args.clone(),
        )
    };

    // Worker/launch-supplied harness argv rides along whenever we are running
    // a real harness CLI (not the inert dispatch placeholder). It lands before
    // the guarded pushes below so user-specified flags take precedence.
    if !cfg.harness_args.is_empty() && !is_dispatch_placeholder(Some(command.as_str()), &args) {
        args.extend(cfg.harness_args.iter().cloned());
    }

    let is_claude =
        is_claude_harness_command(harness, &command, cfg.trusted_provider_identity.as_deref());
    if is_claude {
        if !args
            .iter()
            .any(|arg| arg == "--dangerously-skip-permissions")
        {
            args.push("--dangerously-skip-permissions".to_string());
        }
        if !cfg.native_resume_mode {
            if let Some(model) = cfg.model.as_deref() {
                if !args.iter().any(|arg| arg == "--model") {
                    args.push("--model".to_string());
                    args.push(model.to_string());
                }
            }
            if let Some(effort) = cfg.effort.as_ref().or(cfg.reasoning_effort.as_ref()) {
                if !args.iter().any(|arg| arg == "--effort") {
                    args.push("--effort".to_string());
                    args.push(effort.clone());
                }
            }
            // Deterministic native Claude session identity: pin --session-id to the
            // run's runtime_id (a UUID) so recovery can resume/fork it exactly.
            let session_id = claude_session_id(&ctx.identity.runtime_id);
            if !args.iter().any(|arg| arg == "--session-id") {
                args.push("--session-id".to_string());
                args.push(session_id);
            }
        }
    }
    if matches!(harness, "codex" | "cursor-agent" | "hermes") {
        if let Some(model) = cfg.model.as_ref() {
            if !args.iter().any(|arg| arg == "--model" || arg == "-m") {
                args.push("--model".to_string());
                args.push(model.clone());
            }
        }
    }

    // orgasmic:TASK-AFE5Q — argv delivery when the resolved binary is a
    // supported TUI harness; paste remains for hermes/custom and for
    // non-harness commands (test fixtures, explicit wrappers).
    let paste_prompt = match prompt_text {
        Some(prompt) if argv_prompt_delivery_applies(harness, &command) => {
            push_initial_prompt_argv(&mut args, &prompt);
            None
        }
        other => other,
    };

    let native_runtime = if is_claude {
        if cfg.native_resume_mode {
            let resumed_session_id = args
                .iter()
                .enumerate()
                .find_map(|(idx, arg)| {
                    if arg == "--resume" {
                        args.get(idx + 1).cloned()
                    } else {
                        None
                    }
                })
                .unwrap_or_else(|| claude_session_id(&ctx.identity.runtime_id));
            Some(claude_native_runtime_pending_fork(
                &resumed_session_id,
                &cwd,
                &command,
                &args,
            ))
        } else {
            let session_id = claude_session_id(&ctx.identity.runtime_id);
            Some(claude_native_runtime_with_home(
                &session_id,
                &cwd,
                &command,
                &args,
                cfg.provider_home.as_deref(),
            ))
        }
    } else {
        // Other harnesses store only real launch metadata until their native
        // session semantics are known (dec_052).
        let mut launch_argv = vec![command.clone()];
        launch_argv.extend(args.iter().cloned());
        Some(NativeRuntimeMeta {
            provider: harness.to_string(),
            session_id: None,
            session_path: None,
            launch_argv,
            resume_argv: Vec::new(),
            credential_mode: None,
        })
    };

    TmuxSpawnPlan {
        command,
        args,
        cwd,
        paste_prompt,
        native_runtime,
        run_id: ctx.identity.run_id.clone(),
        runtime_id: ctx.identity.runtime_id.clone(),
        boot_id: ctx.identity.boot_id.clone(),
        manager_terminal_capability: cfg.manager_terminal_capability.clone(),
        harness_env: harness_launch_env(harness),
        native_resume_mode: cfg.native_resume_mode,
        trusted_provider_identity: cfg.trusted_provider_identity.clone(),
        pinned_executable: cfg.pinned_executable.clone(),
        provider_home: cfg.provider_home.clone(),
    }
}

/// Environment a mux launch must export for `harness` so that run's transcript
/// stays reachable afterwards.
///
/// codex derives `session_meta.originator` from its frontend, so a TUI launch
/// records `codex-tui` unless [`crate::CODEX_ORIGINATOR_ENV`] overrides it. The
/// finder's cwd scan is the *only* correlator available for a codex run — codex
/// emits no `NativeRuntime`, so there is no session id to fall back on
/// (TASK-F9VEZ). Without this stamp the scan matches nothing and every codex
/// transcript is unreachable (TASK-GT91X).
// orgasmic:TASK-GT91X
pub(crate) fn harness_launch_env(harness: &str) -> Vec<(String, String)> {
    match harness {
        "codex" => vec![(
            crate::CODEX_ORIGINATOR_ENV.to_string(),
            crate::CODEX_ORIGINATOR.to_string(),
        )],
        _ => Vec::new(),
    }
}

/// Harnesses that accept the compiled initial prompt as one trailing argv
/// element (dec_WDR5K item 8 / TASK-AFE5Q). Hermes has no trustworthy TUI
/// argv form — paste remains the fallback.
// orgasmic:TASK-AFE5Q,dec_WDR5K
pub(crate) fn harness_supports_initial_prompt_argv(harness: &str) -> bool {
    matches!(harness, "claude" | "codex" | "cursor-agent")
}

/// True when both the harness id and the resolved binary basename support
/// initial-prompt argv delivery.
// orgasmic:TASK-AFE5Q
pub(crate) fn argv_prompt_delivery_applies(harness: &str, command: &str) -> bool {
    if !harness_supports_initial_prompt_argv(harness) {
        return false;
    }
    let base = std::path::Path::new(command)
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or(command);
    base == harness
}

/// Append the compiled prompt as exactly one argv element after `--`, so
/// quotes/newlines/metacharacters and leading dashes never reach a shell and
/// are never option-parsed.
// orgasmic:TASK-AFE5Q
pub(crate) fn push_initial_prompt_argv(args: &mut Vec<String>, prompt: &str) {
    args.push("--".to_string());
    args.push(prompt.to_string());
}

// orgasmic:TASK-RKTH1
/// tmux packs an entire command line into ONE imsg to its server, and imsg's
/// payload ceiling is `MAX_IMSGSIZE` (16 KiB) less the header.
///
/// The worker's compiled prompt rides in that command line as an argv element
/// ([`push_initial_prompt_argv`]), so a long brief pushes the packed argv past
/// the ceiling and tmux answers `command too long` — which the daemon can only
/// surface as an opaque `failed to acquire worker run`. Measured 2026-07-30
/// against tmux 3.6a: a 16 000-byte argv spawns, 20 000 does not.
///
/// The constant is tmux's own and cannot be raised from outside it, so the fix
/// is to stop sending the prompt through the command line at all — see
/// [`launcher_script_body`].
const TMUX_PACKED_ARGV_CEILING: usize = 16 * 1024 - 16;

/// Where the direct argv route stops being provably safe.
///
/// Derived from [`TMUX_PACKED_ARGV_CEILING`] rather than written out, so the
/// margin cannot drift away from the limit it exists to clear. A quarter of the
/// ceiling is margin: this crate can only count the argv it builds, while tmux
/// packs its own framing around each element, and a launch that guesses the
/// remaining headroom wrong fails at spawn rather than falling back. Spending
/// the margin early costs nothing — the fallback is a file write.
const TMUX_PACKED_ARGV_BUDGET: usize = TMUX_PACKED_ARGV_CEILING / 4 * 3;

// orgasmic:TASK-RKTH1
// The margin has to be a margin. A budget at or above the ceiling would admit a
// launch tmux then refuses — the exact failure this route exists to remove — and
// the 16 000-byte bound is the largest argv measured to spawn on tmux 3.6a
// (2026-07-30; 20 000 did not). Asserted at compile time rather than in a test:
// the relationship is between two constants, so a build is the right place to
// catch it and no test run is needed to keep it true.
const _: () = assert!(TMUX_PACKED_ARGV_BUDGET < TMUX_PACKED_ARGV_CEILING);
const _: () = assert!(TMUX_PACKED_ARGV_BUDGET < 16_000);

// orgasmic:TASK-RKTH1
/// Bytes tmux packs for `args` — each element is copied with its NUL
/// terminator, which is the whole of the encoding this crate can account for.
fn packed_argv_len<'a>(args: impl IntoIterator<Item = &'a str>) -> usize {
    args.into_iter().map(|arg| arg.len() + 1).sum()
}

// orgasmic:TASK-RKTH1
/// `raw` as a single POSIX-shell word, byte for byte.
///
/// Single quotes are the only shell quoting that interprets nothing at all, so
/// prompt bytes — backslashes, `$`, backticks, newlines, trailing whitespace —
/// survive verbatim, which is the same guarantee argv delivery gives and the
/// reason `build_spawn_plan` trims only to test for emptiness. The one byte a
/// single-quoted string cannot carry is `'` itself: close, emit an escaped
/// quote, reopen.
fn sh_single_quote(raw: &str) -> String {
    let mut quoted = String::with_capacity(raw.len() + 2);
    quoted.push('\'');
    for ch in raw.chars() {
        if ch == '\'' {
            quoted.push_str("'\\''");
        } else {
            quoted.push(ch);
        }
    }
    quoted.push('\'');
    quoted
}

// orgasmic:TASK-RKTH1
/// The launcher a pane runs when the command line will not fit in one imsg.
///
/// tmux only ever sees `/bin/sh <path>`, so the packed argv is bounded by the
/// path length no matter how large the prompt is; the prompt itself reaches the
/// harness through the script, still as one argv element and still byte-exact.
///
/// `rm` precedes `exec` rather than following it because `exec` never returns.
/// Unlinking is safe there: POSIX keeps an unlinked file's inode alive for
/// every open descriptor, including the one `sh` is still reading this script
/// through. The artefact carrying the prompt is therefore gone from the
/// filesystem before the harness starts, and readable to the shell that needs
/// it until it is done.
fn launcher_script_body(command: &str, args: &[String]) -> String {
    let mut script = String::from("#!/bin/sh\nrm -f -- \"$0\"\nexec ");
    script.push_str(&sh_single_quote(command));
    for arg in args {
        script.push(' ');
        script.push_str(&sh_single_quote(arg));
    }
    script.push('\n');
    script
}

// orgasmic:TASK-RKTH1
/// Write [`launcher_script_body`] somewhere only this user can read it.
///
/// `create_new` so the launch can never land on an inode someone else prepared,
/// and `0600` because the file holds the whole worker prompt. No execute bit is
/// needed or granted: the pane runs `/bin/sh <path>`, which reads the script
/// rather than executing it.
fn write_launcher_script(
    session: &str,
    command: &str,
    args: &[String],
) -> Result<PathBuf, DriverError> {
    let path = std::env::temp_dir().join(format!(
        "orgasmic-tmux-launch-{}-{}.sh",
        sanitize_tmux_name(session),
        uuid::Uuid::new_v4()
    ));
    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = options
        .open(&path)
        .map_err(|err| DriverError::Transport(format!("create tmux launcher: {err}")))?;
    file.write_all(launcher_script_body(command, args).as_bytes())
        .map_err(|err| DriverError::Transport(format!("write tmux launcher: {err}")))?;
    Ok(path)
}

/// Deterministic Claude native session id pinned to the run's runtime UUID.
/// The runtime_id is already a UUID, so it satisfies `claude --session-id`.
pub(crate) fn claude_session_id(runtime_id: &str) -> String {
    runtime_id.to_string()
}

fn claude_projects_dir_with_home(
    cwd: &std::path::Path,
    provider_home: Option<&std::path::Path>,
) -> Option<PathBuf> {
    let home = provider_home
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(PathBuf::from))?;
    let encoded: String = cwd
        .to_string_lossy()
        .chars()
        .map(|c| if c == '/' || c == '.' { '-' } else { c })
        .collect();
    Some(home.join(".claude").join("projects").join(encoded))
}

#[cfg(test)]
fn claude_projects_dir(cwd: &std::path::Path) -> Option<PathBuf> {
    claude_projects_dir_with_home(cwd, None)
}

/// Result of proving the Claude session created by `--fork-session`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum ForkDiscoveryResult {
    Unique(String),
    Ambiguous,
    NotFound,
}

const FORK_DISCOVERY_INITIAL_WAIT: Duration = Duration::from_millis(750);
const FORK_DISCOVERY_POLL: Duration = Duration::from_millis(250);
const FORK_DISCOVERY_MAX_AFTER_LAUNCH: Duration = Duration::from_secs(30);

#[cfg(test)]
static CLAUDE_PRE_RELEASE_TEST_HOOK: std::sync::Mutex<Option<Box<dyn FnOnce() + Send>>> =
    std::sync::Mutex::new(None);
#[cfg(test)]
type ForkCandidatePostReadHook = Box<dyn FnOnce(&str) + Send>;
#[cfg(test)]
static FORK_CANDIDATE_POST_READ_TEST_HOOK: std::sync::Mutex<Option<ForkCandidatePostReadHook>> =
    std::sync::Mutex::new(None);

fn system_time_secs(time: std::time::SystemTime) -> Option<u64> {
    time.duration_since(std::time::UNIX_EPOCH)
        .ok()
        .map(|duration| duration.as_secs())
}

/// Filesystem mtimes are often whole-second; compare launch lower bounds at
/// second granularity so confined candidates are not dropped on macOS.
fn file_modified_not_before_launch(
    modified: std::time::SystemTime,
    since: std::time::SystemTime,
) -> bool {
    match (system_time_secs(modified), system_time_secs(since)) {
        (Some(modified_secs), Some(since_secs)) => modified_secs >= since_secs,
        _ => modified >= since,
    }
}

async fn wait_for_claude_fork_session_id(
    resumed_session_id: &str,
    since: std::time::SystemTime,
    excluded: &std::collections::BTreeSet<String>,
    directory: &ClaudeProjectsDirectory,
) -> ForkDiscoveryResult {
    tokio::time::sleep(FORK_DISCOVERY_INITIAL_WAIT).await;
    let deadline = since + FORK_DISCOVERY_MAX_AFTER_LAUNCH;
    loop {
        match discover_claude_fork_session_id_in_directory(
            resumed_session_id,
            since,
            excluded,
            directory,
        ) {
            ForkDiscoveryResult::Unique(id) => {
                // Give a concurrent launch one polling interval to surface;
                // only a stable unique observation is authoritative.
                tokio::time::sleep(FORK_DISCOVERY_POLL).await;
                return match discover_claude_fork_session_id_in_directory(
                    resumed_session_id,
                    since,
                    excluded,
                    directory,
                ) {
                    ForkDiscoveryResult::Unique(confirmed) if confirmed == id => {
                        ForkDiscoveryResult::Unique(id)
                    }
                    ForkDiscoveryResult::Unique(_) | ForkDiscoveryResult::Ambiguous => {
                        ForkDiscoveryResult::Ambiguous
                    }
                    ForkDiscoveryResult::NotFound => ForkDiscoveryResult::NotFound,
                };
            }
            ForkDiscoveryResult::Ambiguous => return ForkDiscoveryResult::Ambiguous,
            ForkDiscoveryResult::NotFound if std::time::SystemTime::now() >= deadline => {
                return ForkDiscoveryResult::NotFound;
            }
            ForkDiscoveryResult::NotFound => {
                tokio::time::sleep(FORK_DISCOVERY_POLL).await;
            }
        }
    }
}

fn fork_candidate_has_provider_proof(
    file: &File,
    session_id: &str,
    cwd: &std::path::Path,
    resumed_session_id: &str,
) -> bool {
    let Ok(expected_cwd) = cwd.canonicalize() else {
        return false;
    };
    let Ok(mut file) = file.try_clone() else {
        return false;
    };
    if file.seek(SeekFrom::Start(0)).is_err() {
        return false;
    }
    let mut raw = String::new();
    if file.read_to_string(&mut raw).is_err() {
        return false;
    }
    raw.lines()
        .filter(|line| !line.trim().is_empty())
        .any(|line| {
            let Ok(value) = serde_json::from_str::<serde_json::Value>(line) else {
                return false;
            };
            let candidate_cwd = value
                .get("cwd")
                .and_then(serde_json::Value::as_str)
                .map(PathBuf::from)
                .and_then(|path| path.canonicalize().ok());
            value.get("sessionId").and_then(serde_json::Value::as_str) == Some(session_id)
                && candidate_cwd.as_deref() == Some(expected_cwd.as_path())
                && value
                    .get("forkedFrom")
                    .and_then(|forked| forked.get("sessionId"))
                    .and_then(serde_json::Value::as_str)
                    == Some(resumed_session_id)
        })
}

#[derive(Clone)]
struct ClaudeProjectsDirectory {
    file: Arc<File>,
    cwd: PathBuf,
}

impl ClaudeProjectsDirectory {
    fn open(cwd: &Path, provider_home: Option<&Path>) -> Result<Self, DriverError> {
        let path = claude_projects_dir_with_home(cwd, provider_home).ok_or_else(|| {
            DriverError::Transport("Claude projects directory is unavailable".into())
        })?;
        let cwd = cwd
            .canonicalize()
            .map_err(|_| DriverError::Transport("Claude recovery cwd is unavailable".into()))?;
        #[cfg(unix)]
        use std::os::unix::fs::OpenOptionsExt;
        let mut options = OpenOptions::new();
        options.read(true);
        #[cfg(unix)]
        options.custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC);
        let file = options.open(&path).map_err(|err| {
            DriverError::Transport(format!("open Claude projects directory: {err}"))
        })?;
        if !file.metadata().map(|meta| meta.is_dir()).unwrap_or(false) {
            return Err(DriverError::Transport(
                "Claude projects path is not a directory".into(),
            ));
        }
        Ok(Self {
            file: Arc::new(file),
            cwd,
        })
    }

    #[cfg(unix)]
    fn names(&self) -> Result<std::collections::BTreeSet<String>, DriverError> {
        use std::ffi::CStr;
        use std::os::fd::AsRawFd;
        // `dup` would share the directory stream offset with `self.file`.
        // Discovery enumerates more than once to prove a stable unique fork,
        // so each pass needs a fresh open file description rooted at the
        // retained directory authority.
        let dot = c".";
        let directory_fd = unsafe {
            libc::openat(
                self.file.as_raw_fd(),
                dot.as_ptr(),
                libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC,
            )
        };
        if directory_fd < 0 {
            return Err(DriverError::Transport(format!(
                "open retained Claude projects directory: {}",
                std::io::Error::last_os_error()
            )));
        }
        let dir = unsafe { libc::fdopendir(directory_fd) };
        if dir.is_null() {
            let error = std::io::Error::last_os_error();
            unsafe { libc::close(directory_fd) };
            return Err(DriverError::Transport(format!(
                "enumerate retained Claude projects directory: {error}"
            )));
        }
        let mut names = std::collections::BTreeSet::new();
        loop {
            let entry = unsafe { libc::readdir(dir) };
            if entry.is_null() {
                break;
            }
            let name = unsafe { CStr::from_ptr((*entry).d_name.as_ptr()) };
            if let Ok(name) = name.to_str() {
                if let Some(stem) = name.strip_suffix(".jsonl") {
                    if validate_fork_session_stem(stem) {
                        names.insert(stem.to_string());
                    }
                }
            }
        }
        unsafe { libc::closedir(dir) };
        Ok(names)
    }

    #[cfg(not(unix))]
    fn names(&self) -> Result<std::collections::BTreeSet<String>, DriverError> {
        Err(DriverError::Unsupported(
            "retained Claude projects directory enumeration",
        ))
    }

    #[cfg(unix)]
    fn open_candidate(&self, stem: &str) -> Option<(File, std::fs::Metadata)> {
        use std::os::fd::{AsRawFd, FromRawFd};
        let name = std::ffi::CString::new(format!("{stem}.jsonl")).ok()?;
        let fd = unsafe {
            libc::openat(
                self.file.as_raw_fd(),
                name.as_ptr(),
                libc::O_RDONLY | libc::O_NOFOLLOW | libc::O_CLOEXEC,
            )
        };
        if fd < 0 {
            return None;
        }
        let file = unsafe { File::from_raw_fd(fd) };
        let metadata = file.metadata().ok()?;
        metadata.is_file().then_some((file, metadata))
    }

    #[cfg(not(unix))]
    fn open_candidate(&self, _stem: &str) -> Option<(File, std::fs::Metadata)> {
        None
    }

    fn current_identity_matches(&self, stem: &str, expected: &std::fs::Metadata) -> bool {
        let Some((_, current)) = self.open_candidate(stem) else {
            return false;
        };
        #[cfg(unix)]
        {
            use std::os::unix::fs::MetadataExt;
            current.dev() == expected.dev() && current.ino() == expected.ino()
        }
        #[cfg(not(unix))]
        {
            current.len() == expected.len() && current.modified().ok() == expected.modified().ok()
        }
    }
}

#[cfg(test)]
fn claude_fork_candidate_names(cwd: &std::path::Path) -> std::collections::BTreeSet<String> {
    claude_fork_candidate_names_with_home(cwd, None)
}

#[cfg(test)]
fn claude_fork_candidate_names_with_home(
    cwd: &std::path::Path,
    provider_home: Option<&std::path::Path>,
) -> std::collections::BTreeSet<String> {
    ClaudeProjectsDirectory::open(cwd, provider_home)
        .and_then(|directory| directory.names())
        .unwrap_or_default()
}

fn validate_fork_session_stem(stem: &str) -> bool {
    !stem.is_empty()
        && stem != "."
        && stem != ".."
        && !stem.contains('/')
        && !stem.contains('\\')
        && std::path::Path::new(stem)
            .components()
            .all(|c| matches!(c, std::path::Component::Normal(_)))
}

/// Discover the Claude session id created by `--fork-session` after resume.
///
/// Proof, not guessing: exactly one candidate within launch bounds,
/// path-contained under the cwd-derived Claude projects dir, and distinct
/// from the resumed session id.
#[cfg(test)]
pub(crate) fn discover_claude_fork_session_id(
    resumed_session_id: &str,
    cwd: &std::path::Path,
    since: std::time::SystemTime,
) -> ForkDiscoveryResult {
    discover_claude_fork_session_id_excluding_with_home(
        resumed_session_id,
        cwd,
        since,
        &Default::default(),
        None,
    )
}

#[cfg(test)]
fn discover_claude_fork_session_id_excluding(
    resumed_session_id: &str,
    cwd: &std::path::Path,
    since: std::time::SystemTime,
    excluded: &std::collections::BTreeSet<String>,
) -> ForkDiscoveryResult {
    discover_claude_fork_session_id_excluding_with_home(
        resumed_session_id,
        cwd,
        since,
        excluded,
        None,
    )
}

#[cfg(test)]
fn discover_claude_fork_session_id_excluding_with_home(
    resumed_session_id: &str,
    cwd: &std::path::Path,
    since: std::time::SystemTime,
    excluded: &std::collections::BTreeSet<String>,
    provider_home: Option<&std::path::Path>,
) -> ForkDiscoveryResult {
    let Ok(directory) = ClaudeProjectsDirectory::open(cwd, provider_home) else {
        return ForkDiscoveryResult::NotFound;
    };
    discover_claude_fork_session_id_in_directory(resumed_session_id, since, excluded, &directory)
}

fn discover_claude_fork_session_id_in_directory(
    resumed_session_id: &str,
    since: std::time::SystemTime,
    excluded: &std::collections::BTreeSet<String>,
    directory: &ClaudeProjectsDirectory,
) -> ForkDiscoveryResult {
    let launch_upper = since + FORK_DISCOVERY_MAX_AFTER_LAUNCH;
    let mut candidates = Vec::new();
    let Ok(names) = directory.names() else {
        return ForkDiscoveryResult::NotFound;
    };
    for stem in names {
        if stem == resumed_session_id
            || excluded.contains(&stem)
            || !validate_fork_session_stem(&stem)
        {
            continue;
        }
        let Some((file, metadata)) = directory.open_candidate(&stem) else {
            continue;
        };
        let Ok(modified) = metadata.modified() else {
            continue;
        };
        if !file_modified_not_before_launch(modified, since) || modified > launch_upper {
            continue;
        }
        if !fork_candidate_has_provider_proof(&file, &stem, &directory.cwd, resumed_session_id) {
            continue;
        }
        #[cfg(test)]
        if let Some(hook) = FORK_CANDIDATE_POST_READ_TEST_HOOK
            .lock()
            .expect("fork post-read hook lock")
            .take()
        {
            hook(&stem);
        }
        if !directory.current_identity_matches(&stem, &metadata) {
            continue;
        }
        candidates.push(stem);
    }
    match candidates.len() {
        0 => ForkDiscoveryResult::NotFound,
        1 => ForkDiscoveryResult::Unique(candidates.remove(0)),
        _ => ForkDiscoveryResult::Ambiguous,
    }
}

pub(crate) fn deterministic_inert_fork_session_id(runtime_id: &str) -> String {
    format!("fork-{runtime_id}")
}

fn extract_resume_session_id(args: &[String]) -> Option<String> {
    args.iter().enumerate().find_map(|(idx, arg)| {
        if arg == "--resume" {
            args.get(idx + 1).cloned()
        } else {
            None
        }
    })
}

fn claude_native_runtime_pending_fork(
    resumed_session_id: &str,
    _cwd: &std::path::Path,
    command: &str,
    args: &[String],
) -> NativeRuntimeMeta {
    let mut launch_argv = vec![command.to_string()];
    launch_argv.extend(args.iter().cloned());
    let resume_argv = vec![
        command.to_string(),
        "--resume".to_string(),
        resumed_session_id.to_string(),
        "--fork-session".to_string(),
        "--dangerously-skip-permissions".to_string(),
    ];
    NativeRuntimeMeta {
        provider: "claude".to_string(),
        session_id: None,
        session_path: None,
        launch_argv,
        resume_argv,
        credential_mode: None,
    }
}

/// Claude stores conversation JSONL under
/// `~/.claude/projects/<encoded-cwd>/<session-id>.jsonl`, where the encoding
/// replaces path separators and dots with `-`.
pub(crate) fn claude_session_path(session_id: &str, cwd: &std::path::Path) -> Option<PathBuf> {
    let home = std::env::var_os("HOME").map(PathBuf::from)?;
    let encoded: String = cwd
        .to_string_lossy()
        .chars()
        .map(|c| if c == '/' || c == '.' { '-' } else { c })
        .collect();
    Some(
        home.join(".claude")
            .join("projects")
            .join(encoded)
            .join(format!("{session_id}.jsonl")),
    )
}

fn claude_native_runtime_with_home(
    session_id: &str,
    cwd: &std::path::Path,
    command: &str,
    args: &[String],
    provider_home: Option<&std::path::Path>,
) -> NativeRuntimeMeta {
    let mut launch_argv = vec![command.to_string()];
    launch_argv.extend(args.iter().cloned());
    // Resume forks the prior conversation into a fresh session id (dec_052).
    let resume_argv = vec![
        command.to_string(),
        "--resume".to_string(),
        session_id.to_string(),
        "--fork-session".to_string(),
        "--dangerously-skip-permissions".to_string(),
    ];
    NativeRuntimeMeta {
        provider: "claude".to_string(),
        session_id: Some(session_id.to_string()),
        session_path: provider_home
            .and_then(|home| {
                claude_projects_dir_with_home(cwd, Some(home))
                    .map(|dir| dir.join(format!("{session_id}.jsonl")))
            })
            .or_else(|| claude_session_path(session_id, cwd)),
        launch_argv,
        resume_argv,
        credential_mode: None,
    }
}

fn should_use_default_command(cfg: &TmuxTuiConfig, _harness: &str) -> bool {
    // The dispatch placeholder is the daemon's explicit "swap me for the real
    // harness" sentinel (api.rs stages every worker with it); honor it for any
    // TUI harness, not just claude. `default_command_for_harness` resolves the
    // right binary (codex, hermes, …) and falls back to `sh` for unknown ones.
    cfg.command.is_none() || is_dispatch_placeholder(cfg.command.as_deref(), &cfg.args)
}

/// The daemon's dispatch path stages every worker with a placeholder command
/// (`sh -lc 'echo orgasmic pipeline stage acquired; exec sh'`); terminal
/// drivers swap it for the real harness invocation. Shared with the tmux
/// driver so both recognize the same sentinel.
pub(crate) fn is_dispatch_placeholder(command: Option<&str>, args: &[String]) -> bool {
    command == Some("sh")
        && args.len() == 2
        && args.first().map(|arg| arg.as_str()) == Some("-lc")
        && args
            .get(1)
            .map(|arg| arg.contains("orgasmic pipeline stage acquired"))
            .unwrap_or(false)
}

fn default_command_for_harness(harness: &str, cfg: &TmuxTuiConfig) -> (String, Vec<String>) {
    match harness {
        "claude" => {
            let mut args = Vec::new();
            if let Some(model) = cfg.model.as_ref() {
                args.push("--model".to_string());
                args.push(model.clone());
            }
            args.push("--dangerously-skip-permissions".to_string());
            ("claude".to_string(), args)
        }
        "codex" => ("codex".to_string(), Vec::new()),
        "cursor-agent" => ("cursor-agent".to_string(), Vec::new()),
        "hermes" => (
            "hermes".to_string(),
            vec!["chat".to_string(), "--tui".to_string()],
        ),
        _ => ("sh".to_string(), Vec::new()),
    }
}

fn inert_reason(cfg: &TmuxTuiConfig, command: &str) -> Option<String> {
    if cfg.force_inert {
        return Some("force_inert".to_string());
    }
    if !tmux_available() {
        return Some("tmux_missing".to_string());
    }
    if !command_available(command) {
        return Some(format!("binary_missing:{command}"));
    }
    None
}

fn command_available(command: &str) -> bool {
    StdCommand::new("which")
        .arg(command)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

/// Initial session geometry. Matches the daemon's PTY-attach bridge init size
/// so the wrapped TUI lays out once instead of repainting on first attach.
const TMUX_SESSION_COLS: &str = "200";
const TMUX_SESSION_ROWS: &str = "50";

struct ClaudeForkLaunchObservation {
    since: std::time::SystemTime,
    excluded: std::collections::BTreeSet<String>,
    directory: ClaudeProjectsDirectory,
}

fn execution_command(plan: &TmuxSpawnPlan) -> Result<(String, Vec<String>), DriverError> {
    let Some(pin) = plan.pinned_executable.as_ref() else {
        return Ok((plan.command.clone(), plan.args.clone()));
    };
    if plan.trusted_provider_identity.as_deref() != Some("claude") {
        return Err(DriverError::InvalidConfig(
            "pinned executable requires trusted provider identity".into(),
        ));
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        let metadata = std::fs::symlink_metadata(&pin.exec_wrapper)
            .map_err(|_| DriverError::InvalidConfig("pinned exec wrapper is unavailable".into()))?;
        if !metadata.is_file()
            || metadata.file_type().is_symlink()
            || metadata.dev() != pin.exec_wrapper_dev
            || metadata.ino() != pin.exec_wrapper_ino
        {
            return Err(DriverError::InvalidConfig(
                "pinned exec wrapper identity mismatch".into(),
            ));
        }
    }
    let mut args = vec![
        "__exec-pinned".to_string(),
        pin.path.to_string_lossy().into_owned(),
    ];
    #[cfg(unix)]
    {
        args.push(pin.dev.to_string());
        args.push(pin.ino.to_string());
    }
    args.push("--".to_string());
    args.extend(plan.args.iter().cloned());
    Ok((pin.exec_wrapper.to_string_lossy().into_owned(), args))
}

async fn spawn_tmux_session(
    session: &str,
    plan: &TmuxSpawnPlan,
) -> Result<Option<ClaudeForkLaunchObservation>, DriverError> {
    // After a daemon crash, a previous tmux pane may still hold this name.
    kill_tmux_session(session).await;

    let (mut execution_command, mut execution_args) = execution_command(plan)?;
    let gate = (plan.native_resume_mode
        && plan.trusted_provider_identity.as_deref() == Some("claude"))
    .then(|| {
        std::env::temp_dir().join(format!(
            "orgasmic-claude-launch-{}-{}",
            sanitize_tmux_name(session),
            uuid::Uuid::new_v4()
        ))
    });
    if let Some(gate) = gate.as_ref() {
        let mut gated_args = vec![
            "-c".to_string(),
            "gate=$1; shift; while [ ! -e \"$gate\" ]; do sleep 0.01; done; rm -f -- \"$gate\"; exec \"$@\""
                .to_string(),
            "orgasmic-claude-launch-gate".to_string(),
            gate.to_string_lossy().into_owned(),
            execution_command,
        ];
        gated_args.append(&mut execution_args);
        execution_command = "/bin/sh".to_string();
        execution_args = gated_args;
    }

    let run_id_env = format!("ORGASMIC_RUN_ID={}", plan.run_id);
    let runtime_id_env = format!("ORGASMIC_RUNTIME_ID={}", plan.runtime_id);
    let boot_id_env = format!("ORGASMIC_BOOT_ID={}", plan.boot_id);
    let manager_terminal_capability_env = plan
        .manager_terminal_capability
        .as_ref()
        .map(|capability| format!("ORGASMIC_MANAGER_TERMINAL_CAPABILITY={capability}"));

    // orgasmic:TASK-RKTH1
    // Everything below goes into one imsg. Count it before tmux does, and move
    // the payload off the command line when it will not fit, so a large brief
    // spawns instead of dying on tmux's `command too long`. Applied after the
    // launch gate and the pinned-executable wrap so whatever those produced is
    // what the launcher execs — neither is bypassed, both just move inside.
    let mut launcher: Option<PathBuf> = None;
    {
        let harness_env: Vec<String> = plan
            .harness_env
            .iter()
            .map(|(key, value)| format!("{key}={value}"))
            .collect();
        let cwd = plan.cwd.to_string_lossy();
        let mut framing: Vec<&str> = vec![
            "new-session",
            "-d",
            "-s",
            session,
            "-x",
            TMUX_SESSION_COLS,
            "-y",
            TMUX_SESSION_ROWS,
            "-e",
            run_id_env.as_str(),
            "-e",
            runtime_id_env.as_str(),
            "-e",
            boot_id_env.as_str(),
        ];
        if let Some(capability) = manager_terminal_capability_env.as_deref() {
            framing.push("-e");
            framing.push(capability);
        }
        for pair in &harness_env {
            framing.push("-e");
            framing.push(pair.as_str());
        }
        framing.extend(["-c", cwd.as_ref(), "--"]);
        let packed = packed_argv_len(framing)
            + packed_argv_len(std::iter::once(execution_command.as_str()))
            + packed_argv_len(execution_args.iter().map(String::as_str));
        if packed > TMUX_PACKED_ARGV_BUDGET {
            let path = write_launcher_script(session, &execution_command, &execution_args)?;
            execution_command = "/bin/sh".to_string();
            execution_args = vec![path.to_string_lossy().into_owned()];
            launcher = Some(path);
        }
    }

    let mut tmux = tmux_async_command();
    tmux.args([
        "new-session",
        "-d",
        "-s",
        session,
        "-x",
        TMUX_SESSION_COLS,
        "-y",
        TMUX_SESSION_ROWS,
        "-e",
    ])
    .arg(&run_id_env);
    tmux.arg("-e").arg(&runtime_id_env);
    tmux.arg("-e").arg(&boot_id_env);
    // orgasmic:TASK-GT91X
    for (key, value) in &plan.harness_env {
        tmux.arg("-e").arg(format!("{key}={value}"));
    }
    tmux.arg("-c")
        .arg(&plan.cwd)
        .arg("--")
        .arg(&execution_command);
    for a in &execution_args {
        tmux.arg(a);
    }
    let output = tmux
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .output()
        .await
        .map_err(|e| DriverError::Transport(format!("tmux spawn: {e}")))?;
    if !output.status.success() {
        // orgasmic:TASK-RKTH1
        // The launcher deletes itself as its first act, so it is still on disk
        // exactly when the pane never ran it. Nothing else will collect it, and
        // it holds the whole prompt.
        if let Some(path) = launcher.as_ref() {
            let _ = std::fs::remove_file(path);
        }
        let stderr = String::from_utf8_lossy(&output.stderr);
        // orgasmic:TASK-0RCRY
        // Name the server. `server exited unexpectedly` is true and useless on
        // a host where tmux is installed and working; which server was reached,
        // and how it got chosen, is the part that tells "tmux is broken" apart
        // from "this server is shared with something else".
        return Err(DriverError::Transport(format!(
            "tmux new-session failed (exit {}) on {}: {}",
            output.status.code().unwrap_or(-1),
            tmux_server_selection(),
            stderr.trim()
        )));
    }
    let launch_observation = if let Some(gate) = gate {
        // The pane is blocked in the launch gate here. Retain the exact
        // provider directory, record the lower bound, release the pane, then
        // snapshot exclusions. Anything created in the former
        // snapshot-to-release gap is now excluded; only candidates absent
        // after the ordered release boundary can be accepted.
        let directory =
            match ClaudeProjectsDirectory::open(&plan.cwd, plan.provider_home.as_deref()) {
                Ok(directory) => directory,
                Err(error) => {
                    kill_tmux_session(session).await;
                    return Err(error);
                }
            };
        #[cfg(test)]
        if let Some(hook) = CLAUDE_PRE_RELEASE_TEST_HOOK
            .lock()
            .expect("Claude pre-release hook lock")
            .take()
        {
            hook();
        }
        let since = std::time::SystemTime::now();
        if let Err(error) = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&gate)
        {
            kill_tmux_session(session).await;
            return Err(DriverError::Transport(format!(
                "release Claude launch gate: {error}"
            )));
        }
        let excluded = match directory.names() {
            Ok(excluded) => excluded,
            Err(error) => {
                kill_tmux_session(session).await;
                return Err(error);
            }
        };
        Some(ClaudeForkLaunchObservation {
            since,
            excluded,
            directory,
        })
    } else {
        None
    };
    // Best-effort quality-of-life options for browser attach (lifted from HAR):
    // mouse lets the operator scroll/select inside the attached xterm; the
    // rename guard keeps the session name stable for run lookups.
    for opts in [
        ["set-option", "-t", session, "mouse", "on"],
        ["set-option", "-t", session, "allow-rename", "off"],
    ] {
        let _ = tmux_async_command()
            .args(opts)
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .await;
    }
    Ok(launch_observation)
}

async fn paste_text_into_pane(
    session: &str,
    text: &str,
    send_child: Option<&SendChildOwner>,
    cancel: Option<&AtomicBool>,
) -> Result<(), DriverError> {
    if text.is_empty() {
        return Ok(());
    }
    let buffer_name = load_tmux_buffer(session, text, send_child, cancel).await?;
    run_tmux(
        &["paste-buffer", "-p", "-b", &buffer_name, "-t", session],
        send_child,
        cancel,
    )
    .await?;
    let _ = run_tmux(&["delete-buffer", "-b", &buffer_name], send_child, cancel).await;
    Ok(())
}

/// Inject the sole externally-wakeable manager payload.
///
/// Do not add `-p` here. tmux's `paste-buffer -p` wraps data in bracketed-paste
/// escape bytes, which makes the raw pane input differ from the fixed marker.
/// The fixed marker is intentionally a shell-inert `:` command if the target
/// races from a provider composer into zsh/bash; `Enter` is sent separately by
/// [`TmuxTuiControl::send_manager_wake`]. Keeping this helper argument-free is
/// an executable boundary: this path can never paste caller-controlled text.
async fn paste_manager_wake_marker_into_pane(
    session: &str,
    send_child: Option<&SendChildOwner>,
    cancel: Option<&AtomicBool>,
) -> Result<(), DriverError> {
    let buffer_name = load_tmux_buffer(
        session,
        crate::r#trait::MANAGER_WAKE_MARKER,
        send_child,
        cancel,
    )
    .await?;
    // Deliberately no `-p`: raw pane bytes must be exactly the marker.
    run_tmux(
        &["paste-buffer", "-b", &buffer_name, "-t", session],
        send_child,
        cancel,
    )
    .await?;
    let _ = run_tmux(&["delete-buffer", "-b", &buffer_name], send_child, cancel).await;
    Ok(())
}

async fn load_tmux_buffer(
    session: &str,
    text: &str,
    send_child: Option<&SendChildOwner>,
    cancel: Option<&AtomicBool>,
) -> Result<String, DriverError> {
    let buffer_name = format!("orgasmic-{}", sanitize_tmux_name(session));
    if let Some(owner) = send_child {
        let mut input = tempfile::tempfile()
            .map_err(|e| DriverError::Transport(format!("tmux load-buffer tempfile: {e}")))?;
        input
            .write_all(text.as_bytes())
            .and_then(|_| input.seek(SeekFrom::Start(0)))
            .map_err(|e| DriverError::Transport(format!("tmux load-buffer prepare: {e}")))?;
        owner
            .spawn_register_and_wait(cancel, || {
                let mut cmd = tmux_async_command();
                cmd.args(["load-buffer", "-b", &buffer_name, "-"])
                    .stdin(Stdio::from(input))
                    .stdout(Stdio::null())
                    .stderr(Stdio::piped());
                Ok(cmd)
            })
            .await?;
    } else {
        let mut child = tmux_async_command()
            .args(["load-buffer", "-b", &buffer_name, "-"])
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .spawn()
            .map_err(|e| DriverError::Transport(format!("tmux load-buffer spawn: {e}")))?;
        if let Some(mut stdin) = child.stdin.take() {
            stdin
                .write_all(text.as_bytes())
                .await
                .map_err(|e| DriverError::Transport(format!("tmux load-buffer write: {e}")))?;
            let _ = stdin.shutdown().await;
        }
        wait_for_send_child(child, cancel).await?;
    }
    Ok(buffer_name)
}

async fn send_keys(
    session: &str,
    keys: &[String],
    send_child: Option<&SendChildOwner>,
    cancel: Option<&AtomicBool>,
) -> Result<(), DriverError> {
    if keys.is_empty() {
        return Ok(());
    }
    let mut args = vec!["send-keys", "-t", session];
    for key in keys {
        args.push(key.as_str());
    }
    run_tmux(&args, send_child, cancel).await
}

async fn run_tmux(
    args: &[&str],
    send_child: Option<&SendChildOwner>,
    cancel: Option<&AtomicBool>,
) -> Result<(), DriverError> {
    if let Some(owner) = send_child {
        let args = args.iter().map(|arg| arg.to_string()).collect::<Vec<_>>();
        owner
            .spawn_register_and_wait(cancel, || {
                let mut cmd = tmux_async_command();
                for arg in &args {
                    cmd.arg(arg);
                }
                cmd.stdout(Stdio::null()).stderr(Stdio::piped());
                cmd.kill_on_drop(true);
                Ok(cmd)
            })
            .await
    } else {
        let child = tmux_async_command()
            .args(args)
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .spawn()
            .map_err(|e| DriverError::Transport(format!("tmux {:?}: {e}", args)))?;
        wait_for_send_child(child, cancel).await
    }
}

pub(crate) async fn wait_for_owned_send_child(
    owner: &SendChildOwner,
    cancel: Option<&AtomicBool>,
) -> Result<(), DriverError> {
    loop {
        if cancel.is_some_and(|flag| flag.load(Ordering::SeqCst)) {
            owner.kill_and_reap().await;
            return Ok(());
        }
        let wait_result = {
            let mut guard = owner.active.lock().unwrap();
            let Some(child) = guard.as_mut() else {
                return Ok(());
            };
            match child.try_wait() {
                Ok(Some(status)) => {
                    guard.take();
                    if status.success() {
                        Ok(Some(true))
                    } else {
                        Ok(Some(false))
                    }
                }
                Ok(None) => Ok(None),
                Err(e) => {
                    guard.take();
                    Err(e)
                }
            }
        };
        match wait_result {
            Ok(Some(true)) => return Ok(()),
            Ok(Some(false)) => {
                return Err(DriverError::Transport(
                    "tmux send child exited with failure".into(),
                ));
            }
            Ok(None) => tokio::time::sleep(Duration::from_millis(10)).await,
            Err(e) => {
                return Err(DriverError::Transport(format!("tmux send child wait: {e}")));
            }
        }
    }
}

async fn wait_for_send_child(
    mut child: Child,
    cancel: Option<&AtomicBool>,
) -> Result<(), DriverError> {
    loop {
        if cancel.is_some_and(|flag| flag.load(Ordering::SeqCst)) {
            let _ = child.kill().await;
            let _ = child.wait().await;
            return Ok(());
        }
        match child.try_wait() {
            Ok(Some(status)) => {
                if status.success() {
                    return Ok(());
                }
                return Err(DriverError::Transport(format!(
                    "tmux send child exited with {status}"
                )));
            }
            Ok(None) => {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
            Err(e) => {
                return Err(DriverError::Transport(format!("tmux send child wait: {e}")));
            }
        }
    }
}

fn sanitize_tmux_name(session: &str) -> String {
    session
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                c
            } else {
                '_'
            }
        })
        .collect()
}

/// Watch for pane/process end only. No scrollback scrape, no marker watch,
/// no TextChunk synthesis — live view stays on `/ws/tmux/:run_id` (TASK-AFE5Q).
// orgasmic:TASK-AFE5Q
fn start_session_exit_watch(
    session_name: String,
    events: mpsc::Sender<DriverEvent>,
    terminal_emitted: Arc<AtomicBool>,
) -> JoinHandle<()> {
    tokio::spawn(async move {
        session_exit_watch(session_name, events, terminal_emitted).await;
    })
}

async fn session_exit_watch(
    session: String,
    events: mpsc::Sender<DriverEvent>,
    terminal_emitted: Arc<AtomicBool>,
) {
    let mut poll = tokio::time::interval(Duration::from_millis(500));
    poll.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

    loop {
        poll.tick().await;
        if terminal_emitted.load(Ordering::SeqCst) {
            break;
        }
        if !has_tmux_session(&session).await.unwrap_or(false) {
            emit_fatal_driver_error_once(
                &events,
                &terminal_emitted,
                format!("tmux session {session} ended without finalize"),
            )
            .await;
            break;
        }
    }
}

// orgasmic:TASK-4CSMY
/// How often the pane watcher re-checks `terminal_emitted` while no bytes are
/// arriving. Only a shutdown latency, not a sampling rate: every byte the pane
/// writes wakes the read arm immediately.
const PANE_ACTIVITY_SHUTDOWN_POLL: Duration = Duration::from_millis(500);

/// Coalescing cadence for pane-liveness events. At 30 seconds, a four-hour
/// run adds at most 480 content-free events while staying far inside the
/// supervisor's stall timeout.
const PANE_ACTIVITY_INTERVAL: Duration = Duration::from_secs(30);

/// Coalesce raw pane byte observations into at most one liveness event per
/// interval. Pane contents never cross this boundary.
struct PaneActivityThrottle {
    interval: Duration,
    window_started_at: Option<tokio::time::Instant>,
    bytes: u64,
    seq: u64,
}

impl PaneActivityThrottle {
    fn new(interval: Duration) -> Self {
        Self {
            interval,
            window_started_at: None,
            bytes: 0,
            seq: 0,
        }
    }

    fn observe_bytes(&mut self, bytes: u64, now: tokio::time::Instant) -> Option<DriverEvent> {
        self.bytes = self.bytes.saturating_add(bytes);
        let window_started_at = *self.window_started_at.get_or_insert(now);
        if now.duration_since(window_started_at) < self.interval {
            return None;
        }
        self.window_started_at = Some(now);
        let event = DriverEvent::PaneActivity {
            seq: self.seq,
            bytes: std::mem::take(&mut self.bytes),
        };
        self.seq += 1;
        Some(event)
    }
}

// orgasmic:TASK-4CSMY
/// Start the tmux transport's pane-liveness channel: the coalesced
/// [`DriverEvent::PaneActivity`] the supervisor's stall clock reads
/// (TASK-RWCRN gave tmux this; tmux had no continuous pane event of any kind,
/// so a provider-bound turn at ~0% cpu had no evidence channel at all).
///
/// Returns `None` when the channel could not be established; the run then
/// behaves exactly as it did before this existed. Failing to start must never
/// fail a run: this is evidence, not control.
#[cfg(unix)]
fn start_pane_activity_watch(
    session_name: String,
    events: mpsc::Sender<DriverEvent>,
    terminal_emitted: Arc<AtomicBool>,
) -> Option<JoinHandle<()>> {
    Some(tokio::spawn(async move {
        pane_activity_watch(
            session_name,
            events,
            terminal_emitted,
            PANE_ACTIVITY_INTERVAL,
        )
        .await;
    }))
}

#[cfg(not(unix))]
fn start_pane_activity_watch(
    _session_name: String,
    _events: mpsc::Sender<DriverEvent>,
    _terminal_emitted: Arc<AtomicBool>,
) -> Option<JoinHandle<()>> {
    None
}

// orgasmic:TASK-4CSMY
/// Publish coalesced pane activity for a live tmux session until the run ends.
///
/// tmux has no SDK output stream, so the analogue of tmux's `PaneOutputStream`
/// is `pipe-pane`: tmux runs a shell command *on the server* with the pane's
/// raw output on its stdin. Piping that into a FIFO this task already holds
/// open is what brings those bytes back into the driver process.
///
/// The unit is raw output BYTES, not lines (TASK-RWCRN.1): a full-screen TUI
/// redraws in place with CR and ANSI and can go for many minutes without ever
/// emitting an LF, and a line-counting channel sees nothing at all for exactly
/// the harnesses this event exists to protect.
///
/// Nothing is persisted. A FIFO has no backing store, and only the byte
/// *count* leaves this function — the buffer is overwritten in place and never
/// forwarded (dec_WDR5K item 7).
#[cfg(unix)]
async fn pane_activity_watch(
    session: String,
    events: mpsc::Sender<DriverEvent>,
    terminal_emitted: Arc<AtomicBool>,
    activity_interval: Duration,
) {
    use tokio::io::AsyncReadExt;

    // The FIFO's directory is a `TempDir`: dropped when this task ends or is
    // aborted, which is every exit path including release.
    let Some((_fifo_dir, fifo_path)) = pane_output_fifo(&session) else {
        return;
    };
    let mut reader = match tokio::net::unix::pipe::OpenOptions::new().open_receiver(&fifo_path) {
        Ok(reader) => reader,
        Err(error) => {
            tracing::warn!(%session, ?error, "tmux pane activity: FIFO unreadable");
            return;
        }
    };
    // A FIFO with no writer reads EOF rather than blocking, and `pipe-pane`'s
    // `cat` comes and goes with the pane. Holding a write end of our own means
    // the read side never ends before this task does. Opened second because
    // O_WRONLY on a FIFO fails with ENXIO until a reader is present.
    let _writer_end = match tokio::net::unix::pipe::OpenOptions::new().open_sender(&fifo_path) {
        Ok(writer) => writer,
        Err(error) => {
            tracing::warn!(%session, ?error, "tmux pane activity: FIFO unwritable");
            return;
        }
    };
    // Both ends first, then the pipe: `cat` blocks opening a FIFO for writing
    // until a reader is present, and that block would be inside the tmux
    // server's child.
    if !start_pipe_pane(&session, &fifo_path).await {
        return;
    }

    let mut activity = PaneActivityThrottle::new(activity_interval);
    let mut buf = vec![0u8; 16 * 1024];
    loop {
        tokio::select! {
            read = reader.read(&mut buf) => {
                let observed = match read {
                    Ok(0) => break,
                    Ok(bytes) => bytes as u64,
                    Err(error) if error.kind() == std::io::ErrorKind::Interrupted => continue,
                    Err(error) => {
                        tracing::warn!(%session, ?error, "tmux pane activity: read ended");
                        break;
                    }
                };
                if terminal_emitted.load(Ordering::SeqCst) {
                    break;
                }
                if let Some(event) =
                    activity.observe_bytes(observed, tokio::time::Instant::now())
                {
                    if events.send(event).await.is_err() {
                        break;
                    }
                }
            }
            _ = tokio::time::sleep(PANE_ACTIVITY_SHUTDOWN_POLL) => {
                if terminal_emitted.load(Ordering::SeqCst) {
                    break;
                }
            }
        }
    }
}

// orgasmic:TASK-4CSMY
/// A private FIFO for one session's pane output, and the directory that owns
/// its lifetime.
#[cfg(unix)]
fn pane_output_fifo(session: &str) -> Option<(tempfile::TempDir, PathBuf)> {
    let dir = match tempfile::Builder::new().prefix("orgasmic-pane-").tempdir() {
        Ok(dir) => dir,
        Err(error) => {
            tracing::warn!(%session, ?error, "tmux pane activity: no FIFO directory");
            return None;
        }
    };
    let path = dir.path().join("pane.fifo");
    let Ok(c_path) = std::ffi::CString::new(path.as_os_str().as_encoded_bytes()) else {
        return None;
    };
    // 0o600: the pane's raw output passes through it, so nothing else on the
    // box may read it (the directory is 0o700 already).
    if unsafe { libc::mkfifo(c_path.as_ptr(), 0o600) } != 0 {
        tracing::warn!(
            %session,
            error = ?std::io::Error::last_os_error(),
            "tmux pane activity: mkfifo failed"
        );
        return None;
    }
    Some((dir, path))
}

// orgasmic:TASK-4CSMY
/// `tmux pipe-pane -t <session> 'exec cat >> <fifo>'` — tell the server to
/// copy the pane's raw output into our FIFO. `exec` so the pane's writer is
/// `cat` itself rather than a shell holding it.
#[cfg(unix)]
async fn start_pipe_pane(session: &str, fifo: &Path) -> bool {
    let sink = format!(
        "exec cat >> {}",
        sh_single_quote(&fifo.display().to_string())
    );
    let status = tmux_async_command()
        .args(["pipe-pane", "-t", session, &sink])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .kill_on_drop(true)
        .status()
        .await;
    match status {
        Ok(status) if status.success() => true,
        Ok(status) => {
            tracing::warn!(%session, ?status, "tmux pane activity: pipe-pane refused");
            false
        }
        Err(error) => {
            tracing::warn!(%session, ?error, "tmux pane activity: pipe-pane failed");
            false
        }
    }
}

async fn capture_pane(session: &str) -> Result<String, DriverError> {
    let output = tmux_async_command()
        .args(["capture-pane", "-p", "-t", session, "-S", "-2000"])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .await
        .map_err(|e| DriverError::Transport(format!("tmux capture-pane: {e}")))?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(DriverError::Transport(format!(
            "tmux capture-pane failed: {}",
            stderr.trim()
        )));
    }
    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
}

/// Deliver the dispatch prompt into a spawned session: wait for the harness
/// input prompt (accepting any folder-trust dialog on the way), then paste the
/// brief and submit. Runs in the background after `acquire` returns; a failure
/// becomes a fatal `DriverError` on the event stream so the run fails cleanly
/// instead of leaving the worker idle without its brief.
#[allow(clippy::too_many_arguments)]
async fn deliver_prompt(
    session: &str,
    command: &str,
    prompt: &str,
    input_ready_timeout: Duration,
    events: &mpsc::Sender<DriverEvent>,
    terminal_emitted: &AtomicBool,
    send_child: Option<SendChildOwner>,
    cancel: Option<Arc<AtomicBool>>,
) {
    if command == "claude" {
        if let Err(e) = wait_for_input_ready(session, input_ready_timeout).await {
            tracing::warn!(
                ?e,
                "tmux TUI input field not detected within timeout; pasting anyway"
            );
        }
    } else {
        tokio::time::sleep(Duration::from_millis(800)).await;
    }
    let result = async {
        paste_text_into_pane(
            session,
            prompt,
            send_child.as_ref(),
            cancel.as_ref().map(|flag| flag.as_ref()),
        )
        .await?;
        send_keys(
            session,
            &[String::from("Enter")],
            send_child.as_ref(),
            cancel.as_ref().map(|flag| flag.as_ref()),
        )
        .await
    }
    .await;
    if let Err(e) = result {
        emit_fatal_driver_error_once(
            events,
            terminal_emitted,
            format!("dispatch prompt delivery failed: {e}"),
        )
        .await;
    }
}

async fn capture_pane_visible(session: &str) -> Result<String, DriverError> {
    let output = tmux_async_command()
        .args(["capture-pane", "-p", "-t", session])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .await
        .map_err(|e| DriverError::Transport(format!("tmux capture-pane: {e}")))?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(DriverError::Transport(format!(
            "tmux capture-pane failed: {}",
            stderr.trim()
        )));
    }
    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
}

pub(crate) fn provider_composer_ready(pane: &str, provider: &str) -> bool {
    // Inspect only the current bottom input component. A prompt glyph anywhere
    // in scrollback is historical output, not authority to interrupt a busy
    // provider. We also require an *empty* composer so a human's typed draft
    // is never overwritten. The marker itself is safe by construction, but
    // this remains a deliberately strict targeting/refusal gate.
    // Claude and Codex both draw the empty input as a two-line bottom
    // component: a visible separator/blank viewport row followed immediately
    // by the prompt line. Restricting the search to the last two screen rows
    // means a prompt glyph from an earlier output frame (including one followed
    // by several empty rows) cannot be mistaken for the live composer.
    let mut bottom = pane
        .lines()
        .rev()
        .take(2)
        .map(strip_ansi_codes)
        .map(|line| line.trim().to_string());
    let Some(line) = bottom.next() else {
        return false;
    };
    let Some(component_top) = bottom.next() else {
        return false;
    };
    if !component_top.is_empty() {
        return false;
    }
    let empty_prompt = |glyph: &str| {
        line.strip_prefix(glyph)
            .is_some_and(|rest| rest.trim().is_empty())
            && !line_is_numbered_menu_item(&line)
    };
    match provider {
        "claude" => empty_prompt("❯"),
        "codex" => empty_prompt("›"),
        _ => false,
    }
}

/// Fresh tmux foreground probe used only to target the fixed inert marker.
/// tmux cannot make the subsequent paste conditional, which is why this
/// function must never be described as a shell-byte safety guarantee.
async fn tmux_provider_ready(session: &str) -> Result<bool, DriverError> {
    let pane_pid = tmux_async_command()
        .args(["display-message", "-p", "-t", session, "#{pane_pid}"])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .await
        .map_err(|e| DriverError::Transport(format!("tmux pane pid: {e}")))?;
    if !pane_pid.status.success() {
        return Err(DriverError::ManagerWakeUnavailable);
    }
    let pane_pid = String::from_utf8_lossy(&pane_pid.stdout).trim().to_string();
    if pane_pid.parse::<u32>().is_err() {
        return Err(DriverError::ManagerWakeUnavailable);
    }
    let foreground = tokio::process::Command::new("/bin/ps")
        .args(["-o", "tpgid=", "-p", &pane_pid])
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .output()
        .await
        .map_err(|e| DriverError::Transport(format!("ps tpgid: {e}")))?;
    if !foreground.status.success() {
        return Err(DriverError::ManagerWakeUnavailable);
    }
    let tpgid = String::from_utf8_lossy(&foreground.stdout)
        .trim()
        .to_string();
    if tpgid.parse::<u32>().is_err() {
        return Err(DriverError::ManagerWakeUnavailable);
    }
    let executable = tokio::process::Command::new("/bin/ps")
        .args(["-o", "comm=", "-p", &tpgid])
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .output()
        .await
        .map_err(|e| DriverError::Transport(format!("ps foreground executable: {e}")))?;
    if !executable.status.success() {
        return Err(DriverError::ManagerWakeUnavailable);
    }
    let executable = String::from_utf8_lossy(&executable.stdout)
        .trim()
        .to_ascii_lowercase();
    let provider = match executable.rsplit('/').next() {
        Some("claude") => "claude",
        Some("codex") => "codex",
        _ => return Err(DriverError::ManagerWakeProviderMismatch),
    };
    let pane = match capture_pane_visible(session).await {
        Ok(pane) => pane,
        Err(error) => return Err(normalize_manager_wake_target_loss(session, error).await),
    };
    Ok(provider_composer_ready(&pane, provider))
}

/// Tmux has no typed error protocol for target disappearance: `capture-pane`,
/// `paste-buffer`, and `send-keys` all reduce it to a failed client command.
/// Re-probe the exact session only on the manager-wake path so the public wake
/// contract returns unavailable (CLI exit 5), not a generic transport failure.
async fn normalize_manager_wake_target_loss(session: &str, error: DriverError) -> DriverError {
    normalize_manager_wake_target_loss_probe(has_tmux_session(session).await, error)
}

fn normalize_manager_wake_target_loss_probe(
    session_exists: Result<bool, DriverError>,
    error: DriverError,
) -> DriverError {
    if matches!(
        error,
        DriverError::ManagerWakeUnavailable | DriverError::ManagerWakeProviderMismatch
    ) {
        return error;
    }
    match session_exists {
        Ok(false) | Err(_) => DriverError::ManagerWakeUnavailable,
        Ok(true) => error,
    }
}

/// True when cursor-agent argv delivery still needs a startup-only trust
/// transition (prompt already on argv — never paste again).
pub(crate) fn cursor_argv_needs_startup_trust(
    harness: &str,
    paste_prompt: &Option<String>,
) -> bool {
    harness == "cursor-agent" && paste_prompt.is_none()
}

/// Startup-only classification of the current visible pane frame. Never scans
/// scrollback — only the live viewport matters (TASK-756WX).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum CursorStartupFrame {
    BlankOrLoading,
    TrustDialog,
    Ready,
}

/// Whether the visible frame matches Cursor 2026.07.16's ordered trust component.
/// Requires the full contiguous interactive component including the real first
/// description line, workspace path, and ordered actions (TASK-ZGT1X).
// orgasmic:TASK-756WX,TASK-AFE5Q,TASK-ZHRRH,TASK-ZGT1X
pub(crate) fn is_cursor_trust_dialog_layout(pane: &str, workspace_path: &str) -> bool {
    parse_cursor_trust_component(pane, workspace_path).is_some()
}

async fn send_trust_key_guarded<S, SFut>(
    validated_pane: &str,
    workspace_path: &str,
    send_key: &mut S,
    cancel: &Option<Arc<AtomicBool>>,
) -> Result<(), DriverError>
where
    S: FnMut(&str) -> SFut,
    SFut: std::future::Future<Output = Result<(), DriverError>>,
{
    if startup_cancelled(cancel) {
        return Ok(());
    }
    if is_cursor_trust_dialog_layout(validated_pane, workspace_path) {
        // Deliver immediately after synchronous re-validation on the captured
        // frame — send_key must spawn without an intervening mux capture (TASK-NW4WV).
        send_key("a").await
    } else {
        Ok(())
    }
}

pub(crate) fn classify_cursor_startup_frame(
    pane: &str,
    workspace_path: &str,
) -> CursorStartupFrame {
    let trimmed = pane.trim();
    if trimmed.is_empty() || cursor_startup_frame_is_loading(trimmed) {
        return CursorStartupFrame::BlankOrLoading;
    }
    if is_cursor_trust_dialog_layout(pane, workspace_path) {
        return CursorStartupFrame::TrustDialog;
    }
    CursorStartupFrame::Ready
}

const CURSOR_TRUST_TITLE: &str = "workspace trust required";
const CURSOR_TRUST_DESCRIPTION: &str =
    "Cursor Agent can execute code and access files in this directory.";
const CURSOR_TRUST_MCP_DESCRIPTION: &str =
    "This will also enable the MCP servers configured for this workspace.";
const CURSOR_TRUST_QUESTION: &str = "Do you trust the contents of this directory?";
const CURSOR_TRUST_ACTION: &str = "[a] trust this workspace";
const CURSOR_TRUST_MCP_ACTION: &str = "[w] trust this workspace, but don't enable all mcp servers";
const CURSOR_TRUST_QUIT: &str = "[q] quit";

fn meaningful_pane_lines(pane: &str) -> Vec<String> {
    pane.lines()
        .map(|line| strip_ansi_codes(line).trim().to_string())
        .filter(|line| !line.is_empty())
        .collect()
}

fn parse_cursor_trust_component(pane: &str, workspace_path: &str) -> Option<()> {
    let lines = meaningful_pane_lines(pane);
    if lines.is_empty() {
        return None;
    }
    let mut i = 0;
    if lines[i].to_ascii_lowercase() != CURSOR_TRUST_TITLE {
        return None;
    }
    i += 1;
    if i >= lines.len() || lines[i] != CURSOR_TRUST_DESCRIPTION {
        return None;
    }
    i += 1;
    let has_mcp_description = i < lines.len() && lines[i] == CURSOR_TRUST_MCP_DESCRIPTION;
    if has_mcp_description {
        i += 1;
    }
    if i >= lines.len() || lines[i] != CURSOR_TRUST_QUESTION {
        return None;
    }
    i += 1;
    if i >= lines.len() || !workspace_path_matches(&lines[i], workspace_path) {
        return None;
    }
    i += 1;
    if i >= lines.len() || lines[i].to_ascii_lowercase() != CURSOR_TRUST_ACTION {
        return None;
    }
    i += 1;
    let has_mcp_action =
        i < lines.len() && lines[i].to_ascii_lowercase() == CURSOR_TRUST_MCP_ACTION;
    if has_mcp_description != has_mcp_action {
        return None;
    }
    if has_mcp_action {
        i += 1;
    }
    if i >= lines.len() || lines[i].to_ascii_lowercase() != CURSOR_TRUST_QUIT {
        return None;
    }
    i += 1;
    if i != lines.len() {
        return None;
    }
    Some(())
}

fn workspace_path_matches(displayed: &str, expected: &str) -> bool {
    fn normalize(path: &str) -> Option<PathBuf> {
        let trimmed = path.trim().trim_end_matches('/');
        if trimmed.is_empty() {
            return None;
        }
        std::path::Path::new(trimmed)
            .canonicalize()
            .ok()
            .or_else(|| Some(PathBuf::from(trimmed)))
    }
    match (normalize(displayed), normalize(expected)) {
        (Some(displayed), Some(expected)) => displayed == expected,
        _ => false,
    }
}

fn cursor_startup_frame_is_loading(pane: &str) -> bool {
    let meaningful: Vec<String> = pane
        .lines()
        .map(|line| strip_ansi_codes(line).trim().to_string())
        .filter(|line| !line.is_empty())
        .collect();
    if meaningful.is_empty() {
        return true;
    }
    if meaningful.len() == 1 {
        let line = meaningful[0].to_ascii_lowercase();
        if line.contains("loading") || line.contains("starting") || line == "..." {
            return true;
        }
    }
    false
}

/// One-shot startup state machine for Cursor workspace trust. Inspects only the
/// current visible frame; sends `a` at most once; terminates on ready/exit.
async fn accept_cursor_workspace_trust(
    session: &str,
    workspace_path: &str,
    timeout: Duration,
    cancel: Option<Arc<AtomicBool>>,
    send_child: Option<SendChildOwner>,
) -> Result<(), DriverError> {
    let session = session.to_string();
    let workspace_path = workspace_path.to_string();
    accept_cursor_workspace_trust_with_capture(
        &workspace_path,
        timeout,
        Duration::from_millis(250),
        {
            let session = session.clone();
            move || {
                let session = session.clone();
                async move { capture_pane_visible(&session).await }
            }
        },
        {
            let session = session.clone();
            move || {
                let session = session.clone();
                async move { has_tmux_session(&session).await.unwrap_or(false) }
            }
        },
        {
            let session = session.clone();
            let send_child = send_child.clone();
            let cancel_for_send = cancel.clone();
            move |key| {
                let session = session.clone();
                let key = key.to_string();
                let send_child = send_child.clone();
                let cancel_for_send = cancel_for_send.clone();
                async move {
                    send_keys(
                        &session,
                        &[key],
                        send_child.as_ref(),
                        cancel_for_send.as_ref().map(|flag| flag.as_ref()),
                    )
                    .await
                }
            }
        },
        cancel,
    )
    .await
}

fn startup_cancelled(cancel: &Option<Arc<AtomicBool>>) -> bool {
    cancel
        .as_ref()
        .is_some_and(|flag| flag.load(Ordering::SeqCst))
}

pub(crate) async fn accept_cursor_workspace_trust_with_capture<C, Fut, A, AFut, S, SFut>(
    workspace_path: &str,
    timeout: Duration,
    poll_interval: Duration,
    mut capture: C,
    mut is_alive: A,
    mut send_key: S,
    cancel: Option<Arc<AtomicBool>>,
) -> Result<(), DriverError>
where
    C: FnMut() -> Fut,
    Fut: std::future::Future<Output = Result<String, DriverError>>,
    A: FnMut() -> AFut,
    AFut: std::future::Future<Output = bool>,
    S: FnMut(&str) -> SFut,
    SFut: std::future::Future<Output = Result<(), DriverError>>,
{
    let workspace_path = workspace_path.to_string();
    let deadline = tokio::time::Instant::now() + timeout;
    let mut poll = tokio::time::interval(poll_interval);
    poll.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    poll.tick().await;
    loop {
        tokio::select! {
            _ = tokio::time::sleep_until(deadline) => {
                return Err(DriverError::InputNotReady(timeout));
            }
            _ = poll.tick() => {
                if startup_cancelled(&cancel) {
                    return Ok(());
                }
                if !is_alive().await {
                    return Ok(());
                }
                match capture().await {
                    Err(_) => continue,
                    Ok(pane) => match classify_cursor_startup_frame(&pane, &workspace_path) {
                        CursorStartupFrame::BlankOrLoading => continue,
                        CursorStartupFrame::TrustDialog => {
                            if startup_cancelled(&cancel) {
                                return Ok(());
                            }
                            match capture().await {
                                Ok(pane)
                                    if is_cursor_trust_dialog_layout(&pane, &workspace_path) =>
                                {
                                    send_trust_key_guarded(
                                        &pane,
                                        &workspace_path,
                                        &mut send_key,
                                        &cancel,
                                    )
                                    .await?;
                                }
                                _ => {}
                            }
                            return Ok(());
                        }
                        CursorStartupFrame::Ready => return Ok(()),
                    },
                }
            }
        }
    }
}

/// Canonical bounded Cursor trust-screen layout for tests and probes.
#[cfg(test)]
pub(crate) fn cursor_trust_dialog_frame(workspace: &str) -> String {
    format!(
        "Workspace Trust Required\n\n\
         {CURSOR_TRUST_DESCRIPTION}\n\n\
         {CURSOR_TRUST_QUESTION}\n\n\
         {workspace}\n\n\
         [a] Trust this workspace\n\
         [q] Quit\n"
    )
}

#[cfg(test)]
pub(crate) fn cursor_trust_dialog_frame_with_mcp(workspace: &str) -> String {
    format!(
        "Workspace Trust Required\n\n\
         {CURSOR_TRUST_DESCRIPTION}\n\n\
         {CURSOR_TRUST_MCP_DESCRIPTION}\n\n\
         {CURSOR_TRUST_QUESTION}\n\n\
         {workspace}\n\n\
         [a] Trust this workspace\n\
         [w] Trust this workspace, but don't enable all MCP servers\n\
         [q] Quit\n"
    )
}

async fn wait_for_input_ready(session: &str, timeout: Duration) -> Result<(), DriverError> {
    let deadline = tokio::time::Instant::now() + timeout;
    let mut poll = tokio::time::interval(Duration::from_millis(250));
    poll.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    poll.tick().await; // first tick is immediate; skip it
    loop {
        tokio::select! {
            _ = tokio::time::sleep_until(deadline) => {
                return Err(DriverError::InputNotReady(timeout));
            }
            _ = poll.tick() => {
                if let Ok(pane) = capture_pane(session).await {
                    // Accept the folder-trust dialog (default selection is
                    // "Yes, proceed") so the harness reaches its composer.
                    if pane_requests_folder_trust(&pane) {
                        let _ = send_keys(session, &[String::from("Enter")], None, None).await;
                        continue;
                    }
                    if pane_has_input_prompt(&pane) {
                        return Ok(());
                    }
                }
            }
        }
    }
}

#[cfg(test)]
async fn wait_for_input_ready_with_capture<C, Fut>(
    timeout: Duration,
    poll_interval: Duration,
    mut capture: C,
) -> Result<(), DriverError>
where
    C: FnMut() -> Fut,
    Fut: std::future::Future<Output = Result<String, DriverError>>,
{
    let deadline = tokio::time::Instant::now() + timeout;
    let mut poll = tokio::time::interval(poll_interval);
    poll.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    poll.tick().await; // first tick is immediate; skip it

    loop {
        tokio::select! {
            _ = tokio::time::sleep_until(deadline) => {
                return Err(DriverError::InputNotReady(timeout));
            }
            _ = poll.tick() => {
                if let Ok(pane) = capture().await {
                    if pane_has_input_prompt(&pane) {
                        return Ok(());
                    }
                }
            }
        }
    }
}

pub(crate) fn pane_has_input_prompt(pane: &str) -> bool {
    pane.lines().any(|line| {
        let line = strip_ansi_codes(line);
        let is_prompt = line_starts_with_prompt(&line, "❯") || line_starts_with_prompt(&line, ">");
        // A `❯`/`>` line can also be the *selected* item of a numbered
        // selection menu (e.g. Claude's "Is this a project you trust?" dialog
        // renders `❯ 1. Yes, proceed`). Those are NOT the harness composer
        // prompt; treating them as ready would paste the dispatch brief into
        // the menu's filter/selector. Require a real composer prompt.
        is_prompt && !line_is_numbered_menu_item(&line)
    })
}

/// Cursor composer readiness: bounded to the harness input component (`❯`
/// at column zero), not generic `>` blockquotes from prompt/model output.
#[cfg(test)]
pub(crate) fn pane_has_cursor_composer_ready(pane: &str) -> bool {
    pane.lines().any(|line| {
        let line = strip_ansi_codes(line);
        line_starts_with_prompt(&line, "❯") && !line_is_numbered_menu_item(&line)
    })
}

fn line_starts_with_prompt(line: &str, marker: &str) -> bool {
    line.strip_prefix(marker)
        .and_then(|rest| rest.chars().next())
        .map(char::is_whitespace)
        .unwrap_or(false)
}

/// Whether a `❯`/`>`-prefixed line is the selected item of a numbered menu
/// (`❯ 1. Yes`, `> 2. No, …`) rather than the harness composer prompt.
fn line_is_numbered_menu_item(line: &str) -> bool {
    let rest = line
        .strip_prefix('❯')
        .or_else(|| line.strip_prefix('>'))
        .map(str::trim_start)
        .unwrap_or("");
    let mut chars = rest.chars().peekable();
    let mut saw_digit = false;
    while chars.peek().is_some_and(|c| c.is_ascii_digit()) {
        saw_digit = true;
        chars.next();
    }
    // `❯ 1. …` or `❯ 1) …` — a digit run terminated by `.`/`)`.
    saw_digit && matches!(chars.next(), Some('.') | Some(')'))
}

/// Whether the pane is showing Claude's folder-trust dialog ("Do you trust the
/// files in this folder?") as a numbered menu. Shared by the tmux driver. Used
/// to accept the dialog (default "Yes, proceed") so a fresh worktree's harness
/// reaches its composer instead of stranding the dispatch.
pub(crate) fn pane_requests_folder_trust(pane: &str) -> bool {
    let lower = pane.to_ascii_lowercase();
    let mentions_trust = lower.contains("do you trust")
        || lower.contains("trust the files")
        || lower.contains("trust this folder");
    mentions_trust
        && pane
            .lines()
            .any(|line| line_is_numbered_menu_item(&strip_ansi_codes(line)))
}

async fn emit_fatal_driver_error_once(
    events: &mpsc::Sender<DriverEvent>,
    terminal_emitted: &AtomicBool,
    message: String,
) {
    if !terminal_emitted.swap(true, Ordering::SeqCst) {
        let _ = events
            .send(DriverEvent::DriverError {
                fatal: true,
                message,
            })
            .await;
    }
}

pub(crate) fn strip_ansi_codes(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    let mut chars = input.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '\x1b' {
            match chars.peek().copied() {
                Some('[') => {
                    chars.next();
                    while let Some(&nc) = chars.peek() {
                        chars.next();
                        let code = nc as u32;
                        if (0x40..=0x7e).contains(&code) {
                            break;
                        }
                    }
                }
                Some(']') => {
                    chars.next();
                    while let Some(&nc) = chars.peek() {
                        if nc == '\x07' {
                            chars.next();
                            break;
                        }
                        if nc == '\x1b' {
                            chars.next();
                            if chars.peek().copied() == Some('\\') {
                                chars.next();
                            }
                            break;
                        }
                        chars.next();
                    }
                }
                Some(_) => {
                    chars.next();
                }
                None => {}
            }
            continue;
        }
        let code = c as u32;
        if code < 0x20 && c != '\n' && c != '\t' && c != '\r' {
            continue;
        }
        if code == 0x7f {
            continue;
        }
        out.push(c);
    }
    out.replace('\r', "")
}

async fn kill_tmux_session(session: &str) {
    let _ = tmux_async_command()
        .args(["kill-session", "-t", session])
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .await;
}

fn kill_tmux_session_sync(session: &str) {
    let _ = tmux_command()
        .args(["kill-session", "-t", session])
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status();
}

/// Convenience constructor for tests + supervisor smoke runs.
pub fn driver() -> TmuxDriver {
    TmuxDriver::new(Box::new(crate::adapters::ClaudeAdapter::new()))
}

/// Inert-mode config that drivers can use when they need a session without
/// actually exec'ing anything (smoke tests, missing tmux).
pub fn inert_config() -> DriverConfig {
    DriverConfig::from_value(json!({"force_inert": true}))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::r#trait::RunKind;
    use crate::test_tooling::{
        assert_required_test_tooling, command_succeeds, skip_test_if_missing,
        test_environment_lock, ToolRequirement,
    };
    use std::collections::VecDeque;
    #[cfg(unix)]
    use std::os::unix::fs::PermissionsExt;
    use std::sync::atomic::AtomicUsize;
    use std::sync::Mutex;

    use crate::test_tooling::live_session_guard;

    /// Drop-guard that kills a real tmux session on every exit path — success,
    /// assert-failure unwinding, or panic. Real-tmux tests assert pane/session
    /// state *before* they call `release`, so without this guard a failed assert
    /// would leak an `orgasmic-…` session behind (TASK-095.3). Holding the guard
    /// for the lifetime of the test makes cleanup unconditional; the synchronous
    /// `kill-session` is a no-op if the session is already gone.
    struct SessionGuard(String);

    impl Drop for SessionGuard {
        fn drop(&mut self) {
            kill_tmux_session_sync(&self.0);
        }
    }

    #[test]
    fn tmux_session_probe_distinguishes_absence_from_client_failure() {
        assert_eq!(
            classify_tmux_session_observation(true, Some(0), ""),
            TmuxSessionObservation::Present
        );
        assert_eq!(
            classify_tmux_session_observation(false, Some(1), "can't find session: gone"),
            TmuxSessionObservation::Absent
        );
        for (code, stderr) in [
            (Some(1), "no server running on /tmp/tmux-501/default"),
            (Some(2), "tmux: bad option"),
            (None, "signal"),
        ] {
            assert_eq!(
                classify_tmux_session_observation(false, code, stderr),
                TmuxSessionObservation::Unobserved,
                "status={code:?}, stderr={stderr:?} must not read as a dead pane"
            );
        }
    }

    async fn tmux_spawn_usable() -> bool {
        if !tmux_available() {
            return false;
        }
        // orgasmic:TASK-0RCRY
        // Every tmux-gated test in this binary funnels through this probe, so
        // this is the one place that has to claim the owned server — and it
        // claims it *before* the first session is created, which is what keeps
        // a probe session off the operator's server.
        own_tmux_server_for_tests();
        let session = format!(
            "orgasmic-test-probe-{}",
            chrono::Utc::now().timestamp_nanos_opt().unwrap_or(0)
        );
        let status = tmux_async_command()
            .args(["new-session", "-d", "-s", &session, "--", "sleep", "1"])
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .await;
        let ok = status.map(|status| status.success()).unwrap_or(false);
        if ok {
            kill_tmux_session(&session).await;
        }
        ok
    }

    async fn tmux_and_command_available(command: Option<&str>) -> (bool, bool) {
        let _environment = test_environment_lock().lock().await;
        (
            tmux_spawn_usable().await,
            command.is_none_or(command_available),
        )
    }

    #[tokio::test]
    async fn required_test_tooling_is_present() {
        let _live_guard = live_session_guard();
        let _environment = test_environment_lock().lock().await;
        assert_required_test_tooling(&[
            // orgasmic:TASK-4CSMY — +1 for the real-pane proof of the tmux
            // pane-liveness channel. The two live smokes beside it carry
            // `#[ignore]` and never run from the default suite, so counting
            // them here would overstate what a green run covered.
            ToolRequirement::new("tmux", 10, tmux_spawn_usable().await),
            ToolRequirement::new("sleep", 1, command_available("sleep")),
            ToolRequirement::new("bash", 1, command_available("bash")),
            ToolRequirement::new("claude", 8, command_succeeds("claude", &["--version"])),
            ToolRequirement::new("codex", 1, command_available("codex")),
        ]);
    }

    fn ctx(run_id: &str, kind: RunKind) -> DriverContext {
        DriverContext {
            identity: RuntimeIdentity::new(run_id, "boot-test"),
            run_kind: kind,
            task_id: "TASK-006".into(),
            worker_id: "implementer-claude-tmux".into(),
            project_id: Some("orgasmic".into()),
            worktree: None,
        }
    }

    #[tokio::test]
    async fn inert_acquire_emits_ready_without_synthetic_terminal_on_release() {
        let d = driver();
        let mut s = d
            .acquire(ctx("run-1", RunKind::Worker), inert_config())
            .await
            .unwrap();
        let ev = s.events.recv().await.unwrap();
        assert!(matches!(ev, DriverEvent::Ready { .. }));
        s.control.release("done").await.unwrap();
        assert!(
            s.events.recv().await.is_none(),
            "control-plane release must close without manufacturing a terminal provider event"
        );
    }

    #[tokio::test]
    async fn inert_acquire_with_prompt_bundle_stays_inert() {
        let d = driver();
        let cfg = DriverConfig::from_value(json!({
            "force_inert": true,
            "prompt_bundle_text": "manager prompt",
        }));
        let mut s = d
            .acquire(ctx("run-prompt-inert", RunKind::Worker), cfg)
            .await
            .unwrap();
        let ev = s.events.recv().await.unwrap();
        let DriverEvent::Ready { capabilities, .. } = ev else {
            panic!("expected Ready");
        };
        assert_eq!(capabilities["inert"], true);
        assert_eq!(capabilities["inert_reason"], "force_inert");
        s.control.release("done").await.unwrap();
    }

    #[test]
    fn claude_spawn_plan_uses_model_and_dangerous_permissions() {
        let cfg = TmuxTuiConfig {
            harness: Some("claude".into()),
            model: Some("claude-sonnet-4-6".into()),
            effort: Some("high".into()),
            prompt_bundle_text: Some("do the task".into()),
            ..TmuxTuiConfig::default()
        };
        let plan = build_spawn_plan(&cfg, &ctx("run-plan", RunKind::Worker), "claude");
        assert_eq!(plan.command, "claude");
        assert!(plan
            .args
            .iter()
            .any(|arg| arg == "--dangerously-skip-permissions"));
        assert!(plan
            .args
            .windows(2)
            .any(|pair| pair == ["--model", "claude-sonnet-4-6"]));
        assert!(plan
            .args
            .windows(2)
            .any(|pair| pair == ["--effort", "high"]));
        // Argv delivery: prompt is one trailing argv element after `--`.
        assert!(plan.paste_prompt.is_none());
        assert!(plan
            .args
            .windows(2)
            .any(|pair| pair == ["--", "do the task"]));
        assert!(!plan.args.iter().any(|arg| arg.contains("orgasmic-eot")));
        assert!(!plan
            .args
            .iter()
            .any(|arg| arg.contains("end-of-turn marker")));
    }

    #[test]
    fn pty_model_and_effort_preserve_exact_option_bytes() {
        let cfg = TmuxTuiConfig {
            harness: Some("claude".into()),
            model: Some("  custom-model  ".into()),
            effort: Some(" XHIGH ".into()),
            ..TmuxTuiConfig::default()
        };
        let plan = build_spawn_plan(&cfg, &ctx("run-verbatim", RunKind::Worker), "claude");
        assert!(plan
            .args
            .windows(2)
            .any(|pair| pair == ["--model", "  custom-model  "]));
        assert!(plan
            .args
            .windows(2)
            .any(|pair| pair == ["--effort", " XHIGH "]));
    }

    #[test]
    fn supported_harnesses_deliver_prompt_as_single_argv_element() {
        // Quotes, newlines, metacharacters, Unicode, and leading dashes must
        // remain one argv element — never shell-concatenated.
        let nasty = "line1\n\"quoted\" $HOME; `--flag` — café";
        for harness in ["claude", "codex", "cursor-agent"] {
            let cfg = TmuxTuiConfig {
                harness: Some(harness.into()),
                prompt_bundle_text: Some(nasty.into()),
                ..TmuxTuiConfig::default()
            };
            let plan = build_spawn_plan(&cfg, &ctx("run-argv", RunKind::Worker), harness);
            assert!(
                plan.paste_prompt.is_none(),
                "{harness} should use argv delivery"
            );
            assert_eq!(plan.args[plan.args.len() - 2], "--");
            assert_eq!(plan.args[plan.args.len() - 1], nasty);
            let native = plan.native_runtime.expect("native meta");
            assert_eq!(native.launch_argv.last().map(String::as_str), Some(nasty));
        }
    }

    #[test]
    fn hermes_and_custom_keep_paste_fallback_without_eot() {
        let hermes_cfg = TmuxTuiConfig {
            harness: Some("hermes".into()),
            prompt_bundle_text: Some("do the task".into()),
            ..TmuxTuiConfig::default()
        };
        let hermes = build_spawn_plan(&hermes_cfg, &ctx("run-hermes", RunKind::Worker), "hermes");
        assert_eq!(hermes.paste_prompt.as_deref(), Some("do the task"));
        assert!(!hermes.args.iter().any(|arg| arg == "do the task"));
        assert!(!hermes
            .paste_prompt
            .as_deref()
            .unwrap()
            .contains("orgasmic-eot"));
    }

    /// Worker `:HARNESS_ARGS:` ride along on the harness argv; a `--model`
    /// there comes after the worker-default model flag, so the CLI's
    /// last-flag-wins semantics give user args precedence.
    #[test]
    fn claude_spawn_plan_appends_harness_args() {
        let cfg = TmuxTuiConfig {
            harness: Some("claude".into()),
            model: Some("claude-sonnet-4-6".into()),
            harness_args: vec!["--betas".into(), "context-1m".into()],
            prompt_bundle_text: Some("do the task".into()),
            ..TmuxTuiConfig::default()
        };
        let plan = build_spawn_plan(&cfg, &ctx("run-plan", RunKind::Worker), "claude");
        assert_eq!(plan.command, "claude");
        assert!(plan
            .args
            .windows(2)
            .any(|pair| pair == ["--betas", "context-1m"]));
        assert!(plan
            .args
            .windows(2)
            .any(|pair| pair == ["--model", "claude-sonnet-4-6"]));
    }

    #[test]
    fn claude_spawn_plan_records_deterministic_native_runtime() {
        let cfg = TmuxTuiConfig {
            harness: Some("claude".into()),
            ..TmuxTuiConfig::default()
        };
        let c = ctx("run-native", RunKind::Worker);
        let runtime_id = c.identity.runtime_id.clone();
        let plan = build_spawn_plan(&cfg, &c, "claude");
        // The launch argv pins --session-id to the runtime UUID.
        assert!(plan
            .args
            .windows(2)
            .any(|pair| pair == ["--session-id", runtime_id.as_str()]));
        let native = plan.native_runtime.expect("claude native metadata");
        assert_eq!(native.provider, "claude");
        assert_eq!(native.session_id.as_deref(), Some(runtime_id.as_str()));
        // Resume forks the prior conversation deterministically (dec_052).
        assert_eq!(
            native.resume_argv,
            vec![
                "claude".to_string(),
                "--resume".to_string(),
                runtime_id.clone(),
                "--fork-session".to_string(),
                "--dangerously-skip-permissions".to_string(),
            ]
        );
    }

    #[test]
    fn native_resume_spawn_plan_defers_fork_session_id_until_discovery() {
        let cfg = TmuxTuiConfig {
            harness: Some("claude".into()),
            native_resume_mode: true,
            command: Some("/trusted/claude".into()),
            args: vec![
                "--resume".into(),
                "origin-session-id".into(),
                "--fork-session".into(),
            ],
            ..TmuxTuiConfig::default()
        };
        let plan = build_spawn_plan(&cfg, &ctx("run-fork", RunKind::Worker), "claude");
        let native = plan.native_runtime.expect("pending fork metadata");
        assert!(native.session_id.is_none());
        assert_eq!(
            native.resume_argv.get(2).map(String::as_str),
            Some("origin-session-id")
        );
        assert_eq!(
            deterministic_inert_fork_session_id("rt-fork"),
            "fork-rt-fork"
        );
    }

    struct HomeGuard(Option<std::ffi::OsString>);

    static FORK_DISCOVERY_TEST_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    impl HomeGuard {
        fn set(home: &std::path::Path) -> Self {
            let previous = std::env::var_os("HOME");
            std::env::set_var("HOME", home);
            Self(previous)
        }
    }

    impl Drop for HomeGuard {
        fn drop(&mut self) {
            match self.0.take() {
                Some(path) => std::env::set_var("HOME", path),
                None => std::env::remove_var("HOME"),
            }
        }
    }

    fn with_home<F: FnOnce()>(home: &std::path::Path, f: F) {
        let _lock = FORK_DISCOVERY_TEST_LOCK
            .lock()
            .expect("fork discovery test lock");
        let _guard = HomeGuard::set(home);
        f();
    }

    fn touch_claude_fork_jsonl(
        home: &std::path::Path,
        cwd: &std::path::Path,
        session_id: &str,
        modified: std::time::SystemTime,
    ) -> std::path::PathBuf {
        let dir = super::claude_projects_dir_with_home(cwd, Some(home)).expect("projects dir");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join(format!("{session_id}.jsonl"));
        std::fs::write(
            &path,
            serde_json::to_string(&json!({
                "sessionId": session_id,
                "cwd": cwd,
                "forkedFrom": {"sessionId": "origin-session"},
            }))
            .unwrap()
                + "\n",
        )
        .unwrap();
        filetime::set_file_mtime(&path, filetime::FileTime::from_system_time(modified)).unwrap();
        path
    }

    #[test]
    fn fork_discovery_returns_unique_confined_candidate() {
        let tmp = tempfile::tempdir().unwrap();
        with_home(tmp.path(), || {
            let cwd = tmp.path().join("repo");
            std::fs::create_dir_all(&cwd).unwrap();
            let since = std::time::SystemTime::now() - Duration::from_millis(50);
            touch_claude_fork_jsonl(tmp.path(), &cwd, "fork-unique", since);
            let result = super::discover_claude_fork_session_id("origin-session", &cwd, since);
            assert_eq!(
                result,
                super::ForkDiscoveryResult::Unique("fork-unique".into())
            );
        });
    }

    #[test]
    fn fork_discovery_rejects_filename_only_without_provider_metadata() {
        let tmp = tempfile::tempdir().unwrap();
        with_home(tmp.path(), || {
            let cwd = tmp.path().join("repo");
            std::fs::create_dir_all(&cwd).unwrap();
            let since = std::time::SystemTime::now() - Duration::from_millis(50);
            let dir = super::claude_projects_dir(&cwd).unwrap();
            std::fs::create_dir_all(&dir).unwrap();
            std::fs::write(dir.join("filename-only.jsonl"), "{}\n").unwrap();
            let result = super::discover_claude_fork_session_id("origin-session", &cwd, since);
            assert_eq!(result, super::ForkDiscoveryResult::NotFound);
        });
    }

    #[test]
    fn fork_discovery_rejects_wrong_resumed_parent() {
        let tmp = tempfile::tempdir().unwrap();
        with_home(tmp.path(), || {
            let cwd = tmp.path().join("repo");
            std::fs::create_dir_all(&cwd).unwrap();
            let since = std::time::SystemTime::now() - Duration::from_millis(50);
            let dir = super::claude_projects_dir(&cwd).unwrap();
            std::fs::create_dir_all(&dir).unwrap();
            std::fs::write(
                dir.join("wrong-parent.jsonl"),
                serde_json::to_string(&json!({
                    "sessionId": "wrong-parent",
                    "cwd": cwd,
                    "forkedFrom": {"sessionId": "another-origin"},
                }))
                .unwrap(),
            )
            .unwrap();
            let result = super::discover_claude_fork_session_id("origin-session", &cwd, since);
            assert_eq!(result, super::ForkDiscoveryResult::NotFound);
        });
    }

    #[test]
    fn fork_discovery_not_found_when_no_candidate() {
        let tmp = tempfile::tempdir().unwrap();
        with_home(tmp.path(), || {
            let cwd = tmp.path().join("repo");
            std::fs::create_dir_all(&cwd).unwrap();
            let since = std::time::SystemTime::now();
            let result = super::discover_claude_fork_session_id("origin-session", &cwd, since);
            assert_eq!(result, super::ForkDiscoveryResult::NotFound);
        });
    }

    #[test]
    fn fork_discovery_ambiguous_when_multiple_candidates() {
        let tmp = tempfile::tempdir().unwrap();
        with_home(tmp.path(), || {
            let cwd = tmp.path().join("repo");
            std::fs::create_dir_all(&cwd).unwrap();
            let since = std::time::SystemTime::now() - Duration::from_millis(50);
            touch_claude_fork_jsonl(tmp.path(), &cwd, "fork-a", since);
            touch_claude_fork_jsonl(tmp.path(), &cwd, "fork-b", since);
            let result = super::discover_claude_fork_session_id("origin-session", &cwd, since);
            assert_eq!(result, super::ForkDiscoveryResult::Ambiguous);
        });
    }

    #[test]
    fn fork_discovery_wrong_cwd_excludes_unrelated_project_dir() {
        let tmp = tempfile::tempdir().unwrap();
        with_home(tmp.path(), || {
            let cwd_a = tmp.path().join("repo-a");
            let cwd_b = tmp.path().join("repo-b");
            std::fs::create_dir_all(&cwd_a).unwrap();
            std::fs::create_dir_all(&cwd_b).unwrap();
            let since = std::time::SystemTime::now();
            touch_claude_fork_jsonl(tmp.path(), &cwd_b, "fork-other-cwd", since);
            let result = super::discover_claude_fork_session_id("origin-session", &cwd_a, since);
            assert_eq!(result, super::ForkDiscoveryResult::NotFound);
        });
    }

    #[cfg(unix)]
    #[test]
    fn fork_discovery_rejects_symlink_candidate() {
        let tmp = tempfile::tempdir().unwrap();
        with_home(tmp.path(), || {
            let cwd = tmp.path().join("repo");
            std::fs::create_dir_all(&cwd).unwrap();
            let since = std::time::SystemTime::now() - Duration::from_millis(50);
            let projects = super::claude_projects_dir(&cwd).unwrap();
            std::fs::create_dir_all(&projects).unwrap();
            let outside = tmp.path().join("outside.jsonl");
            std::fs::write(&outside, "{}\n").unwrap();
            std::os::unix::fs::symlink(&outside, projects.join("fork-symlink.jsonl")).unwrap();
            let result = super::discover_claude_fork_session_id("origin-session", &cwd, since);
            assert_eq!(result, super::ForkDiscoveryResult::NotFound);
        });
    }

    #[test]
    fn fork_discovery_excludes_name_present_before_launch() {
        let tmp = tempfile::tempdir().unwrap();
        with_home(tmp.path(), || {
            let cwd = tmp.path().join("repo");
            std::fs::create_dir_all(&cwd).unwrap();
            let before = std::time::SystemTime::now() - Duration::from_secs(2);
            let path = touch_claude_fork_jsonl(tmp.path(), &cwd, "fork-preexisting", before);
            let excluded = super::claude_fork_candidate_names(&cwd);
            let since = std::time::SystemTime::now() - Duration::from_millis(50);
            filetime::set_file_mtime(
                &path,
                filetime::FileTime::from_system_time(std::time::SystemTime::now()),
            )
            .unwrap();
            let result = super::discover_claude_fork_session_id_excluding(
                "origin-session",
                &cwd,
                since,
                &excluded,
            );
            assert_eq!(result, super::ForkDiscoveryResult::NotFound);
        });
    }

    #[test]
    fn fork_discovery_fails_closed_on_post_read_inode_swap() {
        let tmp = tempfile::tempdir().unwrap();
        with_home(tmp.path(), || {
            let cwd = tmp.path().join("repo");
            std::fs::create_dir_all(&cwd).unwrap();
            let since = std::time::SystemTime::now() - Duration::from_millis(50);
            let path = touch_claude_fork_jsonl(tmp.path(), &cwd, "fork-swap", since);
            let displaced = path.with_extension("opened");
            let replacement = path.clone();
            let cwd_for_swap = cwd.clone();
            *super::FORK_CANDIDATE_POST_READ_TEST_HOOK.lock().unwrap() =
                Some(Box::new(move |_| {
                    std::fs::rename(&replacement, &displaced).unwrap();
                    std::fs::write(
                        &replacement,
                        serde_json::to_string(&json!({
                            "sessionId": "fork-swap",
                            "cwd": cwd_for_swap,
                            "forkedFrom": {"sessionId": "origin-session"},
                        }))
                        .unwrap(),
                    )
                    .unwrap();
                }));
            let result = super::discover_claude_fork_session_id("origin-session", &cwd, since);
            assert_eq!(result, super::ForkDiscoveryResult::NotFound);
        });
    }

    #[tokio::test]
    #[allow(clippy::await_holding_lock)]
    async fn gated_launch_excludes_candidate_inserted_before_release_boundary() {
        let _live_guard = live_session_guard();
        let (tmux_available, sleep_available) = tmux_and_command_available(Some("sleep")).await;
        if skip_test_if_missing(
            "gated_launch_excludes_candidate_inserted_before_release_boundary",
            &[("tmux", tmux_available), ("sleep", sleep_available)],
        ) {
            return;
        }
        let _lock = FORK_DISCOVERY_TEST_LOCK
            .lock()
            .expect("fork discovery test lock");
        let tmp = tempfile::tempdir().unwrap();
        let cwd = tmp.path().join("repo");
        std::fs::create_dir_all(&cwd).unwrap();
        let projects = super::claude_projects_dir_with_home(&cwd, Some(tmp.path())).unwrap();
        std::fs::create_dir_all(&projects).unwrap();
        let home = tmp.path().to_path_buf();
        let cwd_for_hook = cwd.clone();
        *super::CLAUDE_PRE_RELEASE_TEST_HOOK.lock().unwrap() = Some(Box::new(move || {
            touch_claude_fork_jsonl(
                &home,
                &cwd_for_hook,
                "fork-in-old-gap",
                std::time::SystemTime::now(),
            );
        }));
        let session = format!("orgasmic-fork-gap-{}", uuid::Uuid::new_v4().simple());
        let _guard = SessionGuard(session.clone());
        let plan = TmuxSpawnPlan {
            command: "sleep".into(),
            args: vec!["2".into()],
            cwd,
            paste_prompt: None,
            native_runtime: None,
            run_id: "run-fork-gap".into(),
            runtime_id: "runtime-fork-gap".into(),
            boot_id: "boot-test".into(),
            manager_terminal_capability: None,
            harness_env: Vec::new(),
            native_resume_mode: true,
            trusted_provider_identity: Some("claude".into()),
            pinned_executable: None,
            provider_home: Some(tmp.path().to_path_buf()),
        };
        let observation = spawn_tmux_session(&session, &plan)
            .await
            .unwrap()
            .expect("gated observation");
        assert!(observation.excluded.contains("fork-in-old-gap"));
        let result = super::discover_claude_fork_session_id_in_directory(
            "origin-session",
            observation.since,
            &observation.excluded,
            &observation.directory,
        );
        assert_eq!(result, super::ForkDiscoveryResult::NotFound);
        kill_tmux_session(&session).await;
    }

    #[test]
    fn fork_discovery_accepts_candidate_created_after_initial_wait() {
        let tmp = tempfile::tempdir().unwrap();
        with_home(tmp.path(), || {
            let cwd = tmp.path().join("repo");
            std::fs::create_dir_all(&cwd).unwrap();
            let since = std::time::SystemTime::now() - Duration::from_millis(50);
            let delayed = since + Duration::from_millis(900);
            touch_claude_fork_jsonl(tmp.path(), &cwd, "fork-delayed", delayed);
            let result = super::discover_claude_fork_session_id("origin-session", &cwd, since);
            assert_eq!(
                result,
                super::ForkDiscoveryResult::Unique("fork-delayed".into())
            );
        });
    }

    #[tokio::test]
    #[allow(clippy::await_holding_lock)] // serializes process-global HOME for the full async probe
    async fn fork_discovery_polls_until_delayed_candidate_within_launch_bounds() {
        let _lock = FORK_DISCOVERY_TEST_LOCK
            .lock()
            .expect("fork discovery test lock");
        let tmp = tempfile::tempdir().unwrap();
        let _home_guard = HomeGuard::set(tmp.path());
        let cwd = tmp.path().join("repo");
        std::fs::create_dir_all(&cwd).unwrap();
        let projects = super::claude_projects_dir(&cwd).unwrap();
        std::fs::create_dir_all(&projects).unwrap();
        let directory = super::ClaudeProjectsDirectory::open(&cwd, None).unwrap();
        let since = std::time::SystemTime::now();
        let delayed = since + Duration::from_millis(900);
        let cwd_for_delay = cwd.clone();
        let home_for_delay = tmp.path().to_path_buf();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(900)).await;
            touch_claude_fork_jsonl(&home_for_delay, &cwd_for_delay, "fork-late", delayed);
        });
        let result = super::wait_for_claude_fork_session_id(
            "origin-session",
            since,
            &Default::default(),
            &directory,
        )
        .await;
        assert_eq!(
            result,
            super::ForkDiscoveryResult::Unique("fork-late".into())
        );
    }

    #[test]
    fn non_claude_spawn_plan_records_only_launch_metadata() {
        let cfg = TmuxTuiConfig {
            harness: Some("codex".into()),
            ..TmuxTuiConfig::default()
        };
        let plan = build_spawn_plan(&cfg, &ctx("run-codex", RunKind::Worker), "codex");
        let native = plan.native_runtime.expect("native metadata present");
        assert_eq!(native.provider, "codex");
        assert!(native.session_id.is_none());
        assert!(native.resume_argv.is_empty());
        assert_eq!(
            native.launch_argv.first().map(String::as_str),
            Some("codex")
        );
    }

    #[test]
    fn dispatch_placeholder_does_not_override_claude_default_command() {
        let cfg = TmuxTuiConfig {
            command: Some("sh".into()),
            args: vec![
                "-lc".into(),
                "echo orgasmic pipeline stage acquired; exec sh".into(),
            ],
            harness: Some("claude".into()),
            model: Some("claude-sonnet-4-6".into()),
            ..TmuxTuiConfig::default()
        };
        let plan = build_spawn_plan(&cfg, &ctx("run-placeholder", RunKind::Worker), "claude");
        assert_eq!(plan.command, "claude");
        assert!(plan
            .args
            .windows(2)
            .any(|pair| pair == ["--model", "claude-sonnet-4-6"]));
    }

    /// Same stamp as tmux: the tmux mode launches the identical codex TUI, so
    /// it must record the identical originator (TASK-GT91X).
    // orgasmic:TASK-GT91X
    #[test]
    fn codex_tmux_pane_exports_transcript_finder_originator() {
        let cfg = TmuxTuiConfig {
            harness: Some("codex".into()),
            ..TmuxTuiConfig::default()
        };
        let plan = build_spawn_plan(&cfg, &ctx("run-codex-originator", RunKind::Worker), "codex");
        assert!(
            plan.harness_env
                .iter()
                .any(|(key, value)| key == crate::CODEX_ORIGINATOR_ENV
                    && value == crate::CODEX_ORIGINATOR),
            "codex tmux pane must export {}={} or its transcript is unreachable; got {:?}",
            crate::CODEX_ORIGINATOR_ENV,
            crate::CODEX_ORIGINATOR,
            plan.harness_env
        );

        for harness in ["claude", "cursor-agent"] {
            let cfg = TmuxTuiConfig {
                harness: Some(harness.into()),
                ..TmuxTuiConfig::default()
            };
            let plan = build_spawn_plan(&cfg, &ctx("run-other", RunKind::Worker), harness);
            assert!(plan.harness_env.is_empty(), "{harness} needs no stamp");
        }
    }

    #[test]
    fn dispatch_placeholder_swaps_to_codex_default_command() {
        // Regression: the placeholder-swap gate was claude-only, so codex
        // workers ran the placeholder `sh` verbatim and the prompt was typed
        // into a bare shell. The daemon sentinel must swap to real `codex`.
        let cfg = TmuxTuiConfig {
            command: Some("sh".into()),
            args: vec![
                "-lc".into(),
                "echo orgasmic pipeline stage acquired; exec sh".into(),
            ],
            harness: Some("codex".into()),
            ..TmuxTuiConfig::default()
        };
        let plan = build_spawn_plan(
            &cfg,
            &ctx("run-codex-placeholder", RunKind::Worker),
            "codex",
        );
        assert_eq!(plan.command, "codex");
        assert!(!is_dispatch_placeholder(
            Some(plan.command.as_str()),
            &plan.args
        ));
    }

    #[test]
    fn prompt_bytes_preserved_with_leading_trailing_whitespace() {
        let bundle = "\n  do the task  \n";
        for harness in ["claude", "codex", "cursor-agent"] {
            let cfg = TmuxTuiConfig {
                harness: Some(harness.into()),
                prompt_bundle_text: Some(bundle.to_string()),
                ..TmuxTuiConfig::default()
            };
            let plan = build_spawn_plan(&cfg, &ctx("run-bytes", RunKind::Worker), harness);
            assert_eq!(plan.args.last().map(String::as_str), Some(bundle));
            assert_eq!(plan.paste_prompt.as_deref(), None);
        }
        let hermes_cfg = TmuxTuiConfig {
            harness: Some("hermes".into()),
            prompt_bundle_text: Some(bundle.to_string()),
            ..TmuxTuiConfig::default()
        };
        let hermes = build_spawn_plan(
            &hermes_cfg,
            &ctx("run-hermes-bytes", RunKind::Worker),
            "hermes",
        );
        assert_eq!(hermes.paste_prompt.as_deref(), Some(bundle));
    }

    #[test]
    fn tmux_config_defaults_input_ready_timeout_to_ten_seconds() {
        let cfg: TmuxTuiConfig = serde_json::from_value(json!({})).unwrap();
        assert_eq!(cfg.input_ready_timeout, Duration::from_secs(10));
    }

    #[test]
    fn pane_has_cursor_composer_ready_rejects_markdown_blockquote() {
        assert!(!pane_has_cursor_composer_ready(
            "model output\n> quoted line\n"
        ));
        assert!(pane_has_cursor_composer_ready("cursor-agent\n❯ \n"));
    }

    #[test]
    fn pane_has_input_prompt_detects_claude_indicators() {
        assert!(pane_has_input_prompt("banner\n❯ \nfooter"));
        assert!(pane_has_input_prompt("banner\n❯\u{00a0}\nfooter"));
        assert!(pane_has_input_prompt("banner\n> \nfooter"));
        assert!(!pane_has_input_prompt("banner\n  ❯ \nfooter"));
        assert!(!pane_has_input_prompt("banner\nno prompt\nfooter"));
    }

    #[test]
    fn pane_has_input_prompt_rejects_numbered_trust_menu() {
        // Claude's folder-trust dialog renders the selected option as a
        // numbered menu item; it must not be mistaken for the composer prompt
        // or the dispatch brief lands in the trust selector (live regression).
        let trust = "Do you trust the files in this folder?\n\n❯ 1. Yes, proceed\n  2. No, exit\n";
        assert!(!pane_has_input_prompt(trust));
        assert!(!pane_has_input_prompt("❯ 2) No"));
        // But the real composer prompt (no numbered item) is still detected.
        assert!(pane_has_input_prompt("❯ 1. Yes\n❯ "));
    }

    #[test]
    fn automated_wake_composer_gate_requires_current_provider_input_component() {
        // Captured idle Claude and Codex panes: both end in an empty viewport
        // row plus the current prompt line. A bare glyph is intentionally not
        // enough evidence — it may be scrollback or a menu selection.
        let claude_idle = "Claude Code 2.1\n\n❯ ";
        let codex_idle = "OpenAI Codex\n\n› ";
        let claude_busy = "Claude Code 2.1\n\n✻ Thinking…\n\n";
        let codex_busy = "OpenAI Codex\n\nworking…\n\n";
        assert!(provider_composer_ready(claude_idle, "claude"));
        assert!(provider_composer_ready(codex_idle, "codex"));
        assert!(!provider_composer_ready(claude_busy, "claude"));
        assert!(!provider_composer_ready(codex_busy, "codex"));
        assert!(!provider_composer_ready("❯ ", "claude"));
        assert!(!provider_composer_ready("\n❯ 1. Yes, proceed", "claude"));
        assert!(!provider_composer_ready("\n› 1. Run", "codex"));
        assert!(!provider_composer_ready("zsh prompt\n\n% ", "codex"));
        assert!(!provider_composer_ready("❯ ", "unknown"));
        assert!(
            !provider_composer_ready("old output\n❯ \n\n", "claude"),
            "a historical glyph cannot satisfy the bottom-input gate"
        );
        assert!(
            !provider_composer_ready("old output\n\n❯ human draft", "claude"),
            "never paste over a human draft"
        );
    }

    #[test]
    fn automated_wake_target_loss_is_typed_unavailable() {
        let transport = DriverError::Transport("tmux paste-buffer failed: no such pane".into());
        assert!(matches!(
            normalize_manager_wake_target_loss_probe(Ok(false), transport),
            DriverError::ManagerWakeUnavailable
        ));
        let transport = DriverError::Transport("tmux capture-pane failed".into());
        assert!(matches!(
            normalize_manager_wake_target_loss_probe(
                Err(DriverError::Transport("gone".into())),
                transport
            ),
            DriverError::ManagerWakeUnavailable
        ));
        let provider = DriverError::ManagerWakeProviderMismatch;
        assert!(matches!(
            normalize_manager_wake_target_loss_probe(Ok(false), provider),
            DriverError::ManagerWakeProviderMismatch
        ));
    }

    #[test]
    fn pane_requests_folder_trust_matches_claude_dialog() {
        let trust = "Do you trust the files in this folder?\n\n❯ 1. Yes, proceed\n  2. No, exit\n";
        assert!(pane_requests_folder_trust(trust));
        // No numbered menu → not the trust dialog (just prose mentioning trust).
        assert!(!pane_requests_folder_trust("we trust the files here"));
        // A plain composer prompt is not the trust dialog.
        assert!(!pane_requests_folder_trust("Claude Code\n❯ "));
    }

    #[test]
    fn is_cursor_trust_dialog_layout_matches_bounded_dialog() {
        let workspace = "/tmp/worktree";
        let trust = cursor_trust_dialog_frame(workspace);
        assert!(is_cursor_trust_dialog_layout(&trust, workspace));
        assert!(is_cursor_trust_dialog_layout(
            &cursor_trust_dialog_frame_with_mcp(workspace),
            workspace
        ));
        assert!(!is_cursor_trust_dialog_layout(
            "cursor-agent ready\n❯ ",
            workspace
        ));
        assert!(!is_cursor_trust_dialog_layout(
            "Workspace Trust Required\n\n[a] Trust this workspace\n",
            workspace
        ));
        assert!(!is_cursor_trust_dialog_layout(
            "prompt: Workspace Trust Required — choose [a] Trust this workspace now",
            workspace
        ));
    }

    #[test]
    fn classify_cursor_startup_frame_rejects_partial_trust_phrases_and_blockquotes() {
        let workspace = "/tmp/worktree";
        assert_eq!(
            classify_cursor_startup_frame(
                "Workspace Trust Required\n\n[a] Trust this workspace\n",
                workspace
            ),
            CursorStartupFrame::Ready,
            "partial two-line trust prose must not trigger trust; first stable frame exits"
        );
        assert_eq!(
            classify_cursor_startup_frame("model working\n> blockquote line\n", workspace),
            CursorStartupFrame::Ready,
            "first stable non-trust frame terminates startup handling"
        );
        assert_eq!(
            classify_cursor_startup_frame(&cursor_trust_dialog_frame(workspace), workspace),
            CursorStartupFrame::TrustDialog
        );
    }

    #[test]
    fn classify_cursor_startup_frame_rejects_scattered_trust_lines() {
        let workspace = "/tmp/worktree";
        let hostile = "Workspace Trust Required\nmodel output\n\
                        [a] Trust this workspace\nmore output\n\
                        Do you trust the contents of this directory?\n\
                        /tmp/worktree\n[q] Quit\n";
        assert_eq!(
            classify_cursor_startup_frame(hostile, workspace),
            CursorStartupFrame::Ready,
            "unordered scattered trust lines must not trigger trust input"
        );
    }

    #[test]
    fn classify_cursor_startup_frame_rejects_glyph_without_trust_component() {
        let workspace = "/tmp/worktree";
        let prompt = "TASK-756WX fix round 2: Workspace Trust Required\n\n\
                      [a] Trust this workspace in the brief\n\n❯ ";
        assert_eq!(
            classify_cursor_startup_frame(prompt, workspace),
            CursorStartupFrame::Ready,
            "column-zero glyph without bounded trust component must not defer trust handling"
        );
    }

    #[test]
    fn classify_cursor_startup_frame_rejects_wrong_workspace_path() {
        let trust = cursor_trust_dialog_frame("/tmp/other-worktree");
        assert_eq!(
            classify_cursor_startup_frame(&trust, "/tmp/worktree"),
            CursorStartupFrame::Ready,
            "trust dialog with mismatched workspace path must not trigger trust input"
        );
    }

    #[tokio::test]
    async fn accept_cursor_workspace_trust_sends_a_without_pasting_prompt() {
        let trust = cursor_trust_dialog_frame("/tmp/worktree");
        let ready = "cursor-agent\n❯ \n";
        let mut panes =
            VecDeque::from([Ok(trust.clone()), Ok(trust.clone()), Ok(ready.to_string())]);
        let mut sent = Vec::new();
        let result = accept_cursor_workspace_trust_with_capture(
            "/tmp/worktree",
            Duration::from_millis(50),
            Duration::from_millis(1),
            || {
                let pane = panes.pop_front().unwrap_or_else(|| Ok(ready.to_string()));
                async move { pane }
            },
            || async { true },
            |key: &str| {
                sent.push(key.to_string());
                async { Ok(()) }
            },
            None,
        )
        .await;
        assert!(result.is_ok());
        assert_eq!(sent, vec!["a"], "trust gate must accept with [a] only");
    }

    #[tokio::test]
    async fn accept_cursor_workspace_trust_skips_send_when_frame_transitions() {
        let trust = cursor_trust_dialog_frame("/tmp/worktree");
        let ready = "cursor-agent\n❯ \n";
        let mut panes = VecDeque::from([Ok(trust.clone()), Ok(ready.to_string())]);
        let mut sent = Vec::new();
        let result = accept_cursor_workspace_trust_with_capture(
            "/tmp/worktree",
            Duration::from_millis(50),
            Duration::from_millis(1),
            || {
                let pane = panes.pop_front().unwrap_or_else(|| Ok(ready.to_string()));
                async move { pane }
            },
            || async { true },
            |key: &str| {
                sent.push(key.to_string());
                async { Ok(()) }
            },
            None,
        )
        .await;
        assert!(result.is_ok());
        assert!(
            sent.is_empty(),
            "must not send after trust frame transitions"
        );
    }

    #[tokio::test]
    async fn accept_cursor_workspace_trust_no_send_when_pane_transitions_during_blocked_send() {
        let trust = cursor_trust_dialog_frame("/tmp/worktree");
        let ready = "cursor-agent\n❯ \n";
        let pane_state = Arc::new(Mutex::new(trust.clone()));
        let capture_count = Arc::new(AtomicUsize::new(0));
        let second_capture_blocked = Arc::new(tokio::sync::Notify::new());
        let sent = Arc::new(Mutex::new(Vec::new()));
        let accept = accept_cursor_workspace_trust_with_capture(
            "/tmp/worktree",
            Duration::from_millis(200),
            Duration::from_millis(1),
            {
                let pane_state = pane_state.clone();
                let capture_count = capture_count.clone();
                let second_capture_blocked = second_capture_blocked.clone();
                move || {
                    let pane_state = pane_state.clone();
                    let capture_count = capture_count.clone();
                    let second_capture_blocked = second_capture_blocked.clone();
                    async move {
                        let n = capture_count.fetch_add(1, Ordering::SeqCst);
                        if n == 1 {
                            second_capture_blocked.notified().await;
                        }
                        Ok(pane_state.lock().unwrap().clone())
                    }
                }
            },
            || async { true },
            {
                let sent = sent.clone();
                move |key: &str| {
                    let sent = sent.clone();
                    let key = key.to_string();
                    async move {
                        sent.lock().unwrap().push(key);
                        tokio::time::sleep(Duration::from_millis(50)).await;
                        Ok(())
                    }
                }
            },
            None,
        );
        let accept = tokio::spawn(accept);
        let deadline = std::time::Instant::now() + Duration::from_millis(100);
        while capture_count.load(Ordering::SeqCst) < 2 && std::time::Instant::now() < deadline {
            tokio::task::yield_now().await;
        }
        assert!(
            capture_count.load(Ordering::SeqCst) >= 2,
            "expected trust re-validation capture to block before send"
        );
        *pane_state.lock().unwrap() = ready.to_string();
        second_capture_blocked.notify_waiters();
        assert!(accept.await.unwrap().is_ok());
        assert!(
            sent.lock().unwrap().is_empty(),
            "must not send when pane transitions during blocked re-validation capture"
        );
    }

    #[test]
    fn parse_cursor_trust_rejects_impossible_mcp_only_variant() {
        let workspace = "/tmp/worktree";
        let pane = cursor_trust_dialog_frame_with_mcp(workspace).replace(
            "[w] Trust this workspace, but don't enable all MCP servers\n",
            "",
        );
        assert!(
            !is_cursor_trust_dialog_layout(&pane, workspace),
            "MCP description without paired action must fail closed"
        );
    }

    #[tokio::test]
    async fn accept_cursor_workspace_trust_waits_through_loading_then_trust() {
        let loading = "\n\n";
        let trust = cursor_trust_dialog_frame("/tmp/worktree");
        let mut panes = VecDeque::from([
            Ok(loading.to_string()),
            Ok(trust.clone()),
            Ok(trust.clone()),
        ]);
        let mut sent = Vec::new();
        let result = accept_cursor_workspace_trust_with_capture(
            "/tmp/worktree",
            Duration::from_millis(50),
            Duration::from_millis(1),
            || {
                let pane = panes.pop_front().unwrap_or_else(|| Ok(trust.clone()));
                async move { pane }
            },
            || async { true },
            |key: &str| {
                sent.push(key.to_string());
                async { Ok(()) }
            },
            None,
        )
        .await;
        assert!(result.is_ok());
        assert_eq!(sent, vec!["a"]);
    }

    #[tokio::test]
    async fn accept_cursor_workspace_trust_already_trusted_exits_without_input() {
        let ready = "cursor-agent\n❯ \n";
        let mut sent = Vec::new();
        let result = accept_cursor_workspace_trust_with_capture(
            "/tmp/worktree",
            Duration::from_millis(50),
            Duration::from_millis(1),
            || async { Ok(ready.to_string()) },
            || async { true },
            |key: &str| {
                sent.push(key.to_string());
                async { Ok(()) }
            },
            None,
        )
        .await;
        assert!(result.is_ok());
        assert!(sent.is_empty(), "already-trusted UI must send nothing");
    }

    #[tokio::test]
    async fn accept_cursor_workspace_trust_repeated_frames_send_once() {
        let trust = cursor_trust_dialog_frame("/tmp/worktree");
        let mut sent = Vec::new();
        let result = accept_cursor_workspace_trust_with_capture(
            "/tmp/worktree",
            Duration::from_millis(50),
            Duration::from_millis(1),
            || async { Ok(trust.clone()) },
            || async { true },
            |key: &str| {
                sent.push(key.to_string());
                async { Ok(()) }
            },
            None,
        )
        .await;
        assert!(result.is_ok());
        assert_eq!(sent, vec!["a"]);
    }

    #[tokio::test]
    async fn accept_cursor_workspace_trust_prompt_text_sends_nothing() {
        let prose =
            "Implement TASK-756WX\nWorkspace Trust Required\n[a] Trust this workspace\n\n❯ ";
        let mut sent = Vec::new();
        let result = accept_cursor_workspace_trust_with_capture(
            "/tmp/worktree",
            Duration::from_millis(50),
            Duration::from_millis(1),
            || async { Ok(prose.to_string()) },
            || async { true },
            |key: &str| {
                sent.push(key.to_string());
                async { Ok(()) }
            },
            None,
        )
        .await;
        assert!(result.is_ok());
        assert!(sent.is_empty(), "prompt prose must not send trust input");
    }

    #[tokio::test]
    async fn accept_cursor_workspace_trust_recovers_after_capture_errors() {
        let trust = cursor_trust_dialog_frame("/tmp/worktree");
        let mut attempts = 0;
        let mut sent = Vec::new();
        let result = accept_cursor_workspace_trust_with_capture(
            "/tmp/worktree",
            Duration::from_millis(50),
            Duration::from_millis(1),
            || {
                attempts += 1;
                let trust = trust.clone();
                async move {
                    if attempts == 1 {
                        Err(DriverError::Transport("capture failed".into()))
                    } else {
                        Ok(trust)
                    }
                }
            },
            || async { true },
            |key: &str| {
                sent.push(key.to_string());
                async { Ok(()) }
            },
            None,
        )
        .await;
        assert!(result.is_ok());
        assert_eq!(sent, vec!["a"]);
    }

    #[tokio::test]
    async fn accept_cursor_workspace_trust_exits_when_pane_gone_without_input() {
        let trust = cursor_trust_dialog_frame("/tmp/worktree");
        let mut sent = Vec::new();
        let result = accept_cursor_workspace_trust_with_capture(
            "/tmp/worktree",
            Duration::from_millis(50),
            Duration::from_millis(1),
            || async { Ok(trust.clone()) },
            || async { false },
            |key: &str| {
                sent.push(key.to_string());
                async { Ok(()) }
            },
            None,
        )
        .await;
        assert!(result.is_ok());
        assert!(sent.is_empty(), "pane/process exit must not send input");
    }

    #[tokio::test]
    async fn accept_cursor_workspace_trust_honours_cancel_before_send() {
        let trust = cursor_trust_dialog_frame("/tmp/worktree");
        let cancel = Arc::new(AtomicBool::new(true));
        let mut sent = Vec::new();
        let result = accept_cursor_workspace_trust_with_capture(
            "/tmp/worktree",
            Duration::from_millis(50),
            Duration::from_millis(1),
            || async { Ok(trust.clone()) },
            || async { true },
            |key: &str| {
                sent.push(key.to_string());
                async { Ok(()) }
            },
            Some(cancel),
        )
        .await;
        assert!(result.is_ok());
        assert!(
            sent.is_empty(),
            "cancelled startup must not inject trust input"
        );
    }

    #[tokio::test]
    async fn accept_cursor_workspace_trust_blockquote_frame_exits_without_input() {
        let working = "Thinking...\n> quoted model output\n";
        let trust = cursor_trust_dialog_frame("/tmp/worktree");
        let mut panes = VecDeque::from([Ok(working.to_string()), Ok(trust.clone())]);
        let mut sent = Vec::new();
        let result = accept_cursor_workspace_trust_with_capture(
            "/tmp/worktree",
            Duration::from_millis(50),
            Duration::from_millis(1),
            || {
                let pane = panes.pop_front().unwrap_or_else(|| Ok(trust.clone()));
                async move { pane }
            },
            || async { true },
            |key: &str| {
                sent.push(key.to_string());
                async { Ok(()) }
            },
            None,
        )
        .await;
        assert!(result.is_ok());
        assert!(
            sent.is_empty(),
            "first stable non-trust frame must terminate without trust input"
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn send_child_owner_cancel_before_spawn_leaves_no_child() {
        let owner = SendChildOwner::new();
        let cancel = AtomicBool::new(true);
        owner
            .spawn_register_and_wait(Some(&cancel), || {
                let mut cmd = tokio::process::Command::new("sleep");
                cmd.arg("300");
                Ok(cmd)
            })
            .await
            .unwrap();
        owner.kill_and_reap().await;
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn send_child_owner_release_kills_blocked_fake_cli() {
        let owner = SendChildOwner::new();
        let cancel = Arc::new(AtomicBool::new(false));
        let cancel_for_task = cancel.clone();
        let owner_for_task = owner.clone();
        let task = tokio::spawn(async move {
            let _ = owner_for_task
                .spawn_register_and_wait(Some(cancel_for_task.as_ref()), || {
                    let mut cmd = tokio::process::Command::new("sleep");
                    cmd.arg("300");
                    Ok(cmd)
                })
                .await;
        });
        tokio::time::sleep(Duration::from_millis(100)).await;
        let joined = tokio::time::timeout(
            Duration::from_secs(2),
            cancel_and_join_driver_task(cancel.as_ref(), Some(task), Some(&owner)),
        )
        .await;
        assert!(
            joined.is_ok(),
            "release must kill/join a blocked fake tmux CLI child promptly"
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn send_child_owner_cancel_during_blocked_wait_kills_child() {
        let owner = SendChildOwner::new();
        let cancel = Arc::new(AtomicBool::new(false));
        let cancel_for_task = cancel.clone();
        let owner_for_task = owner.clone();
        let task = tokio::spawn(async move {
            let _ = owner_for_task
                .spawn_register_and_wait(Some(cancel_for_task.as_ref()), || {
                    let mut cmd = tokio::process::Command::new("sleep");
                    cmd.arg("300");
                    Ok(cmd)
                })
                .await;
        });
        tokio::time::sleep(Duration::from_millis(100)).await;
        cancel.store(true, Ordering::SeqCst);
        let joined = tokio::time::timeout(
            Duration::from_secs(2),
            cancel_and_join_driver_task(cancel.as_ref(), Some(task), Some(&owner)),
        )
        .await;
        assert!(
            joined.is_ok(),
            "cancel during blocked wait must kill/join the fake tmux CLI child promptly"
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn send_child_owner_child_error_does_not_leave_registered_child() {
        let tmp = tempfile::tempdir().unwrap();
        let stub = tmp.path().join("fail.sh");
        std::fs::write(&stub, "#!/bin/sh\nexit 42\n").unwrap();
        let mut perms = std::fs::metadata(&stub).unwrap().permissions();
        perms.set_mode(0o755);
        std::fs::set_permissions(&stub, perms).unwrap();
        let owner = SendChildOwner::new();
        let result = owner
            .spawn_register_and_wait(None, || Ok(tokio::process::Command::new(&stub)))
            .await;
        assert!(
            result.is_err(),
            "child exit must surface as transport error"
        );
        owner.kill_and_reap().await;
    }

    #[tokio::test]
    async fn cursor_trust_probe_fresh_worktree_when_enabled() {
        if std::env::var("ORGASMIC_PROBE_CURSOR_TRUST").as_deref() != Ok("1") {
            eprintln!(
                "SKIP cursor_trust_probe_fresh_worktree_when_enabled: set ORGASMIC_PROBE_CURSOR_TRUST=1"
            );
            return;
        }
        let tmp = tempfile::tempdir().unwrap();
        let session = format!("orgasmic-trust-probe-{}", std::process::id());
        let _guard = live_session_guard();
        let output = tmux_async_command()
            .args([
                "new-session",
                "-d",
                "-s",
                &session,
                "-c",
                tmp.path().to_str().unwrap(),
                "cursor-agent",
            ])
            .output()
            .await
            .expect("spawn tmux session for cursor trust probe");
        assert!(
            output.status.success(),
            "tmux new-session failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        tokio::time::sleep(Duration::from_secs(2)).await;
        let pane = capture_pane_visible(&session)
            .await
            .expect("capture probe pane");
        let _ = tmux_async_command()
            .args(["kill-session", "-t", &session])
            .status()
            .await;
        let workspace = tmp.path().display().to_string();
        let frame = classify_cursor_startup_frame(&pane, &workspace);
        assert!(
            matches!(
                frame,
                CursorStartupFrame::TrustDialog | CursorStartupFrame::Ready
            ),
            "fresh cursor-agent pane should be trust dialog or already-trusted composer, got {frame:?}\n{pane}"
        );
    }

    #[test]
    fn cursor_argv_delivery_skips_paste_prompt() {
        let cfg = TmuxTuiConfig {
            harness: Some("cursor-agent".into()),
            prompt_bundle_text: Some("do the task".into()),
            ..TmuxTuiConfig::default()
        };
        let plan = build_spawn_plan(&cfg, &ctx("run-cursor", RunKind::Worker), "cursor-agent");
        assert!(plan.paste_prompt.is_none());
        assert!(plan
            .args
            .windows(2)
            .any(|pair| pair == ["--", "do the task"]));
        assert!(cursor_argv_needs_startup_trust(
            "cursor-agent",
            &plan.paste_prompt
        ));
    }

    #[tokio::test]
    async fn wait_for_input_ready_returns_ok_when_mock_pane_has_prompt() {
        let mut panes = VecDeque::from([
            Ok(String::from("Claude Code\nloading\n")),
            Ok(String::from("Claude Code\n❯ \n")),
        ]);
        let result = wait_for_input_ready_with_capture(
            Duration::from_millis(50),
            Duration::from_millis(1),
            || {
                let pane = panes
                    .pop_front()
                    .unwrap_or_else(|| Ok(String::from("Claude Code\n❯ \n")));
                async move { pane }
            },
        )
        .await;
        assert!(
            result.is_ok(),
            "expected prompt-ready mock pane: {result:?}"
        );
    }

    #[tokio::test]
    async fn wait_for_input_ready_returns_input_not_ready_on_timeout() {
        let timeout = Duration::from_millis(10);
        let result =
            wait_for_input_ready_with_capture(timeout, Duration::from_millis(1), || async {
                Ok(String::from("Claude Code\nstill loading\n"))
            })
            .await;
        assert!(
            matches!(result, Err(DriverError::InputNotReady(observed)) if observed == timeout),
            "expected InputNotReady timeout, got {result:?}"
        );
    }

    #[tokio::test]
    async fn implementer_transition_state_is_accepted() {
        let d = driver();
        let mut s = d
            .acquire(ctx("run-tx", RunKind::Worker), inert_config())
            .await
            .unwrap();
        let _ready = s.events.recv().await.unwrap();
        let ack = s
            .control
            .transition_state(TransitionRequest {
                from: "ready".into(),
                to: "in_progress".into(),
                reason: "starting work".into(),
            })
            .await
            .unwrap();
        assert!(ack.accepted);
        let ev = s.events.recv().await.unwrap();
        assert!(matches!(ev, DriverEvent::TransitionState { .. }));
    }

    #[tokio::test]
    async fn release_is_idempotent() {
        let d = driver();
        let mut s = d
            .acquire(ctx("run-i", RunKind::Worker), inert_config())
            .await
            .unwrap();
        let _ = s.events.recv().await;
        s.control.release("a").await.unwrap();
        s.control.release("b").await.unwrap();
    }

    #[test]
    fn transport_name_is_stable() {
        assert_eq!(driver().transport(), "tmux");
    }

    // orgasmic:TASK-0RCRY
    /// The socket a run pins must actually reach the tmux command line.
    ///
    /// This is the injection proof for TASK-0RCRY, and it deliberately touches
    /// no tmux server at all: a run with the isolation removed must be caught
    /// by a check that cannot itself create a session on somebody else's
    /// server. `tmux_sessions_land_on_a_server_the_test_run_owns` is the
    /// behavioural half.
    ///
    /// Injection: make `tmux_socket_args_for` return `Vec::new()` — the pre-fix
    /// state in which no call site passes `-L`/`-S`.
    #[test]
    fn tmux_socket_args_pin_the_server_on_the_command_line() {
        assert_eq!(
            tmux_socket_args_for(Some("orgasmic-test-42")),
            vec!["-L".to_string(), "orgasmic-test-42".to_string()],
            "a pinned socket must reach the tmux command line as -L <socket>"
        );
        assert!(
            tmux_socket_args_for(None).is_empty(),
            "production pins nothing: the daemon must reach the same server an \
             operator's own tmux client reaches"
        );
    }

    // orgasmic:TASK-0RCRY
    /// A test run must never reach a tmux server it did not create.
    ///
    /// Before TASK-0RCRY no call site passed `-L`/`-S`, so every probe and
    /// fixture session landed on whichever server the environment selected: the
    /// operator's own on a developer box or a server hosting live worker panes.
    ///
    /// Injection: make `tmux_socket()` return `None`, or drop the `-L` from
    /// `tmux_command`, and the argv assertion below goes red. It is asserted
    /// first, deliberately: a run with the isolation removed must fail *before*
    /// it can create a session on a server someone else owns.
    #[tokio::test]
    async fn tmux_sessions_land_on_a_server_the_test_run_owns() {
        let _live_guard = live_session_guard();
        let (tmux_available, _) = tmux_and_command_available(None).await;
        if skip_test_if_missing(
            "tmux_sessions_land_on_a_server_the_test_run_owns",
            &[("tmux", tmux_available)],
        ) {
            return;
        }

        let socket = own_tmux_server_for_tests();
        assert!(
            socket.starts_with("orgasmic-test-"),
            "the owned socket must be this test process's own, got {socket:?}"
        );

        let argv = tmux_command()
            .get_args()
            .map(|arg| arg.to_string_lossy().into_owned())
            .collect::<Vec<_>>();
        assert_eq!(
            argv,
            vec!["-L".to_string(), socket.to_string()],
            "every tmux invocation must pin the server this run owns; \
             selection reported as: {}",
            tmux_server_selection()
        );

        // And the session really is only there: created through the same
        // constructor the driver uses, it must be invisible to a client that
        // selects the server the way every call site used to.
        let session = format!(
            "orgasmic-owned-probe-{}",
            chrono::Utc::now().timestamp_nanos_opt().unwrap_or(0)
        );
        let _guard = SessionGuard(session.clone());
        let status = tmux_async_command()
            .args(["new-session", "-d", "-s", &session, "--", "sleep", "10"])
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .await
            .expect("spawn tmux");
        assert!(
            status.success(),
            "session must start on {}",
            tmux_server_selection()
        );
        assert!(
            has_tmux_session(&session).await.unwrap(),
            "the owned server must hold the session this run created"
        );
        let on_shared_server = StdCommand::new("tmux")
            .args(["has-session", "-t", &session])
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .map(|status| status.success())
            .unwrap_or(false);
        assert!(
            !on_shared_server,
            "a test session must not be reachable on the shared default server"
        );
    }

    /// Real-tmux smoke. The binary sentinel reports hosts without `tmux`;
    /// when tmux is present we verify the driver actually spawns + tears
    /// down a session.
    #[tokio::test]
    async fn real_tmux_session_lifecycle() {
        let _live_guard = live_session_guard();
        let (tmux_available, _) = tmux_and_command_available(None).await;
        if skip_test_if_missing("real_tmux_session_lifecycle", &[("tmux", tmux_available)]) {
            return;
        }
        let d = driver();
        // Use `sleep 60` so the wrapped command lives long enough for us
        // to verify the session, then we kill it via release.
        let cfg = DriverConfig::from_value(json!({
            "command": "sleep",
            "args": ["60"],
        }));
        let mut s = d
            .acquire(ctx("run-real", RunKind::Worker), cfg)
            .await
            .unwrap();
        let _guard = SessionGuard(tmux_session_name(&s.identity));
        let mut capabilities = None;
        for _ in 0..5 {
            let ev = s.events.recv().await.unwrap();
            if let DriverEvent::Ready {
                capabilities: caps, ..
            } = ev
            {
                capabilities = Some(caps);
                break;
            }
        }
        let capabilities = capabilities.expect("expected Ready");
        assert_eq!(capabilities["inert"], false);
        // Verify tmux actually has the session.
        let session_name = tmux_session_name(&s.identity);
        let listed = tmux_command()
            .args(["has-session", "-t", &session_name])
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .unwrap();
        assert!(listed.success(), "tmux session should exist");
        s.control.release("done").await.unwrap();
        // Give tmux a moment to actually tear down.
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        let listed = tmux_command()
            .args(["has-session", "-t", &session_name])
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .unwrap();
        assert!(!listed.success(), "tmux session should be gone");
    }

    /// End-to-end byte proof for externally waking an app terminal. The pane
    /// reads raw tty bytes, so tmux's bracketed-paste wrapper would appear as
    /// `ESC[200~` / `ESC[201~` here. The only accepted data is the fixed
    /// shell-inert marker followed by the separately-sent Enter byte.
    #[tokio::test]
    async fn manager_wake_pastes_exact_marker_bytes_without_bracketed_paste() {
        let _live_guard = live_session_guard();
        let (tmux_available, _) = tmux_and_command_available(None).await;
        if skip_test_if_missing(
            "manager_wake_pastes_exact_marker_bytes_without_bracketed_paste",
            &[("tmux", tmux_available)],
        ) {
            return;
        }
        let tmp = tempfile::tempdir().unwrap();
        let marker_path = tmp.path().join("marker.raw");
        let submit_path = tmp.path().join("submit.raw");
        let marker_len = crate::r#trait::MANAGER_WAKE_MARKER.len();
        let reader = format!(
            "stty raw -echo; dd bs=1 count={marker_len} of={} 2>/dev/null; dd bs=1 count=1 of={} 2>/dev/null",
            sh_single_quote(&marker_path.display().to_string()),
            sh_single_quote(&submit_path.display().to_string()),
        );
        let session = format!("orgasmic-wake-bytes-{}", uuid::Uuid::new_v4().simple());
        let _session_guard = SessionGuard(session.clone());
        let status = tmux_async_command()
            .args([
                "new-session",
                "-d",
                "-s",
                &session,
                "--",
                "/bin/sh",
                "-c",
                &reader,
            ])
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .status()
            .await
            .expect("start raw wake reader pane");
        assert!(status.success(), "raw wake reader must start");

        // Let the shell install raw mode before tmux delivers the marker.
        tokio::time::sleep(Duration::from_millis(100)).await;
        paste_manager_wake_marker_into_pane(&session, None, None)
            .await
            .expect("paste fixed manager marker");
        send_keys(&session, &[String::from("Enter")], None, None)
            .await
            .expect("submit manager marker");

        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        while (!marker_path.exists() || !submit_path.exists())
            && std::time::Instant::now() < deadline
        {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        let marker = std::fs::read(&marker_path).expect("raw marker bytes captured");
        let submit = std::fs::read(&submit_path).expect("raw submit byte captured");
        assert_eq!(marker, crate::r#trait::MANAGER_WAKE_MARKER.as_bytes());
        assert_eq!(submit, [b'\r'], "Enter must be a separate submit action");
    }

    #[tokio::test]
    async fn manager_wake_missing_tmux_target_is_typed_unavailable() {
        let _live_guard = live_session_guard();
        let (tmux_available, _) = tmux_and_command_available(None).await;
        if skip_test_if_missing(
            "manager_wake_missing_tmux_target_is_typed_unavailable",
            &[("tmux", tmux_available)],
        ) {
            return;
        }
        let mut control = TmuxTuiControl {
            events: None,
            session_name: format!("orgasmic-wake-gone-{}", uuid::Uuid::new_v4().simple()),
            inert: false,
            lifecycle_abort: None,
            pane_activity_abort: None,
            startup_task: None,
            startup_cancel: Arc::new(AtomicBool::new(false)),
            send_child: SendChildOwner::new(),
            input_ready_timeout: default_input_ready_timeout(),
            terminal_emitted: Arc::new(AtomicBool::new(false)),
            kill_on_drop: false,
            released: false,
        };
        let err = control
            .send_manager_wake(ManagerWakeRequest {})
            .await
            .expect_err("a disappeared target must not become generic transport failure");
        assert!(matches!(err, DriverError::ManagerWakeUnavailable));
    }

    // orgasmic:TASK-4CSMY
    /// A newline-emitting pane and a newline-FREE redrawing pane, the two
    /// fixtures TASK-RWCRN.1 established for tmux. The second is the ship
    /// blocker: a full-screen TUI repaints with CR and ANSI and can go many
    /// minutes without an LF, so anything counting lines sees nothing at all
    /// for exactly the harnesses this event exists to protect.
    #[cfg(unix)]
    const PANE_ACTIVITY_TICKING_FIXTURE: &str = "while :; do echo tick; sleep 0.05; done";
    #[cfg(unix)]
    const PANE_ACTIVITY_REDRAW_FIXTURE: &str = "i=0; while :; do i=$((i+1)); \
         printf '\\r\\033[K%s%% working' \"$i\"; sleep 0.05; done";

    // orgasmic:TASK-4CSMY
    /// Run the pane watcher against a REAL tmux pane on the test-owned server
    /// at a compressed cadence, and return the first `(seq, bytes)` it
    /// publishes. Compressed so the default suite carries this proof instead
    /// of an `#[ignore]`d smoke; the production cadence is proven separately
    /// by the two live smokes below, which go through `acquire`.
    #[cfg(unix)]
    async fn first_pane_activity_for(session: &str, shell: &str) -> (u64, u64) {
        let status = tmux_command()
            .args([
                "new-session",
                "-d",
                "-x",
                TMUX_SESSION_COLS,
                "-y",
                TMUX_SESSION_ROWS,
                "-s",
                session,
                "--",
                "/bin/sh",
                "-c",
                shell,
            ])
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .expect("spawn tmux fixture pane");
        assert!(
            status.success(),
            "tmux new-session failed on {}",
            tmux_server_selection()
        );

        let (tx, mut rx) = mpsc::channel(8);
        let terminal_emitted = Arc::new(AtomicBool::new(false));
        let watcher = tokio::spawn(pane_activity_watch(
            session.to_string(),
            tx,
            terminal_emitted.clone(),
            Duration::from_millis(250),
        ));

        let observed = tokio::time::timeout(Duration::from_secs(10), async {
            loop {
                match rx.recv().await {
                    Some(DriverEvent::PaneActivity { seq, bytes }) => break Some((seq, bytes)),
                    Some(_) => continue,
                    None => break None,
                }
            }
        })
        .await;

        terminal_emitted.store(true, Ordering::SeqCst);
        watcher.abort();
        observed
            .expect("a writing pane must publish PaneActivity")
            .expect("the pane event channel closed before any activity")
    }

    // orgasmic:TASK-4CSMY
    /// The tmux transport had no continuous pane event of any kind, so a
    /// provider-bound turn (~0% cpu, no tool calls) had no evidence channel at
    /// all and the stall clock released it at 600 s. This is that channel,
    /// against a real pane on a real tmux server.
    #[cfg(unix)]
    #[tokio::test]
    async fn tmux_pane_activity_publishes_raw_byte_counts_from_a_real_pane() {
        const TEST: &str = "tmux_pane_activity_publishes_raw_byte_counts_from_a_real_pane";
        let _live_guard = live_session_guard();
        let (tmux_available, _) = tmux_and_command_available(None).await;
        if skip_test_if_missing(TEST, &[("tmux", tmux_available)]) {
            return;
        }

        let ticking = format!("orgasmic-pane-activity-lines-{}", std::process::id());
        let _ticking_guard = SessionGuard(ticking.clone());
        let (seq, bytes) = first_pane_activity_for(&ticking, PANE_ACTIVITY_TICKING_FIXTURE).await;
        assert_eq!(seq, 0, "the first window publishes seq 0");
        assert!(bytes > 0, "a writing pane must report bytes, got {bytes}");

        // The unit is BYTES, not lines: this pane never emits an LF.
        let redraw = format!("orgasmic-pane-activity-redraw-{}", std::process::id());
        let _redraw_guard = SessionGuard(redraw.clone());
        let (seq, bytes) = first_pane_activity_for(&redraw, PANE_ACTIVITY_REDRAW_FIXTURE).await;
        assert_eq!(
            seq, 0,
            "a redrawing pane must publish on the same cadence as a ticking one"
        );
        assert!(
            bytes > 0,
            "a pane repainting with CR and ANSI and no LF must still report bytes, got {bytes}"
        );
    }

    // orgasmic:TASK-4CSMY
    /// The production path, at the production cadence: `acquire` →
    /// `start_pane_activity_watch` → the driver's own event channel, which is
    /// the stream the daemon consumes. The compressed test above cannot prove
    /// the watcher is wired to `acquire`.
    ///
    /// Returns the first observed `(seq, bytes)`, or `None` if the channel
    /// closed first; panics if nothing arrived within two intervals.
    #[cfg(unix)]
    async fn live_tmux_pane_activity_for(
        test_name: &'static str,
        shell: &str,
    ) -> Option<(u64, u64)> {
        let (tmux_available, _) = tmux_and_command_available(None).await;
        assert!(tmux_available, "{test_name} needs a real tmux binary");
        let d = driver();
        let cfg = DriverConfig::from_value(json!({
            "command": "/bin/sh",
            "args": ["-c", shell],
        }));
        let mut s = d
            .acquire(ctx("run-tmux-pane-activity", RunKind::Worker), cfg)
            .await
            .unwrap();
        let _guard = SessionGuard(tmux_session_name(&s.identity));
        let ready = s.events.recv().await.expect("ready event");
        let DriverEvent::Ready { capabilities, .. } = ready else {
            panic!("expected Ready, got {ready:?}");
        };
        assert_eq!(
            capabilities["inert"], false,
            "{test_name} must run against a real pane, not an inert session"
        );

        let observed = tokio::time::timeout(PANE_ACTIVITY_INTERVAL * 2, async {
            loop {
                match s.events.recv().await {
                    Some(DriverEvent::PaneActivity { seq, bytes }) => break Some((seq, bytes)),
                    Some(_) => continue,
                    None => break None,
                }
            }
        })
        .await;

        // Reap before asserting: a failed assertion must not leak the session.
        s.control.release("test done").await.unwrap();

        observed.expect("a writing pane must publish PaneActivity within 2 intervals")
    }

    /// `#[ignore]` because it spawns a real session and waits a full
    /// `PANE_ACTIVITY_INTERVAL`, so the default summary counts it instead of
    /// silently paying 30 s for it. Run with
    /// `cargo test -p orgasmic-drivers -- --ignored live_tmux_pane_publishes`.
    #[cfg(unix)]
    #[tokio::test]
    #[ignore = "live tmux smoke: real tmux session, waits one PANE_ACTIVITY_INTERVAL"]
    async fn live_tmux_pane_publishes_pane_activity_while_it_writes() {
        let _live_guard = live_session_guard();
        let observed = live_tmux_pane_activity_for(
            "live_tmux_pane_publishes_pane_activity_while_it_writes",
            PANE_ACTIVITY_TICKING_FIXTURE,
        )
        .await;
        let (seq, bytes) = observed.expect("event channel closed before any pane activity");
        assert_eq!(seq, 0);
        assert!(
            bytes > 100,
            "a pane writing ~20 lines/s across one window should report many bytes, got {bytes}"
        );
    }

    /// TASK-RWCRN.1's fixture on the tmux transport: CR + ANSI repaint, never
    /// an LF, for longer than one `PANE_ACTIVITY_INTERVAL`.
    #[cfg(unix)]
    #[tokio::test]
    #[ignore = "live tmux smoke: real tmux session, waits one PANE_ACTIVITY_INTERVAL"]
    async fn live_tmux_pane_publishes_pane_activity_for_newline_free_redraws() {
        let _live_guard = live_session_guard();
        let observed = live_tmux_pane_activity_for(
            "live_tmux_pane_publishes_pane_activity_for_newline_free_redraws",
            PANE_ACTIVITY_REDRAW_FIXTURE,
        )
        .await;
        let (seq, bytes) = observed.expect("event channel closed before any pane activity");
        assert_eq!(
            seq, 0,
            "a redrawing pane must publish its first activity on the same cadence"
        );
        assert!(
            bytes > 100,
            "a pane repainting ~20x/s with no LF across one window should report many bytes, \
             got {bytes}"
        );
    }

    #[tokio::test]
    async fn real_tmux_session_exports_orgasmic_run_id() {
        // `orgasmic manager register` (dec_3Y2E1) recognises "I am already
        // supervised" by reading ORGASMIC_RUN_ID from its own environment —
        // prove the spawned pane actually has it set, not just that the
        // spawn plan carries a run id.
        let _live_guard = live_session_guard();
        let (tmux_available, _) = tmux_and_command_available(None).await;
        if skip_test_if_missing(
            "real_tmux_session_exports_orgasmic_run_id",
            &[("tmux", tmux_available)],
        ) {
            return;
        }
        let out_dir = tempfile::tempdir().unwrap();
        let out_path = out_dir.path().join("run-id.txt");
        let d = driver();
        let cfg = DriverConfig::from_value(json!({
            "command": "sh",
            "args": ["-c", format!("printf '%s' \"$ORGASMIC_RUN_ID\" > {}", out_path.display())],
        }));
        let s = d
            .acquire(ctx("run-env-export-test", RunKind::Worker), cfg)
            .await
            .unwrap();
        let _guard = SessionGuard(tmux_session_name(&s.identity));

        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        let mut body = String::new();
        while std::time::Instant::now() < deadline {
            if let Ok(contents) = std::fs::read_to_string(&out_path) {
                if !contents.is_empty() {
                    body = contents;
                    break;
                }
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        assert_eq!(body, "run-env-export-test");
    }

    #[tokio::test]
    async fn real_tmux_control_drop_without_release_kills_session() {
        let _live_guard = live_session_guard();
        let (tmux_available, _) = tmux_and_command_available(None).await;
        if skip_test_if_missing(
            "real_tmux_control_drop_without_release_kills_session",
            &[("tmux", tmux_available)],
        ) {
            return;
        }
        let session_name = {
            let d = driver();
            let cfg = DriverConfig::from_value(json!({
                "command": "sleep",
                "args": ["60"],
            }));
            let mut s = d
                .acquire(ctx("run-drop-cleanup", RunKind::Worker), cfg)
                .await
                .unwrap();
            let _ready = s.events.recv().await.unwrap();
            let session_name = tmux_session_name(&s.identity);
            let listed = tmux_command()
                .args(["has-session", "-t", &session_name])
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .status()
                .unwrap();
            assert!(listed.success(), "tmux session should exist before drop");
            session_name
        };
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        let listed = tmux_command()
            .args(["has-session", "-t", &session_name])
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .unwrap();
        assert!(!listed.success(), "tmux session should be gone after drop");
    }

    /// Real claude TUI smoke. This verifies the prompt-ready detector against
    /// the live pane before the driver pastes an initial prompt.
    #[tokio::test]
    #[ignore = "requires a live Claude TUI; run this test explicitly"]
    async fn real_claude_input_ready_smoke() {
        let _live_guard = live_session_guard();
        let (tmux_available, claude_available) = tmux_and_command_available(Some("claude")).await;
        if skip_test_if_missing(
            "real_claude_input_ready_smoke",
            &[("tmux", tmux_available), ("claude", claude_available)],
        ) {
            assert_required_test_tooling(&[
                ToolRequirement::new("tmux", 1, tmux_available),
                ToolRequirement::new("claude", 1, claude_available),
            ]);
            return;
        }

        let session = format!(
            "orgasmic-input-ready-test-{}",
            chrono::Utc::now().timestamp_nanos_opt().unwrap_or(0)
        );
        let plan = TmuxSpawnPlan {
            command: "claude".into(),
            args: vec!["--dangerously-skip-permissions".into()],
            cwd: std::env::current_dir().unwrap(),
            paste_prompt: None,
            native_runtime: None,
            run_id: "run-input-ready".into(),
            runtime_id: "runtime-input-ready".into(),
            boot_id: "boot-test".into(),
            manager_terminal_capability: None,
            harness_env: Vec::new(),
            native_resume_mode: false,
            trusted_provider_identity: None,
            pinned_executable: None,
            provider_home: None,
        };

        let _guard = SessionGuard(session.clone());
        spawn_tmux_session(&session, &plan).await.unwrap();
        let ready = wait_for_input_ready(&session, Duration::from_secs(10)).await;
        kill_tmux_session(&session).await;
        assert!(
            ready.is_ok(),
            "claude input field should become ready within 10s: {ready:?}"
        );
    }

    /// Non-blocking acquire (zombie-lease fix): with a dispatch prompt, the
    /// non-claude delivery path waits 800ms before pasting — `acquire` must
    /// return well before that because delivery now runs in the background.
    #[tokio::test]
    async fn real_tmux_acquire_returns_before_prompt_delivery() {
        let _live_guard = live_session_guard();
        let (tmux_available, _) = tmux_and_command_available(None).await;
        if skip_test_if_missing(
            "real_tmux_acquire_returns_before_prompt_delivery",
            &[("tmux", tmux_available)],
        ) {
            return;
        }
        let d = driver();
        let cfg = DriverConfig::from_value(json!({
            "command": "sh",
            "args": ["-c", "sleep 30"],
            "prompt_bundle_text": "dispatch brief",
        }));
        let start = std::time::Instant::now();
        let mut s = d
            .acquire(ctx("run-nonblock", RunKind::Worker), cfg)
            .await
            .unwrap();
        let elapsed = start.elapsed();
        let _guard = SessionGuard(tmux_session_name(&s.identity));
        assert!(
            elapsed < Duration::from_millis(700),
            "acquire blocked on prompt delivery: {elapsed:?}"
        );
        let ev = s.events.recv().await.unwrap();
        assert!(matches!(ev, DriverEvent::Ready { .. }));
        s.control.release("cleanup").await.unwrap();
    }

    #[tokio::test]
    async fn real_tmux_early_exit_without_finalize_is_failure() {
        let _live_guard = live_session_guard();
        let (tmux_available, bash_available) = tmux_and_command_available(Some("bash")).await;
        if skip_test_if_missing(
            "real_tmux_early_exit_without_finalize_is_failure",
            &[("tmux", tmux_available), ("bash", bash_available)],
        ) {
            return;
        }
        let d = driver();
        let cfg = DriverConfig::from_value(json!({
            "command": "bash",
            "args": ["-lc", "echo started; exit 0"],
        }));
        let mut s = d
            .acquire(ctx("run-early-exit", RunKind::Worker), cfg)
            .await
            .unwrap();
        let _guard = SessionGuard(tmux_session_name(&s.identity));
        let ev = s.events.recv().await.unwrap();
        assert!(matches!(ev, DriverEvent::Ready { .. }));
        let mut saw_failure = false;
        for _ in 0..10 {
            let ev = tokio::time::timeout(Duration::from_secs(5), s.events.recv())
                .await
                .expect("timed out waiting for early-exit event")
                .expect("event stream closed");
            match ev {
                DriverEvent::DriverError { fatal, message } if fatal => {
                    assert!(message.contains("ended without finalize"), "{message}");
                    saw_failure = true;
                    break;
                }
                DriverEvent::DriverError { fatal: false, .. } => {}
                DriverEvent::RunComplete { .. } => {
                    panic!("early tmux exit must not emit RunComplete")
                }
                other => panic!("unexpected event before early-exit failure: {other:?}"),
            }
        }
        assert!(saw_failure, "expected fatal early-exit DriverError");
        s.control.release("cleanup").await.unwrap();
    }

    #[tokio::test]
    async fn real_tmux_prompt_bundle_is_consumed() {
        let _live_guard = live_session_guard();
        let (tmux_available, _) = tmux_and_command_available(None).await;
        if skip_test_if_missing(
            "real_tmux_prompt_bundle_is_consumed",
            &[("tmux", tmux_available)],
        ) {
            return;
        }
        let d = driver();
        let run_id = "run-prompt-real";
        let cfg = DriverConfig::from_value(json!({
            "command": "cat",
            "prompt_bundle_text": "ORG_PROMPT_SENTINEL",
        }));
        let mut s = d.acquire(ctx(run_id, RunKind::Worker), cfg).await.unwrap();
        let _guard = SessionGuard(tmux_session_name(&s.identity));
        let ev = s.events.recv().await.unwrap();
        let DriverEvent::Ready { capabilities, .. } = ev else {
            panic!("expected Ready, got {ev:?}");
        };
        assert_eq!(capabilities["inert"], false);
        // Prompt delivery is now asynchronous (non-blocking acquire): the
        // non-claude path waits 800ms before pasting, so poll for the sentinel
        // instead of sampling once.
        let session_name = tmux_session_name(&s.identity);
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        let mut pane = String::new();
        while std::time::Instant::now() < deadline {
            let output = tmux_command()
                .args(["capture-pane", "-pt", &session_name, "-S", "-100"])
                .output()
                .unwrap();
            pane = String::from_utf8_lossy(&output.stdout).into_owned();
            if pane.contains("ORG_PROMPT_SENTINEL") {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(150)).await;
        }
        assert!(
            pane.contains("ORG_PROMPT_SENTINEL"),
            "tmux pane should show prompt bundle, got {pane}"
        );
        s.control.release("done").await.unwrap();
    }

    #[tokio::test]
    async fn real_tmux_attach_proves_existing_session() {
        let _live_guard = live_session_guard();
        let (tmux_available, _) = tmux_and_command_available(None).await;
        if skip_test_if_missing(
            "real_tmux_attach_proves_existing_session",
            &[("tmux", tmux_available)],
        ) {
            return;
        }
        let d = driver();
        let cfg = DriverConfig::from_value(json!({
            "command": "sleep",
            "args": ["60"],
        }));
        let mut s = d
            .acquire(ctx("run-attach", RunKind::Worker), cfg)
            .await
            .unwrap();
        let _guard = SessionGuard(tmux_session_name(&s.identity));
        let _ready = s.events.recv().await.unwrap();

        let attached = d
            .attach(ctx("run-attach", RunKind::Worker), DriverConfig::empty())
            .await
            .unwrap();
        let AttachOutcome::NotReattachable = attached else {
            panic!("attach with a fresh identity should not match the acquired session");
        };

        let attach_ctx = DriverContext {
            identity: s.identity.clone(),
            run_kind: RunKind::Worker,
            task_id: "TASK-006".into(),
            worker_id: "implementer-claude-tmux".into(),
            project_id: Some("orgasmic".into()),
            worktree: None,
        };
        let attached = d.attach(attach_ctx, DriverConfig::empty()).await.unwrap();
        let AttachOutcome::Attached(mut attached) = attached else {
            panic!("expected tmux attach to prove live session");
        };
        let ev = attached.session.events.recv().await.unwrap();
        let DriverEvent::Ready { capabilities, .. } = ev else {
            panic!("expected Ready from attach, got {ev:?}");
        };
        assert_eq!(capabilities["reattached"], true);
        s.control.release("done").await.unwrap();
    }

    // orgasmic:TASK-3NJ9K
    /// The daemon's test-profile fence decides whether a mux address is safe
    /// for a test to hold by asking `harness_execs_provider_binary`. That
    /// answer is only worth anything while it still matches what this mode
    /// actually launches, and the two live in different crates — so assert the
    /// agreement over the whole harness table, here, where the launch is
    /// defined.
    ///
    /// The invariant is exact: a harness execs a provider binary precisely when
    /// its default command *is its own name*. `custom` resolves to the
    /// operator's shell and unknown harnesses to `sh`, so neither can turn a
    /// pane into a provider process on its own.
    #[test]
    fn default_command_agrees_with_the_provider_harness_predicate() {
        for harness in crate::HARNESSES {
            let (command, _) = default_command_for_harness(harness, &TmuxTuiConfig::default());
            assert_eq!(
                crate::harness_execs_provider_binary(harness),
                command == *harness,
                "tmux launches {harness} as {command}, which disagrees with \
                 harness_execs_provider_binary"
            );
        }
    }

    #[test]
    fn packed_argv_len_counts_each_nul_terminator() {
        assert_eq!(packed_argv_len(std::iter::empty()), 0);
        assert_eq!(packed_argv_len(["ab", "cde"]), 3 + 4);
    }

    // orgasmic:TASK-RKTH1
    /// The prompt is the reason this route exists, so the quoting has to be
    /// byte-exact — proven against a real shell rather than against our own
    /// idea of what the shell does.
    #[test]
    fn sh_single_quote_survives_a_real_shell_byte_for_byte() {
        for raw in [
            "plain",
            "it's got 'quotes' inside",
            "$HOME `whoami` \\ \"double\" ${x}",
            "trailing whitespace   ",
            "multi\nline\n\n",
            "unicode: тест 🙂 — em-dash",
            "*glob* ?and? [brackets]",
        ] {
            let output = StdCommand::new("/bin/sh")
                .arg("-c")
                .arg(format!("printf %s {}", sh_single_quote(raw)))
                .output()
                .expect("run /bin/sh");
            assert!(output.status.success(), "shell rejected quoting of {raw:?}");
            assert_eq!(
                String::from_utf8_lossy(&output.stdout),
                raw,
                "quoting changed bytes for {raw:?}"
            );
        }
    }

    #[test]
    fn launcher_script_removes_itself_before_exec() {
        let script = launcher_script_body("/bin/echo", &["one".into(), "it's two".into()]);
        // `rm` must precede `exec`: exec never returns, so anything after it is
        // dead code and the artefact would outlive the launch.
        let rm = script.find("rm -f -- \"$0\"").expect("self-delete");
        let exec = script.find("exec ").expect("exec");
        assert!(rm < exec, "self-delete must precede exec:\n{script}");
        assert!(
            script.ends_with("'/bin/echo' 'one' 'it'\\''s two'\n"),
            "{script}"
        );
    }

    // orgasmic:TASK-RKTH1
    /// End to end on a real tmux: a prompt an order of magnitude past the imsg
    /// ceiling reaches the pane's argv intact. Before this route the same spawn
    /// died with tmux's `command too long`.
    #[tokio::test]
    async fn oversized_prompt_spawns_and_arrives_byte_exact() {
        let _live_guard = live_session_guard();
        let (tmux_ok, _) = tmux_and_command_available(None).await;
        if skip_test_if_missing(
            "oversized_prompt_spawns_and_arrives_byte_exact",
            &[("tmux", tmux_ok)],
        ) {
            return;
        }
        let tmp = tempfile::tempdir().unwrap();
        let landed = tmp.path().join("prompt.txt");
        // Far past the ceiling, and carrying every byte class the quoting has
        // to survive.
        let prompt = format!(
            "{}\n'single' \"double\" $VAR `cmd` \\ — тест 🙂\n",
            "x".repeat(100_000)
        );

        let session = format!("orgasmic-rkth1-{}", uuid::Uuid::new_v4().simple());
        let _guard = SessionGuard(session.clone());
        let plan = TmuxSpawnPlan {
            command: "/bin/sh".into(),
            args: vec![
                "-c".into(),
                format!("printf %s \"$1\" > {}", landed.display()),
                "rkth1".into(),
                prompt.clone(),
            ],
            cwd: tmp.path().to_path_buf(),
            paste_prompt: None,
            native_runtime: None,
            run_id: "run-rkth1".into(),
            runtime_id: "runtime-rkth1".into(),
            boot_id: "boot-test".into(),
            manager_terminal_capability: None,
            harness_env: Vec::new(),
            native_resume_mode: false,
            trusted_provider_identity: None,
            pinned_executable: None,
            provider_home: None,
        };
        spawn_tmux_session(&session, &plan)
            .await
            .expect("oversized prompt must spawn, not hit `command too long`");

        let deadline = std::time::Instant::now() + Duration::from_secs(10);
        while !landed.exists() && std::time::Instant::now() < deadline {
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        let written = std::fs::read_to_string(&landed).expect("pane wrote the prompt");
        assert_eq!(written, prompt, "prompt bytes changed in transit");
        kill_tmux_session(&session).await;
    }
}
