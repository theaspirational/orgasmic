// orgasmic:task_A6FGF, task_QPKCD, task_6ZTFM, task_3TEDA
//! Daemon-owned, project-scoped recovery claims for Failed tombstone rescue idempotency.

use std::collections::{BTreeMap, HashMap};
use std::fs::{File, OpenOptions};
use std::io::{Read, Write};
use std::path::{Component, Path, PathBuf};
use std::sync::{Arc, Mutex};

use orgasmic_core::home::Home;
use orgasmic_core::session::{Lifecycle, SessionEnvelope, SessionEventKind};
use orgasmic_core::{
    project_sessions_dir, RuntimeIdentity, SessionLifecycleScan, SessionScanBudget,
};
use orgasmic_drivers::modes::tmux::{
    observe_tmux_session, tmux_session_name, TmuxSessionObservation,
};
use orgasmic_drivers::NativeRuntimeMeta;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use subtle::ConstantTimeEq;

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
    /// HMAC-SHA256 over the immutable plan, keyed by daemon-owned host auth
    /// material. Project JSONL may retain this proof but cannot mint it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub authority_tag: Option<String>,
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
    /// Durable transaction state set immediately before the one permitted
    /// planned-handle spawn. Once true, a missing/dead planned handle is never
    /// relaunched; recovery fails closed.
    #[serde(default)]
    pub spawn_started: bool,
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
    /// Retained no-follow authority for the replacement JSONL. Recovery uses
    /// this exact handle for parsing and the first replacement append.
    pub(crate) session_file: Option<SessionFile>,
}

/// Test-only fault injection for the window between opening the replacement
/// JSONL and validating it, keyed by the `replacement_run_id` it was armed for.
///
/// The static is shared by every test in this binary, and `reconcile_pending_claim`
/// is called concurrently by several of them: three sibling `reconcile_pending_*`
/// tests plus every `POST /runs/:id/recover` test whose origin carries a pending
/// claim. An *unkeyed* hook is consumed by whichever call reaches the site
/// first, which is not necessarily the one the arming test made — the arming
/// test then observes an unperturbed reconcile and fails its `is_err()`
/// assertion. Keying makes the hook fire for exactly one claim, so arming is
/// exclusive without serializing any test against any other.
#[cfg(test)]
#[allow(clippy::type_complexity)]
static PENDING_RECONCILE_AFTER_OPEN_HOOK: std::sync::Mutex<
    Option<(String, Box<dyn FnOnce() + Send>)>,
> = std::sync::Mutex::new(None);

/// Arm [`PENDING_RECONCILE_AFTER_OPEN_HOOK`] for one `replacement_run_id`.
#[cfg(test)]
fn arm_pending_reconcile_after_open_hook(replacement_run_id: &str, hook: Box<dyn FnOnce() + Send>) {
    *PENDING_RECONCILE_AFTER_OPEN_HOOK
        .lock()
        .expect("pending reconcile hook lock") = Some((replacement_run_id.to_string(), hook));
}

/// Test-only injection of an `authority_key` READ FAILURE, keyed by the token
/// path so tests in this binary cannot consume each other's arming.
///
/// orgasmic:TASK-2QK4P.1.1.1 acceptance 7 — "an nth authority-key read failure".
/// The value is a countdown: the call that decrements it to zero fails. There is
/// no filesystem fault that can fail the Nth read of one file and not the
/// others, and the whole point of F1(a) is that a failure at a PARTICULAR
/// position inside one enumeration changes the answer, so the position has to
/// be steerable.
#[cfg(test)]
static AUTHORITY_KEY_FAULTS: std::sync::Mutex<Option<BTreeMap<PathBuf, u32>>> =
    std::sync::Mutex::new(None);

/// Fail the `nth` (1-based) `authority_key` read under this `home`, then stop.
#[cfg(test)]
fn arm_authority_key_fault(home: &Home, nth: u32) {
    assert!(nth >= 1, "reads are counted from one");
    AUTHORITY_KEY_FAULTS
        .lock()
        .expect("authority key fault lock")
        .get_or_insert_with(BTreeMap::new)
        .insert(home.auth_token(), nth);
}

/// Did the armed fault actually FIRE? A test that loops over read positions
/// needs this to tell "position n does not exist on this path" from "position n
/// exists and was survived".
#[cfg(test)]
fn authority_key_fault_fired(home: &Home) -> bool {
    !AUTHORITY_KEY_FAULTS
        .lock()
        .expect("authority key fault lock")
        .as_ref()
        .is_some_and(|map| map.contains_key(&home.auth_token()))
}

#[cfg(test)]
fn disarm_authority_key_fault(home: &Home) {
    if let Some(map) = AUTHORITY_KEY_FAULTS
        .lock()
        .expect("authority key fault lock")
        .as_mut()
    {
        map.remove(&home.auth_token());
    }
}

/// Homes whose `authority_key` reached [`crate::auth::load_or_generate`].
///
/// orgasmic:TASK-2QK4P.1.1.1.1 F5 — the acceptance is "assert `load_or_generate`
/// was NOT reached", and "the token bytes are unchanged" is a weaker proxy: a
/// mint that happened to reproduce identical bytes would pass it. This records
/// the call itself.
#[cfg(test)]
static LOAD_OR_GENERATE_REACHED: std::sync::Mutex<Option<BTreeMap<PathBuf, u32>>> =
    std::sync::Mutex::new(None);

#[cfg(test)]
fn record_load_or_generate_reached(home: &Home) {
    *LOAD_OR_GENERATE_REACHED
        .lock()
        .expect("load_or_generate probe lock")
        .get_or_insert_with(BTreeMap::new)
        .entry(home.auth_token())
        .or_insert(0) += 1;
}

#[cfg(test)]
fn load_or_generate_reached_count(home: &Home) -> u32 {
    LOAD_OR_GENERATE_REACHED
        .lock()
        .expect("load_or_generate probe lock")
        .as_ref()
        .and_then(|map| map.get(&home.auth_token()).copied())
        .unwrap_or(0)
}

/// Fail the `nth` (1-based) `readdir` in [`ClaimDirectory::names`] under this
/// home's state root with `errno`, then stop.
///
/// orgasmic:TASK-2QK4P.1.1.1.1 F2 — a real mid-listing `EIO` cannot be produced
/// from a test, so the seam reproduces its exact observable shape: `readdir`
/// returns NULL with a non-zero `errno`. That is precisely the shape the old
/// loop could not tell from end-of-directory.
///
/// orgasmic:TASK-2QK4P.1.1.1.1.1 P1b — `pub(crate)` so the API-level boot test
/// can arm the SAME seam and drive the fault through
/// `reattach_live_runs_on_boot`. Round five's regression called the predicate
/// directly and would have stayed green under a boot that reattached on
/// `Unobserved`; that is the gap this visibility closes.
#[cfg(test)]
static READDIR_FAULTS: std::sync::Mutex<Option<BTreeMap<PathBuf, ReaddirFault>>> =
    std::sync::Mutex::new(None);

/// One armed `readdir` fault. `sticky` decides whether it survives firing: a
/// single boot pass lists the claim store once per candidate, so a test with
/// more than one candidate needs the fault to hold for the whole pass rather
/// than disarm itself after the first listing.
#[cfg(test)]
#[derive(Clone, Copy)]
struct ReaddirFault {
    nth: u32,
    code: i32,
    sticky: bool,
}

#[cfg(test)]
pub(crate) fn arm_readdir_fault(home: &Home, nth: u32, code: i32) {
    arm_readdir_fault_inner(home, nth, code, false);
}

/// Arm a fault that stays armed until [`disarm_readdir_fault`] clears it.
#[cfg(test)]
pub(crate) fn arm_readdir_fault_until_disarmed(home: &Home, nth: u32, code: i32) {
    arm_readdir_fault_inner(home, nth, code, true);
}

#[cfg(test)]
fn arm_readdir_fault_inner(home: &Home, nth: u32, code: i32, sticky: bool) {
    assert!(nth >= 1, "iterations are counted from one");
    let key = home.state().canonicalize().unwrap_or_else(|_| home.state());
    READDIR_FAULTS
        .lock()
        .expect("readdir fault lock")
        .get_or_insert_with(BTreeMap::new)
        .insert(key, ReaddirFault { nth, code, sticky });
}

#[cfg(test)]
pub(crate) fn disarm_readdir_fault(home: &Home) {
    let key = home.state().canonicalize().unwrap_or_else(|_| home.state());
    if let Some(map) = READDIR_FAULTS.lock().expect("readdir fault lock").as_mut() {
        map.remove(&key);
    }
}

#[cfg(test)]
fn readdir_fault(state_root: &Path, iteration: u32) -> Option<i32> {
    let mut slot = READDIR_FAULTS.lock().expect("readdir fault lock");
    let map = slot.as_mut()?;
    let fault = *map.get(state_root)?;
    if iteration != fault.nth {
        return None;
    }
    if !fault.sticky {
        map.remove(state_root);
    }
    Some(fault.code)
}

/// A synthetic entry name delivered on the `nth` (1-based) iteration of
/// [`ClaimDirectory::names`], as raw bytes.
///
/// orgasmic:TASK-2QK4P.1.1.1.1.1 P2a — APFS and HFS+ REJECT a non-UTF-8 file
/// name with `EILSEQ`, so the byte-preservation half of the F2 non-UTF-8
/// regression could not be built on the platform this project runs on, and what
/// remained was a `Vec<OsString>` type assertion that a reintroduced
/// `to_str()` drop would still satisfy. This seam removes the filesystem from
/// the fixture and nothing else: the bytes are delivered through a real
/// `dirent`'s `d_name`, read back with the same `CStr::from_ptr(..).to_bytes()`
/// the syscall path uses, and handed to the SAME collect tail. Only the kernel
/// is stubbed; every decision under test is production code, so the old
/// drop/lossy implementation reds here on every platform.
/// State root -> (iteration to fire on, the raw name bytes to deliver).
#[cfg(test)]
type ReaddirEntryNames = BTreeMap<PathBuf, (u32, Vec<u8>)>;

#[cfg(test)]
static READDIR_ENTRY_NAMES: std::sync::Mutex<Option<ReaddirEntryNames>> =
    std::sync::Mutex::new(None);

#[cfg(test)]
pub(crate) fn arm_readdir_entry_name(home: &Home, nth: u32, bytes: &[u8]) {
    assert!(nth >= 1, "iterations are counted from one");
    let key = home.state().canonicalize().unwrap_or_else(|_| home.state());
    READDIR_ENTRY_NAMES
        .lock()
        .expect("readdir entry name lock")
        .get_or_insert_with(BTreeMap::new)
        .insert(key, (nth, bytes.to_vec()));
}

#[cfg(test)]
fn readdir_entry_name(state_root: &Path, iteration: u32) -> Option<Vec<u8>> {
    let mut slot = READDIR_ENTRY_NAMES.lock().expect("readdir entry name lock");
    let map = slot.as_mut()?;
    let (nth, _) = map.get(state_root)?;
    if iteration != *nth {
        return None;
    }
    map.remove(state_root).map(|(_, bytes)| bytes)
}

#[cfg(test)]
fn authority_key_fault(home: &Home) -> Result<(), RecoveryClaimError> {
    let mut slot = AUTHORITY_KEY_FAULTS
        .lock()
        .expect("authority key fault lock");
    let Some(map) = slot.as_mut() else {
        return Ok(());
    };
    let token = home.auth_token();
    let Some(remaining) = map.get_mut(&token) else {
        return Ok(());
    };
    *remaining -= 1;
    if *remaining == 0 {
        map.remove(&token);
        return Err(RecoveryClaimError::Io(std::io::Error::new(
            std::io::ErrorKind::PermissionDenied,
            "injected authority key read failure",
        )));
    }
    Ok(())
}

/// Test-only count of COMPLETED origin enumerations per project root.
///
/// orgasmic:TASK-2QK4P.1.1.1 F2 — `one_authority_serves_every_claim_in_a_project`
/// counts files through `ProjectOriginAuthority::cost()`, which can only see
/// passes made through the object it was handed. The endpoint's defect is that
/// it builds TWO objects in one decision, so the count has to live outside both.
/// Keyed by project root because tests in this binary run concurrently under
/// their own temp roots.
#[cfg(test)]
static ORIGIN_ENUMERATION_PASSES: std::sync::Mutex<Option<BTreeMap<PathBuf, u32>>> =
    std::sync::Mutex::new(None);

#[cfg(test)]
fn record_origin_enumeration_pass(project_root: &Path) {
    *ORIGIN_ENUMERATION_PASSES
        .lock()
        .expect("origin enumeration counter lock")
        .get_or_insert_with(BTreeMap::new)
        .entry(project_root.to_path_buf())
        .or_insert(0) += 1;
}

/// Passes recorded for `project_root` since the process started.
#[cfg(test)]
pub fn origin_enumeration_passes(project_root: &Path) -> u32 {
    ORIGIN_ENUMERATION_PASSES
        .lock()
        .expect("origin enumeration counter lock")
        .as_ref()
        .and_then(|map| map.get(project_root).copied())
        .unwrap_or(0)
}

/// Take the hook only when it was armed for `replacement_run_id`.
#[cfg(test)]
fn take_pending_reconcile_after_open_hook(
    replacement_run_id: &str,
) -> Option<Box<dyn FnOnce() + Send>> {
    let mut slot = PENDING_RECONCILE_AFTER_OPEN_HOOK
        .lock()
        .expect("pending reconcile hook lock");
    match slot.as_ref() {
        Some((armed_for, _)) if armed_for == replacement_run_id => {
            slot.take().map(|(_, hook)| hook)
        }
        _ => None,
    }
}

#[derive(Debug, Clone, PartialEq)]
#[must_use = "a recovery-claim resolution states its own observability; dropping \
              it turns `I could not decide` back into `there is nothing here`"]
pub enum ResolvedRecoveryClaim {
    Valid(RecoveryClaim),
    Reconstructed(RecoveryClaim),
    InvalidQuarantined,
    Missing,
    /// The candidate set could not be enumerated, so nothing about this origin
    /// is decidable right now.
    ///
    /// orgasmic:TASK-2QK4P.1.1 — this is deliberately NOT
    /// [`Self::InvalidQuarantined`]. Both suppress recovery authority, but
    /// `InvalidQuarantined` also renames the claim on disk, and a transient
    /// read failure that did that would convert one failed observation into the
    /// permanent loss of a live rescue's idempotency: the claim is gone, the
    /// handler finds no pending plan, and it mints a second replacement beside
    /// the one already running. Unknown completeness is not invalid evidence.
    /// A caller must fail closed and retry, never act and never destroy.
    Unobserved(UnobservedEvidence),
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
/// `parent_fsync`, `association_pending`, `commit`, `cleanup`, and `response`.
pub fn recovery_failpoint(point: &str) {
    let Ok(raw) = std::env::var("ORGASMIC_RECOVERY_FAILPOINT") else {
        return;
    };
    if raw.split(',').map(str::trim).any(|token| token == point) {
        if let Ok(marker) = std::env::var("ORGASMIC_RECOVERY_FAILPOINT_BLOCK_FILE") {
            let _ = std::fs::write(marker, point);
            loop {
                std::thread::park_timeout(std::time::Duration::from_secs(60));
            }
        }
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
        && claim
            .authority_tag
            .as_deref()
            .is_some_and(|tag| tag.len() == 64 && tag.bytes().all(|b| b.is_ascii_hexdigit()))
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

/// Read the daemon-owned host auth material.
///
/// orgasmic:TASK-2QK4P.1.1.1 acceptance 1 — EVERY failure of this function is an
/// observation failure, never a statement about any claim, so it is reported as
/// one. `Path::exists()` was the entry point's own instance of the class: it
/// answers `false` for "the file is not there" AND for "I could not stat it",
/// and the `false` branch calls [`crate::auth::load_or_generate`], which mints
/// and WRITES a fresh token — invalidating every `authority_tag` on the host.
/// `try_exists` keeps the missing-file case (first boot legitimately generates)
/// and turns an unreadable one into a refusal.
fn authority_key(home: &Home) -> Result<Vec<u8>, RecoveryClaimError> {
    #[cfg(test)]
    authority_key_fault(home)?;
    // orgasmic:TASK-2QK4P.1.1.1.1 F5 — THE EXISTENCE PROBE IS ITS OWN
    // OBSERVATION AND IT IS OBSERVED SEPARATELY.
    //
    // Every `authority_key` test injected through `authority_key_fault` above,
    // which returns BEFORE control reaches this branch — including the
    // five-case behavioural pin. So the whole suite stayed green when
    // `try_exists` was swapped back for `exists()` plus `load_or_generate`, and
    // the host-token remint defect came straight back. A hook that shadows the
    // branch it is meant to protect is a test that cannot fail.
    //
    // `token_is_present` is therefore reached by a real stat failure, not by a
    // hook: `authority_key_stat_is_not_shadowed_by_the_read_fault_hook` removes
    // search permission from the token's parent directory, so `try_exists`
    // returns `Err(EACCES)` while the token bytes survive untouched. Under
    // `exists()` that same fixture answers `false` and MINTS.
    if !token_is_present(home)? {
        #[cfg(test)]
        record_load_or_generate_reached(home);
        crate::auth::load_or_generate(home).map_err(|_| RecoveryClaimError::CorruptClaim)?;
    }
    #[cfg(unix)]
    let mut file = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
        .open(home.auth_token())
        .map_err(RecoveryClaimError::Io)?;
    #[cfg(not(unix))]
    let mut file = File::open(home.auth_token()).map_err(RecoveryClaimError::Io)?;
    if !file.metadata().map_err(RecoveryClaimError::Io)?.is_file() {
        return Err(RecoveryClaimError::CorruptClaim);
    }
    let mut key = Vec::new();
    file.read_to_end(&mut key).map_err(RecoveryClaimError::Io)?;
    while key.last().is_some_and(|byte| byte.is_ascii_whitespace()) {
        key.pop();
    }
    if key.is_empty() {
        return Err(RecoveryClaimError::CorruptClaim);
    }
    Ok(key)
}

/// Is the host auth token THERE? Three answers, not two.
///
/// orgasmic:TASK-2QK4P.1.1.1.1 F5 — split out of [`authority_key`] so the stat
/// can be reasoned about and regressed on its own. `Path::exists()` answers
/// `false` for "not there" AND for "I could not stat it", and the `false`
/// branch WRITES a fresh token that invalidates every `authority_tag` on the
/// host. `try_exists` keeps first-boot generation and turns an unreadable
/// parent into a refusal — and the refusal is `Unobserved`, never
/// `CorruptClaim`, so nothing downstream quarantines on it.
fn token_is_present(home: &Home) -> Result<bool, RecoveryClaimError> {
    home.auth_token().try_exists().map_err(|err| {
        claim_io_error(
            err,
            UnobservedSession::AuthorityKeyUnreadable,
            Some("auth/token".to_string()),
        )
    })
}

fn authority_payload(claim: &RecoveryClaim) -> Result<Vec<u8>, RecoveryClaimError> {
    let mut normalized = claim.clone();
    normalized.authority_tag = None;
    normalized.status = RecoveryClaimStatus::Pending;
    serde_json::to_vec(&normalized).map_err(|_| RecoveryClaimError::CorruptClaim)
}

fn hmac_sha256(key: &[u8], message: &[u8]) -> [u8; 32] {
    const BLOCK: usize = 64;
    let mut normalized = [0u8; BLOCK];
    if key.len() > BLOCK {
        normalized[..32].copy_from_slice(&Sha256::digest(key));
    } else {
        normalized[..key.len()].copy_from_slice(key);
    }
    let mut inner_pad = [0x36u8; BLOCK];
    let mut outer_pad = [0x5cu8; BLOCK];
    for index in 0..BLOCK {
        inner_pad[index] ^= normalized[index];
        outer_pad[index] ^= normalized[index];
    }
    let mut inner = Sha256::new();
    inner.update(inner_pad);
    inner.update(message);
    let inner = inner.finalize();
    let mut outer = Sha256::new();
    outer.update(outer_pad);
    outer.update(inner);
    outer.finalize().into()
}

fn authority_tag(home: &Home, claim: &RecoveryClaim) -> Result<String, RecoveryClaimError> {
    let mac = hmac_sha256(&authority_key(home)?, &authority_payload(claim)?);
    Ok(mac.iter().map(|byte| format!("{byte:02x}")).collect())
}

/// Is this claim's `authority_tag` one this daemon minted?
///
/// orgasmic:TASK-2QK4P.1.1.1 F1(a) — THE ROUND-FOUR DEFECT, and the shape the
/// whole task is about. This returned `bool`, and `authority_tag` is fallible:
/// a key-file read failure arrived at every caller as "not authentic". The two
/// callers concluded opposite, both wrong, things from that:
///
/// - [`index_recovery_origins_in_session`] `continue`d, DROPPING the link, and
///   the enumerator still labelled the set `Complete` — so a transient failure
///   while indexing one of two authenticated links let the resolver return
///   `Valid` on the survivor. An observation failure became ABSENCE.
/// - [`verify_committed_claim_against_session`] and [`load_recovery_claim`]
///   read it as invalid evidence and QUARANTINED a live rescue's claim. An
///   observation failure became INVALID EVIDENCE.
///
/// A claim carrying no tag at all, or one whose recomputed tag differs, is a
/// verified negative and still returns [`ClaimEvidence::Invalid`].
fn claim_has_valid_authority(home: &Home, claim: &RecoveryClaim) -> ClaimEvidence {
    let Some(actual) = claim.authority_tag.as_deref() else {
        return ClaimEvidence::Invalid;
    };
    let key = match authority_key(home) {
        Ok(key) => key,
        // orgasmic:TASK-2QK4P.1.1.1.1 F3 — the evidence the leaf produced is
        // forwarded, subject and remediation intact, instead of being flattened
        // back to a bare tag one hop above the failure.
        Err(RecoveryClaimError::Unobserved(evidence)) => {
            return ClaimEvidence::Unobserved(evidence)
        }
        Err(_) => {
            return ClaimEvidence::Unobserved(UnobservedEvidence::about(
                UnobservedSession::AuthorityKeyUnreadable,
                "auth/token",
            ))
        }
    };
    // A claim that cannot be canonicalized cannot be the value this daemon
    // minted, because minting serializes it — so this is a decided negative
    // about the claim, not a failed observation. `serde_json` cannot in fact
    // fail on this shape; the arm exists so the classification is stated.
    let Ok(payload) = authority_payload(claim) else {
        return ClaimEvidence::Invalid;
    };
    let expected: String = hmac_sha256(&key, &payload)
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect();
    ClaimEvidence::verified(actual.as_bytes().ct_eq(expected.as_bytes()).into())
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

#[cfg(unix)]
struct ClaimDirectory {
    file: File,
    /// Canonical state root this handle was opened under. Identity for the
    /// per-home `readdir` fault seam, so two tests running concurrently in one
    /// process cannot arm each other's directory.
    #[cfg_attr(not(test), allow(dead_code))]
    state_root: PathBuf,
}

#[cfg(unix)]
impl ClaimDirectory {
    /// Open the per-project claim directory, or state that it is absent.
    ///
    /// orgasmic:TASK-2QK4P.1.1.1.1 F1 — THIS FUNCTION SAT ONE CALL OUTSIDE
    /// ROUND FOUR'S HAND-DRAWN BOUNDARY and it carried the defect the boundary
    /// existed to hunt. Every component-open error other than `NotFound` became
    /// `CorruptClaim`, a bucket that held `EACCES`, `EIO` and descriptor
    /// exhaustion — observation failures — beside `ELOOP`/`ENOTDIR`, which are
    /// observed path facts. `load_recovery_claim` propagated it and
    /// `resolve_authoritative_recovery_claim` read it as invalid evidence:
    /// quarantine, then reconstruct or `Missing`. For a PENDING claim there is
    /// no committed `RecoveryOrigin` to reconstruct from, so a failed inventory
    /// read removed the pending claim and the next POST saw a complete origin
    /// enumeration plus no claim and minted a competitor beside a live rescue.
    ///
    /// The split is now [`classify_observation`]'s, made once, and every arm
    /// below names which of the three answers it is giving.
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
        //
        // orgasmic:TASK-2QK4P.1.1.1.1 F4 — the state root is the daemon's own,
        // so a failure to canonicalize or open it says nothing about any claim.
        // It used to arrive at the API as a raw `Io` and therefore a 500.
        let state_root = home
            .state()
            .canonicalize()
            .map_err(|err| claim_io_error(err, UnobservedSession::ClaimStoreUnreadable, None))?;
        let state = state_root.clone();
        let mut current = OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC)
            .open(state)
            .map_err(|err| claim_io_error(err, UnobservedSession::ClaimStoreUnreadable, None))?;
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
                match classify_observation(&err) {
                    // Absent, and we were asked to create it.
                    ObservationClass::Absent if create => {
                        if unsafe { libc::mkdirat(current.as_raw_fd(), name.as_ptr(), 0o700) } != 0
                        {
                            let mkdir_err = std::io::Error::last_os_error();
                            if mkdir_err.kind() != std::io::ErrorKind::AlreadyExists {
                                return Err(claim_io_error(
                                    mkdir_err,
                                    UnobservedSession::ClaimStoreUnreadable,
                                    Some(component.to_string()),
                                ));
                            }
                        }
                        current.sync_all().map_err(|err| {
                            claim_io_error(err, UnobservedSession::ClaimStoreUnreadable, None)
                        })?;
                        recovery_failpoint("parent_fsync");
                        fd = open();
                    }
                    // Absent, and absence is the answer: no claims here.
                    ObservationClass::Absent => return Ok(None),
                    // The kernel described the path and the description
                    // disqualifies it — a symlink or a non-directory where a
                    // daemon-owned directory must be. Decided about evidence.
                    ObservationClass::Decided => return Err(RecoveryClaimError::CorruptClaim),
                    // EACCES / EIO / EMFILE. NOTHING is known about any claim
                    // under this directory, so nothing may be quarantined.
                    ObservationClass::Unobserved => {
                        return Err(RecoveryClaimError::Unobserved(UnobservedEvidence::about(
                            UnobservedSession::ClaimStoreUnreadable,
                            component,
                        )))
                    }
                }
            }
            if fd < 0 {
                // Only reachable from the create-then-reopen path above.
                return Err(claim_io_error(
                    std::io::Error::last_os_error(),
                    UnobservedSession::ClaimStoreUnreadable,
                    Some(component.to_string()),
                ));
            }
            current = unsafe { File::from_raw_fd(fd) };
            if !current
                .metadata()
                .map_err(|err| {
                    claim_io_error(
                        err,
                        UnobservedSession::ClaimStoreUnreadable,
                        Some(component.to_string()),
                    )
                })?
                .is_dir()
            {
                // Observed, and it is not a directory. A decided fact.
                return Err(RecoveryClaimError::CorruptClaim);
            }
        }
        Ok(Some(Self {
            file: current,
            state_root: state_root.clone(),
        }))
    }

    /// Open a member by its raw directory-entry NAME.
    ///
    /// orgasmic:TASK-2QK4P.1.1.1.1 F2 — the name is `OsStr`, not `str`. A
    /// directory entry is bytes; requiring it to decode as UTF-8 first is what
    /// let [`Self::names`] silently drop an entry, and a lossy re-encoding
    /// would be worse still because the mangled name opens as `NotFound`, which
    /// every caller reads as "absent".
    fn open_file(
        &self,
        name: impl AsRef<std::ffi::OsStr>,
        flags: libc::c_int,
        mode: libc::mode_t,
    ) -> std::io::Result<File> {
        use std::os::fd::{AsRawFd, FromRawFd};
        use std::os::unix::ffi::OsStrExt;
        let name = std::ffi::CString::new(name.as_ref().as_bytes())
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

    /// Read one claim file whole.
    ///
    /// orgasmic:TASK-2QK4P.1.1.1.1 F1 — the second function outside round
    /// four's boundary. `_ => CorruptClaim` on the open error swept `EACCES`,
    /// `EIO` and `EMFILE` into the bucket the resolver QUARANTINES on, and the
    /// `metadata`/`read_to_string` failures below stayed raw `Io` and therefore
    /// became 500s (F4). All three now go through the one policy.
    fn read_regular(
        &self,
        name: impl AsRef<std::ffi::OsStr>,
    ) -> Result<String, RecoveryClaimError> {
        use std::io::Read;
        let name = name.as_ref();
        let subject = || Some(sanitized_subject(Path::new(""), Path::new(name)));
        let mut file = self.open_file(name, libc::O_RDONLY, 0).map_err(|err| {
            claim_io_error(err, UnobservedSession::ClaimFileUnreadable, subject())
        })?;
        if !file
            .metadata()
            .map_err(|err| claim_io_error(err, UnobservedSession::ClaimFileUnreadable, subject()))?
            .is_file()
        {
            // Observed, and it is not a regular file. Decided.
            return Err(RecoveryClaimError::CorruptClaim);
        }
        let mut raw = String::new();
        file.read_to_string(&mut raw).map_err(|err| {
            claim_io_error(err, UnobservedSession::ClaimFileUnreadable, subject())
        })?;
        Ok(raw)
    }

    fn rename(
        &self,
        from: impl AsRef<std::ffi::OsStr>,
        to: impl AsRef<std::ffi::OsStr>,
    ) -> Result<(), RecoveryClaimError> {
        use std::os::fd::AsRawFd;
        use std::os::unix::ffi::OsStrExt;
        let from = std::ffi::CString::new(from.as_ref().as_bytes())
            .map_err(|_| RecoveryClaimError::InvalidIdentifier)?;
        let to = std::ffi::CString::new(to.as_ref().as_bytes())
            .map_err(|_| RecoveryClaimError::InvalidIdentifier)?;
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

    fn remove(&self, name: impl AsRef<std::ffi::OsStr>) -> Result<bool, RecoveryClaimError> {
        use std::os::fd::AsRawFd;
        use std::os::unix::ffi::OsStrExt;
        let name = std::ffi::CString::new(name.as_ref().as_bytes())
            .map_err(|_| RecoveryClaimError::InvalidIdentifier)?;
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

    /// Every entry name in the claim directory, or the statement that the
    /// listing did not complete.
    ///
    /// orgasmic:TASK-2QK4P.1.1.1.1 F2 — THE THIRD FUNCTION OUTSIDE ROUND FOUR'S
    /// BOUNDARY, and it collapsed the same distinction THREE separate ways in
    /// one loop:
    ///
    /// 1. `readdir` returns NULL at end-of-directory AND on error, and the only
    ///    way to tell is to clear `errno` before the call and read it after.
    ///    This did neither, so a mid-listing `EIO` was returned as
    ///    `Ok(a complete directory)`.
    /// 2. `closedir`'s return was discarded, so a deferred error surfaced at
    ///    close was thrown away.
    /// 3. `if let Ok(name) = name.to_str()` SILENTLY DROPPED any entry whose
    ///    name is not UTF-8, making the set smaller — the unsafe direction, for
    ///    the same reason it was unsafe in round three.
    ///
    /// The caller that matters is `pending_recovery_claim_owns_session`, which
    /// trusts this vector as complete: hide the pending claim owning a boot
    /// candidate and it answers `Invalid`, boot enters generic reattach, and
    /// `Supervisor::reattach` appends `Reattach` into the immutable prefix that
    /// pending recovery owns. That write is not undoable.
    ///
    /// # What a non-UTF-8 entry name means, decided deliberately
    ///
    /// It means THERE IS AN ENTRY WHOSE NAME IS THOSE BYTES. It does not mean
    /// absence, and it is not an error either — an unrelated file with a Latin-1
    /// name in the claim directory must not freeze the project's recovery. So
    /// the whole API works in `OsString` and the bytes are carried through
    /// unchanged; a name that is not valid UTF-8 simply fails the `.json`
    /// filter its callers apply, which is a DECIDED answer about that entry
    /// rather than a silent shrink of the set. (Lossy conversion was rejected:
    /// the mangled name opens as `NotFound`, which reads as "absent" — the
    /// exact collapse, one layer down.)
    fn names(&self) -> Result<Vec<std::ffi::OsString>, RecoveryClaimError> {
        use std::ffi::{CStr, OsStr};
        use std::os::fd::AsRawFd;
        use std::os::unix::ffi::OsStrExt;

        let duplicate = unsafe { libc::dup(self.file.as_raw_fd()) };
        if duplicate < 0 {
            return Err(claim_io_error(
                std::io::Error::last_os_error(),
                UnobservedSession::ClaimStoreUnreadable,
                None,
            ));
        }
        let dir = unsafe { libc::fdopendir(duplicate) };
        if dir.is_null() {
            let err = std::io::Error::last_os_error();
            unsafe { libc::close(duplicate) };
            return Err(claim_io_error(
                err,
                UnobservedSession::ClaimStoreUnreadable,
                None,
            ));
        }
        // `closedir` also closes `duplicate`; it must run on every exit below.
        let close = |dir: *mut libc::DIR| -> Result<(), RecoveryClaimError> {
            if unsafe { libc::closedir(dir) } != 0 {
                return Err(claim_io_error(
                    std::io::Error::last_os_error(),
                    UnobservedSession::ClaimStoreUnreadable,
                    None,
                ));
            }
            Ok(())
        };
        let mut names: Vec<std::ffi::OsString> = Vec::new();
        // orgasmic:TASK-2QK4P.1.1.1.1.1 P2a — THE PER-ENTRY DECISION, IN ONE
        // PLACE. An entry name is BYTES: `.`/`..` are the only names this drops,
        // and everything else is carried through unchanged for the caller's
        // `.json` filter to decide about. It is a closure rather than two copies
        // because the test seam below delivers its bytes HERE: a future edit
        // that reintroduces the `to_str()` drop has exactly one place to put it,
        // and the injected-name regression then reds on every platform.
        let collect = |names: &mut Vec<std::ffi::OsString>, raw: &[u8]| {
            if raw == b"." || raw == b".." {
                return;
            }
            names.push(OsStr::from_bytes(raw).to_os_string());
        };
        #[cfg(test)]
        let mut iteration = 0u32;
        loop {
            // The whole point: NULL alone cannot distinguish EOF from failure.
            errno::set_errno(errno::Errno(0));
            #[cfg(test)]
            {
                iteration += 1;
                if let Some(bytes) = readdir_entry_name(&self.state_root, iteration) {
                    // Deliver the fixture the way the kernel would: a real
                    // `dirent` whose NUL-terminated `d_name` holds those bytes,
                    // read back through the same `CStr::from_ptr(..).to_bytes()`
                    // as the syscall path, into the same `collect`. The
                    // directory stream is NOT advanced, so every real entry is
                    // still listed after it.
                    let mut entry: libc::dirent = unsafe { std::mem::zeroed() };
                    assert!(
                        bytes.len() < entry.d_name.len() && !bytes.contains(&0),
                        "an injected entry name must fit `d_name` and carry no NUL"
                    );
                    for (slot, byte) in entry.d_name.iter_mut().zip(bytes.iter()) {
                        *slot = *byte as libc::c_char;
                    }
                    let raw = unsafe { CStr::from_ptr(entry.d_name.as_ptr()) }.to_bytes();
                    collect(&mut names, raw);
                    continue;
                }
                if let Some(code) = readdir_fault(&self.state_root, iteration) {
                    // Reproduce the real shape exactly: an errno-bearing NULL,
                    // which the pre-fix loop `break`s on and reports as a
                    // COMPLETE directory.
                    errno::set_errno(errno::Errno(code));
                }
            }
            let entry = unsafe {
                #[cfg(test)]
                if errno::errno().0 != 0 {
                    std::ptr::null_mut()
                } else {
                    libc::readdir(dir)
                }
                #[cfg(not(test))]
                libc::readdir(dir)
            };
            if entry.is_null() {
                let code = errno::errno().0;
                if code != 0 {
                    let err = std::io::Error::from_raw_os_error(code);
                    let _ = close(dir);
                    return Err(claim_io_error(
                        err,
                        UnobservedSession::ClaimStoreUnreadable,
                        None,
                    ));
                }
                break;
            }
            let raw = unsafe { CStr::from_ptr((*entry).d_name.as_ptr()) }.to_bytes();
            collect(&mut names, raw);
        }
        close(dir)?;
        Ok(names)
    }

    fn sync(&self) -> Result<(), RecoveryClaimError> {
        self.file.sync_all().map_err(RecoveryClaimError::Io)
    }
}

#[cfg(unix)]
#[derive(Clone, Debug)]
pub(crate) struct SessionDirectory {
    file: Arc<File>,
    canonical_path: PathBuf,
}

#[cfg(unix)]
#[derive(Clone, Debug)]
pub struct SessionFile {
    directory: SessionDirectory,
    name: String,
    file: Arc<File>,
    device: u64,
    inode: u64,
}

#[cfg(unix)]
impl SessionDirectory {
    pub(crate) fn open(project_root: &Path) -> Result<Self, RecoveryClaimError> {
        use std::os::fd::{AsRawFd, FromRawFd};

        let canonical_root = project_root
            .canonicalize()
            .map_err(RecoveryClaimError::Io)?;
        let mut current = OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC)
            .open(&canonical_root)
            .map_err(RecoveryClaimError::Io)?;
        for component in [".orgasmic", "tmp", "sessions"] {
            let name = std::ffi::CString::new(component)
                .map_err(|_| RecoveryClaimError::InvalidIdentifier)?;
            let fd = unsafe {
                libc::openat(
                    current.as_raw_fd(),
                    name.as_ptr(),
                    libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC,
                )
            };
            if fd < 0 {
                return Err(RecoveryClaimError::Io(std::io::Error::last_os_error()));
            }
            current = unsafe { File::from_raw_fd(fd) };
            if !current.metadata().map_err(RecoveryClaimError::Io)?.is_dir() {
                return Err(RecoveryClaimError::CorruptClaim);
            }
        }
        Ok(Self {
            file: Arc::new(current),
            canonical_path: canonical_root.join(".orgasmic/tmp/sessions"),
        })
    }

    fn name_for_path(&self, path: &Path) -> Result<String, RecoveryClaimError> {
        let parent = path.parent().ok_or(RecoveryClaimError::CorruptClaim)?;
        // The caller may hold a lexical macOS `/var/...` project path while
        // the retained directory authority resolves to `/private/var/...`.
        // Canonicalize only the parent for membership; the actual file is
        // still opened relative to the retained directory fd with O_NOFOLLOW.
        // orgasmic:TASK-2QK4P.1.1.1 — the two failures here are different facts.
        // A parent that RESOLVES ELSEWHERE is decided: the path is not a member.
        // A canonicalize that FAILS for any reason other than `NotFound` decided
        // nothing, and its `io` error is preserved so `session_read_evidence`
        // can tell them apart instead of collapsing both into "not a member".
        let resolved_parent = match parent.canonicalize() {
            Ok(resolved) => resolved,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
                return Err(RecoveryClaimError::ForeignSessionPath)
            }
            Err(err) => return Err(RecoveryClaimError::Io(err)),
        };
        if resolved_parent != self.canonical_path {
            return Err(RecoveryClaimError::ForeignSessionPath);
        }
        let name = path
            .file_name()
            .and_then(|name| name.to_str())
            .ok_or(RecoveryClaimError::InvalidIdentifier)?;
        if !validate_safe_component(name) || !name.ends_with(".jsonl") {
            return Err(RecoveryClaimError::InvalidIdentifier);
        }
        Ok(name.to_string())
    }

    fn read_path(&self, path: &Path) -> Result<Vec<SessionEnvelope>, RecoveryClaimError> {
        self.open_path(path, false)?.read_checked()
    }

    /// Bounded lifecycle read through the same pinned-directory, identity-
    /// validated handle as [`Self::read_path`]. Origin indexing only needs
    /// lifecycle envelopes, so it must not pay transcript bytes.
    fn scan_path(
        &self,
        path: &Path,
        budget: SessionScanBudget,
    ) -> Result<SessionLifecycleScan, RecoveryClaimError> {
        self.open_path(path, false)?.scan_lifecycle_checked(budget)
    }

    /// [`Self::scan_path`] with no budget: every line is examined, so the
    /// result carries no skipped middle. The escalation
    /// [`complete_session_scan`] reaches for when a bounded scan truncated.
    fn scan_path_complete(&self, path: &Path) -> Result<SessionLifecycleScan, RecoveryClaimError> {
        self.open_path(path, false)?
            .scan_lifecycle_complete_checked()
    }

    pub(crate) fn open_path(
        &self,
        path: &Path,
        writable: bool,
    ) -> Result<SessionFile, RecoveryClaimError> {
        let name = self.name_for_path(path)?;
        self.open_name(&name, writable)
    }

    pub(crate) fn create_path(&self, path: &Path) -> Result<SessionFile, RecoveryClaimError> {
        let name = self.name_for_path(path)?;
        self.open_name_with_flags(
            &name,
            libc::O_RDWR | libc::O_APPEND | libc::O_CREAT | libc::O_EXCL,
            0o600,
        )
    }

    fn open_name(&self, name: &str, writable: bool) -> Result<SessionFile, RecoveryClaimError> {
        let flags = if writable {
            libc::O_RDWR | libc::O_APPEND
        } else {
            libc::O_RDONLY
        };
        self.open_name_with_flags(name, flags, 0)
    }

    fn open_name_with_flags(
        &self,
        name: &str,
        flags: libc::c_int,
        mode: libc::mode_t,
    ) -> Result<SessionFile, RecoveryClaimError> {
        use std::os::fd::{AsRawFd, FromRawFd};
        if !validate_safe_component(name) || !name.ends_with(".jsonl") {
            return Err(RecoveryClaimError::InvalidIdentifier);
        }
        let c_name =
            std::ffi::CString::new(name).map_err(|_| RecoveryClaimError::InvalidIdentifier)?;
        let fd = unsafe {
            libc::openat(
                self.file.as_raw_fd(),
                c_name.as_ptr(),
                flags | libc::O_NOFOLLOW | libc::O_CLOEXEC,
                libc::c_uint::from(mode),
            )
        };
        if fd < 0 {
            return Err(RecoveryClaimError::Io(std::io::Error::last_os_error()));
        }
        let file = unsafe { File::from_raw_fd(fd) };
        let metadata = file.metadata().map_err(RecoveryClaimError::Io)?;
        if !metadata.is_file() {
            // Decided: the name exists and is not a regular file.
            return Err(RecoveryClaimError::ForeignSessionPath);
        }
        use std::os::unix::fs::MetadataExt;
        Ok(SessionFile {
            directory: self.clone(),
            name: name.to_string(),
            file: Arc::new(file),
            device: metadata.dev(),
            inode: metadata.ino(),
        })
    }
}

#[cfg(unix)]
impl SessionFile {
    pub(crate) fn authorizes_path(&self, path: &Path) -> Result<bool, RecoveryClaimError> {
        Ok(self.directory.name_for_path(path)? == self.name)
    }

    pub(crate) fn read_checked(&self) -> Result<Vec<SessionEnvelope>, RecoveryClaimError> {
        self.validate_current()?;
        use std::io::{Seek, SeekFrom};
        let mut file = self.file.try_clone().map_err(RecoveryClaimError::Io)?;
        file.seek(SeekFrom::Start(0))
            .map_err(RecoveryClaimError::Io)?;
        let mut raw = String::new();
        file.read_to_string(&mut raw)
            .map_err(RecoveryClaimError::Io)?;
        parse_session_raw(&raw)
    }

    /// [`Self::read_checked`] restricted to lifecycle envelopes and a byte
    /// budget. Identity is validated identically before any read.
    pub(crate) fn scan_lifecycle_checked(
        &self,
        budget: SessionScanBudget,
    ) -> Result<SessionLifecycleScan, RecoveryClaimError> {
        self.validate_current()?;
        let mut file = self.file.try_clone().map_err(RecoveryClaimError::Io)?;
        let file_bytes = file.metadata().map_err(RecoveryClaimError::Io)?.len();
        orgasmic_core::scan_session_lifecycle_reader(&mut file, file_bytes, budget)
            .map_err(|_| RecoveryClaimError::CorruptClaim)
    }

    pub(crate) fn scan_lifecycle_complete_checked(
        &self,
    ) -> Result<SessionLifecycleScan, RecoveryClaimError> {
        self.validate_current()?;
        let mut file = self.file.try_clone().map_err(RecoveryClaimError::Io)?;
        let file_bytes = file.metadata().map_err(RecoveryClaimError::Io)?.len();
        orgasmic_core::scan_session_lifecycle_complete_reader(&mut file, file_bytes)
            .map_err(|_| RecoveryClaimError::CorruptClaim)
    }

    pub(crate) fn validate_current(&self) -> Result<(), RecoveryClaimError> {
        let current = self.directory.open_name(&self.name, false)?;
        if current.device != self.device || current.inode != self.inode {
            return Err(RecoveryClaimError::CorruptClaim);
        }
        Ok(())
    }

    pub(crate) fn clone_file_for_append(&self) -> Result<File, RecoveryClaimError> {
        self.validate_current()?;
        self.file.try_clone().map_err(RecoveryClaimError::Io)
    }
}

fn parse_session_raw(raw: &str) -> Result<Vec<SessionEnvelope>, RecoveryClaimError> {
    raw.lines()
        .filter(|line| !line.trim().is_empty())
        .map(|line| serde_json::from_str(line).map_err(|_| RecoveryClaimError::CorruptClaim))
        .collect()
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
    // orgasmic:TASK-2QK4P.1.1.1 acceptance 1 — `is_ok()` here meant "a real
    // claim already exists, leave the temps alone", and its `false` also meant
    // "I could not read it". The `false` path can RENAME a temp onto that exact
    // name, so an unreadable final claim would have been overwritten by a
    // crash-interrupted one.
    match dir.read_regular(&final_name) {
        Ok(_) => return Ok(()),
        Err(RecoveryClaimError::Io(err)) if err.kind() == std::io::ErrorKind::NotFound => {}
        Err(err) => return Err(err),
    }
    let prefix = format!("{final_name}.tmp.");
    let mut valid = Vec::new();
    for name in dir
        .names()?
        .into_iter()
        .filter(|name| name.as_encoded_bytes().starts_with(prefix.as_bytes()))
    {
        // orgasmic:TASK-2QK4P.1.1.1 acceptance 1 — the `else` arm below DELETES,
        // so a predicate that could not look may not reach it. The authority
        // check is the fallible one: an unreadable key file used to answer
        // `false` here and remove a crash-interrupted temp claim that the very
        // next retry would have promoted to the real one.
        let raw = match dir.read_regular(&name) {
            Ok(raw) => Some(raw),
            // The entry went away between `names()` and here; there is nothing
            // left to delete and nothing to promote.
            Err(RecoveryClaimError::Io(err)) if err.kind() == std::io::ErrorKind::NotFound => {
                continue
            }
            Err(err) => return Err(err),
        };
        let parsed = raw
            .and_then(|raw| serde_json::from_str::<RecoveryClaim>(&raw).ok())
            .filter(|claim| {
                claim.project_id == project_id
                    && claim.origin_run_id == origin_run_id
                    && recovery_claim_has_complete_plan(claim)
            });
        let authentic = match parsed
            .as_ref()
            .map(|claim| claim_has_valid_authority(home, claim))
        {
            None | Some(ClaimEvidence::Invalid) => false,
            Some(ClaimEvidence::Valid) => true,
            Some(ClaimEvidence::Unobserved(reason)) => {
                return Err(RecoveryClaimError::Unobserved(reason))
            }
        };
        if authentic {
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
    if !recovery_claim_has_complete_plan(&claim) {
        return Err(RecoveryClaimError::CorruptClaim);
    }
    // orgasmic:TASK-2QK4P.1.1.1 F1(b) — `CorruptClaim` here is what the resolver
    // QUARANTINES on, so an unreadable auth key must not spell itself that way:
    // it would rename a live rescue's claim on a transient failure and the
    // handler would then mint a competitor beside the running replacement.
    match claim_has_valid_authority(home, &claim) {
        ClaimEvidence::Valid => Ok(Some(claim)),
        ClaimEvidence::Invalid => Err(RecoveryClaimError::CorruptClaim),
        ClaimEvidence::Unobserved(reason) => Err(RecoveryClaimError::Unobserved(reason)),
    }
}

/// Routing guard for daemon boot reattach. A pending recovery owns the exact
/// deterministic replacement handle and must be reconciled by POST /recover,
/// which validates the complete plan and backfills lifecycle events in order.
/// Boot's generic reattach pass therefore skips that session instead of
/// inserting a `Reattach` event into the immutable partial prefix.
///
/// This is only a routing hint: recovery authorization still comes from the
/// full handle-bound claim/session verification under the per-origin lock.
///
/// orgasmic:TASK-2QK4P.1.1.1 acceptance 1 — a hint, but a fail-OPEN one, so it
/// is in the enumeration. Every failure here used to answer `false` = "no
/// pending recovery owns this session", and the caller then let boot's generic
/// reattach insert a `Reattach` event into a prefix a pending claim owns. A
/// [`ClaimEvidence::Unobserved`] answer must be treated as ownership by the
/// caller: skipping a reattach is retried on the next boot, while writing into
/// the reserved prefix is not undoable.
pub fn pending_recovery_claim_owns_session(
    home: &Home,
    project_root: &Path,
    project_id: &str,
    session_path: &Path,
) -> ClaimEvidence {
    #[cfg(unix)]
    {
        let candidate = sanitized_subject(project_root, session_path);
        let session_dir = match SessionDirectory::open(project_root) {
            Ok(dir) => dir,
            Err(err) => {
                return evidence_about(
                    session_read_evidence(&err, UnobservedSession::SessionDirectoryUnavailable),
                    candidate,
                )
            }
        };
        let candidate_name = match session_dir.name_for_path(session_path) {
            Ok(name) => name,
            // The session being reattached is not a member of this project's
            // sessions directory, so no claim of this project can own it.
            Err(RecoveryClaimError::ForeignSessionPath)
            | Err(RecoveryClaimError::InvalidIdentifier) => return ClaimEvidence::Invalid,
            Err(err) => {
                return evidence_about(
                    session_read_evidence(&err, UnobservedSession::SessionPathUnresolvable),
                    candidate,
                )
            }
        };
        let dir = match ClaimDirectory::open(home, project_id, false) {
            Ok(Some(dir)) => dir,
            // No claim directory at all is a decided absence.
            Ok(None) => return ClaimEvidence::Invalid,
            Err(err) => {
                return evidence_about(
                    session_read_evidence(&err, UnobservedSession::ClaimStoreUnreadable),
                    format!("recovery-claims/{project_id}"),
                )
            }
        };
        // orgasmic:TASK-2QK4P.1.1.1.1 F2 — this is the caller the finding names.
        // A failed `readdir` used to arrive here as `Ok(a complete directory)`,
        // and a pending claim hidden by it makes this function answer `Invalid`,
        // which lets boot append `Reattach` into the prefix that pending
        // recovery owns. That write is irreversible; a skipped reattach is not.
        let names = match dir.names() {
            Ok(names) => names,
            Err(err) => {
                return evidence_about(
                    session_read_evidence(&err, UnobservedSession::ClaimStoreUnreadable),
                    format!("recovery-claims/{project_id}"),
                )
            }
        };
        for name in names {
            // orgasmic:TASK-2QK4P.1.1.1.1 F2 — entry names are BYTES. An entry
            // whose name is not UTF-8 is decidably not a claim file (every name
            // this daemon writes is a `validate_safe_component` id plus
            // `.json`, which is ASCII), and saying so here is a decision about
            // that entry rather than the silent drop `names()` used to do.
            if !name.as_encoded_bytes().ends_with(b".json") {
                continue;
            }
            let raw = match dir.read_regular(&name) {
                Ok(raw) => raw,
                Err(RecoveryClaimError::Io(err)) if err.kind() == std::io::ErrorKind::NotFound => {
                    continue
                }
                Err(err) => {
                    return evidence_about(
                        session_read_evidence(&err, UnobservedSession::ClaimFileUnreadable),
                        format!("recovery-claims/{}", name.to_string_lossy()),
                    )
                }
            };
            let Ok(claim) = serde_json::from_str::<RecoveryClaim>(&raw) else {
                continue;
            };
            if claim.status != RecoveryClaimStatus::Pending
                || claim.project_id != project_id
                || !recovery_claim_has_complete_plan(&claim)
            {
                continue;
            }
            match claim_has_valid_authority(home, &claim) {
                ClaimEvidence::Valid => {}
                ClaimEvidence::Invalid => continue,
                unobserved @ ClaimEvidence::Unobserved(_) => return unobserved,
            }
            match session_dir.name_for_path(&claim.replacement_session_path) {
                Ok(name) if name == candidate_name => return ClaimEvidence::Valid,
                Ok(_)
                | Err(RecoveryClaimError::ForeignSessionPath)
                | Err(RecoveryClaimError::InvalidIdentifier) => continue,
                Err(err) => {
                    return evidence_about(
                        session_read_evidence(&err, UnobservedSession::SessionPathUnresolvable),
                        sanitized_subject(project_root, &claim.replacement_session_path),
                    )
                }
            }
        }
        ClaimEvidence::Invalid
    }
    #[cfg(not(unix))]
    {
        let _ = project_root;
        let root = recovery_claims_root(home).join(project_id);
        let entries = match std::fs::read_dir(root) {
            Ok(entries) => entries,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
                return ClaimEvidence::Invalid
            }
            Err(_) => {
                return ClaimEvidence::Unobserved(UnobservedEvidence::new(
                    UnobservedSession::SessionDirectoryUnavailable,
                ))
            }
        };
        for entry in entries {
            let Ok(entry) = entry else {
                return ClaimEvidence::Unobserved(UnobservedEvidence::new(
                    UnobservedSession::SessionDirectoryUnavailable,
                ));
            };
            let raw = match std::fs::read_to_string(entry.path()) {
                Ok(raw) => raw,
                Err(err) if err.kind() == std::io::ErrorKind::NotFound => continue,
                Err(_) => {
                    return ClaimEvidence::Unobserved(UnobservedEvidence::new(
                        UnobservedSession::SessionUnreadable,
                    ))
                }
            };
            let Ok(claim) = serde_json::from_str::<RecoveryClaim>(&raw) else {
                continue;
            };
            if claim.status == RecoveryClaimStatus::Pending
                && claim.project_id == project_id
                && claim.replacement_session_path == session_path
                && recovery_claim_has_complete_plan(&claim)
            {
                return ClaimEvidence::Valid;
            }
        }
        ClaimEvidence::Invalid
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
    pub project_root: PathBuf,
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

/// Why one origin-index pass could not state what a session file contains.
///
/// Every variant is an OBSERVATION failure, never a statement about the file's
/// contents. That distinction is the whole point: an unreadable file is not an
/// empty one, and a claim must not be renamed invalid because a read failed
/// (orgasmic:TASK-2QK4P.1.1, reviewer open question 1).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UnobservedSession {
    /// The project's pinned sessions directory could not be opened.
    SessionDirectoryUnavailable,
    /// The session file could not be opened, read, or parsed — including a
    /// malformed lifecycle-relevant line, which the scanner rejects the file
    /// for rather than skipping.
    SessionUnreadable,
    /// A `RecoveryOrigin` named an origin session that could not be read, so
    /// the link can neither be admitted nor dismissed.
    OriginSessionUnreadable,
    /// The daemon's host auth material could not be read, so no `authority_tag`
    /// can be recomputed and NOTHING is known about any claim's authenticity.
    ///
    /// orgasmic:TASK-2QK4P.1.1.1 F1(a) — this is the round-four instance of the
    /// class. `authority_tag` is fallible and its failure used to arrive at
    /// every caller as the boolean `false`, which reads as "not authentic".
    AuthorityKeyUnreadable,
    /// A path naming a session file could not be resolved against the pinned
    /// sessions directory because the resolution itself failed — not because
    /// the path resolved elsewhere, which is a decidable answer.
    SessionPathUnresolvable,
    /// The daemon-owned claim store — the state root, `recovery-claims/`, or
    /// the per-project directory under it — could not be opened or listed.
    ///
    /// orgasmic:TASK-2QK4P.1.1.1.1 F1/F2 — [`ClaimDirectory::open`] used to
    /// answer `CorruptClaim` for every non-`NotFound` component-open error and
    /// [`ClaimDirectory::names`] used to answer `Ok(complete set)` for a failed
    /// `readdir`. Both are observation failures wearing a decided answer's
    /// clothes, and both sat one call outside round four's hand-drawn boundary.
    ClaimStoreUnreadable,
    /// A claim file inside the store could not be opened, stat'd or read.
    ClaimFileUnreadable,
    /// The daemon could not determine whether the exact tmux session reserved
    /// by a spawned recovery claim still exists. A tmux client/probe failure
    /// is not evidence that the replacement died.
    TmuxHandleUnobserved,
}

/// What an operator would have to DO about an unobserved answer.
///
/// orgasmic:TASK-2QK4P.1.1.1.1 F3 — a permanent refusal that names no file and
/// no action is not shippable, and that is what main carries today: one junk
/// line in one session file refuses every recovery in the project forever, and
/// the 503 says only `SessionUnreadable`. The reason tag says what the daemon
/// could not do; this says what would make it able to. Every class is ALSO
/// retryable — a 503 always permits a retry — so there is no `Retry` variant to
/// mistake for "and nothing else will help".
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Remediation {
    /// A named session file under the project's `sessions/` directory holds
    /// content the strict scanner rejects, or cannot be read. Repair or move
    /// that one file aside; recovery resumes on the next request.
    RepairSessionFile,
    /// The project's `sessions/` directory itself could not be opened or a
    /// member path could not be resolved against it.
    RepairSessionStore,
    /// The daemon's own host auth material is unreadable, so no `authority_tag`
    /// can be recomputed. Restore read access to `<home>/auth/token`; do NOT
    /// delete it — a regenerated token invalidates every live claim's tag.
    RepairAuthKey,
    /// The daemon-owned claim store under `<home>/state/recovery-claims/` could
    /// not be opened, listed or read.
    RepairClaimStore,
    /// The daemon could not query tmux for a spawned recovery claim's exact
    /// planned session.
    RepairTmux,
}

impl Remediation {
    /// Stable machine-readable class for the API body and the operator UI.
    pub fn class(self) -> &'static str {
        match self {
            Self::RepairSessionFile => "repair_session_file",
            Self::RepairSessionStore => "repair_session_store",
            Self::RepairAuthKey => "repair_auth_key",
            Self::RepairClaimStore => "repair_claim_store",
            Self::RepairTmux => "repair_tmux",
        }
    }

    /// The documented repair, in one sentence an operator can act on.
    pub fn hint(self) -> &'static str {
        match self {
            Self::RepairSessionFile => {
                "The named session file could not be read as a complete event log. \
                 Restore read access to it, or move that one file out of the \
                 project's .orgasmic/sessions/ directory to quarantine it; \
                 recovery resumes on the next request."
            }
            Self::RepairSessionStore => {
                "The project's .orgasmic/sessions/ directory could not be opened. \
                 Restore read and execute access to it and retry."
            }
            Self::RepairAuthKey => {
                "The daemon could not read its host auth material at <home>/auth/token. \
                 Restore read access to that file — do not delete or regenerate it, \
                 which would invalidate every live recovery claim."
            }
            Self::RepairClaimStore => {
                "The daemon-owned claim store under <home>/state/recovery-claims/ \
                 could not be opened, listed or read. Restore read and execute \
                 access to it and retry."
            }
            Self::RepairTmux => {
                "The daemon could not query tmux for the planned recovery session. \
                 Restore the local tmux client/server and retry; the pending claim \
                 remains unchanged until the session can be observed."
            }
        }
    }
}

impl UnobservedSession {
    /// The remediation class for this reason — derived ONCE, here, so no call
    /// site re-decides what an operator should do about it.
    pub fn remediation(self) -> Remediation {
        match self {
            Self::SessionUnreadable | Self::OriginSessionUnreadable => {
                Remediation::RepairSessionFile
            }
            Self::SessionDirectoryUnavailable | Self::SessionPathUnresolvable => {
                Remediation::RepairSessionStore
            }
            Self::AuthorityKeyUnreadable => Remediation::RepairAuthKey,
            Self::ClaimStoreUnreadable | Self::ClaimFileUnreadable => Remediation::RepairClaimStore,
            Self::TmuxHandleUnobserved => Remediation::RepairTmux,
        }
    }

    /// Stable machine-readable tag, so the API body is not a `Debug` string.
    pub fn tag(self) -> &'static str {
        match self {
            Self::SessionDirectoryUnavailable => "session_directory_unavailable",
            Self::SessionUnreadable => "session_unreadable",
            Self::OriginSessionUnreadable => "origin_session_unreadable",
            Self::AuthorityKeyUnreadable => "authority_key_unreadable",
            Self::SessionPathUnresolvable => "session_path_unresolvable",
            Self::ClaimStoreUnreadable => "claim_store_unreadable",
            Self::ClaimFileUnreadable => "claim_file_unreadable",
            Self::TmuxHandleUnobserved => "tmux_handle_unobserved",
        }
    }
}

/// One failed observation, WITH the identity of what it failed on.
///
/// orgasmic:TASK-2QK4P.1.1.1.1 F3 — [`UnobservedSession`] is a bare tag, so
/// `enumerate_recovery_origin_links` knew exactly which file it stopped on and
/// then threw that away at the first hop. Every recovery in the project then
/// answered 503 with a reason and no subject, which an operator can neither
/// diagnose nor clear.
///
/// # The design question, answered
///
/// The reviewer asked whether a permanently malformed session should be
/// ISOLATED as a per-file authority fault instead of refusing project-wide.
/// **It should not, and the refusal stays project-wide.** The enumeration's
/// only job is to prove that a claim's replacement link is UNIQUE across the
/// project; dropping one unreadable file from the set and calling the remainder
/// complete is bit-for-bit the defect rounds one through three closed — a
/// failed observation reported as a successful one that found nothing. A second
/// authenticated replacement could be in exactly the file that could not be
/// read, which is the arrangement that ends with two daemons holding one lease.
///
/// What was actually wrong is that the refusal was ANONYMOUS and had no exit.
/// So the subject and the [`Remediation`] travel with the reason from the leaf
/// that failed, through the inventory and the 503, into the operator UI — and
/// the repair is a documented single-file action ([`Remediation::hint`]) that
/// clears the refusal for the whole project on the next request, which the
/// `f3_*` regressions exercise end to end.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UnobservedEvidence {
    pub reason: UnobservedSession,
    /// Sanitized, project-relative identity of the file the observation failed
    /// on — never an absolute host path, and never raw operator-supplied bytes.
    pub subject: Option<String>,
    pub remediation: Remediation,
}

impl UnobservedEvidence {
    pub fn new(reason: UnobservedSession) -> Self {
        Self {
            reason,
            subject: None,
            remediation: reason.remediation(),
        }
    }

    pub fn about(reason: UnobservedSession, subject: impl Into<String>) -> Self {
        Self::new(reason).with_subject(Some(subject.into()))
    }

    pub fn with_subject(mut self, subject: Option<String>) -> Self {
        if self.subject.is_none() {
            self.subject = subject;
        }
        self
    }

    /// Fill in a subject only if one was not established deeper in the stack.
    pub fn or_subject(self, subject: impl Into<String>) -> Self {
        self.with_subject(Some(subject.into()))
    }
}

/// Render a path as a sanitized, project-relative identifier fit for a log
/// line, an API body and an operator UI.
///
/// orgasmic:TASK-2QK4P.1.1.1.1 F3 — session file names come from run ids the
/// daemon minted, but this is the boundary where a path becomes operator-facing
/// text, so it is sanitized here rather than trusted: every byte outside
/// `[A-Za-z0-9._-]` becomes `_`, the host prefix above the project root is
/// dropped, and the result is length-capped. A name that sanitizes to nothing
/// still yields a stable placeholder, because "which file" must never be blank.
pub fn sanitized_subject(project_root: &Path, path: &Path) -> String {
    const MAX: usize = 120;
    let relative = path.strip_prefix(project_root).unwrap_or_else(|_| {
        Path::new(
            path.file_name()
                .unwrap_or_else(|| std::ffi::OsStr::new("unnamed")),
        )
    });
    let mut out = String::new();
    for component in relative.components() {
        let Component::Normal(part) = component else {
            continue;
        };
        if !out.is_empty() {
            out.push('/');
        }
        for byte in part.as_encoded_bytes() {
            out.push(match byte {
                b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'.' | b'_' | b'-' => *byte as char,
                _ => '_',
            });
        }
    }
    if out.is_empty() {
        out.push_str("unnamed");
    }
    if out.len() > MAX {
        out.truncate(MAX);
    }
    out
}

/// What one predicate on the recovery-authority path actually established.
///
/// orgasmic:TASK-2QK4P.1.1.1 acceptance 1 — THE GENERATING SHAPE OF FOUR REVIEW
/// ROUNDS WAS A `bool` THAT MEANT BOTH "I VERIFIED AND IT IS FALSE" AND "I
/// COULD NOT CHECK". Rounds one through three each closed one instance:
///
/// ```text
/// round 1  an EMPTY catalog index            read as proof of non-contradiction
/// round 2  a ONE-ELEMENT catalog index       read as proof of uniqueness
/// round 3  a FAILED per-file scan            read as an empty-but-complete file
/// round 4  a FAILED AUTHORITY VERIFICATION   read as "not authentic" / "invalid"
/// ```
///
/// Round three made the collapse unrepresentable in [`IndexedRecoveryOrigins`]
/// and [`AuthoritativeOriginLinks`] — and then handed those types a `bool`
/// computed by [`claim_has_valid_authority`], which had the same defect. So
/// this type is not another point fix: it is the return type every fallible
/// predicate on the path now shares, and the two collapses it closes fail in
/// OPPOSITE directions, which is why one type could not be enough at one site.
///
/// - [`Self::Invalid`] is a VERIFIED negative. A caller may destroy evidence on
///   it: quarantine the claim, drop the link, mint a replacement.
/// - [`Self::Unobserved`] is a statement about the OBSERVER. A caller must fail
///   closed and retry — never act, and never quarantine
///   (orgasmic:TASK-2QK4P.1.1 ruling 1, still binding).
///
/// The one question to ask of every predicate that returns this: *what does a
/// caller conclude from the negative, and is that conclusion still right when
/// the reason was "I could not look"?*
#[derive(Debug, Clone, PartialEq, Eq)]
#[must_use = "a fallible predicate states its own observability; dropping it \
              turns `I could not check` back into `I checked and it is false`"]
pub enum ClaimEvidence {
    /// Observed, and the property holds.
    Valid,
    /// Observed, and the property does not hold. The evidence itself is bad.
    Invalid,
    /// NOT observed. Deliberately carries no boolean to mistake for an answer —
    /// but it DOES carry which file and what would repair it
    /// (orgasmic:TASK-2QK4P.1.1.1.1 F3).
    Unobserved(UnobservedEvidence),
}

impl ClaimEvidence {
    /// `Valid` when `held`, `Invalid` otherwise — for the parts of a predicate
    /// that are pure comparisons over already-observed bytes.
    fn verified(held: bool) -> Self {
        if held {
            Self::Valid
        } else {
            Self::Invalid
        }
    }
}

/// Classify a session-read failure as bad evidence or as a failed observation.
///
/// orgasmic:TASK-2QK4P.1.1.1 — the split is what keeps `Unobserved` honest in
/// BOTH directions. `NotFound` is a verified fact: the file a claim names is
/// not there, and refusing to act on that would freeze every genuinely dead
/// rescue. Every other `io` failure — `EACCES`, `EIO`, `EMFILE`, a mid-read
/// interruption — is the observer failing, and so is a parse failure, because
/// an unreadable file is not an empty one. `InvalidIdentifier` and
/// `CorruptClaim` from the path-membership check are statements about the
/// PATH the claim carries, which is content the claim itself supplied.
fn session_read_evidence(err: &RecoveryClaimError, reason: UnobservedSession) -> ClaimEvidence {
    match err {
        RecoveryClaimError::Io(io) if io.kind() == std::io::ErrorKind::NotFound => {
            ClaimEvidence::Invalid
        }
        // orgasmic:TASK-2QK4P.1.1.1.1 acceptance 2 — the errno policy lives in
        // exactly one place now, so this site applies it instead of restating
        // "every non-NotFound io error is unobserved" and drifting from it.
        RecoveryClaimError::Io(io) => match classify_observation(io) {
            ObservationClass::Absent => ClaimEvidence::Invalid,
            ObservationClass::Decided => ClaimEvidence::Invalid,
            ObservationClass::Unobserved => {
                ClaimEvidence::Unobserved(UnobservedEvidence::new(reason))
            }
        },
        // The strict whole-file envelope parse rejects a file it cannot read as
        // envelopes, and a swapped device/inode means the bytes just read
        // belong to a different file. Neither states that the file lacks the
        // link, so both are the observer failing.
        RecoveryClaimError::CorruptClaim => {
            ClaimEvidence::Unobserved(UnobservedEvidence::new(reason))
        }
        RecoveryClaimError::Unobserved(evidence) => ClaimEvidence::Unobserved(evidence.clone()),
        // The path the claim carries does not name a regular file directly
        // inside this project's pinned sessions directory. That is decided, and
        // it is decided about content the claim supplied.
        RecoveryClaimError::ForeignSessionPath | RecoveryClaimError::InvalidIdentifier => {
            ClaimEvidence::Invalid
        }
        _ => ClaimEvidence::Invalid,
    }
}

/// Attach a subject to an unobserved answer that does not already carry one.
///
/// orgasmic:TASK-2QK4P.1.1.1.1 F3 — the leaf that failed knows the errno; the
/// caller knows the file. Neither alone is a diagnostic.
fn evidence_about(evidence: ClaimEvidence, subject: impl Into<String>) -> ClaimEvidence {
    match evidence {
        ClaimEvidence::Unobserved(unobserved) => {
            ClaimEvidence::Unobserved(unobserved.or_subject(subject))
        }
        other => other,
    }
}

/// Result of one origin-index pass over a session file.
///
/// orgasmic:TASK-2QK4P.1.1 — THE POINT OF THIS TYPE IS THE VARIANT THAT
/// CARRIES NO LINKS.
///
/// Rounds one through three of TASK-2QK4P were one defect: an observation that
/// FAILED was reported as an observation that SUCCEEDED and found nothing. Each
/// round closed the instance it was handed and left the class alive one layer
/// down, and round three's layer was this function's own return type — a struct
/// with a `links` field and a `Default`, so `IndexedRecoveryOrigins::default()`
/// was a legal spelling of "I could not read the file" that every caller read
/// as "the file has no links".
///
/// So this is an enum and not a struct with an `ok` flag, it derives no
/// `Default`, and [`Self::Unobserved`] has NO links field. What that forces:
///
/// - The three error paths below cannot return an empty success, because there
///   is no empty success to return — `Complete` must be constructed with a
///   `links` vector the function actually produced.
/// - A caller cannot reach `links` without matching, and cannot match without
///   writing an arm for `Unobserved`. Discarding the unresolved case is still
///   possible, but only by typing the word, which is what makes it reviewable.
/// - `#[must_use]` means dropping the result entirely is a warning, and the
///   workspace builds with `-D warnings`.
///
/// `the_origin_index_result_cannot_spell_failure_as_an_empty_success` pins the
/// shape so a future edit that re-adds `Default` is a red test rather than a
/// fourth review round.
#[derive(Debug)]
#[must_use = "an origin-index pass states its own completeness; dropping it \
              turns `I did not observe` back into `I observed nothing`"]
pub enum IndexedRecoveryOrigins {
    /// The whole file was observed. `links` is every recovery-origin link it
    /// carries — an empty vector here is a real statement of absence.
    Complete {
        links: Vec<IndexedRecoveryOrigin>,
        /// Bytes read, reported as an inventory stage metric.
        bytes_inspected: u64,
    },
    /// The file was NOT observed. There is deliberately nothing here to mistake
    /// for a result.
    Unobserved {
        reason: UnobservedEvidence,
        bytes_inspected: u64,
    },
}

/// A lifecycle scan that is COMPLETE for the whole file.
///
/// Bounded first, because that is what keeps a whole-board pass cheap; if the
/// bounded windows skipped a middle, the middle is read
/// ([`orgasmic_core::scan_session_lifecycle_complete_reader`], streaming).
///
/// orgasmic:TASK-2QK4P.1.1 F2 — [`SessionScanBudget::DEFAULT`] is a 128 KiB
/// prefix and a 64 KiB tail, and `SessionLifecycleScan::truncated` documents
/// that the gap between them is UNKNOWN rather than absent. Origin indexing
/// decides recovery authority from "no other link exists in this file", which
/// is exactly a statement about the gap, so it may never read a truncated scan
/// as an answer. Nothing bounds a `RecoveryOrigin` envelope to either window:
/// the committed snapshot it embeds repeats the run's `PromptDraft`, and that
/// draft carries uncapped `git diff --stat` output.
///
/// The bounded scan is also escalated past when it ERRORS, not only when it
/// truncates. It rejects a whole file on any malformed lifecycle-relevant line,
/// including a torn final one, and the complete scan can tell that shape apart
/// from a torn middle — see
/// [`orgasmic_core::scan_session_lifecycle_complete_reader`]. Deciding recovery
/// from the stricter of the two would freeze a project's recovery on one junk
/// `.jsonl`.
#[cfg(unix)]
fn complete_session_scan(
    session_dir: &SessionDirectory,
    session_path: &Path,
) -> Result<SessionLifecycleScan, RecoveryClaimError> {
    match session_dir.scan_path(session_path, SessionScanBudget::DEFAULT) {
        Ok(scan) if !scan.truncated => Ok(scan),
        _ => session_dir.scan_path_complete(session_path),
    }
}

#[cfg(not(unix))]
fn complete_session_scan(session_path: &Path) -> Result<SessionLifecycleScan, RecoveryClaimError> {
    match orgasmic_core::scan_session_lifecycle(session_path, SessionScanBudget::DEFAULT) {
        Ok(scan) if !scan.truncated => Ok(scan),
        _ => orgasmic_core::scan_session_lifecycle_complete(session_path)
            .map_err(|_| RecoveryClaimError::CorruptClaim),
    }
}

pub fn index_recovery_origins_in_session(
    home: &Home,
    project_root: &Path,
    session_path: &Path,
    containing_project_id: &str,
) -> IndexedRecoveryOrigins {
    // orgasmic:TASK-2QK4P.1.1.1.1 F3 — WHICH FILE. This function knows exactly
    // which session file it stopped on and used to discard that at the first
    // hop, which is how a project-wide permanent 503 ended up naming nothing.
    let subject = sanitized_subject(project_root, session_path);
    #[cfg(unix)]
    let session_dir = match SessionDirectory::open(project_root) {
        Ok(dir) => dir,
        Err(_) => {
            return IndexedRecoveryOrigins::Unobserved {
                reason: UnobservedEvidence::about(
                    UnobservedSession::SessionDirectoryUnavailable,
                    subject,
                ),
                bytes_inspected: 0,
            }
        }
    };
    #[cfg(unix)]
    let scan = complete_session_scan(&session_dir, session_path);
    #[cfg(not(unix))]
    let scan = complete_session_scan(session_path);
    let scan = match scan {
        Ok(scan) => scan,
        Err(_) => {
            return IndexedRecoveryOrigins::Unobserved {
                reason: UnobservedEvidence::about(UnobservedSession::SessionUnreadable, subject),
                bytes_inspected: 0,
            }
        }
    };
    let mut bytes_inspected = scan.bytes_inspected;
    let complete = |links: Vec<IndexedRecoveryOrigin>, bytes_inspected: u64| {
        IndexedRecoveryOrigins::Complete {
            links,
            bytes_inspected,
        }
    };
    let envelopes = scan.envelopes;
    let Some(first) = envelopes.first() else {
        return complete(Vec::new(), bytes_inspected);
    };
    let Some(run_meta_project) = session_run_meta_project(&envelopes) else {
        return complete(Vec::new(), bytes_inspected);
    };
    if run_meta_project != containing_project_id {
        return complete(Vec::new(), bytes_inspected);
    }
    let draft_prompt = session_prompt_draft(&envelopes);
    let mut links = Vec::new();
    for envelope in &envelopes {
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
            // orgasmic:TASK-2QK4P.1.1.1 F1(a) — a `continue` on an UNOBSERVED
            // authority check drops a link that may be a second authority, out
            // of a set this function then labels `Complete`. That is exactly
            // the state rounds one through three exist to prevent, reached
            // through the authenticator instead of through the index.
            match claim_has_valid_authority(home, &claim_snapshot) {
                ClaimEvidence::Valid => {}
                ClaimEvidence::Invalid => continue,
                ClaimEvidence::Unobserved(evidence) => {
                    return IndexedRecoveryOrigins::Unobserved {
                        reason: evidence.or_subject(subject),
                        bytes_inspected,
                    }
                }
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
            // orgasmic:TASK-2QK4P.1.1.1 acceptance 1 — `Result::ok()` on BOTH
            // sides was the same collapse a third time, and it failed in both
            // directions at once: one side erroring dropped a possibly-second
            // authority out of a `Complete` set, and BOTH sides erroring
            // compared `None == None` and ADMITTED a link whose file identity
            // had not been established. Neither side may be answered by a
            // failure.
            #[cfg(unix)]
            {
                let indexed_name = match session_dir.name_for_path(session_path) {
                    Ok(name) => name,
                    Err(_) => {
                        return IndexedRecoveryOrigins::Unobserved {
                            reason: UnobservedEvidence::about(
                                UnobservedSession::SessionPathUnresolvable,
                                subject,
                            ),
                            bytes_inspected,
                        }
                    }
                };
                match session_dir.name_for_path(&replacement_session_path) {
                    Ok(name) if name == indexed_name => {}
                    // Decided: the link names a path that is not this file.
                    Ok(_)
                    | Err(RecoveryClaimError::ForeignSessionPath)
                    | Err(RecoveryClaimError::InvalidIdentifier) => continue,
                    Err(_) => {
                        return IndexedRecoveryOrigins::Unobserved {
                            reason: UnobservedEvidence::about(
                                UnobservedSession::SessionPathUnresolvable,
                                sanitized_subject(project_root, &replacement_session_path),
                            ),
                            bytes_inspected,
                        }
                    }
                }
            }
            #[cfg(not(unix))]
            if replacement_session_path != session_path {
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
            if !claim_immutable_plan_matches_session(&claim_snapshot, &envelopes) {
                continue;
            }
            if !origin_session_path.is_absolute() {
                continue;
            }
            // orgasmic:TASK-2QK4P.1.1 F1 — a `continue` here would DROP a link
            // that may be a second authority, which is the unsafe direction:
            // the resolver's uniqueness test only fails closed on a set it can
            // trust to be whole. An origin session that cannot be read leaves
            // this link undecidable, and undecidable is unobserved.
            #[cfg(unix)]
            let origin_scan = complete_session_scan(&session_dir, &origin_session_path);
            #[cfg(not(unix))]
            let origin_scan = complete_session_scan(&origin_session_path);
            let origin_scan = match origin_scan {
                Ok(scan) => scan,
                Err(_) => {
                    return IndexedRecoveryOrigins::Unobserved {
                        reason: UnobservedEvidence::about(
                            UnobservedSession::OriginSessionUnreadable,
                            sanitized_subject(project_root, &origin_session_path),
                        ),
                        bytes_inspected,
                    }
                }
            };
            bytes_inspected += origin_scan.bytes_inspected;
            let origin_envelopes = origin_scan.envelopes;
            if origin_envelopes
                .first()
                .is_none_or(|origin| origin.run_id != origin_run_id)
                || session_run_meta_project(&origin_envelopes).as_deref()
                    != Some(containing_project_id)
            {
                continue;
            }
            links.push(IndexedRecoveryOrigin {
                project_root: project_root.to_path_buf(),
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
    complete(links, bytes_inspected)
}

pub fn reconstruct_claim_from_origin(link: &IndexedRecoveryOrigin) -> RecoveryClaim {
    link.claim.clone()
}

fn matching_origin_links<'a>(
    links: &'a [IndexedRecoveryOrigin],
    project_root: &Path,
    project_id: &str,
    origin_run_id: &str,
) -> Vec<&'a IndexedRecoveryOrigin> {
    links
        .iter()
        .filter(|link| {
            link.project_id == project_id
                && link.origin_run_id == origin_run_id
                && link.project_root == project_root
        })
        .collect()
}

/// Cost of building one project's authoritative link set, reported so an
/// operator can read the slow path's price off an inventory response.
#[derive(Debug, Clone, Copy, Default)]
pub struct OriginEnumerationCost {
    pub files: u64,
    pub bytes_inspected: u64,
}

/// Every recovery-origin link that exists on disk under one project, or the
/// statement that the enumeration did NOT complete.
///
/// orgasmic:TASK-2QK4P.1 — this is the filesystem answer, and it exists because
/// the run catalog cannot state its own completeness. The catalog's candidate
/// files are the records it happens to hold, and the session writer invalidates
/// a record on every lifecycle append, so an empty catalog-derived set is
/// produced both by "the one live record was temporarily invalidated" and by
/// "the candidate set never contained the file". Nothing in that set
/// distinguishes them.
///
/// orgasmic:TASK-2QK4P.1.1 — and [`Self::Unobserved`] is the third answer,
/// which is not "nothing found". A sessions directory that cannot be opened, a
/// directory entry that cannot be read, an unreadable member file or one
/// malformed lifecycle line all leave the enumeration incomplete, and an
/// incomplete enumeration is not permission to decide recovery authority. Like
/// [`IndexedRecoveryOrigins`] it derives no `Default` and its unresolved
/// variant carries no links, so no call site can spell failure as an empty set.
#[derive(Debug)]
#[must_use = "an origin enumeration states its own completeness; dropping it \
              turns `I did not observe` back into `I observed nothing`"]
pub enum AuthoritativeOriginLinks {
    Complete(Vec<IndexedRecoveryOrigin>),
    Unobserved(UnobservedEvidence),
}

/// One complete per-project authoritative snapshot, built at most once per
/// inventory or recover decision and shared across every claim that decision
/// resolves.
///
/// orgasmic:TASK-2QK4P.1.1 F4 — the enumeration is a whole-directory scan, so
/// doing it inside each resolver call made a project with N committed claims
/// pay N passes over the same files on every poll. Memoizing by project root
/// keeps the safety property (the answer is the filesystem's, never a cache
/// consulted as authority) while paying for it once: this map lives for the
/// duration of ONE decision and is dropped with it, so it can never become the
/// stale index the whole task exists to stop trusting.
#[derive(Default)]
pub struct ProjectOriginAuthority {
    by_project: BTreeMap<PathBuf, AuthoritativeOriginLinks>,
    cost: OriginEnumerationCost,
}

impl ProjectOriginAuthority {
    pub fn cost(&self) -> OriginEnumerationCost {
        self.cost
    }

    /// Enumerate this project once, then answer from that one enumeration.
    pub fn links_for(
        &mut self,
        home: &Home,
        project_root: &Path,
        project_id: &str,
    ) -> &AuthoritativeOriginLinks {
        if !self.by_project.contains_key(project_root) {
            let (links, cost) = enumerate_recovery_origin_links(home, project_root, project_id);
            self.cost.files += cost.files;
            self.cost.bytes_inspected += cost.bytes_inspected;
            self.by_project.insert(project_root.to_path_buf(), links);
        }
        &self.by_project[project_root]
    }
}

fn enumerate_recovery_origin_links(
    home: &Home,
    project_root: &Path,
    project_id: &str,
) -> (AuthoritativeOriginLinks, OriginEnumerationCost) {
    let dir = project_sessions_dir(project_root);
    #[cfg(test)]
    record_origin_enumeration_pass(project_root);
    let mut links = Vec::new();
    let mut cost = OriginEnumerationCost::default();
    let entries = match std::fs::read_dir(&dir) {
        Ok(entries) => entries,
        // orgasmic:TASK-2QK4P.1.1.1.1 acceptance 2 — THE SAME DIVERGENCE, ONE
        // CALL EARLIER, found by applying the policy rather than by reading the
        // finding list. This site mapped EVERY `read_dir` failure to
        // `Unobserved`, `ENOENT` included — and an absent sessions directory is
        // a decided fact: there are no session files, so there are no
        // `RecoveryOrigin` links, and the enumeration IS complete and empty.
        // Reporting absence as a failed observation refuses recovery for a
        // project that has simply never written a session.
        Err(err) => match classify_observation(&err) {
            ObservationClass::Absent => return (AuthoritativeOriginLinks::Complete(links), cost),
            ObservationClass::Decided | ObservationClass::Unobserved => {
                return (
                    AuthoritativeOriginLinks::Unobserved(UnobservedEvidence::about(
                        UnobservedSession::SessionDirectoryUnavailable,
                        sanitized_subject(project_root, &dir),
                    )),
                    cost,
                )
            }
        },
    };
    for entry in entries {
        let path = match entry {
            Ok(entry) => entry.path(),
            Err(_) => {
                return (
                    AuthoritativeOriginLinks::Unobserved(UnobservedEvidence::about(
                        UnobservedSession::SessionDirectoryUnavailable,
                        sanitized_subject(project_root, &dir),
                    )),
                    cost,
                )
            }
        };
        if path.extension().and_then(|ext| ext.to_str()) != Some("jsonl") {
            continue;
        }
        cost.files += 1;
        // orgasmic:TASK-2QK4P.1.1 F1 — the `Unobserved` arm is the finding. A
        // member file that could not be indexed used to contribute an empty
        // `links` vector, and the union was then labelled authoritative: one
        // unreadable JSONL, or one malformed lifecycle line in it, was enough
        // to hide a second daemon-authenticated replacement and let the
        // resolver return `Valid` for a claim it had not proved unique.
        match index_recovery_origins_in_session(home, project_root, &path, project_id) {
            IndexedRecoveryOrigins::Complete {
                links: found,
                bytes_inspected,
            } => {
                cost.bytes_inspected += bytes_inspected;
                links.extend(found);
            }
            IndexedRecoveryOrigins::Unobserved {
                reason,
                bytes_inspected,
            } => {
                cost.bytes_inspected += bytes_inspected;
                return (AuthoritativeOriginLinks::Unobserved(reason), cost);
            }
        }
    }
    (AuthoritativeOriginLinks::Complete(links), cost)
}

pub fn resolve_authoritative_recovery_claim(
    home: &Home,
    project_root: &Path,
    project_id: &str,
    origin_run_id: &str,
    authority: &mut ProjectOriginAuthority,
) -> Result<ResolvedRecoveryClaim, RecoveryClaimError> {
    // orgasmic:TASK-2QK4P.1.1 F3/F4 — ONE authoritative set, taken before the
    // branch, so every branch below decides from the same evidence. The
    // committed branch used to enumerate the filesystem while the missing and
    // corrupt branches took a catalog-derived slice, which left the same hole
    // open with a different caller; and enumerating inside the branch made a
    // project with several committed claims rescan its session files once per
    // claim on every poll. `authority` is memoized per project for the life of
    // one inventory or recover decision and dropped with it.
    let authoritative = match authority.links_for(home, project_root, project_id) {
        AuthoritativeOriginLinks::Complete(links) => links.as_slice(),
        // Unobserved is NOT invalid. It suppresses recovery authority — the
        // caller must not act on a claim whose uniqueness is unproven — but it
        // must not quarantine, because a transient read failure that renamed a
        // valid committed claim would turn one failed observation into the
        // permanent loss of that rescue's idempotency, and the handler would
        // then mint a second replacement beside the live one.
        AuthoritativeOriginLinks::Unobserved(evidence) => {
            return Ok(ResolvedRecoveryClaim::Unobserved(evidence.clone()))
        }
    };
    let loaded = load_recovery_claim(home, project_id, origin_run_id);
    match loaded {
        Ok(Some(claim)) => {
            if claim.status == RecoveryClaimStatus::Committed {
                // orgasmic:TASK-2QK4P.1 — recovery authority is decided from a
                // candidate set that can state its own COMPLETENESS, never from
                // a silent one.
                //
                // The obvious cheap candidate set is the run catalog, whose
                // records are the ones that happen to be loaded. The catalog is
                // a cache with holes ON PURPOSE:
                // the session writer calls
                // [`crate::run_catalog::RunCatalog::invalidate_session`] on
                // every lifecycle append, which REMOVES the record so the next
                // refresh rebuilds it from the newer bytes. A replacement run
                // that is live — exactly the state a crash-recovery replay finds
                // it in, because the replay's whole job is to hand back the
                // replacement the dead daemon already spawned — is appending, so
                // its record is missing for the window between the append and
                // the next refresh, and a replay landing in that window indexes
                // no link at all. TASK-2QK4P was that window: reading the
                // silence as disproof quarantined a committed claim this
                // function had just verified against its own replacement
                // session, and `/api/runs/<id>/recover` answered 409, blocked by
                // the very replacement the caller was asking for.
                //
                // But TREATING the silence as proof of non-contradiction is the
                // opposite error, and it is the worse one. An empty slice makes
                // no statement about completeness: the same emptiness is
                // produced by "one live record was temporarily invalidated" and
                // by "the candidate set never contained the file", and the
                // second admits the state
                // `duplicate_authenticated_replacements_fail_closed` rules is a
                // safety violation — two daemon-authenticated replacements for
                // one origin, of which this function would silently pick the one
                // it happened to load. Unknown is not permission: the cost of a
                // false quarantine is a retry, the cost of a false `Valid` is
                // two daemons believing they hold the same lease.
                //
                // So a catalog-derived index is not consulted here AT ALL, and
                // that is deliberate rather than an oversight. A ONE-element
                // fast index states no more about completeness than an empty
                // one: the catalog can just as easily hold the record for the
                // replacement this claim names while a SECOND authenticated
                // replacement's record sits invalidated, and that is the
                // likelier arrangement of the two, not the rarer. Enumerating
                // only on a zero match would close the hole one link wide and
                // leave it open at one.
                //
                // The candidate set is therefore always the filesystem's, and it
                // has three answers, not two: exactly one link equal to this
                // claim (accept), more than one or one that disagrees (the
                // duplicate-replacement violation), and `Unobserved` — the
                // enumeration itself did not complete, handled above, which
                // suppresses authority exactly like multiplicity does but
                // without renaming the claim.
                //
                // The cost is one lifecycle scan per session file under this ONE
                // project, once per inventory or recover decision rather than
                // once per claim, and unbounded only for files whose bounded
                // windows skipped a middle. That is paid knowingly: the
                // alternative is deciding a lease-holder from a cache that
                // cannot say what it has not looked at.
                let matching =
                    matching_origin_links(authoritative, project_root, project_id, origin_run_id);
                let uniquely_confirmed =
                    matches!(matching.as_slice(), [only] if only.claim == claim);
                if uniquely_confirmed {
                    // orgasmic:TASK-2QK4P.1.1.1 F1(b) — the enumeration above
                    // completed, and this read can still fail. `false` used to
                    // fall through to the quarantine below, which destroys a
                    // live rescue's idempotency on an observation failure and
                    // lets `POST /recover` mint a competitor.
                    match verify_committed_claim_against_session(home, project_root, &claim) {
                        ClaimEvidence::Valid => return Ok(ResolvedRecoveryClaim::Valid(claim)),
                        ClaimEvidence::Unobserved(reason) => {
                            return Ok(ResolvedRecoveryClaim::Unobserved(reason))
                        }
                        ClaimEvidence::Invalid => {}
                    }
                }
                quarantine_invalid_claim(home, project_id, origin_run_id)?;
                return reconstruct_or_quarantine(
                    home,
                    project_root,
                    project_id,
                    origin_run_id,
                    authoritative,
                );
            }
            Ok(ResolvedRecoveryClaim::Valid(claim))
        }
        Ok(None) => {
            reconstruct_or_quarantine(home, project_root, project_id, origin_run_id, authoritative)
        }
        Err(RecoveryClaimError::CorruptClaim) => {
            quarantine_invalid_claim(home, project_id, origin_run_id)?;
            reconstruct_or_quarantine(home, project_root, project_id, origin_run_id, authoritative)
        }
        // orgasmic:TASK-2QK4P.1.1.1 acceptance 2 — the load could not decide
        // whether the claim on disk is authentic. That is not `CorruptClaim`
        // and must not reach the quarantine arm above.
        Err(RecoveryClaimError::Unobserved(reason)) => {
            Ok(ResolvedRecoveryClaim::Unobserved(reason))
        }
        Err(err) => Err(err),
    }
}

/// Rebuild a lost or corrupt claim from session truth, or refuse.
///
/// orgasmic:TASK-2QK4P.1.1 F3 — `authoritative` must come from
/// [`ProjectOriginAuthority`] and nowhere else. `Missing` says "no
/// daemon-authenticated replacement exists for this origin", which the caller
/// reads as permission to mint one; that is only true of a set that has stated
/// its own completeness. Handing this a catalog-derived slice made `Missing`
/// reachable while a real replacement was live and appending — the same defect
/// as the committed branch, one caller over.
fn reconstruct_or_quarantine(
    home: &Home,
    project_root: &Path,
    project_id: &str,
    origin_run_id: &str,
    authoritative: &[IndexedRecoveryOrigin],
) -> Result<ResolvedRecoveryClaim, RecoveryClaimError> {
    let matching = matching_origin_links(authoritative, project_root, project_id, origin_run_id);
    if matching.len() > 1 {
        return Ok(ResolvedRecoveryClaim::InvalidQuarantined);
    }
    if let [link] = matching.as_slice() {
        let reconstructed = reconstruct_claim_from_origin(link);
        // orgasmic:TASK-2QK4P.1.1.1 F1(b) — both checks are fallible, and
        // `InvalidQuarantined` on an unobserved one is the ruling from round
        // three violated one call deeper: it suppresses the reconstruction AND
        // reports a decided negative the caller may destroy evidence on.
        for evidence in [
            claim_has_valid_authority(home, &reconstructed),
            verify_committed_claim_against_session(home, project_root, &reconstructed),
        ] {
            match evidence {
                ClaimEvidence::Valid => {}
                ClaimEvidence::Invalid => return Ok(ResolvedRecoveryClaim::InvalidQuarantined),
                ClaimEvidence::Unobserved(reason) => {
                    return Ok(ResolvedRecoveryClaim::Unobserved(reason))
                }
            }
        }
        write_claim_atomic_or_reconcile(home, &reconstructed)?;
        return Ok(ResolvedRecoveryClaim::Reconstructed(reconstructed));
    }
    match load_recovery_claim(home, project_id, origin_run_id) {
        Ok(Some(_)) => Ok(ResolvedRecoveryClaim::InvalidQuarantined),
        Ok(None) => Ok(ResolvedRecoveryClaim::Missing),
        // `Missing` is permission to MINT. It may not be reached by a load that
        // could not say whether a claim is there.
        Err(RecoveryClaimError::Unobserved(reason)) => {
            Ok(ResolvedRecoveryClaim::Unobserved(reason))
        }
        Err(RecoveryClaimError::CorruptClaim) => Ok(ResolvedRecoveryClaim::InvalidQuarantined),
        Err(err) => Err(err),
    }
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
    let driver_config = spec.driver_config.clone();
    let planned_native_runtime =
        if spec.action == "start_recovery_run" && spec.harness.as_deref() == Some("claude") {
            let command = driver_config
                .get("command")
                .and_then(serde_json::Value::as_str)
                .unwrap_or("claude")
                .to_string();
            let mut args = driver_config
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
            let session_path = driver_config
                .get("cwd")
                .and_then(serde_json::Value::as_str)
                .and_then(|cwd| {
                    let encoded: String = cwd
                        .chars()
                        .map(|ch| if ch == '/' || ch == '.' { '-' } else { ch })
                        .collect();
                    driver_config
                        .get("provider_home")
                        .and_then(serde_json::Value::as_str)
                        .map(PathBuf::from)
                        .map(|home| {
                            home.join(".claude/projects")
                                .join(encoded)
                                .join(format!("{replacement_runtime_id}.jsonl"))
                        })
                });
            Some(NativeRuntimeMeta {
                provider: "claude".into(),
                session_id: Some(replacement_runtime_id.clone()),
                session_path,
                launch_argv,
                credential_mode: None,
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
    let mut claim = RecoveryClaim {
        plan_version: Some(1),
        authority_tag: None,
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
        driver_config: Some(driver_config),
        force_inert: Some(spec.force_inert),
        worktree: spec.worktree.clone(),
        last_path: spec.last_path.clone(),
        stdout_path: spec.stdout_path.clone(),
        planned_native_runtime,
        run_options: Some(spec.run_options.clone()),
        spawn_started: false,
    };
    claim.authority_tag = Some(authority_tag(home, &claim)?);
    write_claim_atomic_or_reconcile(home, &claim)?;
    recovery_failpoint("pending");
    Ok(PendingRecoveryPlan {
        claim,
        planned_identity,
        reattach_existing: false,
        session_file: None,
    })
}

pub fn mark_pending_recovery_spawn_started(
    home: &Home,
    project_id: &str,
    origin_run_id: &str,
) -> Result<RecoveryClaim, RecoveryClaimError> {
    let mut claim = load_recovery_claim(home, project_id, origin_run_id)?
        .ok_or(RecoveryClaimError::MissingClaim)?;
    if claim.status != RecoveryClaimStatus::Pending {
        return Ok(claim);
    }
    if !claim.spawn_started {
        claim.spawn_started = true;
        claim.authority_tag = None;
        claim.authority_tag = Some(authority_tag(home, &claim)?);
        write_claim_atomic_or_reconcile(home, &claim)?;
    }
    Ok(claim)
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
                ..
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
                credential_mode: None,
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
        if claim.action.as_deref() != Some("start_recovery_run") {
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

/// Does this committed claim still match the replacement session it names?
///
/// orgasmic:TASK-2QK4P.1.1.1 F1(b) — THE OTHER ROUND-FOUR DEFECT, and it fails
/// in the OPPOSITE direction to [`claim_has_valid_authority`]'s use in the
/// indexer, which is why one type change at one site could not catch both. This
/// returned `bool`, and it mapped the replacement-session open, the origin
/// session open, every read and every parse failure to `false`. The resolver
/// reads `false` as INVALID EVIDENCE: it quarantines the claim, can then report
/// `Missing`, and `POST /runs/:id/recover` reads `Missing` as permission to
/// plan a NEW replacement. One transient read failure therefore destroyed a
/// valid live rescue AND minted a competitor beside it.
///
/// `NotFound` stays [`ClaimEvidence::Invalid`]: a replacement session that is
/// genuinely gone is a decided fact, and refusing to act on it would freeze
/// every dead rescue behind a permanent 503.
pub fn verify_committed_claim_against_session(
    home: &Home,
    project_root: &Path,
    claim: &RecoveryClaim,
) -> ClaimEvidence {
    if claim.status != RecoveryClaimStatus::Committed || !recovery_claim_has_complete_plan(claim) {
        return ClaimEvidence::Invalid;
    }
    match claim_has_valid_authority(home, claim) {
        ClaimEvidence::Valid => {}
        other => return other,
    }
    #[cfg(unix)]
    let session_dir = match SessionDirectory::open(project_root) {
        Ok(dir) => dir,
        Err(err) => {
            return session_read_evidence(&err, UnobservedSession::SessionDirectoryUnavailable)
        }
    };
    #[cfg(unix)]
    let envelopes = match session_dir.read_path(&claim.replacement_session_path) {
        Ok(envelopes) => envelopes,
        Err(err) => return session_read_evidence(&err, UnobservedSession::SessionUnreadable),
    };
    #[cfg(not(unix))]
    let envelopes = match orgasmic_core::session::read_session_file(&claim.replacement_session_path)
    {
        Ok(envelopes) => envelopes,
        Err(err) => {
            return session_read_evidence(
                &RecoveryClaimError::Io(std::io::Error::other(err.to_string())),
                UnobservedSession::SessionUnreadable,
            )
        }
    };
    let Some(first) = envelopes.first() else {
        return ClaimEvidence::Invalid;
    };
    if first.run_id != claim.replacement_run_id {
        return ClaimEvidence::Invalid;
    }
    if first.runtime_id != claim.replacement_runtime_id {
        return ClaimEvidence::Invalid;
    }
    if claim
        .boot_id
        .as_deref()
        .is_some_and(|boot| first.boot_id != boot)
    {
        return ClaimEvidence::Invalid;
    }
    let Some(meta_project) = session_run_meta_project(&envelopes) else {
        return ClaimEvidence::Invalid;
    };
    if meta_project != claim.project_id {
        return ClaimEvidence::Invalid;
    }
    if !session_has_acquire(&envelopes) {
        return ClaimEvidence::Invalid;
    }
    let Some((replacement_run_id, replacement_session_path, action)) = recovery_origin_in_session(
        &envelopes,
        &claim.project_id,
        &claim.origin_run_id,
        &claim.request_id,
    ) else {
        return ClaimEvidence::Invalid;
    };
    if claim.replacement_run_id != replacement_run_id
        || claim.replacement_session_path != replacement_session_path
        || claim.action.as_deref() != Some(action.as_str())
    {
        return ClaimEvidence::Invalid;
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
        return ClaimEvidence::Invalid;
    };
    if claim.origin_session_path.as_ref() != Some(&origin_session_path) {
        return ClaimEvidence::Invalid;
    }
    if claim
        .target
        .as_deref()
        .is_some_and(|target| Some(target) != link_target.as_deref())
    {
        return ClaimEvidence::Invalid;
    }
    if !claim_immutable_plan_matches_session(claim, &envelopes) {
        return ClaimEvidence::Invalid;
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
        return ClaimEvidence::Invalid;
    }
    let Some(origin_path) = claim.origin_session_path.as_ref() else {
        return ClaimEvidence::Invalid;
    };
    // orgasmic:TASK-2QK4P.1.1.1 F1(b) — the ORIGIN read is the second collapse
    // in this function and the one the resolver reaches after a cached complete
    // enumeration, so it is the one a repro can hit without touching the
    // enumeration at all.
    #[cfg(unix)]
    let origin_envelopes = match session_dir.read_path(origin_path) {
        Ok(envelopes) => envelopes,
        Err(err) => return session_read_evidence(&err, UnobservedSession::OriginSessionUnreadable),
    };
    #[cfg(not(unix))]
    let origin_envelopes = match orgasmic_core::session::read_session_file(origin_path) {
        Ok(envelopes) => envelopes,
        Err(err) => {
            return session_read_evidence(
                &RecoveryClaimError::Io(std::io::Error::other(err.to_string())),
                UnobservedSession::OriginSessionUnreadable,
            )
        }
    };
    ClaimEvidence::verified(
        origin_envelopes
            .first()
            .is_some_and(|origin| origin.run_id == claim.origin_run_id)
            && session_run_meta_project(&origin_envelopes).as_deref()
                == Some(claim.project_id.as_str()),
    )
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
    #[derive(Clone, Copy)]
    enum ExpectedPhase {
        Acquire,
        RunMeta,
        NativeRuntime,
        PromptDraft,
        RecoveryOrigin,
    }
    let mut expected = vec![ExpectedPhase::Acquire, ExpectedPhase::RunMeta];
    if claim.planned_native_runtime.is_some() {
        expected.push(ExpectedPhase::NativeRuntime);
    }
    expected.push(ExpectedPhase::PromptDraft);
    expected.push(ExpectedPhase::RecoveryOrigin);
    let mut prefix_index = 0usize;
    for envelope in envelopes {
        if envelope.kind != SessionEventKind::Lifecycle {
            if prefix_index < expected.len() {
                return false;
            }
            continue;
        }
        if prefix_index >= expected.len() {
            return false;
        }
        let Ok(lifecycle) = serde_json::from_value::<Lifecycle>(envelope.event.clone()) else {
            return false;
        };
        let phase_matches = matches!(
            (expected[prefix_index], &lifecycle),
            (ExpectedPhase::Acquire, Lifecycle::Acquire { .. })
                | (ExpectedPhase::RunMeta, Lifecycle::RunMeta { .. })
                | (
                    ExpectedPhase::NativeRuntime,
                    Lifecycle::NativeRuntime { .. }
                )
                | (ExpectedPhase::PromptDraft, Lifecycle::PromptDraft { .. })
                | (
                    ExpectedPhase::RecoveryOrigin,
                    Lifecycle::RecoveryOrigin { .. }
                )
        );
        if !phase_matches {
            return false;
        }
        prefix_index += 1;
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
                ..
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
                    credential_mode: None,
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
                if claim.action.as_deref() != Some("start_recovery_run") {
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
            // orgasmic:TASK-KPMFK — only `post_stage` writes a stage identity,
            // and a planned recovery replacement is never a stage launch, so a
            // session carrying one is not this claim's session.
            | Lifecycle::StageMeta { .. }
            | Lifecycle::ManagerTerminalClaim { .. }
            | Lifecycle::ComposerSend { .. } => return false,
        }
    }
    true
}

pub fn reconcile_pending_claim(
    home: &Home,
    project_root: &Path,
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
    let tmux_observation = pending_recovery_claim_planned_handle_observation(claim);
    // A spawned claim has already acquired its recovery authority. Do not
    // touch its replacement JSONL (including merely opening or creating it)
    // unless tmux can prove the planned handle is present or absent. An I/O or
    // client failure is a retryable observation failure, never proof that it
    // is safe to relaunch or reconcile this durable intent.
    if claim.spawn_started && tmux_observation == TmuxSessionObservation::Unobserved {
        return Err(RecoveryClaimError::Unobserved(UnobservedEvidence::about(
            UnobservedSession::TmuxHandleUnobserved,
            format!("tmux/{}", claim.replacement_run_id),
        )));
    }
    let tmux_live = tmux_observation == TmuxSessionObservation::Present;
    let session_dir = SessionDirectory::open(project_root)?;
    let (session_file, created_for_pending_append) =
        match session_dir.open_path(&claim.replacement_session_path, true) {
            Ok(file) => (file, false),
            Err(RecoveryClaimError::Io(err)) if err.kind() == std::io::ErrorKind::NotFound => {
                if claim.spawn_started && tmux_observation == TmuxSessionObservation::Absent {
                    return Err(RecoveryClaimError::DeadPlannedHandle);
                }
                (
                    session_dir.create_path(&claim.replacement_session_path)?,
                    true,
                )
            }
            Err(err) => return Err(err),
        };
    #[cfg(test)]
    if let Some(hook) = take_pending_reconcile_after_open_hook(&claim.replacement_run_id) {
        hook();
    }
    if created_for_pending_append {
        session_file.validate_current()?;
        return Ok(Some(PendingRecoveryPlan {
            claim: claim.clone(),
            planned_identity,
            reattach_existing: tmux_live,
            session_file: Some(session_file),
        }));
    }
    let envelopes = session_file.read_checked()?;
    if !pending_session_prefix_matches_claim(claim, &envelopes) {
        return Err(RecoveryClaimError::CorruptClaim);
    }
    if claim.spawn_started && tmux_observation == TmuxSessionObservation::Absent {
        return Err(RecoveryClaimError::DeadPlannedHandle);
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
            session_file: Some(session_file),
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
        session_file: Some(session_file),
    }))
}

/// Whether a durable, already-spawned pending claim still owns its planned
/// tmux handle. This is deliberately narrower than a recovery claim merely
/// existing: a claim written before spawn is not liveness, and a spawned claim
/// whose pane disappeared is a stale intent that dispatch wait must eventually
/// classify as dead rather than wait forever.
///
/// The claim is authenticated by [`load_recovery_claim`] before callers use
/// this answer. It binds the origin and replacement identities durably across
/// a daemon crash between acquisition and the later `ORIGIN=recovery` tx.
pub fn pending_recovery_claim_planned_handle_observation(
    claim: &RecoveryClaim,
) -> TmuxSessionObservation {
    if claim.status != RecoveryClaimStatus::Pending || !claim.spawn_started {
        return TmuxSessionObservation::Absent;
    }
    let planned_identity = RuntimeIdentity::planned(
        claim.replacement_run_id.clone(),
        claim.replacement_runtime_id.clone(),
        claim_planned_boot_id(claim),
    );
    let planned_name = tmux_session_name(&planned_identity);
    let configured = claim
        .planned_tmux_session
        .as_deref()
        .map(observe_tmux_session);
    let derived = observe_tmux_session(&planned_name);
    // Positive liveness wins. Without it, an unobserved probe wins over an
    // absence: only two explicit no-such-session answers may stale this claim.
    match (configured, derived) {
        (Some(TmuxSessionObservation::Present), _) | (_, TmuxSessionObservation::Present) => {
            TmuxSessionObservation::Present
        }
        (Some(TmuxSessionObservation::Unobserved), _) | (_, TmuxSessionObservation::Unobserved) => {
            TmuxSessionObservation::Unobserved
        }
        (Some(TmuxSessionObservation::Absent), TmuxSessionObservation::Absent)
        | (None, TmuxSessionObservation::Absent) => TmuxSessionObservation::Absent,
    }
}

#[derive(Debug)]
pub enum RecoveryClaimError {
    InvalidIdentifier,
    UnresolvableProjectRoot,
    AlreadyClaimed(Box<RecoveryClaim>),
    CorruptClaim,
    MissingClaim,
    DeadPlannedHandle,
    /// The path does not name a regular file directly inside the project's
    /// pinned sessions directory. Split out of [`Self::CorruptClaim`] by
    /// orgasmic:TASK-2QK4P.1.1.1 because it is a DECIDED negative about a path
    /// the claim supplied, while `CorruptClaim` on a session read means the
    /// bytes could not be understood — which is the observer failing.
    ForeignSessionPath,
    /// A predicate could not observe what it was asked about. Distinct from
    /// [`Self::CorruptClaim`] so no caller can quarantine on it
    /// (orgasmic:TASK-2QK4P.1.1.1 acceptance 2).
    Unobserved(UnobservedEvidence),
    Io(std::io::Error),
}

/// WHAT ONE `io::Error` SAYS ABOUT THE THING THAT WAS BEING OBSERVED.
///
/// orgasmic:TASK-2QK4P.1.1.1.1 acceptance 2 — THE POLICY IS WRITTEN HERE ONCE
/// AND NOWHERE ELSE.
///
/// Round four wrote the rule down in prose and then let every call site
/// re-decide it, and the sites diverged exactly as you would expect:
/// [`ClaimDirectory::open`] mapped every non-`NotFound` component-open error to
/// `CorruptClaim` (so `EACCES` and `EIO` quarantined a live claim), while
/// `session_read_evidence` twenty lines away mapped the same `EACCES` to
/// `Unobserved`. F1 and F4 are that divergence.
///
/// Three answers, because an `io::Error` really does carry three different
/// kinds of news:
///
/// - [`Self::Absent`] — `ENOENT`. The thing is NOT THERE, and that is an
///   observation that succeeded. Refusing to act on it would freeze every
///   genuinely dead rescue, so absence stays actionable.
/// - [`Self::Decided`] — the kernel described the path and the description
///   disqualifies it: `ELOOP` (a symlink where a real file must be),
///   `ENOTDIR`/`EISDIR` (wrong kind), `ENAMETOOLONG`, and a non-UTF-8 body
///   where JSON must be. These are FACTS ABOUT THE EVIDENCE, so a caller may
///   quarantine on them.
/// - [`Self::Unobserved`] — the observer failed: `EACCES`/`EPERM`, `EIO`,
///   `EMFILE`/`ENFILE`/`ENOMEM` (descriptor and memory exhaustion), `EINTR`,
///   `EAGAIN`, `ESTALE`, `EBUSY`, `EOVERFLOW`, and — deliberately — EVERY
///   ERRNO NOT NAMED ABOVE. Unknown maps to unobserved because that is the
///   direction whose worst case is a retry.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ObservationClass {
    Absent,
    Decided,
    Unobserved,
}

/// Apply the policy above to one `io::Error`.
pub fn classify_observation(err: &std::io::Error) -> ObservationClass {
    #[cfg(unix)]
    if let Some(code) = err.raw_os_error() {
        return match code {
            libc::ENOENT => ObservationClass::Absent,
            libc::ELOOP | libc::ENOTDIR | libc::EISDIR | libc::ENAMETOOLONG => {
                ObservationClass::Decided
            }
            _ => ObservationClass::Unobserved,
        };
    }
    match err.kind() {
        std::io::ErrorKind::NotFound => ObservationClass::Absent,
        // `read_to_string` on non-UTF-8 bytes: the file was READ, and what it
        // holds is not the JSON text a claim is. A decided fact about content.
        std::io::ErrorKind::InvalidData | std::io::ErrorKind::InvalidInput => {
            ObservationClass::Decided
        }
        _ => ObservationClass::Unobserved,
    }
}

/// The single conversion from a raw `io::Error` to a claim-store error.
///
/// orgasmic:TASK-2QK4P.1.1.1.1 F1/F4 — every read-side claim IO goes through
/// this, so no call site gets to invent its own mapping again. `Absent` stays
/// [`RecoveryClaimError::Io`] because the `NotFound` shape is what callers key
/// their "no claim here" branches on.
pub(crate) fn claim_io_error(
    err: std::io::Error,
    reason: UnobservedSession,
    subject: Option<String>,
) -> RecoveryClaimError {
    match classify_observation(&err) {
        ObservationClass::Absent => RecoveryClaimError::Io(err),
        ObservationClass::Decided => RecoveryClaimError::CorruptClaim,
        ObservationClass::Unobserved => {
            RecoveryClaimError::Unobserved(UnobservedEvidence::new(reason).with_subject(subject))
        }
    }
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
                worker_id: "implementer-claude-stream-json".into(),
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

    /// The lifecycle envelopes of a committed replacement transcript, in write
    /// order, with the `RecoveryOrigin` LAST — the production order, and the one
    /// orgasmic:TASK-2QK4P.1.1 F2 turns on: `PromptDraft` is written before the
    /// link, the link's embedded claim snapshot repeats that draft, and the
    /// draft carries uncapped `git diff --stat` output.
    fn committed_replacement_events(claim: &RecoveryClaim) -> Vec<serde_json::Value> {
        let mut events = vec![
            serde_json::to_value(Lifecycle::Acquire {
                task_id: claim.task_id.clone().unwrap(),
                kind: claim.kind.clone().unwrap(),
                worker_id: claim.worker_id.clone().unwrap(),
            })
            .unwrap(),
            serde_json::to_value(Lifecycle::RunMeta {
                transport: claim.transport.clone().unwrap(),
                harness: claim.harness.clone(),
                project_id: Some(claim.project_id.clone()),
                worktree: claim.worktree.clone(),
                last_path: claim.last_path.clone(),
                stdout_path: claim.stdout_path.clone(),
                dispatch_attempt_token: None,
                role: claim.role.clone(),
                requires_worker_finalize: claim.requires_worker_finalize,
                credential_mode: None,
                driver_config: claim.driver_config.clone().unwrap(),
            })
            .unwrap(),
        ];
        if let Some(native) = claim.planned_native_runtime.as_ref() {
            events.push(
                serde_json::to_value(Lifecycle::NativeRuntime {
                    provider: native.provider.clone(),
                    session_id: native.session_id.clone(),
                    session_path: native.session_path.clone(),
                    launch_argv: native.launch_argv.clone(),
                    resume_argv: native.resume_argv.clone(),
                })
                .unwrap(),
            );
        }
        if let Some(prompt) = claim.draft_prompt.as_ref() {
            events.push(
                serde_json::to_value(Lifecycle::PromptDraft {
                    text: prompt.clone(),
                    sent: false,
                })
                .unwrap(),
            );
        }
        events.push(
            serde_json::to_value(Lifecycle::RecoveryOrigin {
                project_id: claim.project_id.clone(),
                origin_run_id: claim.origin_run_id.clone(),
                origin_session_path: claim.origin_session_path.clone().unwrap(),
                request_id: claim.request_id.clone(),
                replacement_run_id: claim.replacement_run_id.clone(),
                replacement_session_path: claim.replacement_session_path.clone(),
                action: claim.action.clone().unwrap(),
                target: claim.target.clone(),
                claim: Some(serde_json::to_value(claim).unwrap()),
            })
            .unwrap(),
        );
        events
    }

    fn write_committed_replacement(claim: &RecoveryClaim) {
        let identity = RuntimeIdentity {
            run_id: claim.replacement_run_id.clone(),
            runtime_id: claim.replacement_runtime_id.clone(),
            boot_id: claim.boot_id.clone().unwrap(),
        };
        let mut writer =
            orgasmic_core::SessionWriter::open(&claim.replacement_session_path, identity).unwrap();
        for event in committed_replacement_events(claim) {
            writer.append(SessionEventKind::Lifecycle, event).unwrap();
        }
    }

    /// The links one session file carries, asserting the pass COMPLETED.
    ///
    /// Tests that mean "this file contains these links" must not accept an
    /// `Unobserved` pass as an empty one — that is the very substitution
    /// orgasmic:TASK-2QK4P.1.1 exists to make impossible in production, and a
    /// test helper that quietly did it would hide the next regression.
    fn complete_links(
        home: &Home,
        project_root: &Path,
        session_path: &Path,
    ) -> Vec<IndexedRecoveryOrigin> {
        match index_recovery_origins_in_session(home, project_root, session_path, "orgasmic") {
            IndexedRecoveryOrigins::Complete { links, .. } => links,
            IndexedRecoveryOrigins::Unobserved { reason, .. } => {
                panic!("{session_path:?} was expected to be observable, got {reason:?}")
            }
        }
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
    fn duplicate_authenticated_replacements_fail_closed() {
        let tmp = tempfile::tempdir().unwrap();
        let home = Home::at(tmp.path().join("home"));
        home.ensure().unwrap();
        let project_root = tmp.path().join("proj");
        let (spec, _) = sample_spec(
            &home,
            &project_root,
            "run-duplicate-origin",
            "req-duplicate",
            "boot-duplicate",
            false,
        );
        std::fs::remove_file(&spec.origin_session_path).unwrap();
        let mut origin = orgasmic_core::SessionWriter::open(
            &spec.origin_session_path,
            RuntimeIdentity {
                run_id: "run-duplicate-origin".into(),
                runtime_id: "rt-duplicate-origin".into(),
                boot_id: "boot-origin".into(),
            },
        )
        .unwrap();
        origin
            .append(
                SessionEventKind::Lifecycle,
                serde_json::to_value(Lifecycle::RunMeta {
                    transport: "tmux".into(),
                    harness: Some("claude".into()),
                    project_id: Some("orgasmic".into()),
                    worktree: Some(project_root.clone()),
                    last_path: None,
                    stdout_path: None,
                    dispatch_attempt_token: None,
                    role: Some("implementer".into()),
                    requires_worker_finalize: Some(true),
                    credential_mode: None,
                    driver_config: serde_json::json!({}),
                })
                .unwrap(),
            )
            .unwrap();
        drop(origin);
        let plan = plan_pending_recovery_claim(&home, &spec).unwrap();
        let mut first = plan.claim.clone();
        first.status = RecoveryClaimStatus::Committed;
        write_committed_replacement(&first);
        write_claim_atomic(&home, &first).unwrap();

        let mut second = first.clone();
        second.replacement_run_id = "run-duplicate-second".into();
        second.replacement_runtime_id = "rt-duplicate-second".into();
        second.runtime_id = Some(second.replacement_runtime_id.clone());
        second.replacement_session_path =
            project_sessions_dir(&project_root).join("recover-duplicate-second.jsonl");
        second.planned_tmux_session = Some("orgasmic-duplicate-second".into());
        second.authority_tag = None;
        second.authority_tag = Some(authority_tag(&home, &second).unwrap());
        write_committed_replacement(&second);

        let mut links = complete_links(&home, &project_root, &first.replacement_session_path);
        links.extend(complete_links(
            &home,
            &project_root,
            &second.replacement_session_path,
        ));
        assert_eq!(links.len(), 2, "both daemon-authenticated links must index");
        let resolved = resolve_authoritative_recovery_claim(
            &home,
            &project_root,
            "orgasmic",
            "run-duplicate-origin",
            &mut ProjectOriginAuthority::default(),
        )
        .unwrap();
        assert!(matches!(
            resolved,
            ResolvedRecoveryClaim::InvalidQuarantined
        ));
    }

    /// One project shaped the way the run catalog requires it: a real
    /// `.orgasmic/project.org` whose id matches, under a CANONICAL root, since
    /// the links the index emits carry that root and the resolver matches on it.
    fn seed_indexed_project(root: &Path, project_id: &str) -> PathBuf {
        let project_root = root.join("proj");
        std::fs::create_dir_all(project_root.join(".orgasmic")).unwrap();
        std::fs::write(
            project_root.join(".orgasmic/project.org"),
            format!(
                "#+title: {project_id}\n#+orgasmic_version: 1\n\n* PROJECT {project_id}\n:PROPERTIES:\n:ID:               {project_id}\n:END:\n"
            ),
        )
        .unwrap();
        project_root
    }

    fn write_origin_session(spec: &PendingRecoveryClaimSpec, runtime_id: &str, boot_id: &str) {
        std::fs::remove_file(&spec.origin_session_path).unwrap();
        let mut origin = orgasmic_core::SessionWriter::open(
            &spec.origin_session_path,
            RuntimeIdentity {
                run_id: spec.origin_run_id.clone(),
                runtime_id: runtime_id.into(),
                boot_id: boot_id.into(),
            },
        )
        .unwrap();
        origin
            .append(
                SessionEventKind::Lifecycle,
                serde_json::to_value(Lifecycle::RunMeta {
                    transport: "tmux".into(),
                    harness: Some("claude".into()),
                    project_id: Some(spec.project_id.clone()),
                    worktree: spec.worktree.clone(),
                    last_path: None,
                    stdout_path: None,
                    dispatch_attempt_token: None,
                    role: Some("implementer".into()),
                    requires_worker_finalize: Some(true),
                    credential_mode: None,
                    driver_config: serde_json::json!({}),
                })
                .unwrap(),
            )
            .unwrap();
    }

    /// The candidate set a run-catalog-derived index can offer, reproduced
    /// exactly as `collect_recovery_origin_index` built it before
    /// orgasmic:TASK-2QK4P.1.1 removed it: the project's catalog records that
    /// name a replacement, indexed one file at a time.
    ///
    /// It is a PREMISE here and never evidence. These tests assert what the
    /// cache holds precisely so that the resolver's answer below is
    /// demonstrably independent of it — an empty result and a one-element
    /// result are both states in which the resolver must still decide from the
    /// filesystem.
    fn collector_links(
        home: &Home,
        project_root: &Path,
        catalog: &crate::run_catalog::RunCatalog,
    ) -> Vec<IndexedRecoveryOrigin> {
        catalog
            .entries_for_project(project_root)
            .into_iter()
            .filter(|entry| entry.replacement_run_id.is_some())
            .flat_map(|entry| complete_links(home, project_root, &entry.session_path))
            .collect()
    }

    fn refresh_catalog(catalog: &crate::run_catalog::RunCatalog, project_root: &Path) {
        catalog.refresh_dir(
            &project_sessions_dir(project_root),
            Some("orgasmic"),
            project_root,
            SessionScanBudget::DEFAULT,
        );
    }

    /// TASK-2QK4P: an index that has not SEEN the replacement is not an index
    /// that DISPROVES it — driven through the production collector rather than
    /// a literal empty slice.
    ///
    /// `collect_recovery_origin_index` derives its candidate FILES from the run
    /// catalog, and the catalog drops a record on purpose every time the session
    /// writer appends a lifecycle envelope
    /// ([`crate::run_catalog::RunCatalog::invalidate_session`]). The replacement
    /// a crash-recovery replay is asked about is LIVE — the dead daemon spawned
    /// it before it died, and the next daemon reattaches it — so it appends, and
    /// for the window until the next refresh the collector carries no link for
    /// it. That window is what a replay lands in, and before TASK-2QK4P the
    /// silence quarantined the committed claim, the handler fell through to a
    /// fresh acquire, and `/api/runs/<id>/recover` answered 409 `recovery
    /// blocked by an active lease` — held by the very replacement the caller was
    /// asking for.
    ///
    /// This test is production-shaped on purpose (orgasmic:TASK-2QK4P.1 F2): it
    /// asserts the collector DOES index the link while the record is loaded, so
    /// it cannot pass by the collector being accidentally blind everywhere, and
    /// it then invalidates exactly the matching record to produce the silence.
    ///
    /// Injection: restore `matching.len() == 1 && matching[0].claim == claim`
    /// over the FAST index alone — i.e. delete the slow-path enumeration — and
    /// this quarantines a claim its own replacement session verifies.
    // orgasmic:TASK-2QK4P, TASK-2QK4P.1
    #[test]
    fn committed_claim_survives_a_catalog_that_invalidated_its_live_replacement() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().canonicalize().unwrap();
        let home = Home::at(root.join("home"));
        home.ensure().unwrap();
        let project_root = seed_indexed_project(&root, "orgasmic");
        let (spec, _) = sample_spec(
            &home,
            &project_root,
            "run-blind-origin",
            "req-blind",
            "boot-blind",
            false,
        );
        write_origin_session(&spec, "rt-blind-origin", "boot-dead");

        let plan = plan_pending_recovery_claim(&home, &spec).unwrap();
        let committed = commit_recovery_claim(
            &home,
            "orgasmic",
            "run-blind-origin",
            CommitRecoveryDetails {
                runtime_id: plan.claim.replacement_runtime_id.clone(),
                boot_id: "boot-blind".into(),
                action: "start_recovery_run".into(),
                target: "worker".into(),
                draft_prompt: Some("stable draft".into()),
            },
        )
        .unwrap();
        write_committed_replacement(&committed);

        let catalog = crate::run_catalog::RunCatalog::new();
        refresh_catalog(&catalog, &project_root);

        // PREMISE, and the thing a literal `&[]` cannot state: the production
        // collector is not globally blind. With the replacement's record loaded
        // it indexes exactly the one link, so the silence below is the
        // invalidation and nothing else.
        let seen = collector_links(&home, &project_root, &catalog);
        assert_eq!(
            seen.len(),
            1,
            "the collector must index the replacement while its catalog record is loaded: {seen:?}"
        );
        assert_eq!(seen[0].claim, committed);

        // The live replacement appends, the writer invalidates its record, and
        // the collector's candidate FILES lose the only file carrying the link.
        catalog.invalidate_session(&committed.replacement_session_path);
        let blind = collector_links(&home, &project_root, &catalog);
        assert!(
            blind.is_empty(),
            "an invalidated record must leave the production collector silent: {blind:?}"
        );

        assert_eq!(
            verify_committed_claim_against_session(&home, &project_root, &committed),
            ClaimEvidence::Valid
        );
        let resolved = resolve_authoritative_recovery_claim(
            &home,
            &project_root,
            "orgasmic",
            "run-blind-origin",
            &mut ProjectOriginAuthority::default(),
        )
        .unwrap();
        match resolved {
            ResolvedRecoveryClaim::Valid(valid) => assert_eq!(valid, committed),
            other => panic!(
                "a session-verified committed claim with exactly one link on disk must survive a \
                 silent catalog, got {other:?}"
            ),
        }

        // And nothing was quarantined, so the replay stays idempotent: the next
        // caller reads the same committed claim rather than minting a second
        // replacement beside the live one.
        assert_eq!(
            load_recovery_claim(&home, "orgasmic", "run-blind-origin").unwrap(),
            Some(committed)
        );
        assert!(!claim_path(&home, "orgasmic", "run-blind-origin")
            .unwrap()
            .with_extension("json.quarantine")
            .exists());
    }

    struct TwoReplacements {
        home: Home,
        project_root: PathBuf,
        catalog: crate::run_catalog::RunCatalog,
        committed: RecoveryClaim,
        second: RecoveryClaim,
    }

    /// One origin with TWO daemon-HMAC-authenticated replacements, a real
    /// `RunCatalog` refreshed over the project's session directory, and the
    /// premise both hidden-duplicate tests rest on already asserted: while both
    /// records are loaded the production collector finds BOTH links.
    fn seed_two_authenticated_replacements(root: &Path, origin_run_id: &str) -> TwoReplacements {
        let home = Home::at(root.join("home"));
        home.ensure().unwrap();
        let project_root = seed_indexed_project(root, "orgasmic");
        let (spec, _) = sample_spec(
            &home,
            &project_root,
            origin_run_id,
            "req-hidden",
            "boot-hidden",
            false,
        );
        write_origin_session(&spec, "rt-hidden-origin", "boot-dead");

        let plan = plan_pending_recovery_claim(&home, &spec).unwrap();
        let committed = commit_recovery_claim(
            &home,
            "orgasmic",
            origin_run_id,
            CommitRecoveryDetails {
                runtime_id: plan.claim.replacement_runtime_id.clone(),
                boot_id: "boot-hidden".into(),
                action: "start_recovery_run".into(),
                target: "worker".into(),
                draft_prompt: Some("stable draft".into()),
            },
        )
        .unwrap();
        write_committed_replacement(&committed);

        // A SECOND replacement for the same origin, its transcript equally valid
        // and its authority tag equally daemon-keyed.
        let mut second = committed.clone();
        second.replacement_run_id = "run-hidden-second".into();
        second.replacement_runtime_id = "rt-hidden-second".into();
        second.runtime_id = Some(second.replacement_runtime_id.clone());
        second.replacement_session_path =
            project_sessions_dir(&project_root).join("recover-hidden-second.jsonl");
        second.planned_tmux_session = Some("orgasmic-hidden-second".into());
        second.authority_tag = None;
        second.authority_tag = Some(authority_tag(&home, &second).unwrap());
        write_committed_replacement(&second);

        let catalog = crate::run_catalog::RunCatalog::new();
        refresh_catalog(&catalog, &project_root);

        // PREMISE: both links are real, and the production collector finds both
        // while their records are loaded. Whatever the collector reports below
        // is therefore the invalidation and nothing else.
        let seen = collector_links(&home, &project_root, &catalog);
        assert_eq!(
            seen.len(),
            2,
            "both daemon-authenticated links must index while their records are loaded: {seen:?}"
        );

        // The loaded claim verifies against its OWN replacement transcript,
        // which is precisely why an index that has not seen the other one must
        // not be enough. The other replacement is equally authenticated.
        assert_eq!(
            verify_committed_claim_against_session(&home, &project_root, &committed),
            ClaimEvidence::Valid
        );

        TwoReplacements {
            home,
            project_root,
            catalog,
            committed,
            second,
        }
    }

    /// TASK-2QK4P.1 F1: the paired case, and the one the silence-is-safe rule
    /// admitted — TWO daemon-authenticated replacements for one origin, with
    /// BOTH catalog records invalidated.
    ///
    /// The collector is silent for exactly the same reason as in
    /// `committed_claim_survives_a_catalog_that_invalidated_its_live_replacement`,
    /// and the empty slice it returns is byte-identical in both. That is the
    /// whole finding: an empty slice makes no statement about completeness, so a
    /// resolver that reads it as "nothing contradicts the claim I loaded" hands
    /// back one of two replacements and never discovers the other —
    /// `/api/runs/<id>/recover` then names it, and inventory clears the recovery
    /// actions. `duplicate_authenticated_replacements_fail_closed` already rules
    /// this state a safety violation; this test proves the ruling survives
    /// arrival through the production collector rather than only through a
    /// literal slice.
    ///
    /// Injection: decide from the fast index alone. This then returns `Valid`.
    // orgasmic:TASK-2QK4P.1
    #[test]
    fn a_hidden_duplicate_authenticated_replacement_fails_closed_through_the_collector() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().canonicalize().unwrap();
        let seeded = seed_two_authenticated_replacements(&root, "run-hidden-origin");

        // Both replacements are live and appending, so both records are gone.
        seeded
            .catalog
            .invalidate_session(&seeded.committed.replacement_session_path);
        seeded
            .catalog
            .invalidate_session(&seeded.second.replacement_session_path);
        let blind = collector_links(&seeded.home, &seeded.project_root, &seeded.catalog);
        assert!(
            blind.is_empty(),
            "two invalidated records must leave the production collector silent: {blind:?}"
        );

        let resolved = resolve_authoritative_recovery_claim(
            &seeded.home,
            &seeded.project_root,
            "orgasmic",
            "run-hidden-origin",
            &mut ProjectOriginAuthority::default(),
        )
        .unwrap();
        assert!(
            matches!(resolved, ResolvedRecoveryClaim::InvalidQuarantined),
            "a second authenticated replacement hidden by an invalidated catalog record must \
             still fail closed, got {resolved:?}"
        );
    }

    /// TASK-2QK4P.1: the same hole one link WIDER, and the reason the resolver
    /// does not consult `indexed_origins` on this branch at all.
    ///
    /// Only the SECOND replacement's record is invalidated here, so the
    /// collector reports exactly one link and that link agrees with the loaded
    /// claim — an index that reads as unanimous confirmation. It is not: it is
    /// the same cache saying nothing about the file it was not holding. Closing
    /// the hole only on a ZERO match would leave this arrangement — which needs
    /// one live appender rather than two, and is therefore the likelier of the
    /// pair — admitting a hidden authenticated duplicate.
    ///
    /// Injection: decide from the fast index alone. `matching.len() > 1` is
    /// false and the one link equals the claim, so the injected predicate finds
    /// nothing contradictory and returns `Valid`.
    // orgasmic:TASK-2QK4P.1
    #[test]
    fn a_partially_loaded_index_does_not_confirm_uniqueness() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().canonicalize().unwrap();
        let seeded = seed_two_authenticated_replacements(&root, "run-hidden-origin");

        // Only the second replacement appends, so only its record is dropped.
        seeded
            .catalog
            .invalidate_session(&seeded.second.replacement_session_path);
        let partial = collector_links(&seeded.home, &seeded.project_root, &seeded.catalog);
        assert_eq!(
            partial.len(),
            1,
            "one invalidated record must leave the collector holding exactly the other: {partial:?}"
        );
        assert_eq!(
            partial[0].claim, seeded.committed,
            "and the link it still holds is the loaded claim's own — the index looks unanimous"
        );

        let resolved = resolve_authoritative_recovery_claim(
            &seeded.home,
            &seeded.project_root,
            "orgasmic",
            "run-hidden-origin",
            &mut ProjectOriginAuthority::default(),
        )
        .unwrap();
        assert!(
            matches!(resolved, ResolvedRecoveryClaim::InvalidQuarantined),
            "an index holding one link and blind to a second authenticated replacement must not \
             confirm uniqueness, got {resolved:?}"
        );
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
            authority_tag: None,
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
            spawn_started: false,
        };
        assert_eq!(
            verify_committed_claim_against_session(&home, &project_root, &claim),
            ClaimEvidence::Invalid
        );
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
                serde_json::to_value(orgasmic_core::session::Lifecycle::Acquire {
                    task_id: "TASK-1".into(),
                    kind: "worker".into(),
                    worker_id: "implementer-claude-stream-json".into(),
                })
                .unwrap(),
            )
            .unwrap();
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
                    dispatch_attempt_token: None,
                    role: Some("implementer".into()),
                    requires_worker_finalize: Some(true),
                    credential_mode: None,
                    driver_config: spec.driver_config.clone(),
                })
                .unwrap(),
            )
            .unwrap();
        if let Some(native) = plan.claim.planned_native_runtime.as_ref() {
            writer
                .append(
                    orgasmic_core::session::SessionEventKind::Lifecycle,
                    serde_json::to_value(orgasmic_core::session::Lifecycle::NativeRuntime {
                        provider: native.provider.clone(),
                        session_id: native.session_id.clone(),
                        session_path: native.session_path.clone(),
                        launch_argv: native.launch_argv.clone(),
                        resume_argv: native.resume_argv.clone(),
                    })
                    .unwrap(),
                )
                .unwrap();
        }
        writer
            .append(
                orgasmic_core::session::SessionEventKind::Lifecycle,
                serde_json::to_value(orgasmic_core::session::Lifecycle::PromptDraft {
                    text: spec.draft_prompt.clone().unwrap(),
                    sent: false,
                })
                .unwrap(),
            )
            .unwrap();
        let mut committed_snapshot = plan.claim.clone();
        committed_snapshot.status = RecoveryClaimStatus::Committed;
        writer
            .append(
                orgasmic_core::session::SessionEventKind::Lifecycle,
                serde_json::to_value(orgasmic_core::session::Lifecycle::RecoveryOrigin {
                    project_id: "orgasmic".into(),
                    origin_run_id: "run-origin".into(),
                    origin_session_path: spec.origin_session_path.clone(),
                    request_id: "req-pending".into(),
                    replacement_run_id: plan.claim.replacement_run_id.clone(),
                    replacement_session_path: replacement_path.clone(),
                    action: "start_recovery_run".into(),
                    target: Some("worker".into()),
                    claim: Some(serde_json::to_value(committed_snapshot).unwrap()),
                })
                .unwrap(),
            )
            .unwrap();
        drop(writer);

        let written = orgasmic_core::session::read_session_file(&replacement_path).unwrap();
        assert!(
            claim_immutable_plan_matches_session(&plan.claim, &written),
            "written lifecycle does not match claim: {written:#?}"
        );
        assert!(
            pending_session_prefix_matches_claim(&plan.claim, &written),
            "written lifecycle prefix does not match claim: {written:#?}"
        );

        let plan = reconcile_pending_claim(&home, &project_root, &plan.claim)
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
        let reconciled = reconcile_pending_claim(&home, &project_root, &plan.claim)
            .unwrap()
            .expect("pending plan");
        assert_eq!(reconciled.planned_identity.boot_id, "boot-persisted");
    }

    #[test]
    #[cfg(unix)]
    fn reconcile_pending_rejects_symlink_replacement() {
        let tmp = tempfile::tempdir().unwrap();
        let home = Home::at(tmp.path().join("home"));
        home.ensure().unwrap();
        let project_root = tmp.path().join("proj");
        let (spec, replacement_path) = sample_spec(
            &home,
            &project_root,
            "run-pending-symlink",
            "req-pending-symlink",
            "boot-pending-symlink",
            false,
        );
        let plan = plan_pending_recovery_claim(&home, &spec).unwrap();
        let target = replacement_path.with_extension("target");
        std::fs::write(&target, "").unwrap();
        std::os::unix::fs::symlink(&target, &replacement_path).unwrap();

        assert!(reconcile_pending_claim(&home, &project_root, &plan.claim).is_err());
    }

    #[test]
    #[cfg(unix)]
    fn reconcile_pending_rejects_rename_swap_after_open() {
        let tmp = tempfile::tempdir().unwrap();
        let home = Home::at(tmp.path().join("home"));
        home.ensure().unwrap();
        let project_root = tmp.path().join("proj");
        let (spec, replacement_path) = sample_spec(
            &home,
            &project_root,
            "run-pending-rename",
            "req-pending-rename",
            "boot-pending-rename",
            false,
        );
        let plan = plan_pending_recovery_claim(&home, &spec).unwrap();
        std::fs::write(&replacement_path, "").unwrap();
        let displaced = replacement_path.with_extension("opened");
        let replacement_for_hook = replacement_path.clone();
        arm_pending_reconcile_after_open_hook(
            &plan.claim.replacement_run_id,
            Box::new(move || {
                std::fs::rename(&replacement_for_hook, &displaced).unwrap();
                std::fs::write(&replacement_for_hook, "").unwrap();
            }),
        );

        assert!(reconcile_pending_claim(&home, &project_root, &plan.claim).is_err());
        assert!(
            take_pending_reconcile_after_open_hook(&plan.claim.replacement_run_id).is_none(),
            "this test's own reconcile must be the call that consumed the hook"
        );
    }

    #[test]
    #[cfg(unix)]
    fn retained_pending_authority_rejects_stale_first_append() {
        let tmp = tempfile::tempdir().unwrap();
        let home = Home::at(tmp.path().join("home"));
        home.ensure().unwrap();
        let project_root = tmp.path().join("proj");
        let (spec, replacement_path) = sample_spec(
            &home,
            &project_root,
            "run-pending-stale",
            "req-pending-stale",
            "boot-pending-stale",
            false,
        );
        let plan = plan_pending_recovery_claim(&home, &spec).unwrap();
        let reconciled = reconcile_pending_claim(&home, &project_root, &plan.claim)
            .unwrap()
            .expect("pending plan retains replacement authority");
        let authority = reconciled.session_file.expect("retained session file");
        let displaced = replacement_path.with_extension("opened");
        std::fs::rename(&replacement_path, &displaced).unwrap();
        std::fs::write(&replacement_path, "").unwrap();

        assert!(authority.clone_file_for_append().is_err());
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
                    dispatch_attempt_token: None,
                    role: Some("implementer".into()),
                    requires_worker_finalize: Some(true),
                    credential_mode: None,
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
                    dispatch_attempt_token: None,
                    role: Some("implementer".into()),
                    requires_worker_finalize: Some(true),
                    credential_mode: None,
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
                    worker_id: "implementer-claude-stream-json".into(),
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

        let resolved = resolve_authoritative_recovery_claim(
            &home,
            &project_root,
            "orgasmic",
            "run-corrupt-origin",
            &mut ProjectOriginAuthority::default(),
        )
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
        let mut claim = RecoveryClaim {
            plan_version: Some(1),
            authority_tag: None,
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
            worker_id: Some("implementer-claude-stream-json".into()),
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
            spawn_started: false,
        };
        claim.authority_tag = Some(authority_tag(&home, &claim).unwrap());
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
        let home = Home::at(tmp.path().join("home"));
        home.ensure().unwrap();
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
                    worker_id: "implementer-claude-stream-json".into(),
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
            authority_tag: None,
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
            spawn_started: false,
        };
        assert_eq!(
            verify_committed_claim_against_session(&home, &project_root, &claim),
            ClaimEvidence::Invalid
        );
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
        let home = Home::at(tmp.path().join("home"));
        home.ensure().unwrap();
        let project_root = tmp.path().join("proj");
        let replacement_path = project_sessions_dir(&project_root).join("recover-index.jsonl");
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
        assert!(complete_links(&home, &project_root, &replacement_path).is_empty());
    }

    // ===================================================================
    // orgasmic:TASK-2QK4P.1.1 — round three, and the class rather than the
    // instance.
    //
    // Rounds one and two each closed the case they were handed and left the
    // same sentence true one layer down: AN OBSERVATION THAT FAILED WAS
    // REPORTED AS AN OBSERVATION THAT SUCCEEDED AND FOUND NOTHING. Round one
    // was an EMPTY catalog index read as non-contradiction; round two a
    // ONE-ELEMENT index read as uniqueness; round three is a per-file scan
    // that FAILED read as an empty-but-complete file, and a bounded scan's
    // skipped middle read as absence.
    // ===================================================================

    /// A raw session line, written without [`orgasmic_core::SessionWriter`].
    ///
    /// The writer refuses `text_chunk` driver events on pane transports and
    /// caps driver payloads, both correct in production and both in the way of
    /// building the one shape this file must contain: a transcript large enough
    /// to push a lifecycle line out of BOTH bounded scan windows.
    fn raw_session_line(
        seq: u64,
        identity: &RuntimeIdentity,
        kind: SessionEventKind,
        event: serde_json::Value,
    ) -> String {
        let envelope = SessionEnvelope {
            seq,
            time: chrono::Utc::now(),
            run_id: identity.run_id.clone(),
            runtime_id: identity.runtime_id.clone(),
            boot_id: identity.boot_id.clone(),
            kind,
            event,
        };
        let mut line = serde_json::to_string(&envelope).unwrap();
        line.push('\n');
        line
    }

    /// Bytes of one transcript line. Under
    /// [`orgasmic_core::session`]'s retention filter these are `driver_event`
    /// lines that are not lifecycle-bearing, so they are dropped unparsed —
    /// exactly what a real TUI transcript costs a scan.
    const FILLER_LINE_BYTES: usize = 32 * 1024;

    /// The same replacement transcript [`write_committed_replacement`] writes,
    /// with `filler_before` bytes of transcript between the head lifecycle
    /// events and the `RecoveryOrigin`, and `filler_after` bytes behind it.
    ///
    /// orgasmic:TASK-2QK4P.1.1 F2 — with `filler_before` past
    /// [`SessionScanBudget::DEFAULT`]'s 128 KiB prefix and `filler_after` past
    /// its 64 KiB tail, the authenticated link sits in the region a bounded scan
    /// SKIPS. `SessionLifecycleScan::truncated` says that region is unknown; the
    /// bug was reading it as empty.
    fn write_committed_replacement_with_gap(
        claim: &RecoveryClaim,
        filler_before: usize,
        filler_after: usize,
    ) {
        let identity = RuntimeIdentity {
            run_id: claim.replacement_run_id.clone(),
            runtime_id: claim.replacement_runtime_id.clone(),
            boot_id: claim.boot_id.clone().unwrap(),
        };
        let mut events = committed_replacement_events(claim);
        let origin_event = events.pop().expect("recovery_origin is written last");
        let mut out = String::new();
        let mut seq = 0_u64;
        let push_filler = |out: &mut String, seq: &mut u64, bytes: usize| {
            let mut written = 0;
            while written < bytes {
                let line = raw_session_line(
                    *seq,
                    &identity,
                    SessionEventKind::DriverEvent,
                    serde_json::json!({"type": "text_chunk", "text": "x".repeat(FILLER_LINE_BYTES)}),
                );
                written += line.len();
                *seq += 1;
                out.push_str(&line);
            }
        };
        for event in events {
            out.push_str(&raw_session_line(
                seq,
                &identity,
                SessionEventKind::Lifecycle,
                event,
            ));
            seq += 1;
        }
        push_filler(&mut out, &mut seq, filler_before);
        let origin_offset = out.len();
        out.push_str(&raw_session_line(
            seq,
            &identity,
            SessionEventKind::Lifecycle,
            origin_event,
        ));
        seq += 1;
        push_filler(&mut out, &mut seq, filler_after);

        std::fs::create_dir_all(claim.replacement_session_path.parent().unwrap()).unwrap();
        std::fs::write(&claim.replacement_session_path, &out).unwrap();

        // PREMISE, asserted rather than assumed: the link really is outside BOTH
        // windows. Without this the test could pass for the boring reason that
        // the file was small enough to be read whole.
        let budget = SessionScanBudget::DEFAULT;
        assert!(
            origin_offset as u64 > budget.prefix_bytes,
            "recovery_origin at {origin_offset} must be past the {} byte prefix window",
            budget.prefix_bytes
        );
        assert!(
            (out.len() - origin_offset) as u64 > budget.tail_bytes,
            "recovery_origin must be more than the {} byte tail window from the end",
            budget.tail_bytes
        );
        let bounded =
            orgasmic_core::scan_session_lifecycle(&claim.replacement_session_path, budget)
                .expect("the bounded scan itself must succeed; the link is skipped, not malformed");
        assert!(bounded.truncated, "the fixture must truncate");
        assert!(
            !bounded.envelopes.iter().any(|envelope| matches!(
                serde_json::from_value::<Lifecycle>(envelope.event.clone()),
                Ok(Lifecycle::RecoveryOrigin { .. })
            )),
            "the bounded scan must NOT see the link — that is the whole fixture"
        );
    }

    /// Tear a line in the MIDDLE of a session file.
    ///
    /// `"kind":"lifecycle"` in the envelope header makes the retention filter
    /// keep the line, and the truncated body then fails to parse — the shape a
    /// daemon crash mid-append leaves. The second line is what makes it a
    /// MIDDLE tear rather than a trailing one, and that is the whole point: the
    /// daemon reopens a torn session and appends after it, so yesterday's tear
    /// sits inside today's file, and a reader that stops there has not seen the
    /// lines behind it. A tear with nothing after it hides nothing and is
    /// deliberately NOT this fixture.
    fn tear_a_line_in_the_middle(session_path: &Path) {
        use std::io::Write as _;
        let mut file = OpenOptions::new().append(true).open(session_path).unwrap();
        file.write_all(b"{\"seq\":9001,\"kind\":\"lifecycle\",\"event\":{\"phase\":\n")
            .unwrap();
        file.write_all(b"{\"seq\":9002,\"kind\":\"driver_event\",\"event\":{\"type\":\"text_chunk\",\"text\":\"after the tear\"}}\n")
            .unwrap();
    }

    fn quarantine_exists(home: &Home, project_id: &str, origin_run_id: &str) -> bool {
        claim_path(home, project_id, origin_run_id)
            .unwrap()
            .with_extension("json.quarantine")
            .exists()
    }

    /// orgasmic:TASK-2QK4P.1.1 F1 — a member file that could not be INDEXED is
    /// not a member file that contains NOTHING.
    ///
    /// Two daemon-HMAC-authenticated replacements exist for one origin. The
    /// loaded claim's own session is readable and verifies. The second one's
    /// JSONL carries one malformed lifecycle line, so its scan fails — and
    /// before this round that failure returned `IndexedRecoveryOrigins::default()`,
    /// an empty-but-successful pass, which the enumerator folded into a union it
    /// then labelled AUTHORITATIVE. The resolver saw exactly one link, equal to
    /// the claim it had loaded, and returned `Valid` — silently choosing one of
    /// two lease-holders and never discovering the other.
    ///
    /// Injection: make the scan-failure paths return an empty `Complete`. This
    /// then returns `Valid`.
    // orgasmic:TASK-2QK4P.1.1
    #[test]
    fn a_scan_failure_in_a_second_replacement_is_not_an_empty_file() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().canonicalize().unwrap();
        let seeded = seed_two_authenticated_replacements(&root, "run-hidden-origin");
        tear_a_line_in_the_middle(&seeded.second.replacement_session_path);

        let resolved = resolve_authoritative_recovery_claim(
            &seeded.home,
            &seeded.project_root,
            "orgasmic",
            "run-hidden-origin",
            &mut ProjectOriginAuthority::default(),
        )
        .unwrap();
        assert!(
            matches!(resolved, ResolvedRecoveryClaim::Unobserved(_)),
            "an unindexable member file must leave the enumeration unresolved rather than \
             confirming uniqueness, got {resolved:?}"
        );

        // And the pass over that one file states failure rather than emptiness,
        // which is the mechanism the admission above rests on.
        let indexed = index_recovery_origins_in_session(
            &seeded.home,
            &seeded.project_root,
            &seeded.second.replacement_session_path,
            "orgasmic",
        );
        assert!(
            matches!(
                indexed,
                IndexedRecoveryOrigins::Unobserved {
                    reason: UnobservedEvidence {
                        reason: UnobservedSession::SessionUnreadable,
                        ..
                    },
                    ..
                }
            ),
            "one malformed lifecycle line must make the pass unobserved, got {indexed:?}"
        );

        // Ruling 1: unresolved is not invalid. The claim is untouched, so the
        // rescue keeps its idempotency across the failure.
        assert_eq!(
            load_recovery_claim(&seeded.home, "orgasmic", "run-hidden-origin").unwrap(),
            Some(seeded.committed.clone())
        );
        assert!(!quarantine_exists(
            &seeded.home,
            "orgasmic",
            "run-hidden-origin"
        ));

        // And what the malformed line was HIDING really was a safety violation:
        // repair the file and the same resolver finds two authorities.
        let repaired = std::fs::read_to_string(&seeded.second.replacement_session_path).unwrap();
        let repaired: String = repaired
            .lines()
            .filter(|line| serde_json::from_str::<SessionEnvelope>(line).is_ok())
            .map(|line| format!("{line}\n"))
            .collect();
        std::fs::write(&seeded.second.replacement_session_path, repaired).unwrap();
        let resolved = resolve_authoritative_recovery_claim(
            &seeded.home,
            &seeded.project_root,
            "orgasmic",
            "run-hidden-origin",
            &mut ProjectOriginAuthority::default(),
        )
        .unwrap();
        assert!(
            matches!(resolved, ResolvedRecoveryClaim::InvalidQuarantined),
            "with the file readable the second authenticated replacement must fail closed, got \
             {resolved:?}"
        );
    }

    /// orgasmic:TASK-2QK4P.1.1 F1, the other direction and the reviewer's open
    /// question 1 as a ruling: a transient read failure must not DESTROY a valid
    /// live rescue.
    ///
    /// One committed claim, one replacement, both fine — and an unrelated
    /// sibling JSONL in the same sessions directory that cannot be scanned. The
    /// enumeration is unresolved, so authority is suppressed; but the claim is
    /// not renamed, and once the sibling is gone the very same claim resolves
    /// `Valid`. Had `Unobserved` been folded into `InvalidQuarantined`, one
    /// unreadable file anywhere in the directory would have permanently
    /// quarantined a live rescue and let the handler mint a second replacement
    /// beside it — a BLOCK SHIP in the opposite direction.
    // orgasmic:TASK-2QK4P.1.1
    #[test]
    fn an_unreadable_sibling_session_suppresses_authority_without_destroying_the_claim() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().canonicalize().unwrap();
        let home = Home::at(root.join("home"));
        home.ensure().unwrap();
        let project_root = seed_indexed_project(&root, "orgasmic");
        let (spec, _) = sample_spec(
            &home,
            &project_root,
            "run-transient-origin",
            "req-transient",
            "boot-transient",
            false,
        );
        write_origin_session(&spec, "rt-transient-origin", "boot-dead");
        let plan = plan_pending_recovery_claim(&home, &spec).unwrap();
        let committed = commit_recovery_claim(
            &home,
            "orgasmic",
            "run-transient-origin",
            CommitRecoveryDetails {
                runtime_id: plan.claim.replacement_runtime_id.clone(),
                boot_id: "boot-transient".into(),
                action: "start_recovery_run".into(),
                target: "worker".into(),
                draft_prompt: Some("stable draft".into()),
            },
        )
        .unwrap();
        write_committed_replacement(&committed);

        let sibling = project_sessions_dir(&project_root).join("run-unrelated-torn.jsonl");
        std::fs::write(&sibling, "").unwrap();
        tear_a_line_in_the_middle(&sibling);

        let resolved = resolve_authoritative_recovery_claim(
            &home,
            &project_root,
            "orgasmic",
            "run-transient-origin",
            &mut ProjectOriginAuthority::default(),
        )
        .unwrap();
        assert!(
            matches!(resolved, ResolvedRecoveryClaim::Unobserved(_)),
            "an unreadable sibling must suppress authority, got {resolved:?}"
        );
        assert_eq!(
            load_recovery_claim(&home, "orgasmic", "run-transient-origin").unwrap(),
            Some(committed.clone()),
            "and it must NOT rename the claim: unknown completeness is not invalid evidence"
        );
        assert!(!quarantine_exists(
            &home,
            "orgasmic",
            "run-transient-origin"
        ));

        std::fs::remove_file(&sibling).unwrap();
        let resolved = resolve_authoritative_recovery_claim(
            &home,
            &project_root,
            "orgasmic",
            "run-transient-origin",
            &mut ProjectOriginAuthority::default(),
        )
        .unwrap();
        match resolved {
            ResolvedRecoveryClaim::Valid(valid) => assert_eq!(valid, committed),
            other => panic!("the rescue must survive the transient failure, got {other:?}"),
        }
    }

    /// orgasmic:TASK-2QK4P.1.1 F2 — a bounded scan's SKIPPED MIDDLE is unknown,
    /// and reading it as absence hides a second authority.
    ///
    /// The second replacement's transcript is larger than the 128 KiB prefix
    /// plus 64 KiB tail, and its authenticated `RecoveryOrigin` sits in the gap
    /// between them. `SessionLifecycleScan` documents that gap as unknown; the
    /// enumerator ignored `truncated` and labelled the union of retained links
    /// authoritative, so the resolver saw one link — the loaded claim's own —
    /// and returned `Valid`.
    ///
    /// The production route to a link that far in is the one the review named:
    /// `PromptDraft` is written BEFORE `RecoveryOrigin`, the committed snapshot
    /// embedded in the link REPEATS that draft, and the draft carries uncapped
    /// `git diff --stat` output. The fixture reaches the same state with
    /// transcript bytes, which is cheaper to build and identical to the scanner.
    ///
    /// Injection: drop the escalation in `complete_session_scan` and index from
    /// the bounded scan. This then returns `Valid`.
    // orgasmic:TASK-2QK4P.1.1
    #[test]
    fn a_recovery_origin_outside_both_scan_windows_still_fails_closed() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().canonicalize().unwrap();
        let seeded = seed_two_authenticated_replacements(&root, "run-buried-origin");
        std::fs::remove_file(&seeded.second.replacement_session_path).unwrap();
        write_committed_replacement_with_gap(&seeded.second, 192 * 1024, 128 * 1024);

        let resolved = resolve_authoritative_recovery_claim(
            &seeded.home,
            &seeded.project_root,
            "orgasmic",
            "run-buried-origin",
            &mut ProjectOriginAuthority::default(),
        )
        .unwrap();
        assert!(
            matches!(resolved, ResolvedRecoveryClaim::InvalidQuarantined),
            "a second authenticated replacement whose link sits in the bounded scan's skipped \
             middle must still fail closed, got {resolved:?}"
        );
    }

    /// orgasmic:TASK-2QK4P.1.1 F2, the direction that made the round-two fix
    /// produce the very duplicate it existed to prevent.
    ///
    /// Here the buried link is the LOADED claim's own. Under the bounded scan
    /// the enumeration finds no matching link, `uniquely_confirmed` is false,
    /// and the committed claim is QUARANTINED — and because the catalog uses the
    /// same bounded scanner, no later pass can rediscover it. The refusal is
    /// PERMANENT rather than the retry the code comment claimed, and the handler
    /// then reaches the no-plan branch and mints another replacement beside the
    /// live one.
    ///
    /// Injection: drop the escalation in `complete_session_scan`. This then
    /// returns `Missing` with the claim quarantined.
    // orgasmic:TASK-2QK4P.1.1
    #[test]
    fn a_buried_link_does_not_permanently_destroy_its_own_committed_claim() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().canonicalize().unwrap();
        let home = Home::at(root.join("home"));
        home.ensure().unwrap();
        let project_root = seed_indexed_project(&root, "orgasmic");
        let (spec, _) = sample_spec(
            &home,
            &project_root,
            "run-buried-solo-origin",
            "req-buried",
            "boot-buried",
            false,
        );
        write_origin_session(&spec, "rt-buried-origin", "boot-dead");
        let plan = plan_pending_recovery_claim(&home, &spec).unwrap();
        let committed = commit_recovery_claim(
            &home,
            "orgasmic",
            "run-buried-solo-origin",
            CommitRecoveryDetails {
                runtime_id: plan.claim.replacement_runtime_id.clone(),
                boot_id: "boot-buried".into(),
                action: "start_recovery_run".into(),
                target: "worker".into(),
                draft_prompt: Some("stable draft".into()),
            },
        )
        .unwrap();
        write_committed_replacement_with_gap(&committed, 192 * 1024, 128 * 1024);

        // The claim verifies against its own transcript — `verify` reads the
        // whole file — so the ONLY thing that could refuse it is the bounded
        // enumeration failing to find the link it just verified.
        assert_eq!(
            verify_committed_claim_against_session(&home, &project_root, &committed),
            ClaimEvidence::Valid
        );

        let resolved = resolve_authoritative_recovery_claim(
            &home,
            &project_root,
            "orgasmic",
            "run-buried-solo-origin",
            &mut ProjectOriginAuthority::default(),
        )
        .unwrap();
        match resolved {
            ResolvedRecoveryClaim::Valid(valid) => assert_eq!(valid, committed),
            other => panic!(
                "a committed claim whose own link sits in the skipped middle must not be \
                 quarantined — that loss is permanent, got {other:?}"
            ),
        }
        assert!(!quarantine_exists(
            &home,
            "orgasmic",
            "run-buried-solo-origin"
        ));
    }

    /// orgasmic:TASK-2QK4P.1.1 F3 — the missing/corrupt branch, which is the
    /// same defect wearing a different hat.
    ///
    /// `Ok(None)` and `CorruptClaim` used to reconstruct from the
    /// CATALOG-derived slice while the committed branch enumerated the
    /// filesystem. With the replacement's catalog record invalidated — which the
    /// session writer does on every lifecycle append, so a LIVE replacement is
    /// exactly the case — the slice is silent, reconstruction returns `Missing`,
    /// and `post_run_recover` reads `Missing` as permission to mint a new claim
    /// and session BESIDE the authenticated replacement already on disk.
    ///
    /// Two phases, because `Missing` is dangerous at both widths: one
    /// replacement must RECONSTRUCT rather than report `Missing`, and two must
    /// fail closed.
    ///
    /// Injection: reconstruct from the catalog-derived index. Phase one then
    /// returns `Missing`.
    // orgasmic:TASK-2QK4P.1.1
    #[test]
    fn a_missing_claim_reconstructs_from_the_same_authoritative_set() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().canonicalize().unwrap();
        let seeded = seed_two_authenticated_replacements(&root, "run-missing-origin");
        // The second replacement is not on disk for phase one; the catalog is
        // rebuilt from what remains so its premise below is about invalidation
        // and not about a record for a file that no longer exists.
        std::fs::remove_file(&seeded.second.replacement_session_path).unwrap();
        refresh_catalog(&seeded.catalog, &seeded.project_root);
        // The claim file is gone — a lost claim is the branch under test.
        std::fs::remove_file(claim_path(&seeded.home, "orgasmic", "run-missing-origin").unwrap())
            .unwrap();

        // PREMISE: both live records are invalidated, so a catalog-derived
        // candidate set is silent. That silence is what used to become `Missing`.
        seeded
            .catalog
            .invalidate_session(&seeded.committed.replacement_session_path);
        let blind = collector_links(&seeded.home, &seeded.project_root, &seeded.catalog);
        assert!(
            blind.is_empty(),
            "an invalidated record must leave a catalog-derived set silent: {blind:?}"
        );

        let resolved = resolve_authoritative_recovery_claim(
            &seeded.home,
            &seeded.project_root,
            "orgasmic",
            "run-missing-origin",
            &mut ProjectOriginAuthority::default(),
        )
        .unwrap();
        match resolved {
            ResolvedRecoveryClaim::Reconstructed(claim) => {
                assert_eq!(claim, seeded.committed)
            }
            other => panic!(
                "a lost claim whose authenticated replacement is on disk must be reconstructed, \
                 never reported Missing — Missing is permission to mint a second one, got {other:?}"
            ),
        }

        // Phase two: the second authenticated replacement is back and the claim
        // is lost again. `Missing` here would put a THIRD replacement beside two.
        std::fs::remove_file(claim_path(&seeded.home, "orgasmic", "run-missing-origin").unwrap())
            .unwrap();
        write_committed_replacement(&seeded.second);
        let resolved = resolve_authoritative_recovery_claim(
            &seeded.home,
            &seeded.project_root,
            "orgasmic",
            "run-missing-origin",
            &mut ProjectOriginAuthority::default(),
        )
        .unwrap();
        assert!(
            matches!(resolved, ResolvedRecoveryClaim::InvalidQuarantined),
            "two authenticated replacements must fail closed on the missing branch too, got \
             {resolved:?}"
        );
    }

    /// orgasmic:TASK-2QK4P.1.1 F4 — ONE authoritative snapshot per decision,
    /// shared across claims, pinned by the FILE-SCAN COUNT.
    ///
    /// The inventory loop calls the resolver once per failed-recoverable record.
    /// Round two enumerated inside the resolver, so a project with two committed
    /// claims rescanned every session file in the project twice on every poll —
    /// `GET /runs` back to `O(failed_runs × session_bytes)`, which is precisely
    /// what the caller had removed on purpose.
    ///
    /// Injection: build a fresh `ProjectOriginAuthority` per resolver call. The
    /// count doubles and this goes red.
    // orgasmic:TASK-2QK4P.1.1
    #[test]
    fn one_authority_serves_every_claim_in_a_project() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().canonicalize().unwrap();
        let home = Home::at(root.join("home"));
        home.ensure().unwrap();
        let project_root = seed_indexed_project(&root, "orgasmic");

        for (origin, boot) in [
            ("run-multi-a", "boot-multi-a"),
            ("run-multi-b", "boot-multi-b"),
        ] {
            let (spec, _) = sample_spec(&home, &project_root, origin, "req-multi", boot, false);
            write_origin_session(&spec, &format!("rt-{origin}"), "boot-dead");
            let plan = plan_pending_recovery_claim(&home, &spec).unwrap();
            let committed = commit_recovery_claim(
                &home,
                "orgasmic",
                origin,
                CommitRecoveryDetails {
                    runtime_id: plan.claim.replacement_runtime_id.clone(),
                    boot_id: boot.into(),
                    action: "start_recovery_run".into(),
                    target: "worker".into(),
                    draft_prompt: Some("stable draft".into()),
                },
            )
            .unwrap();
            write_committed_replacement(&committed);
        }
        let files_on_disk = std::fs::read_dir(project_sessions_dir(&project_root))
            .unwrap()
            .flatten()
            .filter(|entry| entry.path().extension().and_then(|ext| ext.to_str()) == Some("jsonl"))
            .count() as u64;
        assert_eq!(files_on_disk, 4, "two origins and two replacements");

        let mut authority = ProjectOriginAuthority::default();
        for origin in ["run-multi-a", "run-multi-b"] {
            let resolved = resolve_authoritative_recovery_claim(
                &home,
                &project_root,
                "orgasmic",
                origin,
                &mut authority,
            )
            .unwrap();
            assert!(
                matches!(resolved, ResolvedRecoveryClaim::Valid(_)),
                "{origin} must resolve valid, got {resolved:?}"
            );
        }
        assert_eq!(
            authority.cost().files,
            files_on_disk,
            "the project's session files must be scanned ONCE for the whole decision, not once \
             per claim"
        );
    }

    /// One healthy committed claim with its replacement and origin sessions on
    /// disk, ready to resolve `Valid`.
    fn seed_one_healthy_claim(root: &Path, origin_run_id: &str) -> (Home, PathBuf, RecoveryClaim) {
        let home = Home::at(root.join("home"));
        home.ensure().unwrap();
        let project_root = seed_indexed_project(root, "orgasmic");
        let (spec, _) = sample_spec(
            &home,
            &project_root,
            origin_run_id,
            "req-healthy",
            "boot-healthy",
            false,
        );
        write_origin_session(&spec, "rt-healthy-origin", "boot-dead");
        let plan = plan_pending_recovery_claim(&home, &spec).unwrap();
        let committed = commit_recovery_claim(
            &home,
            "orgasmic",
            origin_run_id,
            CommitRecoveryDetails {
                runtime_id: plan.claim.replacement_runtime_id.clone(),
                boot_id: "boot-healthy".into(),
                action: "start_recovery_run".into(),
                target: "worker".into(),
                draft_prompt: Some("stable draft".into()),
            },
        )
        .unwrap();
        write_committed_replacement(&committed);
        (home, project_root, committed)
    }

    /// Make `path` unreadable and hand back a guard that restores it, so a
    /// failed assertion cannot leave a 000-mode file inside a `TempDir` that
    /// then fails to clean up.
    struct OwnedRestore(PathBuf);

    impl OwnedRestore {
        fn deny(path: PathBuf) -> Self {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o000)).unwrap();
            Self(path)
        }
    }

    impl Drop for OwnedRestore {
        fn drop(&mut self) {
            use std::os::unix::fs::PermissionsExt;
            let mode = if self.0.is_dir() { 0o700 } else { 0o600 };
            let _ = std::fs::set_permissions(&self.0, std::fs::Permissions::from_mode(mode));
        }
    }

    /// orgasmic:TASK-2QK4P.1.1.1 F1(a) — THE ROUND-FOUR BLOCK SHIP, RUNNING.
    ///
    /// The review derived this from the production error-collapse path and said
    /// plainly that it built no running reproduction. This is it.
    ///
    /// Two daemon-HMAC-authenticated replacements exist for one origin — the
    /// state `duplicate_authenticated_replacements_fail_closed` rules a safety
    /// violation. Both files are readable, both scans succeed, and the
    /// enumeration completes. What fails is ONE `authority_key` read, somewhere
    /// inside the pass. `claim_has_valid_authority` collapsed that to `false`,
    /// the indexer `continue`d and DROPPED that link, and
    /// `enumerate_recovery_origin_links` still returned
    /// `AuthoritativeOriginLinks::Complete`. The resolver then saw exactly one
    /// link, equal to the claim it had loaded, and returned `Valid` — the exact
    /// state rounds one through three exist to prevent, reached through the
    /// authenticator instead of through the index.
    ///
    /// The fault is swept across read positions because `read_dir` order is not
    /// ours to choose: whichever position hides the OTHER replacement is the
    /// dangerous one, and the claim here is about all of them.
    ///
    /// Injection: return `false` from `claim_has_valid_authority` on an
    /// unreadable key. Some sweep position then returns `Valid(`.
    // orgasmic:TASK-2QK4P.1.1.1
    #[test]
    fn an_authority_key_read_failure_cannot_hide_a_second_authenticated_replacement() {
        for nth in 1..=6u32 {
            let tmp = tempfile::tempdir().unwrap();
            let root = tmp.path().canonicalize().unwrap();
            let seeded = seed_two_authenticated_replacements(&root, "run-keyfault-origin");

            arm_authority_key_fault(&seeded.home, nth);
            let resolved = resolve_authoritative_recovery_claim(
                &seeded.home,
                &seeded.project_root,
                "orgasmic",
                "run-keyfault-origin",
                &mut ProjectOriginAuthority::default(),
            );
            let fired = authority_key_fault_fired(&seeded.home);
            disarm_authority_key_fault(&seeded.home);

            let resolved = resolved.unwrap();
            assert!(
                !matches!(resolved, ResolvedRecoveryClaim::Valid(_)),
                "a failed authority-key read at position {nth} must never confirm uniqueness — \
                 there are TWO authenticated replacements and one of them was merely not \
                 authenticated, got {resolved:?}"
            );
            if fired {
                // The failure happened during the enumeration or the load, so
                // nothing about this origin is decidable and the claim on disk
                // must survive untouched.
                assert!(
                    matches!(
                        resolved,
                        ResolvedRecoveryClaim::Unobserved(UnobservedEvidence {
                            reason: UnobservedSession::AuthorityKeyUnreadable,
                            ..
                        })
                    ) || matches!(resolved, ResolvedRecoveryClaim::InvalidQuarantined),
                    "position {nth} fired and must answer unobserved (or fail closed on the \
                     duplicate it did observe), got {resolved:?}"
                );
            }

            // And with the key readable the same origin fails closed on the
            // duplicate, which is what the dropped link was hiding.
            let resolved = resolve_authoritative_recovery_claim(
                &seeded.home,
                &seeded.project_root,
                "orgasmic",
                "run-keyfault-origin",
                &mut ProjectOriginAuthority::default(),
            )
            .unwrap();
            assert!(
                matches!(resolved, ResolvedRecoveryClaim::InvalidQuarantined),
                "after repair the duplicate must be visible again, got {resolved:?}"
            );
        }
    }

    /// orgasmic:TASK-2QK4P.1.1.1 acceptance 7 — an nth authority-key read
    /// failure against a HEALTHY claim: no `Valid`, no quarantine, then normal
    /// resolution after repair.
    ///
    /// This is F1(a)'s sibling in the opposite direction. `load_recovery_claim`
    /// recomputes the tag to accept the claim file and mapped an unreadable key
    /// to `RecoveryClaimError::CorruptClaim` — which is the error the resolver
    /// QUARANTINES on. One transient failure therefore renamed a live rescue's
    /// claim, and `post_run_recover` then found no plan and minted a competitor.
    ///
    /// Injection: map the key-read failure back to `false`/`CorruptClaim`. Some
    /// sweep position then quarantines a claim nothing was wrong with.
    // orgasmic:TASK-2QK4P.1.1.1
    #[test]
    fn an_nth_authority_key_read_failure_never_quarantines_a_healthy_claim() {
        let mut fired_at_least_once = false;
        for nth in 1..=6u32 {
            let tmp = tempfile::tempdir().unwrap();
            let root = tmp.path().canonicalize().unwrap();
            let (home, project_root, committed) =
                seed_one_healthy_claim(&root, "run-keyfault-healthy");

            arm_authority_key_fault(&home, nth);
            let resolved = resolve_authoritative_recovery_claim(
                &home,
                &project_root,
                "orgasmic",
                "run-keyfault-healthy",
                &mut ProjectOriginAuthority::default(),
            );
            let fired = authority_key_fault_fired(&home);
            disarm_authority_key_fault(&home);
            let resolved = resolved.unwrap();

            if fired {
                fired_at_least_once = true;
                assert!(
                    matches!(
                        resolved,
                        ResolvedRecoveryClaim::Unobserved(UnobservedEvidence {
                            reason: UnobservedSession::AuthorityKeyUnreadable,
                            ..
                        })
                    ),
                    "a key read that failed at position {nth} decided nothing, got {resolved:?}"
                );
            } else {
                assert!(
                    matches!(resolved, ResolvedRecoveryClaim::Valid(_)),
                    "position {nth} never fired, so the answer must be the healthy one, got \
                     {resolved:?}"
                );
            }
            // No quarantine and no rewrite: the claim file is byte-identical.
            assert!(!quarantine_exists(
                &home,
                "orgasmic",
                "run-keyfault-healthy"
            ));
            assert_eq!(
                load_recovery_claim(&home, "orgasmic", "run-keyfault-healthy").unwrap(),
                Some(committed.clone()),
                "position {nth} must leave the claim exactly as it found it"
            );

            // Repair: the very same call resolves normally.
            let resolved = resolve_authoritative_recovery_claim(
                &home,
                &project_root,
                "orgasmic",
                "run-keyfault-healthy",
                &mut ProjectOriginAuthority::default(),
            )
            .unwrap();
            match resolved {
                ResolvedRecoveryClaim::Valid(valid) => assert_eq!(valid, committed),
                other => panic!("after repair the rescue must resolve valid, got {other:?}"),
            }
        }
        assert!(
            fired_at_least_once,
            "the sweep proved nothing if the injected fault never fired"
        );
    }

    /// orgasmic:TASK-2QK4P.1.1.1 F1(b) — THE OTHER ROUND-FOUR BLOCK SHIP,
    /// RUNNING, and the one that fails in the opposite direction.
    ///
    /// The enumeration COMPLETES and is cached — `ProjectOriginAuthority`
    /// already holds `Complete` for this project — so nothing round three built
    /// is involved. Then the read that `verify_committed_claim_against_session`
    /// makes fails: the replacement session in one pass, the origin session in
    /// the other. That function returned a bare `bool`, so the resolver read a
    /// transient `EACCES` as INVALID EVIDENCE: it quarantined a claim it had
    /// just matched against a complete link set, `reconstruct_or_quarantine`
    /// hit the same failure again and answered `InvalidQuarantined`, and
    /// `Missing` is one step further along the same path — after which
    /// `POST /runs/:id/recover` plans a NEW replacement beside the live one.
    ///
    /// Injection: return `false` from the two session reads. Both phases then
    /// quarantine.
    // orgasmic:TASK-2QK4P.1.1.1
    #[test]
    fn a_read_failure_after_a_cached_complete_enumeration_never_quarantines() {
        for target in ["replacement", "origin"] {
            let tmp = tempfile::tempdir().unwrap();
            let root = tmp.path().canonicalize().unwrap();
            let (home, project_root, committed) =
                seed_one_healthy_claim(&root, "run-verify-fault-origin");

            // PREMISE: one COMPLETE enumeration, cached before anything breaks.
            // Whatever happens below is therefore about the claim verification
            // and not about the candidate set.
            let mut authority = ProjectOriginAuthority::default();
            let links = authority.links_for(&home, &project_root, "orgasmic");
            assert!(
                matches!(links, AuthoritativeOriginLinks::Complete(found) if found.len() == 1),
                "the enumeration must complete and find exactly this claim's link: {links:?}"
            );
            let passes_after_caching = origin_enumeration_passes(&project_root);

            let victim = match target {
                "replacement" => committed.replacement_session_path.clone(),
                _ => committed.origin_session_path.clone().unwrap(),
            };
            let restore = OwnedRestore::deny(victim.clone());

            let resolved = resolve_authoritative_recovery_claim(
                &home,
                &project_root,
                "orgasmic",
                "run-verify-fault-origin",
                &mut authority,
            )
            .unwrap();
            assert!(
                matches!(resolved, ResolvedRecoveryClaim::Unobserved(_)),
                "an unreadable {target} session decides nothing about a claim whose candidate \
                 set was fully enumerated, got {resolved:?}"
            );
            assert_eq!(
                passes_after_caching,
                origin_enumeration_passes(&project_root),
                "the cached enumeration must be reused, not rebuilt"
            );

            // No quarantine, and — the part `Missing` would have destroyed —
            // the claim is still exactly where the live rescue left it.
            assert!(!quarantine_exists(
                &home,
                "orgasmic",
                "run-verify-fault-origin"
            ));
            assert_eq!(
                load_recovery_claim(&home, "orgasmic", "run-verify-fault-origin").unwrap(),
                Some(committed.clone())
            );
            // And no second replacement session was created beside the live one.
            let sessions = std::fs::read_dir(project_sessions_dir(&project_root))
                .unwrap()
                .flatten()
                .count();
            assert_eq!(
                sessions, 2,
                "origin plus one replacement, and nothing minted"
            );

            drop(restore);
            let resolved = resolve_authoritative_recovery_claim(
                &home,
                &project_root,
                "orgasmic",
                "run-verify-fault-origin",
                &mut ProjectOriginAuthority::default(),
            )
            .unwrap();
            match resolved {
                ResolvedRecoveryClaim::Valid(valid) => assert_eq!(valid, committed),
                other => panic!(
                    "after repair the {target} read succeeds and the rescue must resolve valid, \
                     got {other:?}"
                ),
            }
        }
    }

    /// orgasmic:TASK-2QK4P.1.1.1 acceptance 3 — THE BEHAVIOURAL PIN.
    ///
    /// A source-text pin cannot express the property this round is about. The
    /// property is "no predicate on the recovery-authority path answers a
    /// DECIDED negative when it could not look", and that is a statement about
    /// what every fallible call site concludes, not about how any declaration is
    /// spelled. `the_origin_index_result_cannot_spell_failure_as_an_empty_success`
    /// guards the shape of the two round-three types and was structurally blind
    /// to all of F1 — so this test guards the behaviour, over the whole class,
    /// by injecting each observation failure the path can actually suffer and
    /// demanding the same two answers of every one of them:
    ///
    ///   1. never `Valid` — an unproven claim is not a confirmed one;
    ///   2. never destructive — no quarantine, and the claim file survives
    ///      byte-identical, because `InvalidQuarantined` and `Missing` are both
    ///      permission for the handler to mint a competitor.
    ///
    /// The fixture is deliberately HEALTHY: every one of these resolves `Valid`
    /// with nothing injected, so any answer other than `Unobserved` here is the
    /// injection being read as a fact about the claim.
    // orgasmic:TASK-2QK4P.1.1.1
    #[test]
    fn no_observation_failure_on_the_authority_path_confirms_or_destroys() {
        // (name, how to break observation, the reason it must be reported as)
        type Break = fn(&Home, &Path, &RecoveryClaim) -> Box<dyn std::any::Any>;
        let cases: Vec<(&str, Break)> = vec![
            ("authority key unreadable", |home, _root, _claim| {
                struct Disarm(Home);
                impl Drop for Disarm {
                    fn drop(&mut self) {
                        disarm_authority_key_fault(&self.0);
                    }
                }
                // Position 1 is inside the enumeration for this fixture, which
                // is the site F1(a) is about.
                arm_authority_key_fault(home, 1);
                Box::new(Disarm(home.clone()))
            }),
            ("replacement session unreadable", |_home, _root, claim| {
                let path = claim.replacement_session_path.clone();
                Box::new(OwnedRestore::deny(path))
            }),
            ("origin session unreadable", |_home, _root, claim| {
                let path = claim.origin_session_path.clone().unwrap();
                Box::new(OwnedRestore::deny(path))
            }),
            ("sessions directory unreadable", |_home, root, _claim| {
                Box::new(OwnedRestore::deny(project_sessions_dir(root)))
            }),
            (
                "a sibling session is torn mid-file",
                |_home, root, _claim| {
                    let sibling = project_sessions_dir(root).join("run-unrelated-torn.jsonl");
                    std::fs::write(&sibling, "").unwrap();
                    tear_a_line_in_the_middle(&sibling);
                    struct Remove(PathBuf);
                    impl Drop for Remove {
                        fn drop(&mut self) {
                            let _ = std::fs::remove_file(&self.0);
                        }
                    }
                    Box::new(Remove(sibling))
                },
            ),
        ];

        for (name, break_observation) in cases {
            let tmp = tempfile::tempdir().unwrap();
            let root = tmp.path().canonicalize().unwrap();
            let (home, project_root, committed) = seed_one_healthy_claim(&root, "run-class-pin");

            // The control: with nothing injected this resolves `Valid`, so the
            // difference below is the injection and nothing else.
            let control = resolve_authoritative_recovery_claim(
                &home,
                &project_root,
                "orgasmic",
                "run-class-pin",
                &mut ProjectOriginAuthority::default(),
            )
            .unwrap();
            assert!(
                matches!(control, ResolvedRecoveryClaim::Valid(_)),
                "[{name}] the fixture must be healthy before the injection, got {control:?}"
            );

            let restore = break_observation(&home, &project_root, &committed);
            let resolved = resolve_authoritative_recovery_claim(
                &home,
                &project_root,
                "orgasmic",
                "run-class-pin",
                &mut ProjectOriginAuthority::default(),
            );
            let quarantined = quarantine_exists(&home, "orgasmic", "run-class-pin");
            drop(restore);

            let resolved = resolved.unwrap_or_else(|err| {
                panic!("[{name}] an observation failure is not an error: {err:?}")
            });
            assert!(
                matches!(resolved, ResolvedRecoveryClaim::Unobserved(_)),
                "[{name}] a predicate that could not look must answer unobserved, got {resolved:?}"
            );
            assert!(
                !quarantined,
                "[{name}] unobserved never quarantines: renaming the claim turns one failed \
                 observation into the permanent loss of this rescue's idempotency"
            );
            assert_eq!(
                load_recovery_claim(&home, "orgasmic", "run-class-pin").unwrap(),
                Some(committed.clone()),
                "[{name}] the claim on disk must survive the failed observation"
            );

            // Repair, and the rescue is exactly where it was.
            let repaired = resolve_authoritative_recovery_claim(
                &home,
                &project_root,
                "orgasmic",
                "run-class-pin",
                &mut ProjectOriginAuthority::default(),
            )
            .unwrap();
            match repaired {
                ResolvedRecoveryClaim::Valid(valid) => assert_eq!(valid, committed),
                other => panic!("[{name}] repair must restore normal resolution, got {other:?}"),
            }
        }
    }

    /// orgasmic:TASK-2QK4P.1.1 acceptance 1, RE-AUTHORED FOR TASK-2QK4P.1.1.1
    /// acceptance 3 — THE STRUCTURAL PIN, AND WHAT IT CANNOT SAY.
    ///
    /// # This pin passed on the defect, which is worse than not having it
    ///
    /// The round-three version asserted `code.contains("Unobserved {\n
    /// reason: UnobservedSession,")` — a PREFIX. Adding a `links` field after
    /// the checked prefix kept it green, so the one thing the pin existed to
    /// forbid was expressible without tripping it. It also inspected exactly two
    /// declarations, which left it structurally blind to every predicate
    /// TASK-2QK4P.1.1.1 is about. A green source-text assertion that guarantees
    /// nothing reads as coverage, and reviewers spend rounds trusting it.
    ///
    /// So: the variant payload is extracted WHOLE and compared for EQUALITY
    /// against the exact field set each type is allowed, and the set of pinned
    /// declarations is now every type on the path that carries the unresolved
    /// answer.
    ///
    /// # What a source-text pin cannot express, stated plainly
    ///
    /// The property this round is about is not a shape. It is "no predicate on
    /// the recovery-authority path answers a DECIDED negative when it could not
    /// look", and that is a claim about what each of ~20 fallible call sites
    /// concludes — a `continue`, a `quarantine_invalid_claim`, a `Missing`, a
    /// `false` folded into an `&&`. No amount of grepping this file settles it,
    /// and a pin that pretended otherwise would be the round-three pin again.
    ///
    /// `no_observation_failure_on_the_authority_path_confirms_or_destroys` is
    /// where that property is actually pinned, by injecting each observation
    /// failure the path can suffer and demanding the same answer from all of
    /// them. This test guards the SHAPE those behaviours rest on; that one
    /// guards the behaviour. Neither is a substitute for the other.
    // orgasmic:TASK-2QK4P.1.1, TASK-2QK4P.1.1.1
    #[test]
    fn the_recovery_authority_types_cannot_spell_failure_as_a_decided_negative() {
        let source = include_str!("recovery_claim.rs");
        let production = source
            .split("\nmod tests {")
            .next()
            .expect("this file has a tests module");
        assert!(
            production.len() < source.len(),
            "the split must actually remove the tests module"
        );
        // Comment lines are dropped, so the forbidden spellings this test names
        // — including the ones the types' own doc comments quote as history —
        // are matched only where they would actually compile.
        let code: String = production
            .lines()
            .filter(|line| !line.trim_start().starts_with("//"))
            .map(|line| format!("{line}\n"))
            .collect();

        // EVERY type on the authority path that carries the unresolved answer,
        // with the EXACT payload its unresolved variant is allowed to have.
        // Equality, not containment: an added field is a red test.
        //
        // orgasmic:TASK-2QK4P.1.1.1.1 F3 — the payload is now
        // [`UnobservedEvidence`] rather than a bare tag, and the equality above
        // is what makes that a DELIBERATE widening instead of a drifted one:
        // the pin had to be edited for it. What the variant may still not carry
        // is a partial RESULT — links, a claim, a boolean — and that is pinned
        // by `UnobservedEvidence`'s own field set immediately below.
        let pinned: [(&str, &str); 4] = [
            (
                "IndexedRecoveryOrigins",
                "Unobserved { reason: UnobservedEvidence, bytes_inspected: u64, }",
            ),
            (
                "AuthoritativeOriginLinks",
                "Unobserved(UnobservedEvidence),",
            ),
            ("ClaimEvidence", "Unobserved(UnobservedEvidence),"),
            ("ResolvedRecoveryClaim", "Unobserved(UnobservedEvidence),"),
        ];

        for (name, expected_variant) in pinned {
            let header = format!("pub enum {name} ");
            let start = code.find(&header).unwrap_or_else(|| {
                panic!(
                    "{name} must stay an enum: a struct with a links field is what let an error \
                     path return an empty success"
                )
            });
            assert!(
                !code.contains(&format!("impl Default for {name}")),
                "{name} must not implement Default; `{name}` + `::default()` was round three's \
                 error path"
            );
            let default_call = format!("{name}{}", "::default()");
            assert!(
                !code.contains(&default_call),
                "{default_call} must not appear: it is the collapse itself"
            );
            let head = &code[..start];
            let attr_start = head
                .rfind("#[derive(")
                .expect("the enum carries a derive attribute");
            let attributes = &head[attr_start..];
            assert!(
                !attributes.contains("Default"),
                "{name} must not derive Default; its attributes are:\n{attributes}"
            );
            assert!(
                attributes.contains("#[must_use"),
                "{name} must be #[must_use] so dropping it is a build failure under -D warnings; \
                 its attributes are:\n{attributes}"
            );

            // The variant payload, WHOLE. Round three checked a prefix of this
            // and a `links` field appended after the prefix kept it green.
            let body = enum_body(&code, start);
            let variant = unobserved_variant(&body).unwrap_or_else(|| {
                panic!("{name} must carry an `Unobserved` variant; its body is:\n{body}")
            });
            assert_eq!(
                variant, expected_variant,
                "{name}::Unobserved must carry ONLY the reason it could not observe. Anything \
                 else is a partial result a caller can reach for, which is the collapse wearing \
                 a new field name."
            );
        }

        // orgasmic:TASK-2QK4P.1.1.1.1 F3 — and the carrier itself holds exactly
        // three things: WHY the observation failed, WHICH file it failed on, and
        // WHAT WOULD REPAIR IT. Not a link, not a claim, not a bool. A fourth
        // field is a red test, because "the unresolved answer carries no partial
        // result" is the invariant the whole chain rests on.
        {
            let start = code
                .find("pub struct UnobservedEvidence ")
                .expect("the evidence carrier must exist");
            let body = enum_body(&code, start);
            let fields: Vec<&str> = body
                .trim_matches(|c| c == '{' || c == '}')
                .split(',')
                .map(str::trim)
                .filter(|field| !field.is_empty())
                .collect();
            assert_eq!(
                fields,
                vec![
                    "pub reason: UnobservedSession",
                    "pub subject: Option<String>",
                    "pub remediation: Remediation"
                ],
                "UnobservedEvidence carries the failure's identity and nothing a caller could \
                 mistake for a result; its body is:\n{body}"
            );
        }

        // orgasmic:TASK-2QK4P.1.1.1.1 F4 — NO WILDCARD ARM ON THE RESOLVER ENUM.
        //
        // `committed_claim_is_authoritative` had `_ => NotAuthoritative`, which
        // swept every resolver `Err` — raw `Io` included — into a DECIDED
        // negative. This pin is here and not only in a behaviour test on
        // purpose, and the reason is worth stating: after F1, every unobserved
        // path upstream is converted to `Ok(ResolvedRecoveryClaim::Unobserved)`
        // before it reaches this helper, so restoring the wildcard TODAY does
        // not change any observable answer — the behaviour test stays green. It
        // is the next unobserved variant, added by a later round, that the
        // wildcard would silently swallow. So the guarantee is compile-time:
        // exhaustiveness, pinned by forbidding the one construct that defeats
        // it.
        {
            let api = include_str!("api.rs");
            let api_production = api
                .split("\nmod tests {")
                .next()
                .expect("api.rs has a tests module");
            let start = api_production
                .find("fn committed_claim_is_authoritative")
                .expect("the helper must still exist");
            // Comments are dropped first, exactly as above: this test's own
            // explanation of the defect quotes the forbidden spelling.
            let body: String = enum_body(api_production, start)
                .lines()
                .filter(|line| !line.trim_start().starts_with("//"))
                .map(|line| format!("{line}\n"))
                .collect();
            // `Err(_)` is the same sweep wearing a constructor: it catches
            // every current AND future `RecoveryClaimError`, which is exactly
            // how a later round's new unobserved variant would become a
            // decided negative without anyone editing this function.
            for forbidden in ["_ =>", "_ if", "Err(_)", "Ok(_)"] {
                assert!(
                    !body.contains(forbidden),
                    "committed_claim_is_authoritative must match every resolver result \
                     explicitly; `{forbidden}` is how `I could not decide` becomes `I decided \
                     no`. Its body is:\n{body}"
                );
            }
        }

        // And the two predicates the review named must not have gone back to
        // answering `bool`, which is what handed round three's types the same
        // defect they were built to make unrepresentable.
        for predicate in [
            "fn claim_has_valid_authority",
            "pub fn verify_committed_claim_against_session",
            "pub fn pending_recovery_claim_owns_session",
        ] {
            let start = code
                .find(predicate)
                .unwrap_or_else(|| panic!("{predicate} must still exist"));
            let signature =
                &code[start..code[start..].find(" {\n").map_or(code.len(), |o| start + o)];
            assert!(
                !signature.contains("-> bool"),
                "{predicate} must not answer `bool`: `false` there means both `I verified and it \
                 is false` and `I could not check`, and four review rounds were that one shape. \
                 Its signature is:\n{signature}"
            );
        }
    }

    /// The `{ .. }` body of the enum whose `pub enum` header starts at `start`.
    fn enum_body(code: &str, start: usize) -> String {
        let open = start + code[start..].find('{').expect("enum body opens");
        let mut depth = 0usize;
        for (offset, ch) in code[open..].char_indices() {
            match ch {
                '{' => depth += 1,
                '}' => {
                    depth -= 1;
                    if depth == 0 {
                        return code[open..=open + offset].to_string();
                    }
                }
                _ => {}
            }
        }
        panic!("enum body never closes");
    }

    /// The `Unobserved` variant of `body`, whitespace-normalized, from the
    /// variant name through its closing delimiter INCLUSIVE — so a field added
    /// anywhere inside it changes this string.
    fn unobserved_variant(body: &str) -> Option<String> {
        let start = body.find("Unobserved")?;
        let rest = &body[start..];
        let end = match rest[.."Unobserved".len() + 1].chars().last()? {
            '(' => {
                let close = rest.find(')')? + 1;
                // Include the trailing comma so a variant cannot be renamed
                // into a struct variant and still match.
                close + rest[close..].find(',').map_or(0, |o| o + 1)
            }
            _ => {
                let open = rest.find('{')?;
                let mut depth = 0usize;
                let mut close = open;
                for (offset, ch) in rest[open..].char_indices() {
                    match ch {
                        '{' => depth += 1,
                        '}' => {
                            depth -= 1;
                            if depth == 0 {
                                close = open + offset + 1;
                                break;
                            }
                        }
                        _ => {}
                    }
                }
                close
            }
        };
        Some(
            rest[..end]
                .split_whitespace()
                .collect::<Vec<_>>()
                .join(" ")
                .replace(" ,", ","),
        )
    }

    // ================================================================
    // orgasmic:TASK-2QK4P.1.1.1.1 — ROUND FIVE REGRESSIONS
    //
    // Every one of these is a REPRODUCTION FIRST: the fixture is built to make
    // the pre-fix code produce the wrong answer, and the assertion is the
    // answer the fixed code gives. Where a real errno cannot be produced from a
    // test the seam reproduces the syscall's exact observable shape, never the
    // decision that follows it.
    // ================================================================

    /// Tests that need a real `EACCES` cannot run as root, where permission
    /// bits do not apply.
    fn skip_if_root() -> bool {
        let root = unsafe { libc::geteuid() } == 0;
        if root {
            eprintln!("skipping: permission fixtures are meaningless as root");
        }
        root
    }

    struct RestorePermissions(PathBuf, u32);
    impl Drop for RestorePermissions {
        fn drop(&mut self) {
            use std::os::unix::fs::PermissionsExt as _;
            let _ = std::fs::set_permissions(&self.0, std::fs::Permissions::from_mode(self.1));
        }
    }

    fn deny_all(path: &Path) -> RestorePermissions {
        use std::os::unix::fs::PermissionsExt as _;
        let previous = std::fs::metadata(path).unwrap().permissions().mode() & 0o7777;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o000)).unwrap();
        RestorePermissions(path.to_path_buf(), previous)
    }

    /// F1 — AN UNREADABLE PENDING CLAIM FILE IS NOT A CORRUPT ONE.
    ///
    /// `ClaimDirectory::read_regular` mapped every open error other than
    /// `NotFound` to `CorruptClaim`, which is the bucket
    /// `resolve_authoritative_recovery_claim` QUARANTINES on. For a PENDING
    /// claim there is no committed `RecoveryOrigin` to reconstruct from, so the
    /// quarantine removed the claim outright: the next POST then saw a complete
    /// origin enumeration plus no claim and reached
    /// `plan_pending_recovery_claim` — minting a competitor beside a live
    /// rescue. One `EACCES` destroyed authority and issued permission to mint.
    ///
    /// The fixture is a real `chmod 000` on the claim file, not a hook: the
    /// pre-fix mapping is reached by the genuine syscall error.
    ///
    /// Injection to see it red: restore
    /// `_ => RecoveryClaimError::CorruptClaim` in `read_regular`.
    // orgasmic:TASK-2QK4P.1.1.1.1
    #[test]
    fn f1_an_unreadable_pending_claim_is_unobserved_and_never_quarantined() {
        if skip_if_root() {
            return;
        }
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().canonicalize().unwrap();
        let home = Home::at(root.join("home"));
        home.ensure().unwrap();
        let project_root = seed_indexed_project(&root, "orgasmic");
        let (spec, _) = sample_spec(
            &home,
            &project_root,
            "run-f1-origin",
            "req-f1",
            "boot-f1",
            false,
        );
        write_origin_session(&spec, "rt-f1-origin", "boot-dead");
        plan_pending_recovery_claim(&home, &spec).unwrap();

        let claim_file = claim_path(&home, "orgasmic", "run-f1-origin").unwrap();
        let before = std::fs::read(&claim_file).unwrap();
        assert!(!before.is_empty(), "the fixture must have a pending claim");

        let resolved = {
            let _restore = deny_all(&claim_file);
            resolve_authoritative_recovery_claim(
                &home,
                &project_root,
                "orgasmic",
                "run-f1-origin",
                &mut ProjectOriginAuthority::default(),
            )
            .expect("an unreadable claim is an answer, not an error to the caller")
        };

        // 1. No `Valid`, and no quarantine decision.
        match &resolved {
            ResolvedRecoveryClaim::Unobserved(evidence) => {
                assert_eq!(evidence.reason, UnobservedSession::ClaimFileUnreadable);
                assert_eq!(evidence.remediation, Remediation::RepairClaimStore);
                assert!(
                    evidence.subject.is_some(),
                    "F3: the refusal must name what it failed on"
                );
            }
            other => panic!("an EACCES on the claim file must be unobserved, got {other:?}"),
        }
        // 2. No quarantine on disk.
        assert!(
            !quarantine_exists(&home, "orgasmic", "run-f1-origin"),
            "an observation failure must never rename a live rescue's claim"
        );
        // 3. No new replacement minted: the claim file is byte-identical.
        assert_eq!(
            std::fs::read(&claim_file).unwrap(),
            before,
            "the claim file must survive the failed read untouched"
        );

        // 4. And after repair, reconciliation is normal.
        let repaired = resolve_authoritative_recovery_claim(
            &home,
            &project_root,
            "orgasmic",
            "run-f1-origin",
            &mut ProjectOriginAuthority::default(),
        )
        .unwrap();
        assert!(
            matches!(repaired, ResolvedRecoveryClaim::Valid(ref claim)
                if claim.status == RecoveryClaimStatus::Pending),
            "once readable the same claim resolves normally, got {repaired:?}"
        );
    }

    /// F1(b) — the same failure one level up: an unreadable claim STORE.
    ///
    /// `ClaimDirectory::open` mapped every non-`NotFound` component-open error
    /// to `CorruptClaim` too, and that one is worse: it is a statement about
    /// EVERY claim under the directory, made from a single failed `openat`.
    // orgasmic:TASK-2QK4P.1.1.1.1
    #[test]
    fn f1_an_unreadable_claim_store_is_unobserved_not_corrupt() {
        if skip_if_root() {
            return;
        }
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().canonicalize().unwrap();
        let home = Home::at(root.join("home"));
        home.ensure().unwrap();
        let project_root = seed_indexed_project(&root, "orgasmic");
        let (spec, _) = sample_spec(
            &home,
            &project_root,
            "run-f1b-origin",
            "req-f1b",
            "boot-f1b",
            false,
        );
        write_origin_session(&spec, "rt-f1b-origin", "boot-dead");
        plan_pending_recovery_claim(&home, &spec).unwrap();

        let project_dir = recovery_claims_root(&home).join("orgasmic");
        let loaded = {
            let _restore = deny_all(&project_dir);
            load_recovery_claim(&home, "orgasmic", "run-f1b-origin")
        };
        match loaded {
            Err(RecoveryClaimError::Unobserved(evidence)) => {
                assert_eq!(evidence.reason, UnobservedSession::ClaimStoreUnreadable);
                assert_eq!(evidence.remediation, Remediation::RepairClaimStore);
            }
            other => panic!("an EACCES opening the claim store must be unobserved: {other:?}"),
        }
        assert!(
            !quarantine_exists(&home, "orgasmic", "run-f1b-origin"),
            "nothing may be quarantined on a store the daemon could not open"
        );
        assert!(
            load_recovery_claim(&home, "orgasmic", "run-f1b-origin")
                .unwrap()
                .is_some(),
            "and after repair the claim is still there"
        );
    }

    /// F2 — A FAILED `readdir` IS NOT A COMPLETE DIRECTORY.
    ///
    /// This is the PREDICATE-level regression and that is all it is. It asserts
    /// what `pending_recovery_claim_owns_session` answers; it does not enter the
    /// boot route, and it would stay green under a boot that reattached on
    /// `ClaimEvidence::Unobserved`. The routing itself is pinned by
    /// `api::tests::boot_reattach_refuses_a_candidate_whose_claim_store_could_not_be_listed`
    /// (orgasmic:TASK-2QK4P.1.1.1.1.1 P1b), which arms this same seam and drives
    /// `reattach_live_runs_on_boot`. Keep both: this one localizes the fault to
    /// the predicate, that one proves what boot did with the answer.
    ///
    /// `names()` broke out of its loop on a NULL return and answered
    /// `Ok(names)`. `readdir` returns NULL at end-of-directory AND on error,
    /// and the only way to tell is to clear `errno` before the call and read it
    /// after — which it did neither of. `pending_recovery_claim_owns_session`
    /// trusts that vector as complete, so a listing that stopped early hid the
    /// pending claim owning a boot candidate, the predicate answered `Invalid`,
    /// and `boot_reattach_ownership`'s caller let generic reattach append a
    /// `Reattach` event into the immutable prefix that pending recovery owns.
    /// That write is not undoable.
    ///
    /// The fault reproduces the syscall's exact shape — an errno-bearing NULL
    /// on the nth iteration — not the decision that follows it, so the pre-fix
    /// loop reaches its own `break` and returns the short set.
    ///
    /// Injection to see it red: drop the `errno` clear/read and return
    /// `Ok(names)` on the NULL.
    // orgasmic:TASK-2QK4P.1.1.1.1
    #[test]
    fn f2_a_failed_readdir_is_not_a_complete_directory() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().canonicalize().unwrap();
        let home = Home::at(root.join("home"));
        home.ensure().unwrap();
        let project_root = seed_indexed_project(&root, "orgasmic");
        let (spec, _) = sample_spec(
            &home,
            &project_root,
            "run-f2-origin",
            "req-f2",
            "boot-f2",
            false,
        );
        write_origin_session(&spec, "rt-f2-origin", "boot-dead");
        let plan = plan_pending_recovery_claim(&home, &spec).unwrap();
        let owned_session = plan.claim.replacement_session_path.clone();
        std::fs::write(&owned_session, b"").unwrap();
        let session_before = std::fs::read(&owned_session).unwrap();

        // PREMISE: with the directory listing intact, the pending claim IS
        // found, so the test cannot pass by the predicate being blind.
        assert_eq!(
            pending_recovery_claim_owns_session(&home, &project_root, "orgasmic", &owned_session),
            ClaimEvidence::Valid,
            "the fixture must start out with the claim discoverable"
        );

        // Now fail the FIRST readdir. Pre-fix this returns an empty-but-Ok set,
        // the claim is invisible, and the predicate answers Invalid = "reattach
        // this session".
        arm_readdir_fault(&home, 1, libc::EIO);
        let ownership =
            pending_recovery_claim_owns_session(&home, &project_root, "orgasmic", &owned_session);
        disarm_readdir_fault(&home);
        match &ownership {
            ClaimEvidence::Unobserved(evidence) => {
                assert_eq!(evidence.reason, UnobservedSession::ClaimStoreUnreadable);
                assert_eq!(evidence.remediation, Remediation::RepairClaimStore);
                assert!(
                    evidence.subject.is_some(),
                    "the refusal must name a subject"
                );
            }
            other => panic!(
                "a mid-listing EIO must not be reported as a complete directory, got {other:?}"
            ),
        }
        // The boot loop reattaches ONLY on `Invalid`; this is the condition it
        // evaluates (api.rs `boot_reattach_ownership` / `reattach_live_runs_on_boot`).
        assert!(
            !matches!(ownership, ClaimEvidence::Invalid),
            "boot must skip, not reattach, when the claim store could not be listed"
        );
        assert_eq!(
            std::fs::read(&owned_session).unwrap(),
            session_before,
            "no session write may happen while the predicate is unobserved"
        );

        // After repair, routing is normal again.
        assert_eq!(
            pending_recovery_claim_owns_session(&home, &project_root, "orgasmic", &owned_session),
            ClaimEvidence::Valid,
            "a transient listing failure must not change the answer once it clears"
        );
    }

    /// F2 — `closedir` failure and a non-UTF-8 entry name.
    ///
    /// The third collapse in the same loop was `if let Ok(name) =
    /// name.to_str()`, which SILENTLY DROPPED an entry whose name is not UTF-8
    /// and made the set smaller — the unsafe direction. The decision taken is
    /// that an entry name is BYTES: it is carried through unchanged, and the
    /// `.json` filter its callers apply then decides about it. A lossy
    /// conversion was rejected because the mangled name opens as `NotFound`,
    /// which reads as "absent" — the same collapse one layer down.
    // orgasmic:TASK-2QK4P.1.1.1.1
    #[test]
    fn f2_a_non_utf8_entry_name_is_carried_not_dropped() {
        use std::os::unix::ffi::OsStrExt as _;
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().canonicalize().unwrap();
        let home = Home::at(root.join("home"));
        home.ensure().unwrap();
        ClaimDirectory::open(&home, "orgasmic", true).unwrap();
        let dir = recovery_claims_root(&home).join("orgasmic");
        let raw = std::ffi::OsStr::from_bytes(b"not-\xffutf8.json");
        let non_utf8 = std::fs::write(dir.join(raw), b"{}");
        std::fs::write(dir.join("plain.json"), b"{}").unwrap();

        // TYPE-LEVEL PIN, and it holds on every platform: the API is `OsString`,
        // so there is no `to_str()` left to drop an entry at. A future edit that
        // reintroduces `Vec<String>` is a compile error here.
        let names: Vec<std::ffi::OsString> = ClaimDirectory::open(&home, "orgasmic", false)
            .unwrap()
            .unwrap()
            .names()
            .unwrap();

        match non_utf8 {
            Ok(()) => {
                assert_eq!(
                    names.len(),
                    2,
                    "no entry may vanish from the set: {names:?}"
                );
                assert!(
                    names
                        .iter()
                        .any(|name| name.as_encoded_bytes() == b"not-\xffutf8.json"),
                    "the undecodable entry must be carried through as its bytes: {names:?}"
                );
                assert!(
                    !names.iter().any(|name| name
                        .as_encoded_bytes()
                        .windows(3)
                        .any(|w| w == [0xEF, 0xBF, 0xBD])),
                    "and it must NOT be lossily re-encoded (U+FFFD) into a name that \
                     opens as NotFound"
                );
            }
            // APFS and HFS+ ENFORCE UTF-8 file names and reject this one with
            // `EILSEQ`, so the byte-level half of this regression is unbuildable
            // on macOS. Reported rather than hidden: on macOS this test proves
            // the type-level pin above and nothing more, and the byte assertions
            // run on Linux (ext4/tmpfs impose no such rule), which is the other
            // shipped target.
            Err(err) => {
                assert_eq!(
                    err.raw_os_error(),
                    Some(libc::EILSEQ),
                    "an unexpected failure creating the fixture: {err:?}"
                );
                eprintln!(
                    "note: this filesystem rejects non-UTF-8 names (EILSEQ); the byte-preservation \
                     half of f2_a_non_utf8_entry_name_is_carried_not_dropped did not run here"
                );
                assert_eq!(names.len(), 1, "the entries that exist are all listed");
            }
        }
    }

    /// F2 — the non-UTF-8 entry name, WITH THE FILESYSTEM TAKEN OUT OF THE
    /// FIXTURE, so the byte assertions execute on the platform this project
    /// actually runs on.
    ///
    /// orgasmic:TASK-2QK4P.1.1.1.1.1 P2a — the test above is honest and it has
    /// no teeth here: APFS/HFS+ reject the fixture with `EILSEQ`, so on macOS
    /// all that survived was `names(): Vec<OsString>`, which a reintroduced
    /// `to_str()` drop satisfies just as well. This one delivers the bytes
    /// through a real `dirent`'s `d_name` on the nth `readdir` iteration and
    /// takes them back out with the same `CStr::from_ptr(..).to_bytes()` the
    /// syscall path uses. Everything after that — the `.`/`..` filter, the
    /// `OsStr::from_bytes` collect, and the caller's `.json` filter — is
    /// production code.
    ///
    /// Injection to see it red: put `if let Ok(name) = raw.to_str()` back around
    /// the push in `names()`'s `collect`, or make it lossy
    /// (`String::from_utf8_lossy`). The first drops the entry and the count
    /// assertion fails; the second re-encodes it to U+FFFD and the byte
    /// assertion fails. Neither depends on the filesystem.
    // orgasmic:TASK-2QK4P.1.1.1.1
    #[test]
    fn f2_an_injected_non_utf8_entry_name_survives_the_collect_on_every_platform() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().canonicalize().unwrap();
        let home = Home::at(root.join("home"));
        home.ensure().unwrap();
        ClaimDirectory::open(&home, "orgasmic", true).unwrap();
        let dir = recovery_claims_root(&home).join("orgasmic");
        std::fs::write(dir.join("plain.json"), b"{}").unwrap();

        // The first iteration yields the undecodable name; the real stream is
        // not advanced, so `plain.json` is still listed after it.
        arm_readdir_entry_name(&home, 1, b"not-\xffutf8.json");
        let names: Vec<std::ffi::OsString> = ClaimDirectory::open(&home, "orgasmic", false)
            .unwrap()
            .unwrap()
            .names()
            .unwrap();

        assert_eq!(
            names.len(),
            2,
            "no entry may vanish from the set — this is the count a `to_str()` \
             drop makes wrong: {names:?}"
        );
        assert!(
            names
                .iter()
                .any(|name| name.as_encoded_bytes() == b"not-\xffutf8.json"),
            "the undecodable entry must be carried through as its exact bytes: {names:?}"
        );
        assert!(
            !names.iter().any(|name| name
                .as_encoded_bytes()
                .windows(3)
                .any(|w| w == [0xEF, 0xBF, 0xBD])),
            "and it must NOT be lossily re-encoded (U+FFFD) into a name that \
             opens as NotFound: {names:?}"
        );
        assert!(
            names
                .iter()
                .any(|name| name.as_encoded_bytes() == b"plain.json"),
            "the real entries after the injected one must still be listed: {names:?}"
        );
    }

    /// F2 — and the entry that cannot be decoded does not FREEZE the decision
    /// its caller makes.
    ///
    /// orgasmic:TASK-2QK4P.1.1.1.1.1 P2a — the second half of "carried, not
    /// dropped": `pending_recovery_claim_owns_session` applies the `.json`
    /// filter to those bytes and then fails to open that name, and the DECIDED
    /// answer for the rest of the directory must still come back. A refusal
    /// here would be the F3 shape (project-wide freeze) triggered by an
    /// unrelated file someone dropped in the claim store.
    // orgasmic:TASK-2QK4P.1.1.1.1
    #[test]
    fn f2_an_undecodable_entry_does_not_freeze_the_ownership_answer() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().canonicalize().unwrap();
        let home = Home::at(root.join("home"));
        home.ensure().unwrap();
        let project_root = seed_indexed_project(&root, "orgasmic");
        let (spec, _) = sample_spec(
            &home,
            &project_root,
            "run-p2a-origin",
            "req-p2a",
            "boot-p2a",
            false,
        );
        write_origin_session(&spec, "rt-p2a-origin", "boot-dead");
        let plan = plan_pending_recovery_claim(&home, &spec).unwrap();
        let owned_session = plan.claim.replacement_session_path.clone();
        std::fs::write(&owned_session, b"").unwrap();

        arm_readdir_entry_name(&home, 1, b"not-\xffutf8.json");
        assert_eq!(
            pending_recovery_claim_owns_session(&home, &project_root, "orgasmic", &owned_session),
            ClaimEvidence::Valid,
            "an entry whose name is not UTF-8 is a decided non-claim, not an \
             observation failure"
        );
    }

    /// F4 — read-side claim IO carries an observation reason.
    ///
    /// State-root canonicalize/open failures stayed `RecoveryClaimError::Io`,
    /// and `recovery_claim_load_error` sent raw `Io` to a 500. A 500 says "the
    /// daemon is broken"; the truth is "one file could not be read and the
    /// answer will be decided once it can", which is a retryable 503.
    // orgasmic:TASK-2QK4P.1.1.1.1
    #[test]
    fn f4_state_root_failures_carry_an_observation_reason() {
        if skip_if_root() {
            return;
        }
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().canonicalize().unwrap();
        let home = Home::at(root.join("home"));
        home.ensure().unwrap();
        ClaimDirectory::open(&home, "orgasmic", true).unwrap();

        let loaded = {
            let _restore = deny_all(&home.state());
            load_recovery_claim(&home, "orgasmic", "run-f4")
        };
        match loaded {
            Err(RecoveryClaimError::Unobserved(evidence)) => {
                assert_eq!(evidence.reason, UnobservedSession::ClaimStoreUnreadable);
                assert_eq!(evidence.remediation, Remediation::RepairClaimStore);
            }
            other => panic!("an unreadable state root must be unobserved, not Io: {other:?}"),
        }
    }

    /// The errno policy itself, pinned in one place because it now LIVES in one
    /// place (acceptance 2). `NotFound` is absence, `ELOOP`/`ENOTDIR` are
    /// decided path facts, and everything else — named or not — is the observer
    /// failing.
    // orgasmic:TASK-2QK4P.1.1.1.1
    #[test]
    fn the_errno_policy_is_stated_once_and_defaults_to_unobserved() {
        let class = |code: i32| classify_observation(&std::io::Error::from_raw_os_error(code));
        assert_eq!(class(libc::ENOENT), ObservationClass::Absent);
        for decided in [libc::ELOOP, libc::ENOTDIR, libc::EISDIR, libc::ENAMETOOLONG] {
            assert_eq!(
                class(decided),
                ObservationClass::Decided,
                "errno {decided} describes the path, so it is evidence"
            );
        }
        for unobserved in [
            libc::EACCES,
            libc::EPERM,
            libc::EIO,
            libc::EMFILE,
            libc::ENFILE,
            libc::ENOMEM,
            libc::EINTR,
            libc::EAGAIN,
            libc::EBUSY,
            libc::EOVERFLOW,
            // Deliberately unnamed by the policy: the default must be the safe
            // direction, whose worst case is a retry.
            libc::EDOM,
        ] {
            assert_eq!(
                class(unobserved),
                ObservationClass::Unobserved,
                "errno {unobserved} is the observer failing, not a fact about the file"
            );
        }
    }

    /// F5 — THE STAT OBSERVER IS INJECTED SEPARATELY FROM THE KEY READS, AND
    /// THE PROOF IS THAT THIS TEST CAN FAIL.
    ///
    /// Every `authority_key` test injected through `authority_key_fault`, which
    /// returns BEFORE control reaches the existence probe — including the
    /// five-case behavioural pin. So swapping `try_exists` back for `exists()`
    /// plus `load_or_generate` left the whole suite green and brought the
    /// host-token remint defect straight back.
    ///
    /// This uses no hook at all. Removing search permission from the token's
    /// parent directory makes the real `stat` fail with `EACCES` while the
    /// token bytes survive untouched. Under `exists()` that same fixture
    /// answers `false` and MINTS a new token, invalidating every `authority_tag`
    /// on the host — so the assertions below are exactly the ones that red.
    ///
    /// Injection to see it red (verified for this round):
    /// `if !home.auth_token().exists() { ... }` in place of
    /// `if !token_is_present(home)? { ... }`.
    // orgasmic:TASK-2QK4P.1.1.1.1
    #[test]
    fn f5_authority_key_stat_is_not_shadowed_by_the_read_fault_hook() {
        if skip_if_root() {
            return;
        }
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().canonicalize().unwrap();
        let home = Home::at(root.join("home"));
        home.ensure().unwrap();
        // A real token, whose bytes are the thing a remint would destroy.
        crate::auth::load_or_generate(&home).unwrap();
        let token_before = std::fs::read(home.auth_token()).unwrap();
        assert!(!token_before.is_empty());

        let auth_dir = home.auth_token().parent().unwrap().to_path_buf();
        let minted_before = load_or_generate_reached_count(&home);
        let observed = {
            // No search permission on the parent: `stat` on the token fails,
            // the token itself is untouched.
            let _restore = deny_all(&auth_dir);
            assert!(
                home.auth_token().try_exists().is_err(),
                "the fixture must make the real stat fail, not merely be absent"
            );
            authority_key(&home)
        };
        match observed {
            Err(RecoveryClaimError::Unobserved(evidence)) => {
                assert_eq!(evidence.reason, UnobservedSession::AuthorityKeyUnreadable);
                assert_eq!(evidence.remediation, Remediation::RepairAuthKey);
                assert_eq!(evidence.subject.as_deref(), Some("auth/token"));
            }
            other => panic!("an unstattable token must be unobserved, got {other:?}"),
        }
        assert_eq!(
            load_or_generate_reached_count(&home),
            minted_before,
            "THE ACCEPTANCE: load_or_generate must NOT be reached — a mint here \
             invalidates every authority_tag on the host"
        );
        assert_eq!(
            std::fs::read(home.auth_token()).unwrap(),
            token_before,
            "and the token bytes are unchanged"
        );
        // And the retry succeeds once the directory is readable again.
        assert_eq!(
            authority_key(&home).unwrap(),
            token_before
                .iter()
                .copied()
                .take_while(|byte| !byte.is_ascii_whitespace())
                .collect::<Vec<_>>(),
            "after repair the SAME key is read back"
        );
    }

    /// F3 — the subject is sanitized before it becomes operator-facing text.
    // orgasmic:TASK-2QK4P.1.1.1.1
    #[test]
    fn f3_a_subject_is_project_relative_and_sanitized() {
        let project = Path::new("/hosts/alice/code/proj");
        assert_eq!(
            sanitized_subject(project, &project.join(".orgasmic/sessions/run-1.jsonl")),
            ".orgasmic/sessions/run-1.jsonl",
            "the host prefix above the project root is dropped"
        );
        let hostile = sanitized_subject(project, Path::new("/etc/pa ss\nwd"));
        assert_eq!(
            hostile, "pa_ss_wd",
            "a path outside the project degrades to its sanitized file name"
        );
        assert!(
            !hostile.contains('/') && !hostile.contains('\n'),
            "no path separators or control bytes may reach a log line or an API body"
        );
        assert_eq!(
            sanitized_subject(project, Path::new("/")),
            "unnamed",
            "which file must never render blank"
        );
    }
}
