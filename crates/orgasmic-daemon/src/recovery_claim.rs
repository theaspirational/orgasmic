// orgasmic:task_A6FGF, task_QPKCD, task_6ZTFM
//! Daemon-owned, project-scoped recovery claims for Failed tombstone rescue idempotency.

use std::collections::HashMap;
use std::fs::{File, OpenOptions};
use std::io::Write;
use std::path::{Component, Path, PathBuf};
use std::sync::{Arc, Mutex};

use orgasmic_core::home::Home;
use orgasmic_core::session::{Lifecycle, SessionEnvelope, SessionEventKind};
use orgasmic_core::RuntimeIdentity;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum RecoveryClaimStatus {
    Pending,
    Committed,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct RecoveryClaim {
    pub project_id: String,
    pub origin_run_id: String,
    pub request_id: String,
    pub status: RecoveryClaimStatus,
    pub replacement_run_id: String,
    pub replacement_session_path: PathBuf,
    pub replacement_runtime_id: String,
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

#[derive(Debug, Clone)]
pub struct PendingRecoveryPlan {
    pub claim: RecoveryClaim,
    pub planned_identity: RuntimeIdentity,
}

#[derive(Debug, Clone)]
pub struct CommitRecoveryDetails {
    pub runtime_id: String,
    pub boot_id: String,
    pub action: String,
    pub target: String,
    pub draft_prompt: Option<String>,
}

pub fn recovery_claims_root(home: &Home) -> PathBuf {
    home.state().join("recovery-claims")
}

pub fn validate_safe_component(value: &str) -> bool {
    !value.is_empty()
        && !value.contains('/')
        && !value.contains('\\')
        && value != "."
        && value != ".."
        && Path::new(value)
            .components()
            .all(|c| matches!(c, Component::Normal(_)))
}

fn claim_path(home: &Home, project_id: &str, origin_run_id: &str) -> Result<PathBuf, RecoveryClaimError> {
    if !validate_safe_component(project_id) || !validate_safe_component(origin_run_id) {
        return Err(RecoveryClaimError::InvalidIdentifier);
    }
    Ok(recovery_claims_root(home)
        .join(project_id)
        .join(format!("{origin_run_id}.json")))
}

fn sync_path(path: &Path) -> std::io::Result<()> {
    let file = OpenOptions::new().read(true).open(path)?;
    file.sync_all()?;
    if let Some(parent) = path.parent() {
        if parent.exists() {
            if let Ok(dir) = File::open(parent) {
                let _ = dir.sync_all();
            }
        }
    }
    Ok(())
}

fn write_claim_atomic(home: &Home, claim: &RecoveryClaim) -> Result<(), RecoveryClaimError> {
    let path = claim_path(home, &claim.project_id, &claim.origin_run_id)?;
    let dir = path.parent().ok_or(RecoveryClaimError::InvalidIdentifier)?;
    std::fs::create_dir_all(dir).map_err(RecoveryClaimError::Io)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700));
    }
    let tmp = path.with_extension("json.tmp");
    {
        let mut file = OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .open(&tmp)
            .map_err(RecoveryClaimError::Io)?;
        file.write_all(serde_json::to_string_pretty(claim).unwrap().as_bytes())
            .map_err(RecoveryClaimError::Io)?;
        file.sync_all().map_err(RecoveryClaimError::Io)?;
    }
    std::fs::rename(&tmp, &path).map_err(RecoveryClaimError::Io)?;
    sync_path(&path).map_err(RecoveryClaimError::Io)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600));
    }
    Ok(())
}

pub fn load_recovery_claim(
    home: &Home,
    project_id: &str,
    origin_run_id: &str,
) -> Result<Option<RecoveryClaim>, RecoveryClaimError> {
    let path = claim_path(home, project_id, origin_run_id)?;
    if !path.exists() {
        return Ok(None);
    }
    let raw = std::fs::read_to_string(&path).map_err(RecoveryClaimError::Io)?;
    let claim = serde_json::from_str(&raw).map_err(|_| RecoveryClaimError::CorruptClaim)?;
    Ok(Some(claim))
}

pub fn load_committed_recovery_claim(
    home: &Home,
    project_id: &str,
    origin_run_id: &str,
) -> Result<Option<RecoveryClaim>, RecoveryClaimError> {
    let Some(claim) = load_recovery_claim(home, project_id, origin_run_id)? else {
        return Ok(None);
    };
    if claim.status != RecoveryClaimStatus::Committed {
        return Ok(None);
    }
    Ok(Some(claim))
}

pub fn remove_recovery_claim(
    home: &Home,
    project_id: &str,
    origin_run_id: &str,
) -> Result<(), RecoveryClaimError> {
    let path = claim_path(home, project_id, origin_run_id)?;
    if path.exists() {
        std::fs::remove_file(&path).map_err(RecoveryClaimError::Io)?;
    }
    Ok(())
}

pub fn plan_pending_recovery_claim(
    home: &Home,
    project_id: &str,
    origin_run_id: &str,
    request_id: &str,
    origin_session_path: &Path,
    replacement_session_path: PathBuf,
    boot_id: &str,
) -> Result<PendingRecoveryPlan, RecoveryClaimError> {
    if !validate_safe_component(request_id) {
        return Err(RecoveryClaimError::InvalidIdentifier);
    }
    if let Some(existing) = load_recovery_claim(home, project_id, origin_run_id)? {
        return Err(RecoveryClaimError::AlreadyClaimed(Box::new(existing)));
    }
    let replacement_uuid = uuid::Uuid::new_v4();
    let replacement_run_id = format!(
        "run-{}-{}",
        chrono::Utc::now().format("%Y%m%dT%H%M%S"),
        replacement_uuid.simple()
    );
    let replacement_runtime_id = uuid::Uuid::new_v4().to_string();
    let claim = RecoveryClaim {
        project_id: project_id.to_string(),
        origin_run_id: origin_run_id.to_string(),
        request_id: request_id.to_string(),
        status: RecoveryClaimStatus::Pending,
        replacement_run_id: replacement_run_id.clone(),
        replacement_session_path: replacement_session_path.clone(),
        replacement_runtime_id: replacement_runtime_id.clone(),
        runtime_id: None,
        boot_id: None,
        action: None,
        target: None,
        draft_prompt: None,
    };
    write_claim_atomic(home, &claim)?;
    let planned_identity = RuntimeIdentity::planned(
        replacement_run_id,
        replacement_runtime_id,
        boot_id,
    );
    Ok(PendingRecoveryPlan {
        claim,
        planned_identity,
    })
}

pub fn commit_recovery_claim(
    home: &Home,
    project_id: &str,
    origin_run_id: &str,
    details: CommitRecoveryDetails,
) -> Result<RecoveryClaim, RecoveryClaimError> {
    let mut claim = load_recovery_claim(home, project_id, origin_run_id)?
        .ok_or(RecoveryClaimError::MissingClaim)?;
    claim.status = RecoveryClaimStatus::Committed;
    claim.runtime_id = Some(details.runtime_id);
    claim.boot_id = Some(details.boot_id);
    claim.action = Some(details.action);
    claim.target = Some(details.target);
    claim.draft_prompt = details.draft_prompt;
    write_claim_atomic(home, &claim)?;
    Ok(claim)
}

pub fn recovery_origin_in_session(
    envelopes: &[SessionEnvelope],
    project_id: &str,
    origin_run_id: &str,
    request_id: &str,
) -> Option<(String, PathBuf, String)> {
    envelopes.iter().rev().find_map(|envelope| {
        if envelope.kind != SessionEventKind::Lifecycle {
            return None;
        }
        match serde_json::from_value::<Lifecycle>(envelope.event.clone()).ok()? {
            Lifecycle::RecoveryOrigin {
                project_id: link_project,
                origin_run_id: link_origin,
                request_id: link_request,
                replacement_run_id,
                replacement_session_path,
                action,
                ..
            } if link_project == project_id
                && link_origin == origin_run_id
                && link_request == request_id =>
            {
                Some((replacement_run_id, replacement_session_path, action))
            }
            _ => None,
        }
    })
}

pub fn verify_committed_claim_against_session(claim: &RecoveryClaim) -> bool {
    if !claim.replacement_session_path.exists() {
        return false;
    }
    let Ok(envelopes) =
        orgasmic_core::session::read_session_file(&claim.replacement_session_path)
    else {
        return false;
    };
    let Some((replacement_run_id, replacement_session_path, action)) =
        recovery_origin_in_session(
            &envelopes,
            &claim.project_id,
            &claim.origin_run_id,
            &claim.request_id,
        )
    else {
        return false;
    };
    claim.replacement_run_id == replacement_run_id
        && claim.replacement_session_path == replacement_session_path
        && claim.action.as_deref() == Some(action.as_str())
}

pub fn reconcile_pending_claim(
    home: &Home,
    claim: &RecoveryClaim,
    boot_id: &str,
) -> Result<Option<PendingRecoveryPlan>, RecoveryClaimError> {
    if claim.status != RecoveryClaimStatus::Pending {
        return Ok(None);
    }
    if !claim.replacement_session_path.exists() {
        return Ok(Some(PendingRecoveryPlan {
            claim: claim.clone(),
            planned_identity: RuntimeIdentity::planned(
                claim.replacement_run_id.clone(),
                claim.replacement_runtime_id.clone(),
                boot_id,
            ),
        }));
    }
    let Ok(envelopes) = orgasmic_core::session::read_session_file(&claim.replacement_session_path)
    else {
        return Ok(Some(PendingRecoveryPlan {
            claim: claim.clone(),
            planned_identity: RuntimeIdentity::planned(
                claim.replacement_run_id.clone(),
                claim.replacement_runtime_id.clone(),
                boot_id,
            ),
        }));
    };
    if recovery_origin_in_session(
        &envelopes,
        &claim.project_id,
        &claim.origin_run_id,
        &claim.request_id,
    )
    .is_some()
    {
        let details = CommitRecoveryDetails {
            runtime_id: claim.replacement_runtime_id.clone(),
            boot_id: boot_id.to_string(),
            action: claim
                .action
                .clone()
                .unwrap_or_else(|| "start_recovery_run".to_string()),
            target: claim.target.clone().unwrap_or_else(|| "worker".to_string()),
            draft_prompt: claim.draft_prompt.clone(),
        };
        let committed = commit_recovery_claim(
            home,
            &claim.project_id,
            &claim.origin_run_id,
            details,
        )?;
        return Ok(Some(PendingRecoveryPlan {
            claim: committed,
            planned_identity: RuntimeIdentity::planned(
                claim.replacement_run_id.clone(),
                claim.replacement_runtime_id.clone(),
                boot_id,
            ),
        }));
    }
    Ok(Some(PendingRecoveryPlan {
        claim: claim.clone(),
        planned_identity: RuntimeIdentity::planned(
            claim.replacement_run_id.clone(),
            claim.replacement_runtime_id.clone(),
            boot_id,
        ),
    }))
}

#[derive(Debug)]
pub enum RecoveryClaimError {
    InvalidIdentifier,
    UnresolvableProjectRoot,
    AlreadyClaimed(Box<RecoveryClaim>),
    CorruptClaim,
    MissingClaim,
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
        let home = Home::at(tmp.path().join("home"));
        home.ensure().unwrap();
        let project_root = tmp.path().join("proj");
        let session_path = project_sessions_dir(&project_root).join("run-origin.jsonl");
        std::fs::create_dir_all(session_path.parent().unwrap()).unwrap();
        std::fs::write(&session_path, "{}\n").unwrap();
        let replacement_path = project_sessions_dir(&project_root).join("recover-uuid.jsonl");

        let plan = plan_pending_recovery_claim(
            &home,
            "orgasmic",
            "run-origin",
            "req-1",
            &session_path,
            replacement_path.clone(),
            "boot-new",
        )
        .unwrap();
        assert_eq!(plan.claim.status, RecoveryClaimStatus::Pending);

        commit_recovery_claim(
            &home,
            "orgasmic",
            "run-origin",
            CommitRecoveryDetails {
                runtime_id: plan.claim.replacement_runtime_id.clone(),
                boot_id: "boot-new".into(),
                action: "start_recovery_run".into(),
                target: "worker".into(),
                draft_prompt: Some("draft".into()),
            },
        )
        .unwrap();

        let committed = load_committed_recovery_claim(&home, "orgasmic", "run-origin")
            .unwrap()
            .unwrap();
        assert_eq!(committed.request_id, "req-1");
        assert_eq!(committed.replacement_run_id, plan.claim.replacement_run_id);
    }

    #[test]
    fn rejects_traversal_in_identifiers() {
        let tmp = tempfile::tempdir().unwrap();
        let home = Home::at(tmp.path().join("home"));
        home.ensure().unwrap();
        assert!(matches!(
            claim_path(&home, "../evil", "run"),
            Err(RecoveryClaimError::InvalidIdentifier)
        ));
        assert!(matches!(
            claim_path(&home, "orgasmic", "../run"),
            Err(RecoveryClaimError::InvalidIdentifier)
        ));
    }
}
