//! Codex sandbox approval helpers for orgasmic driver modes.
//!
//! `allowlist_from_driver_config` extracts the `sandbox_permissions` string
//! from a `DriverConfig`, parses the core CSV allowlist format, and falls back
//! to the core trust-all default when the key is absent.
//! `ApprovalResponse::Approved` and `ApprovalResponse::Denied` are the
//! driver's per-request decision values before they are serialized back to the
//! sandbox client.
//! The shared JSON-RPC dispatch loop in `crates/orgasmic-drivers/src/modes/jsonrpc.rs`
//! calls this path through `try_dispatch_approval`.

pub use orgasmic_core::{SandboxAllowlist, SandboxAllowlistParseError};

use orgasmic_core::DriverEvent;
use serde_json::{json, Value};

use crate::r#trait::DriverConfig;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ApprovalResponse {
    Approved,
    Denied,
    Selected { option_id: String },
}

pub fn allowlist_from_driver_config(
    config: &DriverConfig,
) -> Result<SandboxAllowlist, SandboxAllowlistParseError> {
    match config.0.get("sandbox_permissions").and_then(Value::as_str) {
        None => Ok(SandboxAllowlist::default()),
        Some(csv) => SandboxAllowlist::from_csv(csv),
    }
}

pub fn approval_document_events(
    method: &str,
    params: &Value,
    request_id: &Value,
    decision: ApprovalResponse,
    seq: u64,
) -> Vec<DriverEvent> {
    let call_id = request_id.to_string();
    let label = match &decision {
        ApprovalResponse::Approved => "Approved",
        ApprovalResponse::Denied => "Denied",
        ApprovalResponse::Selected { option_id } => option_id.as_str(),
    };
    vec![
        DriverEvent::ToolCall {
            call_id: call_id.clone(),
            name: method.to_string(),
            args: params.clone(),
            seq,
        },
        DriverEvent::ToolResult {
            call_id,
            ok: approval_ok(&decision),
            output: json!(label),
            seq: seq + 1,
        },
    ]
}

pub fn approval_result(decision: ApprovalResponse) -> Value {
    let mut value = json!({
        "decision": match &decision {
            ApprovalResponse::Approved => "accept",
            ApprovalResponse::Denied => "decline",
            ApprovalResponse::Selected { .. } => "selected",
        }
    });
    if let ApprovalResponse::Selected { option_id } = decision {
        value["outcome"] = json!("selected");
        value["optionId"] = json!(option_id);
    }
    value
}

fn approval_ok(decision: &ApprovalResponse) -> bool {
    match decision {
        ApprovalResponse::Approved => true,
        ApprovalResponse::Denied => false,
        ApprovalResponse::Selected { option_id } => option_id.starts_with("allow"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn allowlist_from_driver_config_reads_csv_property() {
        let config = DriverConfig::from_value(json!({
            "sandbox_permissions": "allow_exec=false,allow_patch=true,allow_network=true,allow_writes_outside_cwd=true",
        }));
        let list = allowlist_from_driver_config(&config).unwrap();
        assert!(!list.allow_exec);
        assert!(list.allow_patch);
    }

    #[test]
    fn allowlist_from_driver_config_rejects_malformed_csv() {
        let config = DriverConfig::from_value(json!({
            "sandbox_permissions": "allow_exec=maybe",
        }));
        let err = allowlist_from_driver_config(&config).unwrap_err();
        assert_eq!(
            err,
            SandboxAllowlistParseError::InvalidBool {
                key: "allow_exec".into(),
                value: "maybe".into(),
            }
        );
    }

    #[test]
    fn allowlist_from_driver_config_rejects_bare_key_without_equals() {
        let config = DriverConfig::from_value(json!({
            "sandbox_permissions": "allow_exec",
        }));
        let err = allowlist_from_driver_config(&config).unwrap_err();
        assert_eq!(
            err,
            SandboxAllowlistParseError::UnknownKey("allow_exec".into())
        );
    }
}
