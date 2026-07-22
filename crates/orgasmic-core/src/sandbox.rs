//! Sandbox permission allowlists model which codex sandbox approval requests
//! orgasmic may auto-grant for a task or dispatch-governance layer.
//!
//! `SandboxAllowlist` carries the four driver-facing permission switches:
//! `allow_exec`, `allow_patch`, `allow_network`, and
//! `allow_writes_outside_cwd`.
//! Its `Default` is intentionally trust-all so existing dispatches keep running
//! unless a task or governance layer narrows permissions.
//! `from_csv` accepts comma-separated `key=true|false` entries for those four
//! keys, and `resolve(task, governance)` intersects task restrictions with the
//! governance allowlist (least privilege: task may only further restrict).

use std::str::FromStr;

use serde::Serialize;
use thiserror::Error;

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct SandboxAllowlist {
    pub allow_exec: bool,
    pub allow_patch: bool,
    pub allow_network: bool,
    pub allow_writes_outside_cwd: bool,
}

impl Default for SandboxAllowlist {
    fn default() -> Self {
        Self {
            allow_exec: true,
            allow_patch: true,
            allow_network: true,
            allow_writes_outside_cwd: true,
        }
    }
}

#[derive(Debug, Error, PartialEq, Eq)]
pub enum SandboxAllowlistParseError {
    #[error("unknown sandbox permission key: {0}")]
    UnknownKey(String),
    #[error("invalid boolean for {key}: {value}")]
    InvalidBool { key: String, value: String },
    #[error("empty sandbox permissions entry")]
    Empty,
}

impl SandboxAllowlist {
    /// Effective permissions are the field-wise AND of governance and
    /// task layers. Any `false` at a restriction layer stays false.
    pub fn resolve(task: Option<&Self>, governance: Option<&Self>) -> Self {
        let base = governance.cloned().unwrap_or_default();
        let Some(task) = task else {
            return base;
        };
        Self {
            allow_exec: base.allow_exec && task.allow_exec,
            allow_patch: base.allow_patch && task.allow_patch,
            allow_network: base.allow_network && task.allow_network,
            allow_writes_outside_cwd: base.allow_writes_outside_cwd
                && task.allow_writes_outside_cwd,
        }
    }

    pub fn from_csv(input: &str) -> Result<Self, SandboxAllowlistParseError> {
        let trimmed = input.trim();
        if trimmed.is_empty() {
            return Err(SandboxAllowlistParseError::Empty);
        }
        let mut allowlist = Self::default();
        for part in trimmed.split(',') {
            let part = part.trim();
            if part.is_empty() {
                continue;
            }
            let (key, value) = part
                .split_once('=')
                .ok_or_else(|| SandboxAllowlistParseError::UnknownKey(part.to_string()))?;
            let key = key.trim();
            let value = value.trim();
            let parsed =
                bool::from_str(value).map_err(|_| SandboxAllowlistParseError::InvalidBool {
                    key: key.to_string(),
                    value: value.to_string(),
                })?;
            match key {
                "allow_exec" => allowlist.allow_exec = parsed,
                "allow_patch" => allowlist.allow_patch = parsed,
                "allow_network" => allowlist.allow_network = parsed,
                "allow_writes_outside_cwd" => allowlist.allow_writes_outside_cwd = parsed,
                other => return Err(SandboxAllowlistParseError::UnknownKey(other.to_string())),
            }
        }
        Ok(allowlist)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_is_trust_all() {
        let list = SandboxAllowlist::default();
        assert!(list.allow_exec);
        assert!(list.allow_patch);
        assert!(list.allow_network);
        assert!(list.allow_writes_outside_cwd);
    }

    #[test]
    fn resolve_intersects_task_with_governance_without_widening() {
        let governance = SandboxAllowlist {
            allow_exec: true,
            allow_patch: true,
            allow_network: false,
            allow_writes_outside_cwd: true,
        };
        let task = SandboxAllowlist {
            allow_exec: false,
            allow_patch: true,
            allow_network: true,
            allow_writes_outside_cwd: false,
        };
        let resolved = SandboxAllowlist::resolve(Some(&task), Some(&governance));
        assert!(!resolved.allow_exec, "task may restrict exec");
        assert!(resolved.allow_patch);
        assert!(
            !resolved.allow_network,
            "governance network=false must not widen"
        );
        assert!(
            !resolved.allow_writes_outside_cwd,
            "task may restrict writes"
        );
    }

    #[test]
    fn resolve_falls_back_to_governance_then_default() {
        let governance = SandboxAllowlist {
            allow_exec: false,
            allow_patch: true,
            allow_network: true,
            allow_writes_outside_cwd: true,
        };
        assert_eq!(
            SandboxAllowlist::resolve(None, Some(&governance)),
            governance
        );
        assert_eq!(
            SandboxAllowlist::resolve(None, None),
            SandboxAllowlist::default()
        );
    }

    #[test]
    fn from_csv_parses_key_value_pairs() {
        let list = SandboxAllowlist::from_csv(
            "allow_exec=false,allow_patch=true,allow_network=false,allow_writes_outside_cwd=true",
        )
        .unwrap();
        assert!(!list.allow_exec);
        assert!(list.allow_patch);
        assert!(!list.allow_network);
        assert!(list.allow_writes_outside_cwd);
    }

    #[test]
    fn task_heading_override_wins_over_runtime_default_in_resolve() {
        use crate::org::OrgFile;
        use crate::schema::TaskHeading;

        let task_source = "* IN_PROGRESS TASK-079 Example\n:PROPERTIES:\n:ID: TASK-079\n:SANDBOX_PERMISSIONS: allow_exec=false,allow_patch=true,allow_network=true,allow_writes_outside_cwd=true\n:END:\n";
        let task_file = OrgFile::parse(task_source, "backlog.org").unwrap();
        let task = TaskHeading::from_heading(
            &task_file,
            task_file.headings.first().expect("task heading"),
            "backlog.org",
        )
        .unwrap();
        let runtime_default = SandboxAllowlist::default();
        let resolved =
            SandboxAllowlist::resolve(task.sandbox_permissions.as_ref(), Some(&runtime_default));
        assert!(!resolved.allow_exec);
        assert!(resolved.allow_patch);
    }
}
