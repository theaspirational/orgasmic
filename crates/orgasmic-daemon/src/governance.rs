// orgasmic:TASK-AYXPB, dec_WDR5K
//! Per-kind governance defaults and sparse config/dispatch overlays.
//!
//! Parallel source of truth for governance values (dec_WDR5K item 3). Worker
//! templates remain authoritative at spawn until TASK-DZ5NM cutover; this module
//! builds the resolve path and config overlay without changing runtime winners.
//!
//! Precedence (lowest → highest):
//! code kind default < config kind < config (kind,harness) < per-dispatch override.
//!
//! Model and effort are intentionally absent (dec_WDR5K item 9).

use std::collections::BTreeMap;
use std::str::FromStr;

use orgasmic_core::{SandboxAllowlist, WorkerKind};
use serde::{Deserialize, Deserializer, Serialize, Serializer};

/// Supervisor-floor timeouts mirrored as kind defaults when templates omit them.
pub const DEFAULT_STALL_TIMEOUT_SECS: u32 = 600;
pub const DEFAULT_MAX_RUN_DURATION_SECS: u32 = 14_400;

const BASE_STATES: &[&str] = &["working", "done", "blocked", "cancelled"];
const REVIEWER_STATES: &[&str] = &[
    "working",
    "done",
    "blocked",
    "cancelled",
    "approved",
    "requested_changes",
];

/// Addressed babysitter launch configuration (kind is always babysitter).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct BabysitterAddress {
    pub mode: String,
    pub harness: String,
    #[serde(default)]
    pub harness_args: Vec<String>,
    #[serde(default)]
    pub model: Option<String>,
    #[serde(default)]
    pub effort: Option<String>,
}

/// Fully resolved governance values for a (kind[, harness]) lookup.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GovernanceDefaults {
    pub max_iterations: Option<u32>,
    pub context_budget: Option<u32>,
    pub stall_timeout_secs: Option<u32>,
    pub max_run_duration_secs: Option<u32>,
    pub applicable_states: Vec<String>,
    pub linked_skills: Vec<String>,
    pub babysitter: Option<BabysitterAddress>,
    pub sandbox_permissions: Option<SandboxAllowlist>,
}

/// Sparse patch applied over defaults (config overlay or per-dispatch override).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct GovernancePatch {
    pub max_iterations: Option<u32>,
    pub context_budget: Option<u32>,
    pub stall_timeout_secs: Option<u32>,
    pub max_run_duration_secs: Option<u32>,
    pub applicable_states: Option<Vec<String>>,
    pub linked_skills: Option<Vec<String>>,
    /// Tri-state babysitter attachment: absent = inherit, `Some(None)` = disable,
    /// `Some(Some(addr))` = explicit address.
    #[serde(
        default,
        deserialize_with = "deserialize_babysitter_patch",
        serialize_with = "serialize_babysitter_patch",
        skip_serializing_if = "Option::is_none"
    )]
    pub babysitter: Option<Option<BabysitterAddress>>,
    pub sandbox_permissions: Option<SandboxPermissionsPatch>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct SandboxPermissionsPatch {
    #[serde(default)]
    pub allow_exec: Option<bool>,
    #[serde(default)]
    pub allow_patch: Option<bool>,
    #[serde(default)]
    pub allow_network: Option<bool>,
    #[serde(default)]
    pub allow_writes_outside_cwd: Option<bool>,
}

impl SandboxPermissionsPatch {
    /// Least-privilege merge: explicit `false` restricts; explicit `true` cannot
    /// widen a prior `false` (monotonic across overlay layers).
    fn merge_into(&self, list: &mut SandboxAllowlist) {
        if let Some(v) = self.allow_exec {
            list.allow_exec = list.allow_exec && v;
        }
        if let Some(v) = self.allow_patch {
            list.allow_patch = list.allow_patch && v;
        }
        if let Some(v) = self.allow_network {
            list.allow_network = list.allow_network && v;
        }
        if let Some(v) = self.allow_writes_outside_cwd {
            list.allow_writes_outside_cwd = list.allow_writes_outside_cwd && v;
        }
    }
}

/// Sparse `dispatch:` governance overlays keyed by kind or `kind,harness`.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct DispatchGovernanceOverlay {
    by_key: BTreeMap<String, GovernancePatch>,
}

impl DispatchGovernanceOverlay {
    pub fn from_map(map: BTreeMap<String, GovernancePatch>) -> Self {
        Self { by_key: map }
    }

    pub fn is_empty(&self) -> bool {
        self.by_key.is_empty()
    }

    pub fn get(&self, key: &str) -> Option<&GovernancePatch> {
        self.by_key.get(key)
    }

    pub fn for_kind(&self, kind: WorkerKind) -> Option<&GovernancePatch> {
        self.by_key.get(kind.as_str())
    }

    pub fn for_kind_harness(&self, kind: WorkerKind, harness: &str) -> Option<&GovernancePatch> {
        self.by_key.get(&kind_harness_key(kind, harness))
    }
}

pub fn kind_harness_key(kind: WorkerKind, harness: &str) -> String {
    format!("{},{}", kind.as_str(), harness.trim())
}

/// Seeded from current shipped worker templates (iterations/budget/states) plus
/// supervisor timeout floors. Babysitter attachment is None — templates currently
/// omit `:BABYSITTER_WORKER:`. Sandbox is None at kind level (template harness
/// pins remain template-owned until cutover).
pub fn kind_defaults(kind: WorkerKind) -> GovernanceDefaults {
    match kind {
        WorkerKind::Implementer => defaults(Some(20), Some(150_000), BASE_STATES),
        WorkerKind::Reviewer => defaults(Some(10), Some(150_000), REVIEWER_STATES),
        WorkerKind::Architector => defaults(Some(14), Some(140_000), BASE_STATES),
        WorkerKind::Planner => defaults(Some(12), Some(120_000), BASE_STATES),
        WorkerKind::Artifactor => defaults(Some(20), Some(150_000), BASE_STATES),
        WorkerKind::Griller => defaults(Some(10), Some(100_000), BASE_STATES),
        WorkerKind::Babysitter => defaults(None, Some(80_000), BASE_STATES),
        // Not seeded by this task's kind list; keep a conservative floor.
        WorkerKind::Analyzer | WorkerKind::Glossarist | WorkerKind::Manager => {
            defaults(None, None, BASE_STATES)
        }
    }
}

fn defaults(
    max_iterations: Option<u32>,
    context_budget: Option<u32>,
    states: &[&str],
) -> GovernanceDefaults {
    GovernanceDefaults {
        max_iterations,
        context_budget,
        stall_timeout_secs: Some(DEFAULT_STALL_TIMEOUT_SECS),
        max_run_duration_secs: Some(DEFAULT_MAX_RUN_DURATION_SECS),
        applicable_states: states.iter().map(|s| (*s).to_string()).collect(),
        linked_skills: Vec::new(),
        babysitter: None,
        sandbox_permissions: None,
    }
}

impl GovernanceDefaults {
    fn apply_patch(&mut self, patch: &GovernancePatch) {
        if let Some(v) = patch.max_iterations {
            self.max_iterations = Some(v);
        }
        if let Some(v) = patch.context_budget {
            self.context_budget = Some(v);
        }
        if let Some(v) = patch.stall_timeout_secs {
            self.stall_timeout_secs = Some(v);
        }
        if let Some(v) = patch.max_run_duration_secs {
            self.max_run_duration_secs = Some(v);
        }
        if let Some(ref states) = patch.applicable_states {
            self.applicable_states = states.clone();
        }
        if let Some(ref skills) = patch.linked_skills {
            self.linked_skills = skills.clone();
        }
        if let Some(babysitter) = &patch.babysitter {
            self.babysitter = babysitter.clone();
        }
        if let Some(ref sandbox_patch) = patch.sandbox_permissions {
            let mut list = self.sandbox_permissions.clone().unwrap_or_default();
            sandbox_patch.merge_into(&mut list);
            self.sandbox_permissions = Some(list);
        }
    }
}

/// Resolve governance with overlay precedence.
pub fn resolve_governance(
    kind: WorkerKind,
    harness: Option<&str>,
    overlay: &DispatchGovernanceOverlay,
    dispatch_override: Option<&GovernancePatch>,
) -> GovernanceDefaults {
    let mut resolved = kind_defaults(kind);
    if let Some(patch) = overlay.for_kind(kind) {
        resolved.apply_patch(patch);
    }
    if let Some(harness) = harness.map(str::trim).filter(|h| !h.is_empty()) {
        if let Some(patch) = overlay.for_kind_harness(kind, harness) {
            resolved.apply_patch(patch);
        }
    }
    if let Some(patch) = dispatch_override {
        resolved.apply_patch(patch);
    }
    resolved
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GovernanceKeyParseError {
    InvalidKind,
    EmptyHarness,
    ExtraComma,
}

impl std::fmt::Display for GovernanceKeyParseError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::InvalidKind => write!(f, "unknown worker kind"),
            Self::EmptyHarness => write!(f, "empty harness after comma"),
            Self::ExtraComma => write!(f, "extra comma in kind,harness key"),
        }
    }
}

/// Normalize a `dispatch:` map key to its canonical spelling (trimmed kind or
/// `kind,harness`). Malformed keys return an error instead of being stored under
/// an unreachable raw spelling.
pub fn normalize_governance_key(key: &str) -> Result<String, GovernanceKeyParseError> {
    let trimmed = key.trim();
    if let Some(comma_pos) = trimmed.find(',') {
        let kind_s = trimmed[..comma_pos].trim();
        let rest = trimmed[comma_pos + 1..].trim();
        if rest.contains(',') {
            return Err(GovernanceKeyParseError::ExtraComma);
        }
        if rest.is_empty() {
            return Err(GovernanceKeyParseError::EmptyHarness);
        }
        let kind =
            WorkerKind::from_str(kind_s).map_err(|_| GovernanceKeyParseError::InvalidKind)?;
        Ok(kind_harness_key(kind, rest))
    } else {
        let kind =
            WorkerKind::from_str(trimmed).map_err(|_| GovernanceKeyParseError::InvalidKind)?;
        Ok(kind.as_str().to_string())
    }
}

/// Parse a `dispatch:` map key as kind or `(kind,harness)`.
pub fn parse_governance_key(key: &str) -> Option<(WorkerKind, Option<String>)> {
    let canonical = normalize_governance_key(key).ok()?;
    if let Some((kind_s, harness)) = canonical.split_once(',') {
        let kind = WorkerKind::from_str(kind_s).ok()?;
        Some((kind, Some(harness.to_string())))
    } else {
        let kind = WorkerKind::from_str(&canonical).ok()?;
        Some((kind, None))
    }
}

pub fn is_governance_overlay_key(key: &str) -> bool {
    normalize_governance_key(key).is_ok()
}

pub fn known_governance_patch_keys() -> &'static [&'static str] {
    &[
        "max_iterations",
        "context_budget",
        "stall_timeout_secs",
        "max_run_duration_secs",
        "applicable_states",
        "linked_skills",
        "babysitter",
        "sandbox_permissions",
    ]
}

/// Deserialize tri-state babysitter overlay: absent field = inherit (`None`),
/// JSON/YAML null = disable (`Some(None)`), object = explicit address.
fn deserialize_babysitter_patch<'de, D>(
    deserializer: D,
) -> Result<Option<Option<BabysitterAddress>>, D::Error>
where
    D: Deserializer<'de>,
{
    let value = serde_json::Value::deserialize(deserializer)?;
    match value {
        serde_json::Value::Null => Ok(Some(None)),
        other => BabysitterAddress::deserialize(other)
            .map(|addr| Some(Some(addr)))
            .map_err(serde::de::Error::custom),
    }
}

fn serialize_babysitter_patch<S>(
    value: &Option<Option<BabysitterAddress>>,
    serializer: S,
) -> Result<S::Ok, S::Error>
where
    S: Serializer,
{
    match value {
        None => serializer.serialize_none(),
        Some(None) => serializer.serialize_none(),
        Some(Some(addr)) => addr.serialize(serializer),
    }
}

pub fn known_sandbox_permission_keys() -> &'static [&'static str] {
    &[
        "allow_exec",
        "allow_patch",
        "allow_network",
        "allow_writes_outside_cwd",
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    fn patch_iterations(n: u32) -> GovernancePatch {
        GovernancePatch {
            max_iterations: Some(n),
            ..GovernancePatch::default()
        }
    }

    #[test]
    fn implementer_defaults_match_shipped_templates() {
        let d = kind_defaults(WorkerKind::Implementer);
        assert_eq!(d.max_iterations, Some(20));
        assert_eq!(d.context_budget, Some(150_000));
        assert_eq!(d.stall_timeout_secs, Some(600));
        assert_eq!(d.max_run_duration_secs, Some(14_400));
        assert_eq!(
            d.applicable_states,
            vec!["working", "done", "blocked", "cancelled"]
        );
        assert!(d.linked_skills.is_empty());
        assert!(d.babysitter.is_none());
        assert!(d.sandbox_permissions.is_none());
    }

    #[test]
    fn reviewer_defaults_include_verdict_states() {
        let d = kind_defaults(WorkerKind::Reviewer);
        assert_eq!(d.max_iterations, Some(10));
        assert!(d.applicable_states.iter().any(|s| s == "requested_changes"));
    }

    #[test]
    fn babysitter_defaults_omit_max_iterations() {
        let d = kind_defaults(WorkerKind::Babysitter);
        assert_eq!(d.max_iterations, None);
        assert_eq!(d.context_budget, Some(80_000));
    }

    #[test]
    fn overlay_precedence_code_kind_harness_dispatch() {
        let mut map = BTreeMap::new();
        map.insert("implementer".into(), patch_iterations(30));
        map.insert("implementer,codex".into(), patch_iterations(40));
        let overlay = DispatchGovernanceOverlay::from_map(map);

        let code_only = resolve_governance(WorkerKind::Implementer, None, &overlay, None);
        // kind overlay applies even without harness
        assert_eq!(code_only.max_iterations, Some(30));

        let empty = DispatchGovernanceOverlay::default();
        let from_code = resolve_governance(WorkerKind::Implementer, Some("codex"), &empty, None);
        assert_eq!(from_code.max_iterations, Some(20));

        let kind_only = resolve_governance(WorkerKind::Implementer, Some("claude"), &overlay, None);
        assert_eq!(kind_only.max_iterations, Some(30));

        let kind_harness =
            resolve_governance(WorkerKind::Implementer, Some("codex"), &overlay, None);
        assert_eq!(kind_harness.max_iterations, Some(40));

        let dispatch_wins = resolve_governance(
            WorkerKind::Implementer,
            Some("codex"),
            &overlay,
            Some(&patch_iterations(50)),
        );
        assert_eq!(dispatch_wins.max_iterations, Some(50));
    }

    #[test]
    fn absent_overlay_key_keeps_code_default() {
        let overlay = DispatchGovernanceOverlay::default();
        let resolved = resolve_governance(WorkerKind::Planner, Some("claude"), &overlay, None);
        assert_eq!(resolved.max_iterations, Some(12));
        assert_eq!(resolved.context_budget, Some(120_000));
    }

    #[test]
    fn kind_harness_key_format() {
        assert_eq!(
            kind_harness_key(WorkerKind::Reviewer, "cursor-agent"),
            "reviewer,cursor-agent"
        );
        assert!(is_governance_overlay_key("implementer"));
        assert!(is_governance_overlay_key("implementer,codex"));
        assert!(!is_governance_overlay_key("not-a-kind"));
        assert!(!is_governance_overlay_key("implementer,"));
        assert!(!is_governance_overlay_key("implementer,codex,typo"));
    }

    #[test]
    fn normalize_governance_key_trims_and_canonicalizes() {
        assert_eq!(
            normalize_governance_key(" implementer ").unwrap(),
            "implementer"
        );
        assert_eq!(
            normalize_governance_key("implementer, codex").unwrap(),
            "implementer,codex"
        );
        assert_eq!(
            normalize_governance_key(" implementer , codex ").unwrap(),
            "implementer,codex"
        );
        assert_eq!(
            normalize_governance_key("implementer,codex,typo"),
            Err(GovernanceKeyParseError::ExtraComma)
        );
    }

    #[test]
    fn sandbox_patch_merges_per_field_across_layers() {
        let mut map = BTreeMap::new();
        map.insert(
            "implementer".into(),
            GovernancePatch {
                sandbox_permissions: Some(SandboxPermissionsPatch {
                    allow_exec: None,
                    allow_patch: None,
                    allow_network: Some(false),
                    allow_writes_outside_cwd: None,
                }),
                ..GovernancePatch::default()
            },
        );
        map.insert(
            "implementer,codex".into(),
            GovernancePatch {
                sandbox_permissions: Some(SandboxPermissionsPatch {
                    allow_exec: Some(false),
                    allow_patch: None,
                    allow_network: None,
                    allow_writes_outside_cwd: None,
                }),
                ..GovernancePatch::default()
            },
        );
        let overlay = DispatchGovernanceOverlay::from_map(map);

        let dispatch_patch = GovernancePatch {
            sandbox_permissions: Some(SandboxPermissionsPatch {
                allow_exec: None,
                allow_patch: Some(false),
                allow_network: None,
                allow_writes_outside_cwd: None,
            }),
            ..GovernancePatch::default()
        };

        let resolved = resolve_governance(
            WorkerKind::Implementer,
            Some("codex"),
            &overlay,
            Some(&dispatch_patch),
        );
        let sandbox = resolved
            .sandbox_permissions
            .expect("merged sandbox permissions");
        assert!(!sandbox.allow_exec, "kind,harness layer");
        assert!(!sandbox.allow_patch, "dispatch layer");
        assert!(!sandbox.allow_network, "kind layer must survive");
        assert!(sandbox.allow_writes_outside_cwd, "untouched default field");
    }

    #[test]
    fn babysitter_patch_absent_field_inherits() {
        let patch: GovernancePatch = serde_json::from_str(r#"{"max_iterations": 5}"#).unwrap();
        assert_eq!(patch.babysitter, None);
    }

    #[test]
    fn babysitter_patch_null_disables() {
        let patch: GovernancePatch = serde_json::from_str(r#"{"babysitter": null}"#).unwrap();
        assert_eq!(patch.babysitter, Some(None));
    }

    #[test]
    fn babysitter_patch_object_sets_explicit_address() {
        let patch: GovernancePatch = serde_json::from_str(
            r#"{"babysitter": {"mode": "acp-stdio", "harness": "codex", "model": "gpt-5"}}"#,
        )
        .unwrap();
        assert_eq!(
            patch.babysitter,
            Some(Some(BabysitterAddress {
                mode: "acp-stdio".into(),
                harness: "codex".into(),
                harness_args: Vec::new(),
                model: Some("gpt-5".into()),
                effort: None,
            }))
        );
    }

    #[test]
    fn babysitter_patch_yaml_null_disables() {
        let patch: GovernancePatch = serde_yaml::from_str(
            r#"
babysitter: null
max_iterations: 7
"#,
        )
        .unwrap();
        assert_eq!(patch.babysitter, Some(None));
        assert_eq!(patch.max_iterations, Some(7));
    }

    #[test]
    fn babysitter_patch_disable_overrides_lower_layer_address() {
        let mut map = BTreeMap::new();
        map.insert(
            "implementer".into(),
            GovernancePatch {
                babysitter: Some(Some(BabysitterAddress {
                    mode: "tmux".into(),
                    harness: "codex".into(),
                    ..BabysitterAddress::default()
                })),
                ..GovernancePatch::default()
            },
        );
        let overlay = DispatchGovernanceOverlay::from_map(map);
        let dispatch_patch: GovernancePatch =
            serde_json::from_str(r#"{"babysitter": null}"#).unwrap();
        let resolved = resolve_governance(
            WorkerKind::Implementer,
            Some("codex"),
            &overlay,
            Some(&dispatch_patch),
        );
        assert!(resolved.babysitter.is_none());
    }

    #[test]
    fn sandbox_overlay_cannot_widen_lower_layer_restriction() {
        let mut map = BTreeMap::new();
        map.insert(
            "implementer".into(),
            GovernancePatch {
                sandbox_permissions: Some(SandboxPermissionsPatch {
                    allow_exec: None,
                    allow_patch: None,
                    allow_network: Some(false),
                    allow_writes_outside_cwd: None,
                }),
                ..GovernancePatch::default()
            },
        );
        let overlay = DispatchGovernanceOverlay::from_map(map);
        let dispatch_widen = GovernancePatch {
            sandbox_permissions: Some(SandboxPermissionsPatch {
                allow_exec: None,
                allow_patch: None,
                allow_network: Some(true),
                allow_writes_outside_cwd: None,
            }),
            ..GovernancePatch::default()
        };
        let resolved = resolve_governance(
            WorkerKind::Implementer,
            Some("codex"),
            &overlay,
            Some(&dispatch_widen),
        );
        let sandbox = resolved.sandbox_permissions.expect("sandbox permissions");
        assert!(
            !sandbox.allow_network,
            "dispatch true must not widen kind false"
        );
    }
}
