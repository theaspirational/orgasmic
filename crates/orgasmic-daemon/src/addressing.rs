// orgasmic:TASK-SZEWA, dec_WDR5K
//! Dispatch addressing by `(kind, mode, harness)` — transport registry authority.
//!
//! `orgasmic_drivers::SUPPORTED` is the sole supported `(mode, harness)` matrix.
//! Compatibility labels (`worker_id` strings on runs/tx) are not routing authority.

use orgasmic_core::WorkerKind;
use orgasmic_drivers::validate_supported_pair as drivers_validate_supported_pair;

use crate::governance::{
    resolve_governance, DispatchGovernanceOverlay, GovernanceDefaults, GovernancePatch,
};

/// Validate that `(mode, harness)` is in the sole transport registry.
pub fn validate_supported_pair(mode: &str, harness: &str) -> Result<(), String> {
    drivers_validate_supported_pair(mode, harness)
}

/// Historical/compat run label — never used as routing authority.
pub fn compatibility_worker_id(kind: WorkerKind, mode: &str, harness: &str) -> String {
    format!("{}-{}-{}", kind.as_str(), harness.trim(), mode.trim())
}

/// Resolve governance with the documented precedence for a dispatch address.
pub fn resolve_address_governance(
    kind: WorkerKind,
    harness: &str,
    overlay: &DispatchGovernanceOverlay,
    dispatch_override: Option<&GovernancePatch>,
) -> GovernanceDefaults {
    resolve_governance(kind, Some(harness.trim()), overlay, dispatch_override)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn supported_pair_accepted() {
        validate_supported_pair("acp-stdio", "codex").unwrap();
        validate_supported_pair("rmux", "custom").unwrap();
    }

    #[test]
    fn unsupported_pair_rejected() {
        let err = validate_supported_pair("tmux", "custom").unwrap_err();
        assert!(err.contains("unsupported mode/harness"));
        assert!(err.contains("supported:"));
    }

    #[test]
    fn empty_fields_rejected() {
        assert!(validate_supported_pair("", "codex").is_err());
        assert!(validate_supported_pair("acp-stdio", "").is_err());
    }

    #[test]
    fn compatibility_label_is_not_routing_authority() {
        let id = compatibility_worker_id(WorkerKind::Implementer, "acp-stdio", "cursor-agent");
        assert_eq!(id, "implementer-cursor-agent-acp-stdio");
    }
}
