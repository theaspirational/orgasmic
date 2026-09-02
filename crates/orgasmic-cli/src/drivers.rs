// orgasmic:dec_WDR5K, task_JQARS
//! `orgasmic manager drivers` — the transport matrix, as a manager surface.
//!
//! `shipped/entry/router.org` has always told managers to run this command;
//! until TASK-JQARS it did not exist, so a manager needing a transport read
//! `pub const SUPPORTED` out of `crates/orgasmic-drivers/src/lib.rs` and took
//! the first array entry. Source order is not a decision.
//!
//! Everything printed here is derived from the drivers crate — the pair list is
//! `SUPPORTED` itself, the unattended/pane answer is the driver's, and the
//! model/effort answer is the harness adapter's. No table in this file restates
//! any of it.

use anyhow::Result;
use clap::Args;
use orgasmic_drivers::catalog::{
    health_by_harness, runtime_options_by_harness, transport_profiles, HarnessHealth,
    HarnessRuntimeOptions, RuntimeOptionsSource, TransportInteraction, TransportProfile,
};
use serde::Serialize;

#[derive(Args, Debug, Clone)]
#[command(after_help = "\
Examples:
  orgasmic manager drivers
  orgasmic manager drivers --json
  orgasmic manager drivers --unattended-only
  orgasmic manager drivers --health")]
pub struct DriversArgs {
    /// Emit the full catalog as JSON.
    #[arg(long)]
    pub json: bool,
    /// Only pairs that declared they run with nobody attached.
    #[arg(long = "unattended-only")]
    pub unattended_only: bool,
    /// Skip the per-harness model/effort section (it probes harness adapters).
    #[arg(long = "no-runtime-options")]
    pub no_runtime_options: bool,
    /// Instead of the matrix, run each harness's dispatch preflight and print
    /// `<harness> auth=<ok|missing|unknown> quota=<...>` — the same probe a
    /// dispatch runs, so a lockout shows here before a worker dies of it.
    /// Quota is not probed by any adapter yet and reads `unknown (no probe)`.
    #[arg(long)]
    pub health: bool,
}

#[derive(Debug, Serialize)]
struct DriversCatalog {
    transports: Vec<TransportProfile>,
    #[serde(skip_serializing_if = "Option::is_none")]
    runtime_options: Option<Vec<HarnessRuntimeOptions>>,
}

pub fn cmd_drivers(args: DriversArgs) -> Result<()> {
    if args.health {
        let runtime = tokio::runtime::Runtime::new()?;
        let health = runtime.block_on(health_by_harness());
        if args.json {
            println!("{}", serde_json::to_string_pretty(&health)?);
        } else {
            print!("{}", render_health(&health));
        }
        return Ok(());
    }
    let mut transports = transport_profiles();
    if args.unattended_only {
        transports.retain(|profile| profile.interaction.is_unattended());
    }

    let runtime_options = if args.no_runtime_options {
        None
    } else {
        let runtime = tokio::runtime::Runtime::new()?;
        Some(runtime.block_on(runtime_options_by_harness()))
    };

    let catalog = DriversCatalog {
        transports,
        runtime_options,
    };

    if args.json {
        println!("{}", serde_json::to_string_pretty(&catalog)?);
    } else {
        print!(
            "{}",
            render(&catalog.transports, catalog.runtime_options.as_deref())
        );
    }
    Ok(())
}

/// Render the human-facing listing. Split out so a test can assert the printed
/// surface covers `SUPPORTED` without spawning the binary.
fn render(
    transports: &[TransportProfile],
    runtime_options: Option<&[HarnessRuntimeOptions]>,
) -> String {
    let mut out = String::new();
    out.push_str(&format!(
        "supported transports ({} pairs) — pass as `--mode <mode> --harness <harness>`\n\n",
        transports.len()
    ));

    let mode_width = column_width(transports.iter().map(|p| p.mode.as_str()), "MODE");
    let harness_width = column_width(transports.iter().map(|p| p.harness.as_str()), "HARNESS");
    let dispatch_width = column_width(
        transports.iter().map(|p| p.interaction.as_str()),
        "DISPATCH",
    );

    out.push_str(&format!(
        "{:mode_width$}  {:harness_width$}  {:dispatch_width$}  REQUIRES\n",
        "MODE",
        "HARNESS",
        "DISPATCH",
        mode_width = mode_width,
        harness_width = harness_width,
        dispatch_width = dispatch_width,
    ));
    for profile in transports {
        out.push_str(&format!(
            "{:mode_width$}  {:harness_width$}  {:dispatch_width$}  {}\n",
            profile.mode,
            profile.harness,
            profile.interaction.as_str(),
            requirements(profile),
            mode_width = mode_width,
            harness_width = harness_width,
            dispatch_width = dispatch_width,
        ));
    }

    out.push_str("\nDISPATCH\n");
    for interaction in [
        TransportInteraction::Unattended,
        TransportInteraction::TerminalPane,
        TransportInteraction::Undeclared,
    ] {
        if transports.iter().any(|p| p.interaction == interaction) {
            out.push_str(&format!(
                "  {:<12}  {}\n",
                interaction.as_str(),
                interaction.describe()
            ));
        }
    }

    if let Some(options) = runtime_options {
        out.push_str(
            "\nHARNESS RUNTIME OPTIONS (values for --model / --effort; unvalidated passthrough)\n",
        );
        let harness_width = column_width(options.iter().map(|o| o.harness.as_str()), "");
        for entry in options {
            out.push_str(&format!(
                "  {:harness_width$}  {}\n",
                entry.harness,
                describe_runtime_options(&entry.source),
                harness_width = harness_width,
            ));
        }
        out.push_str(
            "  (catalogs are harness facts; an RPC source is readable only while a session is live)\n",
        );
    }

    out
}

/// One line per harness; the refusal reason, when there is one, follows on
/// the same line so the operator reads the remedy where they read the verdict.
fn render_health(health: &[HarnessHealth]) -> String {
    let mut out = String::new();
    for entry in health {
        out.push_str(&format!(
            "{} auth={} quota={}",
            entry.harness, entry.auth, entry.quota
        ));
        if let Some(detail) = entry.detail.as_deref() {
            out.push_str(&format!("  ({detail})"));
        }
        out.push('\n');
    }
    out
}

fn requirements(profile: &TransportProfile) -> String {
    format!("{} ({})", profile.binary, installed_word(profile.installed))
}

fn installed_word(installed: bool) -> &'static str {
    if installed {
        "installed"
    } else {
        "missing"
    }
}

fn describe_runtime_options(source: &RuntimeOptionsSource) -> String {
    match source {
        RuntimeOptionsSource::ProtocolRpc { method } => {
            format!("structured catalog over the `{method}` protocol RPC")
        }
        RuntimeOptionsSource::Offline {
            source,
            models,
            efforts,
        } => {
            let mut text = format!("{source}: models {}", join_or_none(models));
            if !efforts.is_empty() {
                text.push_str(&format!("; efforts {}", join_or_none(efforts)));
            }
            text
        }
        RuntimeOptionsSource::Unavailable { reason } => {
            format!("no catalog available for this harness ({reason})")
        }
    }
}

/// How many catalog entries the human listing prints inline. Hermes answers
/// with ~90 models, which is a wall of text in a terminal; the rest are one
/// `--json` away and the line says how many were left out rather than trailing
/// off as if the list were complete.
const INLINE_CATALOG_LIMIT: usize = 12;

fn join_or_none(values: &[String]) -> String {
    if values.is_empty() {
        return "none".to_string();
    }
    if values.len() <= INLINE_CATALOG_LIMIT {
        return values.join(", ");
    }
    format!(
        "{}, … +{} more (--json for all {})",
        values[..INLINE_CATALOG_LIMIT].join(", "),
        values.len() - INLINE_CATALOG_LIMIT,
        values.len()
    )
}

fn column_width<'a>(values: impl Iterator<Item = &'a str>, header: &str) -> usize {
    values
        .map(str::len)
        .chain(std::iter::once(header.len()))
        .max()
        .unwrap_or(header.len())
}

#[cfg(test)]
mod tests {
    use super::*;
    use orgasmic_drivers::SUPPORTED;

    /// The regression the task asks for: a pair added to `SUPPORTED` that the
    /// command does not print fails here. A listing that can silently go stale
    /// is the defect, not the fix.
    #[test]
    fn listing_shows_every_supported_pair() {
        let transports = transport_profiles();
        let text = render(&transports, None);
        for &(mode, harness) in SUPPORTED {
            let expected = format!("{mode}  ");
            assert!(
                text.lines().any(|line| {
                    let mut fields = line.split_whitespace();
                    fields.next() == Some(mode) && fields.next() == Some(harness)
                }),
                "`manager drivers` output is missing {mode}/{harness}\n\
                 (looked for a row starting `{expected}{harness}`)\n{text}"
            );
        }
    }

    /// A manager choosing a transport must be able to see, without reading Rust
    /// source, which pairs can run with nobody attached.
    #[test]
    fn listing_marks_pane_transports_distinctly() {
        let text = render(&transport_profiles(), None);
        let tmux_row = text
            .lines()
            .find(|line| line.starts_with("tmux "))
            .expect("tmux row present");
        assert!(
            tmux_row.contains(TransportInteraction::TerminalPane.as_str()),
            "tmux row must name the pane transport: {tmux_row}"
        );
        let stdio_row = text
            .lines()
            .find(|line| line.starts_with("stdio "))
            .expect("stdio row present");
        assert!(
            stdio_row.contains(TransportInteraction::Unattended.as_str()),
            "stdio row must name the unattended transport: {stdio_row}"
        );
        assert!(text.contains(TransportInteraction::TerminalPane.describe()));
        assert!(text.contains(TransportInteraction::Unattended.describe()));
    }

    #[test]
    fn unattended_filter_drops_pane_transports() {
        let mut transports = transport_profiles();
        transports.retain(|profile| profile.interaction.is_unattended());
        assert!(!transports.is_empty());
        assert!(transports.iter().all(|profile| profile.mode != "tmux"));
    }

    /// A harness with no machine-readable catalog is listed with that fact, not
    /// omitted (dec_WDR5K item 9 forbids filling the gap by parsing CLI text).
    #[test]
    fn harnesses_without_a_catalog_say_so() {
        let options = vec![HarnessRuntimeOptions {
            harness: "claude".into(),
            source: RuntimeOptionsSource::Unavailable {
                reason: "operation not supported by this driver: runtime_options_catalog".into(),
            },
        }];
        let text = render(&transport_profiles(), Some(&options));
        assert!(
            text.contains("claude") && text.contains("no catalog available for this harness"),
            "{text}"
        );
    }

    /// A long catalog is trimmed for the terminal, but the line says how many
    /// it left out. A silently truncated list reads as a complete one.
    #[test]
    fn long_catalogs_declare_what_they_left_out() {
        let models = (0..40).map(|i| format!("model-{i}")).collect::<Vec<_>>();
        let options = vec![HarnessRuntimeOptions {
            harness: "hermes".into(),
            source: RuntimeOptionsSource::Offline {
                source: "hermes:inventory".into(),
                models,
                efforts: vec!["low".into(), "high".into()],
            },
        }];
        let text = render(&transport_profiles(), Some(&options));
        assert!(text.contains("model-0, "), "{text}");
        assert!(
            text.contains(&format!(
                "+{} more (--json for all 40)",
                40 - INLINE_CATALOG_LIMIT
            )),
            "{text}"
        );
        assert!(text.contains("efforts low, high"), "{text}");
    }

    /// `--health` is one line per harness, and the quota column never claims
    /// more than the adapters know (TASK-40ZMJ).
    #[test]
    fn health_listing_is_one_line_per_harness_and_never_invents_quota() {
        let health = vec![
            HarnessHealth {
                harness: "claude".into(),
                auth: "ok".into(),
                quota: "unknown (no probe)".into(),
                detail: None,
            },
            HarnessHealth {
                harness: "codex".into(),
                auth: "missing".into(),
                quota: "unknown (no probe)".into(),
                detail: Some("run `codex login`".into()),
            },
        ];
        let text = render_health(&health);
        let lines: Vec<&str> = text.lines().collect();
        assert_eq!(lines.len(), 2, "{text}");
        assert_eq!(lines[0], "claude auth=ok quota=unknown (no probe)");
        assert_eq!(
            lines[1],
            "codex auth=missing quota=unknown (no probe)  (run `codex login`)"
        );
    }

    #[test]
    fn rpc_catalogs_name_their_method() {
        let options = vec![HarnessRuntimeOptions {
            harness: "codex".into(),
            source: RuntimeOptionsSource::ProtocolRpc {
                method: "model/list".into(),
            },
        }];
        let text = render(&transport_profiles(), Some(&options));
        assert!(text.contains("`model/list` protocol RPC"), "{text}");
    }
}
