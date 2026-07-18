// orgasmic:task_A6FGF, task_QPKCD
//! Durable per-origin recovery claims for Failed tombstone rescue idempotency.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use orgasmic_core::project_tmp_dir;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum RecoveryClaimStatus {
    Pending,
    Committed,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct RecoveryClaim {
    pub origin_run_id: String,
    pub request_id: String,
    pub status: RecoveryClaimStatus,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub replacement_run_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub replacement_session_path: Option<PathBuf>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub runtime_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub boot_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub action: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub target: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub draft_prompt: Option<String>,
}

pub fn recovery_claims_dir(project_root: &Path) -> PathBuf {
    project_tmp_dir(project_root).join("recovery-claims")
}

fn claim_path(project_root: &Path, origin_run_id: &str) -> PathBuf {
    recovery_claims_dir(project_root).join(format!("{origin_run_id}.json"))
}

/// Resolve the project root from a per-project session JSONL path.
pub fn project_root_from_session_path(session_path: &Path) -> Option<PathBuf> {
    session_path
        .parent()?
        .parent()?
        .parent()?
        .parent()
        .map(Path::to_path_buf)
}

pub fn load_recovery_claim(session_path: &Path, origin_run_id: &str) -> Option<RecoveryClaim> {
    let project_root = project_root_from_session_path(session_path)?;
    let path = claim_path(&project_root, origin_run_id);
    let raw = std::fs::read_to_string(path).ok()?;
    serde_json::from_str(&raw).ok()
}

pub fn load_committed_recovery_claim(
    session_path: &Path,
    origin_run_id: &str,
) -> Option<RecoveryClaim> {
    load_recovery_claim(session_path, origin_run_id).filter(|claim| {
        claim.status == RecoveryClaimStatus::Committed && claim.replacement_run_id.is_some()
    })
}

fn write_claim_atomic(project_root: &Path, claim: &RecoveryClaim) -> std::io::Result<()> {
    let dir = recovery_claims_dir(project_root);
    std::fs::create_dir_all(&dir)?;
    let path = claim_path(project_root, &claim.origin_run_id);
    let tmp = path.with_extension("json.tmp");
    std::fs::write(&tmp, serde_json::to_string_pretty(claim).unwrap())?;
    std::fs::rename(tmp, path)
}

pub fn remove_recovery_claim(session_path: &Path, origin_run_id: &str) -> std::io::Result<()> {
    let Some(project_root) = project_root_from_session_path(session_path) else {
        return Ok(());
    };
    let path = claim_path(&project_root, origin_run_id);
    if path.exists() {
        std::fs::remove_file(path)?;
    }
    Ok(())
}

pub fn create_pending_recovery_claim(
    session_path: &Path,
    origin_run_id: &str,
    request_id: &str,
) -> Result<(), RecoveryClaimError> {
    let project_root = project_root_from_session_path(session_path)
        .ok_or(RecoveryClaimError::UnresolvableProjectRoot)?;
    let path = claim_path(&project_root, origin_run_id);
    if path.exists() {
        if let Ok(raw) = std::fs::read_to_string(&path) {
            if let Ok(existing) = serde_json::from_str::<RecoveryClaim>(&raw) {
                return Err(RecoveryClaimError::AlreadyClaimed(existing));
            }
        }
        return Err(RecoveryClaimError::CorruptClaim);
    }
    let claim = RecoveryClaim {
        origin_run_id: origin_run_id.to_string(),
        request_id: request_id.to_string(),
        status: RecoveryClaimStatus::Pending,
        replacement_run_id: None,
        replacement_session_path: None,
        runtime_id: None,
        boot_id: None,
        action: None,
        target: None,
        draft_prompt: None,
    };
    write_claim_atomic(&project_root, &claim).map_err(RecoveryClaimError::Io)?;
    Ok(())
}

pub fn commit_recovery_claim(
    session_path: &Path,
    origin_run_id: &str,
    request_id: &str,
    replacement_run_id: &str,
    replacement_session_path: &Path,
    runtime_id: &str,
    boot_id: &str,
    action: &str,
    target: &str,
    draft_prompt: Option<&str>,
) -> Result<RecoveryClaim, RecoveryClaimError> {
    let project_root = project_root_from_session_path(session_path)
        .ok_or(RecoveryClaimError::UnresolvableProjectRoot)?;
    let claim = RecoveryClaim {
        origin_run_id: origin_run_id.to_string(),
        request_id: request_id.to_string(),
        status: RecoveryClaimStatus::Committed,
        replacement_run_id: Some(replacement_run_id.to_string()),
        replacement_session_path: Some(replacement_session_path.to_path_buf()),
        runtime_id: Some(runtime_id.to_string()),
        boot_id: Some(boot_id.to_string()),
        action: Some(action.to_string()),
        target: Some(target.to_string()),
        draft_prompt: draft_prompt.map(str::to_string),
    };
    write_claim_atomic(&project_root, &claim).map_err(RecoveryClaimError::Io)?;
    Ok(claim)
}

#[derive(Debug)]
pub enum RecoveryClaimError {
    UnresolvableProjectRoot,
    AlreadyClaimed(RecoveryClaim),
    CorruptClaim,
    Io(std::io::Error),
}

pub type RecoveryClaimLocks = Arc<Mutex<HashMap<String, Arc<tokio::sync::Mutex<()>>>>>;

pub fn recovery_origin_lock(
    locks: &RecoveryClaimLocks,
    origin_run_id: &str,
) -> Arc<tokio::sync::Mutex<()>> {
    let mut map = locks.lock().unwrap();
    map.entry(origin_run_id.to_string())
        .or_insert_with(|| Arc::new(tokio::sync::Mutex::new(())))
        .clone()
}

#[cfg(test)]
mod tests {
    use super::*;
    use orgasmic_core::project_sessions_dir;

    #[test]
    fn pending_then_committed_claim_roundtrip() {
        let tmp = tempfile::tempdir().unwrap();
        let project_root = tmp.path().join("proj");
        let session_path = project_sessions_dir(&project_root).join("run-origin.jsonl");
        std::fs::create_dir_all(session_path.parent().unwrap()).unwrap();
        std::fs::write(&session_path, "{}\n").unwrap();

        create_pending_recovery_claim(&session_path, "run-origin", "req-1").unwrap();
        let pending = load_recovery_claim(&session_path, "run-origin").unwrap();
        assert_eq!(pending.status, RecoveryClaimStatus::Pending);

        commit_recovery_claim(
            &session_path,
            "run-origin",
            "req-1",
            "run-replacement",
            &project_sessions_dir(&project_root).join("recover-uuid.jsonl"),
            "rt-new",
            "boot-new",
            "start_recovery_run",
            "worker",
            Some("draft"),
        )
        .unwrap();

        let committed = load_committed_recovery_claim(&session_path, "run-origin").unwrap();
        assert_eq!(committed.request_id, "req-1");
        assert_eq!(
            committed.replacement_run_id.as_deref(),
            Some("run-replacement")
        );
    }
}
