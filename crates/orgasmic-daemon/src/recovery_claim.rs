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

#[cfg(unix)]
use std::os::unix::fs::OpenOptionsExt;

#[cfg(unix)]
use libc;

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
    /// When the replacement session already exists, reattach instead of acquire.
    pub reattach_existing: bool,
}

#[derive(Debug, Clone, PartialEq)]
pub enum ResolvedRecoveryClaim {
    Valid(RecoveryClaim),
    Reconstructed(RecoveryClaim),
    InvalidQuarantined,
    Missing,
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

fn claim_path(
    home: &Home,
    project_id: &str,
    origin_run_id: &str,
) -> Result<PathBuf, RecoveryClaimError> {
    if !validate_safe_component(project_id) || !validate_safe_component(origin_run_id) {
        return Err(RecoveryClaimError::InvalidIdentifier);
    }
    Ok(recovery_claims_root(home)
        .join(project_id)
        .join(format!("{origin_run_id}.json")))
}

fn sync_path(path: &Path) -> std::io::Result<()> {
    let file = open_claim_file_read(path)?;
    file.sync_all()?;
    if let Some(parent) = path.parent() {
        if parent.is_dir() {
            sync_directory(parent)?;
        }
    }
    Ok(())
}

#[cfg(unix)]
fn open_claim_file_read(path: &Path) -> std::io::Result<File> {
    OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW)
        .open(path)
}

#[cfg(not(unix))]
fn open_claim_file_read(path: &Path) -> std::io::Result<File> {
    OpenOptions::new().read(true).open(path)
}

#[cfg(unix)]
fn sync_directory(path: &Path) -> std::io::Result<()> {
    let dir = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW)
        .open(path)?;
    dir.sync_all()
}

#[cfg(not(unix))]
fn sync_directory(path: &Path) -> std::io::Result<()> {
    OpenOptions::new().read(true).open(path)?.sync_all()
}

fn claim_path_is_safe_regular_file(path: &Path) -> bool {
    let Ok(meta) = std::fs::symlink_metadata(path) else {
        return false;
    };
    meta.is_file()
}

fn write_claim_atomic(home: &Home, claim: &RecoveryClaim) -> Result<(), RecoveryClaimError> {
    let path = claim_path(home, &claim.project_id, &claim.origin_run_id)?;
    let dir = path.parent().ok_or(RecoveryClaimError::InvalidIdentifier)?;
    std::fs::create_dir_all(dir).map_err(RecoveryClaimError::Io)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
        let _ = std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700));
        let tmp = path.with_extension("json.tmp");
        {
            let mut file = OpenOptions::new()
                .write(true)
                .create_new(true)
                .mode(0o600)
                .open(&tmp)
                .map_err(RecoveryClaimError::Io)?;
            file.write_all(serde_json::to_string_pretty(claim).unwrap().as_bytes())
                .map_err(RecoveryClaimError::Io)?;
            file.sync_all().map_err(RecoveryClaimError::Io)?;
        }
        std::fs::rename(&tmp, &path).map_err(RecoveryClaimError::Io)?;
        sync_path(&path).map_err(RecoveryClaimError::Io)?;
        let _ = std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600));
    }
    #[cfg(not(unix))]
    {
        let tmp = path.with_extension("json.tmp");
        {
            let mut file = OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&tmp)
                .map_err(RecoveryClaimError::Io)?;
            file.write_all(serde_json::to_string_pretty(claim).unwrap().as_bytes())
                .map_err(RecoveryClaimError::Io)?;
            file.sync_all().map_err(RecoveryClaimError::Io)?;
        }
        std::fs::rename(&tmp, &path).map_err(RecoveryClaimError::Io)?;
        sync_path(&path).map_err(RecoveryClaimError::Io)?;
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
    if !claim_path_is_safe_regular_file(&path) {
        return Err(RecoveryClaimError::CorruptClaim);
    }
    let raw = std::fs::read_to_string(&path).map_err(RecoveryClaimError::Io)?;
    let claim: RecoveryClaim =
        serde_json::from_str(&raw).map_err(|_| RecoveryClaimError::CorruptClaim)?;
    if claim.project_id != project_id || claim.origin_run_id != origin_run_id {
        return Err(RecoveryClaimError::CorruptClaim);
    }
    Ok(Some(claim))
}

pub fn quarantine_invalid_claim(
    home: &Home,
    project_id: &str,
    origin_run_id: &str,
) -> Result<(), RecoveryClaimError> {
    let path = claim_path(home, project_id, origin_run_id)?;
    if !path.exists() {
        return Ok(());
    }
    let quarantine = path.with_extension("json.quarantine");
    if quarantine.exists() {
        std::fs::remove_file(&quarantine).map_err(RecoveryClaimError::Io)?;
    }
    std::fs::rename(&path, &quarantine).map_err(RecoveryClaimError::Io)?;
    Ok(())
}

#[derive(Debug, Clone)]
pub struct IndexedRecoveryOrigin {
    pub project_id: String,
    pub origin_run_id: String,
    pub request_id: String,
    pub replacement_run_id: String,
    pub replacement_session_path: PathBuf,
    pub action: String,
    pub target: Option<String>,
    pub origin_session_path: PathBuf,
}

pub fn index_recovery_origins_in_session(
    envelopes: &[SessionEnvelope],
    _session_path: &Path,
) -> Vec<IndexedRecoveryOrigin> {
    let mut links = Vec::new();
    for envelope in envelopes {
        if envelope.kind != SessionEventKind::Lifecycle {
            continue;
        }
        let Ok(lifecycle) = serde_json::from_value::<Lifecycle>(envelope.event.clone()) else {
            continue;
        };
        if let Lifecycle::RecoveryOrigin {
            project_id,
            origin_run_id,
            request_id,
            replacement_run_id,
            replacement_session_path,
            action,
            target,
            origin_session_path,
        } = lifecycle
        {
            links.push(IndexedRecoveryOrigin {
                project_id,
                origin_run_id,
                request_id,
                replacement_run_id,
                replacement_session_path,
                action,
                target,
                origin_session_path,
            });
        }
    }
    links
}

pub fn reconstruct_claim_from_origin(link: &IndexedRecoveryOrigin, boot_id: &str) -> RecoveryClaim {
    RecoveryClaim {
        project_id: link.project_id.clone(),
        origin_run_id: link.origin_run_id.clone(),
        request_id: link.request_id.clone(),
        status: RecoveryClaimStatus::Committed,
        replacement_run_id: link.replacement_run_id.clone(),
        replacement_session_path: link.replacement_session_path.clone(),
        replacement_runtime_id: String::new(),
        runtime_id: None,
        boot_id: Some(boot_id.to_string()),
        action: Some(link.action.clone()),
        target: link.target.clone().or_else(|| Some("worker".to_string())),
        draft_prompt: None,
    }
}

pub fn resolve_authoritative_recovery_claim(
    home: &Home,
    project_id: &str,
    origin_run_id: &str,
    indexed_origins: &[IndexedRecoveryOrigin],
    boot_id: &str,
) -> Result<ResolvedRecoveryClaim, RecoveryClaimError> {
    if let Some(claim) = load_recovery_claim(home, project_id, origin_run_id)? {
        if claim.status == RecoveryClaimStatus::Committed {
            if verify_committed_claim_against_session(&claim) {
                return Ok(ResolvedRecoveryClaim::Valid(claim));
            }
            quarantine_invalid_claim(home, project_id, origin_run_id)?;
            if let Some(link) = indexed_origins
                .iter()
                .find(|link| link.project_id == project_id && link.origin_run_id == origin_run_id)
            {
                let mut reconstructed = reconstruct_claim_from_origin(link, boot_id);
                if let Ok(envelopes) = orgasmic_core::session::read_session_file(
                    &reconstructed.replacement_session_path,
                ) {
                    if let Some(first) = envelopes.first() {
                        reconstructed.replacement_runtime_id = first.runtime_id.clone();
                        reconstructed.runtime_id = Some(first.runtime_id.clone());
                    }
                }
                write_claim_atomic(home, &reconstructed)?;
                return Ok(ResolvedRecoveryClaim::Reconstructed(reconstructed));
            }
            return Ok(ResolvedRecoveryClaim::InvalidQuarantined);
        }
        return Ok(ResolvedRecoveryClaim::Valid(claim));
    }
    if let Some(link) = indexed_origins
        .iter()
        .find(|link| link.project_id == project_id && link.origin_run_id == origin_run_id)
    {
        let mut reconstructed = reconstruct_claim_from_origin(link, boot_id);
        if let Ok(envelopes) =
            orgasmic_core::session::read_session_file(&reconstructed.replacement_session_path)
        {
            if let Some(first) = envelopes.first() {
                reconstructed.replacement_runtime_id = first.runtime_id.clone();
                reconstructed.runtime_id = Some(first.runtime_id.clone());
            }
        }
        write_claim_atomic(home, &reconstructed)?;
        return Ok(ResolvedRecoveryClaim::Reconstructed(reconstructed));
    }
    Ok(ResolvedRecoveryClaim::Missing)
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

#[allow(clippy::too_many_arguments)]
pub fn plan_pending_recovery_claim(
    home: &Home,
    project_id: &str,
    origin_run_id: &str,
    request_id: &str,
    _origin_session_path: &Path,
    replacement_session_path: PathBuf,
    boot_id: &str,
    action: &str,
    target: &str,
    draft_prompt: Option<String>,
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
        action: Some(action.to_string()),
        target: Some(target.to_string()),
        draft_prompt,
    };
    write_claim_atomic(home, &claim)?;
    let planned_identity =
        RuntimeIdentity::planned(replacement_run_id, replacement_runtime_id, boot_id);
    Ok(PendingRecoveryPlan {
        claim,
        planned_identity,
        reattach_existing: false,
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
    let Ok(envelopes) = orgasmic_core::session::read_session_file(&claim.replacement_session_path)
    else {
        return false;
    };
    let Some((replacement_run_id, replacement_session_path, action)) = recovery_origin_in_session(
        &envelopes,
        &claim.project_id,
        &claim.origin_run_id,
        &claim.request_id,
    ) else {
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
    let planned_identity = RuntimeIdentity::planned(
        claim.replacement_run_id.clone(),
        claim.replacement_runtime_id.clone(),
        boot_id,
    );
    if !claim.replacement_session_path.exists() {
        return Ok(Some(PendingRecoveryPlan {
            claim: claim.clone(),
            planned_identity,
            reattach_existing: false,
        }));
    }
    let Ok(envelopes) = orgasmic_core::session::read_session_file(&claim.replacement_session_path)
    else {
        return Ok(Some(PendingRecoveryPlan {
            claim: claim.clone(),
            planned_identity,
            reattach_existing: false,
        }));
    };
    if let Some((_, _, action)) = recovery_origin_in_session(
        &envelopes,
        &claim.project_id,
        &claim.origin_run_id,
        &claim.request_id,
    ) {
        let link_target = envelopes.iter().rev().find_map(|envelope| {
            if envelope.kind != SessionEventKind::Lifecycle {
                return None;
            }
            match serde_json::from_value::<Lifecycle>(envelope.event.clone()).ok()? {
                Lifecycle::RecoveryOrigin { target, .. } => target,
                _ => None,
            }
        });
        let details = CommitRecoveryDetails {
            runtime_id: claim.replacement_runtime_id.clone(),
            boot_id: boot_id.to_string(),
            action: claim.action.clone().unwrap_or(action),
            target: claim
                .target
                .clone()
                .or(link_target)
                .unwrap_or_else(|| "worker".to_string()),
            draft_prompt: claim.draft_prompt.clone(),
        };
        let committed =
            commit_recovery_claim(home, &claim.project_id, &claim.origin_run_id, details)?;
        return Ok(Some(PendingRecoveryPlan {
            claim: committed,
            planned_identity,
            reattach_existing: false,
        }));
    }
    let has_acquire = envelopes.iter().any(|envelope| {
        envelope.kind == SessionEventKind::Lifecycle
            && envelope.event.get("phase").and_then(|phase| phase.as_str()) == Some("acquire")
    });
    Ok(Some(PendingRecoveryPlan {
        claim: claim.clone(),
        planned_identity,
        reattach_existing: has_acquire,
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
    project_id: &str,
    origin_run_id: &str,
) -> Arc<tokio::sync::Mutex<()>> {
    let key = format!("{project_id}:{origin_run_id}");
    let mut map = locks.lock().unwrap();
    map.entry(key)
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
            "start_recovery_run",
            "worker",
            None,
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

    #[test]
    fn claims_live_under_daemon_home_not_project_tmp() {
        let tmp = tempfile::tempdir().unwrap();
        let home = Home::at(tmp.path().join("home"));
        home.ensure().unwrap();
        let root = recovery_claims_root(&home);
        assert!(root.starts_with(home.state()));
        assert!(!root.to_string_lossy().contains(".orgasmic/tmp"));
    }

    #[test]
    fn verify_rejects_forged_committed_claim_without_session_link() {
        let tmp = tempfile::tempdir().unwrap();
        let home = Home::at(tmp.path().join("home"));
        home.ensure().unwrap();
        let project_root = tmp.path().join("proj");
        let replacement_path = project_sessions_dir(&project_root).join("recover-forged.jsonl");
        std::fs::create_dir_all(replacement_path.parent().unwrap()).unwrap();
        std::fs::write(&replacement_path, "{}\n").unwrap();
        let claim = RecoveryClaim {
            project_id: "orgasmic".into(),
            origin_run_id: "run-origin".into(),
            request_id: "req-forged".into(),
            status: RecoveryClaimStatus::Committed,
            replacement_run_id: "run-replacement".into(),
            replacement_session_path: replacement_path,
            replacement_runtime_id: "rt-replacement".into(),
            runtime_id: Some("rt-replacement".into()),
            boot_id: Some("boot-new".into()),
            action: Some("start_recovery_run".into()),
            target: Some("worker".into()),
            draft_prompt: None,
        };
        assert!(!verify_committed_claim_against_session(&claim));
    }

    #[test]
    fn reconcile_pending_commits_when_recovery_origin_exists() {
        let tmp = tempfile::tempdir().unwrap();
        let home = Home::at(tmp.path().join("home"));
        home.ensure().unwrap();
        let project_root = tmp.path().join("proj");
        let origin_path = project_sessions_dir(&project_root).join("run-origin.jsonl");
        std::fs::create_dir_all(origin_path.parent().unwrap()).unwrap();
        std::fs::write(&origin_path, "{}\n").unwrap();
        let replacement_path = project_sessions_dir(&project_root).join("recover-pending.jsonl");
        let plan = plan_pending_recovery_claim(
            &home,
            "orgasmic",
            "run-origin",
            "req-pending",
            &origin_path,
            replacement_path.clone(),
            "boot-plan",
            "start_recovery_run",
            "worker",
            None,
        )
        .unwrap();
        let identity = RuntimeIdentity {
            run_id: plan.claim.replacement_run_id.clone(),
            runtime_id: plan.claim.replacement_runtime_id.clone(),
            boot_id: "boot-old".into(),
        };
        let mut writer = orgasmic_core::SessionWriter::open(&replacement_path, identity).unwrap();
        writer
            .append(
                orgasmic_core::session::SessionEventKind::Lifecycle,
                serde_json::to_value(orgasmic_core::session::Lifecycle::RecoveryOrigin {
                    project_id: "orgasmic".into(),
                    origin_run_id: "run-origin".into(),
                    origin_session_path: origin_path,
                    request_id: "req-pending".into(),
                    replacement_run_id: plan.claim.replacement_run_id.clone(),
                    replacement_session_path: replacement_path,
                    action: "start_recovery_run".into(),
                    target: Some("worker".into()),
                })
                .unwrap(),
            )
            .unwrap();
        drop(writer);

        let plan = reconcile_pending_claim(&home, &plan.claim, "boot-restart")
            .unwrap()
            .expect("pending with existing origin link reconciles");
        assert_eq!(plan.claim.status, RecoveryClaimStatus::Committed);
    }
}
