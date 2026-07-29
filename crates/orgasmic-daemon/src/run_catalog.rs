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
use std::sync::Arc;

use chrono::{DateTime, Utc};
use orgasmic_core::session::{
    scan_session_lifecycle, Lifecycle, ReleaseOutcome, SessionEnvelope, SessionEventKind,
    SessionLifecycleScan, SessionScanBudget,
};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use tokio::sync::Mutex;

/// Snapshot format version. Bumped whenever an entry's meaning changes in a way
/// an older daemon would misread. A snapshot whose version is not exactly this
/// is discarded and rebuilt — forward and backward alike, which is the rollback
/// story: install an older runtime and it re-indexes instead of trusting a
/// record shape it does not know.
pub const CATALOG_VERSION: u32 = 1;

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
    Verified { worktree: PathBuf },
    /// `RunMeta` named a worktree that is no longer on disk. Pruned, moved, or
    /// on an unmounted volume. Stable: recovery under this identity is over.
    Tombstoned { recorded: PathBuf },
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
            Self::Verified { worktree } => Some(worktree.as_path()),
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

#[derive(Debug, Default)]
struct CatalogState {
    by_path: BTreeMap<PathBuf, RunCatalogEntry>,
    dirty: bool,
    last_saved: Option<std::time::Instant>,
}

/// The daemon-lifetime run catalog.
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

    /// Load a project's durable snapshot into the catalog.
    ///
    /// Never fails: an absent, unreadable, corrupt, or foreign-version snapshot
    /// is discarded and the next [`Self::refresh_dir`] rebuilds from the session
    /// files. The catalog is derived state; there is nothing here that a rebuild
    /// cannot reproduce.
    pub async fn load_snapshot(&self, project_root: &Path) -> SnapshotLoad {
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
        let mut state = self.state.lock().await;
        let mut loaded = 0;
        for entry in snapshot.entries {
            // A snapshot entry naming a file outside the project it was loaded
            // for is not this project's record. Refuse rather than let a hand
            // edited or copied snapshot introduce foreign runs.
            if !entry.session_path.starts_with(project_root) {
                continue;
            }
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
    /// Calling this CONSUMES the dirty flag: the caller is expected to persist
    /// the bytes it returns.
    pub async fn snapshot_bytes(&self, project_root: &Path) -> Option<Vec<u8>> {
        self.snapshot_bytes_after(project_root, SNAPSHOT_MIN_INTERVAL)
            .await
    }

    /// [`Self::snapshot_bytes`] with an explicit floor. `Duration::ZERO` forces
    /// a save; tests and the boot path use it.
    pub async fn snapshot_bytes_after(
        &self,
        project_root: &Path,
        min_interval: std::time::Duration,
    ) -> Option<Vec<u8>> {
        let mut state = self.state.lock().await;
        if !state.dirty {
            return None;
        }
        if let Some(last) = state.last_saved {
            if last.elapsed() < min_interval {
                return None;
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
        state.dirty = false;
        state.last_saved = Some(std::time::Instant::now());
        Some(bytes)
    }

    /// Bring the catalog up to date for one project's session directory.
    ///
    /// Blocking filesystem work; callers must keep it off the async runtime's
    /// hot threads the same way the boot scan does.
    pub async fn refresh_dir(
        &self,
        dir: &Path,
        project_id: Option<&str>,
        project_root: &Path,
        budget: SessionScanBudget,
    ) -> CatalogRefreshStats {
        let mut stats = CatalogRefreshStats::default();
        let mut state = self.state.lock().await;
        let mut seen: Vec<PathBuf> = Vec::new();

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
            seen.push(path.clone());
            stats.session_files += 1;
            stats.session_file_bytes += metadata.len();

            let fingerprint = SessionFileFingerprint::of(&metadata);
            let cached_is_current = state
                .by_path
                .get(&path)
                .is_some_and(|entry| entry.fingerprint == fingerprint);
            if cached_is_current {
                stats.cache_hits += 1;
                // Worktree authority is the one verdict that can change without
                // the session file changing, so it is re-checked — but with a
                // single existence probe per record, which is O(records) and
                // never O(bytes). Full re-verification runs only when that
                // probe disagrees with the cached verdict.
                let entry = state.by_path.get_mut(&path).expect("checked above");
                if authority_needs_recheck(&entry.worktree_authority) {
                    let refreshed = verify_worktree_authority(
                        project_id,
                        run_meta_project_worktree(&entry.lifecycle_envelopes),
                    );
                    if refreshed != entry.worktree_authority {
                        entry.worktree_authority = refreshed;
                        stats.authority_reverified += 1;
                        state.dirty = true;
                    }
                }
                if state.by_path[&path].scan_truncated {
                    stats.truncated_scans += 1;
                }
                if state.by_path[&path].unreadable {
                    stats.unreadable_sessions += 1;
                }
                continue;
            }

            stats.rebuilt += 1;
            let built = match scan_session_lifecycle(&path, budget) {
                Ok(scan) => {
                    stats.bytes_inspected += scan.bytes_inspected;
                    if scan.truncated {
                        stats.truncated_scans += 1;
                    }
                    entry_from_scan(&scan, &path, project_id, project_root, fingerprint)
                }
                Err(_) => {
                    stats.unreadable_sessions += 1;
                    unreadable_entry(&path, project_id, project_root, fingerprint, metadata.len())
                }
            };
            state.by_path.insert(path, built);
            state.dirty = true;
        }

        // Evict records whose session file is gone from this directory. Scoped
        // to `dir` so refreshing one project never drops another's entries.
        let stale: Vec<PathBuf> = state
            .by_path
            .keys()
            .filter(|path| path.parent() == Some(dir) && !seen.contains(path))
            .cloned()
            .collect();
        for path in stale {
            state.by_path.remove(&path);
            stats.evicted += 1;
            state.dirty = true;
        }
        stats
    }

    /// Every entry, ordered by session path.
    pub async fn entries(&self) -> Vec<RunCatalogEntry> {
        self.state.lock().await.by_path.values().cloned().collect()
    }

    /// Entries under one project root, ordered by session path.
    pub async fn entries_for_project(&self, project_root: &Path) -> Vec<RunCatalogEntry> {
        self.state
            .lock()
            .await
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
    pub async fn invalidate_session(&self, session_path: &Path) {
        let mut state = self.state.lock().await;
        if state.by_path.remove(session_path).is_some() {
            state.dirty = true;
        }
    }

    #[cfg(test)]
    pub(crate) async fn len(&self) -> usize {
        self.state.lock().await.by_path.len()
    }
}

/// Whether a cached authority verdict must be re-derived.
///
/// One `symlink_metadata` per record: a verified worktree that vanished, or a
/// tombstoned worktree that came back, are the only two transitions the
/// filesystem can make behind an unchanged session file.
fn authority_needs_recheck(authority: &WorktreeAuthority) -> bool {
    match authority {
        WorktreeAuthority::Verified { worktree } => !worktree.exists(),
        WorktreeAuthority::Tombstoned { recorded } => recorded.exists(),
        // A mismatch is a statement about project identity, which does not
        // change under a live daemon, and the two "no authority recorded"
        // verdicts are properties of the session file itself.
        WorktreeAuthority::Mismatched { .. }
        | WorktreeAuthority::Unrecorded
        | WorktreeAuthority::Unidentified => false,
    }
}

/// The `RunMeta` project/worktree pair, if the session recorded one.
fn run_meta_project_worktree(
    envelopes: &[SessionEnvelope],
) -> Option<(Option<String>, Option<PathBuf>)> {
    envelopes.iter().find_map(|envelope| {
        if envelope.kind != SessionEventKind::Lifecycle {
            return None;
        }
        match serde_json::from_value::<Lifecycle>(envelope.event.clone()).ok()? {
            Lifecycle::RunMeta {
                project_id,
                worktree,
                ..
            } => Some((project_id, worktree)),
            _ => None,
        }
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
        return WorktreeAuthority::Tombstoned { recorded };
    }
    let Ok(canonical) = recorded.canonicalize() else {
        return WorktreeAuthority::Mismatched { recorded };
    };
    match crate::api::read_existing_project_identity(&canonical.join(".orgasmic/project.org")) {
        Ok(identity) if identity.project_id == project_id => WorktreeAuthority::Verified {
            worktree: canonical,
        },
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
        worktree_authority: verify_worktree_authority(project_id, run_meta),
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
pub const EVENT_CLASSES: [&str; 7] = [
    "lifecycle",
    "rendered_tui",
    "semantic",
    "pane_activity",
    "babysitter_summary",
    "note",
    "unparsed",
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

/// Whether an event class may be reclaimed by a maintenance pass.
///
/// Only `rendered_tui` is: it is storage the current build already forbids
/// (dec_WDR5K item 7), it carries no lifecycle or native correlation, and it is
/// the entire 2.239 GiB story. Lifecycle is authority. Semantic events are
/// budgeted evidence, already capped at write time. `unparsed` is refused on
/// principle — a line this accounting could not classify is the last line that
/// should be deleted on its say-so.
pub fn class_is_reclaimable(event_class: &str) -> bool {
    event_class == "rendered_tui"
}

/// Read one session file and account for it by event class.
///
/// Whole-file by necessity — accounting for bytes means visiting them — which
/// is why this runs only under an explicit operator command and never on an
/// inventory poll.
pub fn inspect_session_file(path: &Path) -> std::io::Result<BTreeMap<String, HistoryClassTotals>> {
    use std::io::BufRead;
    let file = std::fs::File::open(path)?;
    let reader = std::io::BufReader::with_capacity(256 * 1024, file);
    let mut totals: BTreeMap<String, HistoryClassTotals> = BTreeMap::new();
    for line in reader.split(b'\n') {
        let line = line?;
        if line.iter().all(u8::is_ascii_whitespace) {
            continue;
        }
        let class = classify_history_line(&line);
        let bucket = totals.entry(class.to_string()).or_default();
        bucket.lines += 1;
        // +1 for the newline this line consumed on disk.
        bucket.bytes += line.len() as u64 + 1;
        bucket.files = 1;
    }
    Ok(totals)
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
    let mut by_key: BTreeMap<(String, String), HistoryClassTotals> = BTreeMap::new();
    let mut reclaimable_by_driver: BTreeMap<String, u64> = BTreeMap::new();
    let mut session_file_bytes = 0_u64;
    let mut bytes_accounted = 0_u64;
    let mut unreadable_files = 0_u64;

    for entry in entries {
        let driver = entry.driver_label();
        session_file_bytes += entry.file_bytes;
        let Ok(totals) = inspect_session_file(&entry.session_path) else {
            unreadable_files += 1;
            continue;
        };
        for (class, class_totals) in totals {
            bytes_accounted += class_totals.bytes;
            if class_is_reclaimable(&class) {
                *reclaimable_by_driver.entry(driver.clone()).or_default() += class_totals.bytes;
            }
            let bucket = by_key.entry((driver.clone(), class)).or_default();
            bucket.files += class_totals.files;
            bucket.lines += class_totals.lines;
            bucket.bytes += class_totals.bytes;
        }
    }

    let buckets: Vec<HistoryBucket> = by_key
        .into_iter()
        .map(|((driver, event_class), totals)| HistoryBucket {
            reclaimable: class_is_reclaimable(&event_class),
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
               Compaction of the reclaimable bytes requires an explicit, \
               separately-authorized maintenance run.",
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
        let first = catalog
            .refresh_dir(&sessions, Some("proj-1"), &root, SessionScanBudget::DEFAULT)
            .await;
        assert_eq!(first.session_files, 8);
        assert_eq!(first.rebuilt, 8);
        assert!(first.bytes_inspected > 0);

        let second = catalog
            .refresh_dir(&sessions, Some("proj-1"), &root, SessionScanBudget::DEFAULT)
            .await;
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
        catalog
            .refresh_dir(&sessions, Some("proj-1"), &root, SessionScanBudget::DEFAULT)
            .await;

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

        let stats = catalog
            .refresh_dir(&sessions, Some("proj-1"), &root, SessionScanBudget::DEFAULT)
            .await;
        assert_eq!(stats.rebuilt, 1, "only the file that changed is re-read");
        assert_eq!(stats.cache_hits, 4);
        let entries = catalog.entries().await;
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
        catalog
            .refresh_dir(&sessions, Some("proj-1"), &root, SessionScanBudget::DEFAULT)
            .await;
        let bytes = catalog.snapshot_bytes(&root).await.unwrap();
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
        catalog
            .refresh_dir(&sessions, Some("proj-1"), &root, SessionScanBudget::DEFAULT)
            .await;
        let entry = catalog.entries().await.remove(0);
        assert!(entry.worktree_authority.verified_worktree().is_some());

        std::fs::remove_dir_all(&worktree).unwrap();
        let stats = catalog
            .refresh_dir(&sessions, Some("proj-1"), &root, SessionScanBudget::DEFAULT)
            .await;
        assert_eq!(stats.rebuilt, 0, "the session file did not change");
        assert_eq!(stats.authority_reverified, 1);
        let entry = catalog.entries().await.remove(0);
        assert!(
            entry.worktree_authority.is_tombstoned(),
            "a pruned worktree must become a stable tombstone: {:?}",
            entry.worktree_authority
        );
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
        good.refresh_dir(&sessions, Some("proj-1"), &root, SessionScanBudget::DEFAULT)
            .await;
        let bytes = good.snapshot_bytes(&root).await.unwrap();
        let snapshot_path = root.join(CATALOG_REL_PATH);
        std::fs::write(&snapshot_path, &bytes).unwrap();

        // Sound snapshot: loads.
        let loaded = RunCatalog::new();
        assert_eq!(
            loaded.load_snapshot(&root).await,
            SnapshotLoad::Loaded { entries: 1 }
        );

        // Truncated mid-object — what a kill during the write leaves.
        std::fs::write(&snapshot_path, &bytes[..bytes.len() / 2]).unwrap();
        let torn = RunCatalog::new();
        assert!(matches!(
            torn.load_snapshot(&root).await,
            SnapshotLoad::Corrupt { .. }
        ));
        assert_eq!(torn.len().await, 0);
        // And the rebuild produces the same verdict the sound snapshot held.
        torn.refresh_dir(&sessions, Some("proj-1"), &root, SessionScanBudget::DEFAULT)
            .await;
        let rebuilt = torn.entries().await;
        let original = good.entries().await;
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
        source
            .refresh_dir(&sessions, Some("proj-1"), &root, SessionScanBudget::DEFAULT)
            .await;
        let bytes = source.snapshot_bytes(&root).await.unwrap();
        let mut snapshot: Value = serde_json::from_slice(&bytes).unwrap();
        let path = root.join(CATALOG_REL_PATH);

        for foreign in [0_u64, u64::from(CATALOG_VERSION) + 1] {
            snapshot["catalog_version"] = json!(foreign);
            std::fs::write(&path, serde_json::to_vec(&snapshot).unwrap()).unwrap();
            let catalog = RunCatalog::new();
            assert_eq!(
                catalog.load_snapshot(&root).await,
                SnapshotLoad::VersionMismatch {
                    found: foreign as u32,
                    expected: CATALOG_VERSION,
                },
                "a catalog version this build does not know must be refused, not read"
            );
            assert_eq!(catalog.len().await, 0);
            // Still fully functional: the rebuild is the rollback.
            let stats = catalog
                .refresh_dir(&sessions, Some("proj-1"), &root, SessionScanBudget::DEFAULT)
                .await;
            assert_eq!(stats.rebuilt, 1);
            assert_eq!(catalog.len().await, 1);
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
        source
            .refresh_dir(
                &sessions,
                Some("proj-2"),
                &other,
                SessionScanBudget::DEFAULT,
            )
            .await;
        let bytes = source.snapshot_bytes(&other).await.unwrap();
        std::fs::write(root.join(CATALOG_REL_PATH), bytes).unwrap();

        let catalog = RunCatalog::new();
        assert_eq!(
            catalog.load_snapshot(&root).await,
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
        // Only rendered TUI is reclaimable; authority and unclassifiable lines
        // are not.
        assert!(class_is_reclaimable("rendered_tui"));
        for class in ["lifecycle", "semantic", "pane_activity", "unparsed", "note"] {
            assert!(!class_is_reclaimable(class), "{class}");
        }
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
        catalog
            .refresh_dir(&sessions, Some("proj-1"), &root, SessionScanBudget::DEFAULT)
            .await;
        let report = inspect_history(&catalog.entries().await);

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
