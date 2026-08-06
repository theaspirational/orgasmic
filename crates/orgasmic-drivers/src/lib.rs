// orgasmic:arch_A53QX, dec_ASB1A
//! orgasmic-drivers — fixed mode drivers composed with fixed harness adapters.
//!
//! Legacy transport ids such as `claude-stream-json` and `codex-appserver`
//! remain registry aliases. The first-class shape is `(mode, harness)`, where
//! the mode names only the wire and the harness decides the protocol spoken
//! over it (TASK-XCJYC). See [`RESERVED_MODES`] for the ids that are off
//! limits.

use async_trait::async_trait;

pub mod adapters;
pub mod catalog;
pub mod modes;
/// Shared readiness-probe machinery. Internal: adapters implement
/// `preflight`, callers consume [`r#trait::Preflight`].
pub(crate) mod preflight;
pub mod runtime_options;
pub mod sandbox;
pub mod r#trait;
pub mod transcript_finder;

pub use adapters::{
    ClaudeAdapter, CodexAdapter, CursorAcpAdapter, CursorAdapter, HermesAdapter, ShellAdapter,
};
pub use catalog::{
    harness_runtime_options, runtime_options_by_harness, transport_profile, transport_profiles,
    HarnessRuntimeOptions, RuntimeOptionsSource, TransportInteraction, TransportProfile,
};
pub use modes::rmux::{probe_rmux_binary, RmuxBinaryProbe};
pub use modes::{RmuxDriver, StdioDriver, SubprocessStreamJsonDriver, TmuxDriver, WsDriver};
pub use r#trait::{
    build_babysitter_request, implementer_tool_is_allowed, AttachOutcome, Attached, BabysitterAck,
    BabysitterRequest, DriverConfig, DriverContext, DriverControl, DriverError, DriverSession,
    HarnessControlOutcome, HarnessEventAdapter, HarnessRequest, NativeRuntimeMeta, Preflight,
    PreflightOutcome, RunKind, StdioSpawn, TransitionAck, TransitionRequest, UserInputAck,
    UserInputRequest, WireMessage, WorkerDriver, WsProtocol,
};
pub use runtime_options::{
    RuntimeModelOption, RuntimeOptionsAck, RuntimeOptionsCatalog, RuntimeOptionsCatalogRpc,
    RuntimeOptionsRequest, RuntimeOptionsState, RuntimeProviderOption, RuntimeSpeed,
};
pub use sandbox::{allowlist_from_driver_config, ApprovalResponse, SandboxAllowlist};
pub use transcript_finder::{
    find_native_transcript, lookup_from_envelopes, NativeTranscriptHit, TranscriptConfidence,
    TranscriptFindResult, TranscriptLookup, TranscriptRoots,
};

/// Stable legacy transport ids known to the registry.
pub const TRANSPORTS: &[&str] = &[
    "claude-stream-json",
    "codex-appserver",
    "cursor-acp",
    "cursor-agent",
    "hermes",
    "tmux-tui",
];

/// First-class mode ids.
///
/// `rmux` is a **bounded smoke** mode (TASK-104), not a production replacement
/// for `tmux`. It is registered so the driver-catalog can surface it with its
/// own (separately checked) `rmux` binary requirement.
pub const MODES: &[&str] = &["subprocess-stream-json", "stdio", "ws", "tmux", "rmux"];

// orgasmic:TASK-XCJYC, term_YX8AG
/// Mode ids reserved for the **Agent Client Protocol** and unusable for
/// anything else.
///
/// ACP is a specific open standard (Zed Industries, August 2025): JSON-RPC 2.0
/// over stdio for local agents, HTTP/WebSocket for remote, standardising the
/// editor↔agent boundary the way MCP standardises tool access. Until
/// TASK-XCJYC orgasmic spelled two of its own modes `acp-stdio` and `acp-ws`,
/// and neither carried ACP: `acp-stdio`+claude ran Claude Code's own
/// stream-json wire, `acp-ws`+codex ran `codex app-server`'s own JSON-RPC.
/// Readers twice took two unrelated native protocols for one protocol's two
/// implementations, and the name a real ACP mode would want was already spent.
///
/// The modes were renamed to the wire they actually name (`stdio`, `ws`); the
/// harness field already decides the protocol. These ids stay empty so that an
/// orgasmic mode called `acp-*` can only ever mean ACP. If you are adding real
/// ACP, take one — remove it from here in the same change that registers it.
///
/// [`reserved_modes_are_unused`] is what makes this a rule rather than a
/// comment: it fails if any reserved id reappears in [`MODES`], [`SUPPORTED`],
/// or [`TRANSPORTS`].
pub const RESERVED_MODES: &[&str] = &["acp", "acp-stdio", "acp-ws"];

/// First-class harness ids. `custom` is the pseudo-harness for a bare PTY
/// terminal session (no agent CLI — the operator runs any tool by hand).
pub const HARNESSES: &[&str] = &["codex", "claude", "cursor-agent", "hermes", "custom"];

// orgasmic:TASK-3NJ9K
/// Will a mux launch of `harness` exec an agent CLI on its own?
///
/// A multiplexer runs whatever command the caller writes into the driver
/// config — but the daemon's dispatch path writes a *placeholder* (`sh -lc
/// 'echo orgasmic pipeline stage acquired; exec sh'`), and both mux modes
/// deliberately swap that placeholder for the harness's real binary. So for
/// every harness below, "the caller controls the command" is false on exactly
/// the path a dispatch takes; `custom` is the one first-class harness that
/// cannot become a provider process by itself, which is what makes it the
/// pseudo-harness [`HARNESSES`] describes.
///
/// The daemon's test-profile fence reads this to decide whether a mux address
/// is safe for a test to hold. It lives here, next to [`HARNESSES`], because
/// the ground truth is each mode's `default_command_for_harness`; both modes
/// carry a test asserting they still agree with this answer.
#[must_use]
pub fn harness_execs_provider_binary(harness: &str) -> bool {
    matches!(harness, "claude" | "codex" | "cursor-agent" | "hermes")
}

/// The originator orgasmic stamps onto every codex launch it owns, and the
/// codex environment variable that sets it.
///
/// Codex derives `session_meta.originator` from whichever frontend started the
/// session — `codex-tui` for the interactive TUI, `codex_exec` for `codex
/// exec` — unless [`CODEX_ORIGINATOR_ENV`] overrides it. The app-server driver
/// gets there by a second route: codex adopts the validated `clientInfo.name`
/// from `initialize`, which is why ws sessions have always recorded
/// `orgasmic` while mux-launched TUI sessions recorded `codex-tui`.
///
/// [`transcript_finder`] uses this value as the correlator for its codex
/// cwd scan, so the launch paths that stamp it and the finder that requires it
/// must agree on exactly one constant — a finder gating on a value no launch
/// path produces makes every codex transcript unreachable (TASK-GT91X).
// orgasmic:TASK-GT91X
pub const CODEX_ORIGINATOR_ENV: &str = "CODEX_INTERNAL_ORIGINATOR_OVERRIDE";

/// Value stamped into [`CODEX_ORIGINATOR_ENV`] and required by the codex
/// transcript finder's cwd scan.
// orgasmic:TASK-GT91X
pub const CODEX_ORIGINATOR: &str = "orgasmic";

/// Explicitly supported first-class `(mode, harness)` pairs.
///
/// rmux attaches through the same daemon PTY bridge as tmux (`rmux
/// attach-session`), so it offers the same interactive harnesses. It still
/// requires a separately provisioned `rmux` binary (checked independently).
pub const SUPPORTED: &[(&str, &str)] = &[
    ("stdio", "claude"),
    ("stdio", "codex"),
    ("stdio", "cursor-agent"),
    ("stdio", "hermes"),
    ("ws", "codex"),
    ("ws", "hermes"),
    ("subprocess-stream-json", "cursor-agent"),
    ("tmux", "claude"),
    ("tmux", "codex"),
    ("tmux", "cursor-agent"),
    ("tmux", "hermes"),
    ("rmux", "claude"),
    ("rmux", "codex"),
    ("rmux", "cursor-agent"),
    ("rmux", "hermes"),
    // Arbitrary operator-supplied CLI in an rmux pane. Manager launches with
    // no harness_args get a bare login shell; worker templates supply the
    // wrapped command line via `:HARNESS_ARGS:` (e.g. `opencode`) and the
    // compiled dispatch prompt is pasted into the spawned TUI.
    ("rmux", "custom"),
];

/// Validate that `(mode, harness)` is in the sole transport registry.
pub fn validate_supported_pair(mode: &str, harness: &str) -> Result<(), String> {
    let mode = mode.trim();
    let harness = harness.trim();
    if mode.is_empty() || harness.is_empty() {
        return Err("mode and harness are required".into());
    }
    if SUPPORTED.contains(&(mode, harness)) {
        return Ok(());
    }
    let supported = SUPPORTED
        .iter()
        .map(|(m, h)| format!("{m}/{h}"))
        .collect::<Vec<_>>()
        .join(", ");
    Err(format!(
        "unsupported mode/harness pair {mode}/{harness}; supported: {supported}"
    ))
}

pub struct ClaudeStreamJsonDriver;
pub struct CodexAppserverDriver;
pub struct CursorAcpDriver;
pub struct CursorAgentDriver;
pub struct HermesDriver;
pub struct TmuxTuiDriver;

/// Build a boxed driver by legacy transport id. Returns `None` for unknown ids.
pub fn driver_for(transport: &str) -> Option<Box<dyn WorkerDriver>> {
    match transport {
        "claude-stream-json" => Some(Box::new(ClaudeStreamJsonDriver)),
        "codex-appserver" => Some(Box::new(CodexAppserverDriver)),
        "cursor-acp" => Some(Box::new(CursorAcpDriver)),
        "cursor-agent" => Some(Box::new(CursorAgentDriver)),
        "hermes" => Some(Box::new(HermesDriver)),
        "tmux-tui" => Some(Box::new(TmuxTuiDriver)),
        _ => None,
    }
}

/// The harness adapter a supported `(mode, harness)` pair runs on.
///
/// Some harnesses speak a different dialect under a different mode, so the
/// adapter is a property of the pair, not of the harness alone. Both the driver
/// registry and the manager-facing catalog resolve it here so they cannot
/// disagree about which adapter a pair actually uses.
pub fn adapter_for_pair(mode: &str, harness: &str) -> Option<Box<dyn HarnessEventAdapter>> {
    match (mode, harness) {
        ("stdio", "cursor-agent") => Some(Box::new(CursorAcpAdapter::new())),
        _ => adapter_for(harness),
    }
}

/// Build a mode driver from explicit `(mode, harness)` ids.
pub fn driver_for_mode_harness(mode: &str, harness: &str) -> Option<Box<dyn WorkerDriver>> {
    if !SUPPORTED.contains(&(mode, harness)) {
        return None;
    }
    let adapter: Box<dyn HarnessEventAdapter> = adapter_for_pair(mode, harness)?;
    match mode {
        "subprocess-stream-json" => Some(Box::new(SubprocessStreamJsonDriver::new(adapter))),
        "stdio" => Some(Box::new(StdioDriver::new(adapter))),
        "ws" => Some(Box::new(WsDriver::new(adapter))),
        "tmux" => Some(Box::new(TmuxDriver::new(adapter))),
        "rmux" => Some(Box::new(RmuxDriver::new(adapter))),
        _ => None,
    }
}

pub fn adapter_for(harness: &str) -> Option<Box<dyn HarnessEventAdapter>> {
    match harness {
        "codex" => Some(Box::new(CodexAdapter::new())),
        "claude" => Some(Box::new(ClaudeAdapter::new())),
        "cursor-agent" => Some(Box::new(CursorAdapter::new())),
        "hermes" => Some(Box::new(HermesAdapter::new())),
        "custom" => Some(Box::new(ShellAdapter::new())),
        _ => None,
    }
}

macro_rules! legacy_driver {
    ($ty:ty, $legacy:literal, $mode:literal, $harness:literal) => {
        #[async_trait]
        impl WorkerDriver for $ty {
            fn transport(&self) -> &'static str {
                $legacy
            }

            fn harness(&self) -> Option<&'static str> {
                Some($harness)
            }

            fn interaction(&self) -> catalog::TransportInteraction {
                driver_for_mode_harness($mode, $harness)
                    .expect("legacy mode/harness is registered")
                    .interaction()
            }

            fn validate(&self, config: &DriverConfig) -> Result<(), DriverError> {
                driver_for_mode_harness($mode, $harness)
                    .expect("legacy mode/harness is registered")
                    .validate(config)
            }

            async fn acquire(
                &self,
                ctx: DriverContext,
                config: DriverConfig,
            ) -> Result<DriverSession, DriverError> {
                driver_for_mode_harness($mode, $harness)
                    .expect("legacy mode/harness is registered")
                    .acquire(ctx, config)
                    .await
            }

            async fn attach(
                &self,
                ctx: DriverContext,
                config: DriverConfig,
            ) -> Result<r#trait::AttachOutcome, DriverError> {
                driver_for_mode_harness($mode, $harness)
                    .expect("legacy mode/harness is registered")
                    .attach(ctx, config)
                    .await
            }
        }
    };
}

legacy_driver!(
    ClaudeStreamJsonDriver,
    "claude-stream-json",
    "stdio",
    "claude"
);
legacy_driver!(CodexAppserverDriver, "codex-appserver", "ws", "codex");
legacy_driver!(CursorAcpDriver, "cursor-acp", "stdio", "cursor-agent");
legacy_driver!(
    CursorAgentDriver,
    "cursor-agent",
    "subprocess-stream-json",
    "cursor-agent"
);
legacy_driver!(HermesDriver, "hermes", "stdio", "hermes");
legacy_driver!(TmuxTuiDriver, "tmux-tui", "tmux", "claude");

#[cfg(test)]
mod tests {
    use super::*;

    // orgasmic:TASK-XCJYC
    /// The reservation, enforced instead of remembered.
    ///
    /// A future contributor reaching for `acp-stdio` because it reads well is
    /// exactly the move TASK-XCJYC undid; this fails on the way in and says
    /// why. Removing an id from [`RESERVED_MODES`] is allowed — it is how real
    /// ACP claims the name — and it is a visible, arguable edit rather than a
    /// silent re-squat.
    #[test]
    fn reserved_modes_are_unused() {
        for reserved in RESERVED_MODES {
            assert!(
                !MODES.contains(reserved),
                "`{reserved}` is reserved for the real Agent Client Protocol and must not name \
                 an orgasmic mode (TASK-XCJYC). A mode names its wire — use `stdio`, `ws`, \
                 `subprocess-stream-json`, `tmux` or `rmux`; the `harness` field already says \
                 which protocol runs over it. If this IS ACP, drop `{reserved}` from \
                 RESERVED_MODES in the same change."
            );
            assert!(
                !SUPPORTED.iter().any(|(mode, _)| mode == reserved),
                "the SUPPORTED matrix pairs a harness with the reserved mode id `{reserved}` \
                 (TASK-XCJYC)"
            );
            assert!(
                !TRANSPORTS.contains(reserved),
                "the legacy transport id `{reserved}` is reserved for the real Agent Client \
                 Protocol (TASK-XCJYC)"
            );
        }
    }

    #[test]
    fn registry_covers_every_transport() {
        for t in TRANSPORTS {
            let d = driver_for(t).expect("known transport");
            assert_eq!(d.transport(), *t);
            // A legacy id is an alias for a first-class pair, so it must give
            // that pair's answer rather than fall through to `Undeclared`.
            assert_ne!(
                d.interaction(),
                catalog::TransportInteraction::Undeclared,
                "legacy transport {t} does not declare its interaction"
            );
        }
        assert!(driver_for("unknown").is_none());
    }

    /// The pair adapter both the driver registry and the manager catalog build
    /// from. `stdio/cursor-agent` is the pair whose adapter is not simply
    /// `adapter_for(harness)`.
    #[test]
    fn pair_adapter_matches_the_registry_special_case() {
        assert_eq!(
            adapter_for_pair("stdio", "cursor-agent")
                .expect("cursor ACP adapter")
                .harness(),
            "cursor-agent"
        );
        for &(mode, harness) in SUPPORTED {
            assert!(
                adapter_for_pair(mode, harness).is_some(),
                "no adapter for supported pair {mode}/{harness}"
            );
        }
    }

    #[test]
    fn explicit_mode_harness_registry_covers_known_keys() {
        for &(mode, harness) in SUPPORTED {
            assert!(MODES.contains(&mode), "unknown supported mode {mode}");
            assert!(
                HARNESSES.contains(&harness),
                "unknown supported harness {harness}"
            );
            let d = driver_for_mode_harness(mode, harness).expect("known mode/harness");
            assert_eq!(d.transport(), mode);
        }
        for &harness in HARNESSES {
            assert!(adapter_for(harness).is_some());
        }
        for &mode in MODES {
            for &harness in HARNESSES {
                let supported = SUPPORTED.contains(&(mode, harness));
                assert_eq!(
                    driver_for_mode_harness(mode, harness).is_some(),
                    supported,
                    "mode={mode} harness={harness}"
                );
            }
        }
        assert!(driver_for_mode_harness("ws", "cursor-agent").is_none());
        assert!(driver_for_mode_harness("unknown", "claude").is_none());
        assert!(driver_for_mode_harness("tmux", "unknown").is_none());
    }

    /// The daemon writes the chosen reasoning effort under BOTH `effort` and
    /// `reasoning_effort` in one driver config (`api.rs`, dispatch spawn and
    /// manager launch). Any adapter that declares `#[serde(alias = "effort")]`
    /// on its `reasoning_effort` field therefore sees one field twice, serde
    /// fails with `duplicate field`, and the whole dispatch dies on a 400 that
    /// names nothing. TASK-4YC8E; claude carried the same alias and reproduced
    /// it exactly.
    ///
    /// This is the production validation path: `driver_for_mode_harness(..)
    /// .validate(..)` is what `api.rs` calls before it commits a run.
    #[test]
    fn every_supported_pair_accepts_the_daemons_dual_key_effort_config() {
        for &(mode, harness) in SUPPORTED {
            let driver = driver_for_mode_harness(mode, harness).expect("known mode/harness");
            // The dispatch-spawn config verbatim, minus the keys that vary per
            // run. Both effort keys carry the same value, as the daemon writes
            // them.
            let config = DriverConfig::from_value(serde_json::json!({
                "transport": mode,
                "harness": harness,
                "endpoint": "",
                "provider": serde_json::Value::Null,
                "model": "some-model",
                "effort": "high",
                "reasoning_effort": "high",
                "harness_args": Vec::<String>::new(),
                "command": "sh",
                "args": ["-lc", "echo hi"],
                "auto_start_turn": false,
            }));
            assert!(
                driver.validate(&config).is_ok(),
                "{mode}/{harness} rejected the daemon's dual-key effort config: {:?}",
                driver.validate(&config).unwrap_err()
            );
        }
    }
}
