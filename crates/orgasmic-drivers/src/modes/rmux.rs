// orgasmic:arch_A53QX, arch_C87Z9, arch_MK2Q2
//! rmux hybrid mode driver — **bounded smoke proof** (TASK-104).
//!
//! Strategy 3 from the rmux exploration: drive a detached session through the
//! typed [`rmux_sdk`] facade (`Rmux::ensure_session`) while still relying on a
//! **separately provisioned** `rmux` binary. The SDK starts or connects to a
//! daemon, but it does *not* bundle the multiplexer itself, so the daemon
//! binary is discovered via `RMUX_SDK_DAEMON_BINARY` or PATH (`rmux`) and that
//! check is kept *distinct* from the wrapped harness binary check.
//!
//! ### Output + lifecycle over the typed SDK
//!
//! Unlike the tmux driver — which scrapes `capture-pane` on a 500 ms poll,
//! reconstructs deltas, and waits for an injected end-of-turn marker — the rmux
//! driver streams output from the daemon and learns the session is finished
//! when that stream ends (the daemon reports the pane's process exited).
//! Teardown goes through the typed `Session::kill()`, not a `kill-session`
//! shell-out. This is where rmux is genuinely better than tmux: the daemon owns
//! the buffer/lifecycle/rendering truth, so there is no marker/poll scraping to
//! be flaky about.
//!
//! Two output paths, chosen by what is being driven:
//! - **Full-screen TUIs** (the agent harnesses: claude, codex, cursor-agent,
//!   hermes) → `Pane::render_stream()`. The daemon does the terminal emulation
//!   and hands us *rendered screen snapshots*, debounced and revision-filtered.
//!   We emit the plain-text delta between successive screens — no ANSI/cursor
//!   escape noise, event-driven instead of fixed-rate.
//! - **Plain commands** (`sh`, custom argv) → `Pane::line_stream()` anchored at
//!   `Oldest`, so line-oriented output is captured verbatim from session start
//!   with no early-output race.
//!
//! The `force_render` config knob overrides the auto-detection.
//!
//! ### Honesty contract
//!
//! This is **not** a production terminal driver and it does not replace tmux.
//! When the `rmux` binary is missing, when the SDK cannot reach/start a daemon,
//! or when a capability (e.g. Web Share) is unavailable, the driver records the
//! exact reason in the `Ready` capabilities payload and degrades to inert mode
//! instead of faking a working integration. Nothing here should be read as
//! "rmux is supported"; it is a reproducible probe whose output is captured in
//! the task Evidence and `.orgasmic/decisions.org`.
//!
//! ### Token hygiene
//!
//! Web Share operator URLs and pairing tokens grant live shell access. The
//! driver only ever surfaces **redacted** operator material in events/logs and
//! never persists tokens. Spectator URLs are read-only and may be surfaced in
//! full. See [`RmuxWebShareProof`].

use std::path::{Path, PathBuf};
use std::process::{Command as StdCommand, Stdio};
use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc,
};
use std::time::Duration;

use async_trait::async_trait;
use serde::Deserialize;
use serde_json::{json, Value};
use tokio::sync::mpsc;
use tokio::task::JoinHandle;

use orgasmic_core::{DriverEvent, RuntimeIdentity, TextStream};

use crate::modes::tmux::{
    claude_native_runtime, claude_session_id, default_input_ready_timeout,
    deserialize_duration_secs, is_dispatch_placeholder, pane_contains_eot_marker, pane_delta,
    pane_has_input_prompt, pane_requests_folder_trust, prompt_with_eot_instruction,
    strip_ansi_codes, tmux_eot_marker,
};

use crate::r#trait::{
    AttachOutcome, Attached, BabysitterAck, BabysitterRequest, DriverConfig, DriverContext,
    DriverControl, DriverError, DriverSession, HarnessEventAdapter, NativeRuntimeMeta, RunKind,
    TransitionAck, TransitionRequest, UserInputAck, UserInputRequest, WorkerDriver,
};

const MODE: &str = "rmux";

/// Default mode binary name probed on PATH when `RMUX_SDK_DAEMON_BINARY` is
/// unset. Matches the crate published as `rmux` on crates.io.
const RMUX_BINARY: &str = "rmux";

/// Environment variable the rmux SDK uses to locate the daemon binary it spawns
/// on `connect_or_start`. Mirrored here so the driver's separate binary check
/// honors the same override the SDK would.
const RMUX_SDK_DAEMON_BINARY_ENV: &str = "RMUX_SDK_DAEMON_BINARY";

pub struct RmuxDriver {
    adapter: Box<dyn HarnessEventAdapter>,
}

impl RmuxDriver {
    pub fn new(adapter: Box<dyn HarnessEventAdapter>) -> Self {
        Self { adapter }
    }
}

#[derive(Debug, Clone, Deserialize, Default)]
struct RmuxConfig {
    /// Command to run inside the detached session. Defaults to a bounded
    /// harness smoke command when unset.
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
    /// pushes below, so an explicit `--model` here wins over `model`.
    #[serde(default)]
    harness_args: Vec<String>,
    /// Compiled dispatch prompt. When present (worker dispatches), it is
    /// pasted into the spawned TUI with the tmux-style end-of-turn marker
    /// instruction appended, and the render stream watches for that marker.
    #[serde(default)]
    prompt_bundle_text: Option<String>,
    /// How long to wait for the wrapped TUI's input prompt before pasting the
    /// dispatch prompt anyway. Mirrors the tmux driver knob.
    #[serde(
        default = "default_input_ready_timeout",
        deserialize_with = "deserialize_duration_secs"
    )]
    input_ready_timeout: Duration,
    /// Force inert mode even when an rmux binary is present. Test-only knob.
    #[serde(default)]
    force_inert: bool,
    /// Attempt a Web Share smoke (spectator + operator URL mint) once the
    /// session is live. Off by default so plain session smokes do not require
    /// the web feature/tunnel wiring.
    #[serde(default)]
    web_share: bool,
    /// Override the output-path auto-detection: `Some(true)` forces the
    /// rendered-screen (`render_stream`) path, `Some(false)` forces the
    /// line-oriented (`line_stream`) path. `None` (default) picks `render` for
    /// full-screen agent TUIs and `line` for plain commands.
    #[serde(default)]
    force_render: Option<bool>,
    /// Spawn the session "system-wide": detached from the orgasmic daemon so it
    /// survives a daemon restart/rebuild. The rmux SDK already starts its daemon
    /// in its own session (`setsid`) on a stable per-user socket, so the session
    /// itself outlives us; this flag additionally suppresses the
    /// kill-session-on-drop backstop so a graceful daemon shutdown can never
    /// reap the session. Explicit `release` (operator stop) still tears it down.
    /// Defaults ON for the manager (set by the UI), OFF otherwise.
    #[serde(default)]
    system_wide: bool,
    /// Hot-session runs: keep the dispatch EOT marker in prompts (and followups)
    /// for per-round stall detection and operator visibility, but do **not**
    /// treat marker appearance as run completion — the run ends on pane/process
    /// exit only. The stall babysitter is a separate run that reads the pane
    /// itself and is unaffected by this run's completion wiring.
    #[serde(default)]
    persistent: bool,
}

/// Result of the separate rmux-binary discovery (kept distinct from the
/// harness binary so the catalog can report each independently).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RmuxBinaryProbe {
    /// Whether a usable rmux binary was found.
    pub found: bool,
    /// Resolved path or binary name, when found.
    pub path: Option<String>,
    /// Where the binary was resolved from: `env` or `path`.
    pub source: Option<&'static str>,
}

impl RmuxBinaryProbe {
    fn missing() -> Self {
        Self {
            found: false,
            path: None,
            source: None,
        }
    }
}

/// Discover the rmux daemon binary the SDK would spawn, **separately** from any
/// wrapped harness binary. Honors `RMUX_SDK_DAEMON_BINARY` first (the same
/// override `rmux_sdk` consults), then falls back to a PATH probe for `rmux`.
pub fn probe_rmux_binary() -> RmuxBinaryProbe {
    if let Some(explicit) = std::env::var_os(RMUX_SDK_DAEMON_BINARY_ENV) {
        let path = PathBuf::from(&explicit);
        // An explicit override may be an absolute path or a bare name resolved
        // on PATH. Treat an existing file as found; otherwise still report the
        // configured value so the catalog/event shows what was attempted.
        let found = path.is_file() || binary_on_path(&path.to_string_lossy());
        return RmuxBinaryProbe {
            found,
            path: Some(path.to_string_lossy().into_owned()),
            source: Some("env"),
        };
    }
    if let Some(resolved) = which_on_path(RMUX_BINARY) {
        return RmuxBinaryProbe {
            found: true,
            path: Some(resolved),
            source: Some("path"),
        };
    }
    RmuxBinaryProbe::missing()
}

fn which_on_path(binary: &str) -> Option<String> {
    let paths = std::env::var_os("PATH")?;
    std::env::split_paths(&paths)
        .map(|dir| dir.join(binary))
        .find(|candidate| candidate.is_file())
        .map(|p| p.to_string_lossy().into_owned())
}

fn binary_on_path(binary: &str) -> bool {
    if Path::new(binary).is_absolute() {
        return Path::new(binary).is_file();
    }
    which_on_path(binary).is_some()
}

/// Whether the wrapped harness binary is available. Distinct from the rmux
/// binary probe (acceptance criterion: catalog checks them separately).
fn harness_binary_available(command: &str) -> bool {
    StdCommand::new("which")
        .arg(command)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

/// The reason a smoke run cannot drive a real rmux session, if any.
fn inert_reason(cfg: &RmuxConfig, probe: &RmuxBinaryProbe, command: &str) -> Option<String> {
    if cfg.force_inert {
        return Some("force_inert".to_string());
    }
    if !probe.found {
        return Some("rmux_binary_missing".to_string());
    }
    if !harness_binary_available(command) {
        return Some(format!("harness_binary_missing:{command}"));
    }
    None
}

#[derive(Debug, Clone)]
struct RmuxSpawnPlan {
    command: String,
    args: Vec<String>,
    cwd: PathBuf,
    /// Dispatch prompt (with the EOT-marker instruction appended) to paste
    /// into the spawned TUI once it is input-ready. `None` for plain runs.
    initial_prompt: Option<String>,
    /// Marker whose appearance in the rendered screen completes the run.
    eot_marker: String,
    /// Default output path when `force_render` is unset: rendered screens for
    /// known agent TUIs, and for `custom` dispatches (the wrapped CLI is an
    /// interactive agent we paste the brief into, not a line-oriented command).
    use_render_default: bool,
    native_runtime: Option<NativeRuntimeMeta>,
    persistent: bool,
}

fn build_spawn_plan(cfg: &RmuxConfig, ctx: &DriverContext, harness: &str) -> RmuxSpawnPlan {
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

    // The daemon's dispatch path stages a placeholder command for every
    // worker; like the tmux driver, swap it for the real harness invocation
    // instead of executing the placeholder verbatim. For the `custom` harness
    // the real invocation IS `harness_args` (argv[0] + args) — the template's
    // `:HARNESS_ARGS:` is the whole wrapped command line.
    let staged_placeholder = is_dispatch_placeholder(cfg.command.as_deref(), &cfg.args);
    let mut harness_args_consumed = false;
    // The dispatch placeholder is the daemon's "swap me for the real harness"
    // sentinel; honor it for any harness (codex included), not just claude/custom.
    let (command, mut args) = if cfg.command.is_none() || staged_placeholder {
        if harness == "custom" && !cfg.harness_args.is_empty() {
            harness_args_consumed = true;
            (cfg.harness_args[0].clone(), cfg.harness_args[1..].to_vec())
        } else {
            default_command_for_harness(harness)
        }
    } else {
        (
            cfg.command.clone().unwrap_or_else(|| "sh".to_string()),
            cfg.args.clone(),
        )
    };

    // Worker/launch-supplied harness argv rides along whenever we are running
    // a real harness CLI (not the inert dispatch placeholder). It lands before
    // the guarded pushes below so user-specified flags take precedence.
    if !harness_args_consumed
        && !cfg.harness_args.is_empty()
        && !is_dispatch_placeholder(Some(command.as_str()), &args)
    {
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
        // Deterministic native Claude session identity (mirrors tmux): pin
        // --session-id to the run's runtime UUID so recovery can resume it.
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
    let use_render_default =
        command_is_tui(&command) || (harness == "custom" && initial_prompt.is_some());
    RmuxSpawnPlan {
        command,
        args,
        cwd,
        initial_prompt,
        eot_marker,
        use_render_default,
        native_runtime,
        persistent: cfg.persistent,
    }
}

/// A `custom` dispatch (compiled prompt staged) with no `harness_args` would
/// spawn the fallback shell and paste the brief into it — executing prose as
/// shell commands. Refuse the config instead. Template parsing already
/// enforces `:HARNESS_ARGS:`; this guards hand-rolled driver configs.
fn custom_dispatch_misconfig(harness: &str, cfg: &RmuxConfig) -> Option<String> {
    let has_prompt = cfg
        .prompt_bundle_text
        .as_deref()
        .map(|bundle| !bundle.trim().is_empty())
        .unwrap_or(false);
    (harness == "custom" && has_prompt && cfg.harness_args.is_empty()).then(|| {
        "custom harness dispatch requires harness_args (the wrapped CLI argv); \
         refusing to paste a dispatch prompt into a bare shell"
            .to_string()
    })
}

/// Bounded default command per harness. Kept intentionally small: the smoke
/// proves session lifecycle, not a full agent turn.
fn default_command_for_harness(harness: &str) -> (String, Vec<String>) {
    match harness {
        "codex" => ("codex".to_string(), Vec::new()),
        "claude" => (
            "claude".to_string(),
            vec!["--dangerously-skip-permissions".to_string()],
        ),
        "cursor-agent" => ("cursor-agent".to_string(), Vec::new()),
        "hermes" => (
            "hermes".to_string(),
            vec!["chat".to_string(), "--tui".to_string()],
        ),
        // Bare terminal session: the operator's login shell, no agent CLI.
        // They drive any tool by hand through the attached xterm.
        "custom" => (
            std::env::var("SHELL").unwrap_or_else(|_| "sh".to_string()),
            Vec::new(),
        ),
        _ => ("sh".to_string(), Vec::new()),
    }
}

/// Whether a resolved command launches a full-screen agent TUI (which should be
/// captured as rendered screens) rather than a line-oriented program. Matches
/// the agent binaries in [`default_command_for_harness`]; everything else
/// (`sh`, custom argv) is treated as line-oriented.
fn command_is_tui(command: &str) -> bool {
    // Compare on the basename so an absolute path (e.g. `/usr/local/bin/claude`)
    // is still recognised.
    let base = Path::new(command)
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or(command);
    matches!(base, "claude" | "codex" | "cursor-agent" | "hermes")
}

/// Stable rmux session name for a run. rmux's `SessionName::new` sanitizes `.`
/// and `:`, so this is already conservative.
pub fn rmux_session_name(identity: &RuntimeIdentity) -> String {
    format!("orgasmic-rmux-{}-{}", identity.run_id, identity.runtime_id)
}

/// Web Share smoke outcome. Carries only **redacted** operator material.
#[derive(Debug, Clone, Default)]
struct RmuxWebShareProof {
    attempted: bool,
    /// Full spectator URL — read-only, safe to surface.
    spectator_url: Option<String>,
    /// Whether an operator URL was minted. We never surface the raw URL/token.
    operator_minted: bool,
    /// Redacted operator URL form (scheme/host kept, token elided).
    operator_url_redacted: Option<String>,
    /// Exact limitation captured when a URL could not be produced.
    limitation: Option<String>,
}

impl RmuxWebShareProof {
    fn to_capabilities(&self) -> Value {
        json!({
            "attempted": self.attempted,
            "spectator_url": self.spectator_url,
            "operator_minted": self.operator_minted,
            "operator_url_redacted": self.operator_url_redacted,
            "limitation": self.limitation,
        })
    }
}

/// Redact an operator URL so logs/events never carry the live token. Keeps the
/// scheme + host and the path shape, replacing any token-bearing query/fragment
/// with a placeholder.
fn redact_operator_url(url: &str) -> String {
    let (head, _tail) = match url.find(['?', '#']) {
        Some(idx) => url.split_at(idx),
        None => (url, ""),
    };
    format!("{head}#<operator-token-redacted>")
}

#[async_trait]
impl WorkerDriver for RmuxDriver {
    fn transport(&self) -> &'static str {
        MODE
    }

    fn harness(&self) -> Option<&'static str> {
        Some(self.adapter.harness())
    }

    fn validate(&self, config: &DriverConfig) -> Result<(), DriverError> {
        let cfg: RmuxConfig = serde_json::from_value(config.0.clone())
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
        let cfg: RmuxConfig = serde_json::from_value(config.0.clone())
            .map_err(|e| DriverError::InvalidConfig(e.to_string()))?;
        let (tx, rx) = mpsc::channel(64);
        let harness = cfg
            .harness
            .as_deref()
            .unwrap_or_else(|| self.adapter.harness());
        if let Some(reason) = custom_dispatch_misconfig(harness, &cfg) {
            return Err(DriverError::InvalidConfig(reason));
        }
        let plan = build_spawn_plan(&cfg, &ctx, harness);
        let probe = if cfg.force_inert {
            RmuxBinaryProbe::missing()
        } else {
            probe_rmux_binary()
        };
        let inert_reason = inert_reason(&cfg, &probe, &plan.command);
        let inert = inert_reason.is_some();
        let session_name = rmux_session_name(&ctx.identity);
        let terminal_emitted = Arc::new(AtomicBool::new(false));

        let live = if inert {
            None
        } else {
            // Attempt to drive a real detached session through the SDK. Any
            // failure here is captured honestly and degrades to inert; it never
            // fabricates success.
            let rmux_bin = probe
                .path
                .clone()
                .unwrap_or_else(|| RMUX_BINARY.to_string());
            match run_live_session(
                &session_name,
                &rmux_bin,
                &plan,
                &cfg,
                tx.clone(),
                terminal_emitted.clone(),
            )
            .await
            {
                Ok(live) => Some(live),
                Err(err) => {
                    // Fall back to inert, surfacing the precise SDK/daemon error.
                    let _ = tx
                        .send(DriverEvent::Ready {
                            protocol_version: "rmux-smoke/1".into(),
                            capabilities: ready_capabilities(
                                true,
                                Some(format!("sdk_unavailable:{err}")),
                                &ctx,
                                &plan,
                                &probe,
                                None,
                                &RmuxWebShareProof::default(),
                            ),
                        })
                        .await;
                    return Ok(DriverSession {
                        identity: ctx.identity.clone(),
                        pid: None,
                        events: rx,
                        control: Box::new(RmuxControl::inert(tx, ctx.run_kind)),
                        native_runtime: plan.native_runtime.clone(),
                    });
                }
            }
        };

        let (web_share, capture_task, session) = match live {
            Some(live) => (live.web_share, Some(live.capture_task), Some(live.session)),
            None => (RmuxWebShareProof::default(), None, None),
        };

        let _ = tx
            .send(DriverEvent::Ready {
                protocol_version: "rmux-smoke/1".into(),
                capabilities: ready_capabilities(
                    inert,
                    inert_reason,
                    &ctx,
                    &plan,
                    &probe,
                    (!inert).then(|| session_name.clone()),
                    &web_share,
                ),
            })
            .await;

        // A live (non-inert) run owns a detached rmux session that must be
        // reaped on release/drop, or it lingers on the rmux daemon. The typed
        // `Session` handle is the teardown path (`Session::kill`); inert runs
        // own no session.
        let rmux_bin = probe
            .path
            .clone()
            .unwrap_or_else(|| RMUX_BINARY.to_string());
        Ok(DriverSession {
            identity: ctx.identity.clone(),
            pid: None,
            events: rx,
            control: if inert {
                Box::new(RmuxControl::inert(tx, ctx.run_kind))
            } else {
                Box::new(RmuxControl {
                    events: tx,
                    kind: ctx.run_kind,
                    capture_task,
                    terminal_emitted,
                    released: false,
                    session,
                    // A system-wide session must survive a daemon shutdown, so the
                    // implicit Drop backstop must not reap it.
                    kill_on_drop: !cfg.system_wide,
                    rmux_bin: Some(rmux_bin),
                    session_target: Some(session_name.clone()),
                    run_id: Some(ctx.identity.run_id.clone()),
                    harness_command: Some(plan.command.clone()),
                    input_ready_timeout: cfg.input_ready_timeout,
                })
            },
            native_runtime: plan.native_runtime,
        })
    }

    async fn attach(
        &self,
        ctx: DriverContext,
        config: DriverConfig,
    ) -> Result<AttachOutcome, DriverError> {
        let cfg: RmuxConfig = serde_json::from_value(config.0.clone())
            .map_err(|e| DriverError::InvalidConfig(e.to_string()))?;
        if cfg.force_inert {
            return Ok(AttachOutcome::NotReattachable);
        }
        // A reattachable session lives in the (already running) rmux daemon. Use
        // the SDK's `connect` (never `connect_or_start`) so a *missing* daemon
        // is reported as not-reattachable instead of silently spawning a fresh
        // empty one.
        let rmux = match rmux_sdk::Rmux::builder()
            .default_timeout(Duration::from_secs(5))
            .connect()
            .await
        {
            Ok(rmux) => rmux,
            Err(e) => {
                tracing::info!(error = %e, "rmux attach: no live daemon to connect to");
                return Ok(AttachOutcome::NotReattachable);
            }
        };

        let session_name_str = rmux_session_name(&ctx.identity);
        let session_name = rmux_sdk::SessionName::new(session_name_str.clone())
            .map_err(|e| DriverError::Transport(format!("rmux session name: {e}")))?;
        match rmux.has_session(session_name.clone()).await {
            Ok(true) => {}
            Ok(false) => return Ok(AttachOutcome::NotReattachable),
            Err(e) => {
                tracing::info!(error = %e, "rmux attach: has_session probe failed");
                return Ok(AttachOutcome::NotReattachable);
            }
        }

        let session = rmux
            .session(session_name)
            .await
            .map_err(|e| DriverError::Transport(format!("rmux attach session: {e}")))?;

        // Choose the same output path the original run would have used. We don't
        // have the original prompt here, so derive the command from config and
        // reuse the EOT marker so a worker that completes post-reattach still
        // finalizes. The manager run carries no marker effect (it never emits
        // one), so it simply streams until explicitly released.
        let harness = cfg
            .harness
            .as_deref()
            .unwrap_or_else(|| self.adapter.harness());
        let plan = build_spawn_plan(&cfg, &ctx, harness);
        let use_render = cfg.force_render.unwrap_or(plan.use_render_default);
        // Persistent hot sessions complete on process exit only — see
        // `run_live_session` for the same decoupling.
        let eot_marker = if cfg.persistent {
            None
        } else {
            Some(tmux_eot_marker(&ctx.identity.run_id))
        };

        let (tx, rx) = mpsc::channel(64);
        let terminal_emitted = Arc::new(AtomicBool::new(false));
        let pane = session.pane(0, 0);
        let capture_task = spawn_pane_capture(
            &pane,
            use_render,
            tx.clone(),
            terminal_emitted.clone(),
            eot_marker,
        )
        .await?;

        // No paste on reattach: the harness is already mid-conversation.
        let _ = tx
            .send(DriverEvent::Ready {
                protocol_version: "rmux-smoke/1".into(),
                capabilities: json!({
                    "inert": false,
                    "reattached": true,
                    "kind": ctx.run_kind,
                    "session": session_name_str,
                    "command": plan.command,
                }),
            })
            .await;

        Ok(AttachOutcome::Attached(Attached {
            session: Box::new(DriverSession {
                identity: ctx.identity.clone(),
                pid: None,
                events: rx,
                control: Box::new(RmuxControl {
                    events: tx,
                    kind: ctx.run_kind,
                    capture_task: Some(capture_task),
                    terminal_emitted,
                    released: false,
                    session: Some(session),
                    // A reattached session is, by definition, one we want to
                    // outlive the daemon — never reap it on an implicit Drop.
                    kill_on_drop: false,
                    rmux_bin: probe_rmux_binary()
                        .path
                        .or_else(|| Some(RMUX_BINARY.to_string())),
                    session_target: Some(session_name_str.clone()),
                    run_id: Some(ctx.identity.run_id.clone()),
                    harness_command: Some(plan.command.clone()),
                    input_ready_timeout: cfg.input_ready_timeout,
                }),
                native_runtime: plan.native_runtime,
            }),
        }))
    }
}

#[allow(clippy::too_many_arguments)]
fn ready_capabilities(
    inert: bool,
    inert_reason: Option<String>,
    ctx: &DriverContext,
    plan: &RmuxSpawnPlan,
    probe: &RmuxBinaryProbe,
    session: Option<String>,
    web_share: &RmuxWebShareProof,
) -> Value {
    json!({
        "inert": inert,
        "inert_reason": inert_reason,
        "kind": ctx.run_kind,
        "session": session,
        "command": plan.command,
        "args": plan.args,
        "rmux_binary": {
            "found": probe.found,
            "path": probe.path,
            "source": probe.source,
        },
        "web_share": web_share.to_capabilities(),
        "smoke": true,
    })
}

/// State for a live (non-inert) rmux session.
struct LiveSession {
    capture_task: JoinHandle<()>,
    web_share: RmuxWebShareProof,
    /// Typed session handle, retained so `release`/`Drop` can tear the detached
    /// session down through `Session::kill` (no `kill-session` shell-out).
    session: rmux_sdk::Session,
}

/// Drive a real detached session via the rmux SDK, stream rendered output over
/// `Pane::line_stream`, signal completion when that stream ends (the pane's
/// process exited), and optionally mint Web Share URLs. Returns an error
/// (caller degrades to inert + records it) when the SDK/daemon is unreachable.
async fn run_live_session(
    session_name: &str,
    rmux_bin: &str,
    plan: &RmuxSpawnPlan,
    cfg: &RmuxConfig,
    events: mpsc::Sender<DriverEvent>,
    terminal_emitted: Arc<AtomicBool>,
) -> Result<LiveSession, DriverError> {
    use rmux_sdk::{EnsureSession, EnsureSessionPolicy, ProcessSpec, Rmux, TerminalSizeSpec};

    let session_target = session_name.to_string();
    let rmux = Rmux::builder()
        .default_timeout(Duration::from_secs(5))
        .connect_or_start()
        .await
        .map_err(|e| DriverError::Transport(format!("rmux connect_or_start: {e}")))?;

    let session_name = rmux_sdk::SessionName::new(session_name.to_string())
        .map_err(|e| DriverError::Transport(format!("rmux session name: {e}")))?;

    let process =
        ProcessSpec::argv(std::iter::once(plan.command.clone()).chain(plan.args.iter().cloned()));
    let session = rmux
        .ensure_session(
            EnsureSession::named(session_name)
                .policy(EnsureSessionPolicy::CreateOrReuse)
                .detached(true)
                .working_directory(plan.cwd.to_string_lossy().into_owned())
                .size(TerminalSizeSpec::new(200, 50))
                .process(process),
        )
        .await
        .map_err(|e| DriverError::Transport(format!("rmux ensure_session: {e}")))?;

    let web_share = if cfg.web_share {
        mint_web_share(&session).await
    } else {
        RmuxWebShareProof::default()
    };

    // Pick the output path: rendered screens for full-screen TUIs, line stream
    // for plain commands. `force_render` overrides the per-command heuristic.
    let use_render = cfg.force_render.unwrap_or(plan.use_render_default);

    // The EOT marker only completes non-persistent runs that were given a dispatch
    // prompt (which carries the marker instruction). Persistent hot sessions still
    // append the marker to prompts for the stall babysitter's per-round pane scan
    // (a separate run — unaffected by this completion wiring) but complete on
    // pane/process exit only. Plain sessions without a dispatch prompt also
    // complete on process exit alone.
    let eot_marker = if plan.persistent {
        None
    } else {
        plan.initial_prompt
            .is_some()
            .then(|| plan.eot_marker.clone())
    };

    let pane = session.pane(0, 0);
    let capture_task = spawn_pane_capture(
        &pane,
        use_render,
        events.clone(),
        terminal_emitted.clone(),
        eot_marker,
    )
    .await?;

    // Deliver the dispatch prompt into the spawned TUI (worker dispatches) in
    // the *background* so `acquire` returns as soon as the session is spawned
    // instead of blocking the HTTP handler on the input-ready wait (up to 10s)
    // + paste. Blocking here is what raced the CLI's 10s timeout and left a
    // zombie lease. Mirrors the tmux driver: wait for the input prompt
    // (accepting any folder-trust dialog), then paste + Enter via the rmux CLI.
    // A delivery failure surfaces as a fatal DriverError on the stream — the
    // run then fails and tears the session down via the normal release path,
    // rather than a late `acquire()` error.
    if let Some(prompt) = plan.initial_prompt.clone() {
        let bin = rmux_bin.to_string();
        let session = session_target.clone();
        let command = plan.command.clone();
        let timeout = cfg.input_ready_timeout;
        let deliver_tx = events.clone();
        let deliver_terminal = terminal_emitted.clone();
        tokio::spawn(async move {
            if command == "claude" {
                if let Err(e) = wait_for_input_ready(&bin, &session, timeout).await {
                    tracing::warn!(
                        ?e,
                        "rmux TUI input field not detected within timeout; pasting anyway"
                    );
                }
            } else if let Err(e) = wait_for_pane_stable(&bin, &session, timeout).await {
                tracing::warn!(
                    ?e,
                    "rmux pane did not settle within timeout; pasting anyway"
                );
            }
            if let Err(e) = paste_text_and_submit(&bin, &session, &prompt).await {
                emit_fatal_driver_error_once(
                    &deliver_tx,
                    &deliver_terminal,
                    format!("dispatch prompt delivery failed: {e}"),
                )
                .await;
            }
        });
    }

    Ok(LiveSession {
        capture_task,
        web_share,
        session,
    })
}

/// Open the appropriate pane output stream (rendered screens for full-screen
/// TUIs, line-oriented for plain commands) and spawn the capture task that
/// forwards it onto the driver event channel. Shared by `acquire`'s live path
/// and `attach`'s reattach path.
async fn spawn_pane_capture(
    pane: &rmux_sdk::Pane,
    use_render: bool,
    events: mpsc::Sender<DriverEvent>,
    terminal_emitted: Arc<AtomicBool>,
    eot_marker: Option<String>,
) -> Result<JoinHandle<()>, DriverError> {
    if use_render {
        // The daemon emulates the terminal and hands us debounced, revision
        // -filtered screen snapshots — no ANSI noise, no fixed-rate polling.
        let render_stream = pane
            .render_stream()
            .await
            .map_err(|e| DriverError::Transport(format!("rmux render_stream: {e}")))?;
        Ok(tokio::spawn(capture_render_stream(
            render_stream,
            events,
            terminal_emitted,
            eot_marker,
        )))
    } else {
        // Anchor at `Oldest` so the daemon replays its retained backlog from
        // session start — closing the race between `ensure_session` (which
        // starts the process) and the subscription opening, unlike `Now`.
        let line_stream = pane
            .line_stream_starting_at(rmux_sdk::PaneOutputStart::Oldest)
            .await
            .map_err(|e| DriverError::Transport(format!("rmux line_stream: {e}")))?;
        Ok(tokio::spawn(capture_line_stream(
            line_stream,
            events,
            terminal_emitted,
            eot_marker,
        )))
    }
}

/// Run an rmux CLI verb against the daemon. The rmux CLI is tmux-compatible
/// for the buffer/send-keys verb set (the daemon's ws bridge relies on the
/// same contract).
async fn run_rmux_cli(bin: &str, args: &[&str]) -> Result<(), DriverError> {
    let output = tokio::process::Command::new(bin)
        .args(args)
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .output()
        .await
        .map_err(|e| {
            DriverError::Transport(format!("rmux {}: {e}", args.first().unwrap_or(&"")))
        })?;
    if output.status.success() {
        Ok(())
    } else {
        Err(DriverError::Transport(format!(
            "rmux {} failed: {}",
            args.first().unwrap_or(&""),
            String::from_utf8_lossy(&output.stderr).trim()
        )))
    }
}

async fn rmux_capture_pane(bin: &str, session: &str) -> Result<String, DriverError> {
    let output = tokio::process::Command::new(bin)
        .args(["capture-pane", "-p", "-t", session])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .await
        .map_err(|e| DriverError::Transport(format!("rmux capture-pane: {e}")))?;
    if !output.status.success() {
        return Err(DriverError::Transport(format!(
            "rmux capture-pane failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        )));
    }
    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
}

async fn rmux_capture_pane_bounded(
    bin: &str,
    session: &str,
    timeout: Duration,
) -> Result<String, DriverError> {
    tokio::time::timeout(timeout, rmux_capture_pane(bin, session))
        .await
        .map_err(|_| DriverError::Transport("rmux capture-pane timed out".into()))?
}

/// Paste `text` into the session's pane and press Enter, via the rmux CLI's
/// tmux-compatible buffer verbs (the same path the daemon's composer uses).
async fn paste_text_and_submit(bin: &str, session: &str, text: &str) -> Result<(), DriverError> {
    if text.is_empty() {
        return Ok(());
    }
    let buffer = format!("orgasmic-dispatch-{session}");
    run_rmux_cli(bin, &["set-buffer", "-b", &buffer, "--", text]).await?;
    let paste = run_rmux_cli(bin, &["paste-buffer", "-p", "-b", &buffer, "-t", session]).await;
    let _ = run_rmux_cli(bin, &["delete-buffer", "-b", &buffer]).await;
    paste?;
    run_rmux_cli(bin, &["send-keys", "-t", session, "Enter"]).await
}

/// Poll the rendered pane until the wrapped TUI shows its input prompt.
async fn wait_for_input_ready(
    bin: &str,
    session: &str,
    timeout: Duration,
) -> Result<(), DriverError> {
    let deadline = tokio::time::Instant::now() + timeout;
    let mut poll = tokio::time::interval(Duration::from_millis(250));
    poll.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    poll.tick().await; // first tick is immediate; skip it
    loop {
        if tokio::time::Instant::now() >= deadline {
            return Err(DriverError::InputNotReady(timeout));
        }
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        let capture_timeout = remaining.min(Duration::from_secs(2));
        if let Ok(pane) = rmux_capture_pane_bounded(bin, session, capture_timeout).await {
            // Accept Claude's folder-trust dialog (default "Yes,
            // proceed") so a fresh worktree reaches its composer.
            if pane_requests_folder_trust(&pane) {
                let _ = run_rmux_cli(bin, &["send-keys", "-t", session, "Enter"]).await;
            } else if pane_has_input_prompt(&pane) {
                return Ok(());
            }
        }
        if tokio::time::Instant::now() >= deadline {
            return Err(DriverError::InputNotReady(timeout));
        }
        poll.tick().await;
    }
}

/// Poll the rendered pane until it is non-blank and identical across two
/// consecutive captures — a harness-agnostic "the wrapped TUI finished
/// booting" signal for CLIs we have no composer heuristic for (the `custom`
/// harness, e.g. opencode). The caller pastes anyway on timeout, mirroring
/// the claude input-ready fallback.
async fn wait_for_pane_stable(
    bin: &str,
    session: &str,
    timeout: Duration,
) -> Result<(), DriverError> {
    let deadline = tokio::time::Instant::now() + timeout;
    let mut poll = tokio::time::interval(Duration::from_millis(400));
    poll.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    poll.tick().await; // first tick is immediate; skip it
    let mut previous: Option<String> = None;
    loop {
        tokio::select! {
            _ = tokio::time::sleep_until(deadline) => {
                return Err(DriverError::InputNotReady(timeout));
            }
            _ = poll.tick() => {
                if let Ok(pane) = rmux_capture_pane_bounded(
                    bin,
                    session,
                    Duration::from_secs(2),
                )
                .await
                {
                    if !pane.trim().is_empty() && previous.as_deref() == Some(pane.as_str()) {
                        return Ok(());
                    }
                    previous = Some(pane);
                }
            }
        }
    }
}

/// Drive `Pane::line_stream` to completion: forward each rendered line as an
/// assistant `TextChunk` and emit the terminal event when the stream ends.
///
/// The stream returning `Ok(None)` is the **lifecycle signal** — the daemon
/// reports the pane's process exited, so the run is complete. This is the
/// replacement for tmux's poll-for-session + scrape-for-EOT-marker design:
/// there is no marker to inject and no pane to diff.
async fn capture_line_stream(
    mut lines: rmux_sdk::PaneLineStream,
    events: mpsc::Sender<DriverEvent>,
    terminal_emitted: Arc<AtomicBool>,
    eot_marker: Option<String>,
) {
    use rmux_sdk::PaneLineItem;

    let mut seq = 0_u64;
    let mut summary_tail = WorkerSummaryTail::default();
    loop {
        match lines.next().await {
            Ok(Some(PaneLineItem::Line { text })) => {
                let chunk = strip_ansi_codes(&text);
                if chunk.trim().is_empty() {
                    continue;
                }
                if let Some(marker) = eot_marker.as_deref() {
                    let marker_seen = pane_contains_eot_marker(&chunk, marker);
                    let chunk = strip_eot_marker_lines(&chunk, marker);
                    if !chunk.trim().is_empty() {
                        summary_tail.push(&chunk);
                        seq += 1;
                        if events
                            .send(DriverEvent::TextChunk {
                                stream: TextStream::Assistant,
                                chunk,
                                seq,
                            })
                            .await
                            .is_err()
                        {
                            // Receiver gone; nothing left to stream to.
                            break;
                        }
                    }
                    if marker_seen {
                        emit_run_complete_once(&events, &terminal_emitted, summary_tail.summary())
                            .await;
                        break;
                    }
                } else {
                    summary_tail.push(&chunk);
                    seq += 1;
                    if events
                        .send(DriverEvent::TextChunk {
                            stream: TextStream::Assistant,
                            chunk,
                            seq,
                        })
                        .await
                        .is_err()
                    {
                        // Receiver gone; nothing left to stream to.
                        break;
                    }
                }
            }
            // A daemon-side gap (`Lag`) — the stream already dropped its
            // partial-line buffer, so resume cleanly without fabricating a line.
            // The enum is `#[non_exhaustive]`; any future non-line item is
            // likewise skipped rather than mis-rendered as output.
            Ok(Some(_)) => continue,
            // Stream ended: the pane's process exited → run is complete.
            Ok(None) => {
                emit_run_complete_once(
                    &events,
                    &terminal_emitted,
                    Some("rmux pane output stream ended (process exited)".to_string()),
                )
                .await;
                break;
            }
            // Transport/stream failure: surface it as a fatal terminal so the
            // consumer is never left waiting on a dead stream.
            Err(err) => {
                emit_fatal_driver_error_once(
                    &events,
                    &terminal_emitted,
                    format!("rmux line stream error: {err}"),
                )
                .await;
                break;
            }
        }
    }
}

/// Drive `Pane::render_stream` to completion for full-screen TUIs: the daemon
/// renders the pane into plain-text screen snapshots (debounced and
/// revision-filtered), and we forward the *delta* between successive screens as
/// assistant `TextChunk`s.
///
/// Lifecycle matches [`capture_line_stream`]: `Ok(None)` (the underlying output
/// subscription closing on process exit) drives `RunComplete`; a stream error
/// is a fatal terminal. Screens are already plain text — no ANSI stripping —
/// and [`pane_delta`] keeps repaints from re-emitting the whole screen each
/// revision.
async fn capture_render_stream(
    mut renders: rmux_sdk::PaneRenderStream,
    events: mpsc::Sender<DriverEvent>,
    terminal_emitted: Arc<AtomicBool>,
    eot_marker: Option<String>,
) {
    let mut previous = String::new();
    let mut seq = 0_u64;
    let mut summary_tail = WorkerSummaryTail::default();
    loop {
        match renders.next().await {
            Ok(Some(update)) => {
                let screen = update.snapshot().visible_lines().join("\n");
                let marker_seen = eot_marker
                    .as_deref()
                    .map(|marker| pane_contains_eot_marker(&screen, marker))
                    .unwrap_or(false);
                let mut chunk = pane_delta(&previous, &screen);
                if let Some(marker) = eot_marker.as_deref() {
                    chunk = strip_eot_marker_lines(&chunk, marker);
                }
                previous = screen;
                if !chunk.trim().is_empty() {
                    summary_tail.push(&chunk);
                    seq += 1;
                    if events
                        .send(DriverEvent::TextChunk {
                            stream: TextStream::Assistant,
                            chunk,
                            seq,
                        })
                        .await
                        .is_err()
                    {
                        // Receiver gone; nothing left to stream to.
                        break;
                    }
                }
                if marker_seen {
                    emit_run_complete_once(&events, &terminal_emitted, summary_tail.summary())
                        .await;
                    break;
                }
            }
            // Stream ended: the pane's process exited → run is complete.
            Ok(None) => {
                emit_run_complete_once(
                    &events,
                    &terminal_emitted,
                    Some("rmux pane render stream ended (process exited)".to_string()),
                )
                .await;
                break;
            }
            // Transport/stream failure: surface it as a fatal terminal so the
            // consumer is never left waiting on a dead stream.
            Err(err) => {
                emit_fatal_driver_error_once(
                    &events,
                    &terminal_emitted,
                    format!("rmux render stream error: {err}"),
                )
                .await;
                break;
            }
        }
    }
}

const WORKER_SUMMARY_TAIL_BYTES: usize = 8 * 1024;

#[derive(Debug, Default)]
struct WorkerSummaryTail {
    text: String,
}

impl WorkerSummaryTail {
    fn push(&mut self, chunk: &str) {
        self.text.push_str(chunk);
        trim_to_tail(&mut self.text, WORKER_SUMMARY_TAIL_BYTES);
    }

    fn summary(&self) -> Option<String> {
        let summary = self.text.trim();
        (!summary.is_empty()).then(|| summary.to_string())
    }
}

fn trim_to_tail(text: &mut String, max_bytes: usize) {
    if text.len() <= max_bytes {
        return;
    }
    let mut start = text.len() - max_bytes;
    while !text.is_char_boundary(start) {
        start += 1;
    }
    text.drain(..start);
}

fn strip_eot_marker_lines(text: &str, marker: &str) -> String {
    let core = marker
        .strip_prefix('[')
        .and_then(|m| m.strip_suffix(']'))
        .unwrap_or(marker);
    text.lines()
        .filter(|line| {
            let trimmed = line.trim();
            !(trimmed.starts_with(marker) || trimmed.starts_with(core))
        })
        .collect::<Vec<_>>()
        .join("\n")
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

/// Attempt to mint spectator + operator Web Share URLs, recording the exact
/// limitation when either cannot be produced. Operator material is redacted.
async fn mint_web_share(session: &rmux_sdk::Session) -> RmuxWebShareProof {
    let mut proof = RmuxWebShareProof {
        attempted: true,
        ..RmuxWebShareProof::default()
    };
    match session.share().await {
        Ok(handle) => {
            proof.spectator_url = handle.spectator_url().map(str::to_string);
            if let Some(operator_url) = handle.operator_url() {
                proof.operator_minted = true;
                proof.operator_url_redacted = Some(redact_operator_url(operator_url));
            }
            if proof.spectator_url.is_none() && !proof.operator_minted {
                proof.limitation =
                    Some("web-share create returned neither spectator nor operator URL".into());
            }
            // Stop the share immediately; the smoke only proves URL minting.
            let _ = handle.stop().await;
        }
        Err(err) => {
            proof.limitation = Some(format!("web-share unavailable: {err}"));
        }
    }
    proof
}

struct RmuxControl {
    events: mpsc::Sender<DriverEvent>,
    kind: RunKind,
    capture_task: Option<JoinHandle<()>>,
    terminal_emitted: Arc<AtomicBool>,
    released: bool,
    /// Typed session handle for a live run, so `release`/`Drop` can tear it
    /// down through `Session::kill`. `None` for inert runs, which own no rmux
    /// session.
    session: Option<rmux_sdk::Session>,
    /// Whether an implicit `Drop` (e.g. daemon shutdown) should reap the rmux
    /// session. `false` for system-wide and reattached runs, whose sessions are
    /// meant to outlive the daemon. Explicit `release` always reaps regardless.
    kill_on_drop: bool,
    /// rmux CLI binary for paste-buffer/send-keys followup delivery. `None` on
    /// inert runs (no live session to paste into).
    rmux_bin: Option<String>,
    /// Detached session target name for CLI verbs. `None` on inert runs.
    session_target: Option<String>,
    /// Run id for re-appending the end-of-turn-marker instruction on followups.
    run_id: Option<String>,
    /// Wrapped harness command (`claude`, `codex`, …) — recorded for diagnostics
    /// and future harness-specific followup heuristics.
    #[allow(dead_code)]
    harness_command: Option<String>,
    /// How long to wait for the harness composer before rejecting a followup as
    /// busy. Mirrors the dispatch-paste knob.
    input_ready_timeout: Duration,
}

impl RmuxControl {
    fn inert(events: mpsc::Sender<DriverEvent>, kind: RunKind) -> Self {
        Self {
            events,
            kind,
            capture_task: None,
            terminal_emitted: Arc::new(AtomicBool::new(false)),
            released: false,
            session: None,
            kill_on_drop: true,
            rmux_bin: None,
            session_target: None,
            run_id: None,
            harness_command: None,
            input_ready_timeout: default_input_ready_timeout(),
        }
    }
}

/// Re-append the end-of-turn-marker instruction to a followup payload (same
/// contract as the initial dispatch paste).
fn followup_prompt_with_eot(input: &str, run_id: &str) -> String {
    prompt_with_eot_instruction(input, run_id)
}

/// Poll until the harness shows a composer input prompt. Followup delivery
/// gates on this (not pane stability) so mid-stream paste cannot corrupt an
/// in-flight turn — streaming output without a prompt is rejected.
async fn wait_for_followup_ready(
    bin: &str,
    session: &str,
    timeout: Duration,
) -> Result<(), DriverError> {
    wait_for_input_ready(bin, session, timeout).await
}

#[async_trait]
impl DriverControl for RmuxControl {
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

    async fn send_input(&mut self, req: UserInputRequest) -> Result<UserInputAck, DriverError> {
        let (bin, session, run_id) = match (
            self.rmux_bin.as_deref(),
            self.session_target.as_deref(),
            self.run_id.as_deref(),
        ) {
            (Some(bin), Some(session), Some(run_id)) => (bin, session, run_id),
            _ => return Err(DriverError::Unsupported("send_input")),
        };

        let text = followup_prompt_with_eot(&req.input, run_id);

        // Mid-turn policy: reject rather than queue. Pasting while the harness is
        // still streaming its previous turn corrupts the in-flight turn. Gate on
        // composer input-readiness and return a clear ack when the prompt is not
        // visible yet — never paste blindly mid-stream.
        if wait_for_followup_ready(bin, session, self.input_ready_timeout)
            .await
            .is_err()
        {
            return Ok(UserInputAck {
                accepted: false,
                message: Some("harness busy".into()),
            });
        }

        paste_text_and_submit(bin, session, &text).await?;
        Ok(UserInputAck {
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
        // Reap the detached rmux session through the typed SDK (inert runs own
        // no session). Awaited so the session is gone when `release` returns;
        // teardown failures are non-fatal and only logged.
        if let Some(session) = self.session.take() {
            if let Err(err) = session.kill().await {
                tracing::warn!(?err, "rmux Session::kill failed during release");
            }
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

impl Drop for RmuxControl {
    fn drop(&mut self) {
        if let Some(task) = self.capture_task.take() {
            task.abort();
        }
        // System-wide / reattached runs intentionally outlive the daemon: never
        // reap their session on an implicit Drop (only explicit `release`
        // does). Dropping the `Session` handle does not reap the session — only
        // an explicit `Session::kill` does — so simply let the field drop.
        if !self.kill_on_drop {
            return;
        }
        // Backstop when release() never ran (panic / early drop): fire a
        // detached `Session::kill` on the current runtime so the rmux session is
        // reaped without blocking Drop. `take()` means a prior release() already
        // cleared this. Best-effort: if there is no runtime handle (drop off the
        // async runtime), the detached session is left for the daemon to reap.
        if let Some(session) = self.session.take() {
            match tokio::runtime::Handle::try_current() {
                Ok(handle) => {
                    handle.spawn(async move {
                        if let Err(err) = session.kill().await {
                            tracing::warn!(?err, "rmux Session::kill failed during drop backstop");
                        }
                    });
                }
                Err(_) => {
                    tracing::warn!(
                        "rmux control dropped without release and no runtime handle; \
                         detached session left for daemon reaping"
                    );
                }
            }
        }
    }
}

/// Find a live rmux session whose name starts with `prefix`, connecting to an
/// already-running rmux daemon (never starting one). Used by the daemon's WS
/// bridge as a fallback when the supervisor holds no run record but a
/// system-wide session may have survived a daemon restart.
pub async fn find_live_session_with_prefix(prefix: &str) -> Option<String> {
    let rmux = rmux_sdk::Rmux::builder()
        .default_timeout(Duration::from_secs(5))
        .connect()
        .await
        .ok()?;
    let sessions = rmux.list_sessions().await.ok()?;
    sessions
        .into_iter()
        .map(|s| s.to_string())
        .find(|name| name.starts_with(prefix))
}

/// Convenience constructor for tests + supervisor smoke runs.
pub fn driver() -> RmuxDriver {
    RmuxDriver::new(Box::new(crate::adapters::CodexAdapter::new()))
}

/// Inert-mode config (no real rmux interaction) for smoke tests / missing rmux.
pub fn inert_config() -> DriverConfig {
    DriverConfig::from_value(json!({"force_inert": true}))
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
    poll.tick().await;
    loop {
        tokio::select! {
            _ = tokio::time::sleep_until(deadline) => {
                return Err(DriverError::InputNotReady(timeout));
            }
            _ = poll.tick() => {
                if let Ok(pane) = capture().await {
                    if pane_requests_folder_trust(&pane) {
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
mod tests {
    use super::*;

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

    fn ctx(run_id: &str, kind: RunKind) -> DriverContext {
        DriverContext {
            identity: RuntimeIdentity::new(run_id, "boot-test"),
            run_kind: kind,
            task_id: "TASK-104".into(),
            worker_id: "implementer-codex-rmux".into(),
            project_id: Some("orgasmic".into()),
            worktree: None,
            babysitter_target: None,
            continuation: None,
        }
    }

    #[test]
    fn transport_name_is_stable() {
        assert_eq!(driver().transport(), "rmux");
    }

    #[test]
    fn redact_operator_url_elides_token() {
        let redacted = redact_operator_url("https://share.example/op/abc?token=SECRET");
        assert!(!redacted.contains("SECRET"));
        assert!(redacted.starts_with("https://share.example/op/abc"));
        assert!(redacted.contains("operator-token-redacted"));
        // Fragment-bearing operator URLs are also elided.
        let frag = redact_operator_url("https://share.example/op/abc#k=SECRET");
        assert!(!frag.contains("SECRET"));
    }

    #[test]
    fn probe_honors_explicit_env_override() {
        // SAFETY: single-threaded test; we restore the prior value.
        let prior = std::env::var_os(RMUX_SDK_DAEMON_BINARY_ENV);
        std::env::set_var(RMUX_SDK_DAEMON_BINARY_ENV, "/nonexistent/rmux-binary-xyz");
        let probe = probe_rmux_binary();
        assert_eq!(probe.source, Some("env"));
        assert_eq!(probe.path.as_deref(), Some("/nonexistent/rmux-binary-xyz"));
        assert!(!probe.found, "nonexistent override must not be 'found'");
        match prior {
            Some(v) => std::env::set_var(RMUX_SDK_DAEMON_BINARY_ENV, v),
            None => std::env::remove_var(RMUX_SDK_DAEMON_BINARY_ENV),
        }
    }

    #[test]
    fn inert_reason_reports_missing_rmux_binary_separately() {
        let cfg = RmuxConfig::default();
        let probe = RmuxBinaryProbe::missing();
        // rmux binary missing dominates and is reported on its own, not as a
        // harness-binary problem (acceptance criterion: separate checks).
        assert_eq!(
            inert_reason(&cfg, &probe, "codex"),
            Some("rmux_binary_missing".to_string())
        );
    }

    #[test]
    fn inert_reason_reports_missing_harness_when_rmux_present() {
        let cfg = RmuxConfig::default();
        let probe = RmuxBinaryProbe {
            found: true,
            path: Some("/usr/local/bin/rmux".into()),
            source: Some("path"),
        };
        let reason = inert_reason(&cfg, &probe, "definitely-not-a-real-binary-xyz");
        assert_eq!(
            reason.as_deref(),
            Some("harness_binary_missing:definitely-not-a-real-binary-xyz")
        );
    }

    #[test]
    fn force_inert_short_circuits_probes() {
        let cfg = RmuxConfig {
            force_inert: true,
            ..RmuxConfig::default()
        };
        let probe = RmuxBinaryProbe {
            found: true,
            path: Some("/usr/local/bin/rmux".into()),
            source: Some("path"),
        };
        assert_eq!(
            inert_reason(&cfg, &probe, "codex"),
            Some("force_inert".to_string())
        );
    }

    #[test]
    fn default_command_is_bounded_per_harness() {
        assert_eq!(default_command_for_harness("codex").0, "codex");
        assert_eq!(default_command_for_harness("claude").0, "claude");
        assert_eq!(
            default_command_for_harness("cursor-agent").0,
            "cursor-agent"
        );
        let hermes = default_command_for_harness("hermes");
        assert_eq!(hermes.0, "hermes");
        assert_eq!(hermes.1, vec!["chat".to_string(), "--tui".to_string()]);
        assert_eq!(default_command_for_harness("unknown").0, "sh");
    }

    /// Dispatch placeholder + claude harness must spawn the real claude TUI
    /// with the dispatch prompt staged for delivery — never run the
    /// placeholder verbatim (the bug that made rmux worker dispatches
    /// complete instantly with only the placeholder echo).
    #[test]
    fn dispatch_placeholder_swaps_to_real_claude_invocation() {
        let cfg: RmuxConfig = serde_json::from_value(json!({
            "command": "sh",
            "args": ["-lc", "echo orgasmic pipeline stage acquired; exec sh"],
            "harness": "claude",
            "model": "claude-sonnet-4-6",
            "prompt_bundle_text": "do the task",
        }))
        .unwrap();
        let ctx = ctx("run-dispatch", RunKind::Worker);
        let plan = build_spawn_plan(&cfg, &ctx, "claude");
        assert_eq!(plan.command, "claude");
        assert!(plan
            .args
            .iter()
            .any(|arg| arg == "--dangerously-skip-permissions"));
        assert!(plan
            .args
            .windows(2)
            .any(|pair| pair == ["--model", "claude-sonnet-4-6"]));
        assert!(plan.args.iter().any(|arg| arg == "--session-id"));
        let prompt = plan.initial_prompt.expect("dispatch prompt staged");
        assert!(prompt.contains("do the task"));
        assert!(prompt.contains("end-of-turn marker"));
        // The instruction must not contain the literal marker (the screen
        // scan would match the echoed prompt immediately).
        assert!(!prompt.contains(&plan.eot_marker));
        let native = plan.native_runtime.expect("claude native runtime");
        assert_eq!(native.provider, "claude");
        assert!(!native.resume_argv.is_empty());
    }

    #[test]
    fn dispatch_placeholder_swaps_to_real_codex_invocation() {
        // Regression: the swap gate was `claude || custom` only, so codex
        // workers executed the placeholder `sh` and the prompt was typed into
        // a bare shell. The daemon sentinel must swap to real `codex`.
        let cfg: RmuxConfig = serde_json::from_value(json!({
            "command": "sh",
            "args": ["-lc", "echo orgasmic pipeline stage acquired; exec sh"],
            "harness": "codex",
            "model": "gpt-5.5",
            "prompt_bundle_text": "do the task",
        }))
        .unwrap();
        let ctx = ctx("run-dispatch-codex", RunKind::Worker);
        let plan = build_spawn_plan(&cfg, &ctx, "codex");
        assert_eq!(plan.command, "codex");
        assert!(!is_dispatch_placeholder(
            Some(plan.command.as_str()),
            &plan.args
        ));
        let prompt = plan.initial_prompt.expect("dispatch prompt staged");
        assert!(prompt.contains("do the task"));
        let native = plan.native_runtime.expect("codex native runtime");
        assert_eq!(native.provider, "codex");
    }

    /// Worker `:HARNESS_ARGS:` ride along on the real harness argv, and a
    /// user-supplied `--model` there beats the worker default (the guarded
    /// push skips when the flag is already present). The inert dispatch
    /// placeholder never receives them.
    #[test]
    fn harness_args_extend_claude_argv_and_win_over_model() {
        let cfg: RmuxConfig = serde_json::from_value(json!({
            "command": "sh",
            "args": ["-lc", "echo orgasmic pipeline stage acquired; exec sh"],
            "harness": "claude",
            "model": "claude-sonnet-4-6",
            "harness_args": ["--model", "claude-haiku-4-5", "--betas", "context-1m"],
            "prompt_bundle_text": "do the task",
        }))
        .unwrap();
        let ctx = ctx("run-dispatch", RunKind::Worker);
        let plan = build_spawn_plan(&cfg, &ctx, "claude");
        assert_eq!(plan.command, "claude");
        assert!(plan
            .args
            .windows(2)
            .any(|pair| pair == ["--model", "claude-haiku-4-5"]));
        assert!(!plan
            .args
            .windows(2)
            .any(|pair| pair == ["--model", "claude-sonnet-4-6"]));
        assert!(plan
            .args
            .windows(2)
            .any(|pair| pair == ["--betas", "context-1m"]));
    }

    /// Custom-harness dispatch: the staged placeholder is swapped for the
    /// template's `:HARNESS_ARGS:` command line (argv[0] + args), the compiled
    /// prompt is staged for paste delivery, and the rendered-screen output
    /// path is the default (the wrapped CLI is an interactive agent TUI).
    #[test]
    fn custom_dispatch_placeholder_runs_harness_args_as_command() {
        let cfg: RmuxConfig = serde_json::from_value(json!({
            "command": "sh",
            "args": ["-lc", "echo orgasmic pipeline stage acquired; exec sh"],
            "harness": "custom",
            "harness_args": ["opencode", "--print-logs"],
            "prompt_bundle_text": "do the task",
        }))
        .unwrap();
        let ctx = ctx("run-dispatch-custom", RunKind::Worker);
        let plan = build_spawn_plan(&cfg, &ctx, "custom");
        assert_eq!(plan.command, "opencode");
        assert_eq!(plan.args, vec!["--print-logs"]);
        assert!(plan.use_render_default);
        let prompt = plan.initial_prompt.expect("dispatch prompt staged");
        assert!(prompt.contains("do the task"));
        assert!(prompt.contains("end-of-turn marker"));
        let native = plan.native_runtime.expect("native runtime meta");
        assert_eq!(native.provider, "custom");
        assert_eq!(native.launch_argv, vec!["opencode", "--print-logs"]);
    }

    /// A custom launch with no harness args stays the bare-terminal session
    /// (manager escape hatch): login shell, line-oriented output, no prompt.
    #[test]
    fn custom_without_harness_args_stays_bare_shell() {
        let cfg: RmuxConfig = serde_json::from_value(json!({
            "harness": "custom",
        }))
        .unwrap();
        let plan = build_spawn_plan(&cfg, &ctx("run-bare", RunKind::Worker), "custom");
        let shell = std::env::var("SHELL").unwrap_or_else(|_| "sh".to_string());
        assert_eq!(plan.command, shell);
        assert!(plan.args.is_empty());
        assert!(plan.initial_prompt.is_none());
        assert!(!plan.use_render_default);
    }

    /// A custom dispatch (prompt staged) without harness args must be refused:
    /// pasting the brief into the fallback shell would execute it.
    #[test]
    fn custom_dispatch_without_harness_args_is_refused() {
        let with_prompt: RmuxConfig = serde_json::from_value(json!({
            "harness": "custom",
            "prompt_bundle_text": "do the task",
        }))
        .unwrap();
        assert!(custom_dispatch_misconfig("custom", &with_prompt).is_some());

        let with_args: RmuxConfig = serde_json::from_value(json!({
            "harness": "custom",
            "harness_args": ["opencode"],
            "prompt_bundle_text": "do the task",
        }))
        .unwrap();
        assert!(custom_dispatch_misconfig("custom", &with_args).is_none());

        let no_prompt: RmuxConfig = serde_json::from_value(json!({
            "harness": "custom",
        }))
        .unwrap();
        assert!(custom_dispatch_misconfig("custom", &no_prompt).is_none());
        assert!(custom_dispatch_misconfig("claude", &with_prompt).is_none());
    }

    /// An explicit non-placeholder command is honored verbatim.
    #[test]
    fn explicit_command_is_not_swapped() {
        let cfg: RmuxConfig = serde_json::from_value(json!({
            "command": "sleep",
            "args": ["5"],
            "harness": "claude",
        }))
        .unwrap();
        let plan = build_spawn_plan(&cfg, &ctx("run-explicit", RunKind::Worker), "claude");
        assert_eq!(plan.command, "sleep");
        assert!(plan.initial_prompt.is_none());
    }

    #[test]
    fn command_is_tui_matches_agent_binaries_only() {
        for tui in ["claude", "codex", "cursor-agent", "hermes"] {
            assert!(command_is_tui(tui), "{tui} should be a TUI");
        }
        // Resolved absolute paths are matched on basename.
        assert!(command_is_tui("/usr/local/bin/claude"));
        // Plain / custom commands are line-oriented.
        assert!(!command_is_tui("sh"));
        assert!(!command_is_tui("bash"));
        assert!(!command_is_tui("/bin/sleep"));
    }

    #[test]
    fn session_name_is_run_scoped() {
        let id = RuntimeIdentity::new("run-x", "boot-1");
        let name = rmux_session_name(&id);
        assert!(name.starts_with("orgasmic-rmux-run-x-"));
    }

    #[test]
    fn marker_lines_are_removed_from_worker_summary_tail() {
        let marker = tmux_eot_marker("run-rmux-summary");
        let chunk = format!("final worker answer\n{marker}\n");
        let stripped = strip_eot_marker_lines(&chunk, &marker);

        assert_eq!(stripped, "final worker answer");
        let mut tail = WorkerSummaryTail::default();
        tail.push(&stripped);
        assert_eq!(tail.summary().as_deref(), Some("final worker answer"));
    }

    #[test]
    fn worker_summary_tail_keeps_tail_without_splitting_utf8() {
        let mut tail = WorkerSummaryTail::default();
        tail.push(&"ж".repeat(WORKER_SUMMARY_TAIL_BYTES));

        let summary = tail.summary().expect("summary");
        assert!(summary.len() <= WORKER_SUMMARY_TAIL_BYTES);
        assert!(summary.is_char_boundary(0));
    }

    #[tokio::test]
    async fn inert_acquire_emits_ready_and_completes() {
        let d = driver();
        let mut s = d
            .acquire(ctx("run-inert", RunKind::Worker), inert_config())
            .await
            .unwrap();
        let ev = s.events.recv().await.unwrap();
        let DriverEvent::Ready { capabilities, .. } = ev else {
            panic!("expected Ready, got {ev:?}");
        };
        assert_eq!(capabilities["inert"], true);
        assert_eq!(capabilities["inert_reason"], "force_inert");
        assert_eq!(capabilities["smoke"], true);
        // rmux binary is reported as a separate, distinct field.
        assert!(capabilities["rmux_binary"].is_object());
        assert_eq!(capabilities["rmux_binary"]["found"], false);
        s.control.release("done").await.unwrap();
        let ev2 = s.events.recv().await.unwrap();
        assert!(matches!(ev2, DriverEvent::RunComplete { .. }));
    }

    #[tokio::test]
    async fn attach_force_inert_is_not_reattachable() {
        let d = driver();
        let out = d
            .attach(ctx("run-no-attach", RunKind::Worker), inert_config())
            .await
            .unwrap();
        assert!(matches!(out, AttachOutcome::NotReattachable));
    }

    /// Live reattach smoke (boot auto-reattach path). Acquire a real session,
    /// then `attach` with the same identity must return a second live handle
    /// that streams from the same rmux session. Skipped without an rmux binary.
    #[tokio::test]
    async fn live_rmux_attach_reattaches_when_available() {
        let _live_guard = live_session_guard();
        let probe = probe_rmux_binary();
        if !probe.found {
            eprintln!("skipping live_rmux_attach_reattaches_when_available: rmux binary not found");
            return;
        }
        let d = driver();
        let context = ctx("run-attach", RunKind::Worker);
        let cfg = DriverConfig::from_value(json!({
            "command": "sleep",
            "args": ["30"],
        }));
        let mut s = d.acquire(context.clone(), cfg.clone()).await.unwrap();
        let ev = s.events.recv().await.unwrap();
        let DriverEvent::Ready { capabilities, .. } = ev else {
            panic!("expected Ready, got {ev:?}");
        };
        if capabilities["inert"] == true {
            eprintln!("skipping live_rmux_attach_reattaches_when_available: SDK degraded to inert");
            return;
        }

        let out = d.attach(context.clone(), cfg).await.unwrap();
        let AttachOutcome::Attached(attached) = out else {
            panic!("expected Attached for a live session");
        };
        let mut s2 = *attached.session;
        let ev2 = s2.events.recv().await.unwrap();
        let DriverEvent::Ready { capabilities, .. } = ev2 else {
            panic!("expected Ready from attach, got {ev2:?}");
        };
        assert_eq!(capabilities["reattached"], true);
        assert_eq!(
            capabilities["session"],
            json!(rmux_session_name(&context.identity))
        );

        // Tear down through the original handle; the attached handle must not
        // reap the session on drop (kill_on_drop=false), only stop streaming.
        drop(s2);
        s.control.release("test done").await.unwrap();

        // The session is gone after an explicit release.
        let out = d
            .attach(
                context,
                DriverConfig::from_value(json!({"command": "sleep", "args": ["30"]})),
            )
            .await
            .unwrap();
        assert!(matches!(out, AttachOutcome::NotReattachable));
    }

    #[tokio::test]
    async fn inert_release_is_idempotent() {
        let d = driver();
        let mut s = d
            .acquire(ctx("run-idem", RunKind::Worker), inert_config())
            .await
            .unwrap();
        let _ = s.events.recv().await;
        s.control.release("a").await.unwrap();
        s.control.release("b").await.unwrap();
    }

    #[tokio::test]
    async fn implementer_transition_state_accepted_then_event() {
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
                reason: "starting".into(),
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

    /// Live rmux smoke. Skipped unless a real rmux binary is discoverable
    /// (RMUX_SDK_DAEMON_BINARY or PATH). On hosts without rmux this returns
    /// early so CI stays green; the honest inert path is covered above.
    #[tokio::test]
    async fn live_rmux_session_lifecycle_when_available() {
        let _live_guard = live_session_guard();
        let probe = probe_rmux_binary();
        if !probe.found {
            eprintln!("skipping live_rmux_session_lifecycle_when_available: rmux binary not found");
            return;
        }
        let d = driver();
        let cfg = DriverConfig::from_value(json!({
            "command": "sleep",
            "args": ["30"],
            "web_share": true,
        }));
        let mut s = d
            .acquire(ctx("run-live", RunKind::Worker), cfg)
            .await
            .unwrap();
        let ev = s.events.recv().await.unwrap();
        let DriverEvent::Ready { capabilities, .. } = ev else {
            panic!("expected Ready, got {ev:?}");
        };
        // Either a real session is live, or the SDK degraded honestly with a
        // recorded reason — never a silent fake.
        if capabilities["inert"] == true {
            assert!(
                capabilities["inert_reason"].is_string(),
                "inert live run must carry a reason"
            );
        } else {
            assert!(capabilities["session"].is_string());
            // Web Share proof: a spectator URL and/or operator URL, or an exact
            // recorded limitation. Never expose a raw operator token.
            let ws = &capabilities["web_share"];
            assert_eq!(ws["attempted"], true);
            let produced_url =
                ws["spectator_url"].is_string() || ws["operator_url_redacted"].is_string();
            assert!(
                produced_url || ws["limitation"].is_string(),
                "web-share must produce a URL or record a limitation: {ws}"
            );
            // Operator material is only ever surfaced redacted.
            if let Some(redacted) = ws["operator_url_redacted"].as_str() {
                assert!(
                    redacted.contains("operator-token-redacted"),
                    "operator url must be redacted: {redacted}"
                );
            }
        }
        s.control.release("cleanup").await.unwrap();
    }

    /// Live rmux output + lifecycle smoke. Drives a short command that prints a
    /// line and exits, then proves the new SDK path: the line arrives as a
    /// `TextChunk` over `Pane::line_stream`, and the stream ending (process
    /// exit) emits `RunComplete` on its own — with no EOT marker, no
    /// `capture-pane` poll, and no `kill-session` shell-out. Skipped without a
    /// real rmux binary so CI stays green.
    #[tokio::test]
    async fn live_rmux_streams_output_and_completes_on_exit() {
        let _live_guard = live_session_guard();
        let probe = probe_rmux_binary();
        if !probe.found {
            eprintln!(
                "skipping live_rmux_streams_output_and_completes_on_exit: rmux binary not found"
            );
            return;
        }
        const SENTINEL: &str = "orgasmic-rmux-line-stream-sentinel";
        let d = driver();
        let cfg = DriverConfig::from_value(json!({
            "command": "sh",
            "args": ["-c", format!("printf '{SENTINEL}\\n'; exit 0")],
        }));
        let mut s = d
            .acquire(ctx("run-stream", RunKind::Worker), cfg)
            .await
            .unwrap();

        let ev = s.events.recv().await.unwrap();
        let DriverEvent::Ready { capabilities, .. } = ev else {
            panic!("expected Ready, got {ev:?}");
        };
        // If the SDK degraded to inert (no daemon), there is nothing to stream;
        // the inert path is covered by other tests.
        if capabilities["inert"] == true {
            assert!(capabilities["inert_reason"].is_string());
            s.control.release("cleanup").await.unwrap();
            return;
        }

        let mut saw_line = false;
        let mut saw_complete = false;
        for _ in 0..40 {
            let ev = tokio::time::timeout(Duration::from_secs(5), s.events.recv())
                .await
                .expect("timed out waiting for rmux stream event")
                .expect("event stream closed");
            match ev {
                DriverEvent::TextChunk { chunk, .. } => {
                    saw_line |= chunk.contains(SENTINEL);
                }
                DriverEvent::RunComplete { summary } => {
                    saw_complete = true;
                    assert_eq!(
                        summary.as_deref(),
                        Some("rmux pane output stream ended (process exited)"),
                        "process exit should drive completion, not a marker"
                    );
                    break;
                }
                DriverEvent::DriverError { fatal, message } => {
                    panic!("unexpected driver error (fatal={fatal}): {message}");
                }
                other => panic!("unexpected event before completion: {other:?}"),
            }
        }
        assert!(saw_line, "expected the printed line over Pane::line_stream");
        assert!(
            saw_complete,
            "expected RunComplete when the pane process exited"
        );
        // release() after natural completion is idempotent (terminal already
        // emitted) and tears the session down via the typed Session::kill.
        s.control.release("cleanup").await.unwrap();
    }

    /// Live rmux render-path smoke. Forces `render_stream` (the TUI path) on a
    /// command that paints a line, holds it briefly, then exits — proving the
    /// daemon-rendered screen reaches us as a `TextChunk` and that process exit
    /// closes the render stream into `RunComplete`. The rendered screen is plain
    /// text (no ANSI), so the sentinel appears verbatim. Skipped without a real
    /// rmux binary.
    #[tokio::test]
    async fn live_rmux_render_path_streams_screen_and_completes() {
        let _live_guard = live_session_guard();
        let probe = probe_rmux_binary();
        if !probe.found {
            eprintln!(
                "skipping live_rmux_render_path_streams_screen_and_completes: rmux binary not found"
            );
            return;
        }
        const SENTINEL: &str = "orgasmic-rmux-render-sentinel";
        let d = driver();
        // `force_render` exercises the render path without needing a real agent
        // TUI. Hold the screen ~1s so a snapshot is captured before exit.
        let cfg = DriverConfig::from_value(json!({
            "command": "sh",
            "args": ["-c", format!("printf '{SENTINEL}\\n'; sleep 1; exit 0")],
            "force_render": true,
        }));
        let mut s = d
            .acquire(ctx("run-render", RunKind::Worker), cfg)
            .await
            .unwrap();

        let ev = s.events.recv().await.unwrap();
        let DriverEvent::Ready { capabilities, .. } = ev else {
            panic!("expected Ready, got {ev:?}");
        };
        if capabilities["inert"] == true {
            assert!(capabilities["inert_reason"].is_string());
            s.control.release("cleanup").await.unwrap();
            return;
        }

        let mut saw_screen = false;
        let mut saw_complete = false;
        for _ in 0..40 {
            let ev = tokio::time::timeout(Duration::from_secs(5), s.events.recv())
                .await
                .expect("timed out waiting for rmux render event")
                .expect("event stream closed");
            match ev {
                DriverEvent::TextChunk { chunk, .. } => {
                    saw_screen |= chunk.contains(SENTINEL);
                }
                DriverEvent::RunComplete { summary } => {
                    saw_complete = true;
                    assert_eq!(
                        summary.as_deref(),
                        Some("rmux pane render stream ended (process exited)"),
                        "render path should report its own completion summary"
                    );
                    break;
                }
                DriverEvent::DriverError { fatal, message } => {
                    panic!("unexpected driver error (fatal={fatal}): {message}");
                }
                other => panic!("unexpected event before completion: {other:?}"),
            }
        }
        assert!(
            saw_screen,
            "expected the rendered screen to carry the sentinel over render_stream"
        );
        assert!(
            saw_complete,
            "expected RunComplete when the pane process exited"
        );
        s.control.release("cleanup").await.unwrap();
    }

    /// Persistent hot sessions must not self-complete when the EOT marker
    /// appears — only when the pane process exits.
    #[tokio::test]
    async fn live_rmux_persistent_run_does_not_complete_on_eot_marker() {
        let _live_guard = live_session_guard();
        let probe = probe_rmux_binary();
        if !probe.found {
            eprintln!(
                "skipping live_rmux_persistent_run_does_not_complete_on_eot_marker: rmux binary not found"
            );
            return;
        }
        let run_id = "run-persistent-marker";
        let marker = tmux_eot_marker(run_id);
        let d = driver();
        let cfg = DriverConfig::from_value(json!({
            "command": "sh",
            "args": ["-c", format!("printf '{marker}\\n'; sleep 30")],
            "prompt_bundle_text": "do the task",
            "persistent": true,
            "force_render": true,
        }));
        let mut s = d.acquire(ctx(run_id, RunKind::Worker), cfg).await.unwrap();
        let ev = s.events.recv().await.unwrap();
        let DriverEvent::Ready { capabilities, .. } = ev else {
            panic!("expected Ready, got {ev:?}");
        };
        if capabilities["inert"] == true {
            eprintln!(
                "skipping live_rmux_persistent_run_does_not_complete_on_eot_marker: SDK degraded to inert"
            );
            s.control.release("cleanup").await.unwrap();
            return;
        }

        let mut saw_marker = false;
        let mut saw_complete = false;
        for _ in 0..40 {
            let ev = tokio::time::timeout(Duration::from_secs(5), s.events.recv())
                .await
                .expect("timed out waiting for rmux render event")
                .expect("event stream closed");
            match ev {
                DriverEvent::TextChunk { chunk, .. } => {
                    saw_marker |= chunk.contains(&marker);
                }
                DriverEvent::RunComplete { .. } => {
                    saw_complete = true;
                    break;
                }
                DriverEvent::DriverError { fatal, message } => {
                    panic!("unexpected driver error (fatal={fatal}): {message}");
                }
                other => panic!("unexpected event before completion: {other:?}"),
            }
            if saw_marker {
                break;
            }
        }
        assert!(
            saw_marker,
            "expected the rendered screen to carry the EOT marker"
        );
        assert!(
            !saw_complete,
            "persistent run must not emit RunComplete when the marker appears"
        );
        s.control.release("cleanup").await.unwrap();
    }

    /// Persistent hot sessions complete when the pane process exits.
    #[tokio::test]
    async fn live_rmux_persistent_run_completes_on_process_exit() {
        let _live_guard = live_session_guard();
        let probe = probe_rmux_binary();
        if !probe.found {
            eprintln!(
                "skipping live_rmux_persistent_run_completes_on_process_exit: rmux binary not found"
            );
            return;
        }
        let run_id = "run-persistent-exit";
        let marker = tmux_eot_marker(run_id);
        let d = driver();
        let cfg = DriverConfig::from_value(json!({
            "command": "sh",
            "args": ["-c", format!("printf '{marker}\\n'; exit 0")],
            "prompt_bundle_text": "do the task",
            "persistent": true,
            "force_render": true,
        }));
        let mut s = d.acquire(ctx(run_id, RunKind::Worker), cfg).await.unwrap();
        let ev = s.events.recv().await.unwrap();
        let DriverEvent::Ready { capabilities, .. } = ev else {
            panic!("expected Ready, got {ev:?}");
        };
        if capabilities["inert"] == true {
            eprintln!(
                "skipping live_rmux_persistent_run_completes_on_process_exit: SDK degraded to inert"
            );
            s.control.release("cleanup").await.unwrap();
            return;
        }

        let mut saw_complete = false;
        for _ in 0..40 {
            let ev = tokio::time::timeout(Duration::from_secs(5), s.events.recv())
                .await
                .expect("timed out waiting for rmux render event")
                .expect("event stream closed");
            match ev {
                DriverEvent::RunComplete { summary } => {
                    saw_complete = true;
                    assert_eq!(
                        summary.as_deref(),
                        Some("rmux pane render stream ended (process exited)"),
                        "persistent run should complete on process exit"
                    );
                    break;
                }
                DriverEvent::DriverError { fatal, message } => {
                    panic!("unexpected driver error (fatal={fatal}): {message}");
                }
                _ => {}
            }
        }
        assert!(
            saw_complete,
            "expected RunComplete when the persistent run's process exited"
        );
        s.control.release("cleanup").await.unwrap();
    }

    /// Non-persistent dispatch runs still complete when the EOT marker appears.
    #[tokio::test]
    async fn live_rmux_non_persistent_dispatch_completes_on_eot_marker() {
        let _live_guard = live_session_guard();
        let probe = probe_rmux_binary();
        if !probe.found {
            eprintln!(
                "skipping live_rmux_non_persistent_dispatch_completes_on_eot_marker: rmux binary not found"
            );
            return;
        }
        let run_id = "run-non-persistent-marker";
        let marker = tmux_eot_marker(run_id);
        let d = driver();
        let cfg = DriverConfig::from_value(json!({
            "command": "sh",
            "args": ["-c", format!("printf '{marker}\\n'; sleep 30")],
            "prompt_bundle_text": "do the task",
            "persistent": false,
            "force_render": true,
        }));
        let mut s = d.acquire(ctx(run_id, RunKind::Worker), cfg).await.unwrap();
        let ev = s.events.recv().await.unwrap();
        let DriverEvent::Ready { capabilities, .. } = ev else {
            panic!("expected Ready, got {ev:?}");
        };
        if capabilities["inert"] == true {
            eprintln!(
                "skipping live_rmux_non_persistent_dispatch_completes_on_eot_marker: SDK degraded to inert"
            );
            s.control.release("cleanup").await.unwrap();
            return;
        }

        let mut saw_complete = false;
        for _ in 0..40 {
            let ev = tokio::time::timeout(Duration::from_secs(5), s.events.recv())
                .await
                .expect("timed out waiting for rmux render event")
                .expect("event stream closed");
            match ev {
                DriverEvent::RunComplete { .. } => {
                    saw_complete = true;
                    break;
                }
                DriverEvent::DriverError { fatal, message } => {
                    panic!("unexpected driver error (fatal={fatal}): {message}");
                }
                _ => {}
            }
        }
        assert!(
            saw_complete,
            "non-persistent dispatch run must complete when the EOT marker appears"
        );
        s.control.release("cleanup").await.unwrap();
    }

    #[test]
    fn followup_prompt_reappends_eot_instruction() {
        let run_id = "run-followup-eot";
        let prompt = followup_prompt_with_eot("grill me harder", run_id);
        assert!(prompt.contains("grill me harder"));
        assert!(prompt.contains("end-of-turn marker"));
        assert!(prompt.contains(run_id));
        assert!(!prompt.contains(&tmux_eot_marker(run_id)));
    }

    #[tokio::test]
    async fn inert_send_input_returns_unsupported() {
        let d = driver();
        let mut s = d
            .acquire(ctx("run-inert-input", RunKind::Worker), inert_config())
            .await
            .unwrap();
        let _ = s.events.recv().await;
        let err = s
            .control
            .send_input(UserInputRequest {
                input: "followup".into(),
            })
            .await
            .unwrap_err();
        assert!(matches!(err, DriverError::Unsupported("send_input")));
    }

    #[tokio::test]
    async fn wait_for_input_ready_returns_ok_when_mock_pane_has_prompt() {
        let mut ready = false;
        let result = wait_for_input_ready_with_capture(
            Duration::from_secs(1),
            Duration::from_millis(10),
            || {
                ready = true;
                async move {
                    Ok(if ready {
                        "> followup prompt\n".to_string()
                    } else {
                        "booting harness\n".to_string()
                    })
                }
            },
        )
        .await;
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn wait_for_input_ready_returns_input_not_ready_on_timeout() {
        let timeout = Duration::from_millis(50);
        let err = wait_for_input_ready_with_capture(timeout, Duration::from_millis(10), || async {
            Ok("streaming assistant output\n".to_string())
        })
        .await
        .unwrap_err();
        assert!(matches!(err, DriverError::InputNotReady(_)));
    }

    /// Live rmux followup delivery. Drives a minimal interactive harness that
    /// shows a composer prompt, accepts the dispatch brief, then accepts a
    /// followup via `send_input`. Proves the followup lands in the pane with
    /// the EOT-marker instruction appended (round-end detection contract).
    /// Skipped without a real rmux binary.
    #[tokio::test]
    async fn live_rmux_send_input_delivers_followup_turn() {
        let _live_guard = live_session_guard();
        let probe = probe_rmux_binary();
        if !probe.found {
            eprintln!(
                "skipping live_rmux_send_input_delivers_followup_turn: rmux binary not found"
            );
            return;
        }
        const INITIAL: &str = "ORGASMIC_INITIAL_SENTINEL";
        const FOLLOWUP: &str = "ORGASMIC_FOLLOWUP_SENTINEL";
        let run_id = "run-send-input";
        let harness =
            "while true; do echo '> ready'; IFS= read -r line || exit 0; echo \"ECHO:$line\"; done";
        let d = driver();
        let cfg = DriverConfig::from_value(json!({
            "command": "sh",
            "args": ["-c", harness],
            "prompt_bundle_text": INITIAL,
            "input_ready_timeout": 5,
        }));
        let mut s = d.acquire(ctx(run_id, RunKind::Worker), cfg).await.unwrap();
        let ev = s.events.recv().await.unwrap();
        let DriverEvent::Ready { capabilities, .. } = ev else {
            panic!("expected Ready, got {ev:?}");
        };
        if capabilities["inert"] == true {
            eprintln!(
                "skipping live_rmux_send_input_delivers_followup_turn: SDK degraded to inert"
            );
            s.control.release("cleanup").await.unwrap();
            return;
        }
        let session_name = rmux_session_name(&s.identity);
        let bin = probe.path.as_deref().unwrap_or(RMUX_BINARY);

        let deadline = std::time::Instant::now() + Duration::from_secs(20);
        let mut pane = String::new();
        while std::time::Instant::now() < deadline {
            pane = rmux_capture_pane(bin, &session_name)
                .await
                .unwrap_or_default();
            let dispatch_done =
                pane.contains("ECHO:run_id:") || pane.contains("ECHO:ORGASMIC_INITIAL");
            if dispatch_done && pane.lines().any(pane_has_input_prompt) {
                break;
            }
            tokio::time::sleep(Duration::from_millis(150)).await;
        }
        assert!(
            pane.contains(INITIAL) && pane.lines().any(pane_has_input_prompt),
            "harness should finish dispatch and show composer prompt, got {pane}"
        );

        let ack = tokio::time::timeout(
            Duration::from_secs(8),
            s.control.send_input(UserInputRequest {
                input: FOLLOWUP.into(),
            }),
        )
        .await
        .expect("send_input timed out")
        .unwrap();
        assert!(ack.accepted, "followup should be accepted when ready");

        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        while std::time::Instant::now() < deadline {
            pane = rmux_capture_pane(bin, &session_name)
                .await
                .unwrap_or_default();
            if pane.contains(FOLLOWUP) {
                break;
            }
            tokio::time::sleep(Duration::from_millis(150)).await;
        }
        assert!(
            pane.contains(FOLLOWUP),
            "followup should land as a user turn, got {pane}"
        );
        let staged = followup_prompt_with_eot(FOLLOWUP, run_id);
        assert!(staged.contains("end-of-turn marker"));

        s.control.release("cleanup").await.unwrap();
    }

    /// Live rmux mid-turn guard: while the harness is streaming (no input
    /// prompt), `send_input` must reject rather than paste mid-stream.
    /// Skipped without a real rmux binary.
    #[tokio::test]
    async fn live_rmux_send_input_rejects_while_harness_busy() {
        let _live_guard = live_session_guard();
        let probe = probe_rmux_binary();
        if !probe.found {
            eprintln!(
                "skipping live_rmux_send_input_rejects_while_harness_busy: rmux binary not found"
            );
            return;
        }
        let d = driver();
        let cfg = DriverConfig::from_value(json!({
            "command": "sh",
            "args": ["-c", "i=0; while [ $i -lt 30 ]; do echo streaming-$i; i=$((i+1)); done"],
            "input_ready_timeout": 1,
        }));
        let mut s = d
            .acquire(ctx("run-busy", RunKind::Worker), cfg)
            .await
            .unwrap();
        let ev = s.events.recv().await.unwrap();
        let DriverEvent::Ready { capabilities, .. } = ev else {
            panic!("expected Ready, got {ev:?}");
        };
        if capabilities["inert"] == true {
            eprintln!(
                "skipping live_rmux_send_input_rejects_while_harness_busy: SDK degraded to inert"
            );
            s.control.release("cleanup").await.unwrap();
            return;
        }

        let ack = tokio::time::timeout(
            Duration::from_secs(5),
            s.control.send_input(UserInputRequest {
                input: "should-not-paste".into(),
            }),
        )
        .await
        .expect("send_input timed out")
        .unwrap();
        assert!(!ack.accepted, "busy harness must reject followup");
        assert_eq!(ack.message.as_deref(), Some("harness busy"));

        s.control.release("cleanup").await.unwrap();
    }
}
