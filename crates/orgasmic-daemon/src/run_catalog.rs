// orgasmic:TASK-FZB6T
//! Compact per-run catalog: the operational lifecycle inventory, separated from
//! transcript/evidence storage.
//!
//! # Why this exists
//!
//! Run enumeration and daemon recovery used to be a function of transcript
//! bytes. TASK-KWSTJ and TASK-7QM8M fixed the *reader* — [`scan_session_lifecycle`]
//! reads a bounded prefix/tail window instead of the whole file — but every
//! `GET /api/runs` still re-derived every classification from disk, so the cost
//! stayed proportional to the number of session FILES and their windows, and it
//! was paid again on every poll.
//!
//! The catalog closes that: one compact record per run, keyed by session path
//! and validated by **file identity** (device/inode/length/mtime). A run that
//! has not been written since it was indexed is answered from memory for zero
//! bytes read. A terminal run is never written again, so after the one-time
//! legacy index the steady-state inventory reads only the files that are
//! actually live.
//!
//! # What a catalog entry is
//!
//! Exactly the operational facts (identity, project/task/kind, driver/harness,
//! verified native metadata *reference*, worktree authority, lifecycle
//! classification, terminal outcome/time, replacement link) plus the compact
//! lifecycle envelope set they were derived from. The envelope set is retained
//! deliberately: it is what the existing classifier consumes, so serving
//! classification from the catalog is provably the same computation on the same
//! input rather than a second implementation that can drift.
//!
//! "Compact" is enforced, not hoped for. Driver events are reduced to their
//! `type` (plus `ready`'s `protocol_version`, the only driver-event body any
//! consumer reads) before an entry is built, so an 18 KiB capabilities frame
//! does not enter the catalog and cannot enter its durable snapshot.
//!
//! # Authority
//!
//! The catalog is **derived state and never authority**. Session JSONL decides;
//! the catalog only remembers what a bounded read of it already said. Every
//! failure mode therefore has the same answer — discard and rebuild — which is
//! what makes the durable snapshot safe to ship: a corrupt file, a
//! forward-version file written by a newer daemon, and a missing file are all
//! handled by re-indexing from the sessions themselves.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use chrono::{DateTime, Utc};
use orgasmic_core::session::{
    scan_session_lifecycle, Lifecycle, ReleaseOutcome, SessionEnvelope, SessionEventKind,
    SessionLifecycleScan, SessionScanBudget,
};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

/// Snapshot format version. Bumped whenever an entry's meaning changes in a way
/// an older daemon would misread. A snapshot whose version is not exactly this
/// is discarded and rebuilt — forward and backward alike, which is the rollback
/// story: install an older runtime and it re-indexes instead of trusting a
/// record shape it does not know.
///
/// v2 (orgasmic:TASK-FZB6T.1): entries carry the recorded `RunMeta`
/// project/worktree pair directly, and [`WorktreeAuthority`] carries the
/// worktree's durable directory identity so a tombstone cannot be revived by
/// an unrelated directory appearing at the recorded path.
pub const CATALOG_VERSION: u32 = 2;

/// Where a project's durable catalog snapshot lives, relative to its root.
pub const CATALOG_REL_PATH: &str = ".orgasmic/tmp/run-catalog.json";

/// Durable record of every run whose recorded worktree has been observed gone,
/// relative to a project root (dec_BBPW4 item 2).
///
/// orgasmic:TASK-FZB6T.3 finding 4 — "the catalog is disposable derived state"
/// and "a tombstone never revives" could not both be true. The tombstone lived
/// only in the cache, so prune → tombstone → catalog loss → path reuse re-derived
/// `Verified` from a same-project checkout at the recorded path and a dead run
/// became an attach candidate again. A terminal verdict needs a durable source
/// OUTSIDE the thing that is allowed to be thrown away, so it has one. This file
/// is AUTHORITY, not cache: losing it loses a fact no rebuild can recover, which
/// is exactly the distinction that makes the catalog safe to discard.
///
/// orgasmic:TASK-FZB6T.4 finding 5 / open question 1 — this authority is
/// **machine-local, not repo-shared**, and this path says so. Every fact it
/// holds is a statement about one filesystem ("the directory this run recorded
/// is gone from THIS machine"); it is meaningless to a teammate, and a run id
/// paired with an absolute worktree path is machine identity, not project
/// content. So it lives under `.orgasmic/tmp/`, which `.orgasmic/.gitignore`
/// already excludes, instead of at the project root where the first tombstone
/// made `git status` dirty and a commit would have carried someone's home
/// directory layout into the repository. It is NOT catalog: nothing wipes this
/// directory, its neighbour is the run-history archive — the other durable thing
/// a rebuild cannot reconstruct — and it is a different file from
/// [`CATALOG_REL_PATH`], which is the distinction TASK-FZB6T.3 finding 4 asked
/// for.
pub const TOMBSTONE_REL_PATH: &str = ".orgasmic/tmp/run-tombstones.json";

/// The per-project lock that serializes the ledger's read-merge-write across
/// PROCESSES (orgasmic:TASK-FZB6T.4 finding 2). A second daemon, or a CLI
/// against the same board, is a real concurrent writer; an in-process mutex
/// would not see it, and two racing merges each drop the other's tombstone.
pub const TOMBSTONE_LOCK_REL_PATH: &str = ".orgasmic/tmp/run-tombstones.lock";

/// Default size of the recent-terminal window `GET /api/runs` serves.
///
/// Actionable records (live, recoverable, ambiguous) are always served in full:
/// bounding those would hide work. Terminal history is bounded because it only
/// grows, is never re-decided, and an operator asking "what needs me" is not
/// asking for the whole board's past.
pub const DEFAULT_TERMINAL_WINDOW: usize = 50;

/// Hard ceiling on an explicitly requested terminal page.
pub const MAX_TERMINAL_WINDOW: usize = 1000;

/// Identity of the session file an entry was derived from.
///
/// Device and inode, not just the path: a session file that was replaced (a
/// rotate, a restore, a recovery reconstruction) is a different file even at the
/// same path, and reusing a cached entry for it would classify the new run from
/// the old one's lifecycle. Length and mtime catch in-place appends, which is
/// what a live run does continuously.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionFileFingerprint {
    #[serde(default)]
    pub dev: u64,
    #[serde(default)]
    pub ino: u64,
    pub len: u64,
    /// Modification time in nanoseconds since the Unix epoch. `0` when the
    /// platform did not report one.
    #[serde(default)]
    pub mtime_ns: u64,
}

impl SessionFileFingerprint {
    pub fn of(metadata: &std::fs::Metadata) -> Self {
        #[cfg(unix)]
        let (dev, ino) = {
            use std::os::unix::fs::MetadataExt;
            (metadata.dev(), metadata.ino())
        };
        #[cfg(not(unix))]
        let (dev, ino) = (0, 0);
        let mtime_ns = metadata
            .modified()
            .ok()
            .and_then(|time| time.duration_since(std::time::UNIX_EPOCH).ok())
            .map(|delta| delta.as_nanos().min(u128::from(u64::MAX)) as u64)
            .unwrap_or(0);
        Self {
            dev,
            ino,
            len: metadata.len(),
            mtime_ns,
        }
    }
}

/// Durable identity of a directory: device plus inode.
///
/// orgasmic:TASK-FZB6T.1 finding 5 — a path is a name, not an identity. A
/// dispatch worktree that was pruned and a *different* directory later created
/// at the same path are two different objects, and only the second one is
/// reachable by `exists()`. Recording dev/ino when the worktree was verified is
/// what lets a later probe tell "the volume came back" from "somebody reused
/// the path".
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct DirIdentity {
    pub dev: u64,
    pub ino: u64,
}

impl DirIdentity {
    fn of(metadata: &std::fs::Metadata) -> Option<Self> {
        if !metadata.is_dir() {
            return None;
        }
        #[cfg(unix)]
        {
            use std::os::unix::fs::MetadataExt;
            Some(Self {
                dev: metadata.dev(),
                ino: metadata.ino(),
            })
        }
        #[cfg(not(unix))]
        {
            None
        }
    }

    /// The identity of the directory `path` resolves to, or `None` when the
    /// path is absent, is not a directory, or the platform cannot answer.
    pub fn at(path: &Path) -> Option<Self> {
        Self::of(&std::fs::metadata(path).ok()?)
    }
}

/// Whether this run's recorded origin worktree is still authority.
///
/// A separate axis from lifecycle classification on purpose. "The worktree this
/// run recorded no longer exists" is a stable, terminal fact about the run's
/// authority — it does not become true and false again as a poll races a
/// filesystem — and collapsing it into `ambiguous` is what made a pruned
/// dispatch worktree an eternal attach candidate: every inventory pass spawned a
/// driver attach probe for a run that could never be recovered under that
/// worktree, then discarded the answer.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum WorktreeAuthority {
    /// `RunMeta` named a worktree, it exists, and its `project.org` names the
    /// same project as the directory containing this session file.
    Verified {
        worktree: PathBuf,
        /// Directory identity observed at verification time. `None` only where
        /// the platform cannot report one.
        #[serde(default)]
        identity: Option<DirIdentity>,
    },
    /// `RunMeta` named a worktree that is no longer on disk, or whose directory
    /// identity no longer matches the one this run was verified against.
    /// Pruned, moved, replaced, or on an unmounted volume.
    ///
    /// **Terminal for the run identity, with no way back** (dec_BBPW4). It is
    /// never re-probed and never revived: no directory appearing at the
    /// recorded path — same name, same device, same inode — makes a dead run an
    /// attach candidate again. `verified_identity` is retained as evidence of
    /// what this run was once verified against, not as a revival key.
    Tombstoned {
        recorded: PathBuf,
        #[serde(default)]
        verified_identity: Option<DirIdentity>,
    },
    /// A worktree exists at the recorded path but does not belong to the
    /// project that contains this session file.
    Mismatched { recorded: PathBuf },
    /// The session carries no `RunMeta` project/worktree authority at all
    /// (pre-`RunMeta` sessions).
    Unrecorded,
    /// The session file is not contained by an identified registered project.
    Unidentified,
    /// A `Verified` verdict that could not be checked against the durable
    /// tombstone ledger, because the ledger is unreadable or declares a version
    /// this build does not know.
    ///
    /// orgasmic:TASK-FZB6T.4 finding 2 — this is the fail-closed answer. Only a
    /// positive ledger hit overrules a re-derived `Verified`, so a damaged
    /// ledger used to read as "nothing is tombstoned" and a dead run became an
    /// attach candidate again. Where the terminal facts cannot be READ, the run
    /// is not proven attachable, and an unproven attach candidate is refused
    /// rather than offered.
    Unprovable { recorded: PathBuf },
}

impl WorktreeAuthority {
    pub fn verified_worktree(&self) -> Option<&Path> {
        match self {
            Self::Verified { worktree, .. } => Some(worktree.as_path()),
            _ => None,
        }
    }

    /// The stable "this run's worktree is gone" state. Callers use it to stop
    /// probing rather than to classify.
    pub fn is_tombstoned(&self) -> bool {
        matches!(self, Self::Tombstoned { .. })
    }

    /// Whether an attach probe is meaningless for this run: its worktree is
    /// proven gone, or the authority that would prove otherwise cannot be read
    /// (orgasmic:TASK-FZB6T.4 finding 2). Both are refusals; only one of them is
    /// terminal.
    pub fn blocks_attach(&self) -> bool {
        matches!(self, Self::Tombstoned { .. } | Self::Unprovable { .. })
    }

    /// The reason string the inventory reports for a non-verified authority.
    /// `None` for [`Self::Verified`].
    pub fn authority_error(&self) -> Option<&'static str> {
        match self {
            Self::Verified { .. } => None,
            Self::Unidentified => {
                Some("session is not contained by an identified registered project")
            }
            Self::Unrecorded => Some("session has no origin RunMeta project/worktree authority"),
            Self::Mismatched { .. } => {
                Some("RunMeta project does not match containing registered project")
            }
            Self::Tombstoned { .. } => {
                Some("recorded worktree is gone; run tombstoned, not an attach candidate")
            }
            Self::Unprovable { .. } => Some(
                "the durable tombstone ledger could not be read, so this run is not proven \
                 attachable; repair or remove the ledger",
            ),
        }
    }

    pub fn label(&self) -> &'static str {
        match self {
            Self::Verified { .. } => "verified",
            Self::Tombstoned { .. } => "tombstoned",
            Self::Mismatched { .. } => "mismatched",
            Self::Unrecorded => "unrecorded",
            Self::Unidentified => "unidentified",
            Self::Unprovable { .. } => "unprovable",
        }
    }
}

/// Verified harness-native runtime *reference*. A pointer, never a copy: the
/// native transcript stays vendor-owned where the harness wrote it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct NativeRuntimeRef {
    pub provider: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_path: Option<PathBuf>,
}

/// How a run ended, when it ended at all.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "terminal", rename_all = "snake_case")]
pub enum TerminalRecord {
    /// The run's final envelope is a `Release`.
    Release {
        outcome: ReleaseOutcome,
        at: DateTime<Utc>,
    },
    /// No release, but a terminal driver event (`run_complete` / `run_fail` /
    /// `run_error`) is on record.
    DriverEvent { event: String, at: DateTime<Utc> },
    /// An external manager registration that ended with a daemon restart.
    ExternalRegistrationEnded,
}

impl TerminalRecord {
    pub fn at(&self) -> Option<DateTime<Utc>> {
        match self {
            Self::Release { at, .. } | Self::DriverEvent { at, .. } => Some(*at),
            Self::ExternalRegistrationEnded => None,
        }
    }

    pub fn outcome(&self) -> Option<ReleaseOutcome> {
        match self {
            Self::Release { outcome, .. } => Some(*outcome),
            _ => None,
        }
    }
}

/// One compact per-run catalog record.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RunCatalogEntry {
    // --- identity ---
    pub run_id: String,
    pub runtime_id: String,
    pub boot_id: String,
    pub session_path: PathBuf,

    // --- project / task / kind ---
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub project_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub project_root: Option<PathBuf>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub task_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub kind: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub worker_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stage: Option<String>,

    // --- driver / harness ---
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub transport: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub harness: Option<String>,

    // --- verified native metadata reference ---
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub native: Option<NativeRuntimeRef>,

    // --- worktree authority ---
    pub worktree_authority: WorktreeAuthority,
    /// The `RunMeta` project/worktree pair verbatim, decided once at index
    /// time.
    ///
    /// orgasmic:TASK-FZB6T.1 finding 8 — authority re-verification needs this
    /// pair and used to re-parse it out of `lifecycle_envelopes` on every poll,
    /// inside the catalog mutex. Stored flat, the refresh can clone it in the
    /// short planning critical section and do the filesystem work outside.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub run_meta_project: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub run_meta_worktree: Option<PathBuf>,
    /// Whether a `RunMeta` lifecycle event was recorded at all — the
    /// distinction between "recorded no worktree" and "pre-`RunMeta` session".
    #[serde(default)]
    pub run_meta_recorded: bool,

    // --- lifecycle classification ---
    /// The semantic terminal verdict, decided under exactly the rule the
    /// inventory has always used. See [`terminal_record`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub terminal: Option<TerminalRecord>,
    /// The raw `Release` outcome on the file's genuine final envelope, if any —
    /// `Interrupted` included, which [`Self::terminal`] deliberately does not
    /// report as terminal.
    ///
    /// Kept separately because the two consumers differ on exactly this value:
    /// the inventory classifier treats an interrupted release as non-terminal
    /// and stops there, while boot reattach falls through to the terminal
    /// driver events. Collapsing them would silently change one of the two.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub final_release_outcome: Option<ReleaseOutcome>,
    /// `run_complete` / `run_fail`, from the newest terminal driver event on
    /// record. Raw fact, independent of the release verdict above.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub driver_terminal_event: Option<String>,
    /// `true` when this session is a daemon-local external registration record.
    #[serde(default)]
    pub external_registration: bool,

    // --- replacement / claim link ---
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub replacement_run_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub replacement_session_path: Option<PathBuf>,

    // --- provenance of the derivation ---
    /// The bounded scan skipped the middle of the file.
    #[serde(default)]
    pub scan_truncated: bool,
    /// `run_id` of the session file's final line, read from that line's bounded
    /// header even when the line itself was dropped as transcript.
    ///
    /// orgasmic:TASK-7QM8M — boot reattach pairs a retained prefix segment with
    /// the file's END, and on a truncated scan the newest RETAINED segment is
    /// not provably the newest segment on disk. This is the one fact that makes
    /// the pairing provable, so the catalog has to carry it or the guard is lost
    /// the moment reattach reads a cached entry instead of a fresh scan.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub final_line_run_id: Option<String>,
    /// The file's genuine final envelope was retained, so terminal decisions
    /// may be made from it.
    #[serde(default)]
    pub final_envelope_retained: bool,
    /// The session could not be scanned or parsed at all.
    #[serde(default)]
    pub unreadable: bool,
    pub file_bytes: u64,
    pub indexed_at: DateTime<Utc>,
    pub fingerprint: SessionFileFingerprint,

    /// The compact lifecycle envelope set this entry was derived from, with
    /// driver-event bodies reduced. Retained so the inventory classifier runs
    /// on the same input it always did — a catalog that stored only verdicts
    /// would be a second classifier free to drift from the first.
    #[serde(default)]
    pub lifecycle_envelopes: Vec<SessionEnvelope>,
}

impl RunCatalogEntry {
    /// Driver+harness label used by history accounting. Falls back to the
    /// transport alone, then to `unknown`.
    pub fn driver_label(&self) -> String {
        match (self.transport.as_deref(), self.harness.as_deref()) {
            (Some(transport), Some(harness)) => format!("{transport}/{harness}"),
            (Some(transport), None) => transport.to_string(),
            (None, _) => "unknown".to_string(),
        }
    }

    /// Whether this record is terminal — the question the recent-terminal
    /// window pages over.
    pub fn is_terminal(&self) -> bool {
        self.terminal.is_some()
    }

    /// When this run reached its terminal state, if it recorded one. The
    /// ordering key of the recent-terminal window, in exactly the shape the
    /// response carries it.
    pub fn terminal_at(&self) -> Option<DateTime<Utc>> {
        self.terminal.as_ref().and_then(TerminalRecord::at)
    }

    /// The time used to order the recent-terminal window. Terminal time when
    /// recorded, otherwise the last retained envelope's time, otherwise the
    /// epoch (so records that cannot say when they ended sort oldest and are
    /// paged out first rather than crowding the window).
    pub fn terminal_sort_time(&self) -> DateTime<Utc> {
        self.terminal
            .as_ref()
            .and_then(TerminalRecord::at)
            .or_else(|| self.lifecycle_envelopes.last().map(|e| e.time))
            .unwrap_or_else(|| DateTime::<Utc>::from_timestamp_nanos(0))
    }
}

/// What one catalog refresh actually did.
#[derive(Debug, Clone, Default, Serialize)]
pub struct CatalogRefreshStats {
    /// Session files considered.
    pub session_files: u64,
    /// Entries answered from the cache without reading the file.
    pub cache_hits: u64,
    /// Entries (re)built by a bounded scan.
    pub rebuilt: u64,
    /// Bytes read from disk by those rebuilds. Stays at zero on a steady-state
    /// poll of a board whose runs have all ended: that is the whole claim.
    pub bytes_inspected: u64,
    /// On-disk size of the considered files.
    pub session_file_bytes: u64,
    /// Files whose middle region was skipped by the bounded scan.
    pub truncated_scans: u64,
    /// Files that could not be scanned or parsed.
    pub unreadable_sessions: u64,
    /// Entries dropped because their session file is gone.
    pub evicted: u64,
    /// Worktree authority verdicts re-verified against the filesystem.
    pub authority_reverified: u64,
    /// Rebuilt entries whose re-derived `Verified` verdict was overruled by the
    /// durable tombstone ledger (orgasmic:TASK-FZB6T.3 finding 4).
    pub tombstones_reasserted: u64,
    /// Entries whose re-derived `Verified` verdict could not be checked at all,
    /// because the durable tombstone ledger was unreadable or foreign-version
    /// (orgasmic:TASK-FZB6T.4 finding 2). Non-zero means the board is being
    /// served fail-closed and an operator has a ledger to repair.
    pub tombstones_unprovable: u64,
}

/// How a durable snapshot load ended. Every non-`Loaded` outcome means the same
/// thing operationally — rebuild — but they are distinguished because "the file
/// was corrupt" and "a newer daemon wrote it" are different operator stories.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "outcome", rename_all = "snake_case")]
pub enum SnapshotLoad {
    Loaded { entries: usize },
    Absent,
    Corrupt { error: String },
    VersionMismatch { found: u32, expected: u32 },
}

impl SnapshotLoad {
    pub fn loaded_entries(&self) -> usize {
        match self {
            Self::Loaded { entries } => *entries,
            _ => 0,
        }
    }
}

#[derive(Debug, Serialize, Deserialize)]
struct CatalogSnapshot {
    catalog_version: u32,
    written_at: DateTime<Utc>,
    entries: Vec<RunCatalogEntry>,
}

/// Minimum interval between durable snapshot writes.
///
/// The snapshot's only job is to make the NEXT boot cheap, so a stale one costs
/// exactly one re-scan of whatever changed since — which is bounded and small.
/// Without a floor, every inventory poll of a board with one live run would
/// rewrite the whole index, because that run's session file grows continuously:
/// the catalog would trade a per-poll READ of one file for a per-poll WRITE of
/// every record, which is not an improvement.
pub const SNAPSHOT_MIN_INTERVAL: std::time::Duration = std::time::Duration::from_secs(60);

/// Snapshot bookkeeping for one canonical project root.
///
/// orgasmic:TASK-FZB6T.1 finding 6 — this used to be one global `dirty` flag
/// and one global `last_saved` instant for the whole daemon. On a multi-project
/// board that is wrong in both directions: project A's refresh cleared the flag
/// project B had set (so B's snapshot was never written), and A's save throttled
/// B's for a minute even though B had never been saved at all.
#[derive(Debug, Default, Clone)]
struct ProjectSnapshotState {
    dirty: bool,
    last_saved: Option<std::time::Instant>,
}

#[derive(Debug, Default)]
struct CatalogState {
    by_path: BTreeMap<PathBuf, RunCatalogEntry>,
    /// Per canonical project root.
    projects: BTreeMap<PathBuf, ProjectSnapshotState>,
    /// Monotonic per-path write counter, bumped by
    /// [`RunCatalog::invalidate_session`].
    ///
    /// orgasmic:TASK-FZB6T.1 finding 8 — the refresh scans session files
    /// *outside* the mutex, so a lifecycle append can land between the scan and
    /// the commit. The counter is the compare-and-swap token: a rebuilt entry
    /// is committed only if nobody invalidated that path while it was being
    /// built, so the writer's invalidation can never be silently overwritten by
    /// an entry derived from the bytes that preceded it.
    invalidations: BTreeMap<PathBuf, u64>,
}

impl CatalogState {
    fn mark_dirty(&mut self, project_root: &Path) {
        self.projects
            .entry(project_root.to_path_buf())
            .or_default()
            .dirty = true;
    }

    fn invalidation(&self, path: &Path) -> u64 {
        self.invalidations.get(path).copied().unwrap_or(0)
    }
}

/// The daemon-lifetime run catalog.
///
/// The lock is a `std::sync::Mutex`, not a tokio one, and the methods are
/// synchronous. Every critical section is a bounded run of map lookups and
/// inserts with no `.await` and **no filesystem call** inside it: the refresh
/// gathers directory state, scans session files, and probes worktree paths
/// entirely outside the lock, then commits under one short critical section
/// guarded by the per-path invalidation counter (orgasmic:TASK-FZB6T.1 finding
/// 8). The blocking work belongs on a blocking thread, and every caller — the
/// boot pass, the inventory, `run history inspect` — reaches it through
/// `spawn_blocking`.
///
/// An async API here would have forced those callers to `block_on` a handle
/// from inside a blocking thread, which deadlocks against a current-thread
/// runtime whose only thread is parked awaiting that very `JoinHandle`.
#[derive(Clone, Default)]
pub struct RunCatalog {
    state: Arc<Mutex<CatalogState>>,
}

impl std::fmt::Debug for RunCatalog {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RunCatalog").finish_non_exhaustive()
    }
}

impl RunCatalog {
    pub fn new() -> Self {
        Self::default()
    }

    /// Poisoning cannot corrupt derived state: a panic mid-update leaves at
    /// worst a stale entry, which the next fingerprint check re-derives.
    fn lock(&self) -> std::sync::MutexGuard<'_, CatalogState> {
        self.state.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Load a project's durable snapshot into the catalog.
    ///
    /// Never fails: an absent, unreadable, corrupt, or foreign-version snapshot
    /// is discarded and the next [`Self::refresh_dir`] rebuilds from the session
    /// files. The catalog is derived state; there is nothing here that a rebuild
    /// cannot reproduce.
    pub fn load_snapshot(&self, project_root: &Path) -> SnapshotLoad {
        let path = project_root.join(CATALOG_REL_PATH);
        let source = match std::fs::read_to_string(&path) {
            Ok(source) => source,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                return SnapshotLoad::Absent
            }
            Err(error) => {
                return SnapshotLoad::Corrupt {
                    error: error.to_string(),
                }
            }
        };
        // Read the version before the entries: a snapshot from a newer daemon
        // may hold fields this build would drop, and silently re-serializing a
        // lossy round trip is how a downgrade corrupts state it merely could
        // not understand.
        let probe: Value = match serde_json::from_str(&source) {
            Ok(value) => value,
            Err(error) => {
                return SnapshotLoad::Corrupt {
                    error: error.to_string(),
                }
            }
        };
        let found = probe
            .get("catalog_version")
            .and_then(Value::as_u64)
            .unwrap_or(0) as u32;
        if found != CATALOG_VERSION {
            return SnapshotLoad::VersionMismatch {
                found,
                expected: CATALOG_VERSION,
            };
        }
        let snapshot: CatalogSnapshot = match serde_json::from_value(probe) {
            Ok(snapshot) => snapshot,
            Err(error) => {
                return SnapshotLoad::Corrupt {
                    error: error.to_string(),
                }
            }
        };
        // orgasmic:TASK-FZB6T.1 finding 7 — validate every entry against the
        // filesystem BEFORE taking the lock, so a snapshot with thousands of
        // records cannot hold the catalog mutex across thousands of stats.
        let sessions_dir = project_sessions_dir(project_root);
        let admitted: Vec<RunCatalogEntry> = snapshot
            .entries
            .into_iter()
            .filter(|entry| snapshot_entry_is_admissible(entry, &sessions_dir))
            .collect();
        let mut state = self.lock();
        let mut loaded = 0;
        for entry in admitted {
            state.by_path.insert(entry.session_path.clone(), entry);
            loaded += 1;
        }
        SnapshotLoad::Loaded { entries: loaded }
    }

    /// Serialize this project's entries. `None` when nothing changed since the
    /// last save, so a steady-state poll writes nothing — and `None` again when
    /// a save happened within [`SNAPSHOT_MIN_INTERVAL`], so a board with a live
    /// run does not rewrite the whole index on every poll.
    ///
    /// Both the dirty flag and the throttle are **per project**
    /// (orgasmic:TASK-FZB6T.1 finding 6): one project's save neither clears
    /// another's pending work nor throttles its first write.
    ///
    /// Calling this CONSUMES that project's dirty flag: the caller is expected
    /// to persist the bytes it returns.
    pub fn snapshot_bytes(&self, project_root: &Path) -> Option<Vec<u8>> {
        self.snapshot_bytes_after(project_root, SNAPSHOT_MIN_INTERVAL)
    }

    /// [`Self::snapshot_bytes`] with an explicit floor. `Duration::ZERO` forces
    /// a save; tests and the boot path use it.
    pub fn snapshot_bytes_after(
        &self,
        project_root: &Path,
        min_interval: std::time::Duration,
    ) -> Option<Vec<u8>> {
        let mut state = self.lock();
        {
            let project = state.projects.get(project_root)?;
            if !project.dirty {
                return None;
            }
            if let Some(last) = project.last_saved {
                if last.elapsed() < min_interval {
                    return None;
                }
            }
        }
        let entries: Vec<RunCatalogEntry> = state
            .by_path
            .values()
            .filter(|entry| entry.session_path.starts_with(project_root))
            .cloned()
            .collect();
        let snapshot = CatalogSnapshot {
            catalog_version: CATALOG_VERSION,
            written_at: Utc::now(),
            entries,
        };
        let bytes = serde_json::to_vec_pretty(&snapshot).ok()?;
        let project = state
            .projects
            .entry(project_root.to_path_buf())
            .or_default();
        project.dirty = false;
        project.last_saved = Some(std::time::Instant::now());
        Some(bytes)
    }

    /// Bring the catalog up to date for one project's session directory.
    ///
    /// Blocking filesystem work; callers must keep it off the async runtime's
    /// hot threads the same way the boot scan does.
    ///
    /// orgasmic:TASK-FZB6T.1 finding 8 — four phases, and the mutex is held
    /// only in two of them:
    ///
    /// 1. **gather** (no lock): `read_dir` + one `symlink_metadata` per file;
    /// 2. **plan** (short lock): map lookups only — decide what to rebuild,
    ///    which cached authority verdicts to probe, and what to evict;
    /// 3. **work** (no lock): `scan_session_lifecycle` per rebuilt file and one
    ///    authority probe per cached record. This is all of the expensive work
    ///    and none of it can block another thread's catalog read;
    /// 4. **commit** (short lock): map inserts and removes, each guarded by the
    ///    per-path invalidation counter captured in phase 2.
    pub fn refresh_dir(
        &self,
        dir: &Path,
        project_id: Option<&str>,
        project_root: &Path,
        budget: SessionScanBudget,
    ) -> CatalogRefreshStats {
        self.refresh_dir_observed(dir, project_id, project_root, budget, &mut |_| {})
    }

    /// [`Self::refresh_dir`] with a hook invoked during the unlocked work
    /// phase. Tests use it to prove the mutex is genuinely free while session
    /// files are being scanned; production passes an empty closure.
    fn refresh_dir_observed(
        &self,
        dir: &Path,
        project_id: Option<&str>,
        project_root: &Path,
        budget: SessionScanBudget,
        during_work: &mut dyn FnMut(&Self),
    ) -> CatalogRefreshStats {
        let mut stats = CatalogRefreshStats::default();

        // --- phase 1: gather, no lock held ---------------------------------
        let mut observed: Vec<(PathBuf, SessionFileFingerprint, u64)> = Vec::new();
        let entries = match std::fs::read_dir(dir) {
            Ok(entries) => entries,
            Err(_) => return stats,
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.extension().and_then(|ext| ext.to_str()) != Some("jsonl") {
                continue;
            }
            let Ok(metadata) = std::fs::symlink_metadata(&path) else {
                continue;
            };
            // A session file is a regular file. A symlink or a fifo at this
            // path is not one, and opening it is how an inventory hangs.
            if !metadata.is_file() || metadata.file_type().is_symlink() {
                continue;
            }
            stats.session_files += 1;
            stats.session_file_bytes += metadata.len();
            observed.push((path, SessionFileFingerprint::of(&metadata), metadata.len()));
        }

        // --- phase 2: plan, short lock -------------------------------------
        let mut rebuild: Vec<PlannedRebuild> = Vec::new();
        let mut recheck: Vec<PlannedRecheck> = Vec::new();
        let stale: Vec<PlannedStale>;
        {
            let state = self.lock();
            for (path, fingerprint, len) in &observed {
                match state.by_path.get(path) {
                    Some(entry) if entry.fingerprint == *fingerprint => {
                        stats.cache_hits += 1;
                        if entry.scan_truncated {
                            stats.truncated_scans += 1;
                        }
                        if entry.unreadable {
                            stats.unreadable_sessions += 1;
                        }
                        // Worktree authority is the one verdict that can change
                        // without the session file changing, so it is
                        // re-checked — one stat per record, O(records) and
                        // never O(bytes), and the stat happens in phase 3.
                        recheck.push(PlannedRecheck {
                            path: path.clone(),
                            run_id: entry.run_id.clone(),
                            previous: entry.worktree_authority.clone(),
                            invalidation: state.invalidation(path),
                        });
                    }
                    _ => rebuild.push(PlannedRebuild {
                        path: path.clone(),
                        fingerprint: *fingerprint,
                        len: *len,
                        invalidation: state.invalidation(path),
                    }),
                }
            }
            // Evict records whose session file is gone from this directory.
            // Scoped to `dir` so refreshing one project never drops another's
            // entries.
            //
            // orgasmic:TASK-FZB6T.2 finding 6 — an eviction is a lost update
            // waiting to happen, so it is PLANNED with a generation exactly
            // like a rebuild: the invalidation counter plus the identity of the
            // record actually observed absent. Without it, a refresh that saw
            // the path gone could commit an unconditional remove after another
            // refresh (or the writer) had already indexed a NEWER file at that
            // path, evicting a live record on the strength of a stale
            // observation.
            let present: std::collections::BTreeSet<&PathBuf> =
                observed.iter().map(|(path, _, _)| path).collect();
            stale = state
                .by_path
                .iter()
                .filter(|(path, _)| path.parent() == Some(dir) && !present.contains(path))
                .map(|(path, entry)| PlannedStale {
                    invalidation: state.invalidation(path),
                    path: path.clone(),
                    fingerprint: entry.fingerprint,
                    indexed_at: entry.indexed_at,
                })
                .collect();
        }

        // --- phase 3: work, no lock held -----------------------------------
        let mut built: Vec<(PlannedRebuild, RunCatalogEntry)> = Vec::with_capacity(rebuild.len());
        for planned in rebuild {
            during_work(self);
            stats.rebuilt += 1;
            let entry = match scan_session_lifecycle(&planned.path, budget) {
                Ok(scan) => {
                    stats.bytes_inspected += scan.bytes_inspected;
                    if scan.truncated {
                        stats.truncated_scans += 1;
                    }
                    entry_from_scan(
                        &scan,
                        &planned.path,
                        project_id,
                        project_root,
                        planned.fingerprint,
                    )
                }
                Err(_) => {
                    stats.unreadable_sessions += 1;
                    unreadable_entry(
                        &planned.path,
                        project_id,
                        project_root,
                        planned.fingerprint,
                        planned.len,
                    )
                }
            };
            built.push((planned, entry));
        }
        let mut authority_updates: Vec<(PlannedRecheck, WorktreeAuthority)> = Vec::new();
        for planned in recheck {
            during_work(self);
            let Some(probe) = probe_authority_path(&planned.previous) else {
                continue;
            };
            if let Some(refreshed) = reverify_authority(&planned.previous, probe) {
                authority_updates.push((planned, refreshed));
            }
        }

        // orgasmic:TASK-FZB6T.3 finding 4 / dec_BBPW4 item 2 — the tombstone is
        // durable, and this is where the cache is reconciled against it. A
        // rebuilt entry re-derives `Verified` from whatever directory now
        // answers to the recorded path, which is precisely how a reused dispatch
        // worktree revived a dead run; the ledger overrules it. And a tombstone
        // this pass MINTED is written down before it can be lost with the cache.
        //
        // orgasmic:TASK-FZB6T.4 finding 2 — and a ledger that cannot be READ is
        // not an empty ledger. Where the terminal facts are unavailable, every
        // re-derived `Verified` becomes `Unprovable`: refused, not offered. The
        // ledger is also not written in that state, because merging into a file
        // this build cannot decode would destroy the authority it failed to
        // read.
        let ledger_state = TombstoneLedger::load(project_root);
        let ledger_unusable = ledger_state.unusable_reason();
        let mut ledger = match ledger_state {
            TombstoneLedgerState::Loaded(ledger) => ledger,
            _ => TombstoneLedger::default(),
        };
        if let Some(reason) = &ledger_unusable {
            tracing::warn!(
                project_root = %project_root.display(),
                reason,
                "the durable run tombstone ledger is unusable; every affected run is reported \
                 as unprovable instead of as an attach candidate"
            );
        }
        let mut ledger_grew = false;
        for (_, entry) in &mut built {
            match (
                &entry.worktree_authority,
                ledger_unusable.is_some(),
                ledger.contains(&entry.run_id),
            ) {
                (WorktreeAuthority::Verified { worktree, .. }, true, _) => {
                    entry.worktree_authority = WorktreeAuthority::Unprovable {
                        recorded: worktree.clone(),
                    };
                    stats.tombstones_unprovable += 1;
                }
                (WorktreeAuthority::Verified { worktree, .. }, false, true) => {
                    entry.worktree_authority = WorktreeAuthority::Tombstoned {
                        recorded: worktree.clone(),
                        verified_identity: None,
                    };
                    stats.tombstones_reasserted += 1;
                }
                (WorktreeAuthority::Tombstoned { recorded, .. }, false, false) => {
                    ledger_grew |= ledger.record(&entry.run_id, recorded);
                }
                _ => {}
            }
        }
        for (planned, refreshed) in &authority_updates {
            if let WorktreeAuthority::Tombstoned { recorded, .. } = refreshed {
                ledger_grew |= ledger.record(&planned.run_id, recorded);
            }
        }
        if ledger_grew {
            // A failure here is not fatal to this refresh, but it IS a lost
            // terminal fact, so it is loud in the log rather than swallowed.
            if let Err(error) = ledger.save(project_root) {
                tracing::warn!(
                    project_root = %project_root.display(),
                    error = %error,
                    "could not persist run tombstones; a catalog rebuild may re-offer a \
                     pruned worktree as an attach candidate"
                );
            }
        }

        // --- phase 4: commit, short lock -----------------------------------
        let mut state = self.lock();
        let mut dirty = false;
        for (planned, entry) in built {
            // Compare-and-swap: a lifecycle append that landed while this entry
            // was being scanned wins. Dropping the built entry leaves the path
            // uncached, so the next refresh re-derives it from the newer bytes.
            if state.invalidation(&planned.path) != planned.invalidation {
                continue;
            }
            state.by_path.insert(planned.path, entry);
            dirty = true;
        }
        for (planned, refreshed) in authority_updates {
            if state.invalidation(&planned.path) != planned.invalidation {
                continue;
            }
            let Some(entry) = state.by_path.get_mut(&planned.path) else {
                continue;
            };
            if entry.worktree_authority != planned.previous {
                continue;
            }
            entry.worktree_authority = refreshed;
            stats.authority_reverified += 1;
            dirty = true;
        }
        for planned in stale {
            // Compare-and-swap, same discipline as a rebuild: the counter must
            // not have moved, and the record still at this path must be the
            // exact one this refresh observed absent. Either check failing
            // means a newer entry arrived while this pass was working, and the
            // newer entry wins.
            if state.invalidation(&planned.path) != planned.invalidation {
                continue;
            }
            let Some(entry) = state.by_path.get(&planned.path) else {
                continue;
            };
            if entry.fingerprint != planned.fingerprint || entry.indexed_at != planned.indexed_at {
                continue;
            }
            state.by_path.remove(&planned.path);
            // The generation is NOT deleted with the entry. Dropping it resets
            // the counter to zero, which is what turned a stale eviction into
            // an ABA window: a later refresh could then capture 0, race an
            // invalidation, and still see 0.
            stats.evicted += 1;
            dirty = true;
        }
        if dirty {
            state.mark_dirty(project_root);
        }
        stats
    }

    /// Every entry, ordered by session path.
    pub fn entries(&self) -> Vec<RunCatalogEntry> {
        self.lock().by_path.values().cloned().collect()
    }

    /// Entries under one project root, ordered by session path.
    pub fn entries_for_project(&self, project_root: &Path) -> Vec<RunCatalogEntry> {
        self.lock()
            .by_path
            .values()
            .filter(|entry| entry.session_path.starts_with(project_root))
            .cloned()
            .collect()
    }

    /// Invalidate one run's cached entry so the next refresh rebuilds it from
    /// the session file.
    ///
    /// The session writer calls this on every lifecycle append (see
    /// [`crate::writer`]): a lifecycle envelope is exactly the kind of write
    /// that changes a catalog verdict, and the writer is the one place that
    /// knows a write happened before anyone asks about it. The fingerprint
    /// would catch it anyway on the next refresh — this makes the update
    /// promptness a property of the write path rather than of mtime
    /// granularity.
    ///
    /// Bumping the per-path counter is what makes the invalidation survive a
    /// refresh that is already scanning this file: the refresh's commit is a
    /// compare-and-swap against exactly this number
    /// (orgasmic:TASK-FZB6T.1 finding 8). The critical section is two map
    /// operations, so the writer task never waits on filesystem work.
    pub fn invalidate_session(&self, session_path: &Path) {
        let mut state = self.lock();
        *state
            .invalidations
            .entry(session_path.to_path_buf())
            .or_insert(0) += 1;
        if let Some(entry) = state.by_path.remove(session_path) {
            if let Some(root) = entry.project_root.clone() {
                state.mark_dirty(&root);
            }
        }
    }

    /// The cached record for an exact run id, if the catalog holds one.
    ///
    /// orgasmic:TASK-FZB6T.1 finding 9 — an exact `run show <id>` used to be
    /// answered by classifying the whole board and then searching the *paged*
    /// result, so a terminal run older than the default window was reported as
    /// "no such run". The catalog keys runs by path but holds the id, and a
    /// direct lookup is O(records) with no session read at all.
    ///
    /// Ties are broken by the most recently indexed record: a run id can appear
    /// on more than one path only when history was copied, and the freshest
    /// index is the one that describes the file the daemon is actually serving.
    pub fn find_by_run_id(&self, run_id: &str) -> Option<RunCatalogEntry> {
        self.lock()
            .by_path
            .values()
            .filter(|entry| entry.run_id == run_id)
            .max_by_key(|entry| entry.indexed_at)
            .cloned()
    }

    #[cfg(test)]
    pub(crate) fn len(&self) -> usize {
        self.lock().by_path.len()
    }

    /// The per-path invalidation generation. Test-only: it is a property of the
    /// commit protocol, not something a consumer reads.
    #[cfg(test)]
    pub(crate) fn generation_of(&self, path: &Path) -> u64 {
        self.lock().invalidation(path)
    }
}

/// One session file the plan phase decided must be re-derived from disk.
struct PlannedRebuild {
    path: PathBuf,
    fingerprint: SessionFileFingerprint,
    len: u64,
    invalidation: u64,
}

/// One cached record whose worktree authority the plan phase decided to probe.
struct PlannedRecheck {
    path: PathBuf,
    /// Carried so a minted tombstone can be written to the durable ledger
    /// without re-taking the catalog lock (orgasmic:TASK-FZB6T.3 finding 4).
    run_id: String,
    previous: WorktreeAuthority,
    invalidation: u64,
}

/// One record the plan phase observed absent from the session directory.
///
/// orgasmic:TASK-FZB6T.2 finding 6 — carries the generation AND the identity of
/// the record observed, so the commit can refuse to evict anything newer.
struct PlannedStale {
    path: PathBuf,
    invalidation: u64,
    fingerprint: SessionFileFingerprint,
    indexed_at: DateTime<Utc>,
}

/// Where a project keeps its per-run session JSONL.
///
/// Mirrors `crate::api::project_sessions_dir`; kept here so snapshot admission
/// does not depend on the API module's visibility.
pub fn project_sessions_dir(project_root: &Path) -> PathBuf {
    project_root.join(".orgasmic/tmp/sessions")
}

/// Whether one snapshot entry may be loaded into the catalog.
///
/// orgasmic:TASK-FZB6T.1 finding 7 — `starts_with(project_root)` was the only
/// admission rule, and it admits far more than a session record. Anything under
/// the project root passed: `<root>/.orgasmic/project.org`, a path escaping
/// through `..`, a record for a file that no longer exists, or a record whose
/// file has since been replaced. None of those are ever evicted either, because
/// eviction is scoped to direct children of the sessions directory — so a
/// semantically corrupt entry survived every refresh and kept answering
/// inventory queries with a run the session files never described.
///
/// Five checks, each refusing one of those shapes:
///
/// 1. the path is a **direct child** of this project's sessions directory, so
///    session-directory authority — not the project root — is what admits it;
/// 2. no path component is `..`, so a normalized-looking parent cannot be
///    reached by escaping and coming back;
/// 3. the file is a **regular file** today, not a symlink, directory, or fifo;
/// 4. its **current identity** (device/inode/length/mtime) is exactly the one
///    the entry was derived from;
/// 5. every SEMANTIC field it claims — lifecycle, terminal verdict, driver,
///    transport, worktree pair — is reproduced by re-deriving it from the
///    entry's own retained envelope set (orgasmic:TASK-FZB6T.2 finding 4).
///
/// Check 5 is the one the first four do not cover. Path and fingerprint prove
/// which BYTES an entry is about; they prove nothing about what it says those
/// bytes mean. A record claiming a live ACP session is a terminal rmux run
/// passed all four checks and then authorized its own deletion.
///
/// A refused entry costs one bounded re-scan of a file that is on disk anyway.
fn snapshot_entry_is_admissible(entry: &RunCatalogEntry, sessions_dir: &Path) -> bool {
    let path = entry.session_path.as_path();
    if path.parent() != Some(sessions_dir) {
        return false;
    }
    if path
        .components()
        .any(|component| matches!(component, std::path::Component::ParentDir))
    {
        return false;
    }
    if path.extension().and_then(|ext| ext.to_str()) != Some("jsonl") {
        return false;
    }
    let Ok(metadata) = std::fs::symlink_metadata(path) else {
        return false;
    };
    if !metadata.is_file() || metadata.file_type().is_symlink() {
        return false;
    }
    if SessionFileFingerprint::of(&metadata) != entry.fingerprint {
        return false;
    }
    snapshot_entry_semantics_are_self_consistent(entry)
}

/// Whether an entry's semantic claims are reproduced by its own source
/// envelopes.
///
/// An `unreadable` record was built from no envelopes at all, so it has no
/// derivation to check — instead it must claim nothing, which is the only
/// shape [`unreadable_entry`] produces.
fn snapshot_entry_semantics_are_self_consistent(entry: &RunCatalogEntry) -> bool {
    if entry.unreadable {
        return entry.lifecycle_envelopes.is_empty()
            && entry.terminal.is_none()
            && entry.transport.is_none()
            && entry.harness.is_none()
            && entry.native.is_none()
            && entry.final_release_outcome.is_none()
            && entry.driver_terminal_event.is_none()
            && !entry.run_meta_recorded
            && !entry.external_registration;
    }
    derive_semantics(&entry.lifecycle_envelopes, entry.final_envelope_retained)
        == claimed_semantics(entry)
        && snapshot_authority_verdict_is_consistent(entry)
}

/// Whether a snapshot entry's `worktree_authority` can be the verdict its own
/// recorded `RunMeta` produces.
///
/// orgasmic:TASK-FZB6T.3 finding 4 — semantic validation checked every derived
/// field EXCEPT this one, so a snapshot that passed every other check could
/// still carry a verdict contradicting the record it sits on: `Verified` with no
/// worktree recorded at all, `Unrecorded` while recording one, `Verified` for a
/// project the record says it does not belong to.
///
/// Only the part of [`verify_worktree_authority`] that does not touch the
/// filesystem is checked. Where the recorded metadata is complete and
/// consistent, the verdict is decided by what is on disk NOW, and all three of
/// `Verified`, `Tombstoned` and `Mismatched` are reachable — including
/// `Tombstoned` on a path that is occupied again, which is the whole point of
/// the durable ledger (dec_BBPW4 item 2).
fn snapshot_authority_verdict_is_consistent(entry: &RunCatalogEntry) -> bool {
    match &entry.worktree_authority {
        WorktreeAuthority::Unidentified => entry.project_id.is_none(),
        WorktreeAuthority::Unrecorded => {
            entry.project_id.is_some()
                && (!entry.run_meta_recorded || entry.run_meta_worktree.is_none())
        }
        WorktreeAuthority::Mismatched { recorded } => {
            entry.project_id.is_some()
                && entry.run_meta_recorded
                && entry.run_meta_worktree.as_ref() == Some(recorded)
        }
        WorktreeAuthority::Verified { .. }
        | WorktreeAuthority::Tombstoned { .. }
        | WorktreeAuthority::Unprovable { .. } => {
            entry.project_id.is_some()
                && entry.run_meta_recorded
                && entry.run_meta_worktree.is_some()
                && entry.run_meta_project.as_deref() == entry.project_id.as_deref()
        }
    }
}

/// The durable tombstone ledger's format. A file this build cannot vouch for is
/// refused — and, unlike the catalog, refusing it does NOT mean rebuilding from
/// session bytes, because session bytes are exactly what cannot answer this
/// question. It means every run it named stays unproven, which fails closed.
const TOMBSTONE_LEDGER_VERSION: u32 = 1;

/// Runs whose recorded worktree has been observed gone, and the path each one
/// recorded (orgasmic:TASK-FZB6T.3 finding 4 / dec_BBPW4 item 2).
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct TombstoneLedger {
    #[serde(default)]
    version: u32,
    /// run id -> the worktree path that run recorded.
    #[serde(default)]
    tombstoned: BTreeMap<String, PathBuf>,
}

/// What one read of a project's tombstone ledger found.
///
/// orgasmic:TASK-FZB6T.4 finding 2 — the old `load` collapsed all four of these
/// into an empty ledger, and only a POSITIVE ledger hit overrules a rebuilt
/// entry's re-derived `Verified`. So a damaged ledger read as "nothing is
/// tombstoned", a dead run came back, and `/api/runs` resumed attach probing on
/// a worktree that no longer exists. "Absent" and "I could not read the
/// authority" are opposite answers and must never produce the same behaviour.
#[derive(Debug)]
pub enum TombstoneLedgerState {
    /// No ledger has ever been written for this project. There is nothing to
    /// overrule and nothing was lost: an empty ledger is the TRUTH here.
    Absent,
    /// A ledger this build wrote and can vouch for.
    Loaded(TombstoneLedger),
    /// The file exists but could not be read or decoded. Its contents are
    /// unknown, so every run it might have named is unproven.
    Corrupt { error: String },
    /// The file decodes but declares a version this build does not know. A
    /// newer runtime may express tombstones in a shape this one would read as
    /// absent, which is the same silent-empty failure in a different costume.
    VersionMismatch { found: u32 },
}

impl TombstoneLedgerState {
    /// The reason this state cannot answer "is this run tombstoned?", or `None`
    /// when it can.
    pub fn unusable_reason(&self) -> Option<String> {
        match self {
            Self::Absent | Self::Loaded(_) => None,
            Self::Corrupt { error } => Some(format!("tombstone ledger is unreadable: {error}")),
            Self::VersionMismatch { found } => Some(format!(
                "tombstone ledger declares version {found}, this build knows \
                 {TOMBSTONE_LEDGER_VERSION}"
            )),
        }
    }
}

/// Why a ledger write did not happen. Every variant means the terminal fact this
/// process observed is NOT durable yet, which the caller reports rather than
/// swallows.
#[derive(Debug)]
pub enum TombstoneSaveError {
    /// The on-disk ledger could not be vouched for, so merging into it would
    /// mean overwriting authority this build cannot read. Refused.
    WouldOverwriteUnreadable(String),
    Io(std::io::Error),
}

impl std::fmt::Display for TombstoneSaveError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::WouldOverwriteUnreadable(reason) => write!(
                f,
                "refusing to overwrite a tombstone ledger this build cannot read ({reason}); \
                 the terminal fact was NOT persisted"
            ),
            Self::Io(error) => write!(f, "{error}"),
        }
    }
}

/// Staging names have to be unique per attempt: one fixed `.json.tmp` is a path
/// two concurrent writers race on, and the loser's rename publishes the winner's
/// half-written bytes.
static TOMBSTONE_STAGING_SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// The exclusive per-project ledger lock, held for a whole read-merge-write.
struct TombstoneLock {
    file: std::fs::File,
}

impl TombstoneLock {
    fn acquire(project_root: &Path) -> std::io::Result<Self> {
        let path = project_root.join(TOMBSTONE_LOCK_REL_PATH);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let file = std::fs::OpenOptions::new()
            .create(true)
            .read(true)
            .write(true)
            .truncate(false)
            .open(&path)?;
        // Explicit trait call: `File::lock_exclusive` is 1.89 and would shadow
        // `fs2` here, above the workspace MSRV.
        fs2::FileExt::lock_exclusive(&file)?;
        Ok(Self { file })
    }
}

impl Drop for TombstoneLock {
    fn drop(&mut self) {
        let _ = fs2::FileExt::unlock(&self.file);
    }
}

impl TombstoneLedger {
    /// Load a project's ledger, stating WHICH of the four answers this is.
    ///
    /// Absent and empty are the same behaviour and a truthful one. Unreadable
    /// and foreign-version are not: they are reported, and the caller fails
    /// closed on them (orgasmic:TASK-FZB6T.4 finding 2).
    pub fn load(project_root: &Path) -> TombstoneLedgerState {
        let path = project_root.join(TOMBSTONE_REL_PATH);
        let source = match std::fs::read_to_string(&path) {
            Ok(source) => source,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                return TombstoneLedgerState::Absent
            }
            Err(error) => {
                return TombstoneLedgerState::Corrupt {
                    error: error.to_string(),
                }
            }
        };
        match serde_json::from_str::<Self>(&source) {
            Ok(ledger) if ledger.version == TOMBSTONE_LEDGER_VERSION => {
                TombstoneLedgerState::Loaded(ledger)
            }
            Ok(ledger) => TombstoneLedgerState::VersionMismatch {
                found: ledger.version,
            },
            Err(error) => TombstoneLedgerState::Corrupt {
                error: error.to_string(),
            },
        }
    }

    fn contains(&self, run_id: &str) -> bool {
        self.tombstoned.contains_key(run_id)
    }

    fn record(&mut self, run_id: &str, recorded: &Path) -> bool {
        if run_id.is_empty() {
            return false;
        }
        self.tombstoned
            .insert(run_id.to_string(), recorded.to_path_buf())
            .is_none()
    }

    /// Persist the ledger, MERGING with whatever is on disk, under the
    /// per-project lock, durably.
    ///
    /// Merged rather than replaced because this file is authority: another
    /// daemon, or this one before a restart, may have recorded a tombstone this
    /// process never observed, and a last-writer-wins overwrite would delete a
    /// terminal fact. A tombstone is only ever added.
    ///
    /// orgasmic:TASK-FZB6T.4 finding 2 — the merge used to read through the
    /// lossy `load`, so a corrupt file merged into an EMPTY map and the rename
    /// destroyed it, contradicting the comment that claimed otherwise. And the
    /// read-merge-write was unsynchronised, staged through one fixed
    /// `.json.tmp`, and neither the file nor its directory was fsynced: two
    /// writers could both read generation N and each publish a different N+1,
    /// and a crash could lose a rename this function had already reported as
    /// persisted. All four are closed here — flock across processes, refusal on
    /// unreadable authority, unique staging, and fsync of file then directory.
    fn save(&self, project_root: &Path) -> Result<(), TombstoneSaveError> {
        let path = project_root.join(TOMBSTONE_REL_PATH);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).map_err(TombstoneSaveError::Io)?;
        }
        // The lock covers the read, the merge and the rename. Anything narrower
        // is the lost update it exists to stop.
        let _lock = TombstoneLock::acquire(project_root).map_err(TombstoneSaveError::Io)?;

        let mut merged = match Self::load(project_root) {
            TombstoneLedgerState::Loaded(ledger) => ledger,
            TombstoneLedgerState::Absent => Self::default(),
            unusable => {
                return Err(TombstoneSaveError::WouldOverwriteUnreadable(
                    unusable
                        .unusable_reason()
                        .unwrap_or_else(|| "unknown".to_string()),
                ))
            }
        };
        merged.version = TOMBSTONE_LEDGER_VERSION;
        for (run_id, recorded) in &self.tombstoned {
            merged
                .tombstoned
                .entry(run_id.clone())
                .or_insert_with(|| recorded.clone());
        }
        let bytes = serde_json::to_vec_pretty(&merged)
            .map_err(|error| TombstoneSaveError::Io(std::io::Error::other(error.to_string())))?;

        let seq = TOMBSTONE_STAGING_SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let staged = path.with_extension(format!("json.{}.{seq}.tmp", std::process::id()));
        let write = (|| -> std::io::Result<()> {
            use std::io::Write as _;
            let mut file = std::fs::File::create(&staged)?;
            file.write_all(&bytes)?;
            file.flush()?;
            // Durable BEFORE the rename, or the rename can publish a file whose
            // contents never reached the disk.
            file.sync_all()?;
            drop(file);
            std::fs::rename(&staged, &path)?;
            // A rename is only durable once its DIRECTORY is.
            if let Some(parent) = path.parent() {
                std::fs::File::open(parent)?.sync_all()?;
            }
            Ok(())
        })();
        if write.is_err() {
            let _ = std::fs::remove_file(&staged);
        }
        write.map_err(TombstoneSaveError::Io)
    }
}

/// One filesystem observation of the path a cached authority verdict names.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct AuthorityProbe {
    /// Identity of the directory currently at the recorded path, if any.
    current: Option<DirIdentity>,
    /// Something exists at the path — used only where no durable identity is
    /// available (non-unix).
    exists: bool,
}

/// One stat against the path a cached verdict names.
///
/// `None` when the verdict cannot change: a mismatch is a statement about
/// project identity, which does not change under a live daemon; the two "no
/// authority recorded" verdicts are properties of the session file itself; and
/// a tombstone is terminal (dec_BBPW4), so probing its path would be a stat
/// whose answer is never used.
///
/// Blocking; the refresh runs this outside the catalog mutex.
fn probe_authority_path(authority: &WorktreeAuthority) -> Option<AuthorityProbe> {
    let path = match authority {
        WorktreeAuthority::Verified { worktree, .. } => worktree.as_path(),
        // `Unprovable` is a statement about the LEDGER, not about the path: no
        // stat can change it, and re-probing would only re-derive the verdict
        // that could not be checked in the first place.
        WorktreeAuthority::Tombstoned { .. }
        | WorktreeAuthority::Mismatched { .. }
        | WorktreeAuthority::Unrecorded
        | WorktreeAuthority::Unidentified
        | WorktreeAuthority::Unprovable { .. } => return None,
    };
    Some(AuthorityProbe {
        current: DirIdentity::at(path),
        exists: path.exists(),
    })
}

/// Re-derive a cached authority verdict against one [`AuthorityProbe`].
/// `None` means "unchanged".
///
/// One transition, and it is one-way: a verified worktree whose directory
/// object is gone becomes tombstoned. There is no way back.
///
/// orgasmic:TASK-FZB6T.2 finding 7 / dec_BBPW4 — the revival path is gone
/// rather than strengthened. It readmitted a run when the recorded path held
/// the recorded device and inode, and inode numbers are REUSABLE: an unrelated
/// checkout could eventually satisfy it, while a legitimately returned volume
/// with renumbered inodes never could, and a tombstone that never had an
/// identity could never recover at all. It was unsound in one direction and
/// useless in the other. A run whose recorded worktree is gone is not
/// recoverable under that worktree; the recovery path is a new run, not a
/// revived attach candidate.
///
/// Blocking; runs outside the catalog mutex.
fn reverify_authority(
    previous: &WorktreeAuthority,
    probe: AuthorityProbe,
) -> Option<WorktreeAuthority> {
    let WorktreeAuthority::Verified { worktree, identity } = previous else {
        return None;
    };
    let still_the_same = match identity {
        // Durable identity available: the directory object must be the same
        // one, not merely a directory with the same name.
        Some(identity) => probe.current == Some(*identity),
        // No durable identity (non-unix): existence is all there is.
        None => probe.exists,
    };
    if still_the_same {
        return None;
    }
    Some(WorktreeAuthority::Tombstoned {
        recorded: worktree.clone(),
        verified_identity: *identity,
    })
}

/// Decide worktree authority from the recorded `RunMeta` and the filesystem.
///
/// The verdict order matters: an unidentified containing project beats
/// everything (nothing can be verified against no project), then a missing
/// `RunMeta`, then a project mismatch — and only after all three does the
/// existence of the path decide tombstoned-vs-verified. That ordering is what
/// keeps the reasons the same strings the pre-catalog inventory reported.
pub fn verify_worktree_authority(
    project_id: Option<&str>,
    run_meta: Option<(Option<String>, Option<PathBuf>)>,
) -> WorktreeAuthority {
    let Some(project_id) = project_id else {
        return WorktreeAuthority::Unidentified;
    };
    let Some((embedded_project, recorded)) = run_meta else {
        return WorktreeAuthority::Unrecorded;
    };
    let Some(recorded) = recorded else {
        return WorktreeAuthority::Unrecorded;
    };
    if embedded_project.as_deref() != Some(project_id) {
        return WorktreeAuthority::Mismatched { recorded };
    }
    // A recorded worktree that is not on disk is the pruned case. It is stated
    // before canonicalize so the reason is "gone", not "invalid": those are
    // different operator facts and only one of them is stable.
    if !recorded.exists() {
        return WorktreeAuthority::Tombstoned {
            recorded,
            verified_identity: None,
        };
    }
    let Ok(canonical) = recorded.canonicalize() else {
        return WorktreeAuthority::Mismatched { recorded };
    };
    match crate::api::read_existing_project_identity(&canonical.join(".orgasmic/project.org")) {
        Ok(identity) if identity.project_id == project_id => {
            // Identity of the canonical path, because that is the path a later
            // probe stats and the path a tombstone would record.
            let identity = DirIdentity::at(&canonical);
            WorktreeAuthority::Verified {
                worktree: canonical,
                identity,
            }
        }
        _ => WorktreeAuthority::Mismatched { recorded },
    }
}

/// Reduce a bounded scan's retained envelopes to the compact set the catalog
/// stores.
///
/// Lifecycle envelopes are kept verbatim: they are authority and they are
/// small. Driver events are reduced to `type` plus, for `ready`, the
/// `protocol_version` — the only driver-event body any inventory consumer reads
/// (it is how a session with no persisted run address names its driver). The
/// `capabilities` blob a `ready` frame carries is the largest thing the
/// lifecycle scanner retains and nothing reads it, so it is dropped here rather
/// than copied into a durable snapshot.
fn compact_envelopes(envelopes: &[SessionEnvelope]) -> Vec<SessionEnvelope> {
    envelopes
        .iter()
        .map(|envelope| {
            if envelope.kind != SessionEventKind::DriverEvent {
                return envelope.clone();
            }
            let mut compact = envelope.clone();
            let ty = envelope.event.get("type").and_then(Value::as_str);
            compact.event = match (ty, envelope.event.get("protocol_version")) {
                (Some(ty), Some(protocol)) => {
                    json!({"type": ty, "protocol_version": protocol.clone()})
                }
                (Some(ty), None) => json!({"type": ty}),
                (None, _) => Value::Object(Default::default()),
            };
            compact
        })
        .collect()
}

/// Every semantic fact a catalog entry claims, as derived from the compact
/// envelope set the entry carries.
///
/// orgasmic:TASK-FZB6T.2 finding 4 — snapshot admission checked JSON shape,
/// version, session path and file fingerprint, then took `lifecycle`,
/// `terminal`, `driver` and `transport` VERBATIM. A valid-JSON but semantically
/// corrupt snapshot could therefore present a live ACP run as a terminal rmux
/// one, and a steady-state refresh treats the unchanged fingerprint as a cache
/// hit and never re-derives it.
///
/// The entry already retains the envelope set it was derived from — that is
/// what makes serving classification from the catalog provably the same
/// computation. So the derivation is factored out here and run twice: once when
/// the entry is built, and once when a snapshot entry asks to be admitted. An
/// entry whose claims its own source envelopes do not reproduce is refused and
/// rebuilt. This is the binding the deletion path needs and it costs no
/// filesystem read at all.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct DerivedSemantics {
    run_id: String,
    runtime_id: String,
    boot_id: String,
    task_id: Option<String>,
    kind: Option<String>,
    worker_id: Option<String>,
    stage: Option<String>,
    transport: Option<String>,
    harness: Option<String>,
    native: Option<NativeRuntimeRef>,
    run_meta_project: Option<String>,
    run_meta_worktree: Option<PathBuf>,
    run_meta_recorded: bool,
    external_registration: bool,
    replacement_run_id: Option<String>,
    replacement_session_path: Option<PathBuf>,
    terminal: Option<TerminalRecord>,
    final_release_outcome: Option<ReleaseOutcome>,
    driver_terminal_event: Option<String>,
}

/// Derive every semantic fact from one compact envelope set.
///
/// `final_envelope_retained` is the scan's own statement that the file's
/// genuine last line survived the bounded window; without it a release cannot
/// be proven and the entry must not claim one.
pub(crate) fn derive_semantics(
    envelopes: &[SessionEnvelope],
    final_envelope_retained: bool,
) -> DerivedSemantics {
    // Idempotent: the stored set is already one segment, and re-segmenting it
    // is what makes the two derivations provably the same function.
    let envelopes = latest_run_segment(envelopes);
    let first = envelopes.first();

    let mut task_id = None;
    let mut kind = None;
    let mut worker_id = None;
    let mut transport = None;
    let mut harness = None;
    let mut stage = None;
    let mut native = None;
    let mut replacement_run_id = None;
    let mut replacement_session_path = None;
    let mut external_registration = false;
    let mut run_meta: Option<(Option<String>, Option<PathBuf>)> = None;

    for envelope in envelopes {
        if envelope.kind != SessionEventKind::Lifecycle {
            continue;
        }
        let Ok(lifecycle) = serde_json::from_value::<Lifecycle>(envelope.event.clone()) else {
            continue;
        };
        match lifecycle {
            Lifecycle::Acquire {
                task_id: task,
                kind: run_kind,
                worker_id: worker,
            } => {
                task_id = Some(task);
                kind = Some(run_kind);
                worker_id = Some(worker);
            }
            Lifecycle::RunMeta {
                transport: recorded_transport,
                harness: recorded_harness,
                project_id: embedded,
                worktree,
                ..
            } => {
                external_registration = recorded_transport.trim().eq_ignore_ascii_case("external");
                transport = Some(recorded_transport);
                harness = recorded_harness;
                run_meta = Some((embedded, worktree));
            }
            Lifecycle::StageMeta { stage: launched } => stage = Some(launched),
            Lifecycle::NativeRuntime {
                provider,
                session_id,
                session_path,
                ..
            } => {
                native = Some(NativeRuntimeRef {
                    provider,
                    session_id,
                    session_path,
                })
            }
            Lifecycle::RecoveryOrigin {
                replacement_run_id: replacement,
                replacement_session_path: replacement_path,
                ..
            } => {
                replacement_run_id = Some(replacement);
                replacement_session_path = Some(replacement_path);
            }
            _ => {}
        }
    }

    let release = final_release_outcome(final_envelope_retained, envelopes);
    let driver_event = driver_terminal_event(envelopes);
    let terminal = terminal_record(
        release,
        driver_event.as_ref(),
        envelopes.last().map(|envelope| envelope.time),
        external_registration,
    );
    DerivedSemantics {
        run_id: first.map(|e| e.run_id.clone()).unwrap_or_default(),
        runtime_id: first.map(|e| e.runtime_id.clone()).unwrap_or_default(),
        boot_id: first.map(|e| e.boot_id.clone()).unwrap_or_default(),
        task_id,
        kind,
        worker_id,
        stage,
        transport,
        harness,
        native,
        run_meta_project: run_meta.as_ref().and_then(|(project, _)| project.clone()),
        run_meta_worktree: run_meta.as_ref().and_then(|(_, worktree)| worktree.clone()),
        run_meta_recorded: run_meta.is_some(),
        external_registration,
        replacement_run_id,
        replacement_session_path,
        terminal,
        final_release_outcome: release,
        driver_terminal_event: driver_event.map(|(event, _)| event),
    }
}

/// Build a catalog entry from a bounded lifecycle scan.
///
/// The semantics are derived from the COMPACT envelope set the entry will
/// store, not from the raw scan, so the entry's claims are reproducible from
/// exactly the bytes it carries (orgasmic:TASK-FZB6T.2 finding 4). Compaction
/// only reduces driver-event bodies to their `type`, which is the only part of
/// a driver event any derivation reads, so the two are the same computation.
pub(crate) fn entry_from_scan(
    scan: &SessionLifecycleScan,
    path: &Path,
    project_id: Option<&str>,
    project_root: &Path,
    fingerprint: SessionFileFingerprint,
) -> RunCatalogEntry {
    let compact = compact_envelopes(latest_run_segment(&scan.envelopes));
    let semantics = derive_semantics(&compact, scan.final_envelope_retained);
    let run_meta = semantics.run_meta_recorded.then(|| {
        (
            semantics.run_meta_project.clone(),
            semantics.run_meta_worktree.clone(),
        )
    });
    RunCatalogEntry {
        run_id: semantics.run_id,
        runtime_id: semantics.runtime_id,
        boot_id: semantics.boot_id,
        session_path: path.to_path_buf(),
        project_id: project_id.map(str::to_string),
        project_root: Some(project_root.to_path_buf()),
        task_id: semantics.task_id,
        kind: semantics.kind,
        worker_id: semantics.worker_id,
        stage: semantics.stage,
        transport: semantics.transport,
        harness: semantics.harness,
        native: semantics.native,
        worktree_authority: verify_worktree_authority(project_id, run_meta),
        run_meta_project: semantics.run_meta_project,
        run_meta_worktree: semantics.run_meta_worktree,
        run_meta_recorded: semantics.run_meta_recorded,
        terminal: semantics.terminal,
        final_release_outcome: semantics.final_release_outcome,
        driver_terminal_event: semantics.driver_terminal_event,
        external_registration: semantics.external_registration,
        replacement_run_id: semantics.replacement_run_id,
        replacement_session_path: semantics.replacement_session_path,
        scan_truncated: scan.truncated,
        final_line_run_id: scan.final_line_run_id.clone(),
        final_envelope_retained: scan.final_envelope_retained,
        unreadable: false,
        file_bytes: scan.file_bytes,
        indexed_at: Utc::now(),
        fingerprint,
        lifecycle_envelopes: compact,
    }
}

/// What a fresh bounded read of a session file proves about the run it holds.
///
/// orgasmic:TASK-FZB6T.2 finding 4 / dec_BBPW4 — the catalog is disposable
/// derived state and is never deletion authority. Maintenance uses it for the
/// candidate path list and for nothing else; every fact that authorizes an
/// irreversible operation is re-derived here, from the file's CURRENT bytes,
/// through exactly the derivation the inventory uses.
#[derive(Debug, Clone)]
pub struct SessionAuthority {
    pub run_id: String,
    pub terminal: Option<TerminalRecord>,
    pub transport: Option<String>,
    pub harness: Option<String>,
    /// The bounded scan skipped the middle of the file, so what it did not read
    /// cannot be part of any proof.
    pub scan_truncated: bool,
    /// The file's genuine final envelope was retained, so a terminal verdict
    /// may be made from it. A truncated scan without this proves nothing about
    /// how the run ended.
    pub final_envelope_retained: bool,
    /// `run_id` of the file's last physical line. Equal to [`Self::run_id`]
    /// exactly when the segment that was derived is the segment the file
    /// actually ends with (orgasmic:TASK-7QM8M).
    pub final_line_run_id: Option<String>,
}

impl SessionAuthority {
    pub fn is_terminal(&self) -> bool {
        self.terminal.is_some()
    }

    /// Whether the terminal verdict rests on bytes that were actually read.
    ///
    /// A bounded scan may legitimately skip the middle of a multi-megabyte
    /// legacy session — that is what makes maintenance affordable at all. What
    /// it may not do is decide "this run ended" from a segment it cannot prove
    /// is the file's LAST segment: on a truncated scan the newest retained
    /// segment and the newest segment on disk are different questions
    /// (orgasmic:TASK-7QM8M), and `final_line_run_id` is the one fact that
    /// makes them the same. An untruncated scan read the whole file, so there
    /// is nothing left to prove.
    pub fn terminal_is_proven(&self) -> bool {
        if !self.is_terminal() {
            return false;
        }
        if !self.scan_truncated {
            return true;
        }
        self.final_line_run_id.as_deref() == Some(self.run_id.as_str())
    }

    /// `transport/harness`, matching [`RunCatalogEntry::driver_label`].
    pub fn driver_label(&self) -> String {
        match (self.transport.as_deref(), self.harness.as_deref()) {
            (Some(transport), Some(harness)) => format!("{transport}/{harness}"),
            (Some(transport), None) => transport.to_string(),
            (None, _) => "unknown".to_string(),
        }
    }
}

/// Re-derive one session file's terminal verdict and transport from disk.
pub fn derive_session_authority(
    path: &Path,
    budget: SessionScanBudget,
) -> std::io::Result<SessionAuthority> {
    let scan = scan_session_lifecycle(path, budget)
        .map_err(|error| std::io::Error::other(error.to_string()))?;
    let compact = compact_envelopes(latest_run_segment(&scan.envelopes));
    let semantics = derive_semantics(&compact, scan.final_envelope_retained);
    Ok(SessionAuthority {
        run_id: semantics.run_id,
        terminal: semantics.terminal,
        transport: semantics.transport,
        harness: semantics.harness,
        scan_truncated: scan.truncated,
        final_envelope_retained: scan.final_envelope_retained,
        final_line_run_id: scan.final_line_run_id.clone(),
    })
}

/// The semantics one entry CLAIMS, in the shape [`derive_semantics`] returns —
/// so the two can be compared field for field.
fn claimed_semantics(entry: &RunCatalogEntry) -> DerivedSemantics {
    DerivedSemantics {
        run_id: entry.run_id.clone(),
        runtime_id: entry.runtime_id.clone(),
        boot_id: entry.boot_id.clone(),
        task_id: entry.task_id.clone(),
        kind: entry.kind.clone(),
        worker_id: entry.worker_id.clone(),
        stage: entry.stage.clone(),
        transport: entry.transport.clone(),
        harness: entry.harness.clone(),
        native: entry.native.clone(),
        run_meta_project: entry.run_meta_project.clone(),
        run_meta_worktree: entry.run_meta_worktree.clone(),
        run_meta_recorded: entry.run_meta_recorded,
        external_registration: entry.external_registration,
        replacement_run_id: entry.replacement_run_id.clone(),
        replacement_session_path: entry.replacement_session_path.clone(),
        terminal: entry.terminal.clone(),
        final_release_outcome: entry.final_release_outcome,
        driver_terminal_event: entry.driver_terminal_event.clone(),
    }
}

/// The raw `Release` outcome on the file's genuine final envelope.
///
/// `None` when the scan dropped that line as transcript (the normal shape for a
/// run that is still writing) or when the final envelope is not a release —
/// only the genuine final envelope can prove a release, and treating a newest
/// RETAINED lifecycle line as the end of the run would tombstone a live one.
fn final_release_outcome(
    final_envelope_retained: bool,
    envelopes: &[SessionEnvelope],
) -> Option<ReleaseOutcome> {
    if !final_envelope_retained {
        return None;
    }
    let last = envelopes.last()?;
    if last.kind != SessionEventKind::Lifecycle {
        return None;
    }
    match serde_json::from_value::<Lifecycle>(last.event.clone()).ok()? {
        Lifecycle::Release { outcome, .. } => Some(outcome),
        _ => None,
    }
}

/// `(normalized event name, time)` of the newest terminal driver event.
fn driver_terminal_event(envelopes: &[SessionEnvelope]) -> Option<(String, DateTime<Utc>)> {
    envelopes.iter().rev().find_map(|envelope| {
        if envelope.kind != SessionEventKind::DriverEvent {
            return None;
        }
        match envelope.event.get("type").and_then(Value::as_str) {
            Some("run_complete") => Some(("run_complete".to_string(), envelope.time)),
            Some("run_fail") | Some("run_error") => Some(("run_fail".to_string(), envelope.time)),
            _ => None,
        }
    })
}

/// The semantic terminal verdict the inventory classifier consumes.
///
/// Reproduces the pre-catalog `classify_session_dir` chain exactly, including
/// its one asymmetry: an `Interrupted` release short-circuits to "not terminal"
/// and does NOT fall through to the terminal driver events. An interrupted run
/// is the recoverable case, and letting an earlier `run_complete` outrank the
/// release that says otherwise would tombstone runs the operator still needs.
fn terminal_record(
    release: Option<ReleaseOutcome>,
    driver_event: Option<&(String, DateTime<Utc>)>,
    last_time: Option<DateTime<Utc>>,
    external_registration: bool,
) -> Option<TerminalRecord> {
    match release {
        Some(ReleaseOutcome::Interrupted) => return None,
        Some(outcome) => {
            return Some(TerminalRecord::Release {
                outcome,
                at: last_time.unwrap_or_else(Utc::now),
            })
        }
        None => {}
    }
    if let Some((event, at)) = driver_event {
        return Some(TerminalRecord::DriverEvent {
            event: event.clone(),
            at: *at,
        });
    }
    external_registration.then_some(TerminalRecord::ExternalRegistrationEnded)
}

fn unreadable_entry(
    path: &Path,
    project_id: Option<&str>,
    project_root: &Path,
    fingerprint: SessionFileFingerprint,
    file_bytes: u64,
) -> RunCatalogEntry {
    RunCatalogEntry {
        run_id: path
            .file_stem()
            .and_then(|stem| stem.to_str())
            .unwrap_or("unknown")
            .to_string(),
        runtime_id: String::new(),
        boot_id: String::new(),
        session_path: path.to_path_buf(),
        project_id: project_id.map(str::to_string),
        project_root: Some(project_root.to_path_buf()),
        task_id: None,
        kind: None,
        worker_id: None,
        stage: None,
        transport: None,
        harness: None,
        native: None,
        worktree_authority: WorktreeAuthority::Unrecorded,
        run_meta_project: None,
        run_meta_worktree: None,
        run_meta_recorded: false,
        terminal: None,
        final_release_outcome: None,
        driver_terminal_event: None,
        external_registration: false,
        replacement_run_id: None,
        replacement_session_path: None,
        scan_truncated: false,
        final_line_run_id: None,
        final_envelope_retained: false,
        unreadable: true,
        file_bytes,
        indexed_at: Utc::now(),
        fingerprint,
        lifecycle_envelopes: Vec::new(),
    }
}

/// Newest contiguous run segment by `run_id`, mirroring the inventory's own
/// segmentation of second-granularity manager files.
fn latest_run_segment(envelopes: &[SessionEnvelope]) -> &[SessionEnvelope] {
    let Some(latest) = envelopes.last().map(|envelope| envelope.run_id.as_str()) else {
        return envelopes;
    };
    let start = envelopes
        .iter()
        .rposition(|envelope| envelope.run_id != latest)
        .map_or(0, |index| index + 1);
    &envelopes[start..]
}

// ---------------------------------------------------------------------------
// Legacy storage accounting (orgasmic:TASK-FZB6T item 4, dry-run half)
// ---------------------------------------------------------------------------

/// Per-(driver, harness, event class) accounting of what a session directory
/// actually holds.
#[derive(Debug, Clone, Default, Serialize)]
pub struct HistoryClassTotals {
    pub files: u64,
    pub lines: u64,
    pub bytes: u64,
}

/// One row of `run history inspect`.
#[derive(Debug, Clone, Serialize)]
pub struct HistoryBucket {
    /// `transport/harness` as recorded by `RunMeta`, or `unknown`.
    pub driver: String,
    /// Event class: `lifecycle`, `rendered_tui`, `semantic`, `pane_activity`,
    /// `babysitter_summary`, `note`, or `unparsed`.
    pub event_class: String,
    #[serde(flatten)]
    pub totals: HistoryClassTotals,
    /// Whether this class is reclaimable under the retention policy.
    pub reclaimable: bool,
}

/// Event classes, in the order `run history inspect` reports them.
///
/// `blank` and `torn` were added by orgasmic:TASK-FZB6T.1 finding 2: the
/// accounting claims every byte on disk lands in exactly one class, and before
/// them a blank line was skipped outright (its bytes vanished from the total)
/// while a final record with no terminating newline was charged a newline it
/// does not have. Both are ordinary shapes — a session file torn by a kill ends
/// mid-line by definition.
pub const EVENT_CLASSES: [&str; 9] = [
    "lifecycle",
    "rendered_tui",
    "semantic",
    "pane_activity",
    "babysitter_summary",
    "note",
    "unparsed",
    "blank",
    "torn",
];

/// Classify one raw JSONL line by PROVING its envelope structure.
///
/// orgasmic:TASK-FZB6T.2 finding 1 — this used to take the first byte
/// occurrence of `"type":"` anywhere in the line's first 64 KiB. A perfectly
/// valid `tool_result` whose nested payload happens to carry
/// `{"type":"text_chunk"}` before the envelope's own discriminator was
/// therefore classified `rendered_tui`, and the maintenance pass deleted the
/// whole record — tool and result evidence destroyed by a substring match.
///
/// So the discriminators are now proven rather than found: one structural pass
/// over the line reads `kind` as a member of the top-level object and `type` as
/// a member of the top-level `event` object, and nothing that is nested deeper
/// can be mistaken for either. Still no payload is materialized — the scan
/// allocates only the two short discriminator strings and never copies a
/// value — but a line whose structure cannot be proven is classified `unparsed`
/// and is therefore never reclaimable. **Fail closed:** a line this accounting
/// could not read is the last line that should be deleted on its say-so.
pub fn classify_history_line(line: &[u8]) -> &'static str {
    let Some(envelope) = scan_envelope_discriminators(line) else {
        return "unparsed";
    };
    match envelope.kind.as_deref() {
        Some(b"lifecycle") => "lifecycle",
        Some(b"babysitter_summary") => "babysitter_summary",
        Some(b"note") => "note",
        // Driver events: the rendered-TUI class is the legacy `text_chunk`
        // written by a pane transport before dec_WDR5K item 7 — the payload the
        // maintenance command exists to account for. Everything else is
        // semantic evidence.
        Some(b"driver_event") => match envelope.event_type.as_deref() {
            Some(b"pane_activity") => "pane_activity",
            Some(b"text_chunk") => "rendered_tui",
            Some(_) => "semantic",
            None => "unparsed",
        },
        _ => "unparsed",
    }
}

/// The two discriminators an envelope's class is decided from, each proven to
/// sit where the envelope schema puts it.
struct EnvelopeDiscriminators {
    /// The top-level object's `kind`.
    kind: Option<Vec<u8>>,
    /// The top-level `event` object's own `type`.
    event_type: Option<Vec<u8>>,
}

/// Nesting the structural scan will follow before giving up. Envelope payloads
/// are shallow; a deeper one costs bounded work and classifies `unparsed`.
const SCAN_MAX_DEPTH: usize = 64;

/// Longest string the scan will retain as a discriminator candidate. Every
/// discriminator this module knows is a short snake_case token.
const SCAN_MAX_DISCRIMINATOR_BYTES: usize = 64;

/// The bytes JSON permits between tokens: space, horizontal tab, line feed and
/// carriage return, and nothing else (RFC 8259 §2).
///
/// orgasmic:TASK-FZB6T.4 finding 3 — the scan used to skip
/// `u8::is_ascii_whitespace`, which is a DIFFERENT set. Measured against the
/// locked toolchain, that set is `{0x09, 0x0a, 0x0c, 0x0d, 0x20}`: it adds FORM
/// FEED `0x0c`, which JSON forbids. So a `text_chunk` line carrying a form feed
/// between two tokens was walked successfully, classified `rendered_tui`, and
/// its bytes were eligible for deletion — while `serde_json` rejects the very
/// same line. A record no reader accepts is exactly the record this accounting
/// must not delete on its own say-so.
///
/// The finding also named VERTICAL TAB `0x0b`. That half does not reproduce:
/// Rust follows the WhatWG Infra definition, which excludes `0x0b`, so a
/// vertical tab already failed the scan closed. The fixtures below pin BOTH
/// bytes against `serde_json` anyway, because the class — "the scan's whitespace
/// is not JSON's whitespace" — is what must stay closed, not the one byte that
/// happened to be open.
const fn is_json_whitespace(byte: u8) -> bool {
    matches!(byte, b' ' | b'\t' | b'\n' | b'\r')
}

/// A single-pass structural reader over one JSONL line.
///
/// Deliberately not `serde_json`: parsing the line would allocate the whole
/// payload tree, which for a multi-megabyte legacy `text_chunk` is exactly the
/// cost the accounting exists to avoid. This walks the bytes, keeping only the
/// position and the two short discriminators.
struct JsonScan<'a> {
    bytes: &'a [u8],
    pos: usize,
}

impl<'a> JsonScan<'a> {
    fn new(bytes: &'a [u8]) -> Self {
        Self { bytes, pos: 0 }
    }

    fn peek(&mut self) -> Option<u8> {
        while self
            .bytes
            .get(self.pos)
            .is_some_and(|byte| is_json_whitespace(*byte))
        {
            self.pos += 1;
        }
        self.bytes.get(self.pos).copied()
    }

    fn eat(&mut self, byte: u8) -> Option<()> {
        (self.peek()? == byte).then(|| self.pos += 1)
    }

    /// Consume exactly `want`, or refuse.
    fn eat_exact(&mut self, want: &[u8]) -> Option<()> {
        (self.bytes.get(self.pos..self.pos.checked_add(want.len())?) == Some(want))
            .then(|| self.pos += want.len())
    }

    /// Consume one JSON string, VALIDATING it.
    ///
    /// Returns its raw contents when they are a plain, short token usable as a
    /// discriminator, and `Some(None)` when the string is well formed but
    /// escaped or oversized — consumed correctly, never trusted. No
    /// discriminator this module matches contains an escape, so refusing to
    /// decode them costs nothing and keeps the scan allocation-free.
    ///
    /// orgasmic:TASK-FZB6T.3 finding 3 — "consumed correctly" used to mean
    /// "consumed until the next unescaped quote": an unescaped control byte and
    /// an escape sequence no JSON reader accepts both passed. A line the scan
    /// accepted was therefore not necessarily a line `serde_json` accepts, and
    /// the maintenance pass deletes on this verdict. Every rule RFC 8259 puts on
    /// a string is now checked here.
    fn string(&mut self) -> Option<Option<&'a [u8]>> {
        self.eat(b'"')?;
        let start = self.pos;
        let mut escaped = false;
        loop {
            let byte = *self.bytes.get(self.pos)?;
            self.pos += 1;
            match byte {
                b'"' => break,
                // A raw control character is not legal inside a JSON string.
                // `serde_json` refuses one; so does this.
                0x00..=0x1f => return None,
                b'\\' => {
                    escaped = true;
                    self.escape()?;
                }
                _ => {}
            }
        }
        let raw = &self.bytes[start..self.pos - 1];
        Some((!escaped && raw.len() <= SCAN_MAX_DISCRIMINATOR_BYTES).then_some(raw))
    }

    /// Consume one escape sequence whose `\` was already read.
    fn escape(&mut self) -> Option<()> {
        let byte = *self.bytes.get(self.pos)?;
        self.pos += 1;
        match byte {
            b'"' | b'\\' | b'/' | b'b' | b'f' | b'n' | b'r' | b't' => Some(()),
            b'u' => {
                let first = self.hex4()?;
                // A high surrogate is legal only as the first half of a pair
                // and a low surrogate only as the second — the rule
                // `serde_json` applies, so the class this reports stays the
                // class a parse of the same line would report.
                if (0xd800..0xdc00).contains(&first) {
                    self.eat_exact(br"\u")?;
                    (0xdc00..0xe000).contains(&self.hex4()?).then_some(())
                } else if (0xdc00..0xe000).contains(&first) {
                    None
                } else {
                    Some(())
                }
            }
            _ => None,
        }
    }

    /// Consume exactly four hex digits and return their value.
    fn hex4(&mut self) -> Option<u32> {
        let mut value = 0_u32;
        for _ in 0..4 {
            let byte = *self.bytes.get(self.pos)?;
            self.pos += 1;
            value = value * 16 + char::from(byte).to_digit(16)?;
        }
        Some(value)
    }

    /// One or more decimal digits.
    fn digits(&mut self) -> Option<()> {
        let start = self.pos;
        while self.bytes.get(self.pos).is_some_and(u8::is_ascii_digit) {
            self.pos += 1;
        }
        (self.pos > start).then_some(())
    }

    /// Consume one JSON number, per RFC 8259: an optional `-`, an integer part
    /// with no leading zeros, an optional fraction, an optional exponent.
    fn number(&mut self) -> Option<()> {
        if self.bytes.get(self.pos) == Some(&b'-') {
            self.pos += 1;
        }
        match self.bytes.get(self.pos)? {
            b'0' => self.pos += 1,
            b'1'..=b'9' => self.digits()?,
            _ => return None,
        }
        if self.bytes.get(self.pos) == Some(&b'.') {
            self.pos += 1;
            self.digits()?;
        }
        if matches!(self.bytes.get(self.pos), Some(b'e' | b'E')) {
            self.pos += 1;
            if matches!(self.bytes.get(self.pos), Some(b'+' | b'-')) {
                self.pos += 1;
            }
            self.digits()?;
        }
        Some(())
    }

    /// Structurally consume one JSON value of any shape, VALIDATING it.
    ///
    /// orgasmic:TASK-FZB6T.3 finding 3 — the primitive arm used to accept any
    /// non-empty run of bytes up to the next structural character, so `truX`,
    /// `01`, `-`, `NaN` and `+1` were all "values". A record carrying one of
    /// them is INVALID JSON that this scan nonetheless classified
    /// `rendered_tui`, and the maintenance pass then dropped its raw bytes. The
    /// three primitive shapes JSON actually has are now each parsed by their own
    /// grammar, so a line this returns `Some` for is a line a parser accepts.
    fn skip_value(&mut self, depth: usize) -> Option<()> {
        if depth > SCAN_MAX_DEPTH {
            return None;
        }
        match self.peek()? {
            b'"' => self.string().map(|_| ()),
            b'{' => {
                self.pos += 1;
                self.skip_members(depth)
            }
            b'[' => {
                self.pos += 1;
                if self.peek()? == b']' {
                    self.pos += 1;
                    return Some(());
                }
                loop {
                    self.skip_value(depth + 1)?;
                    match self.peek()? {
                        b',' => self.pos += 1,
                        b']' => {
                            self.pos += 1;
                            return Some(());
                        }
                        _ => return None,
                    }
                }
            }
            b't' => self.eat_exact(b"true"),
            b'f' => self.eat_exact(b"false"),
            b'n' => self.eat_exact(b"null"),
            b'-' | b'0'..=b'9' => self.number(),
            _ => None,
        }
    }

    /// Consume the members of an object whose `{` was already read.
    fn skip_members(&mut self, depth: usize) -> Option<()> {
        if self.peek()? == b'}' {
            self.pos += 1;
            return Some(());
        }
        loop {
            self.string()?;
            self.eat(b':')?;
            self.skip_value(depth + 1)?;
            match self.peek()? {
                b',' => self.pos += 1,
                b'}' => {
                    self.pos += 1;
                    return Some(());
                }
                _ => return None,
            }
        }
    }

    /// Consume the members of an object whose `{` was already read, capturing
    /// the string value of its own `wanted` member.
    ///
    /// Last occurrence wins and a non-string value clears the capture, which is
    /// exactly what `serde_json` does with a duplicate key — so the class this
    /// reports is the class a parse of the same line would report.
    fn capture_member(&mut self, wanted: &[u8], depth: usize) -> Option<Option<Vec<u8>>> {
        let mut found = None;
        if self.peek()? == b'}' {
            self.pos += 1;
            return Some(found);
        }
        loop {
            let key = self.string()?;
            self.eat(b':')?;
            if key == Some(wanted) {
                found = match self.peek()? {
                    b'"' => self.string()?.map(<[u8]>::to_vec),
                    _ => {
                        self.skip_value(depth + 1)?;
                        None
                    }
                };
            } else {
                self.skip_value(depth + 1)?;
            }
            match self.peek()? {
                b',' => self.pos += 1,
                b'}' => {
                    self.pos += 1;
                    return Some(found);
                }
                _ => return None,
            }
        }
    }
}

/// Read one line's envelope discriminators, or `None` when the line is not a
/// single well-formed JSON object.
fn scan_envelope_discriminators(line: &[u8]) -> Option<EnvelopeDiscriminators> {
    // orgasmic:TASK-FZB6T.3 finding 3 — JSON is defined over text. A line
    // holding a byte sequence that is not UTF-8 is not a record any reader can
    // decode, so it is not a record this accounting may authorize deleting. One
    // validated pass, no allocation.
    std::str::from_utf8(line).ok()?;
    let mut scan = JsonScan::new(line);
    scan.eat(b'{')?;
    let mut kind = None;
    let mut event_type = None;
    if scan.peek()? == b'}' {
        scan.pos += 1;
    } else {
        loop {
            let key = scan.string()?;
            scan.eat(b':')?;
            match key {
                Some(b"kind") => {
                    kind = match scan.peek()? {
                        b'"' => scan.string()?.map(<[u8]>::to_vec),
                        _ => {
                            scan.skip_value(1)?;
                            None
                        }
                    };
                }
                Some(b"event") => {
                    // Only an OBJECT `event` has a discriminator. Anything else
                    // leaves the class unproven, which fails closed.
                    event_type = if scan.peek()? == b'{' {
                        scan.pos += 1;
                        scan.capture_member(b"type", 1)?
                    } else {
                        scan.skip_value(1)?;
                        None
                    };
                }
                _ => scan.skip_value(1)?,
            }
            match scan.peek()? {
                b',' => scan.pos += 1,
                b'}' => {
                    scan.pos += 1;
                    break;
                }
                _ => return None,
            }
        }
    }
    // Trailing content after the top-level object means this line is not one
    // envelope, whatever the prefix looked like.
    while scan
        .bytes
        .get(scan.pos)
        .is_some_and(|byte| is_json_whitespace(*byte))
    {
        scan.pos += 1;
    }
    (scan.pos == line.len()).then_some(EnvelopeDiscriminators { kind, event_type })
}

/// Whether a run's transport renders into a pane rather than streaming
/// structured turn events.
///
/// orgasmic:TASK-FZB6T.1 finding 2 — this is the whole difference between a
/// `text_chunk` that is a screen repaint and a `text_chunk` that is the
/// assistant's actual words. A pane transport (rmux/tmux) had no other channel
/// before dec_WDR5K item 7, so its legacy `text_chunk` lines are rendered TUI
/// output; an `acp-*` transport's `text_chunk` is the model's or a subprocess's
/// content, which is evidence and must never be reclaimed.
///
/// orgasmic:TASK-FZB6T.2 finding 5 — the definition now lives in
/// `orgasmic_core::session`, because the session writer refuses a pane
/// `text_chunk` at write time and the two answers must be the same answer.
pub use orgasmic_core::session::transport_is_pane;

/// Whether an event class may be reclaimed by a maintenance pass, for a run on
/// `transport`.
///
/// Only a **proven rendered pane payload** is: a `rendered_tui` line written by
/// a pane transport. It is storage the current build already forbids
/// (dec_WDR5K item 7), it carries no lifecycle or native correlation, and it is
/// the entire 2.239 GiB story.
///
/// Everything else is refused, and the refusals are the point
/// (orgasmic:TASK-FZB6T.1 finding 2):
///
/// - lifecycle is authority;
/// - semantic events are budgeted evidence, capped at write time;
/// - `unparsed`, `blank`, and `torn` are refused on principle — a line this
///   accounting could not classify is the last line that should be deleted on
///   its say-so;
/// - a `text_chunk` from an `acp-*` transport, or from a run whose transport
///   was never recorded, is structured harness/subprocess evidence. The old
///   rule reclaimed all of it, which would have deleted every ACP assistant
///   turn and tool result on the board.
pub fn class_is_reclaimable(event_class: &str, transport: Option<&str>) -> bool {
    event_class == "rendered_tui" && transport.is_some_and(transport_is_pane)
}

/// Read one session file and account for it by event class.
///
/// Whole-file by necessity — accounting for bytes means visiting them — which
/// is why this runs only under an explicit operator command and never on an
/// inventory poll.
///
/// Every byte of the file lands in exactly one class, including the two shapes
/// the first cut lost (orgasmic:TASK-FZB6T.1 finding 2): a blank or
/// whitespace-only record is charged to `blank` rather than skipped, and a
/// final record with no terminating newline is charged to `torn` at its true
/// length rather than to its apparent class plus a newline it does not have.
pub fn inspect_session_file(path: &Path) -> std::io::Result<BTreeMap<String, HistoryClassTotals>> {
    let file = std::fs::File::open(path)?;
    let mut reader = std::io::BufReader::with_capacity(256 * 1024, file);
    let mut totals: BTreeMap<String, HistoryClassTotals> = BTreeMap::new();
    for record in read_history_records(&mut reader) {
        let record = record?;
        let bucket = totals.entry(record.class.to_string()).or_default();
        bucket.lines += 1;
        bucket.bytes += record.bytes;
        bucket.files = 1;
    }
    Ok(totals)
}

/// One physical record of a session file, with its on-disk byte cost.
#[derive(Debug, Clone)]
pub struct HistoryRecord {
    /// The record's bytes INCLUDING its terminating newline when it had one.
    pub raw: Vec<u8>,
    /// Total bytes this record occupies on disk.
    pub bytes: u64,
    pub class: &'static str,
    /// The record ended without a newline, so it is the file's last one and it
    /// may have been cut off mid-write.
    pub torn: bool,
}

/// Iterate a session file's physical records, accounting for every byte.
pub fn read_history_records<R: std::io::BufRead>(
    reader: &mut R,
) -> impl Iterator<Item = std::io::Result<HistoryRecord>> + '_ {
    let mut done = false;
    std::iter::from_fn(move || {
        if done {
            return None;
        }
        let mut raw = Vec::new();
        match reader.read_until(b'\n', &mut raw) {
            Ok(0) => {
                done = true;
                None
            }
            Ok(_) => {
                let bytes = raw.len() as u64;
                let torn = !raw.ends_with(b"\n");
                if torn {
                    done = true;
                }
                let content = raw.strip_suffix(b"\n").unwrap_or(&raw);
                let class = if content.iter().all(u8::is_ascii_whitespace) {
                    "blank"
                } else if torn {
                    // A record with no terminating newline is the last one in
                    // the file and cannot be proven complete. It is accounted
                    // truthfully and never reclaimed, whatever it looks like.
                    "torn"
                } else {
                    classify_history_line(content)
                };
                Some(Ok(HistoryRecord {
                    raw,
                    bytes,
                    class,
                    torn,
                }))
            }
            Err(error) => {
                done = true;
                Some(Err(error))
            }
        }
    })
}

/// Full `run history inspect` report for a set of catalog entries.
#[derive(Debug, Clone, Serialize)]
pub struct HistoryInspectReport {
    /// Whether anything was written. Always `false`: this command reads.
    pub dry_run: bool,
    pub session_files: u64,
    pub session_file_bytes: u64,
    pub bytes_accounted: u64,
    pub buckets: Vec<HistoryBucket>,
    /// Bytes a maintenance pass could reclaim, by driver.
    pub reclaimable_bytes: u64,
    pub reclaimable_by_driver: BTreeMap<String, u64>,
    /// Files that could not be read at all.
    pub unreadable_files: u64,
    /// The retention policy this accounting is measured against.
    pub retention: Vec<RetentionTier>,
    /// What this build will and will not do with the reclaimable bytes.
    pub note: &'static str,
}

#[derive(Debug, Clone, Serialize)]
pub struct RetentionTier {
    pub tier: &'static str,
    pub authority: &'static str,
    pub retention: &'static str,
}

pub fn retention_tiers() -> Vec<RetentionTier> {
    orgasmic_core::session::RETENTION_TIERS
        .iter()
        .map(|(tier, authority, retention)| RetentionTier {
            tier,
            authority,
            retention,
        })
        .collect()
}

/// Account for every session file behind `entries`, grouped by driver+harness
/// and event class.
pub fn inspect_history(entries: &[RunCatalogEntry]) -> HistoryInspectReport {
    // Keyed by (driver label, transport) so the reclaimability verdict a bucket
    // reports is the one that was actually applied to its bytes: two runs can
    // share a `driver` label only when they share a transport, but keeping the
    // transport in the key makes that a property of the code rather than of the
    // label format.
    let mut by_key: BTreeMap<(String, Option<String>, String), HistoryClassTotals> =
        BTreeMap::new();
    let mut reclaimable_by_driver: BTreeMap<String, u64> = BTreeMap::new();
    let mut session_file_bytes = 0_u64;
    let mut bytes_accounted = 0_u64;
    let mut unreadable_files = 0_u64;

    for entry in entries {
        let driver = entry.driver_label();
        let transport = entry.transport.clone();
        session_file_bytes += entry.file_bytes;
        let Ok(totals) = inspect_session_file(&entry.session_path) else {
            unreadable_files += 1;
            continue;
        };
        for (class, class_totals) in totals {
            bytes_accounted += class_totals.bytes;
            if class_is_reclaimable(&class, transport.as_deref()) {
                *reclaimable_by_driver.entry(driver.clone()).or_default() += class_totals.bytes;
            }
            let bucket = by_key
                .entry((driver.clone(), transport.clone(), class))
                .or_default();
            bucket.files += class_totals.files;
            bucket.lines += class_totals.lines;
            bucket.bytes += class_totals.bytes;
        }
    }

    let buckets: Vec<HistoryBucket> = by_key
        .into_iter()
        .map(|((driver, transport, event_class), totals)| HistoryBucket {
            reclaimable: class_is_reclaimable(&event_class, transport.as_deref()),
            driver,
            event_class,
            totals,
        })
        .collect();
    HistoryInspectReport {
        dry_run: true,
        session_files: entries.len() as u64,
        session_file_bytes,
        bytes_accounted,
        buckets,
        reclaimable_bytes: reclaimable_by_driver.values().sum(),
        reclaimable_by_driver,
        unreadable_files,
        retention: retention_tiers(),
        note: "read-only accounting; no file is written, moved, or deleted. \
               Reclaiming these bytes is `run history compact`, which is a dry \
               run until it is given the manifest id of the plan it printed, \
               archives every original whole, and can be rolled back.",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn write_session(
        dir: &Path,
        run_id: &str,
        transcript_bytes: usize,
        released: Option<ReleaseOutcome>,
        worktree: &Path,
        project_id: &str,
    ) -> PathBuf {
        let path = dir.join(format!("{run_id}.jsonl"));
        let mut seq = 0_u64;
        let mut out = String::new();
        let mut push = |kind: SessionEventKind, event: Value, out: &mut String| {
            let envelope = SessionEnvelope {
                seq,
                time: Utc::now(),
                run_id: run_id.to_string(),
                runtime_id: format!("runtime-{run_id}"),
                boot_id: "boot-catalog".to_string(),
                kind,
                event,
            };
            out.push_str(&serde_json::to_string(&envelope).unwrap());
            out.push('\n');
            seq += 1;
        };
        push(
            SessionEventKind::Lifecycle,
            json!({"phase": "acquire", "kind": "worker", "task_id": "TASK-CAT", "worker_id": "implementer-claude-rmux"}),
            &mut out,
        );
        push(
            SessionEventKind::Lifecycle,
            json!({
                "phase": "run_meta",
                "transport": "rmux",
                "harness": "claude",
                "project_id": project_id,
                "worktree": worktree,
                "driver_config": {},
            }),
            &mut out,
        );
        push(
            SessionEventKind::DriverEvent,
            json!({"type": "ready", "protocol_version": "tmux-tui/1", "capabilities": {"blob": "z".repeat(18 * 1024)}}),
            &mut out,
        );
        let chunk = "y".repeat(4096);
        let mut written = 0;
        while written < transcript_bytes {
            push(
                SessionEventKind::DriverEvent,
                json!({"type": "text_chunk", "stream": "stdout", "chunk": chunk}),
                &mut out,
            );
            written += chunk.len();
        }
        if let Some(outcome) = released {
            push(
                SessionEventKind::Lifecycle,
                json!({"phase": "release", "reason": "done", "outcome": outcome}),
                &mut out,
            );
        }
        std::fs::write(&path, out).unwrap();
        path
    }

    fn project(root: &Path, project_id: &str) {
        std::fs::create_dir_all(root.join(".orgasmic/tmp/sessions")).unwrap();
        std::fs::write(
            root.join(".orgasmic/project.org"),
            format!("#+title: p\n\n* PROJECT p\n:PROPERTIES:\n:ID: {project_id}\n:END:\n"),
        )
        .unwrap();
    }

    #[tokio::test]
    async fn a_refreshed_catalog_reads_zero_bytes_for_unchanged_sessions() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("proj");
        project(&root, "proj-1");
        let sessions = root.join(".orgasmic/tmp/sessions");
        for index in 0..8 {
            write_session(
                &sessions,
                &format!("run-{index}"),
                512 * 1024,
                Some(ReleaseOutcome::Completed),
                &root,
                "proj-1",
            );
        }

        let catalog = RunCatalog::new();
        let first =
            catalog.refresh_dir(&sessions, Some("proj-1"), &root, SessionScanBudget::DEFAULT);
        assert_eq!(first.session_files, 8);
        assert_eq!(first.rebuilt, 8);
        assert!(first.bytes_inspected > 0);

        let second =
            catalog.refresh_dir(&sessions, Some("proj-1"), &root, SessionScanBudget::DEFAULT);
        assert_eq!(second.cache_hits, 8);
        assert_eq!(second.rebuilt, 0);
        assert_eq!(
            second.bytes_inspected, 0,
            "a steady-state refresh must read no session bytes at all"
        );
    }

    #[tokio::test]
    async fn a_session_that_grows_is_reindexed_and_only_that_one() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("proj");
        project(&root, "proj-1");
        let sessions = root.join(".orgasmic/tmp/sessions");
        for index in 0..4 {
            write_session(
                &sessions,
                &format!("run-{index}"),
                8 * 1024,
                Some(ReleaseOutcome::Completed),
                &root,
                "proj-1",
            );
        }
        let live = write_session(&sessions, "run-live", 8 * 1024, None, &root, "proj-1");
        let catalog = RunCatalog::new();
        catalog.refresh_dir(&sessions, Some("proj-1"), &root, SessionScanBudget::DEFAULT);

        // The live run appends. Its mtime granularity is not the mechanism —
        // length changes, and length is part of the fingerprint.
        let mut file = std::fs::OpenOptions::new()
            .append(true)
            .open(&live)
            .unwrap();
        use std::io::Write;
        writeln!(
            file,
            "{}",
            serde_json::to_string(&SessionEnvelope {
                seq: 999,
                time: Utc::now(),
                run_id: "run-live".to_string(),
                runtime_id: "runtime-run-live".to_string(),
                boot_id: "boot-catalog".to_string(),
                kind: SessionEventKind::Lifecycle,
                event: json!({"phase": "release", "reason": "done", "outcome": "completed"}),
            })
            .unwrap()
        )
        .unwrap();
        drop(file);

        let stats =
            catalog.refresh_dir(&sessions, Some("proj-1"), &root, SessionScanBudget::DEFAULT);
        assert_eq!(stats.rebuilt, 1, "only the file that changed is re-read");
        assert_eq!(stats.cache_hits, 4);
        let entries = catalog.entries();
        let live_entry = entries
            .iter()
            .find(|entry| entry.run_id == "run-live")
            .unwrap();
        assert!(matches!(
            live_entry.terminal,
            Some(TerminalRecord::Release {
                outcome: ReleaseOutcome::Completed,
                ..
            })
        ));
    }

    #[tokio::test]
    async fn the_catalog_never_stores_the_ready_capabilities_blob() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("proj");
        project(&root, "proj-1");
        let sessions = root.join(".orgasmic/tmp/sessions");
        write_session(&sessions, "run-a", 4096, None, &root, "proj-1");
        let catalog = RunCatalog::new();
        catalog.refresh_dir(&sessions, Some("proj-1"), &root, SessionScanBudget::DEFAULT);
        let bytes = catalog.snapshot_bytes(&root).unwrap();
        let text = String::from_utf8(bytes).unwrap();
        assert!(
            !text.contains("zzzz"),
            "an 18 KiB ready capabilities frame reached the durable catalog"
        );
        assert!(
            text.contains("tmux-tui/1"),
            "the protocol version IS read by driver resolution and must survive"
        );
        assert!(
            !text.contains("yyyy"),
            "transcript payload reached the durable catalog"
        );
    }

    #[tokio::test]
    async fn a_pruned_worktree_becomes_a_stable_tombstone() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("proj");
        project(&root, "proj-1");
        let sessions = root.join(".orgasmic/tmp/sessions");
        let worktree = dir.path().join("wt");
        project(&worktree, "proj-1");
        write_session(&sessions, "run-wt", 4096, None, &worktree, "proj-1");

        let catalog = RunCatalog::new();
        catalog.refresh_dir(&sessions, Some("proj-1"), &root, SessionScanBudget::DEFAULT);
        let entry = catalog.entries().remove(0);
        assert!(entry.worktree_authority.verified_worktree().is_some());

        std::fs::remove_dir_all(&worktree).unwrap();
        let stats =
            catalog.refresh_dir(&sessions, Some("proj-1"), &root, SessionScanBudget::DEFAULT);
        assert_eq!(stats.rebuilt, 0, "the session file did not change");
        assert_eq!(stats.authority_reverified, 1);
        let entry = catalog.entries().remove(0);
        assert!(
            entry.worktree_authority.is_tombstoned(),
            "a pruned worktree must become a stable tombstone: {:?}",
            entry.worktree_authority
        );
    }

    /// orgasmic:TASK-FZB6T.1 finding 5 — a tombstone is terminal for the run
    /// identity. Removing the worktree and creating a NEW one at the same path
    /// must not make the dead run attachable again.
    #[tokio::test]
    async fn a_recreated_worktree_at_the_same_path_does_not_revive_a_tombstone() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("proj");
        project(&root, "proj-1");
        let sessions = root.join(".orgasmic/tmp/sessions");
        let worktree = dir.path().join("wt");
        project(&worktree, "proj-1");
        write_session(&sessions, "run-wt", 4096, None, &worktree, "proj-1");

        let catalog = RunCatalog::new();
        catalog.refresh_dir(&sessions, Some("proj-1"), &root, SessionScanBudget::DEFAULT);
        let verified = catalog.entries().remove(0).worktree_authority;
        let WorktreeAuthority::Verified { identity, .. } = &verified else {
            panic!("expected a verified worktree, got {verified:?}");
        };
        assert!(
            identity.is_some(),
            "a verified worktree must pin a durable directory identity"
        );

        // Pruned.
        std::fs::remove_dir_all(&worktree).unwrap();
        catalog.refresh_dir(&sessions, Some("proj-1"), &root, SessionScanBudget::DEFAULT);
        assert!(catalog.entries()[0].worktree_authority.is_tombstoned());

        // A DIFFERENT checkout is later created at the same path — a perfectly
        // ordinary thing for a dispatch worktree path to have happen to it.
        project(&worktree, "proj-1");
        let stats =
            catalog.refresh_dir(&sessions, Some("proj-1"), &root, SessionScanBudget::DEFAULT);
        assert_eq!(stats.rebuilt, 0, "the session file did not change");
        let entry = catalog.entries().remove(0);
        assert!(
            entry.worktree_authority.is_tombstoned(),
            "a path reused by an unrelated directory must not revive a dead run: {:?}",
            entry.worktree_authority
        );
        assert!(
            entry.worktree_authority.verified_worktree().is_none(),
            "a tombstoned run offers no verified worktree to attach into"
        );
    }

    /// orgasmic:TASK-FZB6T.3 finding 4 / dec_BBPW4 item 2 — the tombstone
    /// survives the CATALOG, because the catalog is allowed to be thrown away.
    ///
    /// "The catalog is disposable derived state" and "a tombstone never revives"
    /// could not both be true while the tombstone lived only in the catalog:
    /// prune -> tombstone -> catalog loss -> path reuse re-derived `Verified`
    /// from a same-project checkout at the recorded path, and a dead run became
    /// an attach candidate again. The verdict is now written to durable
    /// authority OUTSIDE the cache, so every way of losing the cache — deleted,
    /// corrupt, or from a version this build refuses — takes the same answer:
    /// rebuild the derived facts, keep the terminal one.
    #[tokio::test]
    async fn a_tombstone_outlives_the_catalog_it_was_minted_in() {
        for loss in ["deleted", "corrupt", "foreign version"] {
            let dir = tempfile::tempdir().unwrap();
            let root = dir.path().join("proj");
            project(&root, "proj-1");
            let sessions = root.join(".orgasmic/tmp/sessions");
            let worktree = dir.path().join("wt");
            project(&worktree, "proj-1");
            write_session(&sessions, "run-wt", 4096, None, &worktree, "proj-1");

            let catalog = RunCatalog::new();
            catalog.refresh_dir(&sessions, Some("proj-1"), &root, SessionScanBudget::DEFAULT);
            assert!(
                !catalog.entries()[0].worktree_authority.is_tombstoned(),
                "{loss}"
            );

            // Pruned, and the catalog persists what it knows.
            std::fs::remove_dir_all(&worktree).unwrap();
            catalog.refresh_dir(&sessions, Some("proj-1"), &root, SessionScanBudget::DEFAULT);
            assert!(
                catalog.entries()[0].worktree_authority.is_tombstoned(),
                "{loss}"
            );
            std::fs::write(
                root.join(CATALOG_REL_PATH),
                catalog.snapshot_bytes(&root).unwrap(),
            )
            .unwrap();

            // The cache is lost, in each of the three ways the corruption
            // artifact says are the same problem with the same answer.
            let snapshot_path = root.join(CATALOG_REL_PATH);
            match loss {
                "deleted" => std::fs::remove_file(&snapshot_path).unwrap(),
                "corrupt" => std::fs::write(&snapshot_path, b"{\"entries\": [{\"run").unwrap(),
                _ => {
                    let mut value: Value =
                        serde_json::from_slice(&std::fs::read(&snapshot_path).unwrap()).unwrap();
                    value["catalog_version"] = json!(CATALOG_VERSION + 9);
                    std::fs::write(&snapshot_path, serde_json::to_vec(&value).unwrap()).unwrap();
                }
            }

            // The path is reused by an unrelated checkout of the same project —
            // the ordinary fate of a dispatch worktree path — and a brand new
            // catalog rebuilds from the session bytes, which cannot tell the
            // difference.
            project(&worktree, "proj-1");
            let rebuilt = RunCatalog::new();
            let load = rebuilt.load_snapshot(&root);
            assert!(
                !matches!(load, SnapshotLoad::Loaded { entries } if entries > 0),
                "{loss}: the cache must not have survived this test's own setup: {load:?}"
            );
            let stats =
                rebuilt.refresh_dir(&sessions, Some("proj-1"), &root, SessionScanBudget::DEFAULT);
            assert_eq!(
                stats.rebuilt, 1,
                "{loss}: the board is re-derived from disk"
            );
            assert_eq!(
                stats.tombstones_reasserted, 1,
                "{loss}: the durable tombstone must overrule the re-derived verdict"
            );
            let entry = rebuilt.entries().remove(0);
            assert!(
                entry.worktree_authority.is_tombstoned(),
                "{loss}: a tombstone that only lived in a disposable cache is not terminal: \
                 {:?}",
                entry.worktree_authority
            );
            assert!(
                entry.worktree_authority.verified_worktree().is_none(),
                "{loss}: a dead run must not be offered as an attach candidate"
            );
        }
    }

    /// Mint a durable tombstone for `run-wt` the way an ordinary prune does, and
    /// return the board it was minted on. The caller then damages the ledger.
    fn board_with_a_durable_tombstone(dir: &std::path::Path) -> (PathBuf, PathBuf, PathBuf) {
        let root = dir.join("proj");
        project(&root, "proj-1");
        let sessions = root.join(".orgasmic/tmp/sessions");
        let worktree = dir.join("wt");
        project(&worktree, "proj-1");
        write_session(&sessions, "run-wt", 4096, None, &worktree, "proj-1");

        let catalog = RunCatalog::new();
        catalog.refresh_dir(&sessions, Some("proj-1"), &root, SessionScanBudget::DEFAULT);
        std::fs::remove_dir_all(&worktree).unwrap();
        catalog.refresh_dir(&sessions, Some("proj-1"), &root, SessionScanBudget::DEFAULT);
        assert!(catalog.entries()[0].worktree_authority.is_tombstoned());
        assert!(
            matches!(
                TombstoneLedger::load(&root),
                TombstoneLedgerState::Loaded(_)
            ),
            "the prune must have produced a readable durable ledger"
        );
        std::fs::write(
            root.join(CATALOG_REL_PATH),
            catalog.snapshot_bytes(&root).unwrap(),
        )
        .unwrap();
        (root, sessions, worktree)
    }

    /// orgasmic:TASK-FZB6T.4 finding 2 — a ledger this build cannot read is NOT
    /// an empty ledger, and the difference is a dead run coming back to life.
    ///
    /// `load` used to map absent, unreadable, corrupt AND foreign-version onto
    /// the same empty result. Only a positive ledger hit overrules a rebuilt
    /// entry's re-derived `Verified`, so a damaged ledger silently answered "no
    /// run is tombstoned": the catalog rebuilt, the pruned worktree path was
    /// reused by an ordinary same-project checkout, the entry re-derived
    /// `Verified`, and `/api/runs` resumed attach probing against a run whose
    /// worktree is gone forever.
    ///
    /// Both damaged shapes now fail CLOSED — the run is `Unprovable`, offers no
    /// worktree, and blocks the attach probe — and neither is overwritten, so
    /// the authority an operator might still repair survives the daemon that
    /// could not read it.
    #[tokio::test]
    async fn a_damaged_tombstone_ledger_never_revives_a_dead_run() {
        for damage in ["corrupt", "foreign version", "unreadable bytes"] {
            let dir = tempfile::tempdir().unwrap();
            let (root, sessions, worktree) = board_with_a_durable_tombstone(dir.path());
            let ledger_path = root.join(TOMBSTONE_REL_PATH);
            let intact = std::fs::read(&ledger_path).unwrap();

            match damage {
                "corrupt" => std::fs::write(&ledger_path, b"{\"tombstoned\": {\"run-").unwrap(),
                "foreign version" => {
                    let mut value: Value = serde_json::from_slice(&intact).unwrap();
                    value["version"] = json!(TOMBSTONE_LEDGER_VERSION + 9);
                    std::fs::write(&ledger_path, serde_json::to_vec(&value).unwrap()).unwrap();
                }
                // Valid JSON, wrong shape: `tombstoned` is not a map at all.
                _ => std::fs::write(&ledger_path, br#"{"version":1,"tombstoned":[1,2]}"#).unwrap(),
            }
            assert!(
                TombstoneLedger::load(&root).unusable_reason().is_some(),
                "{damage}: the fixture must actually be unusable"
            );

            // The catalog is thrown away — it is allowed to be — and the pruned
            // path is reused by an unrelated checkout of the same project, which
            // is the ordinary fate of a dispatch worktree.
            let _ = std::fs::remove_file(root.join(CATALOG_REL_PATH));
            project(&worktree, "proj-1");
            let rebuilt = RunCatalog::new();
            let stats =
                rebuilt.refresh_dir(&sessions, Some("proj-1"), &root, SessionScanBudget::DEFAULT);
            assert_eq!(stats.rebuilt, 1, "{damage}");
            assert_eq!(
                stats.tombstones_unprovable, 1,
                "{damage}: a ledger that cannot be read must be reported, not assumed empty"
            );
            let entry = rebuilt.entries().remove(0);
            assert!(
                entry.worktree_authority.verified_worktree().is_none(),
                "{damage}: a damaged ledger must never re-offer a worktree: {:?}",
                entry.worktree_authority
            );
            assert!(
                entry.worktree_authority.blocks_attach(),
                "{damage}: the run must not become an attach candidate: {:?}",
                entry.worktree_authority
            );
            assert_eq!(entry.worktree_authority.label(), "unprovable", "{damage}");
            assert!(entry
                .worktree_authority
                .authority_error()
                .unwrap()
                .contains("tombstone ledger"));

            // And the damaged authority was not silently replaced: an operator
            // can still repair it, which is the only reason refusing beats
            // rebuilding here.
            assert!(
                TombstoneLedger::load(&root).unusable_reason().is_some(),
                "{damage}: the unreadable ledger must not have been overwritten"
            );
            assert!(
                matches!(
                    TombstoneLedger::default().save(&root),
                    Err(TombstoneSaveError::WouldOverwriteUnreadable(_))
                ),
                "{damage}: a write over unreadable authority must be refused, not merged"
            );

            // Repairing it restores the terminal verdict, which proves the
            // refusal above was about READABILITY and not about losing the fact.
            std::fs::write(&ledger_path, &intact).unwrap();
            let repaired = RunCatalog::new();
            let stats =
                repaired.refresh_dir(&sessions, Some("proj-1"), &root, SessionScanBudget::DEFAULT);
            assert_eq!(stats.tombstones_unprovable, 0, "{damage}");
            assert_eq!(stats.tombstones_reasserted, 1, "{damage}");
            assert!(repaired.entries()[0].worktree_authority.is_tombstoned());
        }
    }

    /// orgasmic:TASK-FZB6T.4 finding 2 — the read-merge-write is serialized, so
    /// two concurrent writers cannot lose each other's tombstone.
    ///
    /// `save` read the current ledger, merged its own entries in, and renamed —
    /// with no lock and one fixed `.json.tmp`. Two daemons, or two refresh
    /// passes, could both read generation N and each publish a different N+1;
    /// the loser's terminal facts were gone, and both had already been reported
    /// as persisted. A tombstone is authority no rebuild can recover, so a lost
    /// update here is a dead run resurrected later.
    #[test]
    fn concurrent_ledger_writers_never_lose_a_tombstone() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("proj");
        project(&root, "proj-1");

        // Each writer records a DISJOINT set, so the correct merge is the union
        // and any lost update is a missing key rather than a benign tie.
        const WRITERS: usize = 8;
        const PER_WRITER: usize = 12;
        let start = std::sync::Arc::new(std::sync::Barrier::new(WRITERS));
        std::thread::scope(|scope| {
            for writer in 0..WRITERS {
                let root = root.clone();
                let start = std::sync::Arc::clone(&start);
                scope.spawn(move || {
                    let mut ledger = TombstoneLedger::default();
                    for index in 0..PER_WRITER {
                        ledger.record(
                            &format!("run-{writer}-{index}"),
                            std::path::Path::new("/gone"),
                        );
                    }
                    start.wait();
                    ledger.save(&root).expect("save must not fail");
                });
            }
        });

        let TombstoneLedgerState::Loaded(final_ledger) = TombstoneLedger::load(&root) else {
            panic!("the concurrently written ledger must still be readable");
        };
        for writer in 0..WRITERS {
            for index in 0..PER_WRITER {
                let run_id = format!("run-{writer}-{index}");
                assert!(
                    final_ledger.contains(&run_id),
                    "a concurrent writer's tombstone was lost: {run_id}"
                );
            }
        }
        assert_eq!(final_ledger.tombstoned.len(), WRITERS * PER_WRITER);

        // No staging file survived the race, so no writer published another's
        // half-written bytes through a shared temp path.
        let leftovers: Vec<String> = std::fs::read_dir(root.join(".orgasmic/tmp"))
            .unwrap()
            .filter_map(|entry| Some(entry.ok()?.file_name().to_string_lossy().to_string()))
            .filter(|name| name.starts_with("run-tombstones.") && name.ends_with(".tmp"))
            .collect();
        assert!(leftovers.is_empty(), "{leftovers:?}");
    }

    /// orgasmic:TASK-FZB6T.4 finding 2 — a `save` that returned `Ok` means the
    /// bytes and the rename are on the disk, not in a page cache.
    ///
    /// The old write was `fs::write` + `rename` with no fsync of either the file
    /// or its directory, so a crash could lose a tombstone this function had
    /// already reported as persisted. A unit test cannot cut a machine's power;
    /// what it CAN prove is the protocol — that the durable path is the only one
    /// a reader ever sees, that a stale staging file left by a killed writer is
    /// never read as authority, and that the ledger survives the loss of every
    /// derived thing around it.
    #[tokio::test]
    async fn a_persisted_tombstone_survives_the_loss_of_everything_derived() {
        let dir = tempfile::tempdir().unwrap();
        let (root, sessions, worktree) = board_with_a_durable_tombstone(dir.path());
        let ledger_path = root.join(TOMBSTONE_REL_PATH);

        // A writer that died between its staging write and its rename. The
        // staging name is unique per attempt, so this cannot be mistaken for
        // the live file and cannot be renamed over it by anyone else.
        let orphan = root.join(".orgasmic/tmp/run-tombstones.json.99999.0.tmp");
        std::fs::write(&orphan, br#"{"version":1,"tombstoned":{}}"#).unwrap();

        // Everything derived is destroyed: the catalog snapshot, the session
        // directory's cached state, and the daemon itself.
        std::fs::remove_file(root.join(CATALOG_REL_PATH)).unwrap();
        project(&worktree, "proj-1");

        let rebuilt = RunCatalog::new();
        let stats =
            rebuilt.refresh_dir(&sessions, Some("proj-1"), &root, SessionScanBudget::DEFAULT);
        assert_eq!(stats.tombstones_reasserted, 1);
        assert_eq!(stats.tombstones_unprovable, 0);
        let entry = rebuilt.entries().remove(0);
        assert!(
            entry.worktree_authority.is_tombstoned(),
            "{:?}",
            entry.worktree_authority
        );
        assert!(entry.worktree_authority.blocks_attach());
        assert!(entry.worktree_authority.verified_worktree().is_none());

        // The orphaned staging file was neither read nor promoted.
        assert!(orphan.exists());
        let TombstoneLedgerState::Loaded(ledger) = TombstoneLedger::load(&root) else {
            panic!("the durable ledger must still be the readable one");
        };
        assert!(ledger.contains("run-wt"));

        // orgasmic:TASK-FZB6T.4 finding 5 — and it is machine-local state, so it
        // lives where the repository ignores it rather than at the project root
        // where the first tombstone dirtied `git status`.
        assert!(
            ledger_path.starts_with(root.join(".orgasmic/tmp")),
            "{}",
            ledger_path.display()
        );
        assert!(
            !root.join(".orgasmic/run-tombstones.json").exists(),
            "the old repo-visible location must not be written any more"
        );
    }

    /// orgasmic:TASK-FZB6T.2 finding 7 / dec_BBPW4 — a tombstone is TERMINAL,
    /// and the "same directory object came back" revival is gone.
    ///
    /// The old rule readmitted a run when the recorded path carried the
    /// recorded device and inode. Inode numbers are reusable, so an unrelated
    /// checkout could eventually satisfy it, while a returned volume with
    /// renumbered inodes never could — unsound in one direction and useless in
    /// the other. This drives the case the old rule was WRITTEN for: the very
    /// same directory object, moved away and moved back. It must stay dead.
    #[tokio::test]
    async fn a_tombstone_is_terminal_even_when_the_same_directory_returns() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("proj");
        project(&root, "proj-1");
        let sessions = root.join(".orgasmic/tmp/sessions");
        let real = dir.path().join("real-worktree");
        project(&real, "proj-1");
        let link = dir.path().join("wt");
        #[cfg(unix)]
        std::os::unix::fs::symlink(&real, &link).unwrap();
        #[cfg(not(unix))]
        return;
        write_session(&sessions, "run-wt", 4096, None, &link, "proj-1");

        let catalog = RunCatalog::new();
        catalog.refresh_dir(&sessions, Some("proj-1"), &root, SessionScanBudget::DEFAULT);
        assert!(catalog.entries()[0]
            .worktree_authority
            .verified_worktree()
            .is_some());

        // The volume goes away.
        std::fs::rename(&real, dir.path().join("stashed")).unwrap();
        catalog.refresh_dir(&sessions, Some("proj-1"), &root, SessionScanBudget::DEFAULT);
        assert!(catalog.entries()[0].worktree_authority.is_tombstoned());

        // The very same directory object comes back — same device, same inode.
        std::fs::rename(dir.path().join("stashed"), &real).unwrap();
        let identity_now = DirIdentity::at(&real);
        let WorktreeAuthority::Tombstoned {
            verified_identity, ..
        } = &catalog.entries()[0].worktree_authority
        else {
            panic!("expected a tombstone");
        };
        assert_eq!(
            *verified_identity, identity_now,
            "the fixture must actually restore the SAME directory object, or it proves nothing"
        );

        catalog.refresh_dir(&sessions, Some("proj-1"), &root, SessionScanBudget::DEFAULT);
        assert!(
            catalog.entries()[0].worktree_authority.is_tombstoned(),
            "a tombstone is terminal: not even the original directory object returning \
             revives a dead run: {:?}",
            catalog.entries()[0].worktree_authority
        );
        assert!(
            catalog.entries()[0]
                .worktree_authority
                .verified_worktree()
                .is_none(),
            "a tombstoned run offers no verified worktree to attach into"
        );
    }

    /// orgasmic:TASK-FZB6T.1 finding 7 — a snapshot entry is admitted through
    /// session-directory authority and current file identity, not by living
    /// somewhere under the project root.
    ///
    /// This test proves PATH and FINGERPRINT corruption only: which BYTES an
    /// entry is about. What it CLAIMS those bytes mean is
    /// `a_snapshot_entrys_semantic_claims_must_be_reproduced_by_its_own_envelopes`,
    /// which is a separate test on purpose (orgasmic:TASK-FZB6T.3 finding 5) —
    /// the two were one test, the path cases ran first, and under the
    /// TASK-FZB6T.1 injection the first of them panicked so the semantic
    /// assertions were never reached at all.
    #[tokio::test]
    async fn semantically_corrupt_snapshot_entries_are_refused() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("proj");
        project(&root, "proj-1");
        let sessions = root.join(".orgasmic/tmp/sessions");
        let real = write_session(
            &sessions,
            "run-real",
            4096,
            Some(ReleaseOutcome::Completed),
            &root,
            "proj-1",
        );

        let source = RunCatalog::new();
        source.refresh_dir(&sessions, Some("proj-1"), &root, SessionScanBudget::DEFAULT);
        let template = source.entries().remove(0);
        let snapshot_path = root.join(CATALOG_REL_PATH);

        // A decoy file that exists but is not a session record.
        std::fs::write(root.join("decoy.jsonl"), "{}\n").unwrap();
        let nested = sessions.join("nested");
        std::fs::create_dir_all(&nested).unwrap();
        std::fs::write(nested.join("deep.jsonl"), "{}\n").unwrap();
        #[cfg(unix)]
        std::os::unix::fs::symlink(&real, sessions.join("link.jsonl")).unwrap();

        let cases: Vec<(&str, PathBuf)> = vec![
            // Wrong parent: under the project root, not a session record.
            ("wrong parent", root.join("decoy.jsonl")),
            // Not a DIRECT child.
            ("nested below the sessions dir", nested.join("deep.jsonl")),
            // Path traversal that lands back inside the project root.
            (
                "traversal out and back",
                sessions.join("../../../.orgasmic/project.org"),
            ),
            // Traversal whose textual parent looks right.
            (
                "traversal through the sessions dir",
                sessions.join("../sessions/../sessions/run-real.jsonl"),
            ),
            // A record for a file that is not there any more.
            ("missing file", sessions.join("run-gone.jsonl")),
            // Not a `.jsonl` at all.
            ("non-session extension", sessions.join("notes.txt")),
            #[cfg(unix)]
            ("symlink", sessions.join("link.jsonl")),
        ];
        for (name, session_path) in cases {
            let mut entry = template.clone();
            entry.session_path = session_path.clone();
            let snapshot = json!({
                "catalog_version": CATALOG_VERSION,
                "written_at": Utc::now(),
                "entries": [entry],
            });
            std::fs::write(&snapshot_path, serde_json::to_vec(&snapshot).unwrap()).unwrap();
            let catalog = RunCatalog::new();
            assert_eq!(
                catalog.load_snapshot(&root),
                SnapshotLoad::Loaded { entries: 0 },
                "{name}: {} must not be admitted as a session record",
                session_path.display()
            );
            assert_eq!(catalog.len(), 0, "{name}");
        }

        // And a record whose file identity moved on is refused too: the entry
        // describes bytes that are no longer there.
        let mut stale = template.clone();
        stale.fingerprint.len += 1;
        let snapshot = json!({
            "catalog_version": CATALOG_VERSION,
            "written_at": Utc::now(),
            "entries": [stale],
        });
        std::fs::write(&snapshot_path, serde_json::to_vec(&snapshot).unwrap()).unwrap();
        let catalog = RunCatalog::new();
        assert_eq!(
            catalog.load_snapshot(&root),
            SnapshotLoad::Loaded { entries: 0 },
            "an entry whose file identity changed must be re-derived, not trusted"
        );

        // The sound entry still loads, so the rule is not merely refusing
        // everything.
        std::fs::write(&snapshot_path, source.snapshot_bytes(&root).unwrap()).unwrap();
        let catalog = RunCatalog::new();
        assert_eq!(
            catalog.load_snapshot(&root),
            SnapshotLoad::Loaded { entries: 1 }
        );
        let _ = real;
    }

    /// orgasmic:TASK-FZB6T.3 finding 5 — the SEMANTIC half, on its own test and
    /// its own injection signature.
    ///
    /// It used to be the tail of `semantically_corrupt_snapshot_entries_are_refused`,
    /// which drives path and fingerprint corruption FIRST. Under the
    /// TASK-FZB6T.1 injection the first path case panics, so these assertions
    /// were never reached and the pinned red named only the path failure. The
    /// replay still passed — which is the lesson: a passing red-then-green
    /// proves the red HAPPENS, not that it is the RIGHT red.
    ///
    /// Path and fingerprint say which BYTES an entry is about. They say nothing
    /// about what it claims those bytes MEAN. Each case below is valid JSON at
    /// the right path with the right file identity, and each lies about the
    /// session file's meaning — starting with the pair that authorized a
    /// deletion.
    #[tokio::test]
    async fn a_snapshot_entrys_semantic_claims_must_be_reproduced_by_its_own_envelopes() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("proj");
        project(&root, "proj-1");
        let sessions = root.join(".orgasmic/tmp/sessions");
        let real = write_session(
            &sessions,
            "run-real",
            4096,
            Some(ReleaseOutcome::Completed),
            &root,
            "proj-1",
        );

        let source = RunCatalog::new();
        source.refresh_dir(&sessions, Some("proj-1"), &root, SessionScanBudget::DEFAULT);
        let template = source.entries().remove(0);
        let snapshot_path = root.join(CATALOG_REL_PATH);

        // --- THE SEMANTIC HALF (orgasmic:TASK-FZB6T.2 finding 4) -----------
        //
        // Each of these is valid JSON at the right path with the right file
        // identity, and each lies about what the session file MEANS. The one
        // that mattered is the first: it presents this run as a terminal rmux
        // run, which is precisely the pair `plan_compaction` used to read as
        // permission to delete.
        /// One named way to corrupt a semantic field of an otherwise valid
        /// snapshot record.
        type SemanticCorruption = (&'static str, Box<dyn Fn(&mut RunCatalogEntry)>);
        let semantic_cases: Vec<SemanticCorruption> = vec![
            (
                "terminal + transport: the deletion-authority pair",
                Box::new(|entry: &mut RunCatalogEntry| {
                    entry.transport = Some("rmux".to_string());
                    entry.terminal = Some(TerminalRecord::DriverEvent {
                        event: "run_complete".to_string(),
                        at: Utc::now(),
                    });
                }),
            ),
            (
                "terminal verdict alone",
                Box::new(|entry: &mut RunCatalogEntry| {
                    entry.terminal = None;
                }),
            ),
            (
                "transport alone",
                Box::new(|entry: &mut RunCatalogEntry| {
                    entry.transport = Some("acp-stdio".to_string());
                }),
            ),
            (
                "driver harness",
                Box::new(|entry: &mut RunCatalogEntry| {
                    entry.harness = Some("not-what-the-session-says".to_string());
                }),
            ),
            (
                "lifecycle: task and worker",
                Box::new(|entry: &mut RunCatalogEntry| {
                    entry.task_id = Some("TASK-NEVER-RAN".to_string());
                    entry.worker_id = Some("someone-else".to_string());
                }),
            ),
            (
                "worktree authority pair",
                Box::new(|entry: &mut RunCatalogEntry| {
                    entry.run_meta_project = Some("another-project".to_string());
                    entry.run_meta_worktree = Some(PathBuf::from("/somewhere/else"));
                }),
            ),
            (
                "external registration flag",
                Box::new(|entry: &mut RunCatalogEntry| {
                    entry.external_registration = !entry.external_registration;
                }),
            ),
            (
                "the source envelopes emptied, the verdicts kept",
                Box::new(|entry: &mut RunCatalogEntry| {
                    entry.lifecycle_envelopes.clear();
                }),
            ),
        ];
        for (name, corrupt) in semantic_cases {
            let mut entry = template.clone();
            corrupt(&mut entry);
            let snapshot = json!({
                "catalog_version": CATALOG_VERSION,
                "written_at": Utc::now(),
                "entries": [entry],
            });
            std::fs::write(&snapshot_path, serde_json::to_vec(&snapshot).unwrap()).unwrap();
            let catalog = RunCatalog::new();
            assert_eq!(
                catalog.load_snapshot(&root),
                SnapshotLoad::Loaded { entries: 0 },
                "{name}: a semantic claim its own source envelopes do not reproduce \
                 must not be admitted as a session record"
            );
            assert_eq!(catalog.len(), 0, "{name}");
        }

        // An `unreadable` record claims nothing, so it is admissible — but only
        // while it goes on claiming nothing.
        let mut unreadable = template.clone();
        unreadable.unreadable = true;
        unreadable.lifecycle_envelopes.clear();
        unreadable.transport = None;
        unreadable.harness = None;
        unreadable.native = None;
        unreadable.terminal = None;
        unreadable.final_release_outcome = None;
        unreadable.driver_terminal_event = None;
        unreadable.run_meta_recorded = false;
        unreadable.external_registration = false;
        let mut lying = unreadable.clone();
        lying.terminal = Some(TerminalRecord::ExternalRegistrationEnded);
        for (name, entry, expected) in [
            ("claims nothing", unreadable, 1_usize),
            ("claims a terminal verdict it cannot have", lying, 0),
        ] {
            let snapshot = json!({
                "catalog_version": CATALOG_VERSION,
                "written_at": Utc::now(),
                "entries": [entry],
            });
            std::fs::write(&snapshot_path, serde_json::to_vec(&snapshot).unwrap()).unwrap();
            let catalog = RunCatalog::new();
            assert_eq!(
                catalog.load_snapshot(&root),
                SnapshotLoad::Loaded { entries: expected },
                "unreadable record that {name}"
            );
        }

        // The sound entry still loads, so the rule is not merely refusing
        // everything.
        std::fs::write(&snapshot_path, source.snapshot_bytes(&root).unwrap()).unwrap();
        let catalog = RunCatalog::new();
        assert_eq!(
            catalog.load_snapshot(&root),
            SnapshotLoad::Loaded { entries: 1 }
        );
        let _ = real;
    }

    /// orgasmic:TASK-FZB6T.1 finding 6 — dirty state and the save throttle are
    /// per project. One project's save must not consume another's pending
    /// snapshot, nor throttle a project that has never been saved.
    #[tokio::test]
    async fn snapshot_dirty_state_and_throttling_are_per_project() {
        let dir = tempfile::tempdir().unwrap();
        let first = dir.path().join("first");
        let second = dir.path().join("second");
        project(&first, "proj-1");
        project(&second, "proj-2");
        let first_sessions = first.join(".orgasmic/tmp/sessions");
        let second_sessions = second.join(".orgasmic/tmp/sessions");
        write_session(
            &first_sessions,
            "run-1",
            4096,
            Some(ReleaseOutcome::Completed),
            &first,
            "proj-1",
        );
        write_session(
            &second_sessions,
            "run-2",
            4096,
            Some(ReleaseOutcome::Completed),
            &second,
            "proj-2",
        );

        let catalog = RunCatalog::new();
        catalog.refresh_dir(
            &first_sessions,
            Some("proj-1"),
            &first,
            SessionScanBudget::DEFAULT,
        );
        catalog.refresh_dir(
            &second_sessions,
            Some("proj-2"),
            &second,
            SessionScanBudget::DEFAULT,
        );

        // Saving the first project must not clear the second's pending work,
        // and the second's save must not be throttled by the first's.
        let first_bytes = catalog.snapshot_bytes(&first).expect("first has work");
        let second_bytes = catalog
            .snapshot_bytes(&second)
            .expect("a second project's snapshot must not be consumed by the first's save");
        assert!(String::from_utf8(first_bytes)
            .unwrap()
            .contains("run-1.jsonl"));
        let second_text = String::from_utf8(second_bytes).unwrap();
        assert!(second_text.contains("run-2.jsonl"));
        assert!(
            !second_text.contains("run-1.jsonl"),
            "a project's snapshot must carry only its own records"
        );

        // Both are clean now, and a steady-state refresh writes nothing.
        catalog.refresh_dir(
            &first_sessions,
            Some("proj-1"),
            &first,
            SessionScanBudget::DEFAULT,
        );
        assert!(catalog.snapshot_bytes(&first).is_none());
        assert!(catalog.snapshot_bytes(&second).is_none());

        // A new run in the SECOND project marks only the second dirty.
        write_session(
            &second_sessions,
            "run-3",
            4096,
            Some(ReleaseOutcome::Completed),
            &second,
            "proj-2",
        );
        catalog.refresh_dir(
            &second_sessions,
            Some("proj-2"),
            &second,
            SessionScanBudget::DEFAULT,
        );
        assert!(catalog
            .snapshot_bytes_after(&second, std::time::Duration::ZERO)
            .is_some());
        assert!(
            catalog
                .snapshot_bytes_after(&first, std::time::Duration::ZERO)
                .is_none(),
            "an unchanged project must not be rewritten because another changed"
        );
    }

    /// orgasmic:TASK-FZB6T.1 finding 8 — the catalog mutex is not held across
    /// filesystem work.
    ///
    /// Deterministic, not timed: the refresh calls a hook while it is scanning
    /// session files, and the hook proves the lock is free by taking it —
    /// `try_lock` from the refresh's own thread would succeed on a re-entrant
    /// lock and prove nothing, so the probe runs on another thread and must
    /// complete while the scan is still in flight.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn the_catalog_mutex_is_free_while_session_files_are_scanned() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("proj");
        project(&root, "proj-1");
        let sessions = root.join(".orgasmic/tmp/sessions");
        for index in 0..24 {
            write_session(
                &sessions,
                &format!("run-{index}"),
                256 * 1024,
                Some(ReleaseOutcome::Completed),
                &root,
                "proj-1",
            );
        }

        let catalog = RunCatalog::new();
        let observations = std::sync::Arc::new(std::sync::atomic::AtomicU64::new(0));
        let seen = observations.clone();
        let probed = catalog.clone();
        catalog.refresh_dir_observed(
            &sessions,
            Some("proj-1"),
            &root,
            SessionScanBudget::DEFAULT,
            &mut |_| {
                // Another thread does what an inventory poll and the session
                // writer do: read the map, and invalidate a path. Both must
                // complete while this refresh is scanning.
                let probed = probed.clone();
                let seen = seen.clone();
                std::thread::spawn(move || {
                    let _ = probed.entries();
                    probed.invalidate_session(std::path::Path::new("/nonexistent.jsonl"));
                    seen.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                })
                .join()
                .expect("a catalog read must not be blocked by an in-flight scan");
            },
        );
        assert!(
            observations.load(std::sync::atomic::Ordering::SeqCst) >= 24,
            "the hook must have run once per rebuilt file"
        );
    }

    /// A lifecycle append that lands while a refresh is scanning wins: the
    /// refresh's compare-and-swap drops its own now-stale entry rather than
    /// overwriting the invalidation.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn a_write_during_a_scan_wins_the_commit() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("proj");
        project(&root, "proj-1");
        let sessions = root.join(".orgasmic/tmp/sessions");
        let target = write_session(
            &sessions,
            "run-a",
            4096,
            Some(ReleaseOutcome::Completed),
            &root,
            "proj-1",
        );

        let catalog = RunCatalog::new();
        let racer = catalog.clone();
        let path = target.clone();
        let fired = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let flag = fired.clone();
        catalog.refresh_dir_observed(
            &sessions,
            Some("proj-1"),
            &root,
            SessionScanBudget::DEFAULT,
            &mut |_| {
                if flag.swap(true, std::sync::atomic::Ordering::SeqCst) {
                    return;
                }
                // The writer's lifecycle-append invalidation, delivered while
                // the scan of this very file is in flight.
                racer.invalidate_session(&path);
            },
        );
        assert_eq!(
            catalog.len(),
            0,
            "an entry built from bytes older than a concurrent lifecycle write must \
             not be committed"
        );
        // And the next refresh re-derives it from the newer bytes.
        let stats =
            catalog.refresh_dir(&sessions, Some("proj-1"), &root, SessionScanBudget::DEFAULT);
        assert_eq!(stats.rebuilt, 1);
        assert_eq!(catalog.len(), 1);
    }

    /// orgasmic:TASK-FZB6T.2 finding 6 — the lost-update/ABA window on stale
    /// eviction.
    ///
    /// A rebuild captured a per-path generation and committed under a
    /// compare-and-swap; a STALE path captured nothing and committed an
    /// unconditional `remove`, then deleted the path's generation along with the
    /// record. Both halves are bugs, and this drives both:
    ///
    /// 1. A refresh that observed a path absent commits AFTER the file returned
    ///    and a newer refresh indexed it. The unconditional remove evicted that
    ///    live record on the strength of an observation that was already stale —
    ///    a lost update. The commit must refuse.
    /// 2. Deleting the generation with the record reset the counter to zero, so
    ///    a later refresh could capture 0, race an invalidation, and still read
    ///    0 at commit — the compare-and-swap silently stops comparing anything.
    ///    A real eviction must leave the counter where it was.
    ///
    /// The existing stress test (`concurrent_refresh_and_write_traffic_stays_bounded`)
    /// cannot see either: it ends with a quiet refresh that re-derives whatever
    /// the race dropped.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_stale_eviction_never_drops_a_newer_record() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("proj");
        project(&root, "proj-1");
        let sessions = root.join(".orgasmic/tmp/sessions");

        // `anchor` exists throughout and only serves to give the unlocked work
        // phase something to do, so the hook has a window to fire in.
        write_session(
            &sessions,
            "anchor",
            4096,
            Some(ReleaseOutcome::Completed),
            &root,
            "proj-1",
        );
        let victim = write_session(
            &sessions,
            "victim",
            4096,
            Some(ReleaseOutcome::Completed),
            &root,
            "proj-1",
        );

        let catalog = RunCatalog::new();
        catalog.refresh_dir(&sessions, Some("proj-1"), &root, SessionScanBudget::DEFAULT);
        assert_eq!(catalog.len(), 2);

        // Move the victim's generation off zero, the way a writer's lifecycle
        // append does, so a later reset to zero is observable.
        catalog.invalidate_session(&victim);
        catalog.refresh_dir(&sessions, Some("proj-1"), &root, SessionScanBudget::DEFAULT);
        let generation_before = catalog.generation_of(&victim);
        assert_eq!(generation_before, 1);
        assert_eq!(catalog.len(), 2);

        // --- 1. the lost update -------------------------------------------
        //
        // This refresh observes the victim absent and plans its eviction. While
        // it works, the file comes back and another refresh indexes the NEW
        // one. The eviction is now decided against a record that no longer
        // exists, and must not run.
        std::fs::remove_file(&victim).unwrap();
        let racer = catalog.clone();
        let racer_root = root.clone();
        let racer_sessions = sessions.clone();
        let fired = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let flag = fired.clone();
        catalog.refresh_dir_observed(
            &sessions,
            Some("proj-1"),
            &root,
            SessionScanBudget::DEFAULT,
            &mut |_| {
                if flag.swap(true, std::sync::atomic::Ordering::SeqCst) {
                    return;
                }
                write_session(
                    &racer_sessions,
                    "victim",
                    8192,
                    Some(ReleaseOutcome::Completed),
                    &racer_root,
                    "proj-1",
                );
                racer.refresh_dir(
                    &racer_sessions,
                    Some("proj-1"),
                    &racer_root,
                    SessionScanBudget::DEFAULT,
                );
            },
        );
        assert!(
            fired.load(std::sync::atomic::Ordering::SeqCst),
            "the interleaving never happened, so this test proved nothing"
        );

        let survivors: Vec<PathBuf> = catalog
            .entries()
            .into_iter()
            .map(|entry| entry.session_path)
            .collect();
        assert!(
            survivors.contains(&victim),
            "a stale eviction dropped a record another refresh had just indexed: {survivors:?}"
        );
        let live = std::fs::symlink_metadata(&victim).unwrap();
        let cached = catalog
            .entries()
            .into_iter()
            .find(|entry| entry.session_path == victim)
            .expect("the newer record survives");
        assert_eq!(
            cached.fingerprint,
            SessionFileFingerprint::of(&live),
            "the surviving record must be the newer one, not a revived corpse"
        );

        // --- 2. a real eviction keeps the generation ----------------------
        //
        // Nothing races this one, so the path really does go. The record
        // leaves; the counter that guards the path does not.
        std::fs::remove_file(&victim).unwrap();
        let stats =
            catalog.refresh_dir(&sessions, Some("proj-1"), &root, SessionScanBudget::DEFAULT);
        assert_eq!(stats.evicted, 1);
        assert_eq!(catalog.len(), 1);
        assert!(
            catalog.generation_of(&victim) >= generation_before,
            "evicting a record reset its path generation to {}, so the next \
             compare-and-swap on this path compares nothing",
            catalog.generation_of(&victim)
        );
    }

    /// Concurrent refreshes, reads and writer invalidations converge without
    /// deadlock and without losing the board.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn concurrent_refresh_and_write_traffic_stays_bounded() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("proj");
        project(&root, "proj-1");
        let sessions = root.join(".orgasmic/tmp/sessions");
        let mut paths = Vec::new();
        for index in 0..16 {
            paths.push(write_session(
                &sessions,
                &format!("run-{index}"),
                64 * 1024,
                Some(ReleaseOutcome::Completed),
                &root,
                "proj-1",
            ));
        }

        let catalog = RunCatalog::new();
        let mut handles = Vec::new();
        for _ in 0..4 {
            let catalog = catalog.clone();
            let sessions = sessions.clone();
            let root = root.clone();
            handles.push(std::thread::spawn(move || {
                for _ in 0..8 {
                    catalog.refresh_dir(
                        &sessions,
                        Some("proj-1"),
                        &root,
                        SessionScanBudget::DEFAULT,
                    );
                }
            }));
        }
        for _ in 0..4 {
            let catalog = catalog.clone();
            let paths = paths.clone();
            handles.push(std::thread::spawn(move || {
                for _ in 0..32 {
                    for path in &paths {
                        catalog.invalidate_session(path);
                    }
                    let _ = catalog.entries();
                }
            }));
        }
        for handle in handles {
            handle.join().expect("no thread may deadlock or panic");
        }

        // One quiet refresh afterwards restores the full board, whatever
        // interleaving happened.
        let stats =
            catalog.refresh_dir(&sessions, Some("proj-1"), &root, SessionScanBudget::DEFAULT);
        assert_eq!(stats.session_files, 16);
        assert_eq!(catalog.len(), 16);
    }

    #[tokio::test]
    async fn an_exact_run_id_resolves_without_a_board_scan() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("proj");
        project(&root, "proj-1");
        let sessions = root.join(".orgasmic/tmp/sessions");
        for index in 0..8 {
            write_session(
                &sessions,
                &format!("run-{index}"),
                4096,
                Some(ReleaseOutcome::Completed),
                &root,
                "proj-1",
            );
        }
        let catalog = RunCatalog::new();
        catalog.refresh_dir(&sessions, Some("proj-1"), &root, SessionScanBudget::DEFAULT);
        let found = catalog.find_by_run_id("run-5").expect("indexed run");
        assert_eq!(found.run_id, "run-5");
        assert!(catalog.find_by_run_id("run-nope").is_none());
    }

    #[tokio::test]
    async fn a_corrupt_snapshot_is_discarded_and_rebuilt() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("proj");
        project(&root, "proj-1");
        let sessions = root.join(".orgasmic/tmp/sessions");
        write_session(
            &sessions,
            "run-a",
            4096,
            Some(ReleaseOutcome::Completed),
            &root,
            "proj-1",
        );

        let good = RunCatalog::new();
        good.refresh_dir(&sessions, Some("proj-1"), &root, SessionScanBudget::DEFAULT);
        let bytes = good.snapshot_bytes(&root).unwrap();
        let snapshot_path = root.join(CATALOG_REL_PATH);
        std::fs::write(&snapshot_path, &bytes).unwrap();

        // Sound snapshot: loads.
        let loaded = RunCatalog::new();
        assert_eq!(
            loaded.load_snapshot(&root),
            SnapshotLoad::Loaded { entries: 1 }
        );

        // Truncated mid-object — what a kill during the write leaves.
        std::fs::write(&snapshot_path, &bytes[..bytes.len() / 2]).unwrap();
        let torn = RunCatalog::new();
        assert!(matches!(
            torn.load_snapshot(&root),
            SnapshotLoad::Corrupt { .. }
        ));
        assert_eq!(torn.len(), 0);
        // And the rebuild produces the same verdict the sound snapshot held.
        torn.refresh_dir(&sessions, Some("proj-1"), &root, SessionScanBudget::DEFAULT);
        let rebuilt = torn.entries();
        let original = good.entries();
        assert_eq!(rebuilt.len(), original.len());
        assert_eq!(rebuilt[0].run_id, original[0].run_id);
        assert_eq!(
            rebuilt[0]
                .terminal
                .as_ref()
                .and_then(TerminalRecord::outcome),
            original[0]
                .terminal
                .as_ref()
                .and_then(TerminalRecord::outcome)
        );
        assert_eq!(
            rebuilt[0].worktree_authority,
            original[0].worktree_authority
        );
    }

    /// Rollback: an older daemon must not read a newer catalog, and a newer one
    /// must not trust an older shape. Both are the same answer — rebuild.
    #[tokio::test]
    async fn a_foreign_version_snapshot_is_refused_in_both_directions() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("proj");
        project(&root, "proj-1");
        let sessions = root.join(".orgasmic/tmp/sessions");
        write_session(
            &sessions,
            "run-a",
            4096,
            Some(ReleaseOutcome::Completed),
            &root,
            "proj-1",
        );
        let source = RunCatalog::new();
        source.refresh_dir(&sessions, Some("proj-1"), &root, SessionScanBudget::DEFAULT);
        let bytes = source.snapshot_bytes(&root).unwrap();
        let mut snapshot: Value = serde_json::from_slice(&bytes).unwrap();
        let path = root.join(CATALOG_REL_PATH);

        for foreign in [0_u64, u64::from(CATALOG_VERSION) + 1] {
            snapshot["catalog_version"] = json!(foreign);
            std::fs::write(&path, serde_json::to_vec(&snapshot).unwrap()).unwrap();
            let catalog = RunCatalog::new();
            assert_eq!(
                catalog.load_snapshot(&root),
                SnapshotLoad::VersionMismatch {
                    found: foreign as u32,
                    expected: CATALOG_VERSION,
                },
                "a catalog version this build does not know must be refused, not read"
            );
            assert_eq!(catalog.len(), 0);
            // Still fully functional: the rebuild is the rollback.
            let stats =
                catalog.refresh_dir(&sessions, Some("proj-1"), &root, SessionScanBudget::DEFAULT);
            assert_eq!(stats.rebuilt, 1);
            assert_eq!(catalog.len(), 1);
        }
    }

    /// A snapshot entry naming a session file outside the project it is loaded
    /// for is refused. Otherwise a copied catalog injects foreign runs into an
    /// unrelated project's inventory.
    #[tokio::test]
    async fn a_snapshot_entry_outside_its_project_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("proj");
        let other = dir.path().join("other");
        project(&root, "proj-1");
        project(&other, "proj-2");
        let sessions = other.join(".orgasmic/tmp/sessions");
        write_session(
            &sessions,
            "run-foreign",
            4096,
            Some(ReleaseOutcome::Completed),
            &other,
            "proj-2",
        );
        let source = RunCatalog::new();
        source.refresh_dir(
            &sessions,
            Some("proj-2"),
            &other,
            SessionScanBudget::DEFAULT,
        );
        let bytes = source.snapshot_bytes(&other).unwrap();
        std::fs::write(root.join(CATALOG_REL_PATH), bytes).unwrap();

        let catalog = RunCatalog::new();
        assert_eq!(
            catalog.load_snapshot(&root),
            SnapshotLoad::Loaded { entries: 0 }
        );
    }

    #[test]
    fn history_lines_are_classified_without_parsing_payloads() {
        let big = "x".repeat(200_000);
        let redraw = format!(
            "{{\"seq\":1,\"kind\":\"driver_event\",\"event\":{{\"type\":\"text_chunk\",\"chunk\":\"{big}\"}}}}"
        );
        assert_eq!(classify_history_line(redraw.as_bytes()), "rendered_tui");
        assert_eq!(
            classify_history_line(
                b"{\"seq\":0,\"kind\":\"lifecycle\",\"event\":{\"phase\":\"acquire\"}}"
            ),
            "lifecycle"
        );
        assert_eq!(
            classify_history_line(
                b"{\"seq\":2,\"kind\":\"driver_event\",\"event\":{\"type\":\"tool_call\"}}"
            ),
            "semantic"
        );
        assert_eq!(
            classify_history_line(
                b"{\"seq\":3,\"kind\":\"driver_event\",\"event\":{\"type\":\"pane_activity\"}}"
            ),
            "pane_activity"
        );
        assert_eq!(classify_history_line(b"not json at all"), "unparsed");
        // Only a PANE transport's rendered TUI is reclaimable; authority,
        // unclassifiable lines, and the same `text_chunk` shape from a
        // structured transport are not (orgasmic:TASK-FZB6T.1 finding 2).
        for pane in ["rmux", "tmux", "tmux-tui"] {
            assert!(class_is_reclaimable("rendered_tui", Some(pane)), "{pane}");
        }
        for structured in ["acp-stdio", "acp-claude", "cursor-acp", "external"] {
            assert!(
                !class_is_reclaimable("rendered_tui", Some(structured)),
                "{structured}: a structured transport's text_chunk is evidence"
            );
        }
        assert!(
            !class_is_reclaimable("rendered_tui", None),
            "a run whose transport was never recorded cannot prove its text_chunk is a repaint"
        );
        for class in [
            "lifecycle",
            "semantic",
            "pane_activity",
            "unparsed",
            "note",
            "blank",
            "torn",
        ] {
            assert!(!class_is_reclaimable(class, Some("rmux")), "{class}");
        }
    }

    /// orgasmic:TASK-FZB6T.2 finding 1 — the discriminators are PROVEN, not
    /// found. A byte scan takes the first `"type":"` in the line; the envelope
    /// schema puts the one that decides the class at a specific place, and
    /// anything nested deeper is payload.
    ///
    /// Every case below was classified WRONG by the byte scan, and every one of
    /// them is a record maintenance would then have deleted.
    #[test]
    fn a_nested_type_never_decides_an_envelope_class() {
        // The headline case: an ACP tool result whose content block is a
        // `text_chunk`, stated BEFORE the event's own discriminator.
        let collision = br#"{"seq":1,"kind":"driver_event","event":{"output":{"content":[{"type":"text_chunk","text":"hi"}]},"type":"tool_result"}}"#;
        assert_eq!(classify_history_line(collision), "semantic");

        // The same trick one level up: a nested `kind` before the envelope's.
        let nested_kind = br#"{"seq":1,"event":{"meta":{"kind":"lifecycle"},"type":"text_chunk"},"kind":"driver_event"}"#;
        assert_eq!(classify_history_line(nested_kind), "rendered_tui");

        // A nested `type` inside a STRING, which no structural reader can
        // mistake for a key and every substring search does.
        let in_a_string = br#"{"seq":1,"kind":"driver_event","event":{"chunk":"the log said \"type\":\"text_chunk\" here","type":"tool_result"}}"#;
        assert_eq!(classify_history_line(in_a_string), "semantic");

        // A nested `text_chunk` under a run that also nests an object array.
        let deep = br#"{"kind":"driver_event","event":{"args":[[{"type":"text_chunk"}],{"a":{"b":{"type":"text_chunk"}}}],"type":"tool_call"}}"#;
        assert_eq!(classify_history_line(deep), "semantic");

        // FAIL CLOSED. Anything whose structure cannot be proven is
        // `unparsed`, and `unparsed` is never reclaimable — so an unreadable
        // line is refused rather than deleted on a guess.
        for unprovable in [
            // Truncated mid-object.
            &br#"{"kind":"driver_event","event":{"type":"text_chunk","#[..],
            // `event` is not an object, so it has no discriminator.
            &br#"{"kind":"driver_event","event":"type=text_chunk"}"#[..],
            // Two top-level objects on one line.
            &br#"{"kind":"driver_event","event":{"type":"text_chunk"}}{"kind":"note"}"#[..],
            // A trailing comma: not JSON, whatever it looks like.
            &br#"{"kind":"driver_event","event":{"type":"text_chunk"},}"#[..],
            // No envelope `kind` at all.
            &br#"{"event":{"type":"text_chunk"}}"#[..],
            // A non-string `kind`.
            &br#"{"kind":7,"event":{"type":"text_chunk"}}"#[..],
            // A non-string `type`.
            &br#"{"kind":"driver_event","event":{"type":7}}"#[..],
        ] {
            assert_eq!(
                classify_history_line(unprovable),
                "unparsed",
                "{}",
                String::from_utf8_lossy(unprovable)
            );
            assert!(!class_is_reclaimable(
                classify_history_line(unprovable),
                Some("rmux")
            ));
        }

        // Duplicate keys resolve the way a parse of the same line resolves
        // them — last wins — so the accounting can never disagree with what a
        // reader of the record would see.
        let duplicated =
            br#"{"kind":"driver_event","event":{"type":"text_chunk","type":"tool_result"}}"#;
        assert_eq!(classify_history_line(duplicated), "semantic");
        let parsed: Value = serde_json::from_slice(duplicated).unwrap();
        assert_eq!(parsed["event"]["type"].as_str(), Some("tool_result"));

        // And the honest rendered-TUI shape still classifies, with whitespace
        // and escapes in the payload.
        //
        // orgasmic:TASK-FZB6T.3 finding 3 — this fixture used to carry RAW
        // `ESC` bytes, which no JSON reader accepts inside a string and which
        // the session writer never emits; it passed only because the scan did
        // not validate. The escaped form is what is actually on disk, and the
        // assertion below pins the fixture to what a parser accepts so it
        // cannot drift back.
        let real_redraw = br#" {"seq":1, "kind":"driver_event", "event":{"stream":"stdout","chunk":"\u001b[H\u001b[2J\"quoted\"","type":"text_chunk"}} "#;
        assert!(serde_json::from_slice::<Value>(real_redraw.trim_ascii()).is_ok());
        assert_eq!(classify_history_line(real_redraw), "rendered_tui");
    }

    /// orgasmic:TASK-FZB6T.3 finding 3 — the scan VALIDATES; it does not merely
    /// navigate.
    ///
    /// The structural reader closed the nested-key collision and then accepted
    /// any non-empty primitive token as a value, any escape sequence as an
    /// escape, and any byte inside a string as content. So
    /// `{"kind":"driver_event","event":{"type":"text_chunk","payload":truX}}` —
    /// which no JSON reader accepts — was classified `rendered_tui`, and a
    /// maintenance pass then deleted its bytes. A record whose validity cannot
    /// be proven is exactly the record that must not be deleted on this
    /// accounting's say-so.
    ///
    /// Every case is cross-checked against `serde_json`: the claim is not
    /// "the scan rejects these", it is "the scan and a real parser agree", which
    /// is the only version of the claim that stays true as the corpus changes.
    #[test]
    fn a_record_this_scan_cannot_prove_valid_is_never_reclaimable() {
        // Each case is INVALID JSON wearing a reclaimable envelope's clothes.
        let invalid: Vec<Vec<u8>> = vec![
            // The reviewer's case: a truncated `true` literal.
            br#"{"kind":"driver_event","event":{"type":"text_chunk","payload":truX}}"#.to_vec(),
            // The other two literals, equally truncated.
            br#"{"kind":"driver_event","event":{"type":"text_chunk","a":fals}}"#.to_vec(),
            br#"{"kind":"driver_event","event":{"type":"text_chunk","a":nul}}"#.to_vec(),
            // Not a literal at all.
            br#"{"kind":"driver_event","event":{"type":"text_chunk","a":NaN}}"#.to_vec(),
            // Numbers that are not JSON numbers: leading zero, bare sign,
            // trailing point, leading point, empty exponent, hex.
            br#"{"kind":"driver_event","event":{"type":"text_chunk","a":01}}"#.to_vec(),
            br#"{"kind":"driver_event","event":{"type":"text_chunk","a":-}}"#.to_vec(),
            br#"{"kind":"driver_event","event":{"type":"text_chunk","a":1.}}"#.to_vec(),
            br#"{"kind":"driver_event","event":{"type":"text_chunk","a":.5}}"#.to_vec(),
            br#"{"kind":"driver_event","event":{"type":"text_chunk","a":1e}}"#.to_vec(),
            br#"{"kind":"driver_event","event":{"type":"text_chunk","a":0x1f}}"#.to_vec(),
            br#"{"kind":"driver_event","event":{"type":"text_chunk","a":+1}}"#.to_vec(),
            // An escape sequence no JSON reader accepts.
            br#"{"kind":"driver_event","event":{"type":"text_chunk","chunk":"\x41"}}"#.to_vec(),
            // A `\u` escape with too few hex digits, and one that is not hex.
            br#"{"kind":"driver_event","event":{"type":"text_chunk","chunk":"\u01"}}"#.to_vec(),
            br#"{"kind":"driver_event","event":{"type":"text_chunk","chunk":"\u00zz"}}"#.to_vec(),
            // A lone high surrogate and a lone low surrogate.
            br#"{"kind":"driver_event","event":{"type":"text_chunk","chunk":"\ud83d!"}}"#.to_vec(),
            br#"{"kind":"driver_event","event":{"type":"text_chunk","chunk":"\udc00"}}"#.to_vec(),
            // A raw control byte inside a string, which is what a half-written
            // legacy pane payload actually looks like on disk.
            [
                &br#"{"kind":"driver_event","event":{"type":"text_chunk","chunk":""#[..],
                &[0x01][..],
                &br#""}}"#[..],
            ]
            .concat(),
            // A raw newline inside a string.
            [
                &br#"{"kind":"driver_event","event":{"type":"text_chunk","chunk":""#[..],
                &[b'\n'][..],
                &br#""}}"#[..],
            ]
            .concat(),
            // Bytes that are not UTF-8 at all: JSON is defined over text.
            [
                &br#"{"kind":"driver_event","event":{"type":"text_chunk","chunk":""#[..],
                &[0xff, 0xfe][..],
                &br#""}}"#[..],
            ]
            .concat(),
        ];
        for case in &invalid {
            let rendered = String::from_utf8_lossy(case).to_string();
            assert!(
                serde_json::from_slice::<Value>(case).is_err(),
                "fixture is not actually invalid JSON: {rendered}"
            );
            assert_eq!(
                classify_history_line(case),
                "unparsed",
                "an invalid record must not be classified as reclaimable payload: {rendered}"
            );
            assert!(
                !class_is_reclaimable(classify_history_line(case), Some("rmux")),
                "{rendered}"
            );
        }

        // The mirror half, and the reason this cannot just refuse everything:
        // every VALID shape the same grammar produces still classifies, so
        // hardening the scan did not quietly stop reclaiming real payload.
        let valid: Vec<&[u8]> = vec![
            br#"{"kind":"driver_event","event":{"type":"text_chunk","a":true,"b":false,"c":null}}"#,
            br#"{"kind":"driver_event","event":{"type":"text_chunk","a":-0.5e+10,"b":0,"c":12}}"#,
            // A surrogate PAIR, which is legal, alongside every escape JSON
            // defines.
            br#"{"kind":"driver_event","event":{"type":"text_chunk","chunk":"\ud83d\ude00\/\b\f\n\r\t\\\""}}"#,
            "{\"kind\":\"driver_event\",\"event\":{\"type\":\"text_chunk\",\"chunk\":\"héllo ✓\"}}"
                .as_bytes(),
        ];
        for case in valid {
            let rendered = String::from_utf8_lossy(case).to_string();
            assert!(
                serde_json::from_slice::<Value>(case).is_ok(),
                "fixture is not actually valid JSON: {rendered}"
            );
            assert_eq!(
                classify_history_line(case),
                "rendered_tui",
                "a valid rendered pane payload must still be reclaimable: {rendered}"
            );
        }
    }

    /// orgasmic:TASK-FZB6T.4 finding 3 — the scan's whitespace must be JSON's
    /// whitespace, at every token boundary, in both directions.
    ///
    /// The hole was FORM FEED `0x0c`: `u8::is_ascii_whitespace` accepts it and
    /// JSON does not, so a `text_chunk` line carrying one between tokens walked
    /// clean, classified `rendered_tui`, and became eligible for deletion —
    /// while `serde_json` rejects the identical bytes. VERTICAL TAB `0x0b` was
    /// named in the same finding but never reproduced: Rust's set excludes it
    /// (measured below, not assumed), so it already failed closed. Both are
    /// pinned here because the class is what must stay shut.
    ///
    /// This is a generated differential rather than a fixture list: every
    /// candidate byte is inserted at EVERY token boundary of a real reclaimable
    /// envelope, and the scan's verdict is required to agree with `serde_json`
    /// at each one. A future edit to the predicate cannot reopen the class
    /// without failing here.
    #[test]
    fn the_scan_skips_exactly_the_whitespace_json_permits() {
        // Measured, so the comment above is a fact about this toolchain rather
        // than a recollection. If Rust ever changes this set, this line is where
        // it surfaces.
        let rust_ascii_ws: Vec<u8> = (0u8..=0x7f).filter(u8::is_ascii_whitespace).collect();
        assert_eq!(
            rust_ascii_ws,
            vec![0x09, 0x0a, 0x0c, 0x0d, 0x20],
            "`u8::is_ascii_whitespace` is not the set this finding was decided against"
        );
        assert!(
            !rust_ascii_ws.contains(&0x0b),
            "vertical tab is NOT in Rust's ascii-whitespace set; the finding's second byte \
             does not reproduce"
        );

        // A real reclaimable envelope, with every structural boundary marked by
        // `|`. The marker is removed before use; each position is one insertion
        // point for the differential below.
        const MARKED: &str = r#"|{|"kind"|:|"driver_event"|,|"event"|:|{|"type"|:|"text_chunk"|,|"chunk"|:|"redraw"|,|"n"|:|12|}|}|"#;
        let template: Vec<u8> = MARKED.bytes().filter(|byte| *byte != b'|').collect();
        let boundaries: Vec<usize> = MARKED
            .bytes()
            .enumerate()
            .filter(|(_, byte)| *byte == b'|')
            .enumerate()
            .map(|(seen, (index, _))| index - seen)
            .collect();
        assert_eq!(classify_history_line(&template), "rendered_tui");
        assert!(boundaries.len() >= 20, "{boundaries:?}");

        // Everything an operator could plausibly find wedged between two tokens:
        // JSON's four, the two the old predicate got wrong, and NUL for good
        // measure.
        for byte in [b' ', b'\t', b'\n', b'\r', 0x0b, 0x0c, 0x00] {
            let json_permits = matches!(byte, b' ' | b'\t' | b'\n' | b'\r');
            for boundary in &boundaries {
                let mut line = template.clone();
                line.insert(*boundary, byte);
                let parses = serde_json::from_slice::<Value>(&line).is_ok();
                assert_eq!(
                    parses,
                    json_permits,
                    "serde_json disagrees with RFC 8259 for 0x{byte:02x} at {boundary}: {}",
                    String::from_utf8_lossy(&line)
                );
                let class = classify_history_line(&line);
                assert_eq!(
                    class == "rendered_tui",
                    parses,
                    "the scan and the parser must agree for 0x{byte:02x} at offset {boundary}: \
                     classified {class}, serde_json parses = {parses}"
                );
                assert!(
                    parses || !class_is_reclaimable(class, Some("rmux")),
                    "a line no parser accepts must never be reclaimable: 0x{byte:02x} at \
                     offset {boundary}"
                );
            }
        }

        // And the two named bytes as whole-line fixtures, cross-checked, so the
        // finding's exact shape is on record independent of the generator.
        for byte in [0x0b_u8, 0x0c_u8] {
            let line = [
                &br#"{"kind":"driver_event","#[..],
                &[byte][..],
                &br#""event":{"type":"text_chunk","chunk":"redraw"}}"#[..],
            ]
            .concat();
            assert!(
                serde_json::from_slice::<Value>(&line).is_err(),
                "0x{byte:02x} is not JSON whitespace"
            );
            assert_eq!(
                classify_history_line(&line),
                "unparsed",
                "0x{byte:02x} between tokens must fail the scan closed"
            );
            assert!(!class_is_reclaimable(
                classify_history_line(&line),
                Some("rmux")
            ));
        }

        // Trailing-content site: the same predicate guards the bytes AFTER the
        // top-level object, and it must refuse there too.
        for byte in [0x0b_u8, 0x0c_u8] {
            let line = [&template[..], &[byte][..]].concat();
            assert!(serde_json::from_slice::<Value>(&line).is_err());
            assert_eq!(
                classify_history_line(&line),
                "unparsed",
                "trailing 0x{byte:02x} must refuse: this is the second scanner site"
            );
        }
        for byte in [b' ', b'\t', b'\r'] {
            let line = [&template[..], &[byte][..]].concat();
            assert!(serde_json::from_slice::<Value>(&line).is_ok());
            assert_eq!(
                classify_history_line(&line),
                "rendered_tui",
                "trailing JSON whitespace must still classify"
            );
        }
    }

    /// Every byte on disk lands in exactly one class, including the two shapes
    /// the first cut lost: a blank record, and a final record with no
    /// terminating newline (orgasmic:TASK-FZB6T.1 finding 2).
    #[test]
    fn blank_and_torn_records_are_accounted_exactly() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("run-torn.jsonl");
        let mut content = String::new();
        content.push_str("{\"seq\":0,\"kind\":\"lifecycle\",\"event\":{\"phase\":\"acquire\"}}\n");
        content.push('\n');
        content.push_str("   \n");
        content.push_str(
            "{\"seq\":1,\"kind\":\"driver_event\",\"event\":{\"type\":\"text_chunk\"}}\n",
        );
        // Torn: the process died mid-line, so there is no trailing newline.
        content.push_str("{\"seq\":2,\"kind\":\"driver_event\",\"event\":{\"type\":\"text_ch");
        std::fs::write(&path, &content).unwrap();

        let totals = inspect_session_file(&path).unwrap();
        let accounted: u64 = totals.values().map(|class| class.bytes).sum();
        assert_eq!(
            accounted,
            content.len() as u64,
            "every byte on disk must land in exactly one class: {totals:?}"
        );
        assert_eq!(totals["blank"].lines, 2);
        assert_eq!(totals["blank"].bytes, 1 + 4);
        assert_eq!(totals["torn"].lines, 1);
        assert!(
            !class_is_reclaimable("torn", Some("rmux")),
            "a record that may have been cut off mid-write is never reclaimed"
        );
        assert_eq!(totals["rendered_tui"].lines, 1);
        assert_eq!(totals["lifecycle"].lines, 1);
    }

    /// The dry run must account for reclaimable bytes exactly, and change
    /// nothing.
    #[tokio::test]
    async fn the_dry_run_accounts_exactly_and_writes_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("proj");
        project(&root, "proj-1");
        let sessions = root.join(".orgasmic/tmp/sessions");
        let path = write_session(
            &sessions,
            "run-a",
            256 * 1024,
            Some(ReleaseOutcome::Completed),
            &root,
            "proj-1",
        );
        let before_bytes = std::fs::read(&path).unwrap();
        let before_meta = std::fs::metadata(&path).unwrap();

        let catalog = RunCatalog::new();
        catalog.refresh_dir(&sessions, Some("proj-1"), &root, SessionScanBudget::DEFAULT);
        let report = inspect_history(&catalog.entries());

        assert!(report.dry_run);
        assert_eq!(report.unreadable_files, 0);
        assert_eq!(
            report.bytes_accounted,
            before_meta.len(),
            "every byte on disk must land in exactly one class"
        );
        // The rendered-TUI class is the reclaimable one and it is the bulk.
        let rendered: u64 = report
            .buckets
            .iter()
            .filter(|bucket| bucket.event_class == "rendered_tui")
            .map(|bucket| bucket.totals.bytes)
            .sum();
        assert_eq!(report.reclaimable_bytes, rendered);
        assert!(rendered > 256 * 1024);
        assert_eq!(
            report.reclaimable_by_driver.get("rmux/claude").copied(),
            Some(rendered)
        );
        // Lifecycle is accounted and never reclaimable.
        assert!(report
            .buckets
            .iter()
            .any(|bucket| bucket.event_class == "lifecycle" && !bucket.reclaimable));

        assert_eq!(std::fs::read(&path).unwrap(), before_bytes);
        assert_eq!(std::fs::metadata(&path).unwrap().len(), before_meta.len());
    }
}
