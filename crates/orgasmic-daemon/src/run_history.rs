// orgasmic:TASK-FZB6T.1
//! The run-history maintenance transaction: plan, confirm, apply, roll back.
//!
//! # Why this exists
//!
//! TASK-FZB6T shipped the accounting half — `run history inspect` says exactly
//! how many bytes of legacy rendered-TUI payload a board is carrying, per
//! driver and per event class — and then stopped. `run history compact` refused
//! to run without `--dry-run`, and `--dry-run` was the only thing it could do.
//! The reviewer's finding 1 is that a dry run with no confirmable run behind it
//! is not a maintenance capability; it is a number.
//!
//! # The shape of the transaction
//!
//! 1. **Plan.** [`plan_compaction`] reads every candidate session file and
//!    produces a [`CompactionPlan`]: which records are reclaimable, how many
//!    bytes, and the file identity each decision was made against. The plan
//!    carries a [`CompactionPlan::manifest_id`] that is a pure function of its
//!    own content, so the same board state always produces the same id and any
//!    change to the board produces a different one.
//! 2. **Confirm.** [`apply_compaction`] re-plans from scratch and refuses
//!    unless the operator's token equals the id of the plan it just computed.
//!    There is no server-side pending state to go stale: the token *is* the
//!    proof that the operator saw this exact plan.
//! 3. **Apply.** Per file, and only after the file identity is re-verified:
//!    the original is archived whole, the compacted content is written to a
//!    sibling temporary file, and the temporary file is `rename`d over the
//!    original. The rename is the only mutation of the live path and it is
//!    atomic, so a kill at any instant leaves either the original file or the
//!    complete compacted one — never a torn one.
//! 4. **Roll back.** [`rollback_compaction`] restores every archived original
//!    over its recorded path, by the same archive-then-rename discipline.
//!
//! # What is eligible
//!
//! Only **terminal** runs, and only records
//! [`crate::run_catalog::class_is_reclaimable`] proves are rendered pane
//! payload. A live run's session file is held open by the session writer in
//! append mode; renaming a new file over that path would leave the writer
//! appending to an orphaned inode and silently lose every subsequent lifecycle
//! line. That is not a risk worth a few megabytes, so a run that has not ended
//! is never a candidate.

use std::collections::BTreeMap;
use std::io::Write;
use std::path::{Path, PathBuf};

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::run_catalog::{
    class_is_reclaimable, project_sessions_dir, read_history_records, RunCatalogEntry,
    SessionFileFingerprint,
};

/// Where archived originals and manifests live, relative to a project root.
pub const ARCHIVE_REL_PATH: &str = ".orgasmic/tmp/run-history-archive";

/// Suffix of the sibling file a rewrite is staged in before its rename.
const STAGING_SUFFIX: &str = ".orgasmic-compact-tmp";

/// One session file's share of a compaction plan.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CompactionPlanFile {
    pub session_path: PathBuf,
    pub run_id: String,
    pub driver: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub transport: Option<String>,
    /// File identity this plan was decided against. Re-verified before the
    /// rewrite; a mismatch skips the file rather than rewriting bytes the plan
    /// never saw.
    pub fingerprint: SessionFileFingerprint,
    pub total_bytes: u64,
    pub reclaimable_records: u64,
    pub reclaimable_bytes: u64,
    /// SHA-256 over the reclaimable records' raw bytes, in file order. Recorded
    /// so the summary line the rewrite leaves behind names exactly what was
    /// removed.
    pub reclaimable_sha256: String,
}

/// A confirmable maintenance plan.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CompactionPlan {
    /// Stable content digest of this plan. Equal for equal board states,
    /// different for any change to any candidate file. This is the token
    /// [`apply_compaction`] requires.
    pub manifest_id: String,
    pub project_root: PathBuf,
    pub planned_at: DateTime<Utc>,
    pub files: Vec<CompactionPlanFile>,
    pub reclaimable_bytes: u64,
    pub reclaimable_records: u64,
    /// Catalog records considered, including the ones with nothing to reclaim.
    pub candidates_considered: u64,
    /// Records excluded because the run has not ended.
    pub skipped_not_terminal: u64,
    /// Records excluded because their session file could not be read.
    pub skipped_unreadable: u64,
}

impl CompactionPlan {
    pub fn is_empty(&self) -> bool {
        self.files.is_empty()
    }
}

/// What one file's rewrite did.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "outcome", rename_all = "snake_case")]
pub enum CompactionFileOutcome {
    /// Rewritten. `archived` is the copy of the original.
    Compacted {
        reclaimed_records: u64,
        reclaimed_bytes: u64,
        archived: PathBuf,
        bytes_before: u64,
        bytes_after: u64,
    },
    /// The file changed between planning and applying. Left untouched.
    SkippedChanged { reason: String },
    /// The rewrite failed. The original is still in place: nothing is renamed
    /// over the live path until the replacement is complete on disk.
    Failed { error: String },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CompactionFileResult {
    pub session_path: PathBuf,
    pub run_id: String,
    #[serde(flatten)]
    pub outcome: CompactionFileOutcome,
}

/// The durable record of one applied transaction, written under the archive
/// directory before any file is touched.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CompactionManifest {
    pub manifest_id: String,
    pub project_root: PathBuf,
    pub started_at: DateTime<Utc>,
    pub plan: CompactionPlan,
    /// Filled in as the transaction progresses; a manifest whose `results` is
    /// shorter than `plan.files` is the trace of a transaction that was killed
    /// partway, and every file it does not name is still in its original state.
    #[serde(default)]
    pub results: Vec<CompactionFileResult>,
}

/// The answer `run history compact` returns in either mode.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CompactionReport {
    /// `true` when nothing was written, moved, or deleted.
    pub dry_run: bool,
    pub plan: CompactionPlan,
    /// The token an operator must pass back to execute this exact plan.
    pub confirm_token: String,
    #[serde(default)]
    pub results: Vec<CompactionFileResult>,
    pub reclaimed_bytes: u64,
    pub reclaimed_records: u64,
    /// Where the originals were archived, when the transaction ran.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub archive_dir: Option<PathBuf>,
    pub note: &'static str,
}

#[derive(Debug, thiserror::Error)]
pub enum CompactionError {
    #[error(
        "compaction requires --confirm <manifest-id>; the current plan is {expected}. \
         Re-run with --dry-run to read it, then pass that id back to execute it."
    )]
    ConfirmationRequired { expected: String },
    #[error(
        "the board changed since that plan was made: confirmation names {supplied} but the \
         current plan is {expected}. Re-run --dry-run and confirm the new plan."
    )]
    ConfirmationStale { supplied: String, expected: String },
    #[error("no compaction manifest {manifest_id} under {}", archive_dir.display())]
    ManifestNotFound {
        manifest_id: String,
        archive_dir: PathBuf,
    },
    #[error("manifest {manifest_id} is unreadable: {error}")]
    ManifestUnreadable { manifest_id: String, error: String },
    #[error("{0}")]
    Io(String),
}

// ---------------------------------------------------------------------------
// Plan
// ---------------------------------------------------------------------------

/// Read every candidate and decide what a compaction pass would reclaim.
///
/// Whole-file reads, like the accounting it shares its classifier with; this
/// runs only under an explicit operator command.
pub fn plan_compaction(project_root: &Path, entries: &[RunCatalogEntry]) -> CompactionPlan {
    let sessions_dir = project_sessions_dir(project_root);
    let mut files = Vec::new();
    let mut candidates_considered = 0_u64;
    let mut skipped_not_terminal = 0_u64;
    let mut skipped_unreadable = 0_u64;

    for entry in entries {
        // Session-directory authority, same rule the snapshot loader applies:
        // a record that does not name a direct child of this project's sessions
        // directory is not something maintenance may rewrite.
        if entry.session_path.parent() != Some(sessions_dir.as_path()) {
            continue;
        }
        candidates_considered += 1;
        if !entry.is_terminal() {
            skipped_not_terminal += 1;
            continue;
        }
        let transport = entry.transport.clone();
        let Ok(scan) = scan_reclaimable(&entry.session_path, transport.as_deref()) else {
            skipped_unreadable += 1;
            continue;
        };
        if scan.records == 0 {
            continue;
        }
        let Ok(metadata) = std::fs::symlink_metadata(&entry.session_path) else {
            skipped_unreadable += 1;
            continue;
        };
        if !metadata.is_file() || metadata.file_type().is_symlink() {
            skipped_unreadable += 1;
            continue;
        }
        files.push(CompactionPlanFile {
            session_path: entry.session_path.clone(),
            run_id: entry.run_id.clone(),
            driver: entry.driver_label(),
            transport,
            fingerprint: SessionFileFingerprint::of(&metadata),
            total_bytes: metadata.len(),
            reclaimable_records: scan.records,
            reclaimable_bytes: scan.bytes,
            reclaimable_sha256: scan.sha256,
        });
    }
    files.sort_by(|a, b| a.session_path.cmp(&b.session_path));

    let reclaimable_bytes = files.iter().map(|file| file.reclaimable_bytes).sum();
    let reclaimable_records = files.iter().map(|file| file.reclaimable_records).sum();
    let manifest_id = manifest_id_for(project_root, &files);
    CompactionPlan {
        manifest_id,
        project_root: project_root.to_path_buf(),
        planned_at: Utc::now(),
        files,
        reclaimable_bytes,
        reclaimable_records,
        candidates_considered,
        skipped_not_terminal,
        skipped_unreadable,
    }
}

/// The plan's stable id: a digest over the project root and every planned
/// file's path, identity and reclaim decision.
///
/// Deliberately excludes `planned_at`, so re-planning an unchanged board twice
/// produces the same token and a confirmation does not expire for no reason.
fn manifest_id_for(project_root: &Path, files: &[CompactionPlanFile]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(b"orgasmic-run-history-compaction/1\n");
    hasher.update(project_root.as_os_str().as_encoded_bytes());
    hasher.update(b"\n");
    for file in files {
        hasher.update(file.session_path.as_os_str().as_encoded_bytes());
        hasher.update(
            format!(
                "\n{}:{}:{}:{}:{}:{}:{}\n",
                file.fingerprint.dev,
                file.fingerprint.ino,
                file.fingerprint.len,
                file.fingerprint.mtime_ns,
                file.reclaimable_records,
                file.reclaimable_bytes,
                file.reclaimable_sha256,
            )
            .as_bytes(),
        );
    }
    hex(&hasher.finalize())
}

struct ReclaimableScan {
    records: u64,
    bytes: u64,
    sha256: String,
}

fn scan_reclaimable(path: &Path, transport: Option<&str>) -> std::io::Result<ReclaimableScan> {
    let file = std::fs::File::open(path)?;
    let mut reader = std::io::BufReader::with_capacity(256 * 1024, file);
    let mut records = 0_u64;
    let mut bytes = 0_u64;
    let mut hasher = Sha256::new();
    for record in read_history_records(&mut reader) {
        let record = record?;
        if !class_is_reclaimable(record.class, transport) {
            continue;
        }
        records += 1;
        bytes += record.bytes;
        hasher.update(&record.raw);
    }
    Ok(ReclaimableScan {
        records,
        bytes,
        sha256: hex(&hasher.finalize()),
    })
}

fn hex(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        out.push_str(&format!("{byte:02x}"));
    }
    out
}

// ---------------------------------------------------------------------------
// Apply
// ---------------------------------------------------------------------------

/// Dry-run report for `plan`, writing nothing.
pub fn dry_run_report(plan: CompactionPlan) -> CompactionReport {
    let confirm_token = plan.manifest_id.clone();
    CompactionReport {
        dry_run: true,
        plan,
        confirm_token,
        results: Vec::new(),
        reclaimed_bytes: 0,
        reclaimed_records: 0,
        archive_dir: None,
        note: "dry run: no file was written, moved, truncated, or deleted. Pass the \
               confirm token back to execute exactly this plan; the originals are \
               archived whole and `run history rollback` restores them.",
    }
}

/// Execute `plan`, after checking the operator's confirmation against it.
///
/// `plan` must be freshly computed by the caller: the confirmation is checked
/// against the plan that is about to run, never against a stored one.
pub fn apply_compaction(
    plan: CompactionPlan,
    confirm: Option<&str>,
) -> Result<CompactionReport, CompactionError> {
    let Some(confirm) = confirm else {
        return Err(CompactionError::ConfirmationRequired {
            expected: plan.manifest_id.clone(),
        });
    };
    if confirm != plan.manifest_id {
        return Err(CompactionError::ConfirmationStale {
            supplied: confirm.to_string(),
            expected: plan.manifest_id.clone(),
        });
    }

    let archive_dir = plan
        .project_root
        .join(ARCHIVE_REL_PATH)
        .join(&plan.manifest_id);
    std::fs::create_dir_all(&archive_dir)
        .map_err(|error| CompactionError::Io(format!("create archive dir: {error}")))?;

    // The manifest lands BEFORE any file is touched, so a transaction killed
    // partway leaves a durable statement of what it was doing.
    let mut manifest = CompactionManifest {
        manifest_id: plan.manifest_id.clone(),
        project_root: plan.project_root.clone(),
        started_at: Utc::now(),
        plan: plan.clone(),
        results: Vec::new(),
    };
    write_manifest(&archive_dir, &manifest)
        .map_err(|error| CompactionError::Io(format!("write manifest: {error}")))?;

    let mut reclaimed_bytes = 0_u64;
    let mut reclaimed_records = 0_u64;
    for planned in &plan.files {
        let outcome = compact_one_file(planned, &archive_dir, &plan.manifest_id);
        if let CompactionFileOutcome::Compacted {
            reclaimed_records: records,
            reclaimed_bytes: bytes,
            ..
        } = &outcome
        {
            reclaimed_records += records;
            reclaimed_bytes += bytes;
        }
        manifest.results.push(CompactionFileResult {
            session_path: planned.session_path.clone(),
            run_id: planned.run_id.clone(),
            outcome,
        });
        // Rewrite the manifest after every file: the journal is what makes a
        // half-finished transaction recoverable rather than merely survivable.
        let _ = write_manifest(&archive_dir, &manifest);
    }

    let results = manifest.results.clone();
    Ok(CompactionReport {
        dry_run: false,
        confirm_token: plan.manifest_id.clone(),
        plan,
        results,
        reclaimed_bytes,
        reclaimed_records,
        archive_dir: Some(archive_dir),
        note: "originals archived whole; each session file was replaced by an atomic \
               rename. `run history rollback --manifest <id>` restores them byte for \
               byte.",
    })
}

fn write_manifest(archive_dir: &Path, manifest: &CompactionManifest) -> std::io::Result<()> {
    let path = archive_dir.join("manifest.json");
    let staged = archive_dir.join("manifest.json.tmp");
    let bytes = serde_json::to_vec_pretty(manifest)
        .map_err(|error| std::io::Error::other(error.to_string()))?;
    write_and_sync(&staged, &bytes)?;
    std::fs::rename(&staged, &path)
}

fn write_and_sync(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    let mut file = std::fs::File::create(path)?;
    file.write_all(bytes)?;
    file.flush()?;
    file.sync_all()
}

/// Rewrite one session file, keeping its original recoverable.
///
/// Order is the whole safety argument:
///
/// 1. re-verify the file identity the plan was decided against;
/// 2. read and build the replacement in memory;
/// 3. copy the ORIGINAL into the archive and fsync it;
/// 4. write the replacement to a sibling staging file and fsync it;
/// 5. `rename` the staging file over the original.
///
/// A kill before (5) leaves the original in place. A kill during (5) is not
/// observable: `rename` within a directory is atomic. So the live path only
/// ever holds a complete file, and the bytes that left it are already on disk
/// in the archive before they leave.
fn compact_one_file(
    planned: &CompactionPlanFile,
    archive_dir: &Path,
    manifest_id: &str,
) -> CompactionFileOutcome {
    let path = planned.session_path.as_path();
    let metadata = match std::fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) => {
            return CompactionFileOutcome::SkippedChanged {
                reason: format!("session file is unreadable: {error}"),
            }
        }
    };
    if !metadata.is_file() || metadata.file_type().is_symlink() {
        return CompactionFileOutcome::SkippedChanged {
            reason: "session path is no longer a regular file".to_string(),
        };
    }
    if SessionFileFingerprint::of(&metadata) != planned.fingerprint {
        return CompactionFileOutcome::SkippedChanged {
            reason: "session file changed between planning and applying".to_string(),
        };
    }

    let rewrite = match build_compacted(planned, manifest_id) {
        Ok(rewrite) => rewrite,
        Err(error) => {
            return CompactionFileOutcome::Failed {
                error: error.to_string(),
            }
        }
    };
    if rewrite.reclaimed_records != planned.reclaimable_records
        || rewrite.reclaimed_bytes != planned.reclaimable_bytes
    {
        return CompactionFileOutcome::SkippedChanged {
            reason: format!(
                "reclaimable content changed between planning and applying: planned {} \
                 records / {} bytes, found {} / {}",
                planned.reclaimable_records,
                planned.reclaimable_bytes,
                rewrite.reclaimed_records,
                rewrite.reclaimed_bytes
            ),
        };
    }

    let Some(file_name) = path.file_name() else {
        return CompactionFileOutcome::Failed {
            error: "session path has no file name".to_string(),
        };
    };
    let archived = archive_dir.join(file_name);
    if let Err(error) = archive_original(path, &archived) {
        return CompactionFileOutcome::Failed {
            error: format!("archive original: {error}"),
        };
    }

    let staging = staging_path(path);
    if let Err(error) = write_and_sync(&staging, &rewrite.bytes) {
        let _ = std::fs::remove_file(&staging);
        return CompactionFileOutcome::Failed {
            error: format!("stage replacement: {error}"),
        };
    }
    if let Err(error) = std::fs::rename(&staging, path) {
        let _ = std::fs::remove_file(&staging);
        return CompactionFileOutcome::Failed {
            error: format!("commit replacement: {error}"),
        };
    }
    CompactionFileOutcome::Compacted {
        reclaimed_records: rewrite.reclaimed_records,
        reclaimed_bytes: rewrite.reclaimed_bytes,
        archived,
        bytes_before: planned.total_bytes,
        bytes_after: rewrite.bytes.len() as u64,
    }
}

fn staging_path(path: &Path) -> PathBuf {
    let mut name = path.as_os_str().to_os_string();
    name.push(STAGING_SUFFIX);
    PathBuf::from(name)
}

/// Copy `source` to `archived` and fsync both the copy and its directory, so
/// the original bytes are durable before the live path is touched.
fn archive_original(source: &Path, archived: &Path) -> std::io::Result<()> {
    let bytes = std::fs::read(source)?;
    write_and_sync(archived, &bytes)?;
    if let Some(parent) = archived.parent() {
        if let Ok(dir) = std::fs::File::open(parent) {
            let _ = dir.sync_all();
        }
    }
    Ok(())
}

struct CompactedFile {
    bytes: Vec<u8>,
    reclaimed_records: u64,
    reclaimed_bytes: u64,
}

/// Build the replacement content: every non-reclaimable record verbatim, plus
/// one summary record standing where the reclaimed ones were.
///
/// The summary is a `note` envelope carrying the removed byte count, the digest
/// of the removed bytes, and the archive that holds them — a truthful source
/// reference, not a claim that the content is gone. It reuses the last
/// reclaimed record's envelope header so the line belongs to the same run,
/// runtime and boot, and sorts where the removed content used to sit.
fn build_compacted(
    planned: &CompactionPlanFile,
    manifest_id: &str,
) -> std::io::Result<CompactedFile> {
    let file = std::fs::File::open(&planned.session_path)?;
    let mut reader = std::io::BufReader::with_capacity(256 * 1024, file);
    let mut out: Vec<u8> = Vec::with_capacity(planned.total_bytes as usize);
    let mut reclaimed_records = 0_u64;
    let mut reclaimed_bytes = 0_u64;
    let mut hasher = Sha256::new();
    let mut last_reclaimed_header: Option<serde_json::Value> = None;
    let mut summary_at: Option<usize> = None;

    for record in read_history_records(&mut reader) {
        let record = record?;
        if !class_is_reclaimable(record.class, planned.transport.as_deref()) {
            out.extend_from_slice(&record.raw);
            continue;
        }
        reclaimed_records += 1;
        reclaimed_bytes += record.bytes;
        hasher.update(&record.raw);
        if let Ok(value) = serde_json::from_slice::<serde_json::Value>(
            record.raw.strip_suffix(b"\n").unwrap_or(&record.raw),
        ) {
            last_reclaimed_header = Some(value);
        }
        summary_at = Some(out.len());
    }

    if let Some(position) = summary_at {
        let summary = summary_record(
            last_reclaimed_header.as_ref(),
            manifest_id,
            reclaimed_records,
            reclaimed_bytes,
            &hex(&hasher.finalize()),
            &planned.session_path,
        );
        out.splice(position..position, summary);
    }

    Ok(CompactedFile {
        bytes: out,
        reclaimed_records,
        reclaimed_bytes,
    })
}

fn summary_record(
    header: Option<&serde_json::Value>,
    manifest_id: &str,
    records: u64,
    bytes: u64,
    sha256: &str,
    session_path: &Path,
) -> Vec<u8> {
    let string_field = |key: &str| -> String {
        header
            .and_then(|value| value.get(key))
            .and_then(serde_json::Value::as_str)
            .unwrap_or_default()
            .to_string()
    };
    let file_name = session_path
        .file_name()
        .map(|name| name.to_string_lossy().to_string())
        .unwrap_or_default();
    let line = serde_json::json!({
        "seq": header
            .and_then(|value| value.get("seq"))
            .and_then(serde_json::Value::as_u64)
            .unwrap_or(0),
        "time": header
            .and_then(|value| value.get("time"))
            .and_then(serde_json::Value::as_str)
            .map(str::to_string)
            .unwrap_or_else(|| Utc::now().to_rfc3339()),
        "run_id": string_field("run_id"),
        "runtime_id": string_field("runtime_id"),
        "boot_id": string_field("boot_id"),
        "kind": "note",
        "event": {
            "note": "orgasmic-run-history-compacted",
            "manifest_id": manifest_id,
            "reclaimed_records": records,
            "reclaimed_bytes": bytes,
            "sha256": sha256,
            "source": format!("{ARCHIVE_REL_PATH}/{manifest_id}/{file_name}"),
        },
    });
    let mut out = serde_json::to_vec(&line).unwrap_or_default();
    out.push(b'\n');
    out
}

// ---------------------------------------------------------------------------
// Rollback
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RollbackReport {
    pub manifest_id: String,
    pub project_root: PathBuf,
    pub restored: Vec<PathBuf>,
    pub missing_archives: Vec<PathBuf>,
    pub failed: BTreeMap<String, String>,
}

/// Restore every archived original recorded by `manifest_id`.
///
/// Uses the same staging+rename discipline as the forward pass, so a rollback
/// killed partway also leaves each individual file whole.
pub fn rollback_compaction(
    project_root: &Path,
    manifest_id: &str,
) -> Result<RollbackReport, CompactionError> {
    let archive_dir = project_root.join(ARCHIVE_REL_PATH).join(manifest_id);
    let manifest_path = archive_dir.join("manifest.json");
    let source = std::fs::read_to_string(&manifest_path).map_err(|error| {
        if error.kind() == std::io::ErrorKind::NotFound {
            CompactionError::ManifestNotFound {
                manifest_id: manifest_id.to_string(),
                archive_dir: project_root.join(ARCHIVE_REL_PATH),
            }
        } else {
            CompactionError::ManifestUnreadable {
                manifest_id: manifest_id.to_string(),
                error: error.to_string(),
            }
        }
    })?;
    let manifest: CompactionManifest =
        serde_json::from_str(&source).map_err(|error| CompactionError::ManifestUnreadable {
            manifest_id: manifest_id.to_string(),
            error: error.to_string(),
        })?;

    let sessions_dir = project_sessions_dir(project_root);
    let mut report = RollbackReport {
        manifest_id: manifest_id.to_string(),
        project_root: project_root.to_path_buf(),
        restored: Vec::new(),
        missing_archives: Vec::new(),
        failed: BTreeMap::new(),
    };
    for result in &manifest.results {
        let CompactionFileOutcome::Compacted { archived, .. } = &result.outcome else {
            continue;
        };
        // Session-directory authority again: a hand-edited manifest must not be
        // able to name an arbitrary destination.
        if result.session_path.parent() != Some(sessions_dir.as_path()) {
            report.failed.insert(
                result.session_path.display().to_string(),
                "manifest names a path outside this project's sessions directory".to_string(),
            );
            continue;
        }
        let bytes = match std::fs::read(archived) {
            Ok(bytes) => bytes,
            Err(_) => {
                report.missing_archives.push(archived.clone());
                continue;
            }
        };
        let staging = staging_path(&result.session_path);
        if let Err(error) = write_and_sync(&staging, &bytes) {
            let _ = std::fs::remove_file(&staging);
            report
                .failed
                .insert(result.session_path.display().to_string(), error.to_string());
            continue;
        }
        if let Err(error) = std::fs::rename(&staging, &result.session_path) {
            let _ = std::fs::remove_file(&staging);
            report
                .failed
                .insert(result.session_path.display().to_string(), error.to_string());
            continue;
        }
        report.restored.push(result.session_path.clone());
    }
    Ok(report)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::run_catalog::{RunCatalog, CATALOG_REL_PATH};
    use orgasmic_core::session::{
        ReleaseOutcome, SessionEnvelope, SessionEventKind, SessionScanBudget,
    };
    use serde_json::json;

    struct Board {
        _tmp: tempfile::TempDir,
        root: PathBuf,
        sessions: PathBuf,
    }

    fn board() -> Board {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("proj");
        std::fs::create_dir_all(root.join(".orgasmic/tmp/sessions")).unwrap();
        std::fs::write(
            root.join(".orgasmic/project.org"),
            "#+title: p\n\n* PROJECT p\n:PROPERTIES:\n:ID: proj\n:END:\n",
        )
        .unwrap();
        let root = root.canonicalize().unwrap();
        let sessions = root.join(".orgasmic/tmp/sessions");
        Board {
            _tmp: tmp,
            root,
            sessions,
        }
    }

    /// A session file with `redraws` legacy `text_chunk` records, written
    /// directly (the writer fsyncs every line and caps payloads).
    fn write_session(
        sessions: &Path,
        run_id: &str,
        transport: &str,
        redraws: usize,
        released: bool,
    ) -> PathBuf {
        let path = sessions.join(format!("{run_id}.jsonl"));
        let mut seq = 0_u64;
        let mut out = String::new();
        let mut push = |kind: SessionEventKind, event: serde_json::Value, out: &mut String| {
            let envelope = SessionEnvelope {
                seq,
                time: Utc::now(),
                run_id: run_id.to_string(),
                runtime_id: format!("runtime-{run_id}"),
                boot_id: "boot-compact".to_string(),
                kind,
                event,
            };
            out.push_str(&serde_json::to_string(&envelope).unwrap());
            out.push('\n');
            seq += 1;
        };
        push(
            SessionEventKind::Lifecycle,
            json!({"phase": "acquire", "kind": "worker", "task_id": "TASK-CMP", "worker_id": "implementer"}),
            &mut out,
        );
        push(
            SessionEventKind::Lifecycle,
            json!({
                "phase": "run_meta",
                "transport": transport,
                "harness": "claude",
                "project_id": "proj",
                "driver_config": {},
            }),
            &mut out,
        );
        for _ in 0..redraws {
            push(
                SessionEventKind::DriverEvent,
                json!({"type": "text_chunk", "stream": "stdout", "chunk": "\u{1b}[H\u{1b}[2J".to_string() + &"redraw ".repeat(512)}),
                &mut out,
            );
        }
        push(
            SessionEventKind::DriverEvent,
            json!({"type": "tool_call", "call_id": "c1", "name": "shell"}),
            &mut out,
        );
        if released {
            push(
                SessionEventKind::Lifecycle,
                json!({"phase": "release", "reason": "done", "outcome": ReleaseOutcome::Completed}),
                &mut out,
            );
        }
        std::fs::write(&path, out).unwrap();
        path
    }

    fn indexed(board: &Board) -> Vec<RunCatalogEntry> {
        let catalog = RunCatalog::new();
        catalog.refresh_dir(
            &board.sessions,
            Some("proj"),
            &board.root,
            SessionScanBudget::DEFAULT,
        );
        catalog.entries_for_project(&board.root)
    }

    #[test]
    fn a_plan_is_stable_and_a_dry_run_writes_nothing() {
        let board = board();
        let path = write_session(&board.sessions, "run-a", "rmux", 40, true);
        let before = std::fs::read(&path).unwrap();

        let entries = indexed(&board);
        let first = plan_compaction(&board.root, &entries);
        let second = plan_compaction(&board.root, &entries);
        assert_eq!(
            first.manifest_id, second.manifest_id,
            "the same board state must produce the same confirmation token"
        );
        assert_eq!(first.files.len(), 1);
        assert!(first.reclaimable_bytes > 0);

        let report = dry_run_report(first.clone());
        assert!(report.dry_run);
        assert_eq!(report.reclaimed_bytes, 0);
        assert_eq!(report.confirm_token, first.manifest_id);
        assert_eq!(std::fs::read(&path).unwrap(), before);
        assert!(!board.root.join(ARCHIVE_REL_PATH).exists());
    }

    #[test]
    fn compaction_requires_the_exact_confirmation_token() {
        let board = board();
        let path = write_session(&board.sessions, "run-a", "rmux", 20, true);
        let before = std::fs::read(&path).unwrap();
        let plan = plan_compaction(&board.root, &indexed(&board));

        let refused = apply_compaction(plan.clone(), None).unwrap_err();
        assert!(matches!(
            refused,
            CompactionError::ConfirmationRequired { .. }
        ));
        let stale = apply_compaction(plan.clone(), Some("not-the-plan")).unwrap_err();
        assert!(matches!(stale, CompactionError::ConfirmationStale { .. }));
        assert_eq!(
            std::fs::read(&path).unwrap(),
            before,
            "a refused confirmation must not write anything"
        );
    }

    /// The transaction reclaims exactly what it planned, keeps every other byte
    /// verbatim, and the archive is a byte-identical original.
    #[test]
    fn compaction_reclaims_exactly_the_plan_and_stays_recoverable() {
        let board = board();
        let path = write_session(&board.sessions, "run-a", "rmux", 64, true);
        let original = std::fs::read(&path).unwrap();
        let plan = plan_compaction(&board.root, &indexed(&board));
        let token = plan.manifest_id.clone();
        let planned_bytes = plan.reclaimable_bytes;
        assert!(planned_bytes > 0);

        let report = apply_compaction(plan, Some(&token)).unwrap();
        assert!(!report.dry_run);
        assert_eq!(report.reclaimed_bytes, planned_bytes);
        let CompactionFileOutcome::Compacted {
            archived,
            bytes_before,
            bytes_after,
            ..
        } = &report.results[0].outcome
        else {
            panic!("expected a compaction: {:?}", report.results[0].outcome);
        };
        assert_eq!(*bytes_before, original.len() as u64);
        assert!(*bytes_after < *bytes_before);

        // The archive is the original, byte for byte.
        assert_eq!(std::fs::read(archived).unwrap(), original);

        // Every retained record survived verbatim, in order.
        let compacted = std::fs::read_to_string(&path).unwrap();
        let kept: Vec<&str> = compacted
            .lines()
            .filter(|line| !line.contains("orgasmic-run-history-compacted"))
            .collect();
        let expected: Vec<String> = String::from_utf8(original.clone())
            .unwrap()
            .lines()
            .filter(|line| !line.contains("\"text_chunk\""))
            .map(str::to_string)
            .collect();
        assert_eq!(kept, expected);
        assert!(compacted.contains("\"phase\":\"release\""));
        assert!(compacted.contains("\"tool_call\""));
        assert!(!compacted.contains("\"text_chunk\""));

        // The summary names the size, the digest and where the bytes are.
        let summary: serde_json::Value = serde_json::from_str(
            compacted
                .lines()
                .find(|line| line.contains("orgasmic-run-history-compacted"))
                .unwrap(),
        )
        .unwrap();
        assert_eq!(
            summary["event"]["reclaimed_bytes"].as_u64(),
            Some(planned_bytes)
        );
        assert!(summary["event"]["source"]
            .as_str()
            .unwrap()
            .contains(&token));
        assert_eq!(summary["run_id"].as_str(), Some("run-a"));

        // And rollback puts the original back, byte for byte.
        let rollback = rollback_compaction(&board.root, &token).unwrap();
        assert_eq!(rollback.restored.len(), 1);
        assert!(rollback.failed.is_empty());
        assert_eq!(std::fs::read(&path).unwrap(), original);
    }

    /// Structured ACP evidence is never reclaimable, whatever it costs.
    #[test]
    fn acp_text_chunks_are_evidence_and_are_never_planned() {
        let board = board();
        let path = write_session(&board.sessions, "run-acp", "acp-stdio", 64, true);
        let before = std::fs::read(&path).unwrap();
        let plan = plan_compaction(&board.root, &indexed(&board));
        assert!(
            plan.is_empty(),
            "an acp transport's text_chunk is assistant/subprocess evidence: {:?}",
            plan.files
        );
        assert_eq!(plan.reclaimable_bytes, 0);
        assert_eq!(std::fs::read(&path).unwrap(), before);
    }

    /// A live run's file is held open by the writer; it is never a candidate.
    #[test]
    fn a_run_that_has_not_ended_is_never_compacted() {
        let board = board();
        write_session(&board.sessions, "run-live", "rmux", 40, false);
        let plan = plan_compaction(&board.root, &indexed(&board));
        assert!(plan.is_empty());
        assert_eq!(plan.skipped_not_terminal, 1);
    }

    /// A file that changed after the plan was made is skipped, not rewritten
    /// against a decision that no longer describes it.
    #[test]
    fn a_file_that_changed_after_planning_is_skipped_untouched() {
        let board = board();
        let stable = write_session(&board.sessions, "run-a", "rmux", 32, true);
        let racing = write_session(&board.sessions, "run-b", "rmux", 32, true);
        let plan = plan_compaction(&board.root, &indexed(&board));
        let token = plan.manifest_id.clone();
        assert_eq!(plan.files.len(), 2);

        // The racing file gains a line after the plan was decided.
        let mut mutated = std::fs::read(&racing).unwrap();
        mutated.extend_from_slice(
            b"{\"seq\":999,\"time\":\"2026-08-02T00:00:00Z\",\"run_id\":\"run-b\",\
              \"runtime_id\":\"runtime-run-b\",\"boot_id\":\"boot-compact\",\
              \"kind\":\"note\",\"event\":{}}\n",
        );
        std::fs::write(&racing, &mutated).unwrap();

        let report = apply_compaction(plan, Some(&token)).unwrap();
        let by_path: BTreeMap<PathBuf, &CompactionFileOutcome> = report
            .results
            .iter()
            .map(|result| (result.session_path.clone(), &result.outcome))
            .collect();
        assert!(matches!(
            by_path[&racing],
            CompactionFileOutcome::SkippedChanged { .. }
        ));
        assert_eq!(
            std::fs::read(&racing).unwrap(),
            mutated,
            "a skipped file must be byte-identical afterwards"
        );
        assert!(matches!(
            by_path[&stable],
            CompactionFileOutcome::Compacted { .. }
        ));
    }

    /// A torn final record and blank records are preserved verbatim: this
    /// transaction never removes a line it could not classify.
    #[test]
    fn torn_and_blank_records_survive_compaction() {
        let board = board();
        let path = write_session(&board.sessions, "run-a", "rmux", 16, true);
        let mut content = std::fs::read(&path).unwrap();
        // The run's terminal fact has to survive the torn tail, or the run is
        // simply not a candidate: a driver `run_complete` is what a session
        // whose last physical line was cut off still proves.
        content.extend_from_slice(
            b"{\"seq\":800,\"time\":\"2026-08-02T00:00:00Z\",\"run_id\":\"run-a\",\
              \"runtime_id\":\"runtime-run-a\",\"boot_id\":\"boot-compact\",\
              \"kind\":\"driver_event\",\"event\":{\"type\":\"run_complete\"}}\n",
        );
        content.extend_from_slice(b"\n   \n");
        // A final record cut off mid-write, with no terminating newline.
        content.extend_from_slice(
            b"{\"seq\":900,\"kind\":\"driver_event\",\"event\":{\"type\":\"text_chunk\",\"chunk\":\"tor",
        );
        std::fs::write(&path, &content).unwrap();

        let plan = plan_compaction(&board.root, &indexed(&board));
        let token = plan.manifest_id.clone();
        let report = apply_compaction(plan, Some(&token)).unwrap();
        assert!(matches!(
            report.results[0].outcome,
            CompactionFileOutcome::Compacted { .. }
        ));
        let after = std::fs::read(&path).unwrap();
        assert!(
            after.ends_with(b"\"chunk\":\"tor"),
            "the torn final record must survive verbatim"
        );
        assert!(String::from_utf8_lossy(&after).contains("\n   \n"));
    }

    /// A staging file left behind by a killed transaction never becomes the
    /// live file, and does not stop a later transaction from succeeding.
    #[test]
    fn a_stale_staging_file_is_never_the_live_file() {
        let board = board();
        let path = write_session(&board.sessions, "run-a", "rmux", 24, true);
        let staging = staging_path(&path);
        std::fs::write(&staging, b"garbage from a killed transaction\n").unwrap();
        let original = std::fs::read(&path).unwrap();

        let plan = plan_compaction(&board.root, &indexed(&board));
        let token = plan.manifest_id.clone();
        // The plan sees the real file, not the staging one: staging files are
        // not `.jsonl` children the catalog indexes.
        assert_eq!(plan.files.len(), 1);
        let report = apply_compaction(plan, Some(&token)).unwrap();
        assert!(matches!(
            report.results[0].outcome,
            CompactionFileOutcome::Compacted { .. }
        ));
        let after = std::fs::read(&path).unwrap();
        assert!(!after.starts_with(b"garbage"));
        assert!(String::from_utf8_lossy(&after).contains("\"phase\":\"acquire\""));

        // And the archive still holds the pre-transaction original.
        let rollback = rollback_compaction(&board.root, &token).unwrap();
        assert_eq!(rollback.restored.len(), 1);
        assert_eq!(std::fs::read(&path).unwrap(), original);
    }

    /// A manifest that names a path outside the project's sessions directory
    /// cannot be used to write there.
    #[test]
    fn rollback_refuses_a_manifest_naming_a_foreign_path() {
        let board = board();
        write_session(&board.sessions, "run-a", "rmux", 8, true);
        let plan = plan_compaction(&board.root, &indexed(&board));
        let token = plan.manifest_id.clone();
        apply_compaction(plan, Some(&token)).unwrap();

        let manifest_path = board
            .root
            .join(ARCHIVE_REL_PATH)
            .join(&token)
            .join("manifest.json");
        let mut manifest: CompactionManifest =
            serde_json::from_str(&std::fs::read_to_string(&manifest_path).unwrap()).unwrap();
        let victim = board.root.join(CATALOG_REL_PATH);
        std::fs::write(&victim, b"catalog\n").unwrap();
        manifest.results[0].session_path = victim.clone();
        std::fs::write(
            &manifest_path,
            serde_json::to_vec_pretty(&manifest).unwrap(),
        )
        .unwrap();

        let rollback = rollback_compaction(&board.root, &token).unwrap();
        assert!(rollback.restored.is_empty());
        assert_eq!(rollback.failed.len(), 1);
        assert_eq!(std::fs::read(&victim).unwrap(), b"catalog\n");
    }

    #[test]
    fn rollback_of_an_unknown_manifest_is_a_named_error() {
        let board = board();
        let error = rollback_compaction(&board.root, "deadbeef").unwrap_err();
        assert!(matches!(error, CompactionError::ManifestNotFound { .. }));
    }
}
