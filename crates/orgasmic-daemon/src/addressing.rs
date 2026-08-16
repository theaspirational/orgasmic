// orgasmic:TASK-SZEWA, dec_WDR5K
//! Dispatch addressing by `(kind, mode, harness)` — transport registry authority.
//!
//! `orgasmic_drivers::SUPPORTED` is the sole supported `(mode, harness)` matrix.
//! Compatibility labels (`worker_id` strings on runs/tx) are not routing authority.

use orgasmic_core::WorkerKind;
use orgasmic_drivers::validate_supported_pair as drivers_validate_supported_pair;
use std::path::Path;

use crate::governance::{
    resolve_governance, DispatchGovernanceOverlay, GovernanceDefaults, GovernancePatch,
};

/// Validate that `(mode, harness)` is in the sole transport registry.
pub fn validate_supported_pair(mode: &str, harness: &str) -> Result<(), String> {
    // orgasmic:task_3NJ9K
    // Test builds also address the in-process stub transport, which the driver
    // registry deliberately does not list — nothing outside a test build can
    // name it. A test that drives a stage or dispatch endpoint has to get past
    // this check to reach the code it is about, and the alternative is the
    // address it used before: a real harness the endpoint would then exec.
    #[cfg(test)]
    if (mode.trim(), harness.trim())
        == (
            crate::driver_resolution::STUB_MODE,
            crate::driver_resolution::STUB_HARNESS,
        )
    {
        return Ok(());
    }
    drivers_validate_supported_pair(mode, harness)
}

/// Raw argv tokens are valid only for the custom harness.
pub fn validate_address_harness_args(harness: &str, harness_args: &[String]) -> Result<(), String> {
    if harness == "custom" || harness_args.is_empty() {
        return Ok(());
    }
    Err(format!(
        "harness_args are only valid for custom harness; got {} args for {harness}",
        harness_args.len()
    ))
}

/// Historical/compat run label — never used as routing authority.
pub fn compatibility_worker_id(kind: WorkerKind, mode: &str, harness: &str) -> String {
    format!("{}-{}-{}", kind.as_str(), harness.trim(), mode.trim())
}

/// Provider whose existing dispatch address can use the canonical RunDock
/// runtime. Address validation remains owned by `SUPPORTED`; this helper only
/// selects the runtime after the caller has accepted that legacy address.
pub fn dispatch_chat_provider(
    _mode: &str,
    harness: &str,
    harness_args: &[String],
) -> Option<&'static str> {
    match harness.trim().to_ascii_lowercase().as_str() {
        "codex" => Some("codex"),
        "claude" => Some("claude"),
        "custom"
            if harness_args.first().is_some_and(|argument| {
                Path::new(argument)
                    .file_name()
                    .and_then(|name| name.to_str())
                    .is_some_and(|name| name.eq_ignore_ascii_case("opencode"))
            }) =>
        {
            Some("opencode")
        }
        _ => None,
    }
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
        validate_supported_pair("stdio", "codex").unwrap();
        validate_supported_pair("tmux", "custom").unwrap();
    }

    #[test]
    fn unsupported_pair_rejected() {
        let err = validate_supported_pair("stdio", "custom").unwrap_err();
        assert!(err.contains("unsupported mode/harness"));
        assert!(err.contains("supported:"));
    }

    #[test]
    fn empty_fields_rejected() {
        assert!(validate_supported_pair("", "codex").is_err());
        assert!(validate_supported_pair("stdio", "").is_err());
    }

    #[test]
    fn compatibility_label_is_not_routing_authority() {
        let id = compatibility_worker_id(WorkerKind::Implementer, "stdio", "cursor-agent");
        assert_eq!(id, "implementer-cursor-agent-stdio");
    }

    #[test]
    fn harness_args_rejected_for_builtin_harness() {
        let err = validate_address_harness_args("codex", &["--flag".into()]).unwrap_err();
        assert!(err.contains("harness_args are only valid for custom harness"));
    }

    #[test]
    fn harness_args_allowed_for_custom_harness() {
        validate_address_harness_args("custom", &["opencode".into(), "--print-logs".into()])
            .unwrap();
    }

    #[test]
    fn dispatch_chat_provider_maps_builtin_codex_and_claude_addresses() {
        assert_eq!(dispatch_chat_provider("stdio", "codex", &[]), Some("codex"));
        assert_eq!(
            dispatch_chat_provider("tmux", "claude", &[]),
            Some("claude")
        );
    }

    #[test]
    fn dispatch_chat_provider_recognizes_opencode_custom_executable_only() {
        assert_eq!(
            dispatch_chat_provider(
                "tmux",
                "custom",
                &["/opt/homebrew/bin/opencode".into(), "--print-logs".into()],
            ),
            Some("opencode")
        );
        assert_eq!(
            dispatch_chat_provider("tmux", "custom", &["aider".into()]),
            None
        );
        assert_eq!(
            dispatch_chat_provider("subprocess-stream-json", "cursor-agent", &[]),
            None
        );
    }
}
