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
    /// **Terminal for the run identity.** It leaves this state only when a
    /// directory reappears at the recorded path carrying exactly the
    /// `verified_identity` this run was once verified against — the one fact
    /// that proves continuity rather than coincidence of names. A tombstone
    /// that never had a verified identity (the recorded path was already gone
    /// at first index) can never be revived.
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
        }
    }

    pub fn label(&self) -> &'static str {
        match self {
            Self::Verified { .. } => "verified",
            Self::Tombstoned { .. } => "tombstoned",
            Self::Mismatched { .. } => "mismatched",
            Self::Unrecorded => "unrecorded",
            Self::Unidentified => "unidentified",
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
    /// The recorded `RunMeta` project/worktree pair in the shape
    /// [`verify_worktree_authority`] consumes.
    pub fn recorded_run_meta(&self) -> Option<(Option<String>, Option<PathBuf>)> {
        self.run_meta_recorded.then(|| {
            (
                self.run_meta_project.clone(),
                self.run_meta_worktree.clone(),
            )
        })
    }

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
        let stale: Vec<PathBuf>;
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
                            previous: entry.worktree_authority.clone(),
                            run_meta: entry.recorded_run_meta(),
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
            let present: std::collections::BTreeSet<&PathBuf> =
                observed.iter().map(|(path, _, _)| path).collect();
            stale = state
                .by_path
                .keys()
                .filter(|path| path.parent() == Some(dir) && !present.contains(path))
                .cloned()
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
            if let Some(refreshed) = reverify_authority(
                &planned.previous,
                probe,
                project_id,
                planned.run_meta.clone(),
            ) {
                authority_updates.push((planned, refreshed));
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
        for path in stale {
            if state.by_path.remove(&path).is_some() {
                state.invalidations.remove(&path);
                stats.evicted += 1;
                dirty = true;
            }
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
    previous: WorktreeAuthority,
    run_meta: Option<(Option<String>, Option<PathBuf>)>,
    invalidation: u64,
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
/// Four checks, each refusing one of those shapes:
///
/// 1. the path is a **direct child** of this project's sessions directory, so
///    session-directory authority — not the project root — is what admits it;
/// 2. no path component is `..`, so a normalized-looking parent cannot be
///    reached by escaping and coming back;
/// 3. the file is a **regular file** today, not a symlink, directory, or fifo;
/// 4. its **current identity** (device/inode/length/mtime) is exactly the one
///    the entry was derived from.
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
    SessionFileFingerprint::of(&metadata) == entry.fingerprint
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
/// `None` when the verdict is a property of the session file rather than of the
/// filesystem: a mismatch is a statement about project identity, which does not
/// change under a live daemon, and the two "no authority recorded" verdicts are
/// properties of the session file itself.
///
/// Blocking; the refresh runs this outside the catalog mutex.
fn probe_authority_path(authority: &WorktreeAuthority) -> Option<AuthorityProbe> {
    let path = match authority {
        WorktreeAuthority::Verified { worktree, .. } => worktree.as_path(),
        WorktreeAuthority::Tombstoned { recorded, .. } => recorded.as_path(),
        WorktreeAuthority::Mismatched { .. }
        | WorktreeAuthority::Unrecorded
        | WorktreeAuthority::Unidentified => return None,
    };
    Some(AuthorityProbe {
        current: DirIdentity::at(path),
        exists: path.exists(),
    })
}

/// Re-derive a cached authority verdict against one [`AuthorityProbe`].
/// `None` means "unchanged".
///
/// orgasmic:TASK-FZB6T.1 finding 5 — the tombstone is terminal for the run
/// identity. Existence at the recorded path is no longer enough to revive it:
/// only the *same directory object* (dev/ino) the run was once verified against
/// proves continuity. A pruned worktree whose path is later reused by an
/// unrelated checkout stays tombstoned, so a dead run cannot become an attach
/// candidate again by coincidence of names.
///
/// Blocking (it may canonicalize and read a `project.org`); runs outside the
/// catalog mutex.
fn reverify_authority(
    previous: &WorktreeAuthority,
    probe: AuthorityProbe,
    project_id: Option<&str>,
    run_meta: Option<(Option<String>, Option<PathBuf>)>,
) -> Option<WorktreeAuthority> {
    match previous {
        WorktreeAuthority::Verified { worktree, identity } => {
            let still_the_same = match identity {
                // Durable identity available: the directory object must be the
                // same one, not merely a directory with the same name.
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
        WorktreeAuthority::Tombstoned {
            verified_identity, ..
        } => {
            // A tombstone with no verified identity was already gone at first
            // index; nothing can prove continuity for it.
            let identity = (*verified_identity)?;
            if probe.current != Some(identity) {
                return None;
            }
            match verify_worktree_authority(project_id, run_meta) {
                refreshed @ WorktreeAuthority::Verified { .. } => Some(refreshed),
                _ => None,
            }
        }
        _ => None,
    }
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

/// Build a catalog entry from a bounded lifecycle scan.
pub(crate) fn entry_from_scan(
    scan: &SessionLifecycleScan,
    path: &Path,
    project_id: Option<&str>,
    project_root: &Path,
    fingerprint: SessionFileFingerprint,
) -> RunCatalogEntry {
    let envelopes = latest_run_segment(&scan.envelopes);
    let compact = compact_envelopes(envelopes);
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

    let release = final_release_outcome(scan, envelopes);
    let driver_event = driver_terminal_event(envelopes);
    let terminal = terminal_record(
        release,
        driver_event.as_ref(),
        envelopes.last().map(|envelope| envelope.time),
        external_registration,
    );
    RunCatalogEntry {
        run_id: first.map(|e| e.run_id.clone()).unwrap_or_default(),
        runtime_id: first.map(|e| e.runtime_id.clone()).unwrap_or_default(),
        boot_id: first.map(|e| e.boot_id.clone()).unwrap_or_default(),
        session_path: path.to_path_buf(),
        project_id: project_id.map(str::to_string),
        project_root: Some(project_root.to_path_buf()),
        task_id,
        kind,
        worker_id,
        stage,
        transport,
        harness,
        native,
        worktree_authority: verify_worktree_authority(project_id, run_meta.clone()),
        run_meta_project: run_meta.as_ref().and_then(|(project, _)| project.clone()),
        run_meta_worktree: run_meta.as_ref().and_then(|(_, worktree)| worktree.clone()),
        run_meta_recorded: run_meta.is_some(),
        terminal,
        final_release_outcome: release,
        driver_terminal_event: driver_event.map(|(event, _)| event),
        external_registration,
        replacement_run_id,
        replacement_session_path,
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

/// The raw `Release` outcome on the file's genuine final envelope.
///
/// `None` when the scan dropped that line as transcript (the normal shape for a
/// run that is still writing) or when the final envelope is not a release —
/// only the genuine final envelope can prove a release, and treating a newest
/// RETAINED lifecycle line as the end of the run would tombstone a live one.
fn final_release_outcome(
    scan: &SessionLifecycleScan,
    envelopes: &[SessionEnvelope],
) -> Option<ReleaseOutcome> {
    if !scan.final_envelope_retained {
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

/// Classify one raw JSONL line without parsing its payload.
///
/// Deliberately byte-level and bounded: the whole point of the inspect command
/// is to account for 2.239 GiB of legacy history, and it must not cost a parse
/// of every transcript line to do it.
pub fn classify_history_line(line: &[u8]) -> &'static str {
    const PROBE: usize = 1024;
    let header = &line[..line.len().min(PROBE)];
    let kind = probe_value(header, b"\"kind\":\"");
    match kind.as_deref() {
        Some("lifecycle") => return "lifecycle",
        Some("babysitter_summary") => return "babysitter_summary",
        Some("note") => return "note",
        Some("driver_event") => {}
        _ => return "unparsed",
    }
    // Driver events: the rendered-TUI class is the legacy `text_chunk` written
    // by a pane transport before dec_WDR5K item 7 — the payload the maintenance
    // command exists to account for. Everything else is semantic evidence.
    match probe_value(&line[..line.len().min(64 * 1024)], b"\"type\":\"").as_deref() {
        Some("pane_activity") => "pane_activity",
        Some("text_chunk") => "rendered_tui",
        Some(_) => "semantic",
        None => "unparsed",
    }
}

fn probe_value(probe: &[u8], key: &[u8]) -> Option<String> {
    let start = find_bytes(probe, key)? + key.len();
    let rest = &probe[start..];
    let end = rest.iter().position(|byte| *byte == b'"')?;
    std::str::from_utf8(&rest[..end]).ok().map(str::to_string)
}

fn find_bytes(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    if needle.is_empty() || haystack.len() < needle.len() {
        return None;
    }
    haystack
        .windows(needle.len())
        .position(|window| window == needle)
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
/// Mirrors `PaneMux::for_transport` in `supervisor.rs` deliberately rather than
/// sharing it: this module must not depend on the supervisor's private
/// classifier, and the two answer different questions about the same string.
pub fn transport_is_pane(transport: &str) -> bool {
    matches!(transport.trim(), "rmux" | "tmux" | "tmux-tui")
}

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

    /// The one transition a tombstone IS allowed to make: the SAME directory
    /// object comes back, which is what an unmounted volume returning looks
    /// like. Proven by making the recorded path a symlink and swapping it back
    /// to the original directory.
    #[tokio::test]
    async fn a_tombstone_lifts_only_when_the_original_directory_returns() {
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

        // The very same directory comes back.
        std::fs::rename(dir.path().join("stashed"), &real).unwrap();
        catalog.refresh_dir(&sessions, Some("proj-1"), &root, SessionScanBudget::DEFAULT);
        assert!(
            catalog.entries()[0]
                .worktree_authority
                .verified_worktree()
                .is_some(),
            "the original directory object returning IS continuity: {:?}",
            catalog.entries()[0].worktree_authority
        );
    }

    /// orgasmic:TASK-FZB6T.1 finding 7 — a snapshot entry is admitted through
    /// session-directory authority and current file identity, not by living
    /// somewhere under the project root.
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
