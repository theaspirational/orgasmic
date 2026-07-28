// arch: arch_A53QX.2
// orgasmic:arch_A53QX, dec_ASB1A
//! orgasmic-drivers — fixed mode drivers composed with fixed harness adapters.
//!
//! Legacy transport ids such as `claude-acp` and `codex-appserver` remain
//! registry aliases. The first-class shape is `(mode, harness)`.

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
pub use modes::{AcpStdioDriver, AcpWsDriver, RmuxDriver, SubprocessStreamJsonDriver, TmuxDriver};
pub use r#trait::{
    build_babysitter_request, implementer_tool_is_allowed, AcpWsProtocol, AttachOutcome, Attached,
    BabysitterAck, BabysitterRequest, DriverConfig, DriverContext, DriverControl, DriverError,
    DriverSession, HarnessControlOutcome, HarnessEventAdapter, HarnessRequest, NativeRuntimeMeta,
    Preflight, RunKind, StdioSpawn, TransitionAck, TransitionRequest, UserInputAck,
    UserInputRequest, WireMessage, WorkerDriver,
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
    "claude-acp",
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
pub const MODES: &[&str] = &[
    "subprocess-stream-json",
    "acp-stdio",
    "acp-ws",
    "tmux",
    "rmux",
];

/// First-class harness ids. `custom` is the pseudo-harness for a bare PTY
/// terminal session (no agent CLI — the operator runs any tool by hand).
pub const HARNESSES: &[&str] = &["codex", "claude", "cursor-agent", "hermes", "custom"];

/// The originator orgasmic stamps onto every codex launch it owns, and the
/// codex environment variable that sets it.
///
/// Codex derives `session_meta.originator` from whichever frontend started the
/// session — `codex-tui` for the interactive TUI, `codex_exec` for `codex
/// exec` — unless [`CODEX_ORIGINATOR_ENV`] overrides it. The app-server driver
/// gets there by a second route: codex adopts the validated `clientInfo.name`
/// from `initialize`, which is why acp-ws sessions have always recorded
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
    ("acp-stdio", "claude"),
    ("acp-stdio", "codex"),
    ("acp-stdio", "cursor-agent"),
    ("acp-stdio", "hermes"),
    ("acp-ws", "codex"),
    ("acp-ws", "hermes"),
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

pub struct ClaudeAcpDriver;
pub struct CodexAppserverDriver;
pub struct CursorAcpDriver;
pub struct CursorAgentDriver;
pub struct HermesDriver;
pub struct TmuxTuiDriver;

/// Build a boxed driver by legacy transport id. Returns `None` for unknown ids.
pub fn driver_for(transport: &str) -> Option<Box<dyn WorkerDriver>> {
    match transport {
        "claude-acp" => Some(Box::new(ClaudeAcpDriver)),
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
        ("acp-stdio", "cursor-agent") => Some(Box::new(CursorAcpAdapter::new())),
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
        "acp-stdio" => Some(Box::new(AcpStdioDriver::new(adapter))),
        "acp-ws" => Some(Box::new(AcpWsDriver::new(adapter))),
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

legacy_driver!(ClaudeAcpDriver, "claude-acp", "acp-stdio", "claude");
legacy_driver!(CodexAppserverDriver, "codex-appserver", "acp-ws", "codex");
legacy_driver!(CursorAcpDriver, "cursor-acp", "acp-stdio", "cursor-agent");
legacy_driver!(
    CursorAgentDriver,
    "cursor-agent",
    "subprocess-stream-json",
    "cursor-agent"
);
legacy_driver!(HermesDriver, "hermes", "acp-stdio", "hermes");
legacy_driver!(TmuxTuiDriver, "tmux-tui", "tmux", "claude");

#[cfg(test)]
mod tests {
    use super::*;

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
    /// from. `acp-stdio/cursor-agent` is the pair whose adapter is not simply
    /// `adapter_for(harness)`.
    #[test]
    fn pair_adapter_matches_the_registry_special_case() {
        assert_eq!(
            adapter_for_pair("acp-stdio", "cursor-agent")
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
        assert!(driver_for_mode_harness("acp-ws", "cursor-agent").is_none());
        assert!(driver_for_mode_harness("unknown", "claude").is_none());
        assert!(driver_for_mode_harness("tmux", "unknown").is_none());
    }
}
