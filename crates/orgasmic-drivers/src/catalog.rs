// orgasmic:dec_WDR5K, task_JQARS
//! Manager-facing transport catalog.
//!
//! A manager picking a `(mode, harness)` pair needs three things: the pairs
//! that exist, whether a dispatch on one can run with nobody attached, and
//! which model/effort values the harness will accept. Before TASK-JQARS none
//! of it was reachable from the CLI, so a manager read `SUPPORTED` out of the
//! Rust source and took the first entry — source order decided a dispatch.
//!
//! Everything here is derived. The pair list is [`crate::SUPPORTED`] itself,
//! the unattended/pane answer comes from the driver that pair builds
//! ([`crate::WorkerDriver::interaction`]), and the runtime-options answer comes
//! from the harness adapter's own catalog surface. Nothing restates the matrix
//! in a second list that can drift out of step with it.
//!
//! dec_WDR5K item 9 permits structured protocol adapters only: no code here
//! shells out to a harness CLI and parses its text. Where a harness exposes no
//! machine-readable catalog, the honest answer is
//! [`RuntimeOptionsSource::Unavailable`] carrying the adapter's own reason.

use orgasmic_core::RuntimeIdentity;
use serde::Serialize;

use crate::r#trait::{DriverConfig, DriverContext, Preflight, PreflightOutcome, RunKind};
use crate::runtime_options::dedupe_non_empty;
use crate::{adapter_for_pair, driver_for_mode_harness, SUPPORTED};

/// Whether a dispatch on a transport runs with nobody attached, or spawns an
/// interactive terminal pane.
///
/// Three-valued for the same reason [`crate::Preflight`] is: a driver that
/// never declared its answer must not be able to pass as unattended. New mode
/// drivers get [`Self::Undeclared`] until they say otherwise, and
/// `every_supported_pair_declares_interaction` fails while one still does.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum TransportInteraction {
    /// Protocol transport. The daemon drives the harness over a pipe or a
    /// socket; a dispatch needs no terminal and no operator.
    Unattended,
    /// The mode spawns the harness as an interactive TUI in a terminal pane.
    /// The daemon still drives it, but the run owns a pane an operator can
    /// attach to, and the mode's own pane runtime must be installed.
    TerminalPane,
    /// The driver has not declared how it runs. Never read as "unattended".
    Undeclared,
}

impl TransportInteraction {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Unattended => "unattended",
            Self::TerminalPane => "tui-pane",
            Self::Undeclared => "undeclared",
        }
    }

    /// One-line explanation for a human choosing a transport.
    pub fn describe(self) -> &'static str {
        match self {
            Self::Unattended => {
                "protocol transport; the daemon drives it with no terminal and no operator"
            }
            Self::TerminalPane => {
                "harness runs as a TUI in a pane an operator can attach to; needs the mode's own \
                 pane runtime"
            }
            Self::Undeclared => {
                "driver does not declare how it runs; do not assume it is safe unattended"
            }
        }
    }

    /// True only for a transport that positively declared it needs no operator.
    pub fn is_unattended(self) -> bool {
        matches!(self, Self::Unattended)
    }
}

/// One supported `(mode, harness)` pair with what a manager needs to choose it.
#[derive(Debug, Clone, Serialize)]
pub struct TransportProfile {
    pub mode: String,
    pub harness: String,
    /// Human-facing label, e.g. "Claude (tmux)".
    pub display_name: String,
    /// Standalone transport label, e.g. "tmux" / "stdio".
    pub mode_label: String,
    /// Standalone provider label, e.g. "Claude" / "Codex".
    pub harness_label: String,
    pub interaction: TransportInteraction,
    /// Harness CLI expected on PATH.
    pub binary: String,
    pub installed: bool,
}

impl TransportProfile {
    /// True when every binary this pair needs resolves right now.
    pub fn ready(&self) -> bool {
        self.installed
    }
}

/// Where a harness's valid model/effort values come from, if anywhere.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "availability", rename_all = "kebab-case")]
pub enum RuntimeOptionsSource {
    /// The adapter fetches a structured catalog over the transport's own
    /// protocol RPC, so the values are knowable only while a session is live.
    ProtocolRpc { method: String },
    /// The adapter builds a structured catalog with no live session, so the
    /// values are listed here.
    Offline {
        source: String,
        models: Vec<String>,
        efforts: Vec<String>,
    },
    /// No machine-readable catalog for this harness. `reason` is the adapter's
    /// own answer, not a guess. Scraping the harness CLI's help text to fill
    /// this in is forbidden (dec_WDR5K item 9).
    Unavailable { reason: String },
}

/// A harness plus where its runtime options can be discovered.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct HarnessRuntimeOptions {
    pub harness: String,
    #[serde(flatten)]
    pub source: RuntimeOptionsSource,
}

/// Every supported pair, in `SUPPORTED` order.
pub fn transport_profiles() -> Vec<TransportProfile> {
    SUPPORTED
        .iter()
        .map(|&(mode, harness)| transport_profile(mode, harness))
        .collect()
}

/// Profile for one pair. Unsupported pairs still describe themselves; their
/// interaction is [`TransportInteraction::Undeclared`] because no driver exists
/// to answer for them.
pub fn transport_profile(mode: &str, harness: &str) -> TransportProfile {
    let binary = harness_binary(harness);
    TransportProfile {
        mode: mode.to_string(),
        harness: harness.to_string(),
        display_name: driver_display_name(mode, harness),
        mode_label: mode_label(mode).to_string(),
        harness_label: harness_label(harness).to_string(),
        interaction: driver_for_mode_harness(mode, harness)
            .map(|driver| driver.interaction())
            .unwrap_or(TransportInteraction::Undeclared),
        binary: binary.to_string(),
        installed: binary_on_path(binary),
    }
}

/// Distinct harnesses reachable through [`SUPPORTED`], in first-appearance
/// order of the matrix.
pub fn supported_harnesses() -> Vec<&'static str> {
    let mut out: Vec<&'static str> = Vec::new();
    for &(_, harness) in SUPPORTED {
        if !out.contains(&harness) {
            out.push(harness);
        }
    }
    out
}

/// Runtime-options discovery for every harness in the matrix.
pub async fn runtime_options_by_harness() -> Vec<HarnessRuntimeOptions> {
    let mut out = Vec::new();
    for harness in supported_harnesses() {
        out.push(harness_runtime_options(harness).await);
    }
    out
}

/// Ask one harness's adapter where its model/effort values come from.
///
/// The probe is the adapter itself: its protocol RPC descriptor when it has
/// one, otherwise its offline catalog, otherwise its own refusal message. It
/// never launches the harness CLI and never parses harness text.
///
/// The adapter consulted is the one a `(mode, harness)` pair would build for
/// this harness under its first supported mode, so a harness whose stdio adapter
/// differs from its subprocess adapter is answered by the adapter that actually
/// carries its catalog.
pub async fn harness_runtime_options(harness: &str) -> HarnessRuntimeOptions {
    let source = match adapter_for_harness(harness) {
        None => RuntimeOptionsSource::Unavailable {
            reason: format!("no adapter registered for harness {harness}"),
        },
        Some(mut adapter) => {
            if let Some(rpc) = adapter.runtime_options_catalog_rpc() {
                RuntimeOptionsSource::ProtocolRpc { method: rpc.method }
            } else {
                match adapter.runtime_options_catalog().await {
                    Ok(catalog) => {
                        let models = catalog
                            .models
                            .iter()
                            .map(|model| model.id.clone())
                            .collect::<Vec<_>>();
                        let efforts = if catalog.efforts.is_empty() {
                            dedupe_non_empty(
                                catalog
                                    .models
                                    .iter()
                                    .flat_map(|model| model.reasoning_efforts.iter().cloned()),
                            )
                        } else {
                            dedupe_non_empty(catalog.efforts.iter().cloned())
                        };
                        let models = dedupe_non_empty(models);
                        // An empty catalog is not a catalog. Hermes answers
                        // `hermes:unavailable` with no models when its
                        // inventory cannot be read; reporting that as a
                        // discovered option set would be the same lie as
                        // omitting the harness.
                        if models.is_empty() && efforts.is_empty() {
                            RuntimeOptionsSource::Unavailable {
                                reason: format!(
                                    "adapter catalog {} lists no models or efforts",
                                    catalog.source
                                ),
                            }
                        } else {
                            RuntimeOptionsSource::Offline {
                                source: catalog.source,
                                models,
                                efforts,
                            }
                        }
                    }
                    Err(err) => RuntimeOptionsSource::Unavailable {
                        reason: err.to_string(),
                    },
                }
            }
        }
    };
    HarnessRuntimeOptions {
        harness: harness.to_string(),
        source,
    }
}

/// One harness's readiness, as `manager drivers --health` reports it.
///
/// `auth` is the dispatch preflight's own verdict — the same probe, the same
/// budget, run the same way (TASK-40ZMJ). `quota` is not probed by any
/// adapter today, so it is always `unknown (no probe)`: the whole point of
/// this surface is to say what is known, never to guess.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct HarnessHealth {
    pub harness: String,
    /// `ok`, `missing`, or `unknown (<why>)`.
    pub auth: String,
    /// Always `unknown (no probe)` until an adapter learns to ask.
    pub quota: String,
    /// The probe's operator-facing reason when it refused (`auth=missing`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
}

impl HarnessHealth {
    /// Word a preflight verdict for the operator. `None` is a harness no
    /// adapter answers for at all.
    fn from_outcome(harness: &str, outcome: Option<&PreflightOutcome>) -> Self {
        let (auth, detail) = match outcome.map(|outcome| &outcome.verdict) {
            None => ("unknown (no probe)".to_string(), None),
            Some(Preflight::Ready) => ("ok".to_string(), None),
            Some(Preflight::Fatal { reason }) => ("missing".to_string(), Some(reason.clone())),
            Some(Preflight::Unsupported) => (
                format!(
                    "unknown ({})",
                    outcome
                        .and_then(|outcome| outcome.unchecked)
                        .unwrap_or("no verdict")
                ),
                None,
            ),
        };
        Self {
            harness: harness.to_string(),
            auth,
            quota: "unknown (no probe)".to_string(),
            detail,
        }
    }
}

/// Readiness of every harness in the matrix, from the dispatch preflight.
pub async fn health_by_harness() -> Vec<HarnessHealth> {
    let mut out = Vec::new();
    for harness in supported_harnesses() {
        out.push(harness_health(harness).await);
    }
    out
}

/// Run one harness's dispatch preflight with no dispatch behind it.
///
/// The context is empty on purpose: a probe must not depend on a run it is
/// not creating (see `api::spawn_worker_run`), and an empty driver config is
/// what a dispatch with no overrides hands the same probe.
pub async fn harness_health(harness: &str) -> HarnessHealth {
    let outcome = match adapter_for_harness(harness) {
        None => None,
        Some(mut adapter) => {
            let ctx = DriverContext {
                identity: RuntimeIdentity::default(),
                run_kind: RunKind::Worker,
                task_id: String::new(),
                worker_id: String::new(),
                project_id: None,
                worktree: None,
            };
            Some(adapter.preflight(&ctx, &DriverConfig::empty()).await)
        }
    };
    HarnessHealth::from_outcome(harness, outcome.as_ref())
}

/// The adapter a supported pair would build for this harness. Falls back to the
/// plain harness adapter for a harness outside the matrix.
fn adapter_for_harness(harness: &str) -> Option<Box<dyn crate::HarnessEventAdapter>> {
    let mode = SUPPORTED
        .iter()
        .find(|(_, h)| *h == harness)
        .map(|(mode, _)| *mode);
    match mode {
        Some(mode) => adapter_for_pair(mode, harness),
        None => crate::adapter_for(harness),
    }
}

/// CLI binary expected on PATH for a given harness.
pub fn harness_binary(harness: &str) -> &str {
    match harness {
        "claude" => "claude",
        "codex" => "codex",
        "cursor-agent" => "cursor-agent",
        "hermes" => "hermes",
        // Bare terminal pseudo-harness: the shell is always present.
        "custom" => "sh",
        other => other,
    }
}

/// Standalone provider label for a harness, e.g. "Claude". The leaf choice once
/// a transport mode is picked.
pub fn harness_label(harness: &str) -> &str {
    match harness {
        "claude" => "Claude",
        "codex" => "Codex",
        "cursor-agent" => "Cursor",
        "hermes" => "Hermes",
        "opencode" => "OpenCode",
        "custom" => "Custom",
        other => other,
    }
}

/// Standalone transport label for a mode, e.g. "tmux" / "stdio". The first
/// choice a UI groups drivers by.
///
/// A mode names the wire and nothing else (TASK-XCJYC), so every label is the
/// mode id itself apart from the one mode whose id is a mouthful.
pub fn mode_label(mode: &str) -> &str {
    match mode {
        "subprocess-stream-json" => "stream-json",
        other => other,
    }
}

/// Human-facing label for a `(mode, harness)` driver, e.g. "Claude (tmux)".
pub fn driver_display_name(mode: &str, harness: &str) -> String {
    format!("{} ({})", harness_label(harness), mode_label(mode))
}

/// True when `binary` resolves to a file on the current PATH.
pub fn binary_on_path(binary: &str) -> bool {
    std::env::var_os("PATH")
        .map(|paths| std::env::split_paths(&paths).any(|dir| dir.join(binary).is_file()))
        .unwrap_or(false)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The regression the missing command was really about: the catalog is the
    /// matrix, not a copy of it. A pair added to `SUPPORTED` shows up here or
    /// this fails.
    #[test]
    fn profiles_cover_every_supported_pair_exactly() {
        let profiles = transport_profiles();
        assert_eq!(profiles.len(), SUPPORTED.len());
        for (profile, &(mode, harness)) in profiles.iter().zip(SUPPORTED.iter()) {
            assert_eq!(profile.mode, mode);
            assert_eq!(profile.harness, harness);
        }
    }

    /// No supported pair may fall back to the honest-but-useless default. A new
    /// mode driver that forgets to declare how it runs fails here rather than
    /// being read as unattended.
    #[test]
    fn every_supported_pair_declares_interaction() {
        for profile in transport_profiles() {
            assert_ne!(
                profile.interaction,
                TransportInteraction::Undeclared,
                "{}/{} does not declare its interaction",
                profile.mode,
                profile.harness
            );
        }
    }

    /// The discriminator a manager actually chooses on: pane modes are the
    /// tmux is the interactive pane mode; everything else runs headless.
    #[test]
    fn pane_modes_are_the_only_interactive_transports() {
        for profile in transport_profiles() {
            let expected = match profile.mode.as_str() {
                "tmux" => TransportInteraction::TerminalPane,
                _ => TransportInteraction::Unattended,
            };
            assert_eq!(
                profile.interaction, expected,
                "unexpected interaction for {}/{}",
                profile.mode, profile.harness
            );
        }
    }

    #[test]
    fn supported_harnesses_are_distinct_and_complete() {
        let harnesses = supported_harnesses();
        for &(_, harness) in SUPPORTED {
            assert!(harnesses.contains(&harness), "missing harness {harness}");
        }
        let mut sorted = harnesses.clone();
        sorted.sort_unstable();
        sorted.dedup();
        assert_eq!(sorted.len(), harnesses.len(), "duplicate harness entry");
    }

    /// Harnesses without a machine-readable catalog must say so rather than
    /// disappear from the listing (dec_WDR5K item 9).
    ///
    /// `hermes` and `opencode` are probed by the command but not here: hermes
    /// builds its catalog by running a Python inventory API and opencode by
    /// running the SDK provider host, so asserting on them would make this
    /// suite depend on a machine's installs and pay those subprocesses on
    /// every run. The four harnesses below answer from the adapter alone.
    #[tokio::test]
    async fn every_harness_reports_a_runtime_options_answer() {
        for harness in supported_harnesses() {
            if matches!(harness, "hermes" | "opencode") {
                continue;
            }
            let answer = harness_runtime_options(harness).await;
            assert_eq!(answer.harness, harness);
            if let RuntimeOptionsSource::Unavailable { reason } = &answer.source {
                assert!(
                    !reason.trim().is_empty(),
                    "{harness} must give a reason for having no catalog"
                );
            }
        }
        assert_eq!(
            harness_runtime_options("codex").await.source,
            RuntimeOptionsSource::ProtocolRpc {
                method: "model/list".into()
            },
            "codex options come from the app-server model/list RPC"
        );
        // The harness with no catalog surface at all is still listed, with the
        // adapter's own refusal as the reason.
        let claude = harness_runtime_options("claude").await;
        assert!(
            matches!(claude.source, RuntimeOptionsSource::Unavailable { .. }),
            "claude has no catalog surface; it must say so: {:?}",
            claude.source
        );
    }

    /// `--health` words the dispatch preflight and invents nothing: a refused
    /// probe is `missing`, an inconclusive one says why it could not answer,
    /// and quota is `unknown (no probe)` because no adapter asks (TASK-40ZMJ).
    #[test]
    fn health_words_each_preflight_outcome_and_never_invents_quota() {
        let ready = HarnessHealth::from_outcome(
            "claude",
            Some(&PreflightOutcome::verdict(Preflight::Ready)),
        );
        assert_eq!(ready.auth, "ok");
        assert_eq!(ready.quota, "unknown (no probe)");

        let refused = HarnessHealth::from_outcome(
            "codex",
            Some(&PreflightOutcome::verdict(Preflight::fatal(
                "run `codex login`",
            ))),
        );
        assert_eq!(refused.auth, "missing");
        assert_eq!(refused.detail.as_deref(), Some("run `codex login`"));

        let timed_out = HarnessHealth::from_outcome(
            "cursor-agent",
            Some(&PreflightOutcome::unasked(
                crate::preflight::Unasked::Timeout,
            )),
        );
        assert_eq!(timed_out.auth, "unknown (timeout)");

        let no_verdict = HarnessHealth::from_outcome("hermes", Some(&PreflightOutcome::default()));
        assert_eq!(no_verdict.auth, "unknown (no verdict)");

        let no_adapter = HarnessHealth::from_outcome("nothing", None);
        assert_eq!(no_adapter.auth, "unknown (no probe)");
        assert_eq!(no_adapter.quota, "unknown (no probe)");
    }
}
