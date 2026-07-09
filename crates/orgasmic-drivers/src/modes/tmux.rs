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

use std::path::PathBuf;
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
use tokio::sync::mpsc;
use tokio::task::JoinHandle;

use orgasmic_core::{DriverEvent, RuntimeIdentity, TextStream};

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
    #[serde(default)]
    prompt_bundle_text: Option<String>,
    #[serde(
        default = "default_input_ready_timeout",
        deserialize_with = "deserialize_duration_secs"
    )]
    input_ready_timeout: Duration,
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

        let capture_task = if !inert {
            spawn_tmux_session(&session_name, &spawn_plan).await?;
            let task = start_capture_loop(
                session_name.clone(),
                tx.clone(),
                spawn_plan.eot_marker.clone(),
                terminal_emitted.clone(),
            );
            // Deliver the dispatch prompt in the *background* so `acquire`
            // returns as soon as the session is spawned (~100ms) instead of
            // blocking the HTTP handler for the input-ready wait (up to 10s) +
            // paste. Blocking here is what raced the CLI's 10s timeout and left
            // a zombie lease. A delivery failure surfaces as a fatal
            // DriverError on the stream, not a late `acquire()` error.
            if let Some(prompt) = spawn_plan.initial_prompt.clone() {
                let session = session_name.clone();
                let command = spawn_plan.command.clone();
                let timeout = cfg.input_ready_timeout;
                let deliver_tx = tx.clone();
                let deliver_terminal = terminal_emitted.clone();
                tokio::spawn(async move {
                    deliver_prompt(
                        &session,
                        &command,
                        &prompt,
                        timeout,
                        &deliver_tx,
                        &deliver_terminal,
                    )
                    .await;
                });
            }
            Some(task)
        } else {
            None
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
        if let Some(cont) = ctx.continuation.as_ref() {
            // Surface the continuation context as a system text chunk so the
            // session JSONL shows what the worker was seeded with.
            let _ = tx
                .send(DriverEvent::TextChunk {
                    stream: TextStream::System,
                    chunk: format!(
                        "continuation: previous_run={} acceptance_criteria_count={}",
                        cont.previous_run,
                        cont.acceptance_criteria.len()
                    ),
                    seq: 0,
                })
                .await;
        }

        Ok(DriverSession {
            identity: ctx.identity.clone(),
            pid: None,
            events: rx,
            control: Box::new(TmuxTuiControl {
                events: tx,
                session_name,
                kind: ctx.run_kind,
                inert,
                capture_task,
                terminal_emitted,
                kill_on_drop: true,
                released: false,
            }),
            native_runtime: spawn_plan.native_runtime,
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

        Ok(AttachOutcome::Attached(Attached {
            session: Box::new(DriverSession {
                identity: ctx.identity.clone(),
                pid: None,
                events: rx,
                control: {
                    let terminal_emitted = Arc::new(AtomicBool::new(false));
                    let capture_task = start_capture_loop(
                        session_name.clone(),
                        tx.clone(),
                        tmux_eot_marker(&ctx.identity.run_id),
                        terminal_emitted.clone(),
                    );
                    Box::new(TmuxTuiControl {
                        events: tx.clone(),
                        session_name: session_name.clone(),
                        kind: ctx.run_kind,
                        inert: false,
                        capture_task: Some(capture_task),
                        terminal_emitted,
                        kill_on_drop: false,
                        released: false,
                    })
                },
                native_runtime: None,
            }),
        }))
    }
}

struct TmuxTuiControl {
    events: mpsc::Sender<DriverEvent>,
    session_name: String,
    kind: RunKind,
    inert: bool,
    capture_task: Option<JoinHandle<()>>,
    terminal_emitted: Arc<AtomicBool>,
    kill_on_drop: bool,
    released: bool,
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
        let _ = self
            .events
            .send(DriverEvent::TransitionState {
                from: req.from.clone(),
                to: req.to.clone(),
                reason: req.reason.clone(),
            })
            .await;
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
        let _ = self
            .events
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
        Ok(BabysitterAck {
            accepted: true,
            message: None,
        })
    }

    async fn release(&mut self, reason: &str) -> Result<(), DriverError> {
        if self.released {
            return Ok(());
        }
        self.released = true;
        if let Some(task) = self.capture_task.take() {
            task.abort();
        }
        if !self.inert {
            kill_tmux_session(&self.session_name).await;
        }
        if !self.terminal_emitted.swap(true, Ordering::SeqCst) {
            let _ = self
                .events
                .send(DriverEvent::RunComplete {
                    summary: Some(reason.to_string()),
                })
                .await;
        }
        Ok(())
    }
}

impl Drop for TmuxTuiControl {
    fn drop(&mut self) {
        if let Some(task) = self.capture_task.take() {
            task.abort();
        }
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
    initial_prompt: Option<String>,
    eot_marker: String,
    /// Harness-aware native runtime identity recorded into the session JSONL.
    /// `None` when the harness has no known native session semantics.
    native_runtime: Option<NativeRuntimeMeta>,
}

fn build_spawn_plan(cfg: &TmuxTuiConfig, ctx: &DriverContext, harness: &str) -> TmuxSpawnPlan {
    let cwd = cfg
        .cwd
        .clone()
        .or_else(|| ctx.worktree.clone())
        .unwrap_or_else(|| std::env::current_dir().unwrap_or_else(|_| PathBuf::from("/tmp")));
    let eot_marker = tmux_eot_marker(&ctx.identity.run_id);
    let initial_prompt = cfg
        .prompt_bundle_text
        .as_deref()
        .filter(|bundle| !bundle.trim().is_empty())
        .map(|bundle| prompt_with_eot_instruction(bundle, &ctx.identity.run_id));

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

    let is_claude = harness == "claude" && command == "claude";
    if is_claude {
        if !args
            .iter()
            .any(|arg| arg == "--dangerously-skip-permissions")
        {
            args.push("--dangerously-skip-permissions".to_string());
        }
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

    let native_runtime = if is_claude {
        let session_id = claude_session_id(&ctx.identity.runtime_id);
        Some(claude_native_runtime(&session_id, &cwd, &command, &args))
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
        initial_prompt,
        eot_marker,
        native_runtime,
    }
}

/// Deterministic Claude native session id pinned to the run's runtime UUID.
/// The runtime_id is already a UUID, so it satisfies `claude --session-id`.
pub(crate) fn claude_session_id(runtime_id: &str) -> String {
    runtime_id.to_string()
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
    let mut launch_argv = vec![command.to_string()];
    launch_argv.extend(args.iter().cloned());
    // Resume forks the prior conversation into a fresh session id (dec_052).
    let resume_argv = vec![
        "claude".to_string(),
        "--resume".to_string(),
        session_id.to_string(),
        "--fork-session".to_string(),
        "--dangerously-skip-permissions".to_string(),
    ];
    NativeRuntimeMeta {
        provider: "claude".to_string(),
        session_id: Some(session_id.to_string()),
        session_path: claude_session_path(session_id, cwd),
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

pub(crate) fn tmux_eot_marker(run_id: &str) -> String {
    format!("[orgasmic-eot:{run_id}]")
}

pub(crate) fn prompt_with_eot_instruction(bundle: &str, run_id: &str) -> String {
    let mut prompt = String::new();
    prompt.push_str(bundle.trim());
    prompt.push_str("\n\norgasmic tmux-tui control instruction:\n");
    prompt.push_str("When all requested work and verification is complete, print a final end-of-turn marker on its own line. Compose the marker as '[' + 'orgasmic-eot:' + the run id below + ']'.\n");
    prompt.push_str("run_id: ");
    prompt.push_str(run_id);
    prompt.push('\n');
    prompt
}

/// Initial session geometry. Matches the daemon's PTY-attach bridge init size
/// so the wrapped TUI lays out once instead of repainting on first attach.
const TMUX_SESSION_COLS: &str = "200";
const TMUX_SESSION_ROWS: &str = "50";

async fn spawn_tmux_session(session: &str, plan: &TmuxSpawnPlan) -> Result<(), DriverError> {
    // After a daemon crash, a previous tmux pane may still hold this name.
    kill_tmux_session(session).await;

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
        "-c",
    ])
    .arg(&plan.cwd)
    .arg("--")
    .arg(&plan.command);
    for a in &plan.args {
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
    Ok(())
}

async fn paste_text_into_pane(session: &str, text: &str) -> Result<(), DriverError> {
    if text.is_empty() {
        return Ok(());
    }
    let buffer_name = format!("orgasmic-{}", sanitize_tmux_name(session));
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
    let output = child
        .wait_with_output()
        .await
        .map_err(|e| DriverError::Transport(format!("tmux load-buffer wait: {e}")))?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(DriverError::Transport(format!(
            "tmux load-buffer failed: {}",
            stderr.trim()
        )));
    }
    run_tmux(&["paste-buffer", "-p", "-b", &buffer_name, "-t", session]).await?;
    let _ = run_tmux(&["delete-buffer", "-b", &buffer_name]).await;
    Ok(())
}

async fn send_keys(session: &str, keys: &[String]) -> Result<(), DriverError> {
    if keys.is_empty() {
        return Ok(());
    }
    let mut args = vec!["send-keys", "-t", session];
    for key in keys {
        args.push(key.as_str());
    }
    run_tmux(&args).await
}

async fn run_tmux(args: &[&str]) -> Result<(), DriverError> {
    let output = tokio::process::Command::new("tmux")
        .args(args)
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .output()
        .await
        .map_err(|e| DriverError::Transport(format!("tmux {:?}: {e}", args)))?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(DriverError::Transport(format!(
            "tmux {:?} failed: {}",
            args,
            stderr.trim()
        )));
    }
    Ok(())
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

fn start_capture_loop(
    session_name: String,
    events: mpsc::Sender<DriverEvent>,
    eot_marker: String,
    terminal_emitted: Arc<AtomicBool>,
) -> JoinHandle<()> {
    tokio::spawn(async move {
        capture_loop(session_name, events, eot_marker, terminal_emitted).await;
    })
}

async fn capture_loop(
    session: String,
    events: mpsc::Sender<DriverEvent>,
    eot_marker: String,
    terminal_emitted: Arc<AtomicBool>,
) {
    let mut poll = tokio::time::interval(Duration::from_millis(500));
    poll.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    let mut previous = String::new();
    let mut recent_chunk = String::new();
    let mut seq = 0_u64;
    let mut approval_sent = false;

    loop {
        poll.tick().await;
        if terminal_emitted.load(Ordering::SeqCst) {
            break;
        }
        if !has_tmux_session(&session).await.unwrap_or(false) {
            emit_fatal_driver_error_once(
                &events,
                &terminal_emitted,
                format!("tmux session {session} ended before EOT marker"),
            )
            .await;
            break;
        }
        let pane = match capture_pane(&session).await {
            Ok(pane) => pane,
            Err(e) => {
                let _ = events
                    .send(DriverEvent::DriverError {
                        fatal: false,
                        message: e.to_string(),
                    })
                    .await;
                continue;
            }
        };
        if pane != previous {
            let chunk = pane_delta(&previous, &pane);
            let chunk = strip_ansi_codes(&chunk);
            if !chunk.trim().is_empty() && chunk != recent_chunk {
                seq += 1;
                recent_chunk = chunk.clone();
                let _ = events
                    .send(DriverEvent::TextChunk {
                        stream: TextStream::Assistant,
                        chunk,
                        seq,
                    })
                    .await;
            }
            if !approval_sent && pane_requests_approval(&pane) {
                approval_sent = true;
                let _ = send_keys(&session, &[String::from("y"), String::from("Enter")]).await;
            }
            if pane_contains_eot_marker(&pane, &eot_marker) {
                emit_run_complete_once(
                    &events,
                    &terminal_emitted,
                    Some("tmux TUI end-of-turn marker observed".to_string()),
                )
                .await;
                break;
            }
            previous = pane;
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
async fn deliver_prompt(
    session: &str,
    command: &str,
    prompt: &str,
    input_ready_timeout: Duration,
    events: &mpsc::Sender<DriverEvent>,
    terminal_emitted: &AtomicBool,
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
        paste_text_into_pane(session, prompt).await?;
        send_keys(session, &[String::from("Enter")]).await
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
                        let _ = send_keys(session, &[String::from("Enter")]).await;
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

pub(crate) fn pane_delta(previous: &str, current: &str) -> String {
    if previous.is_empty() {
        return current.to_string();
    }

    if previous == current {
        String::new()
    } else if let Some(delta) = current.strip_prefix(previous) {
        delta.to_string()
    } else {
        let previous_lines = split_lines_keepends(previous);
        let current_lines = split_lines_keepends(current);
        let max_overlap = previous_lines.len().min(current_lines.len());
        for overlap in (1..=max_overlap).rev() {
            if previous_lines[previous_lines.len() - overlap..] == current_lines[..overlap] {
                return current_lines[overlap..].concat();
            }
        }
        redraw_delta(current)
    }
}

fn split_lines_keepends(s: &str) -> Vec<&str> {
    s.split_inclusive('\n').collect()
}

fn redraw_delta(current: &str) -> String {
    const REDRAW_MARKER: &str = "[orgasmic-tui-redraw]\n";
    const MAX_REDRAW_BYTES: usize = 10 * 1024;
    let content = if current.len() <= MAX_REDRAW_BYTES {
        current
    } else {
        let mut start = current.len() - MAX_REDRAW_BYTES;
        while !current.is_char_boundary(start) {
            start += 1;
        }
        &current[start..]
    };
    format!("{REDRAW_MARKER}{content}")
}

pub(crate) fn pane_contains_eot_marker(pane: &str, marker: &str) -> bool {
    // Wrapped agent TUIs do not render the marker verbatim: a markdown pass
    // may strip the `[...]` (link-label syntax, observed with opencode), and
    // full-screen layouts can pack sidebar text onto the same rendered row.
    // Accept a line that *starts* with the bracketed marker or its
    // bracket-less core — start-anchored so prose that merely mentions the
    // marker mid-sentence still does not complete the run.
    let core = marker
        .strip_prefix('[')
        .and_then(|m| m.strip_suffix(']'))
        .unwrap_or(marker);
    pane.lines().any(|line| {
        let trimmed = line.trim();
        trimmed.starts_with(marker) || trimmed.starts_with(core)
    })
}

fn pane_requests_approval(pane: &str) -> bool {
    let lower = pane.to_ascii_lowercase();
    lower.contains("approve")
        && (lower.contains(" yes") || lower.contains("[y") || lower.contains("allow"))
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

async fn emit_run_complete_once(
    events: &mpsc::Sender<DriverEvent>,
    terminal_emitted: &AtomicBool,
    summary: Option<String>,
) {
    if !terminal_emitted.swap(true, Ordering::SeqCst) {
        let _ = events.send(DriverEvent::RunComplete { summary }).await;
    }
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
            continuation: None,
        }
    }

    #[tokio::test]
    async fn inert_acquire_emits_ready() {
        let d = driver();
        let mut s = d
            .acquire(ctx("run-1", RunKind::Worker), inert_config())
            .await
            .unwrap();
        let ev = s.events.recv().await.unwrap();
        assert!(matches!(ev, DriverEvent::Ready { .. }));
        s.control.release("done").await.unwrap();
        let ev2 = s.events.recv().await.unwrap();
        assert!(matches!(ev2, DriverEvent::RunComplete { .. }));
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
        let prompt = plan.initial_prompt.unwrap();
        assert!(prompt.contains("do the task"));
        assert!(!prompt.contains("[orgasmic-eot:run-plan]"));
        assert!(prompt.contains("run_id: run-plan"));
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
        let plan = build_spawn_plan(&cfg, &ctx("run-codex-placeholder", RunKind::Worker), "codex");
        assert_eq!(plan.command, "codex");
        assert!(!is_dispatch_placeholder(
            Some(plan.command.as_str()),
            &plan.args
        ));
    }

    #[test]
    fn pane_delta_and_eot_detection_are_stable() {
        let before = "hello\n";
        let after = "hello\nworld\n[orgasmic-eot:run-x]\n";
        assert_eq!(pane_delta(before, after), "world\n[orgasmic-eot:run-x]\n");
        assert!(pane_contains_eot_marker(after, "[orgasmic-eot:run-x]"));
        assert!(!pane_contains_eot_marker(after, "[orgasmic-eot:other]"));
    }

    /// Wrapped TUIs render the marker imperfectly: opencode's markdown pass
    /// strips the `[...]` link-label brackets, and full-screen layouts pack
    /// sidebar columns onto the marker's rendered row. Both must still
    /// complete the run; prose mentioning the marker mid-sentence must not.
    #[test]
    fn pane_eot_detection_tolerates_tui_rendering() {
        let marker = "[orgasmic-eot:run-x]";
        // Markdown link-label pass stripped the brackets.
        assert!(pane_contains_eot_marker("orgasmic-eot:run-x\n", marker));
        // Sidebar text rendered on the same screen row.
        assert!(pane_contains_eot_marker(
            "   orgasmic-eot:run-x          2% used\n",
            marker
        ));
        assert!(pane_contains_eot_marker(
            "  [orgasmic-eot:run-x]   ctrl+p commands\n",
            marker
        ));
        // Mid-sentence mention (instruction echo / agent narration) is not
        // a completion signal.
        assert!(!pane_contains_eot_marker(
            "I will print [orgasmic-eot:run-x] when done\n",
            marker
        ));
        assert!(!pane_contains_eot_marker(
            "compose '[' + 'orgasmic-eot:' + run id + ']'\n",
            marker
        ));
    }

    #[test]
    fn tmux_config_defaults_input_ready_timeout_to_ten_seconds() {
        let cfg: TmuxTuiConfig = serde_json::from_value(json!({})).unwrap();
        assert_eq!(cfg.input_ready_timeout, Duration::from_secs(10));
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

    #[test]
    fn pane_delta_handles_line_window_edges() {
        assert_eq!(pane_delta("", ""), "");
        assert_eq!(pane_delta("", "one line"), "one line");
        assert_eq!(pane_delta("one", "one plus"), " plus");
        assert_eq!(pane_delta("one\n", "one\ntwo\nthree\n"), "two\nthree\n");
        assert_eq!(
            pane_delta("one\ntwo\nthree\n", "two\nthree\nfour\n"),
            "four\n"
        );
        assert_eq!(pane_delta("one\ntwo\n", ""), "[orgasmic-tui-redraw]\n");
        assert_eq!(
            pane_delta("one\ntwo\n", "replacement\n"),
            "[orgasmic-tui-redraw]\nreplacement\n"
        );
    }

    #[test]
    fn pane_delta_caps_redraw_payload() {
        let current = "x".repeat(12 * 1024);
        let delta = pane_delta("old\n", &current);
        assert!(delta.starts_with("[orgasmic-tui-redraw]\n"));
        assert!(delta.len() <= "[orgasmic-tui-redraw]\n".len() + 10 * 1024);
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
            initial_prompt: None,
            eot_marker: tmux_eot_marker("run-input-ready"),
            native_runtime: None,
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

    #[tokio::test]
    async fn real_tmux_eot_marker_emits_run_complete() {
        let _live_guard = live_session_guard();
        if !tmux_spawn_usable().await || !command_available("bash") {
            eprintln!("skipping real_tmux_eot_marker_emits_run_complete: tmux/bash unavailable");
            return;
        }
        let d = driver();
        let run_id = "run-mock-eot";
        let marker = tmux_eot_marker(run_id);
        let script = format!("printf 'mock output\\n{}\\n'; sleep 60", marker);
        let cfg = DriverConfig::from_value(json!({
            "command": "bash",
            "args": ["-lc", script],
        }));
        let mut s = d.acquire(ctx(run_id, RunKind::Worker), cfg).await.unwrap();
        let _guard = SessionGuard(tmux_session_name(&s.identity));
        let ev = s.events.recv().await.unwrap();
        assert!(matches!(ev, DriverEvent::Ready { .. }));

        let mut saw_text = false;
        let mut saw_complete = false;
        for _ in 0..10 {
            let ev = tokio::time::timeout(Duration::from_secs(2), s.events.recv())
                .await
                .expect("timed out waiting for tmux event")
                .expect("event stream closed");
            match ev {
                DriverEvent::TextChunk { chunk, .. } => {
                    saw_text |= chunk.contains("mock output");
                }
                DriverEvent::RunComplete { summary } => {
                    saw_complete = true;
                    assert_eq!(
                        summary.as_deref(),
                        Some("tmux TUI end-of-turn marker observed")
                    );
                    break;
                }
                other => panic!("unexpected event before completion: {other:?}"),
            }
        }
        assert!(saw_text, "expected mock output text chunk");
        assert!(saw_complete, "expected RunComplete");
        s.control.release("cleanup").await.unwrap();
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
    async fn real_tmux_early_exit_before_eot_is_failure() {
        let _live_guard = live_session_guard();
        if !tmux_spawn_usable().await || !command_available("bash") {
            eprintln!("skipping real_tmux_early_exit_before_eot_is_failure: tmux/bash unavailable");
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
                    assert!(message.contains("ended before EOT marker"), "{message}");
                    saw_failure = true;
                    break;
                }
                DriverEvent::DriverError { fatal: false, .. } => {}
                DriverEvent::TextChunk { .. } => {}
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
            continuation: None,
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
