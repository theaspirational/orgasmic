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
use std::path::Path;

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use tokio::sync::mpsc;

use orgasmic_core::{DriverEvent, TextStream};

use crate::modes::tmux::claude_session_path;
use crate::preflight::{classify_api_key, read_status_output};
use crate::r#trait::{
    BabysitterRequest, DriverConfig, DriverContext, DriverError, HarnessControlOutcome,
    HarnessEventAdapter, HarnessRequest, NativeRuntimeMeta, Preflight, PreflightOutcome, RunKind,
    StdioSpawn, TransitionRequest,
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
///
/// Which mode a run gets is *detected*, never inferred from the presence of an
/// ambient key — see [`resolve_credentials`] for the precedence rule and why.
/// It is detected exactly once per dispatch, before the dispatch owns anything,
/// and carried to the launch in a [`CredentialPlan`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
enum ClaudeCredentialMode {
    /// `--bare` plus an API key (or an `apiKeyHelper`) for the child.
    BareApiKey,
    /// No `--bare`; the harness reads its own login (keychain/OAuth) exactly as
    /// it does for an interactive operator. Isolation is rebuilt from
    /// `--safe-mode --strict-mcp-config`.
    ///
    /// Measured 2026-07-28 against claude 2.1.220, on the argv this adapter
    /// actually composes, by reading the `system:init` event and the debug log
    /// of a run whose model was invalid (a 404 costs `total_cost_usd: 0`, so
    /// every surface below was measured without submitting a turn):
    ///
    /// | surface | `--strict-mcp-config` alone | `+ --safe-mode` |
    /// |---|---|---|
    /// | SessionStart hooks | ran (`hook_started` event) | none (`Found 0 total hooks in registry`) |
    /// | MCP servers | 0 (9 without the flag) | 0 |
    /// | plugins / plugin hooks | 15 loaded, 6 enabled | `Skipping plugin hooks - safe mode disables plugins` |
    /// | skills | 36 | 16 bundled only |
    /// | custom agents | 7 | 4 |
    /// | LSP tool | present | absent |
    /// | `permissionMode` | `auto` | `auto` (preserved) |
    /// | session persistence | pinned `--session-id` written | pinned `--session-id` written |
    ///
    /// `--setting-sources ''` suppresses hooks and MCP too, and was rejected on
    /// the same measurement: it drops `permissionMode` to `default`, which
    /// would stall every dispatched worker on a permission prompt.
    ///
    /// Three residuals are accepted rather than papered over, all measured:
    /// - **Background startup prefetches still run.** `Starting background
    ///   startup prefetches` appears in the debug log under `--safe-mode` too;
    ///   only `--bare` skips them, and no narrower flag exists.
    /// - **Policy-managed hooks still run.** The harness says so itself:
    ///   `safe mode disables plugins (managed settings-file hooks still run)`.
    ///   Operator-authored hooks — the ones this gap was about — do not.
    /// - **`CLAUDE.md` auto-discovery and auto-memory are claimed by
    ///   `--safe-mode`'s own help text but are not observable at $0**: both are
    ///   system-prompt content, which neither the stream-json output nor the
    ///   debug log carries. Establishing them would take a billed turn, which
    ///   TASK-S0QRM's non-goals forbid.
    ///
    /// One deliberate divergence from `--bare`: `--bare` keeps skills resolvable
    /// via `/skill-name`, `--safe-mode` does not. A dispatched worker is given a
    /// compiled prompt and reads files, so this is isolation working as
    /// intended, not a capability the worker was using.
    ///
    /// A minimal `--settings '{}'` was tried and dropped: the flag does accept
    /// inline JSON, but an empty object overrides nothing, so passing it only
    /// implied an isolation it never delivered.
    NativeLogin,
}

impl ClaudeCredentialMode {
    fn as_str(self) -> &'static str {
        match self {
            Self::BareApiKey => "bare_api_key",
            Self::NativeLogin => "native_login",
        }
    }

    /// Parse an operator-supplied override. `auto` (and an empty value) means
    /// "detect", which is why this returns `Option` rather than a mode.
    fn parse_override(raw: &str) -> Result<Option<Self>, DriverError> {
        match raw.trim() {
            "" | "auto" => Ok(None),
            "bare_api_key" | "bare" => Ok(Some(Self::BareApiKey)),
            "native_login" | "native" => Ok(Some(Self::NativeLogin)),
            other => Err(DriverError::InvalidConfig(format!(
                "credential_mode '{other}' is not one of auto, bare_api_key, native_login"
            ))),
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
    /// Reasoning effort, forwarded as `claude --effort <level>`.
    ///
    /// Deliberately NOT `alias = "effort"`. The daemon used to write the value
    /// under BOTH keys (`api.rs`: `"effort"` and `"reasoning_effort"`), so an
    /// alias made serde see one field twice and the whole dispatch failed with
    /// "driver configuration is invalid" — a 400 that named nothing useful.
    /// Hermes carried that alias and reproduced it exactly; TASK-4YC8E dropped
    /// the alias there and collapsed the write to one key. The alias stays out
    /// of every adapter, and `lib.rs` has a registry-wide test that replays the
    /// dual-key config against all supported pairs.
    // orgasmic:TASK-4YC8E
    #[serde(default)]
    reasoning_effort: Option<String>,
    #[serde(default)]
    api_key_env: Option<String>,
    /// Per-dispatch credential-mode override: `auto` (default), `bare_api_key`
    /// or `native_login`.
    ///
    /// The operator's escape hatch for the day detection is wrong — a keychain
    /// login the harness reports but cannot use, or a key that must win over a
    /// login. Detection without an override would only move the guess, and a
    /// wrong guess is discovered after lease, session and dispatch ownership
    /// have been committed (TASK-S0QRM).
    #[serde(default)]
    credential_mode: Option<String>,
    /// The credential decision this dispatch was *admitted* on.
    ///
    /// Not an operator field. The daemon writes it between the preflight and
    /// the acquire (see [`crate::PreflightOutcome::pin_into`]); everything below
    /// that point reads it instead of asking `claude auth status` again. When it
    /// is present, detection does not run at all — that is the whole point, and
    /// the reason a dispatch cannot launch with a credential its preflight never
    /// judged (TASK-KKBTP).
    ///
    /// An operator *can* put one here, and it would be honoured. That is no new
    /// authority: `credential_mode` above already lets them choose the mode
    /// outright, more legibly, and with `validate_config` checking the value.
    #[serde(default)]
    credential_plan: Option<CredentialPlan>,
    #[serde(default)]
    prompt_bundle_text: Option<String>,
}

/// The immutable credential decision one dispatch launches with.
///
/// Resolved once, before the dispatch owns a lease, a session or a worktree, and
/// then carried verbatim into acquire and composition. Every field is a
/// *decision* a launch can apply without asking the harness anything, and every
/// field is non-secret: this plan reaches the persisted `RunMeta`.
///
/// The key itself is deliberately absent. `api_key_env` names the variable the
/// child's key comes from, so the launch re-reads the value from this process's
/// own environment — a read that cannot disagree with the probe's, because it is
/// the same environment in the same process. What could disagree, and did, is
/// the subprocess observation; that is what this pins.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct CredentialPlan {
    mode: ClaudeCredentialMode,
    /// Name of the environment variable holding the key the child presents.
    /// `None` in native-login mode and in a helper-backed bare run.
    #[serde(default)]
    api_key_env: Option<String>,
    /// Inline `--settings` JSON declaring an `apiKeyHelper` to `--bare`.
    #[serde(default)]
    settings_json: Option<String>,
    /// Whether the child's inherited `ANTHROPIC_API_KEY` must be blanked.
    #[serde(default)]
    neutralize_ambient_key: bool,
    /// What detection saw. Recorded rather than consulted: a reader of a failed
    /// run needs to know whether the mode was chosen on evidence or on the
    /// absence of it.
    #[serde(default)]
    native_login: NativeLoginEvidence,
}

/// Levels `claude --effort` documents (2.1.220: "low, medium, high, xhigh, max").
///
/// Used to warn, not to reject. An unknown level is still forwarded, matching
/// how an off-list `--model` passes through: the harness owns its own
/// vocabulary, and hardcoding a closed list here would block the day claude
/// adds a level. Genuinely invalid values fail at launch, which the dispatch
/// startup gate now surfaces without leaving an orphan.
const KNOWN_EFFORT_LEVELS: &[&str] = &["low", "medium", "high", "xhigh", "max"];

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
        // Reject an unknown override here, where it reaches the operator as a
        // 400 naming the value, rather than at compose time where it would
        // surface after the dispatch has taken ownership.
        if let Some(raw) = cfg.credential_mode.as_deref() {
            ClaudeCredentialMode::parse_override(raw)?;
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
    /// The probe observes what credentials exist, resolves the mode through the
    /// same [`resolve_credentials`] the launch uses, and then rules on the
    /// credential *that mode* consumes — the distinction the trait doc
    /// explains, and the reason this costs nothing. Measured on claude 2.1.220:
    /// 0.28 s and $0.
    ///
    /// The observation now comes *before* the choice. It used to run only after
    /// the resolver had already picked native mode, which is why an ambient key
    /// could select a tier nothing had checked (TASK-S0QRM).
    ///
    /// **This is the only place a dispatch asks `claude auth status`.** The
    /// resolved plan rides out in [`PreflightOutcome::plan`] and the launch
    /// applies it; asking twice is asking two different questions, because the
    /// second answer can differ from the first (TASK-KKBTP).
    ///
    /// What it cannot prove, stated plainly so nobody reads `Ready` as a
    /// guarantee: a login that exists can still be expired or rate-limited
    /// server-side, and an API key that is present can still be rejected.
    /// Establishing either requires submitting a real turn, which was measured
    /// at $0.0994 per dispatch and rejected on that ground.
    async fn preflight(&mut self, _ctx: &DriverContext, config: &DriverConfig) -> PreflightOutcome {
        let Ok(cfg) = serde_json::from_value::<ClaudeAcpConfig>(config.0.clone()) else {
            return Preflight::Unsupported.into();
        };
        if simulate_override() {
            // Nothing will present a credential, so there is nothing to rule on
            // and nothing that could fail at startup.
            return Preflight::Unsupported.into();
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
        // Ask the same binary the launch will spawn, before the mode is chosen
        // rather than after: the status answer is now an *input* to the choice,
        // not a check on a choice already made (TASK-S0QRM).
        let command = self
            .stdio_spawn()
            .map(|spawn| spawn.command)
            .unwrap_or_else(|| "claude".to_string());
        let probe = ClaudeAuthProbe::observe(&command).await;
        // A misconfigured `api_key_env` is already `validate`'s rejection and
        // reaches the operator as a config error, not a readiness one.
        let Ok(plan) = resolve_credentials(&cfg, &probe) else {
            return Preflight::Unsupported.into();
        };
        let Ok(resolved) = plan.apply() else {
            return Preflight::Unsupported.into();
        };
        let verdict = match plan.mode {
            ClaudeCredentialMode::BareApiKey => {
                if resolved.api_key.is_none() && resolved.settings_json.is_some() {
                    // A helper is a command this probe deliberately does not
                    // run: it may mint a token, hit the network or bill.
                    Preflight::Unsupported
                } else {
                    classify_bare_api_key(resolved.api_key.as_deref())
                }
            }
            ClaudeCredentialMode::NativeLogin => classify_native_login_evidence(probe.native_login),
        };
        tracing::debug!(
            credential_mode = plan.mode.as_str(),
            native_login = ?plan.native_login,
            rejects = verdict.rejects_dispatch().is_some(),
            "claude preflight: resolved verdict"
        );
        // Pin it. Whatever this verdict was reached on is what the launch gets,
        // even if a second `auth status` would now answer differently.
        match serde_json::to_value(&plan) {
            Ok(plan) => PreflightOutcome::verdict(verdict).with_plan(json!({
                "credential_plan": plan,
            })),
            Err(_) => PreflightOutcome::verdict(verdict),
        }
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

        // Apply the credential plan; never re-detect. The mode decides which
        // isolation flags are even available (see `ClaudeCredentialMode`), and
        // it was decided before this dispatch owned anything.
        //
        // The fallback is for the paths that have no preflight to pin one —
        // attach, recovery, a driver used directly — and it deliberately asks
        // the harness *nothing*: an ambient-only probe reads this process's own
        // environment and the operator's settings file, both of which answer the
        // same way every time. `compose_request` is synchronous and is called
        // from inside async `acquire`, so a subprocess here is a blocking call
        // on a Tokio worker thread with no bound (TASK-Z3093, TASK-KKBTP).
        let plan = match cfg.credential_plan.clone() {
            Some(plan) => plan,
            None => resolve_credentials(&cfg, &ClaudeAuthProbe::ambient_only())?,
        };
        let resolved = plan.apply()?;
        let (mode, env) = (plan.mode, resolved.env);

        let mut args = Vec::with_capacity(spawn.args.len() + 10);
        match mode {
            ClaudeCredentialMode::BareApiKey => {
                args.push("--bare".to_string());
                if let Some(settings) = resolved.settings_json.as_deref() {
                    // The only credential channel bare mode has left when no
                    // key is in the environment.
                    args.push("--settings".to_string());
                    args.push(settings.to_string());
                }
            }
            ClaudeCredentialMode::NativeLogin => {
                // Rebuild what `--bare` would have given us, minus its
                // credential policy, from the two flags measured to do it
                // (see `ClaudeCredentialMode::NativeLogin`). `--safe-mode`
                // suppresses hooks, plugins, LSP and CLAUDE.md while leaving
                // auth and `permissionMode` alone; `--strict-mcp-config` yields
                // `mcp_servers: []`. Both, because safe mode's MCP suppression
                // is its own claim and this one is measured on this argv.
                args.push("--safe-mode".to_string());
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

        if let Some(effort) = cfg.reasoning_effort.as_deref() {
            let effort = effort.trim();
            if !effort.is_empty() {
                if !KNOWN_EFFORT_LEVELS.contains(&effort) {
                    tracing::warn!(
                        effort,
                        known = ?KNOWN_EFFORT_LEVELS,
                        "claude: unrecognised effort level; forwarding it anyway"
                    );
                }
                args.push("--effort".into());
                args.push(effort.to_string());
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
            credential_mode: Some(mode.as_str().to_string()),
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

/// Is there a `claude` to spawn?
///
/// A `PATH` walk, deliberately, not `claude --version`. This runs inside
/// synchronous `compose_request`, which `WorkerDriver::acquire` awaits on a
/// Tokio worker thread: a `Command::status()` there blocks that thread for as
/// long as the harness takes to answer — measured at 0.15 s for two calls on
/// claude 2.1.220, and unbounded if the binary ever wedges, because there is no
/// timeout to bound it with in a sync function (TASK-KKBTP; the same lesson as
/// TASK-Z3093). Reading the directory entries costs microseconds and cannot
/// hang on the harness.
///
/// It answers a slightly weaker question — "a `claude` exists and is
/// executable" rather than "a `claude` ran successfully" — and that is the
/// question this gate actually asks. A binary that exists but cannot run fails
/// at spawn, where the error names itself, instead of being silently downgraded
/// to a simulated run.
fn claude_available() -> bool {
    executable_on_path("claude")
}

/// Resolve a command on `PATH` without spawning anything.
pub(crate) fn executable_on_path(command: &str) -> bool {
    if command.contains(std::path::MAIN_SEPARATOR) {
        return is_executable_file(Path::new(command));
    }
    let Some(path) = std::env::var_os("PATH") else {
        return false;
    };
    std::env::split_paths(&path).any(|dir| is_executable_file(&dir.join(command)))
}

fn is_executable_file(candidate: &Path) -> bool {
    let Ok(meta) = std::fs::metadata(candidate) else {
        return false;
    };
    if !meta.is_file() {
        return false;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        meta.permissions().mode() & 0o111 != 0
    }
    #[cfg(not(unix))]
    {
        true
    }
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
///
/// Derived from a [`CredentialPlan`] by [`CredentialPlan::apply`], never
/// resolved independently: this is the plan with its secrets filled in, which is
/// why it carries no mode of its own — the mode is the plan's.
struct ResolvedCredentials {
    /// Environment overrides for the child, beyond what it inherits.
    env: BTreeMap<String, String>,
    /// The API key the child will present, whether it came from the configured
    /// `api_key_env` or from an inherited `ANTHROPIC_API_KEY`. `None` in
    /// native-login mode, where the harness reads its own keychain, and in a
    /// bare run backed by an `apiKeyHelper`, where the harness runs a command
    /// for the key instead of being handed one.
    ///
    /// Never log or surface this: it is a secret, and preflight reasons reach
    /// durable task evidence.
    api_key: Option<String>,
    /// Inline `--settings` JSON the launch must pass, used only to hand an
    /// `apiKeyHelper` to `--bare`. `--bare` reads no settings file of its own
    /// ("strictly ANTHROPIC_API_KEY or apiKeyHelper *via --settings*", claude
    /// 2.1.220 `--help`), so a helper-backed operator needs the declaration
    /// passed explicitly or bare mode has no credential at all.
    ///
    /// Deliberately not the operator's whole settings file: that would drag
    /// their hooks and MCP config into the one mode that suppresses them.
    settings_json: Option<String>,
}

/// What the harness says about the login only *it* can see.
///
/// Three states, not two, because "we could not ask" must never be read as
/// "no". The probe is the same command an operator would run.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
enum NativeLoginEvidence {
    /// `claude auth status` answered `loggedIn: true`.
    Present,
    /// It answered `loggedIn: false`.
    Absent,
    /// It could not be asked, or answered in a shape this adapter does not
    /// recognise (older harness, error banner, timeout).
    #[default]
    Unknown,
}

/// Everything outside the driver config that the credential decision consults.
///
/// A struct rather than three `std::env`/subprocess reads inside the resolver
/// so the rule itself is pure and can be tested by injection: the regression
/// this task exists for — a stale key next to a working subscription login —
/// is otherwise only reproducible on a machine that has both.
#[derive(Debug, Clone, Default)]
struct ClaudeAuthProbe {
    native_login: NativeLoginEvidence,
    /// The `apiKeyHelper` command declared in the operator's claude settings,
    /// which is the second credential `--bare` accepts.
    api_key_helper: Option<String>,
    /// `ANTHROPIC_API_KEY` inherited from this process's environment, kept only
    /// when it is non-empty. An empty key is not a credential: measured against
    /// claude 2.1.220, `ANTHROPIC_API_KEY=""` reports `apiKeySource: "none"`,
    /// exactly as an unset one does.
    ambient_api_key: Option<String>,
}

impl ClaudeAuthProbe {
    /// Read the environment and the operator's settings. Shared by both
    /// observation paths so they can only differ in how they run `claude`.
    fn ambient(api_key_helper: Option<String>) -> Self {
        Self {
            native_login: NativeLoginEvidence::Unknown,
            api_key_helper,
            ambient_api_key: std::env::var("ANTHROPIC_API_KEY")
                .ok()
                .filter(|key| !key.trim().is_empty()),
        }
    }

    /// The async path, for the preflight.
    async fn observe(command: &str) -> Self {
        let evidence = match read_status_output(command, &["auth", "status"]).await {
            Some(status) => native_login_evidence(&status.stdout),
            None => NativeLoginEvidence::Unknown,
        };
        Self {
            native_login: evidence,
            ..Self::ambient(api_key_helper_from_settings_file())
        }
    }

    /// Everything that can be read without asking the harness anything.
    ///
    /// The fallback for composition paths that carry no pinned plan (attach,
    /// recovery, a driver driven directly). It leaves `native_login` at
    /// `Unknown` on purpose: the only way to improve on that is to spawn
    /// `claude auth status`, and the caller is a synchronous method running on
    /// an async worker thread, where spawning is exactly the hazard this task
    /// removed. There used to be a blocking sibling of [`Self::observe`] here;
    /// it polled with `std::thread::sleep` and ran twice per acp-stdio dispatch,
    /// after preflight had already answered the same question (TASK-KKBTP).
    fn ambient_only() -> Self {
        Self::ambient(api_key_helper_from_settings_file())
    }
}

/// Read the one field of `claude auth status` this adapter is entitled to.
///
/// Deliberately not `authMethod`: it distinguishes `claude.ai` from `apiKey`
/// from `none`, but the question here is only whether a login the harness can
/// use exists. Reading fewer fields is also what keeps the payload's email,
/// org and plan out of anything durable.
fn native_login_evidence(stdout: &str) -> NativeLoginEvidence {
    let Ok(status) = serde_json::from_str::<Value>(stdout) else {
        return NativeLoginEvidence::Unknown;
    };
    match status.get("loggedIn").and_then(Value::as_bool) {
        Some(true) => NativeLoginEvidence::Present,
        Some(false) => NativeLoginEvidence::Absent,
        None => NativeLoginEvidence::Unknown,
    }
}

/// The operator's claude settings file, honouring `CLAUDE_CONFIG_DIR`.
fn claude_settings_path() -> Option<std::path::PathBuf> {
    let dir = std::env::var("CLAUDE_CONFIG_DIR")
        .ok()
        .filter(|d| !d.trim().is_empty())
        .map(std::path::PathBuf::from)
        .or_else(|| {
            std::env::var("HOME")
                .ok()
                .map(|h| Path::new(&h).join(".claude"))
        })?;
    Some(dir.join("settings.json"))
}

/// Pull a declared `apiKeyHelper` command out of a claude settings document.
///
/// Split from the file read so the detection is testable without writing to
/// the operator's real `~/.claude` (`.orgasmic/gotchas.org` on process-global
/// state shared by every test in a binary).
fn api_key_helper_from_settings(text: &str) -> Option<String> {
    let settings: Value = serde_json::from_str(text).ok()?;
    settings
        .get("apiKeyHelper")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|helper| !helper.is_empty())
        .map(str::to_string)
}

fn api_key_helper_from_settings_file() -> Option<String> {
    let path = claude_settings_path()?;
    let text = std::fs::read_to_string(path).ok()?;
    api_key_helper_from_settings(&text)
}

/// Decide how a launched `claude` would authenticate.
///
/// Pure, and shared by the probe and the launch so the two cannot drift. That
/// is the whole guarantee behind the preflight: a check that resolved the
/// credential mode by its own reasoning could confidently interrogate a
/// credential the worker was never going to use — which is precisely how
/// `claude auth status` produced a passing answer for a run that could not
/// start (see [`crate::WorkerDriver::preflight`]).
///
/// # Precedence (TASK-S0QRM)
///
/// 1. An explicit `credential_mode` wins outright. It is the operator saying
///    what they want; detection exists to serve them, not to overrule them.
/// 2. A configured `api_key_env` selects bare mode. Naming a variable in the
///    driver config *is* an explicit choice of key, and it is the one shape
///    that distinguishes a preferred key from a forgotten one.
/// 3. Otherwise a detected native login wins over any ambient key. This is the
///    reversal this task exists for: the old rule gave the light path to
///    whatever `ANTHROPIC_API_KEY` happened to be exported, so a stale key beat
///    a working subscription login and the run died after lease, session and
///    dispatch ownership had been committed. A subscription login is evidence;
///    an inherited variable is a leftover.
/// 4. With the harness answering `loggedIn: false` — a definitive "no" — an
///    ambient key or an `apiKeyHelper` selects bare mode. Those are the only
///    credentials anyone can point at, so refusing to use them would break
///    key-only operators to protect a login the harness says does not exist.
/// 5. With detection *inconclusive*, only a credential the operator **declared**
///    selects bare mode: `credential_mode`, `api_key_env`, or an `apiKeyHelper`
///    written into their settings file. An unchosen ambient `ANTHROPIC_API_KEY`
///    does not. See the `Unknown` policy below.
/// 6. With no login and no declared credential, native mode is chosen so the
///    preflight can reject with the actionable "run /login" reason instead of
///    the narrower "your key is empty".
///
/// Native mode additionally *neutralises* an inherited key: the child would
/// otherwise authenticate with the very credential detection rejected.
///
/// # The `Unknown` policy (dec_7P79C, TASK-KKBTP)
///
/// `Unknown` used to be folded into `Absent`, so an unchosen ambient key won
/// whenever the probe could not reach a verdict. It no longer does, and the
/// reason is not symmetry — it is which operator each choice breaks and how
/// often `Unknown` actually happens to them.
///
/// `Unknown` means `claude auth status` could not be spawned, timed out inside
/// [`crate::preflight::STATUS_TIMEOUT`], or answered in a shape this adapter
/// does not recognise. A key-only operator with no login gets a definitive
/// `loggedIn: false` from a near-instant local read, so rule 4 still serves them
/// and rule 5 never applies. The operator for whom the status command has real
/// work to do — a keychain to unlock, a credential to read — is the operator who
/// *has* a login. Inconclusiveness therefore correlates with a login existing,
/// not with its absence, and promoting a leftover environment variable on that
/// evidence is promoting it against the odds.
///
/// The cost of being wrong is the same size in both directions — a dispatch that
/// dies at startup after taking ownership — so the tie is broken on likelihood,
/// and on the same principle rule 3 rests on: a login is evidence, an inherited
/// variable is a leftover. Nothing here removes an operator's ability to choose
/// the key. `credential_mode: bare_api_key`, `api_key_env`, and a declared
/// `apiKeyHelper` all still select bare mode under `Unknown`, deterministically,
/// which is what makes an unchosen variable the only thing that loses.
///
/// The residual this does not fix: `Unknown` still selects a different tier than
/// `Present` would. What it can no longer do is select a different tier *within
/// one dispatch* — the plan is resolved once, before ownership, and applied
/// verbatim thereafter.
// orgasmic:dec_7P79C, task_KKBTP
fn resolve_credentials(
    cfg: &ClaudeAcpConfig,
    probe: &ClaudeAuthProbe,
) -> Result<CredentialPlan, DriverError> {
    let forced = match cfg.credential_mode.as_deref() {
        Some(raw) => ClaudeCredentialMode::parse_override(raw)?,
        None => None,
    };

    // Resolved before the mode is chosen: naming a variable that does not exist
    // is a configuration error whichever mode wins, exactly as before.
    let configured_key = match cfg.api_key_env.as_deref() {
        Some(env_name) => Some(std::env::var(env_name).map_err(|_| {
            DriverError::InvalidConfig(format!(
                "api_key_env '{env_name}' not set but endpoint is configured"
            ))
        })?),
        None => None,
    };

    let mode = match forced {
        Some(mode) => mode,
        None if configured_key.is_some() => ClaudeCredentialMode::BareApiKey,
        None => match probe.native_login {
            NativeLoginEvidence::Present => ClaudeCredentialMode::NativeLogin,
            NativeLoginEvidence::Absent => {
                if probe.ambient_api_key.is_some() || probe.api_key_helper.is_some() {
                    ClaudeCredentialMode::BareApiKey
                } else {
                    ClaudeCredentialMode::NativeLogin
                }
            }
            // Inconclusive: a declared helper still counts, an inherited
            // variable does not.
            NativeLoginEvidence::Unknown => {
                if probe.api_key_helper.is_some() {
                    ClaudeCredentialMode::BareApiKey
                } else {
                    ClaudeCredentialMode::NativeLogin
                }
            }
        },
    };

    let (api_key_env, settings_json) = match mode {
        ClaudeCredentialMode::BareApiKey => {
            // The *name*, never the value: this plan reaches durable run
            // metadata. `apply` reads the variable back out of this process's
            // own environment, which cannot answer differently than it did here.
            let key_env = if configured_key.is_some() {
                cfg.api_key_env.clone()
            } else if probe.ambient_api_key.is_some() {
                Some(AMBIENT_KEY_ENV.to_string())
            } else {
                None
            };
            let helper = if key_env.is_none() {
                probe
                    .api_key_helper
                    .as_ref()
                    .map(|helper| json!({ "apiKeyHelper": helper }).to_string())
            } else {
                None
            };
            (key_env, helper)
        }
        ClaudeCredentialMode::NativeLogin => (None, None),
    };

    Ok(CredentialPlan {
        mode,
        api_key_env,
        settings_json,
        neutralize_ambient_key: mode == ClaudeCredentialMode::NativeLogin
            && probe.ambient_api_key.is_some(),
        native_login: probe.native_login,
    })
}

/// The environment variable `claude` reads its API key from.
const AMBIENT_KEY_ENV: &str = "ANTHROPIC_API_KEY";

impl CredentialPlan {
    /// Turn the pinned decision into the child's launch environment.
    ///
    /// The only step that touches a secret, and it is the last one: the key is
    /// read here, held for the length of one composition, and never written into
    /// the plan or anything the plan reaches.
    fn apply(&self) -> Result<ResolvedCredentials, DriverError> {
        let mut env = BTreeMap::new();
        let api_key = match self.api_key_env.as_deref() {
            Some(env_name) => {
                let key = std::env::var(env_name).map_err(|_| {
                    DriverError::InvalidConfig(format!(
                        "api_key_env '{env_name}' not set but endpoint is configured"
                    ))
                })?;
                // Set explicitly rather than left to inheritance so a configured
                // `api_key_env` reaches the child under the name claude reads.
                env.insert(AMBIENT_KEY_ENV.to_string(), key.clone());
                Some(key)
            }
            None => None,
        };
        if self.neutralize_ambient_key {
            // Blank, not absent: `HarnessRequest::Subprocess` can add child
            // environment but not remove inherited entries, and measured
            // against claude 2.1.220 an empty `ANTHROPIC_API_KEY` reports
            // `apiKeySource: "none"` while a non-empty one reports
            // `ANTHROPIC_API_KEY`. Without this, choosing native mode next to a
            // stale exported key would still authenticate with the stale key.
            env.insert(AMBIENT_KEY_ENV.to_string(), String::new());
        }
        Ok(ResolvedCredentials {
            env,
            api_key,
            settings_json: self.settings_json.clone(),
        })
    }
}

/// Turn detected login evidence into a verdict for a native-login worker.
///
/// Separated from the subprocess so the classification is testable without
/// putting a stub on `PATH`; process-global `PATH` mutation is shared by every
/// test in the binary (`.orgasmic/gotchas.org`).
///
/// Claude is the one harness of the three that answers in JSON, so
/// [`native_login_evidence`] reads a boolean field instead of matching a
/// sentence — a contract far less likely to shift under a version bump than the
/// prose the others emit. The exit status is deliberately not consulted:
/// measured 2026-07-25, `claude auth status` exits 1 precisely when it is
/// logged out, so the non-zero exit accompanies the answer rather than
/// replacing it.
///
/// The reason is a constant. The payload this evidence came from carries the
/// operator's email, org and subscription tier, and a preflight reason reaches
/// durable, committable task evidence.
fn classify_native_login_evidence(evidence: NativeLoginEvidence) -> Preflight {
    match evidence {
        NativeLoginEvidence::Present => Preflight::Ready,
        NativeLoginEvidence::Absent => Preflight::fatal(
            "claude is not logged in. This worker authenticates through the harness's own \
             login (no usable ANTHROPIC_API_KEY or apiKeyHelper was found), so it cannot \
             start until you run `claude` and complete /login on this machine.",
        ),
        NativeLoginEvidence::Unknown => Preflight::Unsupported,
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
    use crate::modes::acp_stdio::AcpStdioComposeAdapter;
    use crate::modes::rmux::test_tooling::{skip_test_if_missing, test_environment_lock};
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
        let (args, native, _) = composed_args_with_ctx(cfg);
        (args, native)
    }

    /// As `composed_args`, but also hands back the context the request was
    /// composed for.
    ///
    /// Without the context a test can only assert that *some* value was pinned
    /// after `--session-id` and that NativeRuntime repeats it — a claim the
    /// argv proves against itself. The property that matters is narrower: the
    /// launched session id is the run's `runtime_id`, which is what makes the
    /// vendor transcript path computable rather than discovered.
    fn composed_args_with_ctx(
        cfg: Value,
    ) -> (Vec<String>, Option<NativeRuntimeMeta>, DriverContext) {
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
        (args, adapter.native_runtime(), ctx)
    }

    /// A config on the real (non-simulated) path, with no api_key_env so
    /// credential resolution picks the operator's own login.
    ///
    /// The endpoint here is incidental now. It used to be load-bearing — an
    /// empty one meant "simulate" — which is exactly why every test in this
    /// file set one and none of them exercised the shape production actually
    /// sends (dec_S18RH). See
    /// `production_shape_with_no_endpoint_composes_a_real_run`.
    fn subprocess_config() -> Value {
        json!({"endpoint": "stdio://claude"})
    }

    /// The config shape a real dispatch sends: no endpoint at all.
    ///
    /// This is the one that mattered and the one nothing covered. Every
    /// dispatch leaves `endpoint` empty, so this shape returning `Simulated`
    /// is what made TASK-VB9DQ's argv unreachable in production while its
    /// tests passed.
    #[tokio::test]
    async fn production_shape_with_no_endpoint_composes_a_real_run() {
        let _guard = env_lock().lock().await;
        std::env::remove_var("ORGASMIC_DRIVER_SIMULATE");
        std::env::remove_var("ANTHROPIC_API_KEY");
        if skip_test_if_missing(
            "production_shape_with_no_endpoint_composes_a_real_run",
            &[("claude", claude_available())],
        ) {
            return;
        }

        // Exactly what the daemon sends: harness set, endpoint empty.
        let (args, native) = composed_args(json!({"endpoint": ""}));

        assert!(
            args.windows(2).any(|w| w[0] == "--session-id"),
            "an endpoint-less run is a real run and must pin a session id: {args:?}"
        );
        assert!(
            native.is_some(),
            "an endpoint-less run must still report NativeRuntime metadata"
        );
    }

    /// `--effort` is forwarded, using the exact driver_config shape the
    /// daemon emits — which carries the value under two keys at once.
    #[tokio::test]
    async fn effort_reaches_the_argv_from_the_daemons_config_shape() {
        let _guard = env_lock().lock().await;
        std::env::remove_var("ORGASMIC_DRIVER_SIMULATE");
        std::env::remove_var("ANTHROPIC_API_KEY");
        if skip_test_if_missing(
            "effort_reaches_the_argv_from_the_daemons_config_shape",
            &[("claude", claude_available())],
        ) {
            return;
        }

        // The daemon writes the value under both keys; the config must accept
        // that exact shape rather than choking on it (see the field comment).
        let (args, _) =
            composed_args(json!({"endpoint": "", "effort": "xhigh", "reasoning_effort": "xhigh"}));
        assert!(
            args.windows(2).any(|w| w == ["--effort", "xhigh"]),
            "effort must reach the argv from the daemon's own config shape: {args:?}"
        );

        // Absent effort adds no flag at all, rather than an empty one.
        let (args, _) = composed_args(json!({"endpoint": ""}));
        assert!(
            !args.iter().any(|a| a == "--effort"),
            "no effort configured must mean no --effort flag: {args:?}"
        );
    }

    #[tokio::test]
    async fn native_login_mode_drops_bare_and_isolates_without_it() {
        let _guard = env_lock().lock().await;
        std::env::remove_var("ORGASMIC_DRIVER_SIMULATE");
        std::env::remove_var("ANTHROPIC_API_KEY");
        if skip_test_if_missing(
            "native_login_mode_drops_bare_and_isolates_without_it",
            &[("claude", claude_available())],
        ) {
            return;
        }
        let (args, native) = composed_args(json!({
            "endpoint": "stdio://claude",
            "credential_mode": "native_login",
        }));

        // `--bare` never reads OAuth or the keychain, so a subscription
        // operator could not authenticate at all while it was hardcoded.
        assert!(
            !args.iter().any(|a| a == "--bare"),
            "native-login mode must not pass --bare: {args:?}"
        );
        // Isolation is rebuilt from the two flags measured to rebuild it:
        // `--safe-mode` (hooks, plugins, LSP, CLAUDE.md) and
        // `--strict-mcp-config` (`mcp_servers: []`).
        assert!(args.iter().any(|a| a == "--safe-mode"), "{args:?}");
        assert!(args.iter().any(|a| a == "--strict-mcp-config"), "{args:?}");
        // Deliberately no `--settings {}`: the flag accepts inline JSON, but an
        // empty object overrides nothing, so passing it would only imply an
        // isolation this mode does not provide. The only `--settings` this
        // adapter ever passes carries an apiKeyHelper, in bare mode.
        assert!(!args.iter().any(|a| a == "--settings"), "{args:?}");
        assert_eq!(
            native.and_then(|native| native.credential_mode).as_deref(),
            Some("native_login"),
            "the resolved mode must ride out to the supervisor for RunMeta"
        );
    }

    /// This test used to assert the opposite — that any non-empty
    /// `ANTHROPIC_API_KEY`, however fake, selects `--bare`. That assertion
    /// pinned the failure TASK-Z8WEJ was filed to remove: a stale exported key
    /// beat a working subscription login, and the run died after lease, session
    /// and dispatch ownership had already been committed. The rule it protected
    /// is gone, so the assertion goes with it (TASK-S0QRM).
    ///
    /// Bare mode itself is not weakened: it is still exactly what an operator
    /// gets when they ask for it, or when no login is detected.
    #[test]
    fn a_stale_ambient_key_no_longer_beats_a_detected_native_login() {
        let probe = ClaudeAuthProbe {
            native_login: NativeLoginEvidence::Present,
            api_key_helper: None,
            ambient_api_key: Some("sk-ant-stale-and-forgotten".into()),
        };
        let plan = resolve_credentials(&ClaudeAcpConfig::default(), &probe).expect("resolve");

        assert_eq!(
            plan.mode,
            ClaudeCredentialMode::NativeLogin,
            "a detected login must beat a key nobody chose"
        );
        // Selecting the mode is not enough: the child inherits this process's
        // environment, so an untouched stale key would still be the credential
        // claude authenticates with (measured: a non-empty ANTHROPIC_API_KEY
        // reports `apiKeySource: ANTHROPIC_API_KEY`, an empty one reports
        // `none`).
        assert!(plan.neutralize_ambient_key);
        let resolved = plan.apply().expect("apply");
        assert_eq!(
            resolved.env.get("ANTHROPIC_API_KEY").map(String::as_str),
            Some(""),
            "native mode must neutralise the inherited key: {:?}",
            resolved.env
        );
        assert!(resolved.api_key.is_none());
    }

    /// The other direction, so the fix is a reversal of precedence rather than
    /// a blanket refusal to use keys — but only on a *definitive* "no login".
    #[test]
    fn a_definitively_logged_out_harness_still_lets_a_key_select_the_light_bare_path() {
        let probe = ClaudeAuthProbe {
            native_login: NativeLoginEvidence::Absent,
            api_key_helper: None,
            ambient_api_key: Some("sk-ant-test-not-real".into()),
        };
        let plan = resolve_credentials(&ClaudeAcpConfig::default(), &probe).expect("resolve");
        assert_eq!(
            plan.mode,
            ClaudeCredentialMode::BareApiKey,
            "with the harness saying it has no login, the key is the only credential there is"
        );
        assert_eq!(plan.api_key_env.as_deref(), Some("ANTHROPIC_API_KEY"));
        assert!(!plan.neutralize_ambient_key);
    }

    /// The `Unknown` policy (TASK-KKBTP ask 4), asserted as the difference it
    /// makes rather than described.
    ///
    /// `Absent` and `Unknown` are no longer the same answer. A harness that says
    /// `loggedIn: false` has ruled; a harness that could not be asked has not,
    /// and "could not be asked" must not promote a variable nobody chose for
    /// this dispatch. The full reasoning — including why inconclusiveness
    /// correlates with *having* a login — is on [`resolve_credentials`].
    #[test]
    fn an_unchosen_ambient_key_does_not_win_on_inconclusive_detection() {
        let inconclusive = ClaudeAuthProbe {
            native_login: NativeLoginEvidence::Unknown,
            api_key_helper: None,
            ambient_api_key: Some("sk-ant-stale-and-forgotten".into()),
        };
        let plan =
            resolve_credentials(&ClaudeAcpConfig::default(), &inconclusive).expect("resolve");
        assert_eq!(
            plan.mode,
            ClaudeCredentialMode::NativeLogin,
            "an unanswerable probe is not evidence that the login is missing"
        );
        assert!(
            plan.neutralize_ambient_key,
            "and the key it declined to use must not authenticate the child anyway"
        );

        // Every way of *choosing* the key still works under the same
        // inconclusive answer, which is what keeps this a rule about unchosen
        // credentials rather than a refusal to use keys.
        let declared_helper = ClaudeAuthProbe {
            api_key_helper: Some("/usr/local/bin/mint-key".into()),
            ..inconclusive.clone()
        };
        assert_eq!(
            resolve_credentials(&ClaudeAcpConfig::default(), &declared_helper)
                .expect("resolve")
                .mode,
            ClaudeCredentialMode::BareApiKey,
            "a helper written into the operator's settings file is a declaration"
        );

        let forced = ClaudeAcpConfig {
            credential_mode: Some("bare_api_key".into()),
            ..ClaudeAcpConfig::default()
        };
        assert_eq!(
            resolve_credentials(&forced, &inconclusive)
                .expect("resolve")
                .mode,
            ClaudeCredentialMode::BareApiKey,
            "--credential-mode is the operator saying which credential they want"
        );
    }

    /// What happens when detection is impossible and there is nothing to fall
    /// back to: native mode, so the preflight rejects with the actionable
    /// "run /login" reason instead of admitting a run that cannot authenticate.
    #[test]
    fn no_login_and_no_key_resolves_to_native_so_the_preflight_can_say_why() {
        for evidence in [NativeLoginEvidence::Absent, NativeLoginEvidence::Unknown] {
            let probe = ClaudeAuthProbe {
                native_login: evidence,
                ..ClaudeAuthProbe::default()
            };
            let plan = resolve_credentials(&ClaudeAcpConfig::default(), &probe).expect("resolve");
            assert_eq!(plan.mode, ClaudeCredentialMode::NativeLogin, "{evidence:?}");
            assert!(!plan.neutralize_ambient_key);
            let resolved = plan.apply().expect("apply");
            assert!(
                resolved.env.is_empty(),
                "nothing to neutralise when nothing was inherited: {:?}",
                resolved.env
            );
        }
        assert!(classify_native_login_evidence(NativeLoginEvidence::Absent)
            .rejects_dispatch()
            .is_some());
    }

    /// An `apiKeyHelper` is the second credential bare mode accepts, and the
    /// one a helper-backed operator had no way to select at all.
    #[test]
    fn an_api_key_helper_is_detected_and_handed_to_bare_explicitly() {
        // Detection is a pure read of the operator's settings document.
        assert_eq!(
            api_key_helper_from_settings(r#"{"apiKeyHelper":"/usr/local/bin/mint-key"}"#)
                .as_deref(),
            Some("/usr/local/bin/mint-key")
        );
        for absent in [
            r#"{"apiKeyHelper":"  "}"#,
            r#"{"model":"opus"}"#,
            "not json",
        ] {
            assert_eq!(api_key_helper_from_settings(absent), None, "{absent}");
        }

        let probe = ClaudeAuthProbe {
            native_login: NativeLoginEvidence::Absent,
            api_key_helper: Some("/usr/local/bin/mint-key".into()),
            ambient_api_key: None,
        };
        let plan = resolve_credentials(&ClaudeAcpConfig::default(), &probe).expect("resolve");
        assert_eq!(plan.mode, ClaudeCredentialMode::BareApiKey);
        // `--bare` reads no settings file of its own, so the declaration has to
        // be passed inline — and only the declaration, not the operator's whole
        // settings document with its hooks and MCP servers.
        let settings = plan
            .settings_json
            .clone()
            .expect("helper must reach --bare");
        assert_eq!(settings, r#"{"apiKeyHelper":"/usr/local/bin/mint-key"}"#);
        assert_eq!(plan.api_key_env, None);
        assert!(plan.apply().expect("apply").api_key.is_none());
    }

    /// The escape hatch, in both directions, at the resolver.
    /// `override_reaches_the_argv_through_the_driver_config` proves the same
    /// thing on the argv a dispatch actually spawns.
    #[test]
    fn an_explicit_override_wins_over_detection_in_both_directions() {
        let logged_in_with_key = ClaudeAuthProbe {
            native_login: NativeLoginEvidence::Present,
            api_key_helper: None,
            ambient_api_key: Some("sk-ant-preferred".into()),
        };
        let forced_bare = ClaudeAcpConfig {
            credential_mode: Some("bare_api_key".into()),
            ..ClaudeAcpConfig::default()
        };
        let plan = resolve_credentials(&forced_bare, &logged_in_with_key).expect("resolve");
        assert_eq!(plan.mode, ClaudeCredentialMode::BareApiKey);
        assert_eq!(plan.api_key_env.as_deref(), Some("ANTHROPIC_API_KEY"));

        let forced_native = ClaudeAcpConfig {
            credential_mode: Some("native_login".into()),
            ..ClaudeAcpConfig::default()
        };
        let no_login_but_a_key = ClaudeAuthProbe {
            native_login: NativeLoginEvidence::Absent,
            api_key_helper: None,
            ambient_api_key: Some("sk-ant-stale".into()),
        };
        let plan = resolve_credentials(&forced_native, &no_login_but_a_key).expect("resolve");
        assert_eq!(plan.mode, ClaudeCredentialMode::NativeLogin);
        assert_eq!(
            plan.apply()
                .expect("apply")
                .env
                .get("ANTHROPIC_API_KEY")
                .map(String::as_str),
            Some("")
        );

        // `auto` and an absent value both mean "detect".
        for auto in [Some("auto"), Some(" "), None] {
            let cfg = ClaudeAcpConfig {
                credential_mode: auto.map(str::to_string),
                ..ClaudeAcpConfig::default()
            };
            assert_eq!(
                resolve_credentials(&cfg, &logged_in_with_key)
                    .expect("resolve")
                    .mode,
                ClaudeCredentialMode::NativeLogin,
                "{auto:?}"
            );
        }
    }

    /// An unknown override is a configuration error the operator sees as a 400
    /// naming their value, not a surprise at compose time after the dispatch
    /// has taken ownership.
    #[test]
    fn an_unknown_credential_mode_is_rejected_by_validate() {
        let err = ClaudeAdapter::new()
            .validate_config(&DriverConfig(json!({"credential_mode": "bare-ish"})))
            .expect_err("an unknown mode must not validate");
        let message = format!("{err:?}");
        assert!(message.contains("bare-ish"), "{message}");
        assert!(message.contains("native_login"), "{message}");
        for accepted in ["auto", "bare_api_key", "native_login"] {
            ClaudeAdapter::new()
                .validate_config(&DriverConfig(json!({"credential_mode": accepted})))
                .unwrap_or_else(|e| panic!("{accepted} must validate: {e:?}"));
        }
    }

    /// The override on the real argv, through the config shape the daemon
    /// sends — the half of the escape hatch a resolver test cannot prove.
    #[tokio::test]
    async fn override_reaches_the_argv_through_the_driver_config() {
        let _guard = env_lock().lock().await;
        std::env::remove_var("ORGASMIC_DRIVER_SIMULATE");
        std::env::remove_var("ANTHROPIC_API_KEY");
        if skip_test_if_missing(
            "override_reaches_the_argv_through_the_driver_config",
            &[("claude", claude_available())],
        ) {
            return;
        }
        std::env::set_var("ANTHROPIC_API_KEY", "sk-ant-test-not-real");
        let (args, native) = composed_args(json!({
            "endpoint": "",
            "credential_mode": "bare_api_key",
        }));
        std::env::remove_var("ANTHROPIC_API_KEY");

        assert!(
            args.iter().any(|a| a == "--bare"),
            "an operator who asks for bare mode must get it: {args:?}"
        );
        assert!(
            !args.iter().any(|a| a == "--safe-mode"),
            "--bare is already the light path; do not double up: {args:?}"
        );
        assert!(
            !args.iter().any(|a| a == "--strict-mcp-config"),
            "--bare already suppresses MCP; do not double up: {args:?}"
        );
        assert_eq!(
            native.and_then(|native| native.credential_mode).as_deref(),
            Some("bare_api_key")
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

    /// The verdict a native-login worker gets from a given `auth status`
    /// payload — the two halves the production preflight now runs separately
    /// (detect, then classify), composed back together so these contract tests
    /// keep reading as one question.
    fn classify_native_login(stdout: &str) -> Preflight {
        classify_native_login_evidence(native_login_evidence(stdout))
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
        if skip_test_if_missing(
            "the_probe_and_the_launch_resolve_the_same_credential",
            &[("claude", claude_available())],
        ) {
            return;
        }
        // A configured `api_key_env` — the operator naming their key — is the
        // one input that selects bare mode without consulting detection, so
        // this stays a joint assertion about one env on any machine, logged in
        // or not. It used to be an empty `ANTHROPIC_API_KEY`, which no longer
        // selects anything: an empty key is not a credential (TASK-S0QRM).
        std::env::set_var("ORGASMIC_TEST_CLAUDE_KEY", "");
        let config = json!({
            "endpoint": "stdio://claude",
            "api_key_env": "ORGASMIC_TEST_CLAUDE_KEY",
        });

        let (args, _) = composed_args(config.clone());
        let verdict = ClaudeAdapter::new()
            .preflight(
                &ctx("run-preflight-agree", RunKind::Worker),
                &DriverConfig(config),
            )
            .await;
        std::env::remove_var("ORGASMIC_TEST_CLAUDE_KEY");

        // The launch commits to `--bare`, whose only credential is the key…
        assert!(
            args.iter().any(|a| a == "--bare"),
            "a configured api_key_env selects the bare path: {args:?}"
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
        assert_eq!(verdict.verdict, Preflight::Unsupported);
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
        assert_eq!(verdict.verdict, Preflight::Ready);
    }

    #[tokio::test]
    async fn every_mode_persists_a_locatable_native_session() {
        let _guard = env_lock().lock().await;
        std::env::remove_var("ORGASMIC_DRIVER_SIMULATE");
        std::env::remove_var("ANTHROPIC_API_KEY");
        if skip_test_if_missing(
            "every_mode_persists_a_locatable_native_session",
            &[("claude", claude_available())],
        ) {
            return;
        }
        let (args, native, ctx) = composed_args_with_ctx(subprocess_config());
        let runtime_id = ctx.identity.runtime_id.as_str();

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

        // Every assertion below compares against `runtime_id`, not against
        // `session_id`. Comparing NativeRuntime to the argv only proves the
        // adapter is self-consistent: swap the source of the pinned value for
        // any other string and all three still agree. What makes the vendor
        // transcript locatable is that the pinned value *is* the run's
        // runtime_id, which is also what every lifecycle event carries.
        assert_eq!(
            session_id, runtime_id,
            "the launched session id must be the run's runtime_id, not merely \
             some stable value: {args:?}"
        );
        let native = native.expect("the adapter must report NativeRuntime metadata");
        assert_eq!(native.provider, "claude");
        assert_eq!(native.session_id.as_deref(), Some(runtime_id));
        assert!(
            native
                .resume_argv
                .windows(2)
                .any(|w| w == ["--resume", runtime_id]),
            "resume argv must target the run's runtime_id: {:?}",
            native.resume_argv
        );
        assert!(
            native
                .launch_argv
                .windows(2)
                .any(|w| w == ["--session-id", runtime_id]),
            "launch argv must record the run's runtime_id: {:?}",
            native.launch_argv
        );
        // `run_id` ends in a 32-hex dispatch attempt token that reads like a
        // session id and is not one — the near-miss most likely to be pinned by
        // accident. If the pin ever drifts onto it, this is what notices.
        assert_ne!(
            session_id, ctx.identity.run_id,
            "the pinned session id must not be the run_id"
        );
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
        test_environment_lock()
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
        warm_up_auth_status_stub(dir, logged_in);
    }

    /// TASK-GEZHQ's `warm_up_stub`, transplanted verbatim in substance: pay the
    /// first-exec cost of a file written a millisecond ago here, where there is
    /// no deadline to blow.
    ///
    /// The preflight gives a harness [`STATUS_TIMEOUT`] to answer and treats
    /// silence as "could not ask", which is not a rejection — so every second
    /// this stub spends being *started* is a second of the test's own premise
    /// draining away. Measured 2026-07-29 under a loaded workspace run: a
    /// freshly written stub's first invocation never reached the first line of
    /// its own script inside the bound, while the identical file exec'd
    /// normally moments later in the same process.
    ///
    /// It transplants without modification here because this stub keeps no
    /// state: it has no ledger to corrupt and no scripted answer to consume, so
    /// asking it the real question one extra time changes nothing any test
    /// reads. [`make_recording_stub`] is the one that could not take this, and
    /// [`warm_up_recording_stub`] says why.
    ///
    /// The answer is asserted rather than discarded, so a stub that cannot
    /// answer at all fails *as a stub*, here, instead of surfacing as a
    /// mystified verdict much further down.
    fn warm_up_auth_status_stub(dir: &std::path::Path, logged_in: bool) {
        let output = std::process::Command::new(dir.join("claude"))
            .args(["auth", "status"])
            .output()
            .expect("the auth status stub must be executable");
        let stdout = String::from_utf8_lossy(&output.stdout);
        let expected = format!("\"loggedIn\":{logged_in}");
        assert!(
            stdout.contains(&expected),
            "the stub must answer {expected} before a preflight is asked to \
             believe it: {stdout:?}"
        );
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
        // The lock is taken *before* the availability probe, not after
        // (TASK-R2HDN). `gate_simulates_for_empty_endpoint_stdio_when_claude_missing`
        // holds this same lock while process-global PATH is `""`; probing first
        // let this test observe that synthetic PATH, skip itself, and leave the
        // suite green with its assertions never run — while the binary sentinel,
        // which does wait for the lock, saw the restored PATH and passed.
        //
        // Holding it also prevents simulated_acquire_emits_ready_and_release from
        // setting ORGASMIC_DRIVER_SIMULATE=1 while this test is running.
        let _guard = env_lock().lock().await;
        if skip_test_if_missing(
            "real_claude_stream_json_bridge_reports_auth_error",
            &[("claude", claude_available())],
        ) {
            return;
        }
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

    // ---- admission-to-launch boundary (TASK-KKBTP) -----------------------

    /// The argv the recording stub answers a warm-up on.
    ///
    /// Nothing in the adapter can mint it — it is not a `claude` subcommand,
    /// not a flag the composer emits, and appears in no production string. That
    /// is the whole basis on which the stub is allowed to keep warm-ups out of
    /// the ledger the one-probe-per-dispatch count is read from.
    const WARM_UP_ARGV: &str = "__orgasmic_warm_up";

    /// What the warm-up arm prints, so a stub that execs but cannot run its own
    /// script fails as a stub rather than as silence.
    const WARM_UP_ACK: &str = "orgasmic-warm-up ok";

    /// A `claude` stub that logs every invocation and can change its answer.
    ///
    /// Two things make it different from [`make_auth_status_stub`], and both
    /// are the point:
    ///
    /// - **Its `auth status` answer changes between calls.** The two tests this
    ///   task's blocker demanded could not exist against a stub that always
    ///   answers the same way — which is exactly why neither
    ///   `the_probe_and_the_launch_resolve_the_same_credential` nor
    ///   `the_resolved_claude_credential_mode_survives_the_mode_layer` could
    ///   fail when the observation moved between admission and launch.
    /// - **It records what it was asked.** "How many times does one dispatch
    ///   ask `claude auth status`?" is a question about production call counts,
    ///   and a count is the only honest way to answer it.
    fn make_recording_stub(dir: &std::path::Path, answers: &[bool]) -> std::path::PathBuf {
        recording_stub(dir, answers, false)
    }

    /// The same stub, made deterministically late on its very first exec.
    ///
    /// This is what the load did, done to the stub — no load replay. Measured
    /// 2026-07-29 (TASK-GEZHQ): under a loaded workspace run the first exec of a
    /// freshly written stub arrived so late that the probe's bound expired while
    /// the child was mid-flight — *after* it had reached its own first lines and
    /// advanced the ledger this test counts, and before it had printed an
    /// answer. So the delay is placed where the child actually died, not at the
    /// top of the script: `read_status_output` kills a timed-out child
    /// (`kill_on_drop`), so a delay before the first line leaves no trace and
    /// TASK-GEZHQ-retry's second attempt survives it completely. A stub that
    /// *remembers* is the one that cannot be saved by asking again — the retry
    /// then reaches a stub whose scripted answers have already moved on, and the
    /// dispatch is admitted on a credential nobody chose.
    ///
    /// Only the first exec is late; the marker is consumed by whoever gets there
    /// first. With the warm-up in place that is the warm-up, unbounded, and the
    /// probe meets a stub that has already paid. Without it, the probe pays.
    fn make_recording_stub_that_starts_late(
        dir: &std::path::Path,
        answers: &[bool],
    ) -> std::path::PathBuf {
        recording_stub(dir, answers, true)
    }

    fn recording_stub(
        dir: &std::path::Path,
        answers: &[bool],
        late_first_exec: bool,
    ) -> std::path::PathBuf {
        let log = dir.join("invocations.log");
        let warmups = dir.join("warmups.log");
        let counter = dir.join("calls.count");
        let late = dir.join("late-first-exec");
        if late_first_exec {
            std::fs::write(&late, "").expect("arm the late first exec");
        }
        let mut arms = String::new();
        for (index, logged_in) in answers.iter().enumerate() {
            arms.push_str(&format!(
                "  if [ \"$n\" = \"{n}\" ]; then printf '%s\\n' '{payload}'; exit {code}; fi\n",
                n = index + 1,
                payload = auth_status_payload(*logged_in),
                code = auth_status_exit_code(*logged_in),
            ));
        }
        // Past the scripted answers the stub hangs, which is what a wedged
        // `auth status` looks like to a caller and what `Unknown` is made of.
        let stub = dir.join("claude");
        std::fs::write(
            &stub,
            format!(
                r#"#!/bin/sh
late=""
if [ -f "{late}" ]; then rm -f "{late}"; late="1"; fi
if [ "$1" = "{warm_up_argv}" ]; then
  if [ -n "$late" ]; then sleep 6; fi
  printf '%s\n' "$*" >> "{warmups}"
  printf '%s\n' '{warm_up_ack}'
  exit 0
fi
printf '%s\n' "$*" >> "{log}"
if [ "$1" = "auth" ] && [ "$2" = "status" ]; then
  n=$(( $(cat "{counter}" 2>/dev/null || echo 0) + 1 ))
  printf '%s' "$n" > "{counter}"
  if [ -n "$late" ]; then sleep 6; fi
{arms}  sleep 60
  exit 3
fi
if [ "$1" = "--version" ]; then
  sleep 60
  exit 0
fi
printf '%s\n' '{{"type":"system","subtype":"init","session_id":"stub-session","model":"stub-model","claude_code_version":"stub"}}'
printf '%s\n' '{{"type":"result","subtype":"success","result":"stub complete"}}'
"#,
                late = late.display(),
                warm_up_argv = WARM_UP_ARGV,
                warm_up_ack = WARM_UP_ACK,
                warmups = warmups.display(),
                log = log.display(),
                counter = counter.display(),
                arms = arms,
            ),
        )
        .expect("write recording stub");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mut perms = std::fs::metadata(&stub).unwrap().permissions();
            perms.set_mode(0o755);
            std::fs::set_permissions(&stub, perms).unwrap();
        }
        warm_up_recording_stub(&stub, &log, &warmups);
        log
    }

    /// The warm-up TASK-GEZHQ's could not be, and the argument that it does not
    /// buy its safety with the property it protects.
    ///
    /// [`warm_up_auth_status_stub`] simply asks the real question one extra
    /// time. This stub cannot be warmed that way, for two reasons that are the
    /// same reason: it remembers. `auth status` appends to the ledger the tests
    /// count ("one auth status per dispatch" is the asserted property) and
    /// advances the scripted-answer index, so an extra real question would
    /// inflate the count and hand the dispatch the *next* stub's answer.
    /// `--version` is worse: that arm sleeps 60 s on purpose, to catch a
    /// composition that spawns the harness.
    ///
    /// So the warm-up is a third argv, [`WARM_UP_ARGV`], answered above the
    /// ledger — and the exemption cannot hide a double probe:
    ///
    /// - It is keyed to an argv **production cannot produce**. `__orgasmic_warm_up`
    ///   is not a `claude` subcommand and appears in no non-test string in this
    ///   crate; every argv the adapter can compose still falls through to the
    ///   `printf … >> log` line untouched.
    /// - Nothing is actually un-recorded. Warm-ups are written to their own
    ///   ledger, and this function asserts, before the test starts, that the
    ///   warm-up landed there and that the counted ledger is still *empty*. A
    ///   warm-up that leaked into the probe ledger fails here rather than
    ///   quietly paying for one of the invocations a test is about to count.
    /// - The scripted-answer counter is untouched by the warm-up arm, so the
    ///   answer the probe gets is the answer the test scripted for it.
    ///
    /// The stub's own answer is asserted for TASK-GEZHQ's rule: a warm-up
    /// failure must fail as a stub failure, here and loudly, not as a downstream
    /// verdict mystery.
    fn warm_up_recording_stub(
        stub: &std::path::Path,
        log: &std::path::Path,
        warmups: &std::path::Path,
    ) {
        let output = std::process::Command::new(stub)
            .arg(WARM_UP_ARGV)
            .output()
            .expect("the recording stub must be executable");
        let stdout = String::from_utf8_lossy(&output.stdout);
        assert!(
            output.status.success() && stdout.contains(WARM_UP_ACK),
            "the recording stub must run its own script before a preflight is \
             asked to believe it: status {:?}, stdout {stdout:?}",
            output.status.code()
        );
        assert_eq!(
            stub_invocations(warmups).len(),
            1,
            "the warm-up must be recorded, in its own ledger — an un-recorded \
             exec is one a double-probe regression could hide behind"
        );
        assert_eq!(
            stub_invocations(log),
            Vec::<String>::new(),
            "the warm-up must not enter the ledger the one-auth-status-per-\
             dispatch count is read from; if it does, that count is no longer \
             a measurement of production"
        );
    }

    fn stub_invocations(log: &std::path::Path) -> Vec<String> {
        std::fs::read_to_string(log)
            .unwrap_or_default()
            .lines()
            .map(str::to_string)
            .collect()
    }

    /// The whole admission-to-launch boundary, on the production call graph.
    ///
    /// The stub answers `loggedIn: false` the first time and `loggedIn: true`
    /// the second, so any second observation resolves a *different* mode than
    /// the one the dispatch was admitted on. With a stale ambient key present,
    /// the first answer pins bare mode; a re-probe would see the login and flip
    /// to native, neutralising the key the preflight ruled on and dropping
    /// `--bare` — after ownership.
    ///
    /// Asserted together, because they are one property: the admitted mode, the
    /// spawned argv, the spawned env and the NativeRuntime record the supervisor
    /// writes to RunMeta must all describe the same decision. And the count is
    /// named, not described: **one** `auth status` per dispatch.
    ///
    /// The stub is deliberately late on its first exec
    /// ([`make_recording_stub_that_starts_late`]), because that is the shape
    /// that made this test fail under load on 2026-07-29 wearing a
    /// credential-precedence mask — `left: "native_login" / right:
    /// "bare_api_key"`, which reads as a broken precedence rule and is really a
    /// probe whose first attempt was killed after it had already consumed the
    /// stub's first answer. What holds the assertion up is
    /// [`warm_up_recording_stub`] paying that first exec where nothing is timing
    /// it; delete the warm-up and this test goes red every run rather than two
    /// runs in five (TASK-D1Z87).
    #[tokio::test]
    async fn the_launch_uses_the_credential_the_preflight_admitted() {
        let dir = tempfile::tempdir().expect("tempdir");
        let log = make_recording_stub_that_starts_late(dir.path(), &[false, true]);
        let settings = tempfile::tempdir().expect("settings dir");

        let _guard = env_lock().lock().await;
        let saved_path = std::env::var("PATH").unwrap_or_default();
        std::env::set_var("PATH", format!("{}:{saved_path}", dir.path().display()));
        std::env::remove_var("ORGASMIC_DRIVER_SIMULATE");
        // An empty settings dir, so the operator's real `apiKeyHelper` (if any)
        // cannot decide this test's outcome.
        std::env::set_var("CLAUDE_CONFIG_DIR", settings.path());
        const STALE_KEY: &str = "sk-ant-stale-but-the-only-credential-there-is";
        std::env::set_var("ANTHROPIC_API_KEY", STALE_KEY);

        let driver = AcpStdioDriver::new(Box::new(ClaudeAdapter::new()));
        let ctx = ctx("run-pinned-credential", RunKind::Worker);
        // The shape a real dispatch sends: no endpoint (dec_S18RH).
        let config = DriverConfig(json!({}));

        // 1. Admission.
        let outcome = driver.preflight(&ctx, &config).await;
        assert!(
            outcome.rejects_dispatch().is_none(),
            "a present key under bare mode is unchecked, not fatal: {outcome:?}"
        );
        let admitted = outcome
            .plan
            .as_ref()
            .and_then(|plan| plan.get("credential_plan"))
            .and_then(|plan| plan.get("mode"))
            .and_then(Value::as_str)
            .expect("the probe must pin the mode it admitted the dispatch on")
            .to_string();
        assert_eq!(admitted, "bare_api_key");

        // 2. Ownership. This is the step the daemon takes between admitting the
        //    dispatch and acquiring it (`spawn_worker_run`).
        let launch_config = outcome.pin_into(&config);

        // 3. Launch. Composed through the mode wrapper `AcpStdioDriver::acquire`
        //    delegates to, so this is the request that would be spawned.
        let mut mode_adapter = AcpStdioComposeAdapter {
            inner: Box::new(ClaudeAdapter::new()),
            jsonrpc_session_init: None,
        };
        let request = mode_adapter
            .compose_request(&ctx, &launch_config)
            .expect("compose");
        let native = mode_adapter
            .native_runtime()
            .expect("NativeRuntime metadata is what reaches RunMeta");

        std::env::set_var("PATH", &saved_path);
        std::env::remove_var("ANTHROPIC_API_KEY");
        std::env::remove_var("CLAUDE_CONFIG_DIR");

        let HarnessRequest::Subprocess { args, env, .. } = request else {
            panic!("a detectable claude must compose a subprocess request");
        };

        // The argv the admitted mode implies…
        assert!(
            args.iter().any(|a| a == "--bare"),
            "the launch must carry the mode the preflight admitted: {args:?}"
        );
        assert!(
            !args.iter().any(|a| a == "--safe-mode"),
            "a second observation flipped this run to native mode: {args:?}"
        );
        // …the env that mode implies…
        assert_eq!(
            env.get("ANTHROPIC_API_KEY").map(String::as_str),
            Some(STALE_KEY),
            "the key the preflight ruled on must be the key the child presents: {env:?}"
        );
        // …and the record the supervisor persists.
        assert_eq!(
            native.credential_mode.as_deref(),
            Some(admitted.as_str()),
            "RunMeta must record the admitted mode, not a later re-detection"
        );

        // The count, named. One dispatch, one question to the harness.
        let asked: Vec<String> = stub_invocations(&log)
            .into_iter()
            .filter(|line| line.starts_with("auth status"))
            .collect();
        assert_eq!(
            asked.len(),
            1,
            "a dispatch must ask `claude auth status` exactly once — before it \
             owns anything. Observed: {asked:?}"
        );
    }

    /// Composition must not spawn the harness, so a wedged `claude` cannot
    /// stall the runtime thread `acquire` is running on.
    ///
    /// The stub hangs for 60s on every probe-shaped invocation. `compose_request`
    /// is synchronous and is awaited inside async `WorkerDriver::acquire`, so a
    /// `Command::status()` there holds a Tokio worker thread for as long as the
    /// harness takes — with no timeout available to bound it, because there is
    /// no timeout in a sync function (TASK-Z3093's lesson, recurring).
    ///
    /// Two assertions, and the second is the stronger one. The deadline proves
    /// acquisition stays bounded; the empty invocation log proves *why* — the
    /// composition asked the harness nothing at all, so there is nothing left
    /// that could hang. This runs the no-plan fallback deliberately: the path
    /// with the least information available is the one that would be tempted to
    /// go and ask.
    #[tokio::test]
    async fn composition_asks_a_wedged_claude_nothing_and_stays_bounded() {
        let dir = tempfile::tempdir().expect("tempdir");
        let log = make_recording_stub(dir.path(), &[]);
        let settings = tempfile::tempdir().expect("settings dir");

        let _guard = env_lock().lock().await;
        let saved_path = std::env::var("PATH").unwrap_or_default();
        std::env::set_var("PATH", format!("{}:{saved_path}", dir.path().display()));
        std::env::remove_var("ORGASMIC_DRIVER_SIMULATE");
        std::env::remove_var("ANTHROPIC_API_KEY");
        std::env::set_var("CLAUDE_CONFIG_DIR", settings.path());

        let ctx = ctx("run-wedged-claude", RunKind::Worker);
        // No pinned plan: the fallback path, with nothing to go on.
        let config = DriverConfig(json!({}));

        // Composed on its own thread and joined against a deadline, so a
        // regression fails this test rather than hanging the suite for a minute
        // per blocking call.
        let (done, composed) = std::sync::mpsc::channel();
        let worker = std::thread::spawn(move || {
            let started = std::time::Instant::now();
            let mut adapter = ClaudeAdapter::new();
            let request = adapter.compose_request(&ctx, &config);
            let _ = done.send((started.elapsed(), request.is_ok()));
        });
        let (elapsed, composed_ok) = composed.recv_timeout(Duration::from_secs(10)).expect(
            "composition never returned: something in compose_request is waiting on the \
                 harness, which is a blocked Tokio worker thread in production",
        );
        let _ = worker.join();

        std::env::set_var("PATH", &saved_path);
        std::env::remove_var("CLAUDE_CONFIG_DIR");

        assert!(composed_ok, "an executable claude must compose a real run");
        assert!(
            elapsed < Duration::from_secs(2),
            "composition took {elapsed:?}; it must not wait on the harness at all"
        );
        assert_eq!(
            stub_invocations(&log),
            Vec::<String>::new(),
            "composition must spawn no `claude` at all — not `--version`, not \
             `auth status`. Anything here is a blocking call on a runtime worker."
        );
    }

    /// The mode layer composes once, not twice.
    ///
    /// `AcpStdioDriver::acquire` used to hand a fresh adapter clone to
    /// `SubprocessStreamJsonDriver::acquire`, which composed the request a
    /// second time — so the argv that got spawned was never the argv acp-stdio
    /// had built, and every per-run decision was made twice.
    #[tokio::test]
    async fn acp_stdio_spawns_the_request_it_composed() {
        let dir = tempfile::tempdir().expect("tempdir");
        let log = make_recording_stub(dir.path(), &[true]);
        let settings = tempfile::tempdir().expect("settings dir");

        let _guard = env_lock().lock().await;
        let saved_path = std::env::var("PATH").unwrap_or_default();
        std::env::set_var("PATH", format!("{}:{saved_path}", dir.path().display()));
        std::env::remove_var("ORGASMIC_DRIVER_SIMULATE");
        std::env::remove_var("ANTHROPIC_API_KEY");
        std::env::set_var("CLAUDE_CONFIG_DIR", settings.path());

        let driver = AcpStdioDriver::new(Box::new(ClaudeAdapter::new()));
        let ctx = ctx("run-single-composition", RunKind::Worker);
        let config = DriverConfig(json!({}));
        let outcome = driver.preflight(&ctx, &config).await;
        let launch_config = outcome.pin_into(&config);

        let mut session = driver
            .acquire(ctx.clone(), launch_config)
            .await
            .expect("acquire must spawn the stub");
        // Wait for the stub to speak before reading its log: `acquire` returns
        // as soon as the child is spawned, which is before the child has run.
        timeout(Duration::from_secs(10), session.events.recv())
            .await
            .expect("timed out waiting for the spawned stub")
            .expect("event stream closed before the stub spoke");

        std::env::set_var("PATH", &saved_path);
        std::env::remove_var("CLAUDE_CONFIG_DIR");

        // The spawned process is the composed one: it carries the pinned
        // session id, which only the adapter's composition mints.
        let native = session
            .native_runtime
            .clone()
            .expect("the spawned run must report NativeRuntime metadata");
        assert_eq!(
            native.session_id.as_deref(),
            Some(ctx.identity.runtime_id.as_str())
        );

        let harness_invocations: Vec<String> = stub_invocations(&log)
            .into_iter()
            .filter(|line| line.contains("--session-id"))
            .collect();
        assert_eq!(
            harness_invocations.len(),
            1,
            "one dispatch spawns one harness: {harness_invocations:?}"
        );
        assert!(
            harness_invocations[0].contains(ctx.identity.runtime_id.as_str()),
            "the spawned argv must be the composed one: {harness_invocations:?}"
        );
        let asked: Vec<String> = stub_invocations(&log)
            .into_iter()
            .filter(|line| line.starts_with("auth status") || line.starts_with("--version"))
            .collect();
        assert_eq!(
            asked.len(),
            1,
            "one `auth status` at preflight and nothing else; observed {asked:?}"
        );

        let _ = session.control.release("test cleanup").await;
    }
}
