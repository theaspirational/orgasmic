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
use serde::Deserialize;

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

/// Fully resolved governance values for a (kind[, harness]) lookup.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GovernanceDefaults {
    pub max_iterations: Option<u32>,
    pub context_budget: Option<u32>,
    pub stall_timeout_secs: Option<u32>,
    pub max_run_duration_secs: Option<u32>,
    pub applicable_states: Vec<String>,
    pub linked_skills: Vec<String>,
    pub babysitter_worker: Option<String>,
    pub sandbox_permissions: Option<SandboxAllowlist>,
}

/// Sparse patch applied over defaults (config overlay or per-dispatch override).
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
#[serde(default)]
pub struct GovernancePatch {
    pub max_iterations: Option<u32>,
    pub context_budget: Option<u32>,
    pub stall_timeout_secs: Option<u32>,
    pub max_run_duration_secs: Option<u32>,
    pub applicable_states: Option<Vec<String>>,
    pub linked_skills: Option<Vec<String>>,
    pub babysitter_worker: Option<String>,
    pub sandbox_permissions: Option<SandboxPermissionsPatch>,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
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
    fn to_allowlist(&self) -> SandboxAllowlist {
        let mut list = SandboxAllowlist::default();
        if let Some(v) = self.allow_exec {
            list.allow_exec = v;
        }
        if let Some(v) = self.allow_patch {
            list.allow_patch = v;
        }
        if let Some(v) = self.allow_network {
            list.allow_network = v;
        }
        if let Some(v) = self.allow_writes_outside_cwd {
            list.allow_writes_outside_cwd = v;
        }
        list
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
        babysitter_worker: None,
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
        if let Some(ref babysitter) = patch.babysitter_worker {
            self.babysitter_worker = Some(babysitter.clone());
        }
        if let Some(ref sandbox) = patch.sandbox_permissions {
            self.sandbox_permissions = Some(sandbox.to_allowlist());
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

/// Parse a `dispatch:` map key as kind or `(kind,harness)`.
pub fn parse_governance_key(key: &str) -> Option<(WorkerKind, Option<&str>)> {
    if let Some((kind_s, harness)) = key.split_once(',') {
        let kind = WorkerKind::from_str(kind_s.trim()).ok()?;
        let harness = harness.trim();
        if harness.is_empty() {
            return None;
        }
        Some((kind, Some(harness)))
    } else {
        let kind = WorkerKind::from_str(key.trim()).ok()?;
        Some((kind, None))
    }
}

pub fn is_governance_overlay_key(key: &str) -> bool {
    parse_governance_key(key).is_some()
}

pub fn known_governance_patch_keys() -> &'static [&'static str] {
    &[
        "max_iterations",
        "context_budget",
        "stall_timeout_secs",
        "max_run_duration_secs",
        "applicable_states",
        "linked_skills",
        "babysitter_worker",
        "sandbox_permissions",
    ]
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
        assert!(d.babysitter_worker.is_none());
        assert!(d.sandbox_permissions.is_none());
    }

    #[test]
    fn reviewer_defaults_include_verdict_states() {
        let d = kind_defaults(WorkerKind::Reviewer);
        assert_eq!(d.max_iterations, Some(10));
        assert!(d
            .applicable_states
            .iter()
            .any(|s| s == "requested_changes"));
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

        let kind_only = resolve_governance(
            WorkerKind::Implementer,
            Some("claude"),
            &overlay,
            None,
        );
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
    }
}
