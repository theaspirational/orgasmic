// orgasmic:TASK-FZB6T.1, TASK-FZB6T.2, dec_BBPW4
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
//! # The rule this module is built on
//!
//! **Where authority cannot be proven from the current bytes, refuse rather
//! than delete** (dec_BBPW4). A refused compaction costs a re-scan; a wrong one
//! costs the only record of what a worker did.
//!
//! The catalog is disposable derived state and is never deletion authority.
//! Maintenance takes exactly one thing from it — the list of candidate paths —
//! and re-derives every fact that authorizes an irreversible operation
//! ([`crate::run_catalog::derive_session_authority`]) from the session file's
//! own current bytes, at planning time and again immediately before the file is
//! touched.
//!
//! # The shape of the transaction
//!
//! 1. **Plan.** [`plan_compaction`] re-derives every candidate from disk and
//!    produces a [`CompactionPlan`]: which records are reclaimable, how many
//!    bytes, the digest of those exact bytes, and the file identity each
//!    decision was made against. The plan carries a
//!    [`CompactionPlan::manifest_id`] that is a pure function of its own
//!    content, so the same board state always produces the same id and any
//!    change to the board produces a different one.
//! 2. **Confirm.** [`apply_compaction`] re-plans from scratch and refuses
//!    unless the operator's token equals the id of the plan it just computed.
//!    There is no server-side pending state to go stale: the token *is* the
//!    proof that the operator saw this exact plan.
//! 3. **Exclude.** The transaction takes an exclusive `flock` on
//!    [`MAINTENANCE_LOCK_REL_PATH`] for its whole duration, and refuses to
//!    touch any session path the caller has not proven it fenced against the
//!    session writer ([`FencedSessions`]).
//! 4. **Apply.** Per file, through a durable journalled state machine
//!    ([`CompactionFileStage`]): archive the ONE byte generation the plan was
//!    decided against, build the replacement from those same bytes, stage it,
//!    journal the staged image's digest, re-verify the live file has not moved,
//!    then `rename`. Every journal write is fsynced and every rename is
//!    followed by a parent-directory fsync.
//! 5. **Roll back.** [`rollback_compaction`] restores every archived original,
//!    and refuses any destination that does not hold a byte image this
//!    transaction recorded.
//!
//! # What is eligible
//!
//! Only runs whose CURRENT bytes prove they are terminal, and only records
//! [`crate::run_catalog::class_is_reclaimable`] proves are rendered pane
//! payload. A live run's session file is held open by the session writer in
//! append mode; renaming a new file over that path would leave the writer
//! appending to an orphaned inode and silently lose every subsequent lifecycle
//! line. That is not a risk worth a few megabytes, so a run that has not ended
//! is never a candidate — and even a terminal one is refused unless its writer
//! handle has been fenced.

use std::collections::{BTreeMap, BTreeSet};
use std::io::Write;
use std::path::{Path, PathBuf};

use chrono::{DateTime, Utc};
use orgasmic_core::session::SessionScanBudget;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::run_catalog::{
    class_is_reclaimable, derive_session_authority, project_sessions_dir, read_history_records,
    transport_is_pane, RunCatalogEntry, SessionFileFingerprint,
};

/// Where archived originals and manifests live, relative to a project root.
pub const ARCHIVE_REL_PATH: &str = ".orgasmic/tmp/run-history-archive";

/// The per-project maintenance exclusion lock (dec_BBPW4 question 3).
///
/// An exclusive `flock`, so it serializes compact against compact and compact
/// against rollback ACROSS PROCESSES — an in-process mutex would leave a second
/// daemon, or a CLI invoked against the same board, free to interleave.
pub const MAINTENANCE_LOCK_REL_PATH: &str = ".orgasmic/tmp/run-history-archive/maintenance.lock";

/// Suffix of the sibling file a rewrite is staged in before its rename.
const STAGING_SUFFIX: &str = ".orgasmic-compact-tmp";

/// One session file's share of a compaction plan.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CompactionPlanFile {
    pub session_path: PathBuf,
    pub run_id: String,
    pub driver: String,
    /// The transport this run's CURRENT bytes record. Re-derived, never taken
    /// from a catalog entry (dec_BBPW4).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub transport: Option<String>,
    /// File identity this plan was decided against. Re-verified before the
    /// rewrite and again immediately before the rename; a mismatch skips the
    /// file rather than rewriting bytes the plan never saw.
    pub fingerprint: SessionFileFingerprint,
    pub total_bytes: u64,
    pub reclaimable_records: u64,
    pub reclaimable_bytes: u64,
    /// SHA-256 over the reclaimable records' raw bytes, in file order.
    ///
    /// orgasmic:TASK-FZB6T.2 finding 3 — the apply pass compares this digest,
    /// not just the record and byte COUNTS. Two different sets of records can
    /// have the same count and the same total size; only the digest proves the
    /// bytes about to be removed are the bytes the operator confirmed.
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
    /// Records excluded because a fresh read of the file does not prove the run
    /// ended.
    pub skipped_not_terminal: u64,
    /// Records excluded because their session file could not be read.
    pub skipped_unreadable: u64,
    /// Records excluded because the file changed WHILE it was being planned, so
    /// no single byte generation was ever proven.
    #[serde(default)]
    pub skipped_unstable: u64,
    /// Records excluded because a bounded scan skipped part of the file, so the
    /// terminal verdict is not provable from what was read.
    #[serde(default)]
    pub skipped_unproven: u64,
}

impl CompactionPlan {
    pub fn is_empty(&self) -> bool {
        self.files.is_empty()
    }
}

/// Session paths the caller holds an exclusive session-writer LEASE on, for the
/// whole of this transaction.
///
/// orgasmic:TASK-FZB6T.2 finding 3 — a `rename` over a path the writer holds
/// open leaves the writer appending to an orphaned inode, and every line it
/// writes afterwards is lost.
///
/// orgasmic:TASK-FZB6T.3 finding 1 — closing the handle once was not enough:
/// the next append reopened the same path, so an append landing between the
/// final fingerprint check and the `rename` still hit the doomed inode, and the
/// archive predated it so rollback could not recover it either. The caller now
/// proves a HELD lease ([`crate::writer::SessionLease`]): while it is held the
/// writer defers appends for these paths before opening anything, and releases
/// them only after the rename and the journal are done. A planned file that is
/// not in this set is refused, not compacted. An empty set therefore compacts
/// nothing, which is the correct fail-closed default.
#[derive(Debug, Clone, Default)]
pub struct FencedSessions {
    paths: BTreeSet<PathBuf>,
}

impl FencedSessions {
    pub fn new(paths: impl IntoIterator<Item = PathBuf>) -> Self {
        Self {
            paths: paths.into_iter().collect(),
        }
    }

    pub fn contains(&self, path: &Path) -> bool {
        self.paths.contains(path)
    }
}

/// What one file's rewrite did, as reported to the operator.
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
    /// Refused before anything was written. The file is untouched.
    SkippedChanged { reason: String },
    /// The rewrite stopped after it had begun. `stage` names the furthest
    /// DURABLE state it reached, which is what a rollback needs to know.
    ///
    /// `stage` defaults because format 1 did not record one: a manifest from
    /// that shape must still decode rather than failing the whole rollback on a
    /// missing field (orgasmic:TASK-FZB6T.3 finding 2).
    Failed {
        error: String,
        #[serde(default = "unrecorded_stage")]
        stage: String,
    },
}

fn unrecorded_stage() -> String {
    "unrecorded".to_string()
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CompactionFileResult {
    pub session_path: PathBuf,
    pub run_id: String,
    #[serde(flatten)]
    pub outcome: CompactionFileOutcome,
}

/// The furthest DURABLE state one file's rewrite reached.
///
/// orgasmic:TASK-FZB6T.2 finding 2 — the old transaction mutated a session
/// file, appended a result in memory, and ignored every journal write failure.
/// A crash or ENOSPC after the live rename but before a durable manifest update
/// left the archived original and the rewritten live file absent from the
/// manifest entirely, and rollback iterated only the results — so it could not
/// restore what it could not name.
///
/// Every stage below is written to the manifest, fsynced, and its parent
/// directory fsynced, BEFORE the operation it authorizes. The critical one is
/// [`Self::Staged`]: it records the replacement's digest before the rename, so
/// a crash across the rename leaves BOTH candidate images named on disk and a
/// rollback can tell which one the live path holds.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "stage", rename_all = "snake_case")]
pub enum CompactionFileStage {
    /// The transaction intends to rewrite this file. Nothing has been touched.
    Planned,
    /// The original is durably archived and hashes to `original_sha256`.
    Archived {
        archived: PathBuf,
        original_sha256: String,
    },
    /// The replacement is durably staged and hashes to `replacement_sha256`.
    /// Written before the rename.
    Staged {
        archived: PathBuf,
        original_sha256: String,
        staging: PathBuf,
        replacement_sha256: String,
        reclaimed_records: u64,
        reclaimed_bytes: u64,
        bytes_before: u64,
        bytes_after: u64,
    },
    /// The rename committed and the sessions directory was fsynced.
    Committed {
        archived: PathBuf,
        original_sha256: String,
        replacement_sha256: String,
        reclaimed_records: u64,
        reclaimed_bytes: u64,
        bytes_before: u64,
        bytes_after: u64,
        post_fingerprint: SessionFileFingerprint,
    },
}

impl CompactionFileStage {
    pub fn label(&self) -> &'static str {
        match self {
            Self::Planned => "planned",
            Self::Archived { .. } => "archived",
            Self::Staged { .. } => "staged",
            Self::Committed { .. } => "committed",
        }
    }
}

/// One file's durable journal record. Exactly one exists per planned file, from
/// the first manifest write onwards — so a rollback can always reconstruct what
/// a killed transaction was doing from the PLAN, even when no result was ever
/// written.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CompactionFileJournal {
    pub session_path: PathBuf,
    pub run_id: String,
    #[serde(flatten)]
    pub stage: CompactionFileStage,
    /// Why this file stopped where it did, when it stopped short of committing.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub refused: Option<String>,
}

/// The manifest shape this build writes.
///
/// 1. `results: Vec<CompactionFileResult>`, unversioned (TASK-FZB6T.1).
/// 2. `files: Vec<CompactionFileJournal>` — one durable journal record per
///    PLANNED file, so a transaction killed before it could report a result is
///    still fully described (TASK-FZB6T.2 finding 2).
pub const COMPACTION_MANIFEST_FORMAT: u32 = 2;

/// A manifest with no `manifest_format` is the unversioned format-1 shape.
fn legacy_manifest_format() -> u32 {
    1
}

/// The durable record of one applied transaction, written under the archive
/// directory before any file is touched.
///
/// # Why this is versioned
///
/// orgasmic:TASK-FZB6T.3 finding 2 — format 2 replaced `results` with `files`
/// and said nothing about it. Both `Vec`s are `#[serde(default)]`, so a reader
/// of the other shape saw an EMPTY list, restored nothing, and reported
/// SUCCESS. For the one mechanism whose whole justification is that deletion is
/// recoverable, a silent successful-empty rollback is the worst available
/// outcome, and it happened in both directions at once.
///
/// So: the format is stated, a format this build does not know is refused
/// loudly rather than read, format 1's `results` are decoded, and every write
/// emits `results` ALONGSIDE `files` so a runtime that predates `files` still
/// finds the archives it needs. A manifest whose plan names files but which
/// carries no per-file record of either shape is refused, never reported as a
/// successful rollback of nothing.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CompactionManifest {
    /// Which shape this manifest is. Absent means format 1.
    #[serde(default = "legacy_manifest_format")]
    pub manifest_format: u32,
    pub manifest_id: String,
    pub project_root: PathBuf,
    pub started_at: DateTime<Utc>,
    pub plan: CompactionPlan,
    /// One record per planned file, in plan order, present from the first write.
    /// Authoritative from format 2 onwards.
    #[serde(default)]
    pub files: Vec<CompactionFileJournal>,
    /// The same journal rendered in format 1's shape.
    ///
    /// Written, never read, when `files` is present: it exists so a runtime
    /// that predates `files` reads a manifest this build wrote and finds the
    /// archived originals instead of an empty list. When `files` is ABSENT this
    /// is the manifest's only per-file record and the rollback decodes it.
    #[serde(default)]
    pub results: Vec<CompactionFileResult>,
    /// `false` on a manifest whose transaction never reached its end.
    #[serde(default)]
    pub complete: bool,
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
    /// `false` when the transaction stopped early; the manifest still names
    /// every file and the furthest durable stage each one reached.
    #[serde(default)]
    pub complete: bool,
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
    #[error(
        "run-history maintenance is already running for {}; it is exclusive per project. \
         Wait for it to finish, then re-read the plan.",
        project_root.display()
    )]
    MaintenanceBusy { project_root: PathBuf },
    #[error("no compaction manifest {manifest_id} under {}", archive_dir.display())]
    ManifestNotFound {
        manifest_id: String,
        archive_dir: PathBuf,
    },
    #[error("manifest {manifest_id} is unreadable: {error}")]
    ManifestUnreadable { manifest_id: String, error: String },
    #[error(
        "manifest {manifest_id} is format {found}; this build understands up to {supported}. \
         It was written by a newer runtime and this one cannot prove what it means, so the \
         rollback is refused rather than run against a shape it would read as empty. Roll \
         back with the runtime that wrote it."
    )]
    ManifestUnsupportedFormat {
        manifest_id: String,
        found: u32,
        supported: u32,
    },
    #[error(
        "manifest {manifest_id} plans {planned} file(s) but records none of them in any shape \
         this build can read, so a rollback would restore nothing while reporting success. \
         Refused: the archived originals are still under the archive directory and can be \
         restored by hand."
    )]
    ManifestUnrestorable { manifest_id: String, planned: usize },
    #[error(
        "manifest {manifest_id} does not describe one coherent transaction ({reason}), so this \
         build cannot prove which generation of which file it would restore. Refused: the \
         archived originals are still under the archive directory and can be restored by hand."
    )]
    ManifestInconsistent { manifest_id: String, reason: String },
    #[error(
        "the transaction journal could not be made durable, so the transaction stopped \
         before it could do anything it could not undo: {0}"
    )]
    JournalFailed(String),
    #[error("{0}")]
    Io(String),
}

// ---------------------------------------------------------------------------
// Exclusion
// ---------------------------------------------------------------------------

/// An exclusive per-project maintenance lock, released when dropped.
pub struct MaintenanceLock {
    _file: std::fs::File,
}

fn acquire_maintenance_lock(project_root: &Path) -> Result<MaintenanceLock, CompactionError> {
    let path = project_root.join(MAINTENANCE_LOCK_REL_PATH);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|error| CompactionError::Io(format!("create archive dir: {error}")))?;
    }
    let file = std::fs::OpenOptions::new()
        .create(true)
        .read(true)
        .write(true)
        .truncate(false)
        .open(&path)
        .map_err(|error| CompactionError::Io(format!("open maintenance lock: {error}")))?;
    fs2::FileExt::try_lock_exclusive(&file).map_err(|_| CompactionError::MaintenanceBusy {
        project_root: project_root.to_path_buf(),
    })?;
    Ok(MaintenanceLock { _file: file })
}

// ---------------------------------------------------------------------------
// Fault injection
// ---------------------------------------------------------------------------

/// A boundary at which a fault can be injected, so the durability claims in
/// this module have execution evidence behind them rather than a comment.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum FaultPoint {
    /// The write of the archived original (full disk).
    ArchiveWrite,
    /// After the archive is durable, before anything else.
    AfterArchive,
    /// The write of the staged replacement (full disk).
    StageWrite,
    /// After the replacement is durably staged, before the rename.
    AfterStage,
    /// After the rename, before the result is journalled — the exact window
    /// the reviewer's finding 2 is about.
    AfterRename,
    /// The journal write that records the committed result (full disk).
    ResultJournalWrite,
}

/// What an injected fault does.
///
/// Only the tests construct these; a production build reaches the same code
/// through [`no_faults`], which never yields one. The type still has to exist
/// outside `cfg(test)` because [`FaultInjector`] is in the signature the
/// production entry point calls through.
#[derive(Debug, Clone)]
#[cfg_attr(not(test), allow(dead_code))]
pub(crate) enum Fault {
    /// The next I/O at this point fails, as a full disk would.
    Io(String),
    /// The process dies here. The transaction returns immediately, leaving the
    /// filesystem in exactly the state the boundary produced.
    Crash,
}

pub(crate) type FaultInjector<'a> = dyn Fn(FaultPoint, &Path) -> Option<Fault> + 'a;

fn no_faults(_: FaultPoint, _: &Path) -> Option<Fault> {
    None
}

/// A crash injected mid-transaction, reported as an ordinary I/O stop. A real
/// crash is not observable by the caller at all; this is the closest a test can
/// get without killing the process.
fn crash_error(point: FaultPoint) -> CompactionError {
    CompactionError::Io(format!("injected crash at {point:?}"))
}

// ---------------------------------------------------------------------------
// Plan
// ---------------------------------------------------------------------------

/// Read every candidate and decide what a compaction pass would reclaim.
///
/// `entries` is a CANDIDATE LIST and nothing more (dec_BBPW4). Terminal state,
/// transport and reclaimability are all re-derived here from each file's
/// current bytes; a catalog entry that says a live ACP run is a terminal rmux
/// one changes which paths are looked at and changes no decision.
///
/// Every planned file is also proven STABLE: the file identity is read before
/// and after the two scans, and a file that moved in between is refused rather
/// than planned against a generation that no longer exists.
pub fn plan_compaction(project_root: &Path, entries: &[RunCatalogEntry]) -> CompactionPlan {
    let sessions_dir = project_sessions_dir(project_root);
    let mut files = Vec::new();
    let mut seen: BTreeSet<PathBuf> = BTreeSet::new();
    let mut candidates_considered = 0_u64;
    let mut skipped_not_terminal = 0_u64;
    let mut skipped_unreadable = 0_u64;
    let mut skipped_unstable = 0_u64;
    let mut skipped_unproven = 0_u64;

    for entry in entries {
        // Session-directory authority, same rule the snapshot loader applies:
        // a record that does not name a direct child of this project's sessions
        // directory is not something maintenance may rewrite.
        if entry.session_path.parent() != Some(sessions_dir.as_path()) {
            continue;
        }
        if !seen.insert(entry.session_path.clone()) {
            continue;
        }
        candidates_considered += 1;
        let path = entry.session_path.as_path();

        let Ok(metadata) = std::fs::symlink_metadata(path) else {
            skipped_unreadable += 1;
            continue;
        };
        if !metadata.is_file() || metadata.file_type().is_symlink() {
            skipped_unreadable += 1;
            continue;
        }
        let fingerprint = SessionFileFingerprint::of(&metadata);

        // Deletion authority, re-derived from the bytes on disk right now.
        let Ok(authority) = derive_session_authority(path, SessionScanBudget::DEFAULT) else {
            skipped_unreadable += 1;
            continue;
        };
        if !authority.is_terminal() {
            skipped_not_terminal += 1;
            continue;
        }
        if !authority.terminal_is_proven() {
            // The verdict does not rest on the file's genuine end — a bounded
            // scan whose retained segment is not provably the last one. Refuse
            // rather than delete.
            skipped_unproven += 1;
            continue;
        }
        // Only a pane transport's `text_chunk` is ever reclaimable, and that
        // has to be proven, not assumed: an unrecorded transport reclaims
        // nothing.
        let transport = authority.transport.clone();
        if !transport.as_deref().is_some_and(transport_is_pane) {
            continue;
        }

        let Ok(scan) = scan_reclaimable(path, transport.as_deref()) else {
            skipped_unreadable += 1;
            continue;
        };
        if scan.records == 0 {
            continue;
        }

        // One byte generation, proven: the identity that was true before the
        // derivation must still be true after it.
        let Ok(after) = std::fs::symlink_metadata(path) else {
            skipped_unreadable += 1;
            continue;
        };
        if SessionFileFingerprint::of(&after) != fingerprint {
            skipped_unstable += 1;
            continue;
        }

        files.push(CompactionPlanFile {
            session_path: entry.session_path.clone(),
            run_id: authority.run_id.clone(),
            driver: authority.driver_label(),
            transport,
            fingerprint,
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
        skipped_unstable,
        skipped_unproven,
    }
}

/// The plan's stable id: a digest over the project root and every planned
/// file's path, identity and reclaim decision.
///
/// Deliberately excludes `planned_at`, so re-planning an unchanged board twice
/// produces the same token and a confirmation does not expire for no reason.
fn manifest_id_for(project_root: &Path, files: &[CompactionPlanFile]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(b"orgasmic-run-history-compaction/2\n");
    hasher.update(project_root.as_os_str().as_encoded_bytes());
    hasher.update(b"\n");
    for file in files {
        hasher.update(file.session_path.as_os_str().as_encoded_bytes());
        hasher.update(
            format!(
                "\n{}:{}:{}:{}:{}:{}:{}:{}\n",
                file.fingerprint.dev,
                file.fingerprint.ino,
                file.fingerprint.len,
                file.fingerprint.mtime_ns,
                file.transport.as_deref().unwrap_or(""),
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
    use std::fmt::Write as _;
    let mut out = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        let _ = write!(out, "{byte:02x}");
    }
    out
}

fn sha256_of(bytes: &[u8]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(bytes);
    hex(&hasher.finalize())
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
        complete: true,
        note: "dry run: no file was written, moved, truncated, or deleted. Pass the \
               confirm token back to execute exactly this plan; the originals are \
               archived whole and `run history rollback` restores them.",
    }
}

/// Execute `plan`, after checking the operator's confirmation against it.
///
/// `plan` must be freshly computed by the caller: the confirmation is checked
/// against the plan that is about to run, never against a stored one.
///
/// `fenced` must name every session path whose writer handle the caller has
/// closed. A planned file that is not fenced is refused.
pub fn apply_compaction(
    plan: CompactionPlan,
    confirm: Option<&str>,
    fenced: &FencedSessions,
) -> Result<CompactionReport, CompactionError> {
    apply_compaction_with(plan, confirm, fenced, &no_faults)
}

pub(crate) fn apply_compaction_with(
    plan: CompactionPlan,
    confirm: Option<&str>,
    fenced: &FencedSessions,
    fault: &FaultInjector<'_>,
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

    // Exclusion first: nothing below this line may run twice against the same
    // board, and nothing below it may run while a rollback does.
    let _lock = acquire_maintenance_lock(&plan.project_root)?;

    let archive_dir = plan
        .project_root
        .join(ARCHIVE_REL_PATH)
        .join(&plan.manifest_id);
    std::fs::create_dir_all(&archive_dir)
        .map_err(|error| CompactionError::Io(format!("create archive dir: {error}")))?;
    sync_dir_of(&archive_dir).map_err(|error| CompactionError::Io(error.to_string()))?;

    // The manifest lands BEFORE any file is touched, already naming every
    // planned file, so a transaction killed at any instant leaves a durable
    // statement of what it was doing and what it might have done.
    let mut manifest = CompactionManifest {
        manifest_format: COMPACTION_MANIFEST_FORMAT,
        manifest_id: plan.manifest_id.clone(),
        project_root: plan.project_root.clone(),
        started_at: Utc::now(),
        plan: plan.clone(),
        files: plan
            .files
            .iter()
            .map(|planned| CompactionFileJournal {
                session_path: planned.session_path.clone(),
                run_id: planned.run_id.clone(),
                stage: CompactionFileStage::Planned,
                refused: None,
            })
            .collect(),
        results: Vec::new(),
        complete: false,
    };
    write_manifest(&archive_dir, &manifest)
        .map_err(|error| CompactionError::JournalFailed(error.to_string()))?;

    let mut stopped: Option<CompactionError> = None;
    for (index, planned) in plan.files.iter().enumerate() {
        match compact_one_file(
            planned,
            &archive_dir,
            &plan.manifest_id,
            fenced,
            fault,
            &mut |stage, refused| {
                manifest.files[index].stage = stage;
                manifest.files[index].refused = refused;
                write_manifest(&archive_dir, &manifest)
            },
        ) {
            Ok(()) => {}
            Err(error) => {
                // A journal that cannot be made durable, or an injected crash,
                // stops the whole transaction. Continuing would mutate files
                // the manifest can no longer describe, which is exactly the
                // shape that made the old rollback unable to restore them.
                stopped = Some(error);
                break;
            }
        }
    }

    if stopped.is_none() {
        manifest.complete = true;
        if let Err(error) = write_manifest(&archive_dir, &manifest) {
            stopped = Some(CompactionError::JournalFailed(error.to_string()));
        }
    }
    if let Some(error) = stopped {
        return Err(error);
    }

    let results: Vec<CompactionFileResult> = manifest.files.iter().map(result_of).collect();
    let (reclaimed_records, reclaimed_bytes) =
        results
            .iter()
            .fold((0, 0), |(records, bytes), r| match &r.outcome {
                CompactionFileOutcome::Compacted {
                    reclaimed_records,
                    reclaimed_bytes,
                    ..
                } => (records + reclaimed_records, bytes + reclaimed_bytes),
                _ => (records, bytes),
            });
    Ok(CompactionReport {
        dry_run: false,
        confirm_token: plan.manifest_id.clone(),
        plan,
        results,
        reclaimed_bytes,
        reclaimed_records,
        archive_dir: Some(archive_dir),
        complete: true,
        note: "originals archived whole; each session file was replaced by an atomic \
               rename. `run history rollback --manifest <id>` restores them byte for \
               byte.",
    })
}

/// Render one journal record as the outcome an operator reads.
fn result_of(journal: &CompactionFileJournal) -> CompactionFileResult {
    let outcome = match (&journal.stage, journal.refused.as_deref()) {
        (
            CompactionFileStage::Committed {
                reclaimed_records,
                reclaimed_bytes,
                archived,
                bytes_before,
                bytes_after,
                ..
            },
            _,
        ) => CompactionFileOutcome::Compacted {
            reclaimed_records: *reclaimed_records,
            reclaimed_bytes: *reclaimed_bytes,
            archived: archived.clone(),
            bytes_before: *bytes_before,
            bytes_after: *bytes_after,
        },
        (CompactionFileStage::Planned, Some(reason)) => CompactionFileOutcome::SkippedChanged {
            reason: reason.to_string(),
        },
        (stage, refused) => CompactionFileOutcome::Failed {
            error: refused
                .unwrap_or("the transaction stopped before this file finished")
                .to_string(),
            stage: stage.label().to_string(),
        },
    };
    CompactionFileResult {
        session_path: journal.session_path.clone(),
        run_id: journal.run_id.clone(),
        outcome,
    }
}

/// Render one journal record as format 1 renders it — which is a statement
/// about what an OLDER RUNTIME must be able to recover, not about what happened.
///
/// orgasmic:TASK-FZB6T.4 finding 4c / open question 2 — [`result_of`] renders
/// `Staged` as `Failed`, and the real format-1 reader (`9bee827`) restores only
/// `Compacted` records, copying the named archive over the session path with no
/// digest check. So a crash after the rename but before the committed journal
/// write left a live REPLACEMENT that an older runtime skipped while reporting
/// success — the silent successful-empty rollback that versioning this manifest
/// was supposed to end.
///
/// Question 2's answer, recorded on TASK-FZB6T.4: the backward promise covers
/// every stage at which a v2 transaction could have MOVED the live file, which
/// is `Staged` and `Committed` — not `Planned` and not `Archived`. A `Staged`
/// record means the live path holds either the original or the replacement, and
/// the old reader's unconditional copy of the archived original is correct for
/// both, so `Staged` projects as `Compacted`. `Archived` does not project,
/// because nothing was renamed and the archive equals the live file; skipping it
/// is the truthful answer there, not a loss.
///
/// The operator-facing report keeps [`result_of`], which calls a stopped
/// transaction stopped. These two must not be the same function: one describes
/// what happened, the other describes what an older reader must do about it.
fn legacy_result_of(journal: &CompactionFileJournal) -> CompactionFileResult {
    if let CompactionFileStage::Staged {
        archived,
        reclaimed_records,
        reclaimed_bytes,
        bytes_before,
        bytes_after,
        ..
    } = &journal.stage
    {
        return CompactionFileResult {
            session_path: journal.session_path.clone(),
            run_id: journal.run_id.clone(),
            outcome: CompactionFileOutcome::Compacted {
                reclaimed_records: *reclaimed_records,
                reclaimed_bytes: *reclaimed_bytes,
                archived: archived.clone(),
                bytes_before: *bytes_before,
                bytes_after: *bytes_after,
            },
        };
    }
    result_of(journal)
}

fn write_manifest(archive_dir: &Path, manifest: &CompactionManifest) -> std::io::Result<()> {
    let path = archive_dir.join("manifest.json");
    let staged = archive_dir.join("manifest.json.tmp");
    // The format-1 rendering is derived here rather than maintained by the
    // caller, so `results` cannot drift from `files` no matter which journal
    // write produced this manifest (orgasmic:TASK-FZB6T.3 finding 2).
    let manifest = CompactionManifest {
        manifest_format: COMPACTION_MANIFEST_FORMAT,
        results: manifest.files.iter().map(legacy_result_of).collect(),
        ..manifest.clone()
    };
    let bytes = serde_json::to_vec_pretty(&manifest)
        .map_err(|error| std::io::Error::other(error.to_string()))?;
    write_and_sync(&staged, &bytes)?;
    std::fs::rename(&staged, &path)?;
    // A rename is only durable once its DIRECTORY is. Without this, a crash
    // after the rename can leave the manifest at its previous content while the
    // session files have already moved on.
    sync_dir_of(&path)
}

fn write_and_sync(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    let mut file = std::fs::File::create(path)?;
    file.write_all(bytes)?;
    file.flush()?;
    file.sync_all()
}

/// fsync the directory containing `path`, so a rename into it is durable.
fn sync_dir_of(path: &Path) -> std::io::Result<()> {
    let Some(parent) = path.parent() else {
        return Ok(());
    };
    std::fs::File::open(parent)?.sync_all()
}

/// Rewrite one session file through a durable, journalled state machine.
///
/// `journal(stage, refused)` makes the named stage durable and must be called
/// BEFORE the operation that stage authorizes. Its failure is propagated, not
/// swallowed: a transaction that cannot record what it is about to do must not
/// do it.
///
/// Order is the whole safety argument:
///
/// 1. re-verify the file identity the plan was decided against;
/// 2. re-prove, from the CURRENT bytes, that the run is terminal, that the
///    transport is a pane transport, and that the reclaimable records still
///    digest to exactly what the plan recorded;
/// 3. read the file ONCE — every later step uses those same bytes, so the
///    archive and the replacement are provably the same generation;
/// 4. archive the original, fsync it and its directory, journal `Archived`;
/// 5. stage the replacement, fsync it and its directory, journal `Staged` with
///    the replacement's digest;
/// 6. re-verify the live identity IMMEDIATELY before the commit;
/// 7. `rename` the staging file over the original, fsync the directory,
///    journal `Committed`.
///
/// A kill before (7) leaves the original in place. A kill during (7) is not
/// observable: `rename` within a directory is atomic. A kill between (7) and
/// its journal write leaves a `Staged` record that names BOTH images, which is
/// what lets rollback decide by digest instead of guessing.
#[allow(clippy::too_many_arguments)]
fn compact_one_file(
    planned: &CompactionPlanFile,
    archive_dir: &Path,
    manifest_id: &str,
    fenced: &FencedSessions,
    fault: &FaultInjector<'_>,
    journal: &mut dyn FnMut(CompactionFileStage, Option<String>) -> std::io::Result<()>,
) -> Result<(), CompactionError> {
    let path = planned.session_path.as_path();

    // Everything in this closure refuses BEFORE anything is written, so the
    // durable stage stays `Planned` and the file is untouched.
    let refuse =
        |reason: String,
         journal: &mut dyn FnMut(CompactionFileStage, Option<String>) -> std::io::Result<()>|
         -> Result<(), CompactionError> {
            journal(CompactionFileStage::Planned, Some(reason))
                .map_err(|error| CompactionError::JournalFailed(error.to_string()))
        };

    if !fenced.contains(path) {
        return refuse(
            "the session writer was not fenced for this path, so a rename would leave it \
             appending to an orphaned inode"
                .to_string(),
            journal,
        );
    }
    let metadata = match std::fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) => {
            return refuse(format!("session file is unreadable: {error}"), journal);
        }
    };
    if !metadata.is_file() || metadata.file_type().is_symlink() {
        return refuse(
            "session path is no longer a regular file".to_string(),
            journal,
        );
    }
    if SessionFileFingerprint::of(&metadata) != planned.fingerprint {
        return refuse(
            "session file changed between planning and applying".to_string(),
            journal,
        );
    }

    // Deletion authority, re-proven from the bytes about to be rewritten.
    match derive_session_authority(path, SessionScanBudget::DEFAULT) {
        Ok(authority) => {
            if !authority.terminal_is_proven() {
                return refuse(
                    "the current bytes do not prove this run ended".to_string(),
                    journal,
                );
            }
            if authority.transport != planned.transport {
                return refuse(
                    format!(
                        "the current bytes record transport {:?}, the plan was decided against \
                         {:?}",
                        authority.transport, planned.transport
                    ),
                    journal,
                );
            }
            if !authority
                .transport
                .as_deref()
                .is_some_and(transport_is_pane)
            {
                return refuse(
                    "only a pane transport's rendered output is reclaimable".to_string(),
                    journal,
                );
            }
        }
        Err(error) => {
            return refuse(
                format!("the current bytes could not be re-derived: {error}"),
                journal,
            );
        }
    }

    // ONE read. The archive, the digest comparison and the replacement are all
    // computed from these bytes, so there is no second generation to disagree.
    let original = match std::fs::read(path) {
        Ok(bytes) => bytes,
        Err(error) => return refuse(format!("read session file: {error}"), journal),
    };
    if original.len() as u64 != planned.fingerprint.len {
        return refuse(
            "session file changed between planning and applying".to_string(),
            journal,
        );
    }

    let rewrite = build_compacted(&original, planned, manifest_id);
    if rewrite.reclaimed_records != planned.reclaimable_records
        || rewrite.reclaimed_bytes != planned.reclaimable_bytes
        || rewrite.reclaimed_sha256 != planned.reclaimable_sha256
    {
        return refuse(
            format!(
                "reclaimable content changed between planning and applying: planned {} \
                 records / {} bytes / {}, found {} / {} / {}",
                planned.reclaimable_records,
                planned.reclaimable_bytes,
                planned.reclaimable_sha256,
                rewrite.reclaimed_records,
                rewrite.reclaimed_bytes,
                rewrite.reclaimed_sha256,
            ),
            journal,
        );
    }

    let Some(file_name) = path.file_name() else {
        return refuse("session path has no file name".to_string(), journal);
    };
    let archived = archive_dir.join(file_name);
    let original_sha256 = sha256_of(&original);

    if let Some(Fault::Io(error)) = fault(FaultPoint::ArchiveWrite, path) {
        return refuse(format!("archive original: {error}"), journal);
    }
    if let Err(error) = archive_original(&original, &archived) {
        return refuse(format!("archive original: {error}"), journal);
    }
    journal(
        CompactionFileStage::Archived {
            archived: archived.clone(),
            original_sha256: original_sha256.clone(),
        },
        None,
    )
    .map_err(|error| CompactionError::JournalFailed(error.to_string()))?;
    if let Some(Fault::Crash) = fault(FaultPoint::AfterArchive, path) {
        return Err(crash_error(FaultPoint::AfterArchive));
    }

    let staging = staging_path(path);
    let replacement_sha256 = sha256_of(&rewrite.bytes);
    let staged_stage = CompactionFileStage::Staged {
        archived: archived.clone(),
        original_sha256: original_sha256.clone(),
        staging: staging.clone(),
        replacement_sha256: replacement_sha256.clone(),
        reclaimed_records: rewrite.reclaimed_records,
        reclaimed_bytes: rewrite.reclaimed_bytes,
        bytes_before: planned.total_bytes,
        bytes_after: rewrite.bytes.len() as u64,
    };
    let stage_write_fault = matches!(fault(FaultPoint::StageWrite, path), Some(Fault::Io(_)));
    let staged = if stage_write_fault {
        Err(std::io::Error::other("injected full disk"))
    } else {
        write_and_sync(&staging, &rewrite.bytes).and_then(|()| sync_dir_of(&staging))
    };
    if let Err(error) = staged {
        let _ = std::fs::remove_file(&staging);
        return stop_after_archive(
            format!("stage replacement: {error}"),
            CompactionFileStage::Archived {
                archived,
                original_sha256,
            },
            journal,
        );
    }
    // The staged image's digest is durable BEFORE the rename, which is what
    // makes the rename recoverable in either direction.
    journal(staged_stage.clone(), None)
        .map_err(|error| CompactionError::JournalFailed(error.to_string()))?;
    if let Some(Fault::Crash) = fault(FaultPoint::AfterStage, path) {
        return Err(crash_error(FaultPoint::AfterStage));
    }

    // Immediately before the commit, and not one step earlier: everything above
    // took time, and an append that landed during it must not be overwritten.
    match std::fs::symlink_metadata(path) {
        Ok(now) if SessionFileFingerprint::of(&now) == planned.fingerprint => {}
        _ => {
            let _ = std::fs::remove_file(&staging);
            return stop_after_archive(
                "session file changed while the replacement was being staged".to_string(),
                staged_stage,
                journal,
            );
        }
    }

    if let Err(error) = std::fs::rename(&staging, path).and_then(|()| sync_dir_of(path)) {
        let _ = std::fs::remove_file(&staging);
        return stop_after_archive(
            format!("commit replacement: {error}"),
            staged_stage,
            journal,
        );
    }
    if let Some(Fault::Crash) = fault(FaultPoint::AfterRename, path) {
        return Err(crash_error(FaultPoint::AfterRename));
    }

    let post_fingerprint = match std::fs::symlink_metadata(path) {
        Ok(now) => SessionFileFingerprint::of(&now),
        Err(error) => {
            return stop_after_archive(
                format!("stat committed replacement: {error}"),
                staged_stage,
                journal,
            )
        }
    };
    if let Some(Fault::Io(error)) = fault(FaultPoint::ResultJournalWrite, path) {
        return Err(CompactionError::JournalFailed(error));
    }
    journal(
        CompactionFileStage::Committed {
            archived,
            original_sha256,
            replacement_sha256,
            reclaimed_records: rewrite.reclaimed_records,
            reclaimed_bytes: rewrite.reclaimed_bytes,
            bytes_before: planned.total_bytes,
            bytes_after: rewrite.bytes.len() as u64,
            post_fingerprint,
        },
        None,
    )
    .map_err(|error| CompactionError::JournalFailed(error.to_string()))
}

/// Record a failure that happened AFTER something durable was written, keeping
/// the durable stage so rollback still knows what exists on disk.
fn stop_after_archive(
    error: String,
    reached: CompactionFileStage,
    journal: &mut dyn FnMut(CompactionFileStage, Option<String>) -> std::io::Result<()>,
) -> Result<(), CompactionError> {
    journal(reached, Some(error)).map_err(|e| CompactionError::JournalFailed(e.to_string()))
}

fn staging_path(path: &Path) -> PathBuf {
    let mut name = path.as_os_str().to_os_string();
    name.push(STAGING_SUFFIX);
    PathBuf::from(name)
}

/// Write the archived original and fsync both the copy and its directory, so
/// the original bytes are durable before the live path is touched.
fn archive_original(original: &[u8], archived: &Path) -> std::io::Result<()> {
    write_and_sync(archived, original)?;
    sync_dir_of(archived)
}

struct CompactedFile {
    bytes: Vec<u8>,
    reclaimed_records: u64,
    reclaimed_bytes: u64,
    reclaimed_sha256: String,
}

/// Build the replacement content from the ALREADY-READ original bytes: every
/// non-reclaimable record verbatim, plus one summary record standing where the
/// reclaimed ones were.
///
/// Takes the bytes rather than the path (orgasmic:TASK-FZB6T.2 finding 3): the
/// old version read the file a second time, so the archive and the replacement
/// could be built from two different generations and an append landing between
/// them was silently dropped.
///
/// The summary is a `note` envelope carrying the removed byte count, the digest
/// of the removed bytes, and the archive that holds them — a truthful source
/// reference, not a claim that the content is gone. It reuses the last
/// reclaimed record's envelope header so the line belongs to the same run,
/// runtime and boot, and sorts where the removed content used to sit.
fn build_compacted(
    original: &[u8],
    planned: &CompactionPlanFile,
    manifest_id: &str,
) -> CompactedFile {
    let mut reader = std::io::BufReader::with_capacity(256 * 1024, original);
    let mut out: Vec<u8> = Vec::with_capacity(original.len());
    let mut reclaimed_records = 0_u64;
    let mut reclaimed_bytes = 0_u64;
    let mut hasher = Sha256::new();
    let mut last_reclaimed_header: Option<serde_json::Value> = None;
    let mut summary_at: Option<usize> = None;

    for record in read_history_records(&mut reader) {
        // Reading from a byte slice cannot fail; a torn record is a value, not
        // an error.
        let Ok(record) = record else { break };
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

    let reclaimed_sha256 = hex(&hasher.finalize());
    if let Some(position) = summary_at {
        let summary = summary_record(
            last_reclaimed_header.as_ref(),
            manifest_id,
            reclaimed_records,
            reclaimed_bytes,
            &reclaimed_sha256,
            &planned.session_path,
        );
        out.splice(position..position, summary);
    }

    CompactedFile {
        bytes: out,
        reclaimed_records,
        reclaimed_bytes,
        reclaimed_sha256,
    }
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
    /// The transaction that wrote this manifest ran to the end.
    #[serde(default)]
    pub source_complete: bool,
    pub restored: Vec<PathBuf>,
    /// Files that already held their original bytes, so there was nothing to
    /// restore — the ordinary shape for a transaction killed before its rename.
    #[serde(default)]
    pub already_original: Vec<PathBuf>,
    pub missing_archives: Vec<PathBuf>,
    /// Destinations that do not hold a byte image this transaction recorded.
    /// Refused, not overwritten.
    #[serde(default)]
    pub refused: BTreeMap<String, String>,
    pub failed: BTreeMap<String, String>,
}

/// One destination a rollback may act on, normalised out of either manifest
/// format (orgasmic:TASK-FZB6T.3 finding 2) and VALIDATED as part of one
/// coherent transaction (orgasmic:TASK-FZB6T.4 finding 4).
#[derive(Debug, Clone)]
struct RollbackRecord {
    session_path: PathBuf,
    archived: PathBuf,
    /// The archive's digest as recorded when it was written. `None` on a
    /// format-1 record, which recorded none — there is nothing to check the
    /// archive against, and pretending otherwise would be the false claim.
    recorded_original_sha256: Option<String>,
    /// The image this transaction left at the live path, as RECORDED. Format 2
    /// only; a format-1 record rebuilds it from the one archive generation it
    /// reads, under [`Self::legacy_plan`].
    recorded_replacement_sha256: Option<String>,
    /// The plan entry this destination correlates to one-to-one. Carried for a
    /// legacy record because rebuilding its replacement image is the only way
    /// to recognise the compacted generation at all.
    legacy_plan: Option<CompactionPlanFile>,
    /// This record came from format 1's `results`.
    legacy: bool,
}

/// One manifest decoded into exactly what a rollback is allowed to do, and
/// nothing it is not.
///
/// orgasmic:TASK-FZB6T.4 finding 4 — `read_manifest` validated the version
/// ceiling and the both-arrays-empty case, and no more. It did not require the
/// requested id, the embedded id and the plan's id to agree; it did not require
/// the manifest's project root to be the root being rolled back; it did not
/// require one journal record per planned file; and it did not confine
/// `archived` or `staging` to the places this transaction is the only writer of.
/// A crafted or cross-wired manifest could therefore name an arbitrary readable
/// file as the "original" of a session path and have it renamed into place.
///
/// Decoding happens ONCE, under the maintenance lock, into this immutable
/// value. Anything that does not correlate refuses the whole rollback rather
/// than restoring the part that happened to parse.
#[derive(Debug, Clone)]
struct RollbackPlan {
    manifest_id: String,
    /// The transaction that wrote this manifest ran to the end.
    source_complete: bool,
    records: Vec<RollbackRecord>,
}

/// Read and decode one manifest, refusing a shape this build cannot vouch for.
fn read_manifest(
    project_root: &Path,
    manifest_id: &str,
) -> Result<CompactionManifest, CompactionError> {
    let manifest_path = project_root
        .join(ARCHIVE_REL_PATH)
        .join(manifest_id)
        .join("manifest.json");
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
    // A newer runtime's manifest may name per-file state in a shape this build
    // would read as absent. Refuse rather than roll back an empty list.
    if manifest.manifest_format > COMPACTION_MANIFEST_FORMAT {
        return Err(CompactionError::ManifestUnsupportedFormat {
            manifest_id: manifest_id.to_string(),
            found: manifest.manifest_format,
            supported: COMPACTION_MANIFEST_FORMAT,
        });
    }
    // The failure this whole versioning exists to stop: a plan that names files
    // and a manifest that records none of them in any shape we can read. That
    // is not "nothing to roll back"; it is "we cannot tell", and the two must
    // never produce the same report.
    if manifest.files.is_empty() && manifest.results.is_empty() && !manifest.plan.files.is_empty() {
        return Err(CompactionError::ManifestUnrestorable {
            manifest_id: manifest_id.to_string(),
            planned: manifest.plan.files.len(),
        });
    }
    Ok(manifest)
}

/// Decode one manifest into a fully validated [`RollbackPlan`], or refuse.
///
/// Every check here answers one question: does this manifest describe ONE
/// transaction, over THIS project, whose per-file journal correlates one-to-one
/// with its own plan, naming only paths this transaction is the sole writer of?
/// Where the answer cannot be proven the whole rollback refuses — restoring the
/// records that happen to correlate would be deciding, file by file, to trust a
/// document that has already been shown to be inconsistent.
fn read_rollback_plan(
    project_root: &Path,
    manifest_id: &str,
) -> Result<RollbackPlan, CompactionError> {
    let manifest = read_manifest(project_root, manifest_id)?;
    let refuse = |reason: String| CompactionError::ManifestInconsistent {
        manifest_id: manifest_id.to_string(),
        reason,
    };

    // --- identity: three ids and two roots, all of which must be the same one.
    if manifest.manifest_id != manifest_id {
        return Err(refuse(format!(
            "it is stored as {manifest_id} but calls itself {}",
            manifest.manifest_id
        )));
    }
    if manifest.plan.manifest_id != manifest.manifest_id {
        return Err(refuse(format!(
            "its embedded plan names manifest {}",
            manifest.plan.manifest_id
        )));
    }
    if manifest.project_root != project_root || manifest.plan.project_root != project_root {
        return Err(refuse(format!(
            "it was written for {} / {}, not for {}",
            manifest.project_root.display(),
            manifest.plan.project_root.display(),
            project_root.display()
        )));
    }

    // --- destinations: inside this project's sessions directory, and distinct.
    let sessions_dir = project_sessions_dir(project_root);
    let archive_dir = project_root.join(ARCHIVE_REL_PATH).join(manifest_id);
    let mut seen: BTreeSet<&Path> = BTreeSet::new();
    for planned in &manifest.plan.files {
        if planned.session_path.parent() != Some(sessions_dir.as_path())
            || planned.session_path.file_name().is_none()
        {
            return Err(refuse(format!(
                "its plan names {}, which is not in this project's sessions directory",
                planned.session_path.display()
            )));
        }
        if !seen.insert(planned.session_path.as_path()) {
            return Err(refuse(format!(
                "its plan names {} twice",
                planned.session_path.display()
            )));
        }
    }

    // The archive a record may name is the one THIS transaction wrote for THAT
    // session file, and no other path. Exact equality rather than a prefix test:
    // the forward pass computes exactly this name, so anything else is a record
    // that was not produced by the transaction it claims to belong to.
    let archive_for = |session_path: &Path| -> Option<PathBuf> {
        Some(archive_dir.join(session_path.file_name()?))
    };

    // Which array is authoritative is decided by the DECLARED format, not by
    // which one happens to be non-empty. A format-2 manifest whose `files` is
    // empty is not a format-1 manifest that can be read from `results`; it is a
    // format-2 manifest missing its journal, and reading it the other way would
    // restore from a projection this build writes but never reads.
    let records = if manifest.manifest_format >= COMPACTION_MANIFEST_FORMAT {
        // --- format 2: one journal record per planned file, in plan order.
        if manifest.files.len() != manifest.plan.files.len() {
            return Err(refuse(format!(
                "it plans {} file(s) but journals {}",
                manifest.plan.files.len(),
                manifest.files.len()
            )));
        }
        let mut records = Vec::new();
        for (journal, planned) in manifest.files.iter().zip(&manifest.plan.files) {
            if journal.session_path != planned.session_path {
                return Err(refuse(format!(
                    "journal record {} does not correlate with planned file {}",
                    journal.session_path.display(),
                    planned.session_path.display()
                )));
            }
            if journal.run_id != planned.run_id {
                return Err(refuse(format!(
                    "journal record {} names run {} but the plan names run {}",
                    journal.session_path.display(),
                    journal.run_id,
                    planned.run_id
                )));
            }
            let (archived, original_sha256, replacement_sha256, staging) = match &journal.stage {
                // Nothing was ever written for this file.
                CompactionFileStage::Planned => continue,
                CompactionFileStage::Archived {
                    archived,
                    original_sha256,
                } => (archived, original_sha256, None, None),
                CompactionFileStage::Staged {
                    archived,
                    original_sha256,
                    replacement_sha256,
                    staging,
                    ..
                } => (
                    archived,
                    original_sha256,
                    Some(replacement_sha256.clone()),
                    Some(staging),
                ),
                CompactionFileStage::Committed {
                    archived,
                    original_sha256,
                    replacement_sha256,
                    ..
                } => (
                    archived,
                    original_sha256,
                    Some(replacement_sha256.clone()),
                    None,
                ),
            };
            if Some(archived.clone()) != archive_for(&journal.session_path) {
                return Err(refuse(format!(
                    "journal record {} names archive {}, which is not the archive this \
                     transaction writes for it",
                    journal.session_path.display(),
                    archived.display()
                )));
            }
            // The staging path is a SIBLING of the destination, not a file under
            // the manifest directory: a rename must stay within one filesystem,
            // so the forward pass stages beside the live file. Pinning it to
            // exactly that name is stricter than confining it to a directory.
            if let Some(staging) = staging {
                if staging != &staging_path(&journal.session_path) {
                    return Err(refuse(format!(
                        "journal record {} names staging file {}, which is not the one this \
                         transaction stages",
                        journal.session_path.display(),
                        staging.display()
                    )));
                }
            }
            records.push(RollbackRecord {
                session_path: journal.session_path.clone(),
                archived: archived.clone(),
                recorded_original_sha256: Some(original_sha256.clone()),
                recorded_replacement_sha256: replacement_sha256,
                legacy_plan: None,
                legacy: false,
            });
        }
        records
    } else {
        // --- format 1: `results` names only the files whose rename committed.
        // Each one must still correlate to exactly one planned file, because
        // rebuilding its replacement image — the only way to recognise the
        // compacted generation without a recorded digest — is done from that
        // plan entry.
        let mut records = Vec::new();
        let mut claimed: BTreeSet<&Path> = BTreeSet::new();
        for result in &manifest.results {
            let CompactionFileOutcome::Compacted { archived, .. } = &result.outcome else {
                continue;
            };
            let Some(planned) = manifest
                .plan
                .files
                .iter()
                .find(|planned| planned.session_path == result.session_path)
            else {
                return Err(refuse(format!(
                    "it records a result for {}, which its own plan does not name",
                    result.session_path.display()
                )));
            };
            if result.run_id != planned.run_id {
                return Err(refuse(format!(
                    "result {} names run {} but the plan names run {}",
                    result.session_path.display(),
                    result.run_id,
                    planned.run_id
                )));
            }
            if !claimed.insert(result.session_path.as_path()) {
                return Err(refuse(format!(
                    "it records {} twice",
                    result.session_path.display()
                )));
            }
            if Some(archived.clone()) != archive_for(&result.session_path) {
                return Err(refuse(format!(
                    "result {} names archive {}, which is not the archive this transaction \
                     writes for it",
                    result.session_path.display(),
                    archived.display()
                )));
            }
            records.push(RollbackRecord {
                session_path: result.session_path.clone(),
                archived: archived.clone(),
                recorded_original_sha256: None,
                recorded_replacement_sha256: None,
                legacy_plan: Some(planned.clone()),
                legacy: true,
            });
        }
        records
    };

    Ok(RollbackPlan {
        manifest_id: manifest.manifest_id.clone(),
        source_complete: manifest.complete,
        records,
    })
}

/// The one generation of an archived original a rollback decides and acts on.
///
/// orgasmic:TASK-FZB6T.4 finding 4a — the format-1 path used to hash and
/// reconstruct from an archive read at one point and then REREAD the archive
/// later to write it back. Because `recorded_original_sha256` is deliberately
/// `None` for a legacy record, the second generation was never compared against
/// the digest that authorized the decision, so bytes B could be renamed over a
/// session whose overwrite was authorized against bytes A. The archive is read
/// exactly once here and these bytes are what gets written — authorization and
/// staging cannot disagree because they are the same value.
struct ArchivedOriginal {
    bytes: Vec<u8>,
    sha256: String,
    /// The image the transaction left at the live path, rebuilt from THESE
    /// bytes for a legacy record and taken from the journal for a format-2 one.
    replacement_sha256: Option<String>,
}

fn read_archived_original(
    record: &RollbackRecord,
    manifest_id: &str,
) -> std::io::Result<ArchivedOriginal> {
    let bytes = std::fs::read(&record.archived)?;
    let sha256 = sha256_of(&bytes);
    let replacement_sha256 = match &record.legacy_plan {
        Some(planned) => Some(sha256_of(
            &build_compacted(&bytes, planned, manifest_id).bytes,
        )),
        None => record.recorded_replacement_sha256.clone(),
    };
    Ok(ArchivedOriginal {
        bytes,
        sha256,
        replacement_sha256,
    })
}

/// Every session path one manifest names, so a caller can fence the session
/// writer against them before asking for a rollback. Empty when the manifest is
/// absent, unreadable or inconsistent — [`rollback_compaction`] reports that
/// properly, and an empty fence compacts and restores nothing.
pub fn manifest_session_paths(project_root: &Path, manifest_id: &str) -> Vec<PathBuf> {
    // The PLAN, not the journal: fencing has to cover every path the
    // transaction could have touched, including the ones a format-1 manifest
    // never named because their rewrite never committed. The validated decode
    // has already proven that the journal names nothing the plan does not.
    let Ok(manifest) = read_manifest(project_root, manifest_id) else {
        return Vec::new();
    };
    if read_rollback_plan(project_root, manifest_id).is_err() {
        return Vec::new();
    }
    manifest
        .plan
        .files
        .iter()
        .map(|planned| planned.session_path.clone())
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect()
}

/// Restore every archived original recorded by `manifest_id`.
///
/// Reconstructed from the PLAN, not from a list of successes: the manifest
/// holds one journal record per planned file from its first write, so a
/// transaction killed after a rename but before its result was recorded is
/// still fully described (orgasmic:TASK-FZB6T.2 finding 2).
///
/// Every destination is proven before it is overwritten
/// (orgasmic:TASK-FZB6T.2 finding 3): the live file must hash to either the
/// replacement image this transaction staged — in which case it is restored —
/// or the original image, in which case there is nothing to do. Anything else
/// is a file somebody has written since, and it is REFUSED rather than
/// clobbered. The archive itself is verified against its recorded digest before
/// a single byte of it is written back.
///
/// Content is the authority here, not metadata: a byte-identical file whose
/// mtime or inode changed (a touch, a copy back, a restore from a backup) is
/// still the compacted generation, and refusing it would leave an operator
/// unable to recover for no safety gain. The recorded `post_fingerprint` is
/// reported for diagnosis, and the digest decides.
///
/// `fenced` is the SAME lease the forward pass requires, asserted the same way
/// (orgasmic:TASK-FZB6T.5 finding 3). Rollback renames over live session paths
/// exactly as compaction does, and its caller has to decide what to lease
/// BEFORE the maintenance lock is taken — from a manifest read outside it. So
/// the generation it leased and the generation it decodes here need not be the
/// same one: a compaction holding the lock but not yet its manifest gives a
/// rollback an EMPTY lease, and by the time the lock is free the manifest is
/// valid and names paths nothing is fencing. Restoring them then races the
/// deferred lifecycle appends the released writer lease lets through — the
/// orphaned-inode window the lease exists to close. The fence is what makes the
/// two generations one: a path the decoded plan names and the lease does not is
/// refused, untouched.
pub fn rollback_compaction(
    project_root: &Path,
    manifest_id: &str,
    fenced: &FencedSessions,
) -> Result<RollbackReport, CompactionError> {
    // Exclusion FIRST, and the decode inside it (orgasmic:TASK-FZB6T.4 finding
    // 4): the manifest used to be read, normalised and hashed before the lock
    // was taken, so a concurrent compaction could be rewriting the very archives
    // this pass was deciding against. Same exclusion as the forward pass — a
    // rollback racing a compaction is the same defect as two compactions racing.
    let _lock = acquire_maintenance_lock(project_root)?;
    let plan = read_rollback_plan(project_root, manifest_id)?;

    let mut report = RollbackReport {
        manifest_id: manifest_id.to_string(),
        project_root: project_root.to_path_buf(),
        source_complete: plan.source_complete,
        restored: Vec::new(),
        already_original: Vec::new(),
        missing_archives: Vec::new(),
        refused: BTreeMap::new(),
        failed: BTreeMap::new(),
    };
    for record in &plan.records {
        let key = record.session_path.display().to_string();

        // orgasmic:TASK-FZB6T.5 finding 3 — the fence assertion the forward pass
        // makes at `compact_one_file`, over the same lease and before anything
        // is read or written. A plan decoded under the maintenance lock may name
        // a path the lease taken before that lock does not cover.
        if !fenced.contains(&record.session_path) {
            report.refused.insert(
                key,
                "the session writer was not leased for this path, so restoring it would leave \
                 the writer appending to an orphaned inode; the manifest changed between \
                 leasing and decoding"
                    .to_string(),
            );
            continue;
        }

        // ONE read of the archive, and every decision below is made against
        // exactly these bytes and these digests — including the bytes that get
        // written back.
        let original = match read_archived_original(record, &plan.manifest_id) {
            Ok(original) => original,
            Err(_) => {
                report.missing_archives.push(record.archived.clone());
                continue;
            }
        };
        if let Some(recorded) = record.recorded_original_sha256.as_deref() {
            if original.sha256 != recorded {
                report.refused.insert(
                    key,
                    "the archived original does not match the digest recorded when it was \
                     archived; it is not restorable"
                        .to_string(),
                );
                continue;
            }
        }

        let live = match std::fs::read(&record.session_path) {
            Ok(bytes) => Some(sha256_of(&bytes)),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
            Err(error) => {
                report.failed.insert(key, error.to_string());
                continue;
            }
        };
        match live.as_deref() {
            // Already the original: a transaction killed before its rename.
            Some(digest) if digest == original.sha256 => {
                report.already_original.push(record.session_path.clone());
                continue;
            }
            // The compacted generation, and provably this transaction's.
            Some(digest) if Some(digest) == original.replacement_sha256.as_deref() => {}
            // The file is gone: restoring the archived original is exactly
            // right, and there is nothing there to clobber.
            None => {}
            Some(_) => {
                report.refused.insert(
                    key,
                    if record.legacy {
                        "the live file matches neither the archived original nor the \
                         replacement rebuilt from it; this manifest predates recorded \
                         digests, so rollback cannot prove the live bytes are this \
                         transaction's output and refuses rather than overwriting them"
                            .to_string()
                    } else {
                        "the live file holds neither the original this transaction archived \
                         nor the replacement it staged; something has written it since, so \
                         rollback refuses rather than overwriting it"
                            .to_string()
                    },
                );
                continue;
            }
        }

        let bytes = original.bytes;
        let staging = staging_path(&record.session_path);
        if let Err(error) = write_and_sync(&staging, &bytes).and_then(|()| sync_dir_of(&staging)) {
            let _ = std::fs::remove_file(&staging);
            report.failed.insert(key, error.to_string());
            continue;
        }
        if let Err(error) = std::fs::rename(&staging, &record.session_path)
            .and_then(|()| sync_dir_of(&record.session_path))
        {
            let _ = std::fs::remove_file(&staging);
            report.failed.insert(key, error.to_string());
            continue;
        }
        report.restored.push(record.session_path.clone());
    }
    Ok(report)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::run_catalog::{RunCatalog, CATALOG_REL_PATH};
    use orgasmic_core::session::{ReleaseOutcome, SessionEnvelope, SessionEventKind};
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
    /// directly (the writer now refuses them outright, which is the point of
    /// the redraw lock; this fixture is the LEGACY history that already exists
    /// on disk and is what maintenance has to deal with).
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

    /// The fence a caller proves after closing every planned path's writer
    /// handle. Tests that are not about the fence use this.
    fn fence_all(plan: &CompactionPlan) -> FencedSessions {
        FencedSessions::new(plan.files.iter().map(|file| file.session_path.clone()))
    }

    fn apply(plan: CompactionPlan, token: &str) -> Result<CompactionReport, CompactionError> {
        let fenced = fence_all(&plan);
        apply_compaction(plan, Some(token), &fenced)
    }

    /// Rollback exactly as the route drives it: the lease is taken over every
    /// path the manifest names, read BEFORE the maintenance lock, and handed to
    /// the transaction as its fence. Tests that are not about the fence use
    /// this.
    fn rollback_fenced(
        project_root: &Path,
        manifest_id: &str,
    ) -> Result<RollbackReport, CompactionError> {
        let fenced = FencedSessions::new(manifest_session_paths(project_root, manifest_id));
        rollback_compaction(project_root, manifest_id, &fenced)
    }

    fn manifest_of(board: &Board, manifest_id: &str) -> CompactionManifest {
        let path = board
            .root
            .join(ARCHIVE_REL_PATH)
            .join(manifest_id)
            .join("manifest.json");
        serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap()
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

        let fenced = fence_all(&plan);
        let refused = apply_compaction(plan.clone(), None, &fenced).unwrap_err();
        assert!(matches!(
            refused,
            CompactionError::ConfirmationRequired { .. }
        ));
        let stale = apply_compaction(plan.clone(), Some("not-the-plan"), &fenced).unwrap_err();
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

        let report = apply(plan, &token).unwrap();
        assert!(!report.dry_run);
        assert!(report.complete);
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
        let rollback = rollback_fenced(&board.root, &token).unwrap();
        assert_eq!(rollback.restored.len(), 1);
        assert!(rollback.failed.is_empty());
        assert!(rollback.refused.is_empty());
        assert!(rollback.source_complete);
        assert_eq!(std::fs::read(&path).unwrap(), original);
    }

    /// orgasmic:TASK-FZB6T.3 finding 1 — the fence was not exclusion, and this
    /// is the window it left open.
    ///
    /// `FenceSession` dropped the cached handle ONCE; the next append reopened
    /// the same path. So an append issued after the transaction's final
    /// fingerprint check and before its `rename` landed on the ORIGINAL inode —
    /// which the rename immediately orphaned. The replacement did not contain
    /// that line and the archive PREDATED it, so "rollback restores it byte for
    /// byte" was false for exactly this window. It is a real lifecycle edge: a
    /// persisted terminal driver event can make a file eligible BEFORE the
    /// supervisor appends its final `Lifecycle::Release`.
    ///
    /// The append here is SCHEDULED at that instant — issued from the
    /// transaction's own `AfterStage` boundary, which sits between the staged
    /// journal write and the pre-rename identity check — and the test does not
    /// proceed until the writer has provably taken it, so there is no sleep and
    /// no race to lose. Both halves of the required outcome are asserted:
    ///
    /// 1. with a lease held, the final lifecycle line lands in the REPLACEMENT;
    /// 2. with no lease, a write that reaches the file at the same instant makes
    ///    the transaction REFUSE, leaving the original untouched.
    #[tokio::test]
    async fn an_append_at_the_pre_rename_instant_lands_in_the_replacement() {
        use crate::writer::{SessionAppend, WriterHandle};
        use orgasmic_core::session::RuntimeIdentity;

        let held = board();
        let path = write_session(&held.sessions, "run-lease", "rmux", 12, true);
        let original = std::fs::read(&path).unwrap();
        let plan = plan_compaction(&held.root, &indexed(&held));
        let token = plan.manifest_id.clone();
        assert_eq!(plan.files.len(), 1);

        let writer: WriterHandle =
            crate::writer::spawn_with_catalog(crate::events::EventBus::new(), None);
        let lease = writer.lease_sessions(vec![path.clone()]).await.unwrap();
        let fenced = FencedSessions::new(lease.paths().to_vec());

        let (result_tx, result_rx) = tokio::sync::oneshot::channel();
        let result_tx = std::sync::Arc::new(std::sync::Mutex::new(Some(result_tx)));
        let runtime = tokio::runtime::Handle::current();
        let schedule_append = {
            let writer = writer.clone();
            let path = path.clone();
            move |point: FaultPoint, _: &Path| -> Option<Fault> {
                if point != FaultPoint::AfterStage {
                    return None;
                }
                let before = writer.deferred_session_appends();
                let append = SessionAppend {
                    run_id: "run-lease".to_string(),
                    session_path: path.clone(),
                    identity: RuntimeIdentity {
                        run_id: "run-lease".to_string(),
                        runtime_id: "runtime-run-lease".to_string(),
                        boot_id: "boot-compact".to_string(),
                    },
                    authority: None,
                    kind: SessionEventKind::Lifecycle,
                    event: json!({
                        "phase": "release",
                        "reason": "supervisor finished after the file became eligible",
                        "outcome": ReleaseOutcome::Completed,
                    }),
                };
                let issuing = writer.clone();
                let result_tx = std::sync::Arc::clone(&result_tx);
                runtime.spawn(async move {
                    let outcome = issuing.append_session(append).await;
                    if let Some(tx) = result_tx.lock().unwrap().take() {
                        let _ = tx.send(outcome.map(|ok| ok.seq));
                    }
                });
                // Do not leave this boundary until the writer has PROVABLY
                // taken the append and queued it behind the lease. That is what
                // makes this a scheduled pre-rename append rather than a hope.
                while writer.deferred_session_appends() == before {
                    std::thread::sleep(std::time::Duration::from_millis(1));
                }
                None
            }
        };

        let report = {
            let token = token.clone();
            tokio::task::spawn_blocking(move || {
                apply_compaction_with(plan, Some(&token), &fenced, &schedule_append)
            })
            .await
            .unwrap()
            .unwrap()
        };
        assert!(report.complete);
        assert!(matches!(
            report.results[0].outcome,
            CompactionFileOutcome::Compacted { .. }
        ));

        // The append is still queued: the transaction ran to completion without
        // it ever reaching the doomed inode.
        assert_eq!(writer.deferred_session_appends(), 1);
        lease.release().await;
        result_rx
            .await
            .unwrap()
            .expect("the deferred append must run once the lease is released");

        let compacted = std::fs::read_to_string(&path).unwrap();
        assert!(
            compacted.contains("orgasmic-run-history-compacted"),
            "this must be the replacement, not the orphaned original"
        );
        assert!(
            !compacted.contains("\"text_chunk\""),
            "the replacement must be the compacted generation"
        );
        assert_eq!(
            compacted
                .lines()
                .filter(|line| line.contains("\"phase\":\"release\""))
                .count(),
            2,
            "the final lifecycle line must land IN the replacement: {compacted}"
        );
        assert!(compacted.contains("supervisor finished after the file became eligible"));

        // ---- the other half: no lease, so the transaction must REFUSE. ------
        let unheld = board();
        let path = write_session(&unheld.sessions, "run-unheld", "rmux", 12, true);
        let original_unheld = std::fs::read(&path).unwrap();
        let plan = plan_compaction(&unheld.root, &indexed(&unheld));
        let token = plan.manifest_id.clone();
        let fenced = FencedSessions::new(vec![path.clone()]);
        let write_at_pre_rename = {
            let path = path.clone();
            move |point: FaultPoint, _: &Path| -> Option<Fault> {
                if point == FaultPoint::AfterStage {
                    let mut file = std::fs::OpenOptions::new()
                        .append(true)
                        .open(&path)
                        .unwrap();
                    file.write_all(b"{\"kind\":\"lifecycle\",\"event\":{\"phase\":\"release\"}}\n")
                        .unwrap();
                }
                None
            }
        };
        let report =
            apply_compaction_with(plan, Some(&token), &fenced, &write_at_pre_rename).unwrap();
        let CompactionFileOutcome::Failed { error, stage } = &report.results[0].outcome else {
            panic!(
                "an append at the pre-rename instant must refuse: {:?}",
                report.results[0].outcome
            );
        };
        assert!(
            error.contains("changed while the replacement was being staged"),
            "{error}"
        );
        assert_eq!(stage, "staged");
        assert_eq!(report.reclaimed_bytes, 0);
        let live = std::fs::read(&path).unwrap();
        assert!(
            live.starts_with(&original_unheld),
            "the original must be untouched by a refused transaction"
        );
        assert!(!original.is_empty());
    }

    /// orgasmic:TASK-FZB6T.4 finding 1 — the lease must outlive the REQUEST,
    /// not merely the await.
    ///
    /// The test above holds the lease in a local binding for the whole test, so
    /// it can never exercise this: a real handler holds it in the request
    /// future, and axum drops that future on client disconnect, route
    /// cancellation or shutdown. A started `spawn_blocking` task is NOT
    /// cancelled by dropping its join handle, so the transaction kept running
    /// while `SessionLease::drop` queued `ReleaseSessions` — reopening the
    /// writer between the transaction's final fingerprint check and its
    /// `rename`, which is precisely the orphaned-inode window the lease exists
    /// to close.
    ///
    /// So this test does the one thing the other cannot: it DROPS the caller's
    /// future while the transaction is provably parked at `AfterStage` with an
    /// append already queued behind the lease, and then requires
    ///
    /// 1. the append to stay deferred across the cancellation — the lease is
    ///    still held by an owner the caller does not have;
    /// 2. the transaction to run to its end anyway, journal and all;
    /// 3. the deferred append to land in the REPLACEMENT once that owner
    ///    releases, which is only true if it never reached the doomed inode.
    #[tokio::test]
    async fn cancelling_the_request_does_not_release_the_transaction_lease() {
        use crate::writer::{SessionAppend, WriterHandle};
        use orgasmic_core::session::RuntimeIdentity;
        use std::sync::atomic::{AtomicBool, Ordering};
        use std::sync::Arc;

        let board = board();
        let path = write_session(&board.sessions, "run-cancel", "rmux", 12, true);
        let plan = plan_compaction(&board.root, &indexed(&board));
        let token = plan.manifest_id.clone();
        assert_eq!(plan.files.len(), 1);
        let fenced = FencedSessions::new(vec![path.clone()]);

        let writer: WriterHandle =
            crate::writer::spawn_with_catalog(crate::events::EventBus::new(), None);

        // Signals across the blocking transaction thread. `parked` tells the
        // test the transaction is at `AfterStage` with its append queued;
        // `proceed` keeps it there until the test has cancelled the caller.
        let parked = Arc::new(AtomicBool::new(false));
        let proceed = Arc::new(AtomicBool::new(false));
        let (append_tx, append_rx) = tokio::sync::oneshot::channel();
        let append_tx = Arc::new(std::sync::Mutex::new(Some(append_tx)));
        let runtime = tokio::runtime::Handle::current();

        let park_with_a_queued_append = {
            let writer = writer.clone();
            let path = path.clone();
            let parked = Arc::clone(&parked);
            let proceed = Arc::clone(&proceed);
            move |point: FaultPoint, _: &Path| -> Option<Fault> {
                if point != FaultPoint::AfterStage {
                    return None;
                }
                let before = writer.deferred_session_appends();
                let append = SessionAppend {
                    run_id: "run-cancel".to_string(),
                    session_path: path.clone(),
                    identity: RuntimeIdentity {
                        run_id: "run-cancel".to_string(),
                        runtime_id: "runtime-run-cancel".to_string(),
                        boot_id: "boot-compact".to_string(),
                    },
                    authority: None,
                    kind: SessionEventKind::Lifecycle,
                    event: json!({
                        "phase": "release",
                        "reason": "supervisor finished while the client was disconnecting",
                        "outcome": ReleaseOutcome::Completed,
                    }),
                };
                let issuing = writer.clone();
                let append_tx = Arc::clone(&append_tx);
                runtime.spawn(async move {
                    let outcome = issuing.append_session(append).await;
                    if let Some(tx) = append_tx.lock().unwrap().take() {
                        let _ = tx.send(outcome.map(|ok| ok.seq));
                    }
                });
                while writer.deferred_session_appends() == before {
                    std::thread::sleep(std::time::Duration::from_millis(1));
                }
                parked.store(true, Ordering::SeqCst);
                // Hold the transaction open across the cancellation below —
                // BOUNDED. A blocking thread that spins forever survives the
                // test's panic and wedges the whole binary at runtime shutdown,
                // which turns a legible failure into a hang.
                let give_up = std::time::Instant::now() + std::time::Duration::from_secs(60);
                while !proceed.load(Ordering::SeqCst) && std::time::Instant::now() < give_up {
                    std::thread::sleep(std::time::Duration::from_millis(1));
                }
                None
            }
        };

        // `Box::pin`, deliberately, NOT `tokio::pin!`: the latter pins a value
        // that outlives the binding, so `drop` would only drop a `Pin<&mut _>`
        // and the future — and its lease — would stay alive on the stack. This
        // test is worthless unless the future itself is really destroyed.
        let mut caller = Box::pin(
            writer.with_detached_session_lease(vec![path.clone()], move || {
                apply_compaction_with(plan, Some(&token), &fenced, &park_with_a_queued_append)
            }),
        );

        // Poll the caller until the transaction is parked, then DROP it. This is
        // the client disconnect.
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
        loop {
            assert!(
                std::time::Instant::now() < deadline,
                "the transaction never reached the pre-rename boundary"
            );
            tokio::select! {
                outcome = &mut caller => panic!("the transaction returned early: {outcome:?}"),
                () = tokio::time::sleep(std::time::Duration::from_millis(2)) => {}
            }
            if parked.load(Ordering::SeqCst) {
                break;
            }
        }
        assert_eq!(
            writer.deferred_session_appends(),
            1,
            "the append must be queued behind the lease before the cancellation"
        );
        drop(caller);

        // The lease has no owner in this test's stack any more. If it were held
        // by the request future, `SessionLease::drop` has already queued
        // `ReleaseSessions` and the append is about to hit the doomed inode.
        for _ in 0..25 {
            tokio::time::sleep(std::time::Duration::from_millis(2)).await;
            assert_eq!(
                writer.deferred_session_appends(),
                1,
                "cancelling the caller must not release the transaction's lease"
            );
        }

        // Let the detached owner finish. It renames, journals, and only then
        // releases — so the queued append runs against the replacement.
        proceed.store(true, Ordering::SeqCst);
        tokio::time::timeout(std::time::Duration::from_secs(30), append_rx)
            .await
            .expect("the deferred append must run once the detached owner releases")
            .unwrap()
            .expect("the deferred append must succeed");

        let compacted = std::fs::read_to_string(&path).unwrap();
        assert!(
            compacted.contains("orgasmic-run-history-compacted"),
            "the detached transaction must have completed despite the cancellation: {compacted}"
        );
        assert!(
            !compacted.contains("\"text_chunk\""),
            "this must be the compacted generation, not the orphaned original"
        );
        assert!(
            compacted.contains("supervisor finished while the client was disconnecting"),
            "the deferred append must land IN the replacement: {compacted}"
        );

        // And the journal the manifest holds is the completed one, not a
        // transaction abandoned at `Staged`.
        let manifest = manifest_of(&board, &writer_manifest_id(&board));
        assert!(manifest.complete);
        assert_eq!(manifest.files[0].stage.label(), "committed");
    }

    /// The single manifest one board's archive directory holds.
    fn writer_manifest_id(board: &Board) -> String {
        let archive = board.root.join(ARCHIVE_REL_PATH);
        let mut ids: Vec<String> = std::fs::read_dir(&archive)
            .unwrap()
            .filter_map(|entry| {
                let entry = entry.ok()?;
                entry
                    .file_type()
                    .ok()?
                    .is_dir()
                    .then(|| entry.file_name().to_string_lossy().to_string())
            })
            .collect();
        assert_eq!(ids.len(), 1, "{ids:?}");
        ids.pop().unwrap()
    }

    /// Read the manifest one transaction wrote, as JSON.
    fn manifest_value(board: &Board, manifest_id: &str) -> serde_json::Value {
        let path = board
            .root
            .join(ARCHIVE_REL_PATH)
            .join(manifest_id)
            .join("manifest.json");
        serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap()
    }

    fn write_manifest_value(board: &Board, manifest_id: &str, value: &serde_json::Value) {
        let path = board
            .root
            .join(ARCHIVE_REL_PATH)
            .join(manifest_id)
            .join("manifest.json");
        std::fs::write(path, serde_json::to_vec_pretty(value).unwrap()).unwrap();
    }

    /// orgasmic:TASK-FZB6T.3 finding 2 — the manifest format changed and broke
    /// rollback in BOTH directions.
    ///
    /// TASK-FZB6T.2 replaced the durable manifest's `results` array with a
    /// `files` array, with no format version and no compatibility decoder. Both
    /// arrays are `#[serde(default)]`, so each runtime read the other's manifest
    /// as an EMPTY list of files, restored nothing, and reported SUCCESS. For
    /// the one mechanism whose entire justification is that deletion is
    /// recoverable, a silent successful-empty rollback is the worst available
    /// outcome.
    ///
    /// Both directions, one test:
    ///
    /// - FORWARD: a format-1 manifest — no version, no `files`, only `results` —
    ///   is decoded and actually restores the original bytes;
    /// - BACKWARD: the manifest this build writes carries `results` too, so a
    ///   runtime that only knows that shape finds the archived originals rather
    ///   than an empty list. The old shape is deserialized here from the real
    ///   bytes on disk, which is the only version of this claim that stays true.
    #[test]
    fn a_manifest_rolls_back_across_the_format_change_in_both_directions() {
        // The format-1 reader, exactly as TASK-FZB6T.1 declared it.
        #[derive(Deserialize)]
        struct Format1Manifest {
            manifest_id: String,
            #[serde(default)]
            results: Vec<CompactionFileResult>,
        }

        // ---- BACKWARD: an older runtime reads what this build writes. -------
        let board = board();
        let path = write_session(&board.sessions, "run-compat", "rmux", 24, true);
        let original = std::fs::read(&path).unwrap();
        let plan = plan_compaction(&board.root, &indexed(&board));
        let token = plan.manifest_id.clone();
        apply(plan, &token).unwrap();

        let written = manifest_value(&board, &token);
        assert_eq!(
            written["manifest_format"].as_u64(),
            Some(u64::from(COMPACTION_MANIFEST_FORMAT)),
            "the manifest must state its own format"
        );
        let as_format_1: Format1Manifest =
            serde_json::from_value(written.clone()).expect("format 1 must still decode this");
        assert_eq!(as_format_1.manifest_id, token);
        let archived: Vec<PathBuf> = as_format_1
            .results
            .iter()
            .filter_map(|result| match &result.outcome {
                CompactionFileOutcome::Compacted { archived, .. } => Some(archived.clone()),
                _ => None,
            })
            .collect();
        assert_eq!(
            archived.len(),
            1,
            "a runtime that only reads `results` must still find the archived original, not \
             an empty list it would report as a successful rollback of nothing"
        );
        assert_eq!(std::fs::read(&archived[0]).unwrap(), original);

        // ---- FORWARD: this build reads a format-1 manifest. ------------------
        // The exact shape TASK-FZB6T.1 wrote: no `manifest_format`, no `files`,
        // no per-file digests, `Failed` with no `stage`.
        let mut legacy = written.clone();
        let object = legacy.as_object_mut().unwrap();
        object.remove("manifest_format");
        object.remove("files");
        object.remove("complete");
        for result in object["results"].as_array_mut().unwrap() {
            if let Some(failed) = result.as_object_mut() {
                failed.remove("stage");
            }
        }
        write_manifest_value(&board, &token, &legacy);

        let compacted = std::fs::read(&path).unwrap();
        assert_ne!(
            compacted, original,
            "the fixture must actually be compacted"
        );
        let rollback = rollback_fenced(&board.root, &token).unwrap();
        assert_eq!(
            rollback.restored.len(),
            1,
            "a format-1 manifest must restore its archived original, not report an empty \
             success: {rollback:?}"
        );
        assert!(rollback.refused.is_empty(), "{rollback:?}");
        assert!(rollback.failed.is_empty(), "{rollback:?}");
        assert_eq!(std::fs::read(&path).unwrap(), original);

        // And the fail-closed half of the legacy path: a format-1 record carries
        // no digests, so a live file that is neither the archive nor the
        // replacement rebuilt from it is REFUSED, not clobbered.
        std::fs::write(&path, b"{\"kind\":\"note\",\"event\":{}}\n").unwrap();
        let refused = rollback_fenced(&board.root, &token).unwrap();
        assert!(refused.restored.is_empty(), "{refused:?}");
        assert_eq!(refused.refused.len(), 1, "{refused:?}");
    }

    /// The format-1 reader, byte-for-byte as `9bee827` declared it: the fields
    /// that commit's `CompactionManifest`, `CompactionFileResult` and
    /// `CompactionFileOutcome::Compacted` actually had, and no others.
    #[derive(Deserialize)]
    struct Nine9Bee827Manifest {
        manifest_id: String,
        project_root: PathBuf,
        #[serde(default)]
        results: Vec<Nine9Bee827Result>,
    }

    #[derive(Deserialize)]
    struct Nine9Bee827Result {
        session_path: PathBuf,
        #[allow(dead_code)]
        run_id: String,
        #[serde(flatten)]
        outcome: Nine9Bee827Outcome,
    }

    #[derive(Deserialize)]
    #[serde(tag = "outcome", rename_all = "snake_case")]
    enum Nine9Bee827Outcome {
        Compacted {
            archived: PathBuf,
            #[allow(dead_code)]
            reclaimed_records: u64,
            #[allow(dead_code)]
            reclaimed_bytes: u64,
            #[allow(dead_code)]
            bytes_before: u64,
            #[allow(dead_code)]
            bytes_after: u64,
        },
        SkippedChanged {
            #[allow(dead_code)]
            reason: String,
        },
        Failed {
            #[allow(dead_code)]
            error: String,
        },
    }

    /// `9bee827`'s rollback, reproduced: for every `Compacted` result whose
    /// destination is inside the sessions directory, copy the named archive over
    /// the session path. No digests, no stage, no second live-file check —
    /// that reader had none of those, which is exactly why what this build
    /// PROJECTS into `results` has to be correct for it.
    fn rollback_as_9bee827(manifest: &Nine9Bee827Manifest) -> Vec<PathBuf> {
        let sessions_dir = project_sessions_dir(&manifest.project_root);
        let mut restored = Vec::new();
        for result in &manifest.results {
            let Nine9Bee827Outcome::Compacted { archived, .. } = &result.outcome else {
                continue;
            };
            if result.session_path.parent() != Some(sessions_dir.as_path()) {
                continue;
            }
            let Ok(bytes) = std::fs::read(archived) else {
                continue;
            };
            let staging = staging_path(&result.session_path);
            std::fs::write(&staging, &bytes).unwrap();
            std::fs::rename(&staging, &result.session_path).unwrap();
            restored.push(result.session_path.clone());
        }
        restored
    }

    /// orgasmic:TASK-FZB6T.4 finding 4c / open question 2 — the backward promise
    /// has to cover the crash state, not just the completed one.
    ///
    /// A crash after the `rename` but before the committed journal write leaves
    /// a `Staged` record and a LIVE REPLACEMENT. `result_of` renders `Staged` as
    /// `Failed`, and the real `9bee827` reader restores only `Compacted`
    /// records — so that older runtime skipped the one file whose live path had
    /// actually moved, and reported success. This drives the actual old reader
    /// over the actual emitted bytes at the actual crash state.
    ///
    /// The answer recorded for question 2: the promise covers every stage at
    /// which the transaction could have MOVED the live file — `Staged` and
    /// `Committed` — and not `Planned` or `Archived`, where the live path still
    /// holds the original and skipping is correct.
    #[test]
    fn an_older_runtime_recovers_the_after_rename_crash_state() {
        for point in [
            FaultPoint::AfterArchive,
            FaultPoint::AfterStage,
            FaultPoint::AfterRename,
        ] {
            let board = board();
            let path = write_session(&board.sessions, "run-crash", "rmux", 16, true);
            let original = std::fs::read(&path).unwrap();
            let plan = plan_compaction(&board.root, &indexed(&board));
            let token = plan.manifest_id.clone();
            let fenced = fence_all(&plan);
            apply_compaction_with(plan, Some(&token), &fenced, &fault_at(point, Fault::Crash))
                .unwrap_err();

            let stage = manifest_of(&board, &token).files[0].stage.label();
            assert_eq!(
                stage,
                match point {
                    FaultPoint::AfterArchive => "archived",
                    _ => "staged",
                },
                "{point:?}"
            );
            let live_moved = std::fs::read(&path).unwrap() != original;
            assert_eq!(live_moved, point == FaultPoint::AfterRename, "{point:?}");

            // The old reader, over the real emitted bytes.
            let legacy: Nine9Bee827Manifest =
                serde_json::from_value(manifest_value(&board, &token)).expect("format 1 decodes");
            assert_eq!(legacy.manifest_id, token);
            let restorable = legacy
                .results
                .iter()
                .filter(|result| matches!(result.outcome, Nine9Bee827Outcome::Compacted { .. }))
                .count();
            match stage {
                // Nothing was renamed: the live path holds the original and the
                // archive equals it, so an old reader has nothing to do.
                "archived" => assert_eq!(restorable, 0, "{point:?}"),
                // The live path may hold EITHER image, and copying the archive
                // over it is correct for both. It must be offered.
                _ => assert_eq!(
                    restorable, 1,
                    "{point:?}: a crash that may have moved the live file must be restorable by \
                     a runtime that predates `files`"
                ),
            }

            let restored = rollback_as_9bee827(&legacy);
            assert_eq!(restored.len(), restorable, "{point:?}");
            assert_eq!(
                std::fs::read(&path).unwrap(),
                original,
                "{point:?}: an older runtime must land on the original, not on a live \
                 replacement it skipped while reporting success"
            );

            // And this build lands there too, from the same bytes.
            let rollback = rollback_fenced(&board.root, &token).unwrap();
            assert!(rollback.failed.is_empty(), "{point:?}: {rollback:?}");
            assert!(rollback.refused.is_empty(), "{point:?}: {rollback:?}");
            assert!(!rollback.source_complete, "{point:?}");
            assert_eq!(std::fs::read(&path).unwrap(), original, "{point:?}");
        }
    }

    /// orgasmic:TASK-FZB6T.4 finding 4a — the decode happens UNDER the lock.
    ///
    /// `rollback_compaction` used to read the manifest, normalise it and hash
    /// every archive BEFORE it asked for the maintenance lock. Everything it
    /// decided in that prologue was decided against a board another maintenance
    /// transaction was free to be rewriting. This pins the ordering the only way
    /// a test can observe it: with the lock held, a rollback must report the
    /// LOCK, not the manifest — which is only possible if it looked at the lock
    /// first.
    #[test]
    fn a_rollback_decodes_nothing_before_it_holds_the_lock() {
        let board = board();
        write_session(&board.sessions, "run-a", "rmux", 8, true);
        let plan = plan_compaction(&board.root, &indexed(&board));
        let token = plan.manifest_id.clone();
        apply(plan, &token).unwrap();

        let held = acquire_maintenance_lock(&board.root).unwrap();
        for manifest_id in [token.as_str(), "deadbeef"] {
            let error = rollback_fenced(&board.root, manifest_id).unwrap_err();
            assert!(
                matches!(error, CompactionError::MaintenanceBusy { .. }),
                "{manifest_id}: the lock must be taken before the manifest is read: {error:?}"
            );
        }
        drop(held);
        assert!(rollback_fenced(&board.root, &token).is_ok());
    }

    /// orgasmic:TASK-FZB6T.4 finding 4b — a manifest that disagrees with itself
    /// refuses as a whole, before a single byte is written anywhere.
    ///
    /// `read_manifest` checked the format ceiling and the both-arrays-empty
    /// case, and nothing else: not that the requested id, the embedded id and
    /// the plan's id agree, not that the manifest was written for the root being
    /// rolled back, not that there is one journal record per planned file, and
    /// not that `archived` and `staging` name the paths this transaction is the
    /// only writer of. Each case below is a document a hand edit or a crossed
    /// wire produces, and each one used to be acted on.
    #[test]
    fn a_manifest_that_disagrees_with_itself_is_refused_whole() {
        type Damage = fn(&mut serde_json::Value, &Path);
        let cases: Vec<(&str, Damage)> = vec![
            ("the embedded id", |manifest, _| {
                manifest["manifest_id"] = json!("not-the-id-it-is-stored-under");
            }),
            ("the plan's id", |manifest, _| {
                manifest["plan"]["manifest_id"] = json!("some-other-transaction");
            }),
            ("the project root", |manifest, _| {
                manifest["project_root"] = json!("/somewhere/else");
            }),
            ("the plan's root", |manifest, _| {
                manifest["plan"]["project_root"] = json!("/somewhere/else");
            }),
            ("a journal record per planned file", |manifest, _| {
                manifest["files"].as_array_mut().unwrap().clear();
            }),
            ("the run id", |manifest, _| {
                manifest["files"][0]["run_id"] = json!("run-somebody-else");
            }),
            ("archive confinement", |manifest, root| {
                manifest["files"][0]["archived"] = json!(root.join("outside.jsonl"));
            }),
            ("staging confinement", |manifest, root| {
                manifest["files"][0]["staging"] = json!(root.join("outside.tmp"));
            }),
        ];

        for (what, damage) in cases {
            let board = board();
            let path = write_session(&board.sessions, "run-a", "rmux", 12, true);
            let plan = plan_compaction(&board.root, &indexed(&board));
            let token = plan.manifest_id.clone();
            let fenced = fence_all(&plan);
            // A crash at `AfterStage` so the journal carries a `staging` field
            // for the confinement case; every other case is unaffected by it.
            let _ = apply_compaction_with(
                plan,
                Some(&token),
                &fenced,
                &fault_at(FaultPoint::AfterStage, Fault::Crash),
            );
            let live = std::fs::read(&path).unwrap();

            let mut manifest = manifest_value(&board, &token);
            damage(&mut manifest, &board.root);
            write_manifest_value(&board, &token, &manifest);

            let error = rollback_fenced(&board.root, &token).unwrap_err();
            assert!(
                matches!(
                    error,
                    CompactionError::ManifestInconsistent { .. }
                        | CompactionError::ManifestUnrestorable { .. }
                ),
                "{what}: an incoherent manifest must refuse, not restore: {error:?}"
            );
            assert_eq!(
                std::fs::read(&path).unwrap(),
                live,
                "{what}: nothing may be written on the strength of a refused manifest"
            );
            assert!(
                manifest_session_paths(&board.root, &token).is_empty(),
                "{what}: and nothing may be fenced on it either"
            );
        }
    }

    /// orgasmic:TASK-FZB6T.4 finding 4a — a legacy rollback is bound to ONE
    /// generation of the archive it read.
    ///
    /// The manifest here is authored in `9bee827`'s shape field by field from
    /// that commit's struct definitions — not a completed v2 manifest with keys
    /// deleted, which is what the round-3 test did and what the reviewer
    /// correctly refused to accept as evidence.
    ///
    /// Because `recorded_original_sha256` is deliberately `None` for a legacy
    /// record, the archive was the ONLY authority, and it used to be read twice:
    /// once to derive the digests the decision was made against, and again to
    /// produce the bytes that were renamed over the session file. Nothing
    /// compared the two, so bytes B could be written over a file whose overwrite
    /// was authorized against bytes A. The archive is read once now, and the
    /// second half of this test proves it by swapping the archive between the
    /// two reads: the swap must either be invisible or be refused, and never be
    /// written.
    #[test]
    fn a_real_format_1_manifest_restores_the_generation_it_authorized() {
        let board = board();
        let path = write_session(&board.sessions, "run-legacy", "rmux", 24, true);
        let original = std::fs::read(&path).unwrap();
        let plan = plan_compaction(&board.root, &indexed(&board));
        let token = plan.manifest_id.clone();
        let planned = plan.files[0].clone();
        let report = apply(plan.clone(), &token).unwrap();
        let CompactionFileOutcome::Compacted {
            archived,
            reclaimed_records,
            reclaimed_bytes,
            bytes_before,
            bytes_after,
        } = report.results[0].outcome.clone()
        else {
            panic!("expected a compaction: {:?}", report.results[0]);
        };
        let compacted = std::fs::read(&path).unwrap();
        assert_ne!(compacted, original);

        // The `9bee827` document, authored from that commit's field lists.
        // `CompactionManifest`: manifest_id, project_root, started_at, plan,
        // results. `CompactionPlan`: manifest_id, project_root, planned_at,
        // files, reclaimable_bytes, reclaimable_records, candidates_considered,
        // skipped_not_terminal, skipped_unreadable — and nothing else; the two
        // `skipped_*` counters this build added did not exist.
        let legacy = json!({
            "manifest_id": token,
            "project_root": board.root,
            "started_at": "2026-08-02T17:52:39Z",
            "plan": {
                "manifest_id": token,
                "project_root": board.root,
                "planned_at": "2026-08-02T17:52:39Z",
                "files": [{
                    "session_path": planned.session_path,
                    "run_id": planned.run_id,
                    "driver": planned.driver,
                    "transport": planned.transport,
                    "fingerprint": planned.fingerprint,
                    "total_bytes": planned.total_bytes,
                    "reclaimable_records": planned.reclaimable_records,
                    "reclaimable_bytes": planned.reclaimable_bytes,
                    "reclaimable_sha256": planned.reclaimable_sha256,
                }],
                "reclaimable_bytes": plan.reclaimable_bytes,
                "reclaimable_records": plan.reclaimable_records,
                "candidates_considered": plan.candidates_considered,
                "skipped_not_terminal": plan.skipped_not_terminal,
                "skipped_unreadable": plan.skipped_unreadable,
            },
            "results": [{
                "session_path": planned.session_path,
                "run_id": planned.run_id,
                "outcome": "compacted",
                "reclaimed_records": reclaimed_records,
                "reclaimed_bytes": reclaimed_bytes,
                "archived": archived,
                "bytes_before": bytes_before,
                "bytes_after": bytes_after,
            }],
        });
        // The fixture is genuinely the old shape, not the new one undressed.
        let object = legacy.as_object().unwrap();
        assert!(!object.contains_key("manifest_format"));
        assert!(!object.contains_key("files"));
        assert!(!object.contains_key("complete"));
        assert!(!legacy["plan"]
            .as_object()
            .unwrap()
            .contains_key("skipped_unstable"));
        write_manifest_value(&board, &token, &legacy);

        let rollback = rollback_fenced(&board.root, &token).unwrap();
        assert_eq!(rollback.restored.len(), 1, "{rollback:?}");
        assert!(rollback.refused.is_empty(), "{rollback:?}");
        assert!(rollback.failed.is_empty(), "{rollback:?}");
        assert!(
            !rollback.source_complete,
            "a format-1 manifest states no completion, and inventing one would be a claim"
        );
        assert_eq!(std::fs::read(&path).unwrap(), original);

        // ---- one generation: swap the archive, and nothing else. -------------
        // Put the live file back to the compacted image so there is something to
        // roll back, then replace the archive with DIFFERENT bytes that are
        // still a valid session file. A rollback that authorized against one
        // generation and wrote another would land these bytes on the session
        // path; a single-read rollback cannot, because the digest it decided
        // with and the bytes it writes are the same value.
        std::fs::write(&path, &compacted).unwrap();
        let mut tampered = original.clone();
        tampered.extend_from_slice(b"{\"seq\":9999,\"kind\":\"note\",\"event\":{}}\n");
        std::fs::write(&archived, &tampered).unwrap();

        let after = rollback_fenced(&board.root, &token).unwrap();
        assert!(
            after.restored.is_empty(),
            "the live file is not the replacement rebuilt from THESE archive bytes, so the \
             rollback must refuse: {after:?}"
        );
        assert_eq!(after.refused.len(), 1, "{after:?}");
        assert_eq!(
            std::fs::read(&path).unwrap(),
            compacted,
            "a refused destination must be byte-identical afterwards"
        );
    }

    /// orgasmic:TASK-FZB6T.3 finding 2 — a manifest this build cannot read is
    /// refused LOUDLY. It is never reported as a rollback that restored nothing,
    /// because "there was nothing to restore" and "I cannot tell what there was"
    /// are different facts and only one of them is safe to act on.
    #[test]
    fn a_manifest_this_build_cannot_read_is_refused_not_reported_as_an_empty_success() {
        let board = board();
        let path = write_session(&board.sessions, "run-future", "rmux", 8, true);
        let plan = plan_compaction(&board.root, &indexed(&board));
        let token = plan.manifest_id.clone();
        let original = std::fs::read(&path).unwrap();
        apply(plan, &token).unwrap();

        // A manifest from a runtime this build does not know. Its per-file state
        // may live in a field this build would silently read as absent.
        let written = manifest_value(&board, &token);
        let mut future = written.clone();
        future["manifest_format"] = serde_json::json!(COMPACTION_MANIFEST_FORMAT + 1);
        write_manifest_value(&board, &token, &future);
        let error = rollback_fenced(&board.root, &token).unwrap_err();
        assert!(
            matches!(
                error,
                CompactionError::ManifestUnsupportedFormat { found, supported, .. }
                    if found == COMPACTION_MANIFEST_FORMAT + 1
                        && supported == COMPACTION_MANIFEST_FORMAT
            ),
            "a newer manifest must be refused, not read: {error}"
        );

        // A manifest that plans files and records none of them in any readable
        // shape — which is EXACTLY what each runtime saw of the other's manifest
        // before this fix, and exactly what it called a successful rollback.
        let mut blind = written.clone();
        let object = blind.as_object_mut().unwrap();
        object.remove("files");
        object.remove("results");
        write_manifest_value(&board, &token, &blind);
        let error = rollback_fenced(&board.root, &token).unwrap_err();
        assert!(
            matches!(
                error,
                CompactionError::ManifestUnrestorable { planned, .. } if planned == 1
            ),
            "a plan with files and no readable per-file record must fail loudly: {error}"
        );

        // Neither refusal touched the live file, and the real manifest still
        // rolls back once it is restored.
        assert_ne!(std::fs::read(&path).unwrap(), original);
        write_manifest_value(&board, &token, &written);
        assert_eq!(
            rollback_fenced(&board.root, &token).unwrap().restored.len(),
            1
        );
        assert_eq!(std::fs::read(&path).unwrap(), original);
    }

    /// orgasmic:TASK-FZB6T.3 finding 3 — a record whose validity this build
    /// cannot prove survives compaction BYTE FOR BYTE.
    ///
    /// The malformed line sits in the middle of an otherwise ordinary terminal
    /// pane session, which is where the reviewer's case actually lives: a legacy
    /// record in the skipped middle of a file whose ends both classify cleanly.
    /// It must be neither counted as reclaimable nor rewritten.
    #[test]
    fn an_invalid_record_survives_compaction_byte_for_byte() {
        let board = board();
        let path = write_session(&board.sessions, "run-invalid", "rmux", 6, true);
        let source = std::fs::read_to_string(&path).unwrap();
        let mut lines: Vec<String> = source.lines().map(str::to_string).collect();

        // A `text_chunk` envelope carrying a truncated `true` literal: invalid
        // JSON wearing the exact shape the maintenance pass reclaims.
        let malformed = r#"{"seq":99,"time":"2026-08-03T00:00:00Z","run_id":"run-invalid","runtime_id":"runtime-run-invalid","boot_id":"boot-compact","kind":"driver_event","event":{"type":"text_chunk","stream":"stdout","chunk":"x","final":truX}}"#;
        assert!(serde_json::from_str::<serde_json::Value>(malformed).is_err());
        lines.insert(lines.len() / 2, malformed.to_string());
        let rebuilt = format!("{}\n", lines.join("\n"));
        std::fs::write(&path, &rebuilt).unwrap();

        let plan = plan_compaction(&board.root, &indexed(&board));
        let token = plan.manifest_id.clone();
        assert_eq!(plan.files.len(), 1);
        assert_eq!(
            plan.files[0].reclaimable_records, 6,
            "only the six PROVEN rendered payloads are reclaimable; the malformed record is \
             not one of them"
        );

        let report = apply(plan, &token).unwrap();
        assert!(report.complete);
        let compacted = std::fs::read_to_string(&path).unwrap();
        assert!(
            compacted.contains(malformed),
            "a record this accounting could not prove valid must survive verbatim"
        );
        assert!(!compacted.contains("\"stream\":\"stdout\",\"chunk\":\"\\u001b"));
        // And the retained bytes are the original's, in order, minus exactly the
        // provable payloads.
        let kept: Vec<&str> = compacted
            .lines()
            .filter(|line| !line.contains("orgasmic-run-history-compacted"))
            .collect();
        let expected: Vec<&str> = rebuilt
            .lines()
            .filter(|line| !line.contains("\"text_chunk\"") || line.contains("truX"))
            .collect();
        assert_eq!(kept, expected);
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

    /// dec_BBPW4 / finding 4 — the catalog is a candidate list, never deletion
    /// authority. A semantically corrupt entry claiming a LIVE ACP run is a
    /// terminal rmux one authorizes nothing: the plan re-derives both facts from
    /// the file's current bytes and refuses.
    #[test]
    fn a_corrupt_catalog_entry_cannot_authorize_a_deletion() {
        let board = board();
        // Live (no release) and ACP: two independent reasons to refuse.
        let path = write_session(&board.sessions, "run-acp", "acp-stdio", 40, false);
        let before = std::fs::read(&path).unwrap();

        let mut entries = indexed(&board);
        assert_eq!(entries.len(), 1);
        // The exact corruption the reviewer describes: valid JSON, right path,
        // right fingerprint, lying semantics.
        entries[0].transport = Some("rmux".to_string());
        entries[0].terminal = Some(crate::run_catalog::TerminalRecord::DriverEvent {
            event: "run_complete".to_string(),
            at: Utc::now(),
        });
        assert!(entries[0].is_terminal());

        let plan = plan_compaction(&board.root, &entries);
        assert!(
            plan.is_empty(),
            "a catalog entry must never be deletion authority: {:?}",
            plan.files
        );
        assert_eq!(plan.candidates_considered, 1);
        assert_eq!(plan.skipped_not_terminal, 1);
        assert_eq!(std::fs::read(&path).unwrap(), before);
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

        let report = apply(plan, &token).unwrap();
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

    /// finding 3 — the plan's DIGEST is compared, not just its counts. A file
    /// edited to hold different reclaimable records of the same count and the
    /// same total size is refused.
    #[test]
    fn compaction_compares_the_planned_digest_not_just_the_counts() {
        let board = board();
        let path = write_session(&board.sessions, "run-a", "rmux", 8, true);
        let plan = plan_compaction(&board.root, &indexed(&board));
        let token = plan.manifest_id.clone();

        // Swap one reclaimable record's payload for a different one of exactly
        // the same length, and restore the original file identity so nothing
        // upstream of the digest can notice.
        let original = std::fs::read(&path).unwrap();
        let source = String::from_utf8(original.clone()).unwrap();
        let victim = source
            .lines()
            .find(|line| line.contains("\"text_chunk\""))
            .unwrap()
            .to_string();
        let swapped = victim.replacen("redraw ", "REDRAW ", 1);
        assert_eq!(swapped.len(), victim.len());
        assert_ne!(swapped, victim);
        std::fs::write(&path, source.replacen(&victim, &swapped, 1)).unwrap();
        // Same length, and the fingerprint's mtime is restored so the identity
        // check cannot be what refuses this.
        let mut mutated_plan = plan.clone();
        mutated_plan.files[0].fingerprint =
            crate::run_catalog::SessionFileFingerprint::of(&std::fs::metadata(&path).unwrap());
        let mutated_bytes = std::fs::read(&path).unwrap();
        assert_eq!(mutated_bytes.len(), original.len());

        let fenced = fence_all(&mutated_plan);
        let report = apply_compaction(mutated_plan, Some(&token), &fenced);
        // The plan's manifest id no longer matches (the digest is part of it),
        // so the confirmation itself is refused — and if a caller forces the
        // fingerprint through, the per-file digest comparison refuses too.
        match report {
            Err(CompactionError::ConfirmationStale { .. }) => {}
            Ok(report) => {
                let CompactionFileOutcome::SkippedChanged { reason } = &report.results[0].outcome
                else {
                    panic!("digest mismatch must refuse: {:?}", report.results[0]);
                };
                assert!(reason.contains("reclaimable content changed"), "{reason}");
            }
            other => panic!("unexpected outcome: {other:?}"),
        }
        assert_eq!(
            std::fs::read(&path).unwrap(),
            mutated_bytes,
            "a refused file must be byte-identical afterwards"
        );
    }

    /// finding 3 — a planned file whose session writer was not fenced is
    /// refused, because renaming over a held-open path orphans the inode.
    #[test]
    fn an_unfenced_session_is_never_rewritten() {
        let board = board();
        let path = write_session(&board.sessions, "run-a", "rmux", 16, true);
        let before = std::fs::read(&path).unwrap();
        let plan = plan_compaction(&board.root, &indexed(&board));
        let token = plan.manifest_id.clone();

        let report = apply_compaction(plan, Some(&token), &FencedSessions::default()).unwrap();
        let CompactionFileOutcome::SkippedChanged { reason } = &report.results[0].outcome else {
            panic!("an unfenced path must be refused: {:?}", report.results[0]);
        };
        assert!(reason.contains("not fenced"), "{reason}");
        assert_eq!(std::fs::read(&path).unwrap(), before);
        assert_eq!(report.reclaimed_bytes, 0);
    }

    /// orgasmic:TASK-FZB6T.5 finding 3 — rollback leased one manifest
    /// generation and executed another.
    ///
    /// The route reads the manifest to decide what to lease, and
    /// `rollback_compaction` decodes it again after taking the maintenance
    /// lock. Those are two instants with a lock wait between them, and
    /// `manifest_session_paths` maps an absent, unreadable or inconsistent
    /// manifest to an EMPTY path set. So the sequence the reviewer names —
    /// compaction holds the lock with its manifest unwritten, rollback sees
    /// nothing and leases nothing, compaction commits and releases, rollback
    /// then decodes a valid manifest — put rollback in the middle of renaming
    /// live session paths with no writer lease covering any of them, which is
    /// exactly the orphaned-inode window the lease exists to close.
    ///
    /// Reproduced without the race: a lease that does not cover the decoded
    /// plan is the whole defect, whatever produced it. Forward compaction has
    /// refused this since TASK-FZB6T.2 finding 3
    /// (`an_unfenced_session_is_never_rewritten`); this is the same assertion
    /// over the same lease on the way back.
    #[test]
    fn an_unleased_session_is_never_restored_by_rollback() {
        let board = board();
        let path = write_session(&board.sessions, "run-a", "rmux", 16, true);
        let original = std::fs::read(&path).unwrap();
        let plan = plan_compaction(&board.root, &indexed(&board));
        let token = plan.manifest_id.clone();
        apply(plan, &token).unwrap();
        let compacted = std::fs::read(&path).unwrap();
        assert_ne!(compacted, original, "the fixture must have been rewritten");

        // The empty lease the race produces: the paths were read at an instant
        // when the manifest did not answer, and the plan decoded under the lock
        // names one anyway.
        let report = rollback_compaction(&board.root, &token, &FencedSessions::default()).unwrap();
        let reason = report
            .refused
            .get(&path.display().to_string())
            .expect("an unleased path must be refused");
        assert!(reason.contains("not leased"), "{reason}");
        assert!(report.restored.is_empty());
        assert_eq!(
            std::fs::read(&path).unwrap(),
            compacted,
            "a refused rollback must not have touched the live file"
        );

        // And with the lease the route actually takes, the same rollback
        // restores — the fence refuses a mismatch, it does not break rollback.
        let report = rollback_fenced(&board.root, &token).unwrap();
        assert_eq!(report.restored, vec![path.clone()]);
        assert!(report.refused.is_empty());
        assert_eq!(std::fs::read(&path).unwrap(), original);
    }

    /// finding 3 — maintenance is exclusive per project. A second transaction
    /// while one holds the lock is refused, not interleaved.
    #[test]
    fn maintenance_is_exclusive_per_project() {
        let board = board();
        write_session(&board.sessions, "run-a", "rmux", 8, true);
        let held = acquire_maintenance_lock(&board.root).unwrap();

        let plan = plan_compaction(&board.root, &indexed(&board));
        let token = plan.manifest_id.clone();
        let refused = apply(plan, &token).unwrap_err();
        assert!(
            matches!(refused, CompactionError::MaintenanceBusy { .. }),
            "{refused:?}"
        );
        let refused = rollback_fenced(&board.root, &token).unwrap_err();
        assert!(
            matches!(
                refused,
                CompactionError::MaintenanceBusy { .. } | CompactionError::ManifestNotFound { .. }
            ),
            "{refused:?}"
        );
        drop(held);

        // And it is released, so the next transaction runs.
        let plan = plan_compaction(&board.root, &indexed(&board));
        let token = plan.manifest_id.clone();
        assert!(apply(plan, &token).is_ok());
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
        let report = apply(plan, &token).unwrap();
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

    /// finding 1 — a semantic driver event whose NESTED payload carries
    /// `"type":"text_chunk"` before the envelope's own discriminator is
    /// evidence, and compaction must preserve it byte for byte.
    #[test]
    fn a_nested_type_collision_is_never_reclaimed() {
        let board = board();
        let path = write_session(&board.sessions, "run-a", "rmux", 8, true);
        let mut content = std::fs::read_to_string(&path).unwrap();
        // The exact shape the byte-scan classifier got wrong: an ACP-style
        // tool_result whose content block is `{"type":"text_chunk"}`, and whose
        // `event` object states its own type AFTER that nested one.
        let collision = serde_json::to_string(&json!({
            "seq": 700,
            "time": "2026-08-02T00:00:00Z",
            "run_id": "run-a",
            "runtime_id": "runtime-run-a",
            "boot_id": "boot-compact",
            "kind": "driver_event",
            "event": {
                "output": {"content": [{"type": "text_chunk", "text": "tool said this"}]},
                "call_id": "c9",
                "ok": true,
                "type": "tool_result",
            },
        }))
        .unwrap();
        assert!(
            collision.find("\"text_chunk\"").unwrap() < collision.find("\"tool_result\"").unwrap(),
            "the fixture must put the nested type FIRST or it proves nothing"
        );
        content.push_str(&collision);
        content.push('\n');
        content.push_str(
            "{\"seq\":701,\"time\":\"2026-08-02T00:00:01Z\",\"run_id\":\"run-a\",\
             \"runtime_id\":\"runtime-run-a\",\"boot_id\":\"boot-compact\",\
             \"kind\":\"lifecycle\",\"event\":{\"phase\":\"release\",\"reason\":\"done\",\
             \"outcome\":\"completed\"}}\n",
        );
        std::fs::write(&path, &content).unwrap();

        assert_eq!(
            crate::run_catalog::classify_history_line(collision.as_bytes()),
            "semantic",
            "the outer discriminator decides, not the first `type` in the bytes"
        );

        let plan = plan_compaction(&board.root, &indexed(&board));
        let token = plan.manifest_id.clone();
        let report = apply(plan, &token).unwrap();
        assert!(matches!(
            report.results[0].outcome,
            CompactionFileOutcome::Compacted { .. }
        ));
        let after = std::fs::read_to_string(&path).unwrap();
        assert!(
            after.contains(&collision),
            "a tool result carrying a nested text_chunk must survive byte for byte"
        );
        assert!(after.contains("tool said this"));
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
        let report = apply(plan, &token).unwrap();
        assert!(matches!(
            report.results[0].outcome,
            CompactionFileOutcome::Compacted { .. }
        ));
        let after = std::fs::read(&path).unwrap();
        assert!(!after.starts_with(b"garbage"));
        assert!(String::from_utf8_lossy(&after).contains("\"phase\":\"acquire\""));

        // And the archive still holds the pre-transaction original.
        let rollback = rollback_fenced(&board.root, &token).unwrap();
        assert_eq!(rollback.restored.len(), 1);
        assert_eq!(std::fs::read(&path).unwrap(), original);
    }

    /// A manifest that names a path outside the project's sessions directory
    /// cannot be used to write there.
    ///
    /// orgasmic:TASK-FZB6T.4 finding 4b — this used to be a PER-RECORD refusal,
    /// which meant the rest of an inconsistent manifest was still acted on. A
    /// document that has been shown to disagree with itself is not a document to
    /// trust the rest of, so the whole rollback now refuses at decode.
    #[test]
    fn rollback_refuses_a_manifest_naming_a_foreign_path() {
        let board = board();
        write_session(&board.sessions, "run-a", "rmux", 8, true);
        let plan = plan_compaction(&board.root, &indexed(&board));
        let token = plan.manifest_id.clone();
        apply(plan, &token).unwrap();

        let manifest_path = board
            .root
            .join(ARCHIVE_REL_PATH)
            .join(&token)
            .join("manifest.json");
        let mut manifest = manifest_of(&board, &token);
        let victim = board.root.join(CATALOG_REL_PATH);
        std::fs::write(&victim, b"catalog\n").unwrap();
        manifest.files[0].session_path = victim.clone();
        std::fs::write(
            &manifest_path,
            serde_json::to_vec_pretty(&manifest).unwrap(),
        )
        .unwrap();

        let error = rollback_fenced(&board.root, &token).unwrap_err();
        assert!(
            matches!(error, CompactionError::ManifestInconsistent { .. }),
            "{error:?}"
        );
        assert_eq!(std::fs::read(&victim).unwrap(), b"catalog\n");
        // And nothing may be fenced on the strength of a manifest that cannot
        // be decoded, so the caller cannot compact against it either.
        assert!(manifest_session_paths(&board.root, &token).is_empty());
    }

    #[test]
    fn rollback_of_an_unknown_manifest_is_a_named_error() {
        let board = board();
        let error = rollback_fenced(&board.root, "deadbeef").unwrap_err();
        assert!(matches!(error, CompactionError::ManifestNotFound { .. }));
    }

    /// finding 3 — a destination that holds neither the original this
    /// transaction archived nor the replacement it staged is REFUSED, not
    /// overwritten. Rollback is a restore, not a blind write.
    #[test]
    fn rollback_refuses_a_destination_it_did_not_produce() {
        let board = board();
        let path = write_session(&board.sessions, "run-a", "rmux", 8, true);
        let plan = plan_compaction(&board.root, &indexed(&board));
        let token = plan.manifest_id.clone();
        apply(plan, &token).unwrap();

        // Somebody else wrote the session file after the compaction committed.
        let foreign = b"{\"seq\":0,\"kind\":\"note\",\"event\":{}}\n".to_vec();
        std::fs::write(&path, &foreign).unwrap();

        let rollback = rollback_fenced(&board.root, &token).unwrap();
        assert!(rollback.restored.is_empty());
        assert_eq!(rollback.refused.len(), 1, "{rollback:?}");
        assert_eq!(
            std::fs::read(&path).unwrap(),
            foreign,
            "a refused destination must be byte-identical afterwards"
        );
    }

    /// finding 3 — an archive that no longer matches the digest recorded when
    /// it was written is not restorable, and rollback says so instead of
    /// writing corrupt bytes over a live file.
    #[test]
    fn rollback_refuses_a_corrupted_archive() {
        let board = board();
        let path = write_session(&board.sessions, "run-a", "rmux", 8, true);
        let plan = plan_compaction(&board.root, &indexed(&board));
        let token = plan.manifest_id.clone();
        let report = apply(plan, &token).unwrap();
        let CompactionFileOutcome::Compacted { archived, .. } = &report.results[0].outcome else {
            panic!("expected a compaction");
        };
        let compacted = std::fs::read(&path).unwrap();
        std::fs::write(archived, b"not the original\n").unwrap();

        let rollback = rollback_fenced(&board.root, &token).unwrap();
        assert!(rollback.restored.is_empty());
        assert_eq!(rollback.refused.len(), 1, "{rollback:?}");
        assert_eq!(std::fs::read(&path).unwrap(), compacted);
    }

    // -----------------------------------------------------------------------
    // Fault injection (finding 2): a durability claim with no fault injection
    // behind it is a comment, not a proof.
    // -----------------------------------------------------------------------

    fn fault_at(point: FaultPoint, fault: Fault) -> impl Fn(FaultPoint, &Path) -> Option<Fault> {
        move |seen, _| (seen == point).then(|| fault.clone())
    }

    /// A crash at each durable boundary leaves the manifest naming the file and
    /// the stage it reached, and a rollback afterwards restores the original —
    /// or proves it was never lost.
    #[test]
    fn a_crash_at_every_boundary_stays_recoverable() {
        for point in [
            FaultPoint::AfterArchive,
            FaultPoint::AfterStage,
            FaultPoint::AfterRename,
        ] {
            let board = board();
            let path = write_session(&board.sessions, "run-a", "rmux", 12, true);
            let original = std::fs::read(&path).unwrap();
            let plan = plan_compaction(&board.root, &indexed(&board));
            let token = plan.manifest_id.clone();
            let fenced = fence_all(&plan);

            let error =
                apply_compaction_with(plan, Some(&token), &fenced, &fault_at(point, Fault::Crash))
                    .unwrap_err();
            assert!(
                error.to_string().contains("injected crash"),
                "{point:?}: {error}"
            );

            // The manifest names the file and the furthest DURABLE stage, even
            // though no result was ever recorded.
            let manifest = manifest_of(&board, &token);
            assert!(!manifest.complete, "{point:?}");
            assert_eq!(manifest.files.len(), 1, "{point:?}");
            let expected_stage = match point {
                FaultPoint::AfterArchive => "archived",
                _ => "staged",
            };
            assert_eq!(
                manifest.files[0].stage.label(),
                expected_stage,
                "{point:?}: {:?}",
                manifest.files[0]
            );

            // A crash after the rename really did leave the compacted file
            // live; the earlier two left the original untouched.
            let live = std::fs::read(&path).unwrap();
            if point == FaultPoint::AfterRename {
                assert_ne!(live, original, "{point:?}");
            } else {
                assert_eq!(live, original, "{point:?}");
            }

            // And in every case rollback lands on the original.
            let rollback = rollback_fenced(&board.root, &token).unwrap();
            assert!(rollback.failed.is_empty(), "{point:?}: {rollback:?}");
            assert!(rollback.refused.is_empty(), "{point:?}: {rollback:?}");
            assert!(!rollback.source_complete, "{point:?}");
            assert_eq!(
                std::fs::read(&path).unwrap(),
                original,
                "{point:?}: rollback must land on the original"
            );
        }
    }

    /// A full disk at the archive or staging write stops that file BEFORE the
    /// live path is touched, and the transaction says so.
    #[test]
    fn a_full_disk_before_the_rename_never_touches_the_live_file() {
        for point in [FaultPoint::ArchiveWrite, FaultPoint::StageWrite] {
            let board = board();
            let path = write_session(&board.sessions, "run-a", "rmux", 12, true);
            let original = std::fs::read(&path).unwrap();
            let plan = plan_compaction(&board.root, &indexed(&board));
            let token = plan.manifest_id.clone();
            let fenced = fence_all(&plan);

            let report = apply_compaction_with(
                plan,
                Some(&token),
                &fenced,
                &fault_at(point, Fault::Io("no space left on device".to_string())),
            )
            .unwrap();
            assert_eq!(report.reclaimed_bytes, 0, "{point:?}");
            assert_eq!(
                std::fs::read(&path).unwrap(),
                original,
                "{point:?}: the live file must be untouched"
            );
            assert!(
                !staging_path(&path).exists(),
                "{point:?}: the staging file must be cleaned up"
            );

            // Rollback finds nothing to do and refuses nothing.
            let rollback = rollback_fenced(&board.root, &token).unwrap();
            assert!(rollback.restored.is_empty(), "{point:?}: {rollback:?}");
            assert!(rollback.refused.is_empty(), "{point:?}: {rollback:?}");
            assert_eq!(std::fs::read(&path).unwrap(), original, "{point:?}");
        }
    }

    /// finding 2 — a journal write that fails STOPS the transaction. The old
    /// code ignored every `write_manifest` failure, which is how a rewritten
    /// file ended up absent from the manifest that was supposed to describe it.
    #[test]
    fn a_failed_result_journal_stops_the_transaction() {
        let board = board();
        let first = write_session(&board.sessions, "run-a", "rmux", 12, true);
        let second = write_session(&board.sessions, "run-b", "rmux", 12, true);
        let original_second = std::fs::read(&second).unwrap();
        let plan = plan_compaction(&board.root, &indexed(&board));
        let token = plan.manifest_id.clone();
        let fenced = fence_all(&plan);
        assert_eq!(plan.files.len(), 2);

        let error = apply_compaction_with(plan, Some(&token), &fenced, &|point, path| {
            (point == FaultPoint::ResultJournalWrite && path == first)
                .then(|| Fault::Io("no space left on device".to_string()))
        })
        .unwrap_err();
        assert!(
            matches!(error, CompactionError::JournalFailed(_)),
            "{error:?}"
        );

        // The second file was never touched: the transaction stopped.
        assert_eq!(std::fs::read(&second).unwrap(), original_second);
        let manifest = manifest_of(&board, &token);
        assert!(!manifest.complete);
        assert_eq!(manifest.files[1].stage.label(), "planned");

        // And the first file, whose rename DID land but whose result was never
        // journalled, is still fully described and fully restorable — the exact
        // window the reviewer's finding 2 names.
        assert_eq!(manifest.files[0].stage.label(), "staged");
        let rollback = rollback_fenced(&board.root, &token).unwrap();
        assert_eq!(rollback.restored, vec![first.clone()]);
        assert!(rollback.refused.is_empty(), "{rollback:?}");
    }
}
