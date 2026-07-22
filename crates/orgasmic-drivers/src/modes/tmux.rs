// arch: arch_A53QX.4
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

use crate::r#trait::{
    AttachOutcome, Attached, BabysitterAck, BabysitterRequest, DriverConfig, DriverContext,
    DriverControl, DriverError, DriverSession, HarnessEventAdapter, NativeRuntimeMeta, RunKind,
    TransitionAck, TransitionRequest, WorkerDriver,
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
        let (lifecycle_task, startup_task) = if !inert {
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
            (Some(task), startup_task)
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
            (None, None)
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
                kind: ctx.run_kind,
                inert,
                lifecycle_abort: lifecycle_task.as_ref().map(JoinHandle::abort_handle),
                startup_task,
                startup_cancel,
                send_child,
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
        if cfg.force_inert || !tmux_available() {
            return Ok(AttachOutcome::NotReattachable);
        }

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

        Ok(AttachOutcome::Attached(Attached {
            session: Box::new(DriverSession {
                identity: ctx.identity.clone(),
                pid: None,
                events: rx,
                control: Box::new(TmuxTuiControl {
                    events: Some(tx.clone()),
                    session_name: session_name.clone(),
                    kind: ctx.run_kind,
                    inert: false,
                    lifecycle_abort: Some(lifecycle_abort),
                    startup_task: None,
                    startup_cancel: Arc::new(AtomicBool::new(false)),
                    send_child: SendChildOwner::new(),
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
    kind: RunKind,
    inert: bool,
    /// Watches pane/process end only — never scrollback capture (TASK-AFE5Q).
    lifecycle_abort: Option<tokio::task::AbortHandle>,
    /// One-shot startup helper (prompt paste or Cursor trust gate).
    startup_task: Option<JoinHandle<()>>,
    startup_cancel: Arc<AtomicBool>,
    /// In-flight tmux CLI send child; killed/reaped before release returns.
    send_child: SendChildOwner,
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
        if self.kind == RunKind::Babysitter {
            return Err(DriverError::WorkerToolBlocked("transition_state".into()));
        }
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

    async fn babysitter_action(
        &mut self,
        req: BabysitterRequest,
    ) -> Result<BabysitterAck, DriverError> {
        if self.kind == RunKind::Worker {
            return Err(DriverError::BabysitterToolBlocked(req.tool.as_str().into()));
        }
        if let Some(events) = self.events.as_ref() {
            let _ = events
                .send(DriverEvent::ToolCall {
                    call_id: format!(
                        "bs-{}",
                        chrono::Utc::now().timestamp_nanos_opt().unwrap_or(0)
                    ),
                    name: req.tool.as_str().into(),
                    args: req.payload.clone(),
                    seq: 0,
                })
                .await;
        }
        Ok(BabysitterAck {
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
        abort_driver_task(self.startup_task.take());
        if !self.released && self.kill_on_drop && !self.inert {
            kill_tmux_session_sync(&self.session_name);
        }
    }
}

fn tmux_available() -> bool {
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

/// Synchronous tmux session probe for crash-reconciliation paths that cannot
/// await driver I/O.
pub fn tmux_session_exists(session: &str) -> bool {
    std::process::Command::new("tmux")
        .args(["has-session", "-t", session])
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

async fn has_tmux_session(session: &str) -> Result<bool, DriverError> {
    let status = tokio::process::Command::new("tmux")
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
            if let Some(model) = cfg
                .model
                .as_deref()
                .filter(|model| !model.trim().is_empty())
            {
                if !args.iter().any(|arg| arg == "--model") {
                    args.push("--model".to_string());
                    args.push(model.to_string());
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
        })
    };

    TmuxSpawnPlan {
        command,
        args,
        cwd,
        paste_prompt,
        native_runtime,
        run_id: ctx.identity.run_id.clone(),
        native_resume_mode: cfg.native_resume_mode,
        trusted_provider_identity: cfg.trusted_provider_identity.clone(),
        pinned_executable: cfg.pinned_executable.clone(),
        provider_home: cfg.provider_home.clone(),
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
    }
}

/// Claude stores conversation JSONL under
/// `~/.claude/projects/<encoded-cwd>/<session-id>.jsonl`, where the encoding
/// replaces path separators and dots with `-`.
fn claude_session_path(session_id: &str, cwd: &std::path::Path) -> Option<PathBuf> {
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

pub(crate) fn claude_native_runtime(
    session_id: &str,
    cwd: &std::path::Path,
    command: &str,
    args: &[String],
) -> NativeRuntimeMeta {
    claude_native_runtime_with_home(session_id, cwd, command, args, None)
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
/// drivers swap it for the real harness invocation. Shared with the rmux
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
            if let Some(model) = cfg
                .model
                .as_deref()
                .filter(|model| !model.trim().is_empty())
            {
                args.push("--model".to_string());
                args.push(model.to_string());
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

    let mut tmux = tokio::process::Command::new("tmux");
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
    .arg(format!("ORGASMIC_RUN_ID={}", plan.run_id))
    .arg("-c")
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
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(DriverError::Transport(format!(
            "tmux new-session failed (exit {}): {}",
            output.status.code().unwrap_or(-1),
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
        let _ = tokio::process::Command::new("tmux")
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
                let mut cmd = tokio::process::Command::new("tmux");
                cmd.args(["load-buffer", "-b", &buffer_name, "-"])
                    .stdin(Stdio::from(input))
                    .stdout(Stdio::null())
                    .stderr(Stdio::piped());
                Ok(cmd)
            })
            .await?;
    } else {
        let mut child = tokio::process::Command::new("tmux")
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
    run_tmux(
        &["paste-buffer", "-p", "-b", &buffer_name, "-t", session],
        send_child,
        cancel,
    )
    .await?;
    let _ = run_tmux(&["delete-buffer", "-b", &buffer_name], send_child, cancel).await;
    Ok(())
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
                let mut cmd = tokio::process::Command::new("tmux");
                for arg in &args {
                    cmd.arg(arg);
                }
                cmd.stdout(Stdio::null()).stderr(Stdio::piped());
                cmd.kill_on_drop(true);
                Ok(cmd)
            })
            .await
    } else {
        let child = tokio::process::Command::new("tmux")
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

async fn capture_pane(session: &str) -> Result<String, DriverError> {
    let output = tokio::process::Command::new("tmux")
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
    let output = tokio::process::Command::new("tmux")
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
/// files in this folder?") as a numbered menu. Shared by the rmux driver. Used
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
    let _ = tokio::process::Command::new("tmux")
        .args(["kill-session", "-t", session])
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .await;
}

fn kill_tmux_session_sync(session: &str) {
    let _ = StdCommand::new("tmux")
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
    use serde_json::Value;
    use std::collections::VecDeque;
    #[cfg(unix)]
    use std::os::unix::fs::PermissionsExt;
    use std::sync::atomic::AtomicUsize;
    use std::sync::Mutex;

    /// Serialize real-tmux/rmux tests across ALL test binaries: they spawn real
    /// mux daemons and contend under `cargo test --workspace` (TASK-X0ZVE). An
    /// advisory flock on a shared temp path lets at most one run at a time,
    /// cross-process. Held for the whole test via the returned guard.
    fn live_session_guard() -> LiveSessionGuard {
        let path = std::env::temp_dir().join("orgasmic-live-session-tests.lock");
        let file = std::fs::OpenOptions::new()
            .create(true)
            .truncate(false)
            .write(true)
            .open(&path)
            .expect("open live-session lock file");
        // MSRV 1.87: call fs2 explicitly — std's File::lock_exclusive (1.89) shadows it.
        fs2::FileExt::lock_exclusive(&file).expect("flock live-session lock");
        LiveSessionGuard(file)
    }

    struct LiveSessionGuard(std::fs::File);
    impl Drop for LiveSessionGuard {
        fn drop(&mut self) {
            let _ = fs2::FileExt::unlock(&self.0);
        }
    }

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

    async fn tmux_spawn_usable() -> bool {
        if !tmux_available() {
            return false;
        }
        let session = format!(
            "orgasmic-test-probe-{}",
            chrono::Utc::now().timestamp_nanos_opt().unwrap_or(0)
        );
        let status = tokio::process::Command::new("tmux")
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

    fn ctx(run_id: &str, kind: RunKind) -> DriverContext {
        DriverContext {
            identity: RuntimeIdentity::new(run_id, "boot-test"),
            run_kind: kind,
            task_id: "TASK-006".into(),
            worker_id: "implementer-claude-tmux".into(),
            project_id: Some("orgasmic".into()),
            worktree: None,
            babysitter_target: None,
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
        if !tmux_spawn_usable().await || !command_available("sleep") {
            eprintln!("skipping gated launch gap regression: tmux/sleep unavailable");
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
        let output = tokio::process::Command::new("tmux")
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
        let _ = tokio::process::Command::new("tmux")
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
    async fn babysitter_cannot_transition_state() {
        let d = driver();
        let mut s = d
            .acquire(ctx("run-bs", RunKind::Babysitter), inert_config())
            .await
            .unwrap();
        let _ready = s.events.recv().await.unwrap();
        let err = s
            .control
            .transition_state(TransitionRequest {
                from: "ready".into(),
                to: "in_progress".into(),
                reason: "x".into(),
            })
            .await
            .unwrap_err();
        assert!(matches!(err, DriverError::WorkerToolBlocked(_)));
    }

    #[tokio::test]
    async fn implementer_cannot_invoke_babysitter_tool() {
        let d = driver();
        let mut s = d
            .acquire(ctx("run-im", RunKind::Worker), inert_config())
            .await
            .unwrap();
        let _ready = s.events.recv().await.unwrap();
        let err = s
            .control
            .babysitter_action(BabysitterRequest {
                tool: orgasmic_core::BabysitterTool::Poke,
                target_run: "run-target".into(),
                payload: Value::Null,
            })
            .await
            .unwrap_err();
        assert!(matches!(err, DriverError::BabysitterToolBlocked(_)));
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

    /// Real-tmux smoke. Skipped on hosts without `tmux` on PATH so CI
    /// without tmux still passes. When tmux is present we verify the
    /// driver actually spawns + tears down a session.
    #[tokio::test]
    async fn real_tmux_session_lifecycle() {
        let _live_guard = live_session_guard();
        if !tmux_spawn_usable().await {
            eprintln!("skipping real_tmux_session_lifecycle: tmux unavailable or unusable");
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
        let listed = std::process::Command::new("tmux")
            .args(["has-session", "-t", &session_name])
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .unwrap();
        assert!(listed.success(), "tmux session should exist");
        s.control.release("done").await.unwrap();
        // Give tmux a moment to actually tear down.
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        let listed = std::process::Command::new("tmux")
            .args(["has-session", "-t", &session_name])
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .unwrap();
        assert!(!listed.success(), "tmux session should be gone");
    }

    #[tokio::test]
    async fn real_tmux_session_exports_orgasmic_run_id() {
        // `orgasmic manager register` (dec_3Y2E1) recognises "I am already
        // supervised" by reading ORGASMIC_RUN_ID from its own environment —
        // prove the spawned pane actually has it set, not just that the
        // spawn plan carries a run id.
        let _live_guard = live_session_guard();
        if !tmux_spawn_usable().await {
            eprintln!(
                "skipping real_tmux_session_exports_orgasmic_run_id: tmux unavailable or unusable"
            );
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
        if !tmux_spawn_usable().await {
            eprintln!(
                "skipping real_tmux_control_drop_without_release_kills_session: tmux unavailable or unusable"
            );
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
            let listed = std::process::Command::new("tmux")
                .args(["has-session", "-t", &session_name])
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .status()
                .unwrap();
            assert!(listed.success(), "tmux session should exist before drop");
            session_name
        };
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        let listed = std::process::Command::new("tmux")
            .args(["has-session", "-t", &session_name])
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .unwrap();
        assert!(!listed.success(), "tmux session should be gone after drop");
    }

    /// Real claude TUI smoke. Skipped unless both `tmux` and `claude` are
    /// available. This verifies the prompt-ready detector against the live
    /// pane before the driver pastes an initial prompt.
    #[tokio::test]
    async fn real_claude_input_ready_smoke() {
        if !tmux_spawn_usable().await || !command_available("claude") {
            eprintln!("skipping real_claude_input_ready_smoke: tmux/claude unavailable");
            return;
        }
        if std::env::var_os("ORGASMIC_RUN_REAL_CLAUDE_SMOKE").is_none() {
            eprintln!(
                "skipping real_claude_input_ready_smoke: set ORGASMIC_RUN_REAL_CLAUDE_SMOKE=1 to exercise real claude"
            );
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
        if !tmux_spawn_usable().await {
            eprintln!(
                "skipping real_tmux_acquire_returns_before_prompt_delivery: tmux unavailable"
            );
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
        if !tmux_spawn_usable().await || !command_available("bash") {
            eprintln!(
                "skipping real_tmux_early_exit_without_finalize_is_failure: tmux/bash unavailable"
            );
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
        if !tmux_spawn_usable().await {
            eprintln!("skipping real_tmux_prompt_bundle_is_consumed: tmux unavailable or unusable");
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
            let output = std::process::Command::new("tmux")
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
        if !tmux_spawn_usable().await {
            eprintln!(
                "skipping real_tmux_attach_proves_existing_session: tmux unavailable or unusable"
            );
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
            babysitter_target: None,
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
}
