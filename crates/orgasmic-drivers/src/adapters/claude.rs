// arch: arch_A53QX.3
// orgasmic:arch_A53QX, dec_ASB1A, task_TGGAJ
//! Claude Code harness adapter for ACP-like stdio stream-json.
//!
//! Claude Code's exposed programmatic wire is the Agent SDK JSONL stream:
//! `claude -p --input-format stream-json --output-format stream-json`.
//! The `endpoint` field in config is retained in capabilities for audit only.
//! An empty endpoint is normal for the ACP-stdio pairing, where the mode
//! upgrades this adapter's simulated request into `stdio_spawn`; ACP-WS keeps
//! the simulated request because a WebSocket URL is required there.
//!
//! Simulation is also used when `claude` is not detectable on PATH, or when
//! `ORGASMIC_DRIVER_SIMULATE=1` is set explicitly.  Both cases emit a WARN
//! log naming which check caused the fallback so the next operator can debug
//! without reading source.

use std::collections::BTreeMap;
use std::process::{Command as StdCommand, Stdio};

use async_trait::async_trait;
use serde::Deserialize;
use serde_json::{json, Value};
use tokio::sync::mpsc;

use orgasmic_core::{DriverEvent, TextStream};

use crate::modes::tmux::claude_session_path;
use crate::preflight::{classify_api_key, read_status_output};
use crate::r#trait::{
    BabysitterRequest, DriverConfig, DriverContext, DriverError, HarnessControlOutcome,
    HarnessEventAdapter, HarnessRequest, NativeRuntimeMeta, Preflight, RunKind, StdioSpawn,
    TransitionRequest,
};

const TRANSPORT: &str = "claude-acp";

/// How this run authenticates the `claude` subprocess.
///
/// `--bare` is the light path — it skips hooks, LSP, plugin sync, attribution,
/// auto-memory, background prefetches and CLAUDE.md auto-discovery — but its
/// contract is that OAuth and keychain are *never* read: credentials must come
/// from `ANTHROPIC_API_KEY` or an `apiKeyHelper`. An operator authenticated
/// through a claude.ai subscription cannot satisfy either, so hardcoding
/// `--bare` made the harness unusable for them (TASK-Z8WEJ).
///
/// Measured 2026-07-25 against claude 2.1.220: setting `CLAUDE_CODE_SIMPLE=1`
/// alone, with no `--bare` flag, fails identically. The lightweight behaviour
/// and the credential policy are one switch and cannot be separated, so the
/// fallback rebuilds isolation from narrower flags instead of trying to keep
/// bare mode.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ClaudeCredentialMode {
    /// `--bare` plus an API key in the child's environment.
    BareApiKey,
    /// No `--bare`; the harness reads its own login (keychain/OAuth) exactly as
    /// it does for an interactive operator. Isolation comes from
    /// `--strict-mcp-config`.
    ///
    /// Known gap, measured rather than assumed: this mode CANNOT suppress the
    /// operator's hooks. `--settings '{}'` and `--settings '{"hooks":{}}'` both
    /// still ran SessionStart hooks; `--bare` is the only flag that skips them,
    /// and `--include-hook-events` governs reporting, not execution. So a
    /// native-login worker executes whatever hooks the operator has configured.
    /// A minimal `--settings '{}'` was tried and dropped: the flag does accept
    /// inline JSON, but an empty object overrides nothing, so passing it only
    /// implied an isolation it never delivered.
    ///
    /// MCP is fully suppressed (`mcp_servers: []`, versus nine servers without
    /// the flag on the machine this was measured on), which is the larger half
    /// of "light". Accepted deliberately: the alternative is that
    /// subscription-authenticated operators cannot dispatch claude at all.
    NativeLogin,
}

impl ClaudeCredentialMode {
    fn as_str(self) -> &'static str {
        match self {
            Self::BareApiKey => "bare_api_key",
            Self::NativeLogin => "native_login",
        }
    }
}

pub struct ClaudeAdapter {
    translator: Option<AcpTranslator>,
    /// Native session identity for the run this adapter last composed, handed
    /// to the mode so the supervisor can record it (see `native_runtime`).
    native_runtime: Option<NativeRuntimeMeta>,
}

impl ClaudeAdapter {
    pub fn new() -> Self {
        Self {
            translator: None,
            native_runtime: None,
        }
    }

    async fn collect<F>(&mut self, f: F) -> Vec<DriverEvent>
    where
        F: for<'a> FnOnce(
            &'a mut AcpTranslator,
            mpsc::Sender<DriverEvent>,
        )
            -> std::pin::Pin<Box<dyn std::future::Future<Output = ()> + Send + 'a>>,
    {
        let Some(translator) = self.translator.as_mut() else {
            return Vec::new();
        };
        let (tx, mut rx) = mpsc::channel(32);
        f(translator, tx.clone()).await;
        drop(tx);
        let mut events = Vec::new();
        while let Ok(event) = rx.try_recv() {
            events.push(event);
        }
        events
    }
}

impl Default for ClaudeAdapter {
    fn default() -> Self {
        Self::new()
    }
}

#[derive(Debug, Clone, Deserialize, Default)]
struct ClaudeAcpConfig {
    #[serde(default)]
    endpoint: Option<String>,
    #[serde(default)]
    model: Option<String>,
    #[serde(default)]
    api_key_env: Option<String>,
    #[serde(default)]
    prompt_bundle_text: Option<String>,
}

#[async_trait]
impl HarnessEventAdapter for ClaudeAdapter {
    fn harness(&self) -> &'static str {
        "claude"
    }

    fn clone_box(&self) -> Box<dyn HarnessEventAdapter> {
        Box::new(ClaudeAdapter::new())
    }

    fn native_runtime(&self) -> Option<NativeRuntimeMeta> {
        self.native_runtime.clone()
    }

    async fn parse_event(&mut self, raw: Value) -> Vec<DriverEvent> {
        self.collect(|translator, tx| {
            Box::pin(async move {
                translator.translate_value(&tx, &raw).await;
            })
        })
        .await
    }

    async fn parse_stdout_line(&mut self, line: &str) -> Vec<DriverEvent> {
        let line = line.to_string();
        self.collect(|translator, tx| {
            Box::pin(async move {
                translator.translate_stdout_line(&tx, &line).await;
            })
        })
        .await
    }

    fn next_seq(&mut self) -> u64 {
        self.translator
            .as_mut()
            .map(AcpTranslator::next_seq)
            .unwrap_or(0)
    }

    fn validate_config(&self, config: &DriverConfig) -> Result<(), DriverError> {
        let cfg: ClaudeAcpConfig = serde_json::from_value(config.0.clone())
            .map_err(|e| DriverError::InvalidConfig(e.to_string()))?;
        if let Some(env_name) = cfg.api_key_env.as_deref() {
            if std::env::var(env_name).is_err() && cfg.endpoint.is_some() {
                return Err(DriverError::InvalidConfig(format!(
                    "api_key_env '{env_name}' not set but endpoint is configured"
                )));
            }
        }
        Ok(())
    }

    /// The wire flags every claude run needs, independent of how it
    /// authenticates. Credential-mode flags, isolation flags and the native
    /// session id are added by `compose_request`, which has the context this
    /// method does not.
    ///
    /// Deliberately no `--no-session-persistence`: suppressing the harness's
    /// own session recording left these runs with no resumable native
    /// transcript, and therefore no retro source and no `resume_native_fork`
    /// recovery action (dec_Y5MPK, TASK-VB9DQ).
    fn stdio_spawn(&self) -> Option<StdioSpawn> {
        Some(StdioSpawn {
            command: "claude".into(),
            args: vec![
                "-p".to_string(),
                "--input-format".to_string(),
                "stream-json".to_string(),
                "--output-format".to_string(),
                "stream-json".to_string(),
                "--include-partial-messages".to_string(),
                "--verbose".to_string(),
            ],
            cwd: None,
            env: Vec::new(),
        })
    }

    fn upgrades_simulated_to_subprocess(&self) -> bool {
        std::env::var("ORGASMIC_DRIVER_SIMULATE")
            .map(|v| v != "1")
            .unwrap_or(true)
    }

    fn stdio_initial_payload(
        &mut self,
        ctx: &DriverContext,
        config: &DriverConfig,
    ) -> Result<Option<Vec<u8>>, DriverError> {
        let cfg: ClaudeAcpConfig = serde_json::from_value(config.0.clone())
            .map_err(|e| DriverError::InvalidConfig(e.to_string()))?;
        self.translator = Some(AcpTranslator::new(
            cfg.endpoint.clone(),
            ctx.run_kind,
            cfg.model.clone(),
        ));
        Ok(Some(json_line_bytes(&claude_user_message(
            build_spawn_prompt(ctx, &cfg),
        ))?))
    }

    /// Rule on this worker's credentials before the dispatch commits anything.
    ///
    /// The probe resolves the credential mode through the same
    /// [`resolve_credentials`] the launch uses, then asks about the credential
    /// *that mode* consumes — the distinction the trait doc explains, and the
    /// reason this costs nothing. Measured on claude 2.1.220: 0.28 s and $0.
    ///
    /// What it cannot prove, stated plainly so nobody reads `Ready` as a
    /// guarantee: a login that exists can still be expired or rate-limited
    /// server-side, and an API key that is present can still be rejected.
    /// Establishing either requires submitting a real turn, which was measured
    /// at $0.0994 per dispatch and rejected on that ground.
    async fn preflight(&mut self, _ctx: &DriverContext, config: &DriverConfig) -> Preflight {
        let Ok(cfg) = serde_json::from_value::<ClaudeAcpConfig>(config.0.clone()) else {
            return Preflight::Unsupported;
        };
        if simulate_override() {
            // Nothing will present a credential, so there is nothing to rule on
            // and nothing that could fail at startup.
            return Preflight::Unsupported;
        }
        // Deliberately not the full `simulation_reason` check, for two reasons.
        //
        // It would call `claude_available()`, a *blocking* `claude --version`,
        // on the async path — the hazard TASK-KKGKM was about, where one
        // synchronous call inside a future stalls the task mid-poll. It also
        // does not need to: a missing binary makes the status command
        // unspawnable, which `read_status_output` already reports as
        // inconclusive.
        //
        // And an empty endpoint must *not* skip the probe. It makes this
        // adapter compose a simulated request, but acp-stdio then upgrades that
        // into a real subprocess whenever the binary is present
        // (`upgrades_simulated_to_subprocess`) — a real subprocess presenting a
        // real credential that can fail at startup. Skipping there would have
        // exempted the most common dispatch shape there is.
        let Ok(resolved) = resolve_credentials(&cfg) else {
            // A misconfigured `api_key_env` is already `validate`'s rejection
            // and reaches the operator as a config error, not a readiness one.
            return Preflight::Unsupported;
        };
        let verdict = match resolved.mode {
            ClaudeCredentialMode::BareApiKey => classify_bare_api_key(resolved.api_key.as_deref()),
            ClaudeCredentialMode::NativeLogin => {
                // Ask the same binary the launch will spawn.
                let command = self
                    .stdio_spawn()
                    .map(|spawn| spawn.command)
                    .unwrap_or_else(|| "claude".to_string());
                match read_status_output(&command, &["auth", "status"]).await {
                    // Claude answers in JSON on stdout; parsing a stream the
                    // harness may also use for warnings would be fragile.
                    Some(status) => classify_native_login(&status.stdout),
                    None => Preflight::Unsupported,
                }
            }
        };
        tracing::debug!(
            credential_mode = resolved.mode.as_str(),
            rejects = verdict.rejects_dispatch().is_some(),
            "claude preflight: resolved verdict"
        );
        verdict
    }

    fn compose_request(
        &mut self,
        ctx: &DriverContext,
        config: &DriverConfig,
    ) -> Result<HarnessRequest, DriverError> {
        let cfg: ClaudeAcpConfig = serde_json::from_value(config.0.clone())
            .map_err(|e| DriverError::InvalidConfig(e.to_string()))?;
        self.validate_config(config)?;
        if let Some(reason) = simulation_reason(&cfg) {
            if let Some(warning) = reason.warning() {
                tracing::warn!("{warning}");
            }
            return Ok(HarnessRequest::Simulated {
                events: simulated_start_events(ctx, &cfg),
            });
        }

        self.translator = Some(AcpTranslator::new(
            cfg.endpoint.clone(),
            ctx.run_kind,
            cfg.model.clone(),
        ));
        let spawn = self
            .stdio_spawn()
            .expect("claude adapter always exposes stdio_spawn");

        // Resolve credentials first: the mode decides which isolation flags are
        // even available (see `ClaudeCredentialMode`).
        let resolved = resolve_credentials(&cfg)?;
        let (mode, env) = (resolved.mode, resolved.env);

        let mut args = Vec::with_capacity(spawn.args.len() + 8);
        match mode {
            ClaudeCredentialMode::BareApiKey => args.push("--bare".to_string()),
            ClaudeCredentialMode::NativeLogin => {
                // Rebuild what `--bare` would have given us, minus its
                // credential policy. `--strict-mcp-config` alone yields
                // `mcp_servers: []` (measured); without it this machine loaded
                // nine MCP servers into a worker that wants none.
                args.push("--strict-mcp-config".to_string());
            }
        }
        args.extend(spawn.args.iter().cloned());

        // Pin the native session id to the run's runtime_id, exactly as the TUI
        // path does, so the vendor transcript lands at a path orgasmic can
        // compute rather than discover (dec_Y5MPK item 3). Verified: `-p` mode
        // honours `--session-id` and writes
        // `~/.claude/projects/<encoded-cwd>/<session-id>.jsonl`.
        let session_id = ctx.identity.runtime_id.clone();
        args.push("--session-id".to_string());
        args.push(session_id.clone());

        if let Some(model) = cfg.model.as_deref() {
            if !model.is_empty() {
                args.push("--model".into());
                args.push(model.to_string());
            }
        }

        let cwd = spawn.cwd.clone().or_else(|| ctx.worktree.clone());
        let mut launch_argv = vec![spawn.command.clone()];
        launch_argv.extend(args.iter().cloned());
        self.native_runtime = Some(NativeRuntimeMeta {
            provider: "claude".to_string(),
            session_id: Some(session_id.clone()),
            session_path: cwd
                .as_deref()
                .and_then(|cwd| claude_session_path(&session_id, cwd)),
            launch_argv,
            resume_argv: vec![
                spawn.command.clone(),
                "--resume".to_string(),
                session_id,
                "--fork-session".to_string(),
            ],
        });
        tracing::debug!(
            credential_mode = mode.as_str(),
            "claude stdio: resolved credential mode"
        );

        Ok(HarnessRequest::Subprocess {
            binary: spawn.command,
            args,
            env,
            cwd,
            stdin_payload: Some(json_line_bytes(&claude_user_message(build_spawn_prompt(
                ctx, &cfg,
            )))?),
            close_stdin: false,
        })
    }

    async fn transition_state(
        &mut self,
        req: TransitionRequest,
    ) -> Result<HarnessControlOutcome, DriverError> {
        let text = format!(
            "orgasmic control: transition_state requested\nfrom: {}\nto: {}\nreason: {}",
            req.from, req.to, req.reason
        );
        Ok(HarnessControlOutcome {
            events: vec![DriverEvent::TransitionState {
                from: req.from,
                to: req.to,
                reason: req.reason,
            }],
            stdin_payloads: vec![json_line_bytes(&claude_user_message(text))?],
            ..HarnessControlOutcome::default()
        })
    }

    async fn babysitter_action(
        &mut self,
        req: BabysitterRequest,
    ) -> Result<HarnessControlOutcome, DriverError> {
        let call_id = format!("acp-bs-{}", uuid::Uuid::new_v4());
        let payload = json!({
            "tool": req.tool.as_str(),
            "target_run": req.target_run,
            "payload": req.payload,
        });
        let text = format!(
            "orgasmic babysitter control action:\n```json\n{}\n```",
            serde_json::to_string_pretty(&payload).unwrap_or_else(|_| payload.to_string())
        );
        Ok(HarnessControlOutcome {
            events: vec![DriverEvent::ToolCall {
                call_id,
                name: payload["tool"].as_str().unwrap_or("unknown").into(),
                args: payload["payload"].clone(),
                seq: self.next_seq(),
            }],
            stdin_payloads: vec![json_line_bytes(&claude_user_message(text))?],
            ..HarnessControlOutcome::default()
        })
    }

    async fn release(&mut self, reason: String) -> Result<HarnessControlOutcome, DriverError> {
        let text = format!("orgasmic control: release requested\nreason: {reason}");
        Ok(HarnessControlOutcome {
            events: vec![DriverEvent::RunComplete {
                summary: Some(reason),
            }],
            stdin_payloads: vec![json_line_bytes(&claude_user_message(text))?],
            close: true,
            ..HarnessControlOutcome::default()
        })
    }
}

fn claude_available() -> bool {
    StdCommand::new("claude")
        .arg("--version")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

/// Why a run would take the simulated path instead of spawning `claude`.
///
/// Named rather than inlined because two callers must agree on it: the launch
/// short-circuits to canned events, and the preflight has nothing to probe. A
/// probe that reported on credentials a simulated run will never present would
/// be reporting on a process that is not going to exist.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SimulationReason {
    /// `ORGASMIC_DRIVER_SIMULATE=1`.
    ExplicitOverride,
    /// No `claude` binary is detectable.
    BinaryMissing,
}

impl SimulationReason {
    /// Both remaining reasons are worth saying out loud: each means no real
    /// harness will run. There is deliberately no quiet reason left — a silent
    /// simulation is how an unreachable code path stays unreachable (dec_S18RH).
    fn warning(self) -> Option<&'static str> {
        match self {
            Self::ExplicitOverride => Some(
                "claude-acp: ORGASMIC_DRIVER_SIMULATE=1 is set; using simulated mode (explicit override)",
            ),
            Self::BinaryMissing => Some(
                "claude-acp: 'claude' binary not found on PATH; using simulated mode (binary not detectable)",
            ),
        }
    }
}

/// The operator's explicit "do not touch a real harness" switch.
fn simulate_override() -> bool {
    std::env::var("ORGASMIC_DRIVER_SIMULATE")
        .map(|v| v == "1")
        .unwrap_or(false)
}

/// An empty `endpoint` is deliberately NOT a simulation reason (dec_S18RH).
///
/// It used to be, and every ordinary dispatch leaves `endpoint` empty — so
/// `compose_request` returned `Simulated` and never reached the argv it builds
/// below. The mode then "upgraded" that back into a real process spawned from
/// the bare `stdio_spawn` base, so the pinned `--session-id`, the resolved
/// credential mode and `--model` were computed, recorded, and dropped. The
/// tests passed because they pass an endpoint, exercising the one branch
/// production never takes (TASK-SGRTX, TASK-VB9DQ).
///
/// `claude -p` with line-delimited stream JSON is a real transport, not a
/// simulation of one. Having no ACP endpoint to dial does not make the run
/// fake; it only means this transport is the local binary.
fn simulation_reason(_cfg: &ClaudeAcpConfig) -> Option<SimulationReason> {
    if simulate_override() {
        return Some(SimulationReason::ExplicitOverride);
    }
    if !claude_available() {
        return Some(SimulationReason::BinaryMissing);
    }
    None
}

/// The credential a launched `claude` will actually present.
struct ResolvedCredentials {
    mode: ClaudeCredentialMode,
    /// Environment overrides for the child, beyond what it inherits.
    env: BTreeMap<String, String>,
    /// The API key the child will present, whether it came from the configured
    /// `api_key_env` or from an inherited `ANTHROPIC_API_KEY`. `None` in
    /// native-login mode, where the harness reads its own keychain.
    ///
    /// Never log or surface this: it is a secret, and preflight reasons reach
    /// durable task evidence.
    api_key: Option<String>,
}

/// Decide how a launched `claude` would authenticate.
///
/// Extracted from `compose_request` so the probe and the launch cannot drift.
/// That is the whole guarantee behind the preflight: a check that resolved the
/// credential mode by its own reasoning could confidently interrogate a
/// credential the worker was never going to use — which is precisely how
/// `claude auth status` produced a passing answer for a run that could not
/// start (see [`crate::WorkerDriver::preflight`]).
fn resolve_credentials(cfg: &ClaudeAcpConfig) -> Result<ResolvedCredentials, DriverError> {
    let mut env = BTreeMap::new();
    let (mode, api_key) = match cfg.api_key_env.as_deref() {
        Some(env_name) => {
            let api_key = std::env::var(env_name).map_err(|_| {
                DriverError::InvalidConfig(format!(
                    "api_key_env '{env_name}' not set but endpoint is configured"
                ))
            })?;
            env.insert("ANTHROPIC_API_KEY".into(), api_key.clone());
            (ClaudeCredentialMode::BareApiKey, Some(api_key))
        }
        None => match std::env::var("ANTHROPIC_API_KEY") {
            Ok(inherited) => (ClaudeCredentialMode::BareApiKey, Some(inherited)),
            Err(_) => (ClaudeCredentialMode::NativeLogin, None),
        },
    };
    Ok(ResolvedCredentials { mode, env, api_key })
}

/// Turn `claude auth status` output into a verdict about a native-login worker.
///
/// Separated from the subprocess so the classification is testable without
/// putting a stub on `PATH`; process-global `PATH` mutation is shared by every
/// test in the binary (`.orgasmic/gotchas.org`).
///
/// Claude is the one harness of the three that answers in JSON, so this reads a
/// boolean field instead of matching a sentence — a contract far less likely to
/// shift under a version bump than the prose the others emit.
fn classify_native_login(stdout: &str) -> Preflight {
    // Parse first, without consulting the exit status: measured 2026-07-25,
    // `claude auth status` exits 1 precisely when it is logged out, so the
    // non-zero exit accompanies the answer rather than replacing it.
    let Ok(status) = serde_json::from_str::<Value>(stdout) else {
        return Preflight::Unsupported;
    };
    // Read exactly one field. The payload also carries the operator's email,
    // org and subscription tier, and a preflight reason is durable evidence.
    match status.get("loggedIn").and_then(Value::as_bool) {
        Some(true) => Preflight::Ready,
        Some(false) => Preflight::fatal(
            "claude is not logged in. This worker authenticates through the harness's own \
             login (no ANTHROPIC_API_KEY is set), so it cannot start until you run `claude` \
             and complete /login on this machine.",
        ),
        None => Preflight::Unsupported,
    }
}

/// Verdict for a worker that will present an API key under `--bare`.
///
/// `--bare` reads no credential but the key, so an empty one is a certain
/// failure worth rejecting for free; see [`classify_api_key`] for why a
/// non-empty key is nonetheless not evidence of a working worker.
fn classify_bare_api_key(api_key: Option<&str>) -> Preflight {
    classify_api_key(
        api_key,
        "the ANTHROPIC_API_KEY this worker would present is empty, and `--bare` reads no \
         other credential source. Set a real key, or unset it to use the harness's own login.",
    )
}

fn build_spawn_prompt(ctx: &DriverContext, cfg: &ClaudeAcpConfig) -> String {
    let payload = json!({
        "transport": TRANSPORT,
        "wire": "claude-code-stdio-stream-json",
        "endpoint": cfg.endpoint,
        "model": cfg.model,
        "run": {
            "kind": ctx.run_kind,
            "task_id": ctx.task_id,
            "worker_id": ctx.worker_id,
            "project_id": ctx.project_id,
            "worktree": ctx.worktree.as_ref().map(|p| p.display().to_string()),
            "babysitter_target": ctx.babysitter_target,
        }
    });
    let pretty = serde_json::to_string_pretty(&payload).unwrap_or_else(|_| payload.to_string());
    let mut prompt = String::new();
    if let Some(bundle) = cfg.prompt_bundle_text.as_deref() {
        if !bundle.trim().is_empty() {
            prompt.push_str(bundle.trim());
            prompt.push_str("\n\n");
        }
    }
    prompt.push_str(
        "orgasmic runtime context follows. Treat this as runtime metadata for the worker run.\n\n```json\n",
    );
    prompt.push_str(&pretty);
    prompt.push_str("\n```\n");
    prompt
}

fn claude_user_message(text: String) -> Value {
    json!({
        "type": "user",
        "message": {
            "role": "user",
            "content": [
                {
                    "type": "text",
                    "text": text,
                }
            ],
        },
        "parent_tool_use_id": null,
    })
}

fn json_line_bytes(value: &Value) -> Result<Vec<u8>, DriverError> {
    let mut line = serde_json::to_vec(value).map_err(|e| DriverError::Other(e.to_string()))?;
    line.push(b'\n');
    Ok(line)
}

fn simulated_start_events(ctx: &DriverContext, cfg: &ClaudeAcpConfig) -> Vec<DriverEvent> {
    vec![DriverEvent::Ready {
        protocol_version: "acp/1".into(),
        capabilities: json!({
            "simulated": true,
            "kind": ctx.run_kind,
            "model": cfg.model,
        }),
    }]
}

struct StreamingTool {
    call_id: String,
    name: String,
    initial_input: Value,
    partial_json: String,
}

struct AcpTranslator {
    seq: u64,
    endpoint: Option<String>,
    kind: RunKind,
    configured_model: Option<String>,
    streaming_tools: BTreeMap<u64, StreamingTool>,
    saw_partial_text: bool,
}

impl AcpTranslator {
    fn new(endpoint: Option<String>, kind: RunKind, configured_model: Option<String>) -> Self {
        Self {
            seq: 0,
            endpoint,
            kind,
            configured_model,
            streaming_tools: BTreeMap::new(),
            saw_partial_text: false,
        }
    }

    fn next_seq(&mut self) -> u64 {
        let seq = self.seq;
        self.seq += 1;
        seq
    }

    async fn translate_stdout_line(&mut self, events: &mpsc::Sender<DriverEvent>, line: &str) {
        match serde_json::from_str::<Value>(line) {
            Ok(value) => self.translate_value(events, &value).await,
            Err(_) => {
                let _ = events
                    .send(DriverEvent::TextChunk {
                        stream: TextStream::Stdout,
                        chunk: line.to_string(),
                        seq: self.next_seq(),
                    })
                    .await;
            }
        }
    }

    async fn translate_value(&mut self, events: &mpsc::Sender<DriverEvent>, value: &Value) {
        match value.get("type").and_then(Value::as_str) {
            Some("system") => self.translate_system(events, value).await,
            Some("assistant") => self.translate_assistant(events, value).await,
            Some("stream_event") => self.translate_stream_event(events, value).await,
            Some("result") => self.translate_result(events, value).await,
            Some("user") => self.translate_user(events, value).await,
            Some(other) => {
                let _ = events
                    .send(DriverEvent::TextChunk {
                        stream: TextStream::System,
                        chunk: format!("claude event {other}: {value}"),
                        seq: self.next_seq(),
                    })
                    .await;
            }
            None => {
                let _ = events
                    .send(DriverEvent::TextChunk {
                        stream: TextStream::System,
                        chunk: format!("claude event: {value}"),
                        seq: self.next_seq(),
                    })
                    .await;
            }
        }
    }

    async fn translate_system(&mut self, events: &mpsc::Sender<DriverEvent>, value: &Value) {
        match value.get("subtype").and_then(Value::as_str) {
            Some("init") => {
                let observed_model = value
                    .get("model")
                    .and_then(Value::as_str)
                    .or(self.configured_model.as_deref());
                let _ = events
                    .send(DriverEvent::Ready {
                        protocol_version: "claude-code-stream-json/1".into(),
                        capabilities: json!({
                            "simulated": false,
                            "kind": self.kind,
                            "wire": "stdio-stream-json",
                            "endpoint": self.endpoint,
                            "model": observed_model,
                            "session_id": value.get("session_id").cloned().unwrap_or(Value::Null),
                            "claude_code_version": value.get("claude_code_version").cloned().unwrap_or(Value::Null),
                        }),
                    })
                    .await;
            }
            Some("status") => {
                let status = value
                    .get("status")
                    .and_then(Value::as_str)
                    .unwrap_or("unknown");
                let _ = events
                    .send(DriverEvent::TextChunk {
                        stream: TextStream::System,
                        chunk: format!("claude status: {status}"),
                        seq: self.next_seq(),
                    })
                    .await;
            }
            Some(subtype) => {
                let _ = events
                    .send(DriverEvent::TextChunk {
                        stream: TextStream::System,
                        chunk: format!("claude system {subtype}: {value}"),
                        seq: self.next_seq(),
                    })
                    .await;
            }
            None => {
                let _ = events
                    .send(DriverEvent::TextChunk {
                        stream: TextStream::System,
                        chunk: format!("claude system: {value}"),
                        seq: self.next_seq(),
                    })
                    .await;
            }
        }
    }

    async fn translate_assistant(&mut self, events: &mpsc::Sender<DriverEvent>, value: &Value) {
        if let Some(content) = value.pointer("/message/content") {
            self.translate_content(events, content, !self.saw_partial_text)
                .await;
        }
        if let Some(error) = value.get("error").and_then(Value::as_str) {
            let text = value
                .pointer("/message/content/0/text")
                .and_then(Value::as_str)
                .unwrap_or(error);
            let _ = events
                .send(DriverEvent::DriverError {
                    fatal: true,
                    message: format!("claude {error}: {text}"),
                })
                .await;
        }
    }

    async fn translate_user(&mut self, events: &mpsc::Sender<DriverEvent>, value: &Value) {
        if let Some(content) = value.pointer("/message/content") {
            self.translate_tool_results(events, content).await;
        }
    }

    async fn translate_content(
        &mut self,
        events: &mpsc::Sender<DriverEvent>,
        content: &Value,
        emit_text: bool,
    ) {
        if let Some(text) = content.as_str() {
            if emit_text && !text.is_empty() {
                let _ = events
                    .send(DriverEvent::TextChunk {
                        stream: TextStream::Assistant,
                        chunk: text.to_string(),
                        seq: self.next_seq(),
                    })
                    .await;
            }
            return;
        }
        let Some(items) = content.as_array() else {
            return;
        };
        for item in items {
            match item.get("type").and_then(Value::as_str) {
                Some("text") if emit_text => {
                    if let Some(text) = item.get("text").and_then(Value::as_str) {
                        if !text.is_empty() {
                            let _ = events
                                .send(DriverEvent::TextChunk {
                                    stream: TextStream::Assistant,
                                    chunk: text.to_string(),
                                    seq: self.next_seq(),
                                })
                                .await;
                        }
                    }
                }
                Some("tool_use") => {
                    let call_id = item
                        .get("id")
                        .and_then(Value::as_str)
                        .map(ToString::to_string)
                        .unwrap_or_else(|| format!("claude-tool-{}", uuid::Uuid::new_v4()));
                    let name = item
                        .get("name")
                        .and_then(Value::as_str)
                        .unwrap_or("unknown")
                        .to_string();
                    let args = item.get("input").cloned().unwrap_or(Value::Null);
                    let _ = events
                        .send(DriverEvent::ToolCall {
                            call_id,
                            name,
                            args,
                            seq: self.next_seq(),
                        })
                        .await;
                }
                _ => {}
            }
        }
    }

    async fn translate_tool_results(
        &mut self,
        events: &mpsc::Sender<DriverEvent>,
        content: &Value,
    ) {
        let Some(items) = content.as_array() else {
            return;
        };
        for item in items {
            if item.get("type").and_then(Value::as_str) != Some("tool_result") {
                continue;
            }
            let call_id = item
                .get("tool_use_id")
                .and_then(Value::as_str)
                .map(ToString::to_string)
                .unwrap_or_else(|| format!("claude-tool-result-{}", uuid::Uuid::new_v4()));
            let ok = !item
                .get("is_error")
                .and_then(Value::as_bool)
                .unwrap_or(false);
            let _ = events
                .send(DriverEvent::ToolResult {
                    call_id,
                    ok,
                    output: item.clone(),
                    seq: self.next_seq(),
                })
                .await;
        }
    }

    async fn translate_stream_event(&mut self, events: &mpsc::Sender<DriverEvent>, value: &Value) {
        let Some(event) = value.get("event") else {
            return;
        };
        match event.get("type").and_then(Value::as_str) {
            Some("content_block_start") => {
                let index = event.get("index").and_then(Value::as_u64).unwrap_or(0);
                if let Some(block) = event.get("content_block") {
                    if block.get("type").and_then(Value::as_str) == Some("tool_use") {
                        self.streaming_tools.insert(
                            index,
                            StreamingTool {
                                call_id: block
                                    .get("id")
                                    .and_then(Value::as_str)
                                    .map(ToString::to_string)
                                    .unwrap_or_else(|| {
                                        format!("claude-tool-{}", uuid::Uuid::new_v4())
                                    }),
                                name: block
                                    .get("name")
                                    .and_then(Value::as_str)
                                    .unwrap_or("unknown")
                                    .to_string(),
                                initial_input: block.get("input").cloned().unwrap_or(Value::Null),
                                partial_json: String::new(),
                            },
                        );
                    }
                }
            }
            Some("content_block_delta") => {
                let delta = event.get("delta").unwrap_or(&Value::Null);
                match delta.get("type").and_then(Value::as_str) {
                    Some("text_delta") => {
                        if let Some(text) = delta.get("text").and_then(Value::as_str) {
                            self.saw_partial_text = true;
                            if !text.is_empty() {
                                let _ = events
                                    .send(DriverEvent::TextChunk {
                                        stream: TextStream::Assistant,
                                        chunk: text.to_string(),
                                        seq: self.next_seq(),
                                    })
                                    .await;
                            }
                        }
                    }
                    Some("input_json_delta") => {
                        let index = event.get("index").and_then(Value::as_u64).unwrap_or(0);
                        if let Some(tool) = self.streaming_tools.get_mut(&index) {
                            if let Some(partial) = delta.get("partial_json").and_then(Value::as_str)
                            {
                                tool.partial_json.push_str(partial);
                            }
                        }
                    }
                    _ => {}
                }
            }
            Some("content_block_stop") => {
                let index = event.get("index").and_then(Value::as_u64).unwrap_or(0);
                if let Some(tool) = self.streaming_tools.remove(&index) {
                    let args = if tool.partial_json.trim().is_empty() {
                        tool.initial_input
                    } else {
                        serde_json::from_str(&tool.partial_json)
                            .unwrap_or_else(|_| json!({"partial_json": tool.partial_json}))
                    };
                    let _ = events
                        .send(DriverEvent::ToolCall {
                            call_id: tool.call_id,
                            name: tool.name,
                            args,
                            seq: self.next_seq(),
                        })
                        .await;
                }
            }
            _ => {}
        }
    }

    async fn translate_result(&mut self, events: &mpsc::Sender<DriverEvent>, value: &Value) {
        let summary = value
            .get("result")
            .and_then(Value::as_str)
            .map(ToString::to_string);
        let seq = self.next_seq();
        let _ = events.send(DriverEvent::AgentTurnComplete { seq }).await;
        if value
            .get("is_error")
            .and_then(Value::as_bool)
            .unwrap_or(false)
        {
            let code = value
                .get("api_error_status")
                .and_then(Value::as_i64)
                .map(|s| format!("claude_api_error_{s}"))
                .unwrap_or_else(|| "claude_result_error".into());
            let message = summary.unwrap_or_else(|| value.to_string());
            let _ = events
                .send(DriverEvent::RunFail {
                    error_code: code,
                    error_markdown: message,
                })
                .await;
        } else {
            let _ = events.send(DriverEvent::RunComplete { summary }).await;
        }
    }
}

/// Simulated config used by supervisor smoke tests and CI runs without a
/// live `claude` binary.
pub fn simulated_config() -> DriverConfig {
    DriverConfig::from_value(Value::Object(Default::default()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{AcpStdioDriver, AcpWsDriver, AttachOutcome, ClaudeAcpDriver, WorkerDriver};
    use orgasmic_core::RuntimeIdentity;
    use tokio::time::{timeout, Duration};

    fn ctx(id: &str, kind: RunKind) -> DriverContext {
        DriverContext {
            identity: RuntimeIdentity::new(id, "boot-test"),
            run_kind: kind,
            task_id: "TASK-006".into(),
            worker_id: "implementer-claude".into(),
            project_id: Some("orgasmic".into()),
            worktree: None,
            babysitter_target: None,
        }
    }

    /// Compose a real subprocess request and return its argv, bypassing the
    /// simulate short-circuit so the actual launch flags are asserted.
    fn composed_args(cfg: Value) -> (Vec<String>, Option<NativeRuntimeMeta>) {
        let mut adapter = ClaudeAdapter::new();
        let ctx = ctx("run-args", RunKind::Worker);
        let request = adapter
            .compose_request(&ctx, &DriverConfig(cfg))
            .expect("compose");
        let args = match request {
            HarnessRequest::Subprocess { args, .. } => args,
            other => panic!(
                "expected a subprocess request, got {other:?}; is `claude` missing from PATH?"
            ),
        };
        (args, adapter.native_runtime())
    }

    /// A config that forces the real (non-simulated) path: a non-empty
    /// endpoint, with no api_key_env so credential resolution picks the
    /// operator's own login.
    fn subprocess_config() -> Value {
        json!({"endpoint": "stdio://claude"})
    }

    #[tokio::test]
    async fn native_login_mode_drops_bare_and_isolates_without_it() {
        let _guard = env_lock().lock().await;
        std::env::remove_var("ORGASMIC_DRIVER_SIMULATE");
        std::env::remove_var("ANTHROPIC_API_KEY");
        if !claude_available() {
            eprintln!("skipping: `claude` not on PATH");
            return;
        }
        let (args, _) = composed_args(subprocess_config());

        // `--bare` never reads OAuth or the keychain, so a subscription
        // operator could not authenticate at all while it was hardcoded.
        assert!(
            !args.iter().any(|a| a == "--bare"),
            "native-login mode must not pass --bare: {args:?}"
        );
        // Isolation is rebuilt from narrower flags: measured, --strict-mcp-config
        // alone yields `mcp_servers: []`.
        assert!(args.iter().any(|a| a == "--strict-mcp-config"), "{args:?}");
        // Deliberately no `--settings {}`: the flag accepts inline JSON, but an
        // empty object overrides nothing, so passing it would only imply an
        // isolation this mode does not provide.
        assert!(!args.iter().any(|a| a == "--settings"), "{args:?}");
    }

    #[tokio::test]
    async fn api_key_mode_keeps_the_light_bare_path() {
        let _guard = env_lock().lock().await;
        std::env::remove_var("ORGASMIC_DRIVER_SIMULATE");
        if !claude_available() {
            eprintln!("skipping: `claude` not on PATH");
            return;
        }
        std::env::set_var("ANTHROPIC_API_KEY", "sk-ant-test-not-real");
        let (args, _) = composed_args(subprocess_config());
        std::env::remove_var("ANTHROPIC_API_KEY");

        assert!(
            args.iter().any(|a| a == "--bare"),
            "an API key is the one credential --bare accepts, so keep the light path: {args:?}"
        );
        assert!(
            !args.iter().any(|a| a == "--strict-mcp-config"),
            "--bare already suppresses MCP; do not double up: {args:?}"
        );
    }

    // ---- preflight (TASK-TJKFC) ---------------------------------------

    /// The exact payload shape `claude auth status` emits, captured from
    /// claude 2.1.220 on 2026-07-25. Kept verbatim rather than minimised: the
    /// fields this probe must *not* read are as much a part of the contract as
    /// the one it does.
    fn auth_status_payload(logged_in: bool) -> String {
        if logged_in {
            json!({
                "loggedIn": true,
                "authMethod": "claude.ai",
                "apiProvider": "firstParty",
                "email": "operator@example.com",
                "orgId": "5cfb7ac5-4f69-4a41-8435-bc905f0f36fd",
                "orgName": "Example Org",
                "subscriptionType": "max"
            })
        } else {
            // The logged-out answer is genuinely this short — it names no
            // operator because there is none.
            json!({
                "loggedIn": false,
                "authMethod": "none",
                "apiProvider": "firstParty"
            })
        }
        .to_string()
    }

    /// `claude auth status` exits 1 when logged out and 0 when logged in
    /// (measured 2026-07-25). The stub reproduces that, because an earlier
    /// version of this probe gated on a zero exit and so read the rejection it
    /// exists to catch as "no answer".
    fn auth_status_exit_code(logged_in: bool) -> u8 {
        if logged_in {
            0
        } else {
            1
        }
    }

    #[test]
    fn a_logged_out_harness_is_a_definitive_rejection_with_a_remedy() {
        let verdict = classify_native_login(&auth_status_payload(false));
        let reason = verdict
            .rejects_dispatch()
            .expect("a harness that says it is logged out must reject the dispatch");
        // The operator reading this in a failed dispatch needs to know what to
        // do, not merely that something was wrong.
        assert!(reason.contains("/login"), "{reason}");
    }

    /// A forward guard, not a description of today's payload: the real
    /// logged-out answer happens to carry no identity, but the logged-in one
    /// carries the operator's email, org and plan, and a plausible future
    /// rejection ("logged in, token expired") would carry both. Preflight
    /// reasons reach tx records and task evidence, which are committed and may
    /// be published (see the entry router's contributing discipline), so the
    /// reason must stay a constant rather than echo what the harness said.
    #[test]
    fn a_preflight_reason_never_carries_the_operator_identity() {
        let identity_bearing_rejection = json!({
            "loggedIn": false,
            "authMethod": "claude.ai",
            "email": "operator@example.com",
            "orgId": "5cfb7ac5-4f69-4a41-8435-bc905f0f36fd",
            "orgName": "Example Org",
            "subscriptionType": "max"
        })
        .to_string();
        let verdict = classify_native_login(&identity_bearing_rejection);
        let reason = verdict.rejects_dispatch().expect("fatal");
        for secret in ["operator@example.com", "Example Org", "5cfb7ac5", "max"] {
            assert!(
                !reason.contains(secret),
                "preflight reason leaked {secret:?}: {reason}"
            );
        }
    }

    #[test]
    fn a_logged_in_harness_is_ready() {
        assert_eq!(
            classify_native_login(&auth_status_payload(true)),
            Preflight::Ready
        );
    }

    /// Every way of failing to *get* an answer is inconclusive, never fatal.
    /// Rejecting a dispatch because the probe itself broke would turn a
    /// safeguard into a new outage.
    #[test]
    fn an_unanswerable_probe_never_rejects_a_dispatch() {
        let inconclusive = [
            // Not JSON at all — an older or newer harness, or an error banner.
            classify_native_login("claude: unknown command 'auth'"),
            // JSON without the field this probe reads.
            classify_native_login(r#"{"authMethod":"claude.ai"}"#),
            // Empty output.
            classify_native_login(""),
        ];
        for verdict in inconclusive {
            assert_eq!(verdict, Preflight::Unsupported, "{verdict:?}");
            assert!(verdict.rejects_dispatch().is_none());
        }
    }

    #[test]
    fn an_empty_api_key_is_fatal_and_a_present_one_is_merely_unchecked() {
        assert!(classify_bare_api_key(Some("")).rejects_dispatch().is_some());
        assert!(classify_bare_api_key(Some("   "))
            .rejects_dispatch()
            .is_some());
        // Present but unverified. `Ready` would claim the API accepted a key
        // nobody presented to it; only a billed turn could establish that.
        assert_eq!(
            classify_bare_api_key(Some("sk-ant-not-real")),
            Preflight::Unsupported
        );
        assert_eq!(classify_bare_api_key(None), Preflight::Unsupported);
    }

    /// The guarantee the whole probe rests on: it must rule on the credential
    /// the launch will actually present. Asserted jointly — one env, both code
    /// paths — because a probe that resolved the mode by its own reasoning
    /// could confidently interrogate a credential the worker never uses, which
    /// is exactly how `claude auth status` passed a run that could not start.
    #[tokio::test]
    async fn the_probe_and_the_launch_resolve_the_same_credential() {
        let _guard = env_lock().lock().await;
        std::env::remove_var("ORGASMIC_DRIVER_SIMULATE");
        if !claude_available() {
            eprintln!("skipping: `claude` not on PATH");
            return;
        }
        std::env::set_var("ANTHROPIC_API_KEY", "");

        let (args, _) = composed_args(subprocess_config());
        let verdict = ClaudeAdapter::new()
            .preflight(
                &ctx("run-preflight-agree", RunKind::Worker),
                &DriverConfig(subprocess_config()),
            )
            .await;
        std::env::remove_var("ANTHROPIC_API_KEY");

        // The launch commits to `--bare`, whose only credential is the key…
        assert!(
            args.iter().any(|a| a == "--bare"),
            "an ANTHROPIC_API_KEY, even an empty one, selects the bare path: {args:?}"
        );
        // …so the probe must rule on the key, not on the operator's login.
        assert!(
            verdict.rejects_dispatch().is_some(),
            "an empty key under --bare cannot start a worker, so preflight must \
             reject it rather than defer to the harness's own login: {verdict:?}"
        );
    }

    /// A simulated run spawns nothing and presents no credentials. Probing one
    /// would rule on a process that is never going to exist.
    #[tokio::test]
    async fn a_simulated_run_is_never_preflighted() {
        let _guard = env_lock().lock().await;
        std::env::set_var("ORGASMIC_DRIVER_SIMULATE", "1");
        let verdict = ClaudeAdapter::new()
            .preflight(
                &ctx("run-preflight-sim", RunKind::Worker),
                &simulated_config(),
            )
            .await;
        std::env::remove_var("ORGASMIC_DRIVER_SIMULATE");
        assert_eq!(verdict, Preflight::Unsupported);
    }

    /// End to end through the transport the 2026-07-25 incident used: a
    /// logged-out harness must reach the supervisor as a rejection, not as a
    /// worker that dies 1.2 s after acquiring a lease and a worktree.
    #[tokio::test]
    async fn acp_stdio_rejects_a_dispatch_for_a_logged_out_claude() {
        let dir = tempfile::tempdir().expect("tempdir");
        make_auth_status_stub(dir.path(), false);

        let _guard = env_lock().lock().await;
        let saved_path = std::env::var("PATH").unwrap_or_default();
        // Prepend rather than replace: a bare tempdir as the whole PATH breaks
        // every other test in this binary that spawns a real tool
        // (`.orgasmic/gotchas.org`).
        std::env::set_var("PATH", format!("{}:{}", dir.path().display(), saved_path));
        std::env::remove_var("ORGASMIC_DRIVER_SIMULATE");
        std::env::remove_var("ANTHROPIC_API_KEY");

        let driver = AcpStdioDriver::new(Box::new(ClaudeAdapter::new()));
        let verdict = driver
            .preflight(
                &ctx("run-preflight-stdio", RunKind::Worker),
                &DriverConfig(subprocess_config()),
            )
            .await;

        std::env::set_var("PATH", saved_path);
        assert!(
            verdict.rejects_dispatch().is_some(),
            "acp-stdio must carry the harness's verdict through to the supervisor: {verdict:?}"
        );
    }

    /// An empty endpoint is not a reason to skip the probe: acp-stdio upgrades
    /// that config into a real `claude` subprocess whenever the binary exists,
    /// so the credential is just as real as with an endpoint set.
    #[tokio::test]
    async fn an_empty_endpoint_still_gets_a_verdict_because_it_still_spawns_claude() {
        let dir = tempfile::tempdir().expect("tempdir");
        make_auth_status_stub(dir.path(), false);

        let _guard = env_lock().lock().await;
        let saved_path = std::env::var("PATH").unwrap_or_default();
        std::env::set_var("PATH", format!("{}:{}", dir.path().display(), saved_path));
        std::env::remove_var("ORGASMIC_DRIVER_SIMULATE");
        std::env::remove_var("ANTHROPIC_API_KEY");

        let verdict = AcpStdioDriver::new(Box::new(ClaudeAdapter::new()))
            .preflight(
                &ctx("run-preflight-no-endpoint", RunKind::Worker),
                // No endpoint at all — the shape most dispatches use.
                &DriverConfig(json!({})),
            )
            .await;

        std::env::set_var("PATH", saved_path);
        assert!(
            verdict.rejects_dispatch().is_some(),
            "an endpoint-less config still launches a real claude, so it must be \
             ruled on: {verdict:?}"
        );
    }

    #[tokio::test]
    async fn acp_stdio_accepts_a_dispatch_for_a_logged_in_claude() {
        let dir = tempfile::tempdir().expect("tempdir");
        make_auth_status_stub(dir.path(), true);

        let _guard = env_lock().lock().await;
        let saved_path = std::env::var("PATH").unwrap_or_default();
        std::env::set_var("PATH", format!("{}:{}", dir.path().display(), saved_path));
        std::env::remove_var("ORGASMIC_DRIVER_SIMULATE");
        std::env::remove_var("ANTHROPIC_API_KEY");

        let driver = AcpStdioDriver::new(Box::new(ClaudeAdapter::new()));
        let verdict = driver
            .preflight(
                &ctx("run-preflight-stdio-ok", RunKind::Worker),
                &DriverConfig(subprocess_config()),
            )
            .await;

        std::env::set_var("PATH", saved_path);
        assert_eq!(verdict, Preflight::Ready);
    }

    #[tokio::test]
    async fn every_mode_persists_a_locatable_native_session() {
        let _guard = env_lock().lock().await;
        std::env::remove_var("ORGASMIC_DRIVER_SIMULATE");
        std::env::remove_var("ANTHROPIC_API_KEY");
        if !claude_available() {
            eprintln!("skipping: `claude` not on PATH");
            return;
        }
        let (args, native) = composed_args(subprocess_config());

        // Suppressing persistence left these runs with no resumable transcript,
        // so no retro source and no resume_native_fork recovery action.
        assert!(
            !args.iter().any(|a| a == "--no-session-persistence"),
            "vendor persistence must never be suppressed: {args:?}"
        );
        // Correlation is minted, not discovered (dec_Y5MPK item 3).
        let session_id = args
            .windows(2)
            .find(|w| w[0] == "--session-id")
            .map(|w| w[1].clone())
            .expect("a native session id must be pinned before launch");

        let native = native.expect("the adapter must report NativeRuntime metadata");
        assert_eq!(native.provider, "claude");
        assert_eq!(native.session_id.as_deref(), Some(session_id.as_str()));
        assert!(
            native
                .resume_argv
                .windows(2)
                .any(|w| w == ["--resume", session_id.as_str()]),
            "resume argv must target the pinned session: {:?}",
            native.resume_argv
        );
        assert!(native.launch_argv.iter().any(|a| a == "--session-id"));
    }

    #[tokio::test]
    async fn simulated_acquire_emits_ready_and_release() {
        // Explicit simulate override so the test is not affected by whether
        // the `claude` binary is on PATH on the host running the suite.
        // tokio::sync::MutexGuard may be held across await points; clippy's
        // `await_holding_lock` lint targets only std::sync::MutexGuard.
        let _guard = env_lock().lock().await;
        std::env::set_var("ORGASMIC_DRIVER_SIMULATE", "1");

        let d = ClaudeAcpDriver;
        let mut s = d
            .acquire(ctx("run-c", RunKind::Worker), simulated_config())
            .await
            .unwrap();
        let ev = s.events.recv().await.unwrap();
        assert!(matches!(ev, DriverEvent::Ready { .. }));
        s.control.release("done").await.unwrap();
        let last = s.events.recv().await.unwrap();
        std::env::remove_var("ORGASMIC_DRIVER_SIMULATE");
        assert!(matches!(last, DriverEvent::RunComplete { .. }));
    }

    #[tokio::test]
    async fn missing_api_key_with_endpoint_fails_validation() {
        let d = ClaudeAcpDriver;
        std::env::remove_var("ORGASMIC_TEST_MISSING_ACP_KEY");
        let cfg = DriverConfig::from_value(json!({
            "endpoint": "https://example.com",
            "api_key_env": "ORGASMIC_TEST_MISSING_ACP_KEY"
        }));
        assert!(d.validate(&cfg).is_err());
    }

    #[test]
    fn transport_name_is_stable() {
        assert_eq!(ClaudeAcpDriver.transport(), "claude-acp");
    }

    #[tokio::test]
    async fn attach_is_not_reattachable() {
        let d = ClaudeAcpDriver;
        let outcome = d
            .attach(ctx("run-acp-attach", RunKind::Worker), simulated_config())
            .await
            .unwrap();
        assert!(matches!(outcome, AttachOutcome::NotReattachable));
    }

    #[tokio::test]
    async fn stream_json_init_emits_ready() {
        let (tx, mut rx) = mpsc::channel(8);
        let mut translator =
            AcpTranslator::new(Some("stdio".into()), RunKind::Worker, Some("sonnet".into()));
        translator
            .translate_value(
                &tx,
                &json!({
                    "type": "system",
                    "subtype": "init",
                    "session_id": "sess-1",
                    "model": "claude-sonnet-4-6",
                    "claude_code_version": "2.1.147"
                }),
            )
            .await;
        let ev = rx.recv().await.unwrap();
        let DriverEvent::Ready {
            protocol_version,
            capabilities,
        } = ev
        else {
            panic!("expected Ready");
        };
        assert_eq!(protocol_version, "claude-code-stream-json/1");
        assert_eq!(capabilities["simulated"], false);
        assert_eq!(capabilities["wire"], "stdio-stream-json");
        assert_eq!(capabilities["session_id"], "sess-1");
    }

    #[tokio::test]
    async fn stream_json_text_delta_maps_to_assistant_chunk() {
        let (tx, mut rx) = mpsc::channel(8);
        let mut translator = AcpTranslator::new(None, RunKind::Worker, None);
        translator
            .translate_value(
                &tx,
                &json!({
                    "type": "stream_event",
                    "event": {
                        "type": "content_block_delta",
                        "index": 0,
                        "delta": {
                            "type": "text_delta",
                            "text": "hello"
                        }
                    }
                }),
            )
            .await;
        let ev = rx.recv().await.unwrap();
        assert_eq!(
            ev,
            DriverEvent::TextChunk {
                stream: TextStream::Assistant,
                chunk: "hello".into(),
                seq: 0,
            }
        );
    }

    #[tokio::test]
    async fn stream_json_tool_delta_maps_to_tool_call() {
        let (tx, mut rx) = mpsc::channel(8);
        let mut translator = AcpTranslator::new(None, RunKind::Worker, None);
        translator
            .translate_value(
                &tx,
                &json!({
                    "type": "stream_event",
                    "event": {
                        "type": "content_block_start",
                        "index": 1,
                        "content_block": {
                            "type": "tool_use",
                            "id": "toolu_1",
                            "name": "Bash",
                            "input": {}
                        }
                    }
                }),
            )
            .await;
        translator
            .translate_value(
                &tx,
                &json!({
                    "type": "stream_event",
                    "event": {
                        "type": "content_block_delta",
                        "index": 1,
                        "delta": {
                            "type": "input_json_delta",
                            "partial_json": "{\"command\":\"git status\"}"
                        }
                    }
                }),
            )
            .await;
        translator
            .translate_value(
                &tx,
                &json!({
                    "type": "stream_event",
                    "event": {
                        "type": "content_block_stop",
                        "index": 1
                    }
                }),
            )
            .await;
        let ev = rx.recv().await.unwrap();
        assert_eq!(
            ev,
            DriverEvent::ToolCall {
                call_id: "toolu_1".into(),
                name: "Bash".into(),
                args: json!({"command": "git status"}),
                seq: 0,
            }
        );
    }

    // Serialize tests that mutate process env.
    //
    // A single tokio::sync::Mutex is shared by both sync tests (use
    // blocking_lock()) and async tests (use .lock().await). tokio's MutexGuard
    // may be held across await points without triggering the
    // `await_holding_lock` clippy lint — that lint targets only std::sync::Mutex.
    fn env_lock() -> &'static tokio::sync::Mutex<()> {
        static LOCK: std::sync::OnceLock<tokio::sync::Mutex<()>> = std::sync::OnceLock::new();
        LOCK.get_or_init(|| tokio::sync::Mutex::new(()))
    }

    /// A `claude` that answers `--version` (so the adapter sees a real binary
    /// rather than falling back to simulation) and `auth status` with a chosen
    /// login state. Anything else exits non-zero: a preflight test that
    /// accidentally launched a worker should fail loudly, not silently pass.
    fn make_auth_status_stub(dir: &std::path::Path, logged_in: bool) {
        let stub = dir.join("claude");
        std::fs::write(
            &stub,
            format!(
                r#"#!/bin/sh
if [ "$1" = "--version" ]; then
  exit 0
fi
if [ "$1" = "auth" ] && [ "$2" = "status" ]; then
  printf '%s\n' '{payload}'
  exit {status_exit}
fi
echo "unexpected stub invocation: $*" >&2
exit 3
"#,
                payload = auth_status_payload(logged_in),
                status_exit = auth_status_exit_code(logged_in)
            ),
        )
        .expect("write auth status stub");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mut perms = std::fs::metadata(&stub).unwrap().permissions();
            perms.set_mode(0o755);
            std::fs::set_permissions(&stub, perms).unwrap();
        }
    }

    fn make_claude_stub(dir: &std::path::Path) {
        let stub = dir.join("claude");
        std::fs::write(
            &stub,
            r#"#!/bin/sh
if [ "$1" = "--version" ]; then
  exit 0
fi
printf '%s\n' '{"type":"system","subtype":"init","session_id":"stub-session","model":"stub-model","claude_code_version":"stub"}'
printf '%s\n' '{"type":"result","subtype":"success","result":"stub complete"}'
"#,
        )
        .expect("write claude stub");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mut perms = std::fs::metadata(&stub).unwrap().permissions();
            perms.set_mode(0o755);
            std::fs::set_permissions(&stub, perms).unwrap();
        }
    }

    /// Endpoint-empty + ACP-stdio + detectable `claude` (via stub) must take
    /// the real stdio path. ACP-stdio is the discriminator: it upgrades the
    /// adapter's empty-endpoint Simulated request through `stdio_spawn`.
    #[tokio::test]
    async fn gate_real_wire_for_empty_endpoint_stdio_when_claude_stub_on_path() {
        let dir = tempfile::tempdir().expect("tempdir");
        make_claude_stub(dir.path());

        let _guard = env_lock().lock().await;
        let saved_path = std::env::var("PATH").unwrap_or_default();
        let new_path = format!("{}:{}", dir.path().display(), saved_path);
        std::env::set_var("PATH", &new_path);
        std::env::remove_var("ORGASMIC_DRIVER_SIMULATE");

        let driver = AcpStdioDriver::new(Box::new(ClaudeAdapter::new()));
        let mut session = driver
            .acquire(ctx("run-gate-real", RunKind::Worker), simulated_config())
            .await
            .expect("stdio acquire should spawn claude stub");
        std::env::set_var("PATH", &saved_path);

        let event = timeout(Duration::from_secs(5), session.events.recv())
            .await
            .expect("timed out waiting for stub Ready")
            .expect("event stream closed before Ready");
        match event {
            DriverEvent::Ready { capabilities, .. } => assert_eq!(
                capabilities["simulated"],
                serde_json::Value::Bool(false),
                "stdio + available claude must emit simulated:false"
            ),
            other => panic!("expected Ready, got {other:?}"),
        }
        let _ = session.control.release("test cleanup").await;
    }

    /// Endpoint-empty + ACP-WS + detectable `claude` is now an explicit
    /// unsupported-shape error rather than a silent simulation.
    ///
    /// This test used to assert the simulation. It was guarding a pairing the
    /// registry forbids — `("acp-ws", "claude")` is not in `SUPPORTED`, and
    /// `driver_for_mode_harness` returns `None` for it; only this direct
    /// construction reaches it. With an empty endpoint no longer meaning
    /// "simulate" (dec_S18RH), the adapter composes the real local-spawn
    /// request it always meant to, and a WS driver correctly refuses it.
    ///
    /// Refusing is the better answer: the endpoint requirement belongs to the
    /// transport, not the harness, and a silent simulated Ready for an
    /// impossible pairing is how the acp-stdio discard stayed invisible.
    #[tokio::test]
    async fn ws_refuses_the_local_spawn_request_claude_composes_without_an_endpoint() {
        let dir = tempfile::tempdir().expect("tempdir");
        make_claude_stub(dir.path());

        let _guard = env_lock().lock().await;
        let saved_path = std::env::var("PATH").unwrap_or_default();
        let new_path = format!("{}:{}", dir.path().display(), saved_path);
        std::env::set_var("PATH", &new_path);
        std::env::remove_var("ORGASMIC_DRIVER_SIMULATE");

        let driver = AcpWsDriver::new(Box::new(ClaudeAdapter::new()));
        let outcome = driver
            .acquire(ctx("run-gate-ws-sim", RunKind::Worker), simulated_config())
            .await;
        std::env::set_var("PATH", &saved_path);

        match outcome {
            Err(DriverError::Unsupported(what)) => assert!(
                what.contains("acp-ws request shape"),
                "expected a request-shape refusal, got {what:?}"
            ),
            Ok(_) => panic!("ws must not accept a local-spawn request"),
            Err(other) => panic!("expected Unsupported, got {other:?}"),
        }
    }

    /// Endpoint-empty + ACP-stdio + missing `claude` remains simulated because
    /// the stdio upgrade only fires when the spawn command is available.
    #[tokio::test]
    async fn gate_simulates_for_empty_endpoint_stdio_when_claude_missing() {
        let _guard = env_lock().lock().await;
        let saved_path = std::env::var("PATH").unwrap_or_default();
        std::env::set_var("PATH", "");
        std::env::remove_var("ORGASMIC_DRIVER_SIMULATE");

        let driver = AcpStdioDriver::new(Box::new(ClaudeAdapter::new()));
        let mut session = driver
            .acquire(ctx("run-gate-missing", RunKind::Worker), simulated_config())
            .await
            .expect("stdio acquire should fall back to simulated when claude is missing");
        std::env::set_var("PATH", &saved_path);

        let event = session.events.recv().await.expect("simulated Ready");
        match event {
            DriverEvent::Ready { capabilities, .. } => assert_eq!(
                capabilities["simulated"],
                serde_json::Value::Bool(true),
                "stdio + missing claude must remain simulated"
            ),
            other => panic!("expected Ready, got {other:?}"),
        }
    }

    /// ORGASMIC_DRIVER_SIMULATE=1 must force simulated mode even when claude
    /// would be detectable on PATH.
    #[test]
    fn gate_simulate_when_env_var_set() {
        let _guard = env_lock().blocking_lock();
        std::env::set_var("ORGASMIC_DRIVER_SIMULATE", "1");

        let mut adapter = ClaudeAdapter::new();
        let result =
            adapter.compose_request(&ctx("run-gate-sim", RunKind::Worker), &simulated_config());

        std::env::remove_var("ORGASMIC_DRIVER_SIMULATE");

        let request = result.expect("compose_request should succeed");
        match request {
            HarnessRequest::Simulated { events } => {
                let ready = events
                    .into_iter()
                    .find(|e| matches!(e, DriverEvent::Ready { .. }));
                let DriverEvent::Ready { capabilities, .. } =
                    ready.expect("Simulated must emit Ready")
                else {
                    panic!("first event is not Ready");
                };
                assert_eq!(
                    capabilities["simulated"],
                    serde_json::Value::Bool(true),
                    "simulated mode must set capabilities.simulated=true"
                );
            }
            other => panic!("expected Simulated, got {other:?}"),
        }
    }

    /// Real-Claude smoke. Skipped on hosts without `claude` on PATH so CI
    /// without Claude Code still exercises simulated mode only. When
    /// present, use an invalid API key in `--bare` mode so the bridge
    /// verifies stdio JSONL parsing without spending tokens.
    #[tokio::test]
    async fn real_claude_stream_json_bridge_reports_auth_error() {
        if !claude_available() {
            eprintln!(
                "skipping real_claude_stream_json_bridge_reports_auth_error: claude not on PATH"
            );
            return;
        }
        // Hold the env lock to prevent simulated_acquire_emits_ready_and_release
        // from setting ORGASMIC_DRIVER_SIMULATE=1 while this test is running.
        let _guard = env_lock().lock().await;
        std::env::set_var("ORGASMIC_TEST_CLAUDE_ACP_KEY", "invalid");
        let d = ClaudeAcpDriver;
        let cfg = DriverConfig::from_value(json!({
            "endpoint": "stdio",
            "api_key_env": "ORGASMIC_TEST_CLAUDE_ACP_KEY",
            "model": "sonnet"
        }));
        let mut s = d
            .acquire(ctx("run-real-claude", RunKind::Worker), cfg)
            .await
            .unwrap();

        let mut saw_ready = false;
        let mut saw_failure = false;
        for _ in 0..10 {
            let ev = timeout(Duration::from_secs(15), s.events.recv())
                .await
                .expect("real claude smoke timed out")
                .expect("event stream closed before auth failure");
            match ev {
                DriverEvent::Ready { capabilities, .. } => {
                    saw_ready = capabilities["simulated"] == false;
                }
                DriverEvent::RunFail { error_code, .. } => {
                    saw_failure = error_code.starts_with("claude_api_error_")
                        || error_code == "claude_result_error";
                }
                DriverEvent::DriverError { fatal, message } => {
                    saw_failure = saw_failure || (fatal && message.contains("authentication"));
                }
                _ => {}
            }
            if saw_ready && saw_failure {
                break;
            }
        }
        let _ = s.control.release("test cleanup").await;
        assert!(
            saw_ready,
            "real claude bridge should emit non-simulated Ready"
        );
        assert!(
            saw_failure,
            "invalid API key should surface as driver failure"
        );
    }
}
