// orgasmic:task_A6FGF, task_QPKCD, task_6ZTFM, task_3TEDA
//! Daemon-owned, project-scoped recovery claims for Failed tombstone rescue idempotency.

use std::collections::HashMap;
use std::fs::{File, OpenOptions};
use std::io::Write;
use std::path::{Component, Path, PathBuf};
use std::sync::{Arc, Mutex};

use orgasmic_core::home::Home;
use orgasmic_core::session::{Lifecycle, SessionEnvelope, SessionEventKind};
use orgasmic_core::RuntimeIdentity;
use orgasmic_drivers::modes::tmux::{tmux_session_exists, tmux_session_name};
use orgasmic_drivers::NativeRuntimeMeta;
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

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct RecoveryRunOptions {
    pub stall_timeout_secs: Option<u32>,
    pub max_run_duration_secs: Option<u32>,
    pub idle_timeout_secs: Option<u32>,
    pub babysitter_target: Option<String>,
    pub cleanup_on_failure: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct RecoveryClaim {
    /// Versioned marker proving that all immutable plan fields were persisted
    /// before spawn. Claims without it are historical/incomplete and fail closed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub plan_version: Option<u32>,
    pub project_id: String,
    pub origin_run_id: String,
    pub request_id: String,
    pub status: RecoveryClaimStatus,
    pub replacement_run_id: String,
    pub replacement_session_path: PathBuf,
    pub replacement_runtime_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub runtime_id: Option<String>,
    /// Daemon boot id pinned at plan time; stable across crash/retry.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub boot_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub action: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub target: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub draft_prompt: Option<String>,
    /// Stable response fields persisted before driver spawn (crash replay).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub origin_session_path: Option<PathBuf>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub planned_tmux_session: Option<String>,
    /// Immutable execution plan — persisted in Pending before spawn.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub task_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub kind: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub worker_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub role: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub requires_worker_finalize: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub transport: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub harness: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub driver_config: Option<serde_json::Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub force_inert: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub worktree: Option<PathBuf>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_path: Option<PathBuf>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stdout_path: Option<PathBuf>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub planned_native_runtime: Option<NativeRuntimeMeta>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub run_options: Option<RecoveryRunOptions>,
}

/// Complete immutable plan persisted before driver spawn.
#[derive(Debug, Clone)]
pub struct PendingRecoveryClaimSpec {
    pub project_id: String,
    pub origin_run_id: String,
    pub request_id: String,
    pub origin_session_path: PathBuf,
    pub replacement_session_path: PathBuf,
    pub boot_id: String,
    pub action: String,
    pub target: String,
    pub draft_prompt: Option<String>,
    pub force_inert: bool,
    pub task_id: String,
    pub kind: String,
    pub worker_id: String,
    pub role: String,
    pub requires_worker_finalize: bool,
    pub transport: String,
    pub harness: Option<String>,
    pub driver_config: serde_json::Value,
    pub worktree: Option<PathBuf>,
    pub last_path: Option<PathBuf>,
    pub stdout_path: Option<PathBuf>,
    pub planned_native_runtime: Option<NativeRuntimeMeta>,
    pub run_options: RecoveryRunOptions,
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

/// Env-triggered failpoints for crash/replay tests (`ORGASMIC_RECOVERY_FAILPOINT`).
/// Comma-separated tokens name durable boundaries such as `pending`,
/// `spawn_before_jsonl`, each `*_append`, `temp_fsync`, `rename`,
/// `parent_fsync`, `commit`, `cleanup`, and `response`.
pub fn recovery_failpoint(point: &str) {
    let Ok(raw) = std::env::var("ORGASMIC_RECOVERY_FAILPOINT") else {
        return;
    };
    if raw.split(',').map(str::trim).any(|token| token == point) {
        panic!("recovery failpoint triggered: {point}");
    }
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

fn recovery_claim_has_complete_plan(claim: &RecoveryClaim) -> bool {
    claim.plan_version == Some(1)
        && claim.runtime_id.as_deref() == Some(claim.replacement_runtime_id.as_str())
        && claim
            .boot_id
            .as_deref()
            .is_some_and(|value| !value.is_empty())
        && claim
            .action
            .as_deref()
            .is_some_and(|value| !value.is_empty())
        && claim
            .target
            .as_deref()
            .is_some_and(|value| !value.is_empty())
        && claim.draft_prompt.is_some()
        && claim.origin_session_path.is_some()
        && claim
            .planned_tmux_session
            .as_deref()
            .is_some_and(|value| !value.is_empty())
        && claim
            .task_id
            .as_deref()
            .is_some_and(|value| !value.is_empty())
        && claim.kind.as_deref().is_some_and(|value| !value.is_empty())
        && claim
            .worker_id
            .as_deref()
            .is_some_and(|value| !value.is_empty())
        && claim.role.as_deref().is_some_and(|value| !value.is_empty())
        && claim.requires_worker_finalize.is_some()
        && claim
            .transport
            .as_deref()
            .is_some_and(|value| !value.is_empty())
        && claim
            .harness
            .as_deref()
            .is_some_and(|value| !value.is_empty())
        && claim.driver_config.is_some()
        && claim.force_inert.is_some()
        && claim.run_options.is_some()
}

#[cfg(any(test, not(unix)))]
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

fn claim_path_is_safe_regular_file(path: &Path) -> bool {
    let Ok(meta) = std::fs::symlink_metadata(path) else {
        return false;
    };
    meta.is_file()
}

#[cfg(unix)]
struct ClaimDirectory {
    file: File,
}

#[cfg(unix)]
impl ClaimDirectory {
    fn open(
        home: &Home,
        project_id: &str,
        create: bool,
    ) -> Result<Option<Self>, RecoveryClaimError> {
        use std::os::fd::{AsRawFd, FromRawFd};

        if !validate_safe_component(project_id) {
            return Err(RecoveryClaimError::InvalidIdentifier);
        }
        // Canonicalize only the daemon-owned state root. Every untrusted
        // component below it is opened relative to retained directory handles
        // with O_NOFOLLOW, so a symlink swap cannot redirect a transaction.
        let state = home
            .state()
            .canonicalize()
            .map_err(RecoveryClaimError::Io)?;
        let mut current = OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC)
            .open(state)
            .map_err(RecoveryClaimError::Io)?;
        for component in ["recovery-claims", project_id] {
            let name = std::ffi::CString::new(component)
                .map_err(|_| RecoveryClaimError::InvalidIdentifier)?;
            let open = || unsafe {
                libc::openat(
                    current.as_raw_fd(),
                    name.as_ptr(),
                    libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC,
                )
            };
            let mut fd = open();
            if fd < 0 {
                let err = std::io::Error::last_os_error();
                if err.kind() == std::io::ErrorKind::NotFound && create {
                    if unsafe { libc::mkdirat(current.as_raw_fd(), name.as_ptr(), 0o700) } != 0 {
                        let mkdir_err = std::io::Error::last_os_error();
                        if mkdir_err.kind() != std::io::ErrorKind::AlreadyExists {
                            return Err(RecoveryClaimError::Io(mkdir_err));
                        }
                    }
                    current.sync_all().map_err(RecoveryClaimError::Io)?;
                    recovery_failpoint("parent_fsync");
                    fd = open();
                } else if err.kind() == std::io::ErrorKind::NotFound {
                    return Ok(None);
                } else {
                    return Err(RecoveryClaimError::CorruptClaim);
                }
            }
            if fd < 0 {
                return Err(RecoveryClaimError::CorruptClaim);
            }
            current = unsafe { File::from_raw_fd(fd) };
            if !current.metadata().map_err(RecoveryClaimError::Io)?.is_dir() {
                return Err(RecoveryClaimError::CorruptClaim);
            }
        }
        Ok(Some(Self { file: current }))
    }

    fn open_file(
        &self,
        name: &str,
        flags: libc::c_int,
        mode: libc::mode_t,
    ) -> std::io::Result<File> {
        use std::os::fd::{AsRawFd, FromRawFd};
        let name = std::ffi::CString::new(name)
            .map_err(|_| std::io::Error::from(std::io::ErrorKind::InvalidInput))?;
        let fd = unsafe {
            libc::openat(
                self.file.as_raw_fd(),
                name.as_ptr(),
                flags | libc::O_NOFOLLOW | libc::O_CLOEXEC,
                libc::c_uint::from(mode),
            )
        };
        if fd < 0 {
            Err(std::io::Error::last_os_error())
        } else {
            Ok(unsafe { File::from_raw_fd(fd) })
        }
    }

    fn read_regular(&self, name: &str) -> Result<String, RecoveryClaimError> {
        use std::io::Read;
        let mut file = self
            .open_file(name, libc::O_RDONLY, 0)
            .map_err(|err| match err.kind() {
                std::io::ErrorKind::NotFound => RecoveryClaimError::Io(err),
                _ => RecoveryClaimError::CorruptClaim,
            })?;
        if !file.metadata().map_err(RecoveryClaimError::Io)?.is_file() {
            return Err(RecoveryClaimError::CorruptClaim);
        }
        let mut raw = String::new();
        file.read_to_string(&mut raw)
            .map_err(RecoveryClaimError::Io)?;
        Ok(raw)
    }

    fn rename(&self, from: &str, to: &str) -> Result<(), RecoveryClaimError> {
        use std::os::fd::AsRawFd;
        let from =
            std::ffi::CString::new(from).map_err(|_| RecoveryClaimError::InvalidIdentifier)?;
        let to = std::ffi::CString::new(to).map_err(|_| RecoveryClaimError::InvalidIdentifier)?;
        if unsafe {
            libc::renameat(
                self.file.as_raw_fd(),
                from.as_ptr(),
                self.file.as_raw_fd(),
                to.as_ptr(),
            )
        } != 0
        {
            return Err(RecoveryClaimError::Io(std::io::Error::last_os_error()));
        }
        Ok(())
    }

    fn remove(&self, name: &str) -> Result<bool, RecoveryClaimError> {
        use std::os::fd::AsRawFd;
        let name =
            std::ffi::CString::new(name).map_err(|_| RecoveryClaimError::InvalidIdentifier)?;
        if unsafe { libc::unlinkat(self.file.as_raw_fd(), name.as_ptr(), 0) } == 0 {
            return Ok(true);
        }
        let err = std::io::Error::last_os_error();
        if err.kind() == std::io::ErrorKind::NotFound {
            Ok(false)
        } else {
            Err(RecoveryClaimError::Io(err))
        }
    }

    fn names(&self) -> Result<Vec<String>, RecoveryClaimError> {
        use std::ffi::CStr;
        use std::os::fd::AsRawFd;
        let duplicate = unsafe { libc::dup(self.file.as_raw_fd()) };
        if duplicate < 0 {
            return Err(RecoveryClaimError::Io(std::io::Error::last_os_error()));
        }
        let dir = unsafe { libc::fdopendir(duplicate) };
        if dir.is_null() {
            unsafe { libc::close(duplicate) };
            return Err(RecoveryClaimError::Io(std::io::Error::last_os_error()));
        }
        let mut names = Vec::new();
        loop {
            let entry = unsafe { libc::readdir(dir) };
            if entry.is_null() {
                break;
            }
            let name = unsafe { CStr::from_ptr((*entry).d_name.as_ptr()) };
            if let Ok(name) = name.to_str() {
                if name != "." && name != ".." {
                    names.push(name.to_string());
                }
            }
        }
        unsafe { libc::closedir(dir) };
        Ok(names)
    }

    fn sync(&self) -> Result<(), RecoveryClaimError> {
        self.file.sync_all().map_err(RecoveryClaimError::Io)
    }
}

fn claim_file_name(origin_run_id: &str) -> Result<String, RecoveryClaimError> {
    if !validate_safe_component(origin_run_id) {
        return Err(RecoveryClaimError::InvalidIdentifier);
    }
    Ok(format!("{origin_run_id}.json"))
}

#[cfg(unix)]
fn reconcile_stale_claim_temp(
    home: &Home,
    project_id: &str,
    origin_run_id: &str,
) -> Result<(), RecoveryClaimError> {
    let Some(dir) = ClaimDirectory::open(home, project_id, false)? else {
        return Ok(());
    };
    let final_name = claim_file_name(origin_run_id)?;
    if dir.read_regular(&final_name).is_ok() {
        return Ok(());
    }
    let prefix = format!("{final_name}.tmp.");
    let mut valid = Vec::new();
    for name in dir
        .names()?
        .into_iter()
        .filter(|name| name.starts_with(&prefix))
    {
        let parsed = dir
            .read_regular(&name)
            .ok()
            .and_then(|raw| serde_json::from_str::<RecoveryClaim>(&raw).ok())
            .filter(|claim| {
                claim.project_id == project_id
                    && claim.origin_run_id == origin_run_id
                    && recovery_claim_has_complete_plan(claim)
            });
        if parsed.is_some() {
            valid.push(name);
        } else {
            let _ = dir.remove(&name);
            recovery_failpoint("cleanup");
        }
    }
    if valid.len() > 1 {
        return Err(RecoveryClaimError::CorruptClaim);
    }
    if let Some(name) = valid.pop() {
        dir.rename(&name, &final_name)?;
        recovery_failpoint("rename");
        dir.sync()?;
        recovery_failpoint("parent_fsync");
    }
    Ok(())
}

#[cfg(unix)]
fn write_claim_atomic(home: &Home, claim: &RecoveryClaim) -> Result<(), RecoveryClaimError> {
    let dir = ClaimDirectory::open(home, &claim.project_id, true)?
        .ok_or(RecoveryClaimError::CorruptClaim)?;
    let final_name = claim_file_name(&claim.origin_run_id)?;
    let tmp_name = format!("{final_name}.tmp.{}", uuid::Uuid::new_v4());
    let result = (|| {
        let mut file = dir
            .open_file(
                &tmp_name,
                libc::O_WRONLY | libc::O_CREAT | libc::O_EXCL,
                0o600,
            )
            .map_err(RecoveryClaimError::Io)?;
        file.write_all(serde_json::to_string_pretty(claim).unwrap().as_bytes())
            .map_err(RecoveryClaimError::Io)?;
        recovery_failpoint("temp_write");
        file.sync_all().map_err(RecoveryClaimError::Io)?;
        recovery_failpoint("temp_fsync");
        dir.rename(&tmp_name, &final_name)?;
        recovery_failpoint("rename");
        dir.sync()?;
        recovery_failpoint("parent_fsync");
        Ok(())
    })();
    if result.is_err() {
        let _ = dir.remove(&tmp_name);
    }
    result
}

#[cfg(not(unix))]
fn reconcile_stale_claim_temp(
    _home: &Home,
    _project_id: &str,
    _origin_run_id: &str,
) -> Result<(), RecoveryClaimError> {
    Ok(())
}

#[cfg(not(unix))]
fn write_claim_atomic(home: &Home, claim: &RecoveryClaim) -> Result<(), RecoveryClaimError> {
    let path = claim_path(home, &claim.project_id, &claim.origin_run_id)?;
    let dir = path.parent().ok_or(RecoveryClaimError::InvalidIdentifier)?;
    std::fs::create_dir_all(dir).map_err(RecoveryClaimError::Io)?;
    let tmp = path.with_extension(format!("json.tmp.{}", uuid::Uuid::new_v4()));
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&tmp)
        .map_err(RecoveryClaimError::Io)?;
    file.write_all(serde_json::to_string_pretty(claim).unwrap().as_bytes())
        .map_err(RecoveryClaimError::Io)?;
    file.sync_all().map_err(RecoveryClaimError::Io)?;
    std::fs::rename(tmp, path).map_err(RecoveryClaimError::Io)
}

pub fn write_claim_atomic_or_reconcile(
    home: &Home,
    claim: &RecoveryClaim,
) -> Result<(), RecoveryClaimError> {
    match write_claim_atomic(home, claim) {
        Ok(()) => Ok(()),
        Err(RecoveryClaimError::Io(err)) if err.kind() == std::io::ErrorKind::AlreadyExists => {
            reconcile_stale_claim_temp(home, &claim.project_id, &claim.origin_run_id)?;
            write_claim_atomic(home, claim)
        }
        Err(other) => Err(other),
    }
}

pub fn load_recovery_claim(
    home: &Home,
    project_id: &str,
    origin_run_id: &str,
) -> Result<Option<RecoveryClaim>, RecoveryClaimError> {
    let name = claim_file_name(origin_run_id)?;
    #[cfg(unix)]
    let raw = {
        let Some(mut dir) = ClaimDirectory::open(home, project_id, false)? else {
            return Ok(None);
        };
        match dir.read_regular(&name) {
            Ok(raw) => raw,
            Err(RecoveryClaimError::Io(err)) if err.kind() == std::io::ErrorKind::NotFound => {
                reconcile_stale_claim_temp(home, project_id, origin_run_id)?;
                let Some(reopened) = ClaimDirectory::open(home, project_id, false)? else {
                    return Ok(None);
                };
                dir = reopened;
                match dir.read_regular(&name) {
                    Ok(raw) => raw,
                    Err(RecoveryClaimError::Io(err))
                        if err.kind() == std::io::ErrorKind::NotFound =>
                    {
                        return Ok(None);
                    }
                    Err(err) => return Err(err),
                }
            }
            Err(err) => return Err(err),
        }
    };
    #[cfg(not(unix))]
    let raw = {
        let path = claim_path(home, project_id, origin_run_id)?;
        if !path.exists() {
            return Ok(None);
        }
        std::fs::read_to_string(path).map_err(RecoveryClaimError::Io)?
    };
    let claim: RecoveryClaim =
        serde_json::from_str(&raw).map_err(|_| RecoveryClaimError::CorruptClaim)?;
    if claim.project_id != project_id || claim.origin_run_id != origin_run_id {
        return Err(RecoveryClaimError::CorruptClaim);
    }
    Ok(Some(claim))
}

/// Routing guard for daemon boot reattach. A pending recovery owns the exact
/// deterministic replacement handle and must be reconciled by POST /recover,
/// which validates the complete plan and backfills lifecycle events in order.
/// Boot's generic reattach pass therefore skips that session instead of
/// inserting a `Reattach` event into the immutable partial prefix.
///
/// This is only a routing hint: recovery authorization still comes from the
/// full handle-bound claim/session verification under the per-origin lock.
pub fn pending_recovery_claim_owns_session(
    home: &Home,
    project_id: &str,
    session_path: &Path,
) -> bool {
    #[cfg(unix)]
    {
        let Ok(Some(dir)) = ClaimDirectory::open(home, project_id, false) else {
            return false;
        };
        let Ok(names) = dir.names() else {
            return false;
        };
        names.into_iter().any(|name| {
            if !name.ends_with(".json") {
                return false;
            }
            dir.read_regular(&name)
                .ok()
                .and_then(|raw| serde_json::from_str::<RecoveryClaim>(&raw).ok())
                .is_some_and(|claim| {
                    claim.status == RecoveryClaimStatus::Pending
                        && claim.project_id == project_id
                        && claim.replacement_session_path == session_path
                        && recovery_claim_has_complete_plan(&claim)
                })
        })
    }
    #[cfg(not(unix))]
    {
        let root = recovery_claims_root(home).join(project_id);
        std::fs::read_dir(root)
            .ok()
            .into_iter()
            .flatten()
            .flatten()
            .any(|entry| {
                std::fs::read_to_string(entry.path())
                    .ok()
                    .and_then(|raw| serde_json::from_str::<RecoveryClaim>(&raw).ok())
                    .is_some_and(|claim| {
                        claim.status == RecoveryClaimStatus::Pending
                            && claim.project_id == project_id
                            && claim.replacement_session_path == session_path
                            && recovery_claim_has_complete_plan(&claim)
                    })
            })
    }
}

pub fn quarantine_invalid_claim(
    home: &Home,
    project_id: &str,
    origin_run_id: &str,
) -> Result<(), RecoveryClaimError> {
    #[cfg(unix)]
    {
        let Some(dir) = ClaimDirectory::open(home, project_id, false)? else {
            return Ok(());
        };
        let name = claim_file_name(origin_run_id)?;
        let quarantine = format!("{name}.quarantine");
        let _ = dir.remove(&quarantine)?;
        match dir.rename(&name, &quarantine) {
            Ok(()) => {
                dir.sync()?;
                recovery_failpoint("parent_fsync");
                Ok(())
            }
            Err(RecoveryClaimError::Io(err)) if err.kind() == std::io::ErrorKind::NotFound => {
                Ok(())
            }
            Err(err) => Err(err),
        }
    }
    #[cfg(not(unix))]
    {
        let path = claim_path(home, project_id, origin_run_id)?;
        if !path.exists() {
            return Ok(());
        }
        let quarantine = path.with_extension("json.quarantine");
        if quarantine.exists() {
            std::fs::remove_file(&quarantine).map_err(RecoveryClaimError::Io)?;
        }
        std::fs::rename(path, quarantine).map_err(RecoveryClaimError::Io)
    }
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
    pub replacement_boot_id: String,
    pub draft_prompt: Option<String>,
    pub claim: RecoveryClaim,
}

fn session_run_meta_project(envelopes: &[SessionEnvelope]) -> Option<String> {
    envelopes.iter().find_map(|envelope| {
        if envelope.kind != SessionEventKind::Lifecycle {
            return None;
        }
        match serde_json::from_value::<Lifecycle>(envelope.event.clone()).ok()? {
            Lifecycle::RunMeta { project_id, .. } => project_id,
            _ => None,
        }
    })
}

fn session_prompt_draft(envelopes: &[SessionEnvelope]) -> Option<String> {
    envelopes.iter().rev().find_map(|envelope| {
        if envelope.kind != SessionEventKind::Lifecycle {
            return None;
        }
        match serde_json::from_value::<Lifecycle>(envelope.event.clone()).ok()? {
            Lifecycle::PromptDraft { text, sent: false } => Some(text),
            _ => None,
        }
    })
}

pub fn index_recovery_origins_in_session(
    envelopes: &[SessionEnvelope],
    session_path: &Path,
    containing_project_id: &str,
) -> Vec<IndexedRecoveryOrigin> {
    let Some(session_parent) = session_path.parent() else {
        return Vec::new();
    };
    let Ok(canonical_parent) = session_parent.canonicalize() else {
        return Vec::new();
    };
    let Ok(session_meta) = std::fs::symlink_metadata(session_path) else {
        return Vec::new();
    };
    if !session_meta.is_file() || session_meta.file_type().is_symlink() {
        return Vec::new();
    }
    let Ok(canonical_session) = session_path.canonicalize() else {
        return Vec::new();
    };
    if canonical_session.parent() != Some(canonical_parent.as_path()) {
        return Vec::new();
    }
    let Some(first) = envelopes.first() else {
        return Vec::new();
    };
    let Some(run_meta_project) = session_run_meta_project(envelopes) else {
        return Vec::new();
    };
    if run_meta_project != containing_project_id {
        return Vec::new();
    }
    let draft_prompt = session_prompt_draft(envelopes);
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
            claim,
        } = lifecycle
        {
            let Some(claim_value) = claim else {
                continue;
            };
            let Ok(claim_snapshot) = serde_json::from_value::<RecoveryClaim>(claim_value) else {
                continue;
            };
            if claim_snapshot.status != RecoveryClaimStatus::Committed
                || !recovery_claim_has_complete_plan(&claim_snapshot)
            {
                continue;
            }
            if envelope.run_id != replacement_run_id {
                continue;
            }
            if envelope.runtime_id != first.runtime_id {
                continue;
            }
            if envelope.boot_id != first.boot_id {
                continue;
            }
            if project_id != containing_project_id {
                continue;
            }
            let Ok(link_canonical_session) = replacement_session_path.canonicalize() else {
                continue;
            };
            if link_canonical_session != canonical_session {
                continue;
            }
            if claim_snapshot.project_id != project_id
                || claim_snapshot.origin_run_id != origin_run_id
                || claim_snapshot.request_id != request_id
                || claim_snapshot.replacement_run_id != replacement_run_id
                || claim_snapshot.replacement_runtime_id != first.runtime_id
                || claim_snapshot.boot_id.as_deref() != Some(first.boot_id.as_str())
                || claim_snapshot.replacement_session_path != replacement_session_path
                || claim_snapshot.action.as_deref() != Some(action.as_str())
                || claim_snapshot.target != target
                || claim_snapshot.origin_session_path.as_ref() != Some(&origin_session_path)
            {
                continue;
            }
            if !claim_immutable_plan_matches_session(&claim_snapshot, envelopes) {
                continue;
            }
            let Ok(origin_meta) = std::fs::symlink_metadata(&origin_session_path) else {
                continue;
            };
            if !origin_session_path.is_absolute()
                || !origin_meta.is_file()
                || origin_meta.file_type().is_symlink()
            {
                continue;
            }
            let Ok(canonical_origin) = origin_session_path.canonicalize() else {
                continue;
            };
            if canonical_origin.parent() != Some(canonical_parent.as_path()) {
                continue;
            }
            let Ok(origin_envelopes) = orgasmic_core::session::read_session_file(&canonical_origin)
            else {
                continue;
            };
            if origin_envelopes
                .first()
                .is_none_or(|origin| origin.run_id != origin_run_id)
                || session_run_meta_project(&origin_envelopes).as_deref()
                    != Some(containing_project_id)
            {
                continue;
            }
            links.push(IndexedRecoveryOrigin {
                project_id,
                origin_run_id,
                request_id,
                replacement_run_id,
                replacement_session_path,
                action,
                target,
                origin_session_path,
                replacement_boot_id: first.boot_id.clone(),
                draft_prompt: draft_prompt.clone(),
                claim: claim_snapshot,
            });
        }
    }
    links
}

pub fn reconstruct_claim_from_origin(link: &IndexedRecoveryOrigin) -> RecoveryClaim {
    link.claim.clone()
}

pub fn resolve_authoritative_recovery_claim(
    home: &Home,
    project_id: &str,
    origin_run_id: &str,
    indexed_origins: &[IndexedRecoveryOrigin],
) -> Result<ResolvedRecoveryClaim, RecoveryClaimError> {
    let loaded = load_recovery_claim(home, project_id, origin_run_id);
    match loaded {
        Ok(Some(claim)) => {
            if claim.status == RecoveryClaimStatus::Committed {
                if verify_committed_claim_against_session(&claim) {
                    return Ok(ResolvedRecoveryClaim::Valid(claim));
                }
                quarantine_invalid_claim(home, project_id, origin_run_id)?;
                return reconstruct_or_quarantine(home, project_id, origin_run_id, indexed_origins);
            }
            Ok(ResolvedRecoveryClaim::Valid(claim))
        }
        Ok(None) => reconstruct_or_quarantine(home, project_id, origin_run_id, indexed_origins),
        Err(RecoveryClaimError::CorruptClaim) => {
            quarantine_invalid_claim(home, project_id, origin_run_id)?;
            reconstruct_or_quarantine(home, project_id, origin_run_id, indexed_origins)
        }
        Err(err) => Err(err),
    }
}

fn reconstruct_or_quarantine(
    home: &Home,
    project_id: &str,
    origin_run_id: &str,
    indexed_origins: &[IndexedRecoveryOrigin],
) -> Result<ResolvedRecoveryClaim, RecoveryClaimError> {
    if let Some(link) = indexed_origins
        .iter()
        .find(|link| link.project_id == project_id && link.origin_run_id == origin_run_id)
    {
        let mut reconstructed = reconstruct_claim_from_origin(link);
        if let Ok(envelopes) =
            orgasmic_core::session::read_session_file(&reconstructed.replacement_session_path)
        {
            if let Some(first) = envelopes.first() {
                reconstructed.replacement_runtime_id = first.runtime_id.clone();
                reconstructed.runtime_id = Some(first.runtime_id.clone());
                if reconstructed.boot_id.is_none() {
                    reconstructed.boot_id = Some(first.boot_id.clone());
                }
            }
            if reconstructed.draft_prompt.is_none() {
                reconstructed.draft_prompt = session_prompt_draft(&envelopes);
            }
        }
        write_claim_atomic_or_reconcile(home, &reconstructed)?;
        return Ok(ResolvedRecoveryClaim::Reconstructed(reconstructed));
    }
    if load_recovery_claim(home, project_id, origin_run_id)?.is_some() {
        return Ok(ResolvedRecoveryClaim::InvalidQuarantined);
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
    #[cfg(unix)]
    {
        let Some(dir) = ClaimDirectory::open(home, project_id, false)? else {
            return Ok(());
        };
        if dir.remove(&claim_file_name(origin_run_id)?)? {
            dir.sync()?;
            recovery_failpoint("parent_fsync");
        }
        Ok(())
    }
    #[cfg(not(unix))]
    {
        let path = claim_path(home, project_id, origin_run_id)?;
        if path.exists() {
            std::fs::remove_file(path).map_err(RecoveryClaimError::Io)?;
        }
        Ok(())
    }
}

#[allow(clippy::too_many_arguments)]
pub fn plan_pending_recovery_claim(
    home: &Home,
    spec: &PendingRecoveryClaimSpec,
) -> Result<PendingRecoveryPlan, RecoveryClaimError> {
    if !validate_safe_component(&spec.request_id) {
        return Err(RecoveryClaimError::InvalidIdentifier);
    }
    if let Some(existing) = load_recovery_claim(home, &spec.project_id, &spec.origin_run_id)? {
        return Err(RecoveryClaimError::AlreadyClaimed(Box::new(existing)));
    }
    let replacement_uuid = uuid::Uuid::new_v4();
    let replacement_run_id = format!(
        "run-{}-{}",
        chrono::Utc::now().format("%Y%m%dT%H%M%S"),
        replacement_uuid.simple()
    );
    let replacement_runtime_id = uuid::Uuid::new_v4().to_string();
    let planned_identity = RuntimeIdentity::planned(
        replacement_run_id.clone(),
        replacement_runtime_id.clone(),
        &spec.boot_id,
    );
    let planned_native_runtime =
        if spec.action == "start_recovery_run" && spec.harness.as_deref() == Some("claude") {
            let command = spec
                .driver_config
                .get("command")
                .and_then(serde_json::Value::as_str)
                .unwrap_or("claude")
                .to_string();
            let mut args = spec
                .driver_config
                .get("args")
                .and_then(serde_json::Value::as_array)
                .map(|args| {
                    args.iter()
                        .filter_map(serde_json::Value::as_str)
                        .map(str::to_string)
                        .collect::<Vec<_>>()
                })
                .unwrap_or_default();
            if !args
                .iter()
                .any(|arg| arg == "--dangerously-skip-permissions")
            {
                args.push("--dangerously-skip-permissions".into());
            }
            if !args.iter().any(|arg| arg == "--session-id") {
                args.push("--session-id".into());
                args.push(replacement_runtime_id.clone());
            }
            let mut launch_argv = vec![command.clone()];
            launch_argv.extend(args);
            let session_path = spec
                .driver_config
                .get("cwd")
                .and_then(serde_json::Value::as_str)
                .and_then(|cwd| {
                    let encoded: String = cwd
                        .chars()
                        .map(|ch| if ch == '/' || ch == '.' { '-' } else { ch })
                        .collect();
                    std::env::var_os("HOME").map(|home| {
                        PathBuf::from(home)
                            .join(".claude/projects")
                            .join(encoded)
                            .join(format!("{replacement_runtime_id}.jsonl"))
                    })
                });
            Some(NativeRuntimeMeta {
                provider: "claude".into(),
                session_id: Some(replacement_runtime_id.clone()),
                session_path,
                launch_argv,
                resume_argv: vec![
                    command,
                    "--resume".into(),
                    replacement_runtime_id.clone(),
                    "--fork-session".into(),
                    "--dangerously-skip-permissions".into(),
                ],
            })
        } else {
            spec.planned_native_runtime.clone()
        };
    let claim = RecoveryClaim {
        plan_version: Some(1),
        project_id: spec.project_id.clone(),
        origin_run_id: spec.origin_run_id.clone(),
        request_id: spec.request_id.clone(),
        status: RecoveryClaimStatus::Pending,
        replacement_run_id: replacement_run_id.clone(),
        replacement_session_path: spec.replacement_session_path.clone(),
        replacement_runtime_id: replacement_runtime_id.clone(),
        runtime_id: Some(replacement_runtime_id.clone()),
        boot_id: Some(spec.boot_id.clone()),
        action: Some(spec.action.clone()),
        target: Some(spec.target.clone()),
        draft_prompt: spec.draft_prompt.clone(),
        origin_session_path: Some(spec.origin_session_path.clone()),
        planned_tmux_session: Some(tmux_session_name(&planned_identity)),
        task_id: Some(spec.task_id.clone()),
        kind: Some(spec.kind.clone()),
        worker_id: Some(spec.worker_id.clone()),
        role: Some(spec.role.clone()),
        requires_worker_finalize: Some(spec.requires_worker_finalize),
        transport: Some(spec.transport.clone()),
        harness: spec.harness.clone(),
        driver_config: Some(spec.driver_config.clone()),
        force_inert: Some(spec.force_inert),
        worktree: spec.worktree.clone(),
        last_path: spec.last_path.clone(),
        stdout_path: spec.stdout_path.clone(),
        planned_native_runtime,
        run_options: Some(spec.run_options.clone()),
    };
    write_claim_atomic_or_reconcile(home, &claim)?;
    recovery_failpoint("pending");
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
    if claim.replacement_runtime_id != details.runtime_id {
        return Err(RecoveryClaimError::CorruptClaim);
    }
    if claim
        .boot_id
        .as_deref()
        .is_some_and(|boot| boot != details.boot_id.as_str())
    {
        return Err(RecoveryClaimError::CorruptClaim);
    }
    if claim
        .action
        .as_deref()
        .is_some_and(|action| action != details.action.as_str())
    {
        return Err(RecoveryClaimError::CorruptClaim);
    }
    if claim
        .target
        .as_deref()
        .is_some_and(|target| target != details.target.as_str())
    {
        return Err(RecoveryClaimError::CorruptClaim);
    }
    claim.status = RecoveryClaimStatus::Committed;
    if claim.runtime_id.is_none() {
        claim.runtime_id = Some(details.runtime_id);
    }
    if claim.boot_id.is_none() {
        claim.boot_id = Some(details.boot_id);
    }
    if claim.action.is_none() {
        claim.action = Some(details.action);
    }
    if claim.target.is_none() {
        claim.target = Some(details.target);
    }
    if claim.draft_prompt.is_none() {
        claim.draft_prompt = details.draft_prompt;
    }
    write_claim_atomic_or_reconcile(home, &claim)?;
    recovery_failpoint("commit");
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

fn session_has_acquire(envelopes: &[SessionEnvelope]) -> bool {
    envelopes.iter().any(|envelope| {
        envelope.kind == SessionEventKind::Lifecycle
            && matches!(
                serde_json::from_value::<Lifecycle>(envelope.event.clone()),
                Ok(Lifecycle::Acquire { .. })
            )
    })
}

fn claim_immutable_plan_matches_session(
    claim: &RecoveryClaim,
    envelopes: &[SessionEnvelope],
) -> bool {
    if !recovery_claim_has_complete_plan(claim) {
        return false;
    }
    let Some((task_id, kind, worker_id)) = envelopes.iter().find_map(|envelope| {
        if envelope.kind != SessionEventKind::Lifecycle {
            return None;
        }
        match serde_json::from_value::<Lifecycle>(envelope.event.clone()).ok()? {
            Lifecycle::Acquire {
                task_id,
                kind,
                worker_id,
            } => Some((task_id, kind, worker_id)),
            _ => None,
        }
    }) else {
        return false;
    };
    if claim.task_id.as_deref() != Some(task_id.as_str())
        || claim.kind.as_deref() != Some(kind.as_str())
        || claim.worker_id.as_deref() != Some(worker_id.as_str())
    {
        return false;
    }
    let Some((
        transport,
        harness,
        project_id,
        worktree,
        last_path,
        stdout_path,
        role,
        requires_worker_finalize,
        driver_config,
    )) = envelopes.iter().find_map(|envelope| {
        if envelope.kind != SessionEventKind::Lifecycle {
            return None;
        }
        match serde_json::from_value::<Lifecycle>(envelope.event.clone()).ok()? {
            Lifecycle::RunMeta {
                transport,
                harness,
                project_id,
                worktree,
                last_path,
                stdout_path,
                role,
                requires_worker_finalize,
                driver_config,
            } => Some((
                transport,
                harness,
                project_id,
                worktree,
                last_path,
                stdout_path,
                role,
                requires_worker_finalize,
                driver_config,
            )),
            _ => None,
        }
    })
    else {
        return false;
    };
    if claim.transport.as_deref() != Some(transport.as_str())
        || claim.harness != harness
        || project_id.as_deref() != Some(claim.project_id.as_str())
        || claim.worktree != worktree
        || claim.last_path != last_path
        || claim.stdout_path != stdout_path
        || claim.role != role
        || claim.requires_worker_finalize != requires_worker_finalize
        || claim.driver_config.as_ref() != Some(&driver_config)
        || claim.force_inert
            != driver_config
                .get("force_inert")
                .and_then(serde_json::Value::as_bool)
    {
        return false;
    }
    let prompt = session_prompt_draft(envelopes);
    if claim.draft_prompt != prompt {
        return false;
    }
    if let Some(actual_native) = envelopes.iter().find_map(|envelope| {
        if envelope.kind != SessionEventKind::Lifecycle {
            return None;
        }
        match serde_json::from_value::<Lifecycle>(envelope.event.clone()).ok()? {
            Lifecycle::NativeRuntime {
                provider,
                session_id,
                session_path,
                launch_argv,
                resume_argv,
            } => Some(NativeRuntimeMeta {
                provider,
                session_id,
                session_path,
                launch_argv,
                resume_argv,
            }),
            _ => None,
        }
    }) {
        if claim.action.as_deref() == Some("start_recovery_run")
            && claim.planned_native_runtime.as_ref() != Some(&actual_native)
        {
            return false;
        }
        if actual_native.provider != claim.harness.as_deref().unwrap_or_default() {
            return false;
        }
        let expected_launch = claim.driver_config.as_ref().and_then(|config| {
            let command = config.get("command")?.as_str()?.to_string();
            let mut argv = vec![command];
            argv.extend(
                config
                    .get("args")?
                    .as_array()?
                    .iter()
                    .map(|value| value.as_str().map(str::to_string))
                    .collect::<Option<Vec<_>>>()?,
            );
            Some(argv)
        });
        if expected_launch.is_some_and(|expected| expected != actual_native.launch_argv) {
            return false;
        }
    }
    true
}

fn recovery_claim_snapshot_in_session(
    envelopes: &[SessionEnvelope],
    project_id: &str,
    origin_run_id: &str,
    request_id: &str,
) -> Option<RecoveryClaim> {
    envelopes.iter().rev().find_map(|envelope| {
        if envelope.kind != SessionEventKind::Lifecycle {
            return None;
        }
        match serde_json::from_value::<Lifecycle>(envelope.event.clone()).ok()? {
            Lifecycle::RecoveryOrigin {
                project_id: link_project,
                origin_run_id: link_origin,
                request_id: link_request,
                claim: Some(value),
                ..
            } if link_project == project_id
                && link_origin == origin_run_id
                && link_request == request_id =>
            {
                serde_json::from_value(value).ok()
            }
            _ => None,
        }
    })
}

pub fn verify_committed_claim_against_session(claim: &RecoveryClaim) -> bool {
    if claim.status != RecoveryClaimStatus::Committed || !recovery_claim_has_complete_plan(claim) {
        return false;
    }
    if !claim.replacement_session_path.exists() {
        return false;
    }
    if !claim_path_is_safe_regular_file(&claim.replacement_session_path) {
        return false;
    }
    let Ok(envelopes) = orgasmic_core::session::read_session_file(&claim.replacement_session_path)
    else {
        return false;
    };
    let Some(first) = envelopes.first() else {
        return false;
    };
    if first.run_id != claim.replacement_run_id {
        return false;
    }
    if first.runtime_id != claim.replacement_runtime_id {
        return false;
    }
    if claim
        .boot_id
        .as_deref()
        .is_some_and(|boot| first.boot_id != boot)
    {
        return false;
    }
    let Some(meta_project) = session_run_meta_project(&envelopes) else {
        return false;
    };
    if meta_project != claim.project_id {
        return false;
    }
    if !session_has_acquire(&envelopes) {
        return false;
    }
    let Some((replacement_run_id, replacement_session_path, action)) = recovery_origin_in_session(
        &envelopes,
        &claim.project_id,
        &claim.origin_run_id,
        &claim.request_id,
    ) else {
        return false;
    };
    if claim.replacement_run_id != replacement_run_id
        || claim.replacement_session_path != replacement_session_path
        || claim.action.as_deref() != Some(action.as_str())
    {
        return false;
    }
    let origin_path_ok = envelopes.iter().rev().find_map(|envelope| {
        if envelope.kind != SessionEventKind::Lifecycle {
            return None;
        }
        match serde_json::from_value::<Lifecycle>(envelope.event.clone()).ok()? {
            Lifecycle::RecoveryOrigin {
                origin_session_path,
                target,
                ..
            } => Some((origin_session_path, target)),
            _ => None,
        }
    });
    let Some((origin_session_path, link_target)) = origin_path_ok else {
        return false;
    };
    if claim.origin_session_path.as_ref() != Some(&origin_session_path) {
        return false;
    }
    if claim
        .target
        .as_deref()
        .is_some_and(|target| Some(target) != link_target.as_deref())
    {
        return false;
    }
    if !claim_immutable_plan_matches_session(claim, &envelopes) {
        return false;
    }
    if recovery_claim_snapshot_in_session(
        &envelopes,
        &claim.project_id,
        &claim.origin_run_id,
        &claim.request_id,
    )
    .as_ref()
        != Some(claim)
    {
        return false;
    }
    let Some(origin_path) = claim.origin_session_path.as_ref() else {
        return false;
    };
    let Ok(origin_meta) = std::fs::symlink_metadata(origin_path) else {
        return false;
    };
    if !origin_meta.is_file() || origin_meta.file_type().is_symlink() {
        return false;
    }
    let (Ok(replacement_parent), Ok(origin_canonical)) = (
        claim
            .replacement_session_path
            .parent()
            .unwrap_or_else(|| Path::new(""))
            .canonicalize(),
        origin_path.canonicalize(),
    ) else {
        return false;
    };
    if origin_canonical.parent() != Some(replacement_parent.as_path()) {
        return false;
    }
    let Ok(origin_envelopes) = orgasmic_core::session::read_session_file(origin_canonical) else {
        return false;
    };
    origin_envelopes
        .first()
        .is_some_and(|origin| origin.run_id == claim.origin_run_id)
        && session_run_meta_project(&origin_envelopes).as_deref() == Some(claim.project_id.as_str())
}

fn claim_planned_boot_id(claim: &RecoveryClaim) -> &str {
    claim.boot_id.as_deref().unwrap_or("")
}

pub fn pending_session_prefix_matches_claim(
    claim: &RecoveryClaim,
    envelopes: &[SessionEnvelope],
) -> bool {
    if !recovery_claim_has_complete_plan(claim) {
        return false;
    }
    let Some(boot_id) = claim.boot_id.as_deref() else {
        return false;
    };
    if envelopes.iter().any(|envelope| {
        envelope.run_id != claim.replacement_run_id
            || envelope.runtime_id != claim.replacement_runtime_id
            || envelope.boot_id != boot_id
    }) {
        return false;
    }
    for envelope in envelopes {
        if envelope.kind != SessionEventKind::Lifecycle {
            continue;
        }
        let Ok(lifecycle) = serde_json::from_value::<Lifecycle>(envelope.event.clone()) else {
            return false;
        };
        match lifecycle {
            Lifecycle::Acquire {
                task_id,
                kind,
                worker_id,
            } => {
                if claim.task_id.as_deref() != Some(task_id.as_str())
                    || claim.kind.as_deref() != Some(kind.as_str())
                    || claim.worker_id.as_deref() != Some(worker_id.as_str())
                {
                    return false;
                }
            }
            Lifecycle::RunMeta {
                transport,
                harness,
                project_id,
                worktree,
                last_path,
                stdout_path,
                role,
                requires_worker_finalize,
                driver_config,
            } => {
                if claim.transport.as_deref() != Some(transport.as_str())
                    || claim.harness != harness
                    || project_id.as_deref() != Some(claim.project_id.as_str())
                    || claim.worktree != worktree
                    || claim.last_path != last_path
                    || claim.stdout_path != stdout_path
                    || claim.role != role
                    || claim.requires_worker_finalize != requires_worker_finalize
                    || claim.driver_config.as_ref() != Some(&driver_config)
                {
                    return false;
                }
            }
            Lifecycle::PromptDraft { text, sent } => {
                if sent || claim.draft_prompt.as_deref() != Some(text.as_str()) {
                    return false;
                }
            }
            Lifecycle::NativeRuntime {
                provider,
                session_id,
                session_path,
                launch_argv,
                resume_argv,
            } => {
                let actual = NativeRuntimeMeta {
                    provider: provider.clone(),
                    session_id,
                    session_path,
                    launch_argv: launch_argv.clone(),
                    resume_argv,
                };
                if claim.action.as_deref() == Some("start_recovery_run")
                    && claim.planned_native_runtime.as_ref() != Some(&actual)
                {
                    return false;
                }
                if claim.harness.as_deref() != Some(provider.as_str()) {
                    return false;
                }
                if let Some(expected) = claim.driver_config.as_ref().and_then(|config| {
                    let command = config.get("command")?.as_str()?.to_string();
                    let mut argv = vec![command];
                    argv.extend(
                        config
                            .get("args")?
                            .as_array()?
                            .iter()
                            .map(|value| value.as_str().map(str::to_string))
                            .collect::<Option<Vec<_>>>()?,
                    );
                    Some(argv)
                }) {
                    if expected != launch_argv {
                        return false;
                    }
                }
            }
            Lifecycle::RecoveryOrigin {
                project_id,
                origin_run_id,
                origin_session_path,
                request_id,
                replacement_run_id,
                replacement_session_path,
                action,
                target,
                claim: snapshot,
            } => {
                if project_id != claim.project_id
                    || origin_run_id != claim.origin_run_id
                    || claim.origin_session_path.as_ref() != Some(&origin_session_path)
                    || request_id != claim.request_id
                    || replacement_run_id != claim.replacement_run_id
                    || replacement_session_path != claim.replacement_session_path
                    || claim.action.as_deref() != Some(action.as_str())
                    || claim.target != target
                {
                    return false;
                }
                let Some(mut snapshot) =
                    snapshot.and_then(|value| serde_json::from_value::<RecoveryClaim>(value).ok())
                else {
                    return false;
                };
                snapshot.status = RecoveryClaimStatus::Pending;
                if &snapshot != claim {
                    return false;
                }
            }
            Lifecycle::Release { .. }
            | Lifecycle::Attach
            | Lifecycle::Continuation { .. }
            | Lifecycle::BabysitterSpawned { .. }
            | Lifecycle::Reattach { .. }
            | Lifecycle::ComposerSend { .. } => return false,
        }
    }
    true
}

pub fn reconcile_pending_claim(
    home: &Home,
    claim: &RecoveryClaim,
) -> Result<Option<PendingRecoveryPlan>, RecoveryClaimError> {
    if claim.status != RecoveryClaimStatus::Pending {
        return Ok(None);
    }
    let boot_id = claim_planned_boot_id(claim);
    let planned_identity = RuntimeIdentity::planned(
        claim.replacement_run_id.clone(),
        claim.replacement_runtime_id.clone(),
        boot_id,
    );
    let tmux_live = claim
        .planned_tmux_session
        .as_deref()
        .is_some_and(tmux_session_exists)
        || tmux_session_exists(&tmux_session_name(&planned_identity));
    if !claim.replacement_session_path.exists() {
        return Ok(Some(PendingRecoveryPlan {
            claim: claim.clone(),
            planned_identity,
            reattach_existing: tmux_live,
        }));
    }
    let envelopes = orgasmic_core::session::read_session_file(&claim.replacement_session_path)
        .map_err(|_| RecoveryClaimError::CorruptClaim)?;
    if !pending_session_prefix_matches_claim(claim, &envelopes) {
        return Err(RecoveryClaimError::CorruptClaim);
    }
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
        reattach_existing: has_acquire || tmux_live,
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

    fn sample_spec(
        _home: &Home,
        project_root: &Path,
        origin_run_id: &str,
        request_id: &str,
        boot_id: &str,
        force_inert: bool,
    ) -> (PendingRecoveryClaimSpec, PathBuf) {
        let origin_path = project_sessions_dir(project_root).join(format!("{origin_run_id}.jsonl"));
        std::fs::create_dir_all(origin_path.parent().unwrap()).unwrap();
        std::fs::write(&origin_path, "{}\n").unwrap();
        let replacement_path =
            project_sessions_dir(project_root).join(format!("recover-{origin_run_id}.jsonl"));
        (
            PendingRecoveryClaimSpec {
                project_id: "orgasmic".into(),
                origin_run_id: origin_run_id.into(),
                request_id: request_id.into(),
                origin_session_path: origin_path,
                replacement_session_path: replacement_path.clone(),
                boot_id: boot_id.into(),
                action: "start_recovery_run".into(),
                target: "worker".into(),
                draft_prompt: Some("stable draft".into()),
                force_inert,
                task_id: "TASK-1".into(),
                kind: "worker".into(),
                worker_id: "implementer-claude-acp".into(),
                role: "implementer".into(),
                requires_worker_finalize: true,
                transport: "tmux".into(),
                harness: Some("claude".into()),
                driver_config: serde_json::json!({"force_inert": force_inert, "harness": "claude"}),
                worktree: Some(project_root.to_path_buf()),
                last_path: None,
                stdout_path: None,
                planned_native_runtime: None,
                run_options: RecoveryRunOptions {
                    stall_timeout_secs: None,
                    max_run_duration_secs: None,
                    idle_timeout_secs: None,
                    babysitter_target: None,
                    cleanup_on_failure: false,
                },
            },
            replacement_path,
        )
    }

    #[test]
    fn pending_then_committed_claim_roundtrip() {
        let tmp = tempfile::tempdir().unwrap();
        let home = Home::at(tmp.path().join("home"));
        home.ensure().unwrap();
        let project_root = tmp.path().join("proj");
        let (spec, _) = sample_spec(
            &home,
            &project_root,
            "run-origin",
            "req-1",
            "boot-new",
            false,
        );

        let plan = plan_pending_recovery_claim(&home, &spec).unwrap();
        assert_eq!(plan.claim.status, RecoveryClaimStatus::Pending);
        assert_eq!(plan.claim.boot_id.as_deref(), Some("boot-new"));
        assert_eq!(plan.claim.force_inert, Some(false));

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
            plan_version: None,
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
            origin_session_path: None,
            planned_tmux_session: None,
            task_id: None,
            kind: None,
            worker_id: None,
            role: None,
            requires_worker_finalize: None,
            transport: None,
            harness: None,
            driver_config: None,
            force_inert: None,
            worktree: None,
            last_path: None,
            stdout_path: None,
            planned_native_runtime: None,
            run_options: None,
        };
        assert!(!verify_committed_claim_against_session(&claim));
    }

    #[test]
    fn reconcile_pending_commits_when_recovery_origin_exists() {
        let tmp = tempfile::tempdir().unwrap();
        let home = Home::at(tmp.path().join("home"));
        home.ensure().unwrap();
        let project_root = tmp.path().join("proj");
        let (spec, replacement_path) = sample_spec(
            &home,
            &project_root,
            "run-origin",
            "req-pending",
            "boot-plan",
            true,
        );
        let plan = plan_pending_recovery_claim(&home, &spec).unwrap();
        let identity = RuntimeIdentity {
            run_id: plan.claim.replacement_run_id.clone(),
            runtime_id: plan.claim.replacement_runtime_id.clone(),
            boot_id: "boot-plan".into(),
        };
        let mut writer = orgasmic_core::SessionWriter::open(&replacement_path, identity).unwrap();
        writer
            .append(
                orgasmic_core::session::SessionEventKind::Lifecycle,
                serde_json::to_value(orgasmic_core::session::Lifecycle::RunMeta {
                    transport: "tmux".into(),
                    harness: Some("claude".into()),
                    project_id: Some("orgasmic".into()),
                    worktree: spec.worktree.clone(),
                    last_path: None,
                    stdout_path: None,
                    role: Some("implementer".into()),
                    requires_worker_finalize: Some(true),
                    driver_config: spec.driver_config.clone(),
                })
                .unwrap(),
            )
            .unwrap();
        let mut committed_snapshot = plan.claim.clone();
        committed_snapshot.status = RecoveryClaimStatus::Committed;
        writer
            .append(
                orgasmic_core::session::SessionEventKind::Lifecycle,
                serde_json::to_value(orgasmic_core::session::Lifecycle::Acquire {
                    task_id: "TASK-1".into(),
                    kind: "worker".into(),
                    worker_id: "implementer-claude-acp".into(),
                })
                .unwrap(),
            )
            .unwrap();
        writer
            .append(
                orgasmic_core::session::SessionEventKind::Lifecycle,
                serde_json::to_value(orgasmic_core::session::Lifecycle::RecoveryOrigin {
                    project_id: "orgasmic".into(),
                    origin_run_id: "run-origin".into(),
                    origin_session_path: spec.origin_session_path.clone(),
                    request_id: "req-pending".into(),
                    replacement_run_id: plan.claim.replacement_run_id.clone(),
                    replacement_session_path: replacement_path,
                    action: "start_recovery_run".into(),
                    target: Some("worker".into()),
                    claim: Some(serde_json::to_value(committed_snapshot).unwrap()),
                })
                .unwrap(),
            )
            .unwrap();
        drop(writer);

        let plan = reconcile_pending_claim(&home, &plan.claim)
            .unwrap()
            .expect("pending with existing origin link reconciles");
        assert_eq!(plan.claim.status, RecoveryClaimStatus::Committed);
        assert_eq!(plan.claim.boot_id.as_deref(), Some("boot-plan"));
    }

    #[test]
    fn reconcile_pending_uses_persisted_boot_id_not_current_daemon() {
        let tmp = tempfile::tempdir().unwrap();
        let home = Home::at(tmp.path().join("home"));
        home.ensure().unwrap();
        let project_root = tmp.path().join("proj");
        let (spec, _) = sample_spec(
            &home,
            &project_root,
            "run-boot",
            "req-boot",
            "boot-persisted",
            false,
        );
        let plan = plan_pending_recovery_claim(&home, &spec).unwrap();
        let reconciled = reconcile_pending_claim(&home, &plan.claim)
            .unwrap()
            .expect("pending plan");
        assert_eq!(reconciled.planned_identity.boot_id, "boot-persisted");
    }

    #[test]
    fn retry_force_inert_does_not_alter_existing_pending_plan() {
        let tmp = tempfile::tempdir().unwrap();
        let home = Home::at(tmp.path().join("home"));
        home.ensure().unwrap();
        let project_root = tmp.path().join("proj");
        let (mut spec, _) = sample_spec(
            &home,
            &project_root,
            "run-inert",
            "req-inert",
            "boot-a",
            true,
        );
        plan_pending_recovery_claim(&home, &spec).unwrap();
        spec.force_inert = false;
        spec.driver_config = serde_json::json!({"force_inert": false});
        assert!(matches!(
            plan_pending_recovery_claim(&home, &spec),
            Err(RecoveryClaimError::AlreadyClaimed(existing)) if existing.force_inert == Some(true)
        ));
    }

    #[test]
    fn corrupt_claim_quarantines_and_reconstructs_from_session_truth() {
        let tmp = tempfile::tempdir().unwrap();
        let home = Home::at(tmp.path().join("home"));
        home.ensure().unwrap();
        let project_root = tmp.path().join("proj");
        let (spec, replacement_path) = sample_spec(
            &home,
            &project_root,
            "run-corrupt-origin",
            "req-truth",
            "boot-truth",
            false,
        );
        let origin_identity = RuntimeIdentity {
            run_id: "run-corrupt-origin".into(),
            runtime_id: "rt-origin".into(),
            boot_id: "boot-origin".into(),
        };
        std::fs::remove_file(&spec.origin_session_path).unwrap();
        let mut origin_writer =
            orgasmic_core::SessionWriter::open(&spec.origin_session_path, origin_identity).unwrap();
        origin_writer
            .append(
                SessionEventKind::Lifecycle,
                serde_json::to_value(Lifecycle::RunMeta {
                    transport: "tmux".into(),
                    harness: Some("claude".into()),
                    project_id: Some("orgasmic".into()),
                    worktree: None,
                    last_path: None,
                    stdout_path: None,
                    role: Some("implementer".into()),
                    requires_worker_finalize: Some(true),
                    driver_config: serde_json::json!({}),
                })
                .unwrap(),
            )
            .unwrap();
        drop(origin_writer);
        let plan = plan_pending_recovery_claim(&home, &spec).unwrap();
        let claim_path = claim_path(&home, "orgasmic", "run-corrupt-origin").unwrap();
        std::fs::write(&claim_path, "{not-json").unwrap();

        let identity = RuntimeIdentity {
            run_id: plan.claim.replacement_run_id.clone(),
            runtime_id: plan.claim.replacement_runtime_id.clone(),
            boot_id: "boot-truth".into(),
        };
        let mut writer = orgasmic_core::SessionWriter::open(&replacement_path, identity).unwrap();
        writer
            .append(
                SessionEventKind::Lifecycle,
                serde_json::to_value(Lifecycle::RunMeta {
                    transport: "tmux".into(),
                    harness: Some("claude".into()),
                    project_id: Some("orgasmic".into()),
                    worktree: spec.worktree.clone(),
                    last_path: None,
                    stdout_path: None,
                    role: Some("implementer".into()),
                    requires_worker_finalize: Some(true),
                    driver_config: spec.driver_config.clone(),
                })
                .unwrap(),
            )
            .unwrap();
        writer
            .append(
                SessionEventKind::Lifecycle,
                serde_json::to_value(Lifecycle::Acquire {
                    task_id: "TASK-1".into(),
                    kind: "worker".into(),
                    worker_id: "implementer-claude-acp".into(),
                })
                .unwrap(),
            )
            .unwrap();
        writer
            .append(
                SessionEventKind::Lifecycle,
                serde_json::to_value(Lifecycle::PromptDraft {
                    text: spec.draft_prompt.clone().unwrap(),
                    sent: false,
                })
                .unwrap(),
            )
            .unwrap();
        writer
            .append(
                SessionEventKind::Lifecycle,
                serde_json::to_value(Lifecycle::RecoveryOrigin {
                    project_id: "orgasmic".into(),
                    origin_run_id: "run-corrupt-origin".into(),
                    origin_session_path: spec.origin_session_path.clone(),
                    request_id: "req-truth".into(),
                    replacement_run_id: plan.claim.replacement_run_id.clone(),
                    replacement_session_path: replacement_path.clone(),
                    action: "start_recovery_run".into(),
                    target: Some("worker".into()),
                    claim: {
                        let mut snapshot = plan.claim.clone();
                        snapshot.status = RecoveryClaimStatus::Committed;
                        Some(serde_json::to_value(snapshot).unwrap())
                    },
                })
                .unwrap(),
            )
            .unwrap();
        drop(writer);

        let links = index_recovery_origins_in_session(
            &orgasmic_core::session::read_session_file(&replacement_path).unwrap(),
            &replacement_path,
            "orgasmic",
        );
        let resolved =
            resolve_authoritative_recovery_claim(&home, "orgasmic", "run-corrupt-origin", &links)
                .unwrap();
        assert!(matches!(resolved, ResolvedRecoveryClaim::Reconstructed(_)));
        assert!(claim_path.with_extension("json.quarantine").exists());
        assert!(load_recovery_claim(&home, "orgasmic", "run-corrupt-origin")
            .unwrap()
            .is_some());
    }

    #[test]
    fn stale_temp_claim_is_reconciled_on_retry() {
        let tmp = tempfile::tempdir().unwrap();
        let home = Home::at(tmp.path().join("home"));
        home.ensure().unwrap();
        let project_root = tmp.path().join("proj");
        let (spec, replacement_path) = sample_spec(
            &home,
            &project_root,
            "run-temp-wedge",
            "req-temp",
            "boot-temp",
            false,
        );
        let claim = RecoveryClaim {
            plan_version: Some(1),
            project_id: spec.project_id.clone(),
            origin_run_id: spec.origin_run_id.clone(),
            request_id: spec.request_id.clone(),
            status: RecoveryClaimStatus::Pending,
            replacement_run_id: "run-temp-replacement".into(),
            replacement_session_path: replacement_path,
            replacement_runtime_id: "rt-temp".into(),
            runtime_id: Some("rt-temp".into()),
            boot_id: Some("boot-temp".into()),
            action: Some("start_recovery_run".into()),
            target: Some("worker".into()),
            draft_prompt: Some("stable draft".into()),
            origin_session_path: Some(spec.origin_session_path),
            planned_tmux_session: Some("orgasmic-run-temp-replacement-rt-temp".into()),
            task_id: Some("TASK-1".into()),
            kind: Some("worker".into()),
            worker_id: Some("implementer-claude-acp".into()),
            role: Some("implementer".into()),
            requires_worker_finalize: Some(true),
            transport: Some("tmux".into()),
            harness: Some("claude".into()),
            driver_config: Some(serde_json::json!({"force_inert": false})),
            force_inert: Some(false),
            worktree: None,
            last_path: None,
            stdout_path: None,
            planned_native_runtime: None,
            run_options: Some(spec.run_options.clone()),
        };
        let path = claim_path(&home, "orgasmic", "run-temp-wedge").unwrap();
        ClaimDirectory::open(&home, "orgasmic", true).unwrap();
        let stale = path.with_extension(format!("json.tmp.{}", uuid::Uuid::new_v4()));
        std::fs::write(&stale, serde_json::to_string_pretty(&claim).unwrap()).unwrap();
        reconcile_stale_claim_temp(&home, "orgasmic", "run-temp-wedge").unwrap();
        assert!(
            path.exists(),
            "reconcile_stale_claim_temp must promote orphan temp"
        );
        let loaded = load_recovery_claim(&home, "orgasmic", "run-temp-wedge")
            .unwrap()
            .unwrap();
        assert_eq!(loaded.draft_prompt.as_deref(), Some("stable draft"));
    }

    #[test]
    fn verify_rejects_missing_run_meta() {
        let tmp = tempfile::tempdir().unwrap();
        let project_root = tmp.path().join("proj");
        let replacement_path = project_sessions_dir(&project_root).join("recover-nometa.jsonl");
        std::fs::create_dir_all(replacement_path.parent().unwrap()).unwrap();
        let identity = RuntimeIdentity {
            run_id: "run-replacement".into(),
            runtime_id: "rt-replacement".into(),
            boot_id: "boot-new".into(),
        };
        let origin_path = project_sessions_dir(&project_root).join("run-origin.jsonl");
        std::fs::write(&origin_path, "{}\n").unwrap();
        let mut writer = orgasmic_core::SessionWriter::open(&replacement_path, identity).unwrap();
        writer
            .append(
                SessionEventKind::Lifecycle,
                serde_json::to_value(Lifecycle::Acquire {
                    task_id: "TASK-1".into(),
                    kind: "worker".into(),
                    worker_id: "implementer-claude-acp".into(),
                })
                .unwrap(),
            )
            .unwrap();
        writer
            .append(
                SessionEventKind::Lifecycle,
                serde_json::to_value(Lifecycle::RecoveryOrigin {
                    project_id: "orgasmic".into(),
                    origin_run_id: "run-origin".into(),
                    origin_session_path: origin_path,
                    request_id: "req-1".into(),
                    replacement_run_id: "run-replacement".into(),
                    replacement_session_path: replacement_path.clone(),
                    action: "start_recovery_run".into(),
                    target: Some("worker".into()),
                    claim: None,
                })
                .unwrap(),
            )
            .unwrap();
        drop(writer);
        let claim = RecoveryClaim {
            plan_version: None,
            project_id: "orgasmic".into(),
            origin_run_id: "run-origin".into(),
            request_id: "req-1".into(),
            status: RecoveryClaimStatus::Committed,
            replacement_run_id: "run-replacement".into(),
            replacement_session_path: replacement_path,
            replacement_runtime_id: "rt-replacement".into(),
            runtime_id: Some("rt-replacement".into()),
            boot_id: Some("boot-new".into()),
            action: Some("start_recovery_run".into()),
            target: Some("worker".into()),
            draft_prompt: None,
            origin_session_path: None,
            planned_tmux_session: None,
            task_id: None,
            kind: None,
            worker_id: None,
            role: None,
            requires_worker_finalize: None,
            transport: None,
            harness: None,
            driver_config: None,
            force_inert: None,
            worktree: None,
            last_path: None,
            stdout_path: None,
            planned_native_runtime: None,
            run_options: None,
        };
        assert!(!verify_committed_claim_against_session(&claim));
    }

    #[cfg(unix)]
    #[test]
    fn rejects_symlinked_recovery_claims_parent() {
        let tmp = tempfile::tempdir().unwrap();
        let home = Home::at(tmp.path().join("home"));
        home.ensure().unwrap();
        let real_root = home.state().join("recovery-claims-real");
        std::fs::create_dir_all(&real_root).unwrap();
        let link_root = home.state().join("recovery-claims");
        std::os::unix::fs::symlink(&real_root, &link_root).unwrap();
        let (spec, _) = sample_spec(
            &home,
            &tmp.path().join("proj"),
            "run-slink",
            "req-slink",
            "boot-s",
            false,
        );
        assert!(matches!(
            plan_pending_recovery_claim(&home, &spec),
            Err(RecoveryClaimError::CorruptClaim)
        ));
    }

    #[cfg(unix)]
    #[test]
    fn rejects_symlink_claim_file() {
        let tmp = tempfile::tempdir().unwrap();
        let home = Home::at(tmp.path().join("home"));
        home.ensure().unwrap();
        let project_root = tmp.path().join("proj");
        let (spec, _) = sample_spec(
            &home,
            &project_root,
            "run-sfile",
            "req-sfile",
            "boot-s",
            false,
        );
        plan_pending_recovery_claim(&home, &spec).unwrap();
        let path = claim_path(&home, "orgasmic", "run-sfile").unwrap();
        let real = path.with_extension("json.real");
        std::fs::rename(&path, &real).unwrap();
        std::os::unix::fs::symlink(&real, &path).unwrap();
        assert!(matches!(
            load_recovery_claim(&home, "orgasmic", "run-sfile"),
            Err(RecoveryClaimError::CorruptClaim)
        ));
    }

    #[test]
    fn index_requires_run_meta_project_match() {
        let tmp = tempfile::tempdir().unwrap();
        let replacement_path =
            project_sessions_dir(&tmp.path().join("proj")).join("recover-index.jsonl");
        std::fs::create_dir_all(replacement_path.parent().unwrap()).unwrap();
        let identity = RuntimeIdentity {
            run_id: "run-r".into(),
            runtime_id: "rt-r".into(),
            boot_id: "boot-r".into(),
        };
        let origin_path = project_sessions_dir(&tmp.path().join("proj")).join("run-o.jsonl");
        std::fs::write(&origin_path, "{}\n").unwrap();
        let mut writer = orgasmic_core::SessionWriter::open(&replacement_path, identity).unwrap();
        writer
            .append(
                SessionEventKind::Lifecycle,
                serde_json::to_value(Lifecycle::RecoveryOrigin {
                    project_id: "orgasmic".into(),
                    origin_run_id: "run-o".into(),
                    origin_session_path: origin_path,
                    request_id: "req".into(),
                    replacement_run_id: "run-r".into(),
                    replacement_session_path: replacement_path.clone(),
                    action: "start_recovery_run".into(),
                    target: Some("worker".into()),
                    claim: None,
                })
                .unwrap(),
            )
            .unwrap();
        drop(writer);
        let envelopes = orgasmic_core::session::read_session_file(&replacement_path).unwrap();
        assert!(
            index_recovery_origins_in_session(&envelopes, &replacement_path, "orgasmic").is_empty()
        );
    }
}
