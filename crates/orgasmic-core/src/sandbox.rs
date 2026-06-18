//! Sandbox permission allowlists model which codex sandbox approval requests
//! orgasmic may auto-grant for a task or worker.
//!
//! `SandboxAllowlist` carries the four driver-facing permission switches:
//! `allow_exec`, `allow_patch`, `allow_network`, and
//! `allow_writes_outside_cwd`.
//! Its `Default` is intentionally trust-all so existing workers keep running
//! unless a task or worker narrows permissions.
//! `from_csv` accepts comma-separated `key=true|false` entries for those four
//! keys, and `resolve(task, worker)` applies task > worker > default precedence.

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
    pub fn resolve(task: Option<&Self>, worker: Option<&Self>) -> Self {
        task.cloned()
            .or_else(|| worker.cloned())
            .unwrap_or_default()
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
    fn resolve_prefers_task_over_worker() {
        let worker = SandboxAllowlist {
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
        let resolved = SandboxAllowlist::resolve(Some(&task), Some(&worker));
        assert_eq!(resolved, task);
    }

    #[test]
    fn resolve_falls_back_to_worker_then_default() {
        let worker = SandboxAllowlist {
            allow_exec: false,
            allow_patch: true,
            allow_network: true,
            allow_writes_outside_cwd: true,
        };
        assert_eq!(SandboxAllowlist::resolve(None, Some(&worker)), worker);
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
    fn task_heading_override_wins_over_worker_in_resolve() {
        use crate::org::OrgFile;
        use crate::schema::TaskHeading;
        use crate::workers::Worker;

        let worker_source = "* WORKER implementer-codex-stdio\n:PROPERTIES:\n:ID: implementer-codex-stdio\n:KIND:             implementer\n:DRIVER: acp-stdio\n:HARNESS: codex\n:SANDBOX_PERMISSIONS: allow_exec=true,allow_patch=true,allow_network=true,allow_writes_outside_cwd=true\n:END:\n";
        let task_source = "* IN_PROGRESS TASK-079 Example\n:PROPERTIES:\n:ID: TASK-079\n:WORKER: implementer-codex-stdio\n:SANDBOX_PERMISSIONS: allow_exec=false,allow_patch=true,allow_network=true,allow_writes_outside_cwd=true\n:END:\n";
        let worker_file = OrgFile::parse(worker_source, "worker.org").unwrap();
        let task_file = OrgFile::parse(task_source, "backlog.org").unwrap();
        let worker = Worker::from_org(&worker_file, "worker.org").unwrap();
        let task = TaskHeading::from_heading(
            &task_file,
            task_file.headings.first().expect("task heading"),
            "backlog.org",
        )
        .unwrap();
        let resolved = SandboxAllowlist::resolve(
            task.sandbox_permissions.as_ref(),
            worker.sandbox_permissions.as_ref(),
        );
        assert!(!resolved.allow_exec);
        assert!(resolved.allow_patch);
    }
}
