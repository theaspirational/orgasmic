// arch: arch_QXS5W.2
// orgasmic:arch_A53QX, arch_QXS5W
//! Worker schema: execution mode/harness plus provider capabilities.

use std::str::FromStr;

use serde::Serialize;
use thiserror::Error;

use crate::org::OrgFile;
use crate::sandbox::SandboxAllowlist;
use crate::schema::{required, section_body, tokenize, SchemaError, WorkerKind};

/// Core's copy of the driver matrix shipped by `orgasmic-drivers`.
///
/// `orgasmic-drivers` depends on `orgasmic-core`, so this crate cannot import
/// `orgasmic_drivers::SUPPORTED` without creating a package cycle. Keep this in
/// lockstep with `crates/orgasmic-drivers/src/lib.rs::SUPPORTED` until the
/// registry data moves to a dependency-neutral crate.
pub const SUPPORTED_WORKER_DRIVER_HARNESSES: &[(&str, &str)] = &[
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
    // rmux attaches through the same PTY bridge as tmux, so it offers the same
    // interactive harnesses; it still requires a separately provisioned `rmux`
    // binary (checked independently of the harness binary).
    ("rmux", "claude"),
    ("rmux", "codex"),
    ("rmux", "cursor-agent"),
    ("rmux", "hermes"),
    // `custom` wraps an arbitrary operator-supplied CLI in an rmux pane:
    // the template's `:HARNESS_ARGS:` is the whole command line (argv[0] +
    // args). rmux-only because the PTY paste path is the only prompt
    // delivery that works for a CLI orgasmic knows nothing about.
    ("rmux", "custom"),
];

#[derive(Debug, Error)]
pub enum WorkerError {
    #[error(transparent)]
    Schema(#[from] SchemaError),
    #[error(
        "{file}: heading {heading}: unsupported worker driver/harness pair: {driver}/{harness}"
    )]
    UnsupportedDriverHarness {
        file: String,
        heading: String,
        driver: String,
        harness: String,
    },
    #[error("{file}: heading {heading}: unsupported legacy :DEFAULT_DRIVER: {driver}")]
    UnsupportedLegacyDefaultDriver {
        file: String,
        heading: String,
        driver: String,
    },
    #[error(
        "{file}: heading {heading}: custom harness requires :HARNESS_ARGS: (the wrapped CLI argv, e.g. `opencode`)"
    )]
    CustomHarnessMissingArgs { file: String, heading: String },
}

/// Legacy `:CONTEXT_BUDGET:` stored approximate tokens; multiply by four for chars.
pub const LEGACY_CONTEXT_BUDGET_TOKEN_MULTIPLIER: u32 = 4;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ContextBudgetCharsError {
    BothFieldsPresent,
    LegacyOverflow,
}

impl std::fmt::Display for ContextBudgetCharsError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::BothFieldsPresent => write!(
                f,
                "CONTEXT_BUDGET and CONTEXT_BUDGET_CHARS cannot both be set"
            ),
            Self::LegacyOverflow => write!(
                f,
                "CONTEXT_BUDGET token value overflows when migrated to characters"
            ),
        }
    }
}

/// Resolve character budget from legacy token and/or explicit char properties.
pub fn resolve_context_budget_chars(
    legacy_tokens: Option<u32>,
    chars: Option<u32>,
) -> Result<Option<u32>, ContextBudgetCharsError> {
    match (legacy_tokens, chars) {
        (Some(_), Some(_)) => Err(ContextBudgetCharsError::BothFieldsPresent),
        (Some(tokens), None) => tokens
            .checked_mul(LEGACY_CONTEXT_BUDGET_TOKEN_MULTIPLIER)
            .ok_or(ContextBudgetCharsError::LegacyOverflow)
            .map(Some),
        (None, Some(c)) => Ok(Some(c)),
        (None, None) => Ok(None),
    }
}

fn parse_context_budget_chars_from_heading(
    heading: &crate::Heading,
    display: &str,
) -> Result<Option<u32>, WorkerError> {
    let parse = |key: &str| -> Result<Option<u32>, WorkerError> {
        heading
            .property(key)
            .map(|value| {
                value.parse::<u32>().map_err(|err| {
                    SchemaError::InvalidPropertyValue {
                        file: display.into(),
                        heading: heading.title.clone(),
                        key: key.into(),
                        detail: format!("expected an unsigned integer: {err}"),
                    }
                    .into()
                })
            })
            .transpose()
    };
    let legacy = parse("CONTEXT_BUDGET")?;
    let chars = parse("CONTEXT_BUDGET_CHARS")?;
    resolve_context_budget_chars(legacy, chars).map_err(|err| {
        SchemaError::InvalidPropertyValue {
            file: display.into(),
            heading: heading.title.clone(),
            key: "CONTEXT_BUDGET".into(),
            detail: err.to_string(),
        }
        .into()
    })
}

#[derive(Debug, Clone, Serialize)]
pub struct Worker<'a> {
    pub id: &'a str,
    pub kind: WorkerKind,
    pub driver: &'a str,
    pub harness: &'a str,
    pub providers: Vec<String>,
    pub default_provider: Option<String>,
    pub linked_skills: Vec<&'a str>,
    pub applicable_states: Vec<String>,
    pub max_iterations: Option<u32>,
    pub context_budget_chars: Option<u32>,
    pub stall_timeout_secs: Option<u32>,
    pub max_run_duration_secs: Option<u32>,
    pub babysitter_worker: Option<&'a str>,
    pub sandbox_permissions: Option<SandboxAllowlist>,
    /// Extra argv passed verbatim to the harness CLI (`:HARNESS_ARGS:`,
    /// whitespace-separated). Lets a template pin harness-specific flags the
    /// schema has no dedicated property for; user args win over the driver's
    /// Harness-specific flags with no dedicated schema property.
    pub harness_args: Vec<String>,
    pub version: Option<&'a str>,
    pub persona: Option<String>,
    pub operating_rules: Option<String>,
}

impl<'a> Worker<'a> {
    pub fn from_org(file: &'a OrgFile, display: &str) -> Result<Self, WorkerError> {
        let file_display = display;
        let (heading, legacy_heading) = if let Some(heading) = file
            .headings
            .iter()
            .find(|h| h.title.starts_with("WORKER "))
        {
            (heading, false)
        } else {
            let heading = file
                .headings
                .iter()
                .find(|h| h.title.starts_with("AGENT-TEMPLATE "))
                .ok_or_else(|| SchemaError::MissingSection {
                    file: display.into(),
                    heading: "WORKER".into(),
                })?;
            tracing::warn!(
                file = %file_display,
                heading = %heading.title,
                "legacy AGENT-TEMPLATE heading parsed as WORKER"
            );
            (heading, true)
        };

        let id = required(heading, "ID", display)?;
        let kind_str = required(heading, "KIND", display)?;
        let kind = WorkerKind::from_str(kind_str).map_err(|_| SchemaError::UnknownWorkerKind {
            file: display.into(),
            heading: id.into(),
            kind: kind_str.into(),
        })?;
        let (driver, harness) = driver_harness(heading, display)?;
        if !is_supported_worker_pair(driver, harness) {
            return Err(WorkerError::UnsupportedDriverHarness {
                file: display.into(),
                heading: heading.title.clone(),
                driver: driver.into(),
                harness: harness.into(),
            });
        }

        let default_provider = normalize_optional(heading.property("DEFAULT_PROVIDER"));
        let providers = parse_or_default_list(heading.property("PROVIDERS"), &default_provider);
        let applicable_states = heading
            .property("APPLICABLE_STATES")
            .map(|v| {
                v.split(',')
                    .map(|s| s.trim())
                    .filter(|s| !s.is_empty())
                    .map(str::to_string)
                    .collect()
            })
            .unwrap_or_default();
        let babysitter_worker = if let Some(value) = heading.property("BABYSITTER_WORKER") {
            Some(value)
        } else {
            let legacy = heading.property("BABYSITTER_AGENT_TEMPLATE");
            if legacy.is_some() {
                tracing::warn!(
                    file = %file_display,
                    worker_id = %id,
                    "legacy BABYSITTER_AGENT_TEMPLATE property parsed as BABYSITTER_WORKER"
                );
            }
            legacy
        };
        if legacy_heading {
            tracing::warn!(
                file = %file_display,
                worker_id = %id,
                "legacy worker properties parsed through compatibility path"
            );
        }
        let sandbox_permissions = heading
            .property("SANDBOX_PERMISSIONS")
            .map(SandboxAllowlist::from_csv)
            .transpose()
            .map_err(|e| SchemaError::InvalidPropertyValue {
                file: display.into(),
                heading: heading.title.clone(),
                key: "SANDBOX_PERMISSIONS".into(),
                detail: e.to_string(),
            })?;

        let harness_args: Vec<String> = heading
            .property("HARNESS_ARGS")
            .map(|v| v.split_whitespace().map(str::to_string).collect())
            .unwrap_or_default();
        // For `custom` the args ARE the wrapped command line; without them a
        // dispatch would spawn the fallback shell and paste the compiled
        // prompt into it, executing prose as shell commands.
        if harness == "custom" && harness_args.is_empty() {
            return Err(WorkerError::CustomHarnessMissingArgs {
                file: display.into(),
                heading: heading.title.clone(),
            });
        }

        Ok(Self {
            id,
            kind,
            driver,
            harness,
            providers,
            default_provider,
            linked_skills: tokenize(heading.property("LINKED_SKILLS")),
            applicable_states,
            max_iterations: heading
                .property("MAX_ITERATIONS")
                .and_then(|v| v.parse().ok()),
            context_budget_chars: parse_context_budget_chars_from_heading(heading, display)?,
            stall_timeout_secs: heading
                .property("STALL_TIMEOUT_SECS")
                .and_then(|v| v.parse().ok()),
            max_run_duration_secs: heading
                .property("MAX_RUN_DURATION_SECS")
                .and_then(|v| v.parse().ok()),
            babysitter_worker,
            sandbox_permissions,
            harness_args,
            version: heading.property("VERSION"),
            persona: section_body(file, heading, "Persona"),
            operating_rules: section_body(file, heading, "Operating Rules"),
        })
    }
}

pub fn parse_string_list(value: Option<&str>) -> Vec<String> {
    value
        .unwrap_or_default()
        .split(|c: char| c == ',' || c.is_whitespace())
        .map(|s| s.trim().to_ascii_lowercase())
        .filter(|s| !s.is_empty())
        .collect()
}

pub fn is_supported_worker_pair(driver: &str, harness: &str) -> bool {
    SUPPORTED_WORKER_DRIVER_HARNESSES.contains(&(driver, harness))
}

fn driver_harness<'a>(
    heading: &'a crate::Heading,
    display: &str,
) -> Result<(&'a str, &'a str), WorkerError> {
    let file_display = display;
    match (heading.property("DRIVER"), heading.property("HARNESS")) {
        (Some(driver), Some(harness)) => Ok((driver, harness)),
        (Some(_), None) => Err(SchemaError::MissingProperty {
            file: display.into(),
            heading: heading.title.clone(),
            key: "HARNESS".into(),
        }
        .into()),
        (None, Some(_)) => Err(SchemaError::MissingProperty {
            file: display.into(),
            heading: heading.title.clone(),
            key: "DRIVER".into(),
        }
        .into()),
        (None, None) => {
            let legacy = required(heading, "DEFAULT_DRIVER", display)?;
            if let Some(default_harness) = heading.property("DEFAULT_HARNESS") {
                return Ok((legacy, default_harness));
            }
            tracing::warn!(
                file = %file_display,
                heading = %heading.title,
                default_driver = %legacy,
                "legacy DEFAULT_DRIVER parsed as DRIVER/HARNESS pair"
            );
            legacy_driver_pair(legacy).ok_or_else(|| WorkerError::UnsupportedLegacyDefaultDriver {
                file: display.into(),
                heading: heading.title.clone(),
                driver: legacy.into(),
            })
        }
    }
}

fn legacy_driver_pair(driver: &str) -> Option<(&'static str, &'static str)> {
    match driver
        .trim()
        .to_ascii_lowercase()
        .replace('_', "-")
        .as_str()
    {
        "claude-acp" => Some(("acp-stdio", "claude")),
        "claude-tmux" => Some(("tmux", "claude")),
        "codex-appserver" => Some(("acp-ws", "codex")),
        "cursor-acp" => Some(("acp-stdio", "cursor-agent")),
        "cursor-agent" => Some(("subprocess-stream-json", "cursor-agent")),
        "hermes" => Some(("acp-stdio", "hermes")),
        "tmux-tui" => Some(("tmux", "claude")),
        _ => None,
    }
}

fn normalize_optional(value: Option<&str>) -> Option<String> {
    value
        .map(|s| s.trim().to_ascii_lowercase())
        .filter(|s| !s.is_empty())
}

fn parse_or_default_list(value: Option<&str>, default: &Option<String>) -> Vec<String> {
    let parsed = parse_string_list(value);
    if parsed.is_empty() {
        default.iter().cloned().collect()
    } else {
        parsed
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{
        atomic::{AtomicUsize, Ordering},
        Arc,
    };
    use tracing::span::{Attributes, Id, Record};
    use tracing::{Event, Metadata, Subscriber};

    fn parse(source: &str) -> OrgFile {
        OrgFile::parse(source, "inline.org").unwrap()
    }

    #[test]
    fn parse_string_list_splits_commas_whitespace_and_lowercases() {
        assert_eq!(
            parse_string_list(Some(" OpenAI, anthropic\t*\nCursor ,, ")),
            vec!["openai", "anthropic", "*", "cursor"]
        );
        assert!(parse_string_list(Some(" , \n\t")).is_empty());
        assert!(parse_string_list(None).is_empty());
    }

    #[test]
    fn worker_rejects_unsupported_driver_harness_pair() {
        let file = parse(
            "* WORKER bad\n:PROPERTIES:\n:ID: bad\n:KIND:             implementer\n:DRIVER: acp-ws\n:HARNESS: cursor-agent\n:END:\n",
        );
        let err = Worker::from_org(&file, "inline.org").unwrap_err();
        assert!(matches!(
            err,
            WorkerError::UnsupportedDriverHarness {
                driver,
                harness,
                ..
            } if driver == "acp-ws" && harness == "cursor-agent"
        ));
    }

    #[test]
    fn legacy_default_driver_ignores_stale_model_catalog_properties() {
        let file = parse(
            "* AGENT-TEMPLATE old\n:PROPERTIES:\n:ID: old\n:KIND:             implementer\n:DEFAULT_DRIVER: cursor-agent\n:DEFAULT_PROVIDER: cursor\n:DEFAULT_MODEL: composer-2.5-fast\n:MODELS: composer-2.5-fast\n:DEFAULT_EFFORT: high\n:REASONING_EFFORTS: high\n:END:\n",
        );
        let worker = Worker::from_org(&file, "inline.org").unwrap();
        assert_eq!(worker.driver, "subprocess-stream-json");
        assert_eq!(worker.harness, "cursor-agent");
        assert_eq!(worker.providers, vec!["cursor"]);
    }

    #[test]
    fn legacy_template_can_pin_default_driver_and_harness_pair() {
        let file = parse(
            "* AGENT-TEMPLATE old\n:PROPERTIES:\n:ID: old\n:KIND:             implementer\n:DEFAULT_DRIVER: acp-stdio\n:DEFAULT_HARNESS: cursor-agent\n:END:\n",
        );
        let worker = Worker::from_org(&file, "inline.org").unwrap();
        assert_eq!(worker.driver, "acp-stdio");
        assert_eq!(worker.harness, "cursor-agent");
    }

    #[test]
    fn worker_parses_run_timeout_properties() {
        let file = parse(
            "* WORKER timed\n:PROPERTIES:\n:ID: timed\n:KIND:             implementer\n:DRIVER: tmux\n:HARNESS: codex\n:STALL_TIMEOUT_SECS: 7200\n:MAX_RUN_DURATION_SECS: 86400\n:END:\n",
        );
        let worker = Worker::from_org(&file, "inline.org").unwrap();
        assert_eq!(worker.stall_timeout_secs, Some(7200));
        assert_eq!(worker.max_run_duration_secs, Some(86400));
    }

    #[test]
    fn worker_parses_harness_args_whitespace_separated() {
        let file = parse(
            "* WORKER argv\n:PROPERTIES:\n:ID: argv\n:KIND:             implementer\n:DRIVER: rmux\n:HARNESS: claude\n:HARNESS_ARGS: --model claude-haiku-4-5  --betas context-1m\n:END:\n",
        );
        let worker = Worker::from_org(&file, "inline.org").unwrap();
        assert_eq!(
            worker.harness_args,
            vec!["--model", "claude-haiku-4-5", "--betas", "context-1m"]
        );

        let bare = parse(
            "* WORKER plain\n:PROPERTIES:\n:ID: plain\n:KIND:             implementer\n:DRIVER: rmux\n:HARNESS: claude\n:END:\n",
        );
        assert!(Worker::from_org(&bare, "inline.org")
            .unwrap()
            .harness_args
            .is_empty());
    }

    #[test]
    fn custom_harness_worker_parses_with_harness_args_as_command_line() {
        let file = parse(
            "* WORKER oc\n:PROPERTIES:\n:ID: oc\n:KIND:             implementer\n:DRIVER: rmux\n:HARNESS: custom\n:HARNESS_ARGS: opencode --print-logs\n:END:\n",
        );
        let worker = Worker::from_org(&file, "inline.org").unwrap();
        assert_eq!(worker.harness, "custom");
        assert_eq!(worker.harness_args, vec!["opencode", "--print-logs"]);
    }

    #[test]
    fn custom_harness_worker_requires_harness_args() {
        let file = parse(
            "* WORKER oc\n:PROPERTIES:\n:ID: oc\n:KIND:             implementer\n:DRIVER: rmux\n:HARNESS: custom\n:END:\n",
        );
        let err = Worker::from_org(&file, "inline.org").unwrap_err();
        assert!(matches!(err, WorkerError::CustomHarnessMissingArgs { .. }));
    }

    #[test]
    fn custom_harness_worker_rejects_non_rmux_driver() {
        let file = parse(
            "* WORKER oc\n:PROPERTIES:\n:ID: oc\n:KIND:             implementer\n:DRIVER: tmux\n:HARNESS: custom\n:HARNESS_ARGS: opencode\n:END:\n",
        );
        let err = Worker::from_org(&file, "inline.org").unwrap_err();
        assert!(matches!(
            err,
            WorkerError::UnsupportedDriverHarness { driver, harness, .. }
                if driver == "tmux" && harness == "custom"
        ));
    }

    #[test]
    fn legacy_default_driver_maps_all_aliases_to_driver_harness_pairs() {
        let cases = &[
            ("claude-acp", "acp-stdio", "claude"),
            ("claude-tmux", "tmux", "claude"),
            ("codex-appserver", "acp-ws", "codex"),
            ("cursor-acp", "acp-stdio", "cursor-agent"),
            ("cursor-agent", "subprocess-stream-json", "cursor-agent"),
            ("hermes", "acp-stdio", "hermes"),
            ("tmux-tui", "tmux", "claude"),
        ];

        for &(legacy_id, expected_driver, expected_harness) in cases {
            let file = parse(&format!(
                "* AGENT-TEMPLATE old\n:PROPERTIES:\n:ID: old\n:KIND:             implementer\n:DEFAULT_DRIVER: {legacy_id}\n:END:\n"
            ));
            let worker = Worker::from_org(&file, "inline.org").unwrap();
            assert_eq!(worker.driver, expected_driver, "{legacy_id}");
            assert_eq!(worker.harness, expected_harness, "{legacy_id}");
        }
    }

    #[test]
    fn legacy_parse_emits_tracing_warning() {
        let file = parse(
            "* AGENT-TEMPLATE old\n:PROPERTIES:\n:ID: old\n:KIND:             implementer\n:DEFAULT_DRIVER: claude-acp\n:END:\n",
        );
        let warnings = Arc::new(AtomicUsize::new(0));
        let subscriber = WarnCounter {
            warnings: warnings.clone(),
        };
        tracing::dispatcher::with_default(&tracing::Dispatch::new(subscriber), || {
            Worker::from_org(&file, "inline.org").unwrap();
        });
        assert!(warnings.load(Ordering::SeqCst) > 0);
    }

    struct WarnCounter {
        warnings: Arc<AtomicUsize>,
    }

    impl Subscriber for WarnCounter {
        fn enabled(&self, metadata: &Metadata<'_>) -> bool {
            *metadata.level() <= tracing::Level::WARN
        }

        fn new_span(&self, _span: &Attributes<'_>) -> Id {
            Id::from_u64(1)
        }

        fn record(&self, _span: &Id, _values: &Record<'_>) {}

        fn record_follows_from(&self, _span: &Id, _follows: &Id) {}

        fn event(&self, event: &Event<'_>) {
            if *event.metadata().level() == tracing::Level::WARN {
                self.warnings.fetch_add(1, Ordering::SeqCst);
            }
        }

        fn enter(&self, _span: &Id) {}

        fn exit(&self, _span: &Id) {}
    }

    #[test]
    fn context_budget_legacy_tokens_migrate_to_chars() {
        let file = parse(
            "* WORKER legacy\n:PROPERTIES:\n:ID: legacy\n:KIND:             implementer\n:DRIVER: tmux\n:HARNESS: codex\n:CONTEXT_BUDGET: 100\n:END:\n",
        );
        let worker = Worker::from_org(&file, "inline.org").unwrap();
        assert_eq!(worker.context_budget_chars, Some(400));
    }

    #[test]
    fn context_budget_chars_property_is_used_directly() {
        let file = parse(
            "* WORKER chars\n:PROPERTIES:\n:ID: chars\n:KIND:             implementer\n:DRIVER: tmux\n:HARNESS: codex\n:CONTEXT_BUDGET_CHARS: 500\n:END:\n",
        );
        let worker = Worker::from_org(&file, "inline.org").unwrap();
        assert_eq!(worker.context_budget_chars, Some(500));
    }

    #[test]
    fn context_budget_rejects_both_legacy_and_chars_properties() {
        let file = parse(
            "* WORKER both\n:PROPERTIES:\n:ID: both\n:KIND:             implementer\n:DRIVER: tmux\n:HARNESS: codex\n:CONTEXT_BUDGET: 100\n:CONTEXT_BUDGET_CHARS: 500\n:END:\n",
        );
        let err = Worker::from_org(&file, "inline.org").unwrap_err();
        assert!(
            matches!(err, WorkerError::Schema(SchemaError::InvalidPropertyValue { detail, .. }) if detail.contains("cannot both be set"))
        );
    }

    #[test]
    fn context_budget_absent_inherits_none() {
        let file = parse(
            "* WORKER plain\n:PROPERTIES:\n:ID: plain\n:KIND:             implementer\n:DRIVER: tmux\n:HARNESS: codex\n:END:\n",
        );
        let worker = Worker::from_org(&file, "inline.org").unwrap();
        assert_eq!(worker.context_budget_chars, None);
    }

    #[test]
    fn context_budget_legacy_migration_rejects_overflow() {
        let file = parse(&format!(
            "* WORKER overflow\n:PROPERTIES:\n:ID: overflow\n:KIND:             implementer\n:DRIVER: tmux\n:HARNESS: codex\n:CONTEXT_BUDGET: {}\n:END:\n",
            u32::MAX
        ));
        let err = Worker::from_org(&file, "inline.org").unwrap_err();
        assert!(
            matches!(err, WorkerError::Schema(SchemaError::InvalidPropertyValue { detail, .. }) if detail.contains("overflow"))
        );
    }

    #[test]
    fn context_budget_rejects_malformed_values() {
        for expected_key in ["CONTEXT_BUDGET", "CONTEXT_BUDGET_CHARS"] {
            let file = parse(&format!(
                "* WORKER malformed\n:PROPERTIES:\n:ID: malformed\n:KIND: implementer\n:DRIVER: tmux\n:HARNESS: codex\n:{expected_key}: many\n:END:\n"
            ));
            let err = Worker::from_org(&file, "inline.org").unwrap_err();
            assert!(
                matches!(err, WorkerError::Schema(SchemaError::InvalidPropertyValue { ref key, .. }) if key == expected_key),
                "malformed {expected_key} must fail closed: {err}"
            );
        }
    }

    #[test]
    fn resolve_context_budget_chars_unit_cases() {
        assert_eq!(
            resolve_context_budget_chars(Some(25), None).unwrap(),
            Some(100)
        );
        assert_eq!(
            resolve_context_budget_chars(None, Some(500)).unwrap(),
            Some(500)
        );
        assert_eq!(resolve_context_budget_chars(None, None).unwrap(), None);
        assert_eq!(
            resolve_context_budget_chars(Some(u32::MAX), None),
            Err(ContextBudgetCharsError::LegacyOverflow)
        );
        assert_eq!(
            resolve_context_budget_chars(Some(1), Some(1)),
            Err(ContextBudgetCharsError::BothFieldsPresent)
        );
    }
}
