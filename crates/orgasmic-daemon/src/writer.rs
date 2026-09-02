// orgasmic:arch_BVH7M, arch_C87Z9, dec_WH9PD, dec_R75SW, task_C2PQ3
//! Single serialized writer for tx files, session JSONLs, and direct-edit
//! Org files.
//!
//! Runs as a dedicated tokio task. Every mutation goes through one mpsc
//! channel, so write ordering is total and append handles never race. Tx
//! and session writers wrap the primitives in `orgasmic-core` (which handle
//! the macOS append-mode read pitfall — see `AGENTS.md`). Direct
//! edits take an advisory `flock` per dec_005.
//!
//! Idempotency: every mutation carries an optional `request_id`. If the
//! same `request_id` is replayed (CLI retry, manager retry), the writer
//! returns the cached response instead of double-applying the change.
//! Closes AC #4 (stable request IDs for retriable mutations).

use std::collections::{HashMap, HashSet};
use std::fs::{File, OpenOptions};
use std::io::Write;
#[cfg(unix)]
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Instant;

use anyhow::{anyhow, bail, Context, Result};
use fs2::FileExt;
use futures::FutureExt;
use orgasmic_core::session::{RuntimeIdentity, SessionEventKind, SessionWriter};
use orgasmic_core::tx::{parse_tx_file, TxEntry, TxWriter};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use tokio::sync::{mpsc, oneshot, Mutex};
use tracing::{error, warn};
use uuid::Uuid;

use crate::events::{EventBus, EventPayload, Topic};

pub(crate) const DAEMON_OWNED_SURFACES: [&str; 4] = ["machines", "tx", "tmp", "views"];

#[derive(Debug)]
pub enum CommentMutationActor {
    Member(String),
    Admin(String),
}

impl CommentMutationActor {
    fn name(&self) -> &str {
        match self {
            Self::Member(name) | Self::Admin(name) => name,
        }
    }
}

/// The multi-tx append reached the ledger, but the writer could not confirm
/// that the retained descriptor was synced. Callers must distinguish this
/// committed outcome from an ordinary failed transaction without parsing its
/// human-readable text.
#[derive(Debug)]
pub struct CommittedSyncUncertainError {
    retry: bool,
    source: String,
}

impl CommittedSyncUncertainError {
    pub(crate) fn initial(source: impl std::fmt::Display) -> Self {
        Self {
            retry: false,
            source: source.to_string(),
        }
    }

    fn retry(source: impl std::fmt::Display) -> Self {
        Self {
            retry: true,
            source: source.to_string(),
        }
    }
}

impl std::fmt::Display for CommittedSyncUncertainError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        if self.retry {
            write!(
                f,
                "multi transaction committed but durability remains uncertain; retained ledger \
                 descriptor could not be synced: {}",
                self.source
            )
        } else {
            write!(
                f,
                "multi transaction committed but durability is uncertain; retry the same \
                 request_id to sync the retained ledger descriptor without appending again: {}",
                self.source
            )
        }
    }
}

impl std::error::Error for CommittedSyncUncertainError {}

/// Test-only counters and injectors for writer durability tests.
#[doc(hidden)]
pub mod test_hooks {
    use super::*;

    static SYNC_COUNT: AtomicU64 = AtomicU64::new(0);
    static SYNC_ATTEMPT_COUNT: AtomicU64 = AtomicU64::new(0);
    static FLOCK_COUNT: AtomicU64 = AtomicU64::new(0);
    static FAIL_NEXT_SYNC: AtomicUsize = AtomicUsize::new(0);
    static FAIL_NEXT_MULTI_BEFORE_COMMIT: AtomicUsize = AtomicUsize::new(0);

    #[cfg(test)]
    #[derive(Debug)]
    struct RenameBeforeSync {
        source: PathBuf,
        destination: PathBuf,
    }

    #[cfg(test)]
    static RENAME_BEFORE_SYNC: std::sync::Mutex<Vec<RenameBeforeSync>> =
        std::sync::Mutex::new(Vec::new());

    /// Per-`WriterHandle` failure observation for lifecycle persistence tests.
    /// It deliberately has no process-global registry: two writers may append
    /// the same run/path concurrently, and only the handle that armed this
    /// seam may consume it.
    #[cfg(test)]
    #[derive(Debug)]
    pub(crate) struct SessionAppendFailure {
        attempts: AtomicU64,
    }

    #[cfg(test)]
    impl SessionAppendFailure {
        pub(crate) fn new() -> Self {
            Self {
                attempts: AtomicU64::new(0),
            }
        }

        pub(crate) fn attempt_count(&self) -> u64 {
            self.attempts.load(Ordering::SeqCst)
        }

        pub(crate) fn fail(&self) -> Result<()> {
            self.attempts.fetch_add(1, Ordering::SeqCst);
            bail!("injected session lifecycle append failure");
        }
    }

    pub fn reset() {
        SYNC_COUNT.store(0, Ordering::SeqCst);
        SYNC_ATTEMPT_COUNT.store(0, Ordering::SeqCst);
        FLOCK_COUNT.store(0, Ordering::SeqCst);
        FAIL_NEXT_SYNC.store(0, Ordering::SeqCst);
        FAIL_NEXT_MULTI_BEFORE_COMMIT.store(0, Ordering::SeqCst);
    }

    pub fn sync_count() -> u64 {
        SYNC_COUNT.load(Ordering::SeqCst)
    }

    pub fn sync_attempt_count() -> u64 {
        SYNC_ATTEMPT_COUNT.load(Ordering::SeqCst)
    }

    pub fn flock_count() -> u64 {
        FLOCK_COUNT.load(Ordering::SeqCst)
    }

    pub(crate) fn record_flock() {
        FLOCK_COUNT.fetch_add(1, Ordering::SeqCst);
    }

    pub fn fail_next_sync(count: usize) {
        FAIL_NEXT_SYNC.store(count, Ordering::SeqCst);
    }

    pub fn fail_next_multi_before_commit(count: usize) {
        FAIL_NEXT_MULTI_BEFORE_COMMIT.store(count, Ordering::SeqCst);
    }

    pub(crate) fn before_multi_commit() -> Result<()> {
        if FAIL_NEXT_MULTI_BEFORE_COMMIT
            .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |remaining| {
                remaining.checked_sub(1)
            })
            .is_ok()
        {
            bail!("injected failure before multi transaction commit");
        }
        Ok(())
    }

    #[cfg(test)]
    pub(crate) fn rename_tx_path_before_next_sync(source: &Path, destination: &Path) {
        let mut armed = RENAME_BEFORE_SYNC
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        assert!(
            !armed.iter().any(|rename| rename.source == source),
            "a tx path rename before sync is already armed for {}",
            source.display()
        );
        armed.push(RenameBeforeSync {
            source: source.to_path_buf(),
            destination: destination.to_path_buf(),
        });
    }

    pub(crate) fn before_sync(path: &Path) -> Result<()> {
        #[cfg(not(test))]
        let _ = path;
        SYNC_ATTEMPT_COUNT.fetch_add(1, Ordering::SeqCst);
        if FAIL_NEXT_SYNC
            .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |remaining| {
                remaining.checked_sub(1)
            })
            .is_ok()
        {
            bail!("injected tx append fsync failure");
        }
        #[cfg(test)]
        {
            let rename = {
                let mut armed = RENAME_BEFORE_SYNC
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner());
                armed
                    .iter()
                    .position(|rename| rename.source == path)
                    .map(|position| armed.remove(position))
            };
            if let Some(rename) = rename {
                std::fs::rename(&rename.source, &rename.destination).with_context(|| {
                    format!(
                        "rename tx path before sync {} -> {}",
                        rename.source.display(),
                        rename.destination.display()
                    )
                })?;
            }
        }
        Ok(())
    }

    pub(crate) fn after_sync() {
        SYNC_COUNT.fetch_add(1, Ordering::SeqCst);
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TxAppend {
    /// Target tx file. The writer keeps one open handle per file.
    pub tx_path: PathBuf,
    pub entry: TxEntry,
    pub project_id: Option<String>,
    #[serde(default)]
    pub tx_id_policy: TxIdPolicy,
    #[serde(default)]
    pub request_id: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TxAppendResult {
    pub tx_id: String,
    pub tx_path: PathBuf,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub enum TxIdPolicy {
    #[default]
    Preserve,
    ProjectSequence {
        project_id: String,
        date: String,
    },
}

#[derive(Debug)]
pub struct SessionAppend {
    pub run_id: String,
    pub session_path: PathBuf,
    pub identity: RuntimeIdentity,
    pub authority: Option<crate::recovery_claim::SessionFile>,
    pub kind: SessionEventKind,
    pub event: Value,
}

#[derive(Debug, Clone)]
pub struct SessionAppendResult {
    pub seq: u64,
}

#[derive(Debug, Clone)]
pub struct FileRewrite {
    pub path: PathBuf,
    pub new_contents: Vec<u8>,
}

pub type FileMutateTransform = Box<dyn FnOnce(&str) -> Result<Vec<u8>> + Send>;

/// Atomic read-modify-write request for a single file.
///
/// The writer task opens the path with an exclusive flock, reads the
/// current contents (empty string if the file did not exist), passes them
/// to `transform`, and atomically renames the result back. The lock is
/// held across the entire round trip so two concurrent mutates against
/// the same path serialize through the writer.
pub struct FileMutate {
    pub path: PathBuf,
    pub transform: FileMutateTransform,
}

impl std::fmt::Debug for FileMutate {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("FileMutate")
            .field("path", &self.path)
            .finish()
    }
}

enum WriterCommand {
    Tx {
        req: TxAppend,
        reply: oneshot::Sender<Result<TxAppendResult>>,
    },
    Session {
        req: SessionAppend,
        reply: oneshot::Sender<Result<SessionAppendResult>>,
        #[cfg(test)]
        injected_failure: Option<Arc<test_hooks::SessionAppendFailure>>,
    },
    Rewrite {
        req: FileRewrite,
        reply: oneshot::Sender<Result<()>>,
    },
    Mutate {
        req: FileMutate,
        reply: oneshot::Sender<Result<()>>,
    },
    Transaction {
        req: TransactionRequest,
        reply: oneshot::Sender<Result<TxAppendResult>>,
    },
    TransactionMulti {
        req: TransactionMultiRequest,
        reply: oneshot::Sender<Result<Vec<TxAppendResult>>>,
    },
    TransactionMutate {
        req: TransactionMutateRequest,
        reply: oneshot::Sender<Result<TxAppendResult>>,
    },
    /// Take an exclusive hold on a set of session paths
    /// (orgasmic:TASK-FZB6T.3 finding 1). Drops each path's cached append
    /// handle and marks it held; every later append for a held path is DEFERRED
    /// before it opens anything.
    LeaseSessions {
        paths: Vec<PathBuf>,
        reply: oneshot::Sender<Result<()>>,
    },
    /// Release a hold and run the appends that queued behind it, in order.
    ReleaseSessions {
        paths: Vec<PathBuf>,
        reply: oneshot::Sender<()>,
    },
    Barrier {
        run: Box<dyn FnOnce() + Send>,
        reply: oneshot::Sender<()>,
    },
    Shutdown {
        reply: oneshot::Sender<()>,
    },
}

#[derive(Debug)]
struct TransactionRequest {
    rewrites: Vec<FileRewrite>,
    tx: TxAppend,
    request_id: String,
    mutation: Option<MutationIdentity>,
    mutation_id: Option<String>,
}

#[derive(Debug)]
struct TransactionMultiRequest {
    rewrites: Vec<FileRewrite>,
    txs: Vec<TxAppend>,
    request_id: String,
    mutation: MutationIdentity,
}

#[derive(Debug)]
struct TransactionMutateRequest {
    file: FileMutate,
    tx: TxAppend,
    request_id: String,
    mutation: MutationIdentity,
    mutation_id: Option<String>,
}

/// Semantic scope retained by the writer for a retriable mutation. Keeping it
/// with the cached result prevents replay recovery from consulting a lagging
/// index snapshot.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MutationIdentity {
    pub operation: String,
    pub project_id: String,
    pub payload: String,
}

impl MutationIdentity {
    pub fn new(
        operation: impl Into<String>,
        project_id: impl Into<String>,
        payload: impl Into<String>,
    ) -> Self {
        Self {
            operation: operation.into(),
            project_id: project_id.into(),
            payload: payload.into(),
        }
    }
}

#[derive(Debug, Clone)]
pub struct CachedMutation {
    pub tx_id: String,
    pub mutation_id: String,
}

/// orgasmic:TASK-Q07Y5 — the writer half of the shutdown budget.
///
/// `WriterHandle::shutdown` queues behind every command the writer has already
/// accepted, and the command being processed can be blocked in `write`/`fsync`.
/// Unbounded, it makes the whole SIGTERM path unbounded, so no service-manager
/// kill timeout can be *proven* larger than it — which is what TASK-WGXKD.2
/// finding 1 objected to. 10s is one worst-case blocked fsync (a stalled or
/// remote-backed volume) plus the queue behind it; past that the work is not
/// arriving in time to be worth waiting for, and the loss is recorded instead
/// of the daemon being SIGKILLed mid-write with nothing written down.
pub const WRITER_SHUTDOWN_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(10);

/// The write the writer task is executing (or was executing when a shutdown
/// budget expired). Carries the run id when the command has one, because a
/// shutdown-loss report an operator can act on has to name the run, not a count
/// (TASK-Q07Y5).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PendingWrite {
    /// `tx`, `session`, `transaction`, `transaction_mutate`, `mutate`,
    /// `rewrite`, or `shutdown`.
    pub kind: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub run_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tx_type: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub path: Option<PathBuf>,
}

#[derive(Debug, Default)]
struct WriterMetrics {
    in_flight_started: std::sync::Mutex<Option<Instant>>,
    /// Session files the writer currently holds open — one per run that has
    /// appended and not yet released. A run's handle is dropped on its
    /// `release` lifecycle append; before that fix this grew by one fd per
    /// dispatch for the daemon's whole life and hit the 256 soft limit.
    open_session_handles: AtomicUsize,
    completed_total: AtomicU64,
    failed_total: AtomicU64,
    last_duration_ms: AtomicU64,
    max_duration_ms: AtomicU64,
}

/// Boot-local writer diagnostics exposed through `/daemon/status`.
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct WriterStatus {
    pub liveness: bool,
    pub queue_depth: usize,
    /// Session files held open right now (see `WriterMetrics`).
    pub open_session_handles: usize,
    pub in_flight_operation: Option<PendingWrite>,
    pub in_flight_age_ms: Option<u64>,
    pub completed_total: u64,
    pub failed_total: u64,
    pub last_duration_ms: u64,
    pub max_duration_ms: u64,
}

/// What [`WriterHandle::shutdown_within`] observed. `TimedOut` is the outcome
/// callers must make durable: the writer still owns unwritten work and the
/// process is about to exit anyway.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "outcome", rename_all = "snake_case")]
pub enum WriterShutdownOutcome {
    /// The writer acknowledged the shutdown: everything it had accepted is
    /// written and its handles are closed.
    Clean,
    /// The writer task was already gone, so there is nothing left to flush.
    AlreadyGone,
    /// The budget expired first. `in_flight` is the write that did not finish;
    /// `queued` is the number still waiting behind it (a lower bound — commands
    /// blocked on a full channel are not counted).
    TimedOut {
        queued: usize,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        in_flight: Option<PendingWrite>,
    },
}

impl WriterShutdownOutcome {
    /// Whether the writer stopped with nothing left unwritten.
    pub fn is_clean(&self) -> bool {
        matches!(self, Self::Clean | Self::AlreadyGone)
    }
}

#[derive(Debug, Clone)]
pub struct WriterHandle {
    tx: mpsc::Sender<WriterCommand>,
    idempotency: Arc<Mutex<HashMap<String, CachedResponse>>>,
    /// Head-of-line write, published by the writer task so a shutdown that
    /// gives up can name what it gave up on.
    in_flight: Arc<std::sync::Mutex<Option<PendingWrite>>>,
    metrics: Arc<WriterMetrics>,
    index: Option<crate::index::Index>,
    machine_id: Option<String>,
    /// Appends currently queued behind a session lease
    /// (orgasmic:TASK-FZB6T.3 finding 1). Read only by the regression that
    /// schedules an append at the pre-rename instant; the writer publishes it
    /// unconditionally so the two builds cannot diverge.
    #[cfg_attr(not(test), allow(dead_code))]
    deferred_appends: Arc<std::sync::atomic::AtomicUsize>,
    /// Apply-own-write projection failures keyed by their owning tx/request.
    /// The write is already durable, so `publish_paths` records the failure
    /// here rather than failing the call: reporting "failed to record
    /// transaction" for a transaction that committed is a lie the caller acts
    /// on. `refresh_after_tx` takes it and answers the committed-503 contract.
    apply_failures: Arc<std::sync::Mutex<HashMap<String, String>>>,
    /// Paths this writer committed but could not apply to the projection, in
    /// write order. The writes they belong to are already durable,
    /// so they are queued rather than lost. The queue outlives the error
    /// itself because the retry that repairs the projection usually hits the
    /// API idempotency cache and never re-publishes: without it that retry
    /// would answer 200 over a still-stale view.
    ///
    /// It holds paths rather than a dirty bit, and one queue serves the whole
    /// daemon, so whoever runs the repair repairs every project that is behind.
    /// As a bare flag it was global but the repair was project-scoped, so a
    /// write to one project could clear the staleness recorded for another and
    /// then answer 200 over a view that was still stale.
    unapplied: Arc<std::sync::Mutex<Vec<PathBuf>>>,
    #[cfg(test)]
    transaction_gate: Arc<Mutex<Option<Arc<TestTransactionGate>>>>,
    /// Test-only, handle-local lifecycle fault. This state is intentionally
    /// owned by the handle (and its clones), not by the writer process or a
    /// global key: dropping all clones drops an unconsumed arm, and another
    /// WriterHandle with an identical run/path cannot see it.
    #[cfg(test)]
    session_append_failure: Arc<std::sync::Mutex<Option<ArmedSessionAppendFailure>>>,
}

#[cfg(test)]
#[derive(Debug)]
struct ArmedSessionAppendFailure {
    run_id: String,
    session_path: PathBuf,
    failure: Arc<test_hooks::SessionAppendFailure>,
}

/// An exclusive hold on a set of session paths, held for the whole of a
/// maintenance transaction and released when it completes
/// (orgasmic:TASK-FZB6T.3 finding 1).
///
/// Releasing runs the appends that queued behind it. Dropping without
/// [`Self::release`] still releases — a lease that outlived its transaction
/// would block a run's lifecycle appends forever — but the drop path cannot
/// wait for the writer to acknowledge, so a caller that has an `await` to spend
/// should spend it.
pub struct SessionLease {
    tx: mpsc::Sender<WriterCommand>,
    paths: Vec<PathBuf>,
    released: bool,
}

impl SessionLease {
    /// The paths this lease holds, which is exactly the set a transaction may
    /// prove it excluded.
    pub fn paths(&self) -> &[PathBuf] {
        &self.paths
    }

    /// Release the hold and wait for the writer to drain the appends that
    /// queued behind it.
    pub async fn release(mut self) {
        self.released = true;
        let paths = std::mem::take(&mut self.paths);
        let (reply, rx) = oneshot::channel();
        if self
            .tx
            .send(WriterCommand::ReleaseSessions { paths, reply })
            .await
            .is_ok()
        {
            let _ = rx.await;
        }
    }
}

/// Why a detached leased transaction produced no outcome
/// (orgasmic:TASK-FZB6T.4 finding 1). Kept apart from the transaction's own
/// error type: "the lease could not be taken" and "the transaction refused" are
/// different answers and the caller reports them differently.
#[derive(Debug)]
pub enum LeasedTransactionError {
    /// The session writer would not grant the lease, so nothing ran and nothing
    /// was touched.
    Lease(String),
    /// The detached owner or its blocking task did not return an outcome — a
    /// panic or a runtime shutdown, never a caller-side cancellation.
    Transaction(String),
}

impl std::fmt::Display for LeasedTransactionError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Lease(error) => write!(f, "{error}"),
            Self::Transaction(error) => write!(f, "{error}"),
        }
    }
}

impl Drop for SessionLease {
    fn drop(&mut self) {
        if self.released || self.paths.is_empty() {
            return;
        }
        let paths = std::mem::take(&mut self.paths);
        let (reply, _rx) = oneshot::channel();
        let command = WriterCommand::ReleaseSessions { paths, reply };
        match self.tx.try_send(command) {
            Ok(()) => {}
            Err(mpsc::error::TrySendError::Full(command)) => {
                // The channel is backed up. Handing the release to the runtime
                // is the only way left to guarantee the lease does not outlive
                // this value.
                if let Ok(handle) = tokio::runtime::Handle::try_current() {
                    let tx = self.tx.clone();
                    handle.spawn(async move {
                        let _ = tx.send(command).await;
                    });
                }
            }
            Err(mpsc::error::TrySendError::Closed(_)) => {}
        }
    }
}

#[cfg(test)]
#[derive(Debug)]
pub(crate) struct TestTransactionGate {
    entered: tokio::sync::Notify,
    release: tokio::sync::Notify,
}

#[cfg(test)]
impl TestTransactionGate {
    pub(crate) async fn wait_until_entered(&self) {
        self.entered.notified().await;
    }

    pub(crate) fn release(&self) {
        self.release.notify_one();
    }
}

#[derive(Debug, Clone)]
enum CachedResponse {
    Tx {
        result: TxAppendResult,
        mutation: Option<MutationIdentity>,
        mutation_id: Option<String>,
    },
    Multi {
        results: Vec<TxAppendResult>,
        mutation: MutationIdentity,
        durability: MultiDurability,
    },
    Rewrite,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum MultiDurability {
    Durable,
    SyncUncertain,
}

fn cached_mutation_from_map(
    cache: &HashMap<String, CachedResponse>,
    request_id: &str,
    expected: &MutationIdentity,
) -> Result<Option<CachedMutation>> {
    let Some(cached) = cache.get(request_id) else {
        return Ok(None);
    };
    let CachedResponse::Tx {
        result,
        mutation,
        mutation_id,
    } = cached
    else {
        bail!("request_id `{request_id}` was already used by a different mutation type");
    };
    if mutation.as_ref() != Some(expected) {
        bail!(
            "request_id `{request_id}` was reused with a different operation, project, or payload"
        );
    }
    let mutation_id = mutation_id
        .clone()
        .ok_or_else(|| anyhow!("cached mutation lacks its recorded identity"))?;
    Ok(Some(CachedMutation {
        tx_id: result.tx_id.clone(),
        mutation_id,
    }))
}

fn transaction_identity(tx: &TxAppend, rewrites: &[FileRewrite]) -> MutationIdentity {
    let payload = rewrites
        .iter()
        .map(|rewrite| {
            format!(
                "{}:{:?}",
                rewrite.path.display(),
                rewrite.new_contents.as_slice()
            )
        })
        .collect::<Vec<_>>()
        .join("|");
    MutationIdentity::new(
        tx.entry.ty.clone(),
        tx.project_id
            .clone()
            .unwrap_or_else(|| "<none>".to_string()),
        payload,
    )
}

fn multi_transaction_identity(txs: &[TxAppend], rewrites: &[FileRewrite]) -> MutationIdentity {
    let payload = serde_json::json!({
        "rewrites": rewrites
            .iter()
            .map(|rewrite| serde_json::json!({
                "path": rewrite.path.display().to_string(),
                "contents": rewrite.new_contents.as_slice(),
            }))
            .collect::<Vec<_>>(),
        // Prepared tx ids and timestamps are server-generated, so they change
        // when an HTTP retry rebuilds the same semantic request. Everything
        // caller-controlled remains part of the collision identity.
        //
        // EVENT_ID (TASK-MSYN4) joins that server-generated class: it is minted
        // per attempt, so leaving it in makes every lost-response retry look
        // like a different mutation and fail closed. Duplicate delivery is
        // still caught downstream by event-id dedup at ingest.
        "txs": txs
            .iter()
            .map(|tx| serde_json::json!({
                "tx_path": tx.tx_path.display().to_string(),
                "project_id": tx.project_id,
                "tx_id_policy": tx.tx_id_policy,
                "request_id": tx.request_id,
                "type": tx.entry.ty,
                "actor": tx.entry.actor,
                "machine": tx.entry.machine,
                "project": tx.entry.project,
                "task": tx.entry.task,
                "target": tx.entry.target,
                "reason": tx.entry.reason,
                "extra": tx.entry.extra
                    .iter()
                    .filter(|(key, _)| key != "EVENT_ID")
                    .collect::<Vec<_>>(),
            }))
            .collect::<Vec<_>>(),
    })
    .to_string();
    MutationIdentity::new(
        "transaction_multi",
        txs.iter()
            .filter_map(|tx| tx.project_id.as_deref())
            .collect::<Vec<_>>()
            .join("+"),
        payload,
    )
}

fn multi_cache_key(request_id: &str) -> String {
    format!("transaction-multi:{request_id}")
}

fn cached_multi_from_map(
    cache: &HashMap<String, CachedResponse>,
    cache_key: &str,
    request_id: &str,
    expected: &MutationIdentity,
) -> Result<Option<(Vec<TxAppendResult>, MultiDurability)>> {
    let Some(cached) = cache.get(cache_key) else {
        return Ok(None);
    };
    let CachedResponse::Multi {
        results,
        mutation,
        durability,
    } = cached
    else {
        bail!("request_id `{request_id}` was already used by a different mutation type");
    };
    if mutation != expected {
        bail!(
            "request_id `{request_id}` was reused with different multi-transaction rewrites or txs"
        );
    }
    Ok(Some((results.clone(), *durability)))
}

fn cached_transaction_from_map(
    cache: &HashMap<String, CachedResponse>,
    request_id: &str,
    expected: &MutationIdentity,
) -> Result<Option<TxAppendResult>> {
    let Some(cached) = cache.get(request_id) else {
        return Ok(None);
    };
    let CachedResponse::Tx {
        result, mutation, ..
    } = cached
    else {
        bail!("request_id `{request_id}` was already used by a different mutation type");
    };
    if mutation.as_ref() != Some(expected) {
        bail!(
            "request_id `{request_id}` was reused with a different operation, project, or payload"
        );
    }
    Ok(Some(result.clone()))
}

impl WriterHandle {
    pub(crate) fn applies_own_writes(&self) -> bool {
        self.index.is_some()
    }

    /// Run blocking maintenance after all accepted writes and before later ones.
    pub async fn run_barrier<T, F>(&self, run: F) -> Result<T>
    where
        T: Send + 'static,
        F: FnOnce() -> T + Send + 'static,
    {
        let (result_tx, result_rx) = oneshot::channel();
        let (reply, rx) = oneshot::channel();
        self.tx
            .send(WriterCommand::Barrier {
                run: Box::new(move || {
                    let _ = result_tx.send(run());
                }),
                reply,
            })
            .await
            .map_err(|_| anyhow!("writer task is gone"))?;
        rx.await.map_err(|_| anyhow!("writer reply dropped"))?;
        result_rx
            .await
            .map_err(|_| anyhow!("writer barrier result dropped"))
    }

    /// Apply the paths this write just produced to the live projection.
    ///
    /// A failure here is NOT a failed write — the bytes are already durable.
    /// Record it and return Ok so the caller reports the committed-but-
    /// unprojected 503 (with its tx id) instead of a generic "write failed".
    async fn publish_paths(
        &self,
        owner: Option<&str>,
        paths: impl IntoIterator<Item = PathBuf>,
    ) -> Result<()> {
        let Some(index) = self.index.as_ref() else {
            return Ok(());
        };
        let pending: Vec<PathBuf> = paths.into_iter().collect();
        for (i, path) in pending.iter().enumerate() {
            if let Err(error) = index.apply_written_path(path).await {
                tracing::error!(path = %path.display(), error = %error, "write committed but index apply failed");
                if let Some(owner) = owner {
                    self.apply_failures
                        .lock()
                        .unwrap()
                        .insert(owner.to_string(), error);
                }
                // Stop at the first failure. Applying the remaining paths would
                // leave the projection torn — half this write visible — which is
                // harder to reason about than uniformly stale. This path and
                // the ones after it go on the repair queue instead.
                self.unapplied
                    .lock()
                    .unwrap()
                    .extend(pending[i..].iter().cloned());
                return Ok(());
            }
        }
        Ok(())
    }

    /// Take only this write's projection failure. A foreign failure remains
    /// until its owner takes it or a later request repairs the queued paths.
    pub(crate) fn take_apply_failure(&self, owner: &str) -> Option<String> {
        self.apply_failures.lock().unwrap().remove(owner)
    }

    #[cfg(test)]
    pub(crate) fn apply_failure_count(&self) -> usize {
        self.apply_failures.lock().unwrap().len()
    }

    /// Re-apply everything committed but not yet projected, and report whether
    /// the projection is now whole.
    ///
    /// Any caller may run this and every caller should: the queue is
    /// daemon-wide, so whoever gets here repairs every project rather than only
    /// its own. On failure the offending path and those behind it go back on
    /// the front of the queue, keeping write order, and the error becomes the
    /// caller's committed-503.
    pub(crate) async fn repair_projection(&self) -> std::result::Result<(), String> {
        let Some(index) = self.index.as_ref() else {
            return Ok(());
        };
        let pending = std::mem::take(&mut *self.unapplied.lock().unwrap());
        for (i, path) in pending.iter().enumerate() {
            if let Err(error) = index.apply_written_path(path).await {
                self.unapplied
                    .lock()
                    .unwrap()
                    .splice(0..0, pending[i..].iter().cloned());
                return Err(error);
            }
        }
        self.apply_failures.lock().unwrap().clear();
        Ok(())
    }

    fn guard_node_paths<'a>(&self, paths: impl IntoIterator<Item = &'a Path>) -> Result<()> {
        let Some(machine_id) = self.machine_id.as_deref() else {
            return Ok(());
        };
        for path in paths {
            guard_node_write(path, machine_id)?;
        }
        Ok(())
    }

    /// Append a tx entry through the daemon writer. Re-using `request_id`
    /// is safe — the second call returns the same result.
    pub async fn append_tx(
        &self,
        req: TxAppend,
        request_id: Option<String>,
    ) -> Result<TxAppendResult> {
        let written_path = req.tx_path.clone();
        self.guard_node_paths([written_path.as_path()])?;
        let request_id = request_id
            .or_else(|| req.request_id.clone())
            .unwrap_or_else(|| Uuid::new_v4().to_string());
        let cached = {
            let cache = self.idempotency.lock().await;
            match cache.get(&request_id) {
                Some(CachedResponse::Tx { result, .. }) => Some(result.clone()),
                _ => None,
            }
        };
        if let Some(result) = cached {
            self.publish_paths(Some(&result.tx_id), [written_path])
                .await?;
            return Ok(result);
        }
        let (reply, rx) = oneshot::channel();
        self.tx
            .send(WriterCommand::Tx { req, reply })
            .await
            .map_err(|_| anyhow!("writer task is gone"))?;
        let res = rx.await.map_err(|_| anyhow!("writer reply dropped"))??;
        self.idempotency.lock().await.insert(
            request_id,
            CachedResponse::Tx {
                result: res.clone(),
                mutation: None,
                mutation_id: None,
            },
        );
        self.publish_paths(Some(&res.tx_id), [written_path]).await?;
        Ok(res)
    }

    /// Peek the in-memory idempotency cache for a prior transaction result.
    /// Used by graph create to return the original node id on a lost-response
    /// retry before uniqueness guards treat the survivor as a distinct duplicate.
    pub async fn cached_tx_id(&self, request_id: &str) -> Option<String> {
        let cache = self.idempotency.lock().await;
        match cache.get(request_id) {
            Some(CachedResponse::Tx { result, .. }) => Some(result.tx_id.clone()),
            _ => None,
        }
    }

    /// Recover a mutation only when its exact semantic scope matches. A
    /// request-id collision across operations, projects, or payloads fails
    /// closed instead of returning an unrelated prior result.
    pub async fn cached_mutation(
        &self,
        request_id: &str,
        expected: &MutationIdentity,
    ) -> Result<Option<CachedMutation>> {
        let cache = self.idempotency.lock().await;
        cached_mutation_from_map(&cache, request_id, expected)
    }

    pub async fn transaction(&self, rewrites: Vec<FileRewrite>, tx: TxAppend) -> Result<String> {
        self.guard_node_paths(
            rewrites
                .iter()
                .map(|rewrite| rewrite.path.as_path())
                .chain(std::iter::once(tx.tx_path.as_path())),
        )?;
        let written_paths = rewrites
            .iter()
            .map(|rewrite| rewrite.path.clone())
            .chain(std::iter::once(tx.tx_path.clone()))
            .collect::<Vec<_>>();
        #[cfg(test)]
        if let Some(gate) = self.transaction_gate.lock().await.take() {
            gate.entered.notify_one();
            gate.release.notified().await;
        }
        let request_id = tx
            .request_id
            .clone()
            .unwrap_or_else(|| Uuid::new_v4().to_string());
        let mutation = transaction_identity(&tx, &rewrites);
        let cached = {
            let cache = self.idempotency.lock().await;
            cached_transaction_from_map(&cache, &request_id, &mutation)?
        };
        if let Some(result) = cached {
            self.publish_paths(Some(&result.tx_id), written_paths)
                .await?;
            return Ok(result.tx_id);
        }
        let (reply, rx) = oneshot::channel();
        self.tx
            .send(WriterCommand::Transaction {
                req: TransactionRequest {
                    rewrites,
                    tx,
                    request_id: request_id.clone(),
                    mutation: Some(mutation),
                    mutation_id: None,
                },
                reply,
            })
            .await
            .map_err(|_| anyhow!("writer task is gone"))?;
        let res = rx.await.map_err(|_| anyhow!("writer reply dropped"))??;
        self.publish_paths(Some(&res.tx_id), written_paths).await?;
        Ok(res.tx_id)
    }

    // orgasmic:task_P9T4N
    /// Rewrite files and append an ordered group of tx entries as one writer
    /// command. The group must target one ledger, so it is emitted by one
    /// underlying append and acknowledged by one sync.
    pub async fn transaction_multi(
        &self,
        rewrites: Vec<FileRewrite>,
        txs: Vec<TxAppend>,
    ) -> Result<Vec<TxAppendResult>> {
        self.guard_node_paths(
            rewrites
                .iter()
                .map(|rewrite| rewrite.path.as_path())
                .chain(txs.iter().map(|tx| tx.tx_path.as_path())),
        )?;
        let written_paths = rewrites
            .iter()
            .map(|rewrite| rewrite.path.clone())
            .chain(txs.iter().map(|tx| tx.tx_path.clone()))
            .collect::<Vec<_>>();
        let first = txs
            .first()
            .ok_or_else(|| anyhow!("multi transaction requires at least one tx"))?;
        let request_id = first
            .request_id
            .clone()
            .unwrap_or_else(|| Uuid::new_v4().to_string());
        let mutation = multi_transaction_identity(&txs, &rewrites);
        let cache_key = multi_cache_key(&request_id);
        let cached = {
            let cache = self.idempotency.lock().await;
            cached_multi_from_map(&cache, &cache_key, &request_id, &mutation)?
        };
        if let Some((results, MultiDurability::Durable)) = cached {
            let owner = results
                .first()
                .ok_or_else(|| anyhow!("writer returned no transactions"))?;
            self.publish_paths(Some(&owner.tx_id), written_paths)
                .await?;
            return Ok(results);
        }
        let (reply, rx) = oneshot::channel();
        self.tx
            .send(WriterCommand::TransactionMulti {
                req: TransactionMultiRequest {
                    rewrites,
                    txs,
                    request_id,
                    mutation,
                },
                reply,
            })
            .await
            .map_err(|_| anyhow!("writer task is gone"))?;
        let results = rx.await.map_err(|_| anyhow!("writer reply dropped"))??;
        let owner = results
            .first()
            .ok_or_else(|| anyhow!("writer returned no transactions"))?;
        self.publish_paths(Some(&owner.tx_id), written_paths)
            .await?;
        Ok(results)
    }

    /// Apply a single-file read-modify-write and append its tx as one writer
    /// command. The transform runs only after the command reaches the head of
    /// the serialized writer queue, so it always sees the result of every
    /// earlier daemon mutation instead of replacing it with caller-stale bytes.
    pub async fn transaction_mutate_file(
        &self,
        file: FileMutate,
        tx: TxAppend,
        mutation: MutationIdentity,
    ) -> Result<String> {
        self.guard_node_paths([file.path.as_path(), tx.tx_path.as_path()])?;
        let written_paths = [file.path.clone(), tx.tx_path.clone()];
        let request_id = tx
            .request_id
            .clone()
            .unwrap_or_else(|| Uuid::new_v4().to_string());
        let cached = {
            let cache = self.idempotency.lock().await;
            cached_transaction_from_map(&cache, &request_id, &mutation)?
        };
        if let Some(result) = cached {
            self.publish_paths(Some(&result.tx_id), written_paths)
                .await?;
            return Ok(result.tx_id);
        }
        let (reply, rx) = oneshot::channel();
        self.tx
            .send(WriterCommand::TransactionMutate {
                req: TransactionMutateRequest {
                    file,
                    tx,
                    request_id,
                    mutation,
                    mutation_id: None,
                },
                reply,
            })
            .await
            .map_err(|_| anyhow!("writer task is gone"))?;
        let res = rx.await.map_err(|_| anyhow!("writer reply dropped"))??;
        self.publish_paths(Some(&res.tx_id), written_paths).await?;
        Ok(res.tx_id)
    }

    /// [`Self::transaction_mutate_file`] with a caller-visible mutation id
    /// retained in the idempotency cache (for creates whose response includes
    /// both the created id and tx id).
    pub async fn transaction_mutate_file_mutation(
        &self,
        file: FileMutate,
        tx: TxAppend,
        mutation: MutationIdentity,
        mutation_id: String,
    ) -> Result<CachedMutation> {
        self.guard_node_paths([file.path.as_path(), tx.tx_path.as_path()])?;
        let written_paths = [file.path.clone(), tx.tx_path.clone()];
        let request_id = tx
            .request_id
            .clone()
            .unwrap_or_else(|| Uuid::new_v4().to_string());
        if let Some(cached) = self.cached_mutation(&request_id, &mutation).await? {
            self.publish_paths(Some(&cached.tx_id), written_paths)
                .await?;
            return Ok(cached);
        }
        let (reply, rx) = oneshot::channel();
        self.tx
            .send(WriterCommand::TransactionMutate {
                req: TransactionMutateRequest {
                    file,
                    tx,
                    request_id: request_id.clone(),
                    mutation: mutation.clone(),
                    mutation_id: Some(mutation_id),
                },
                reply,
            })
            .await
            .map_err(|_| anyhow!("writer task is gone"))?;
        let result = rx.await.map_err(|_| anyhow!("writer reply dropped"))??;
        self.publish_paths(Some(&result.tx_id), written_paths)
            .await?;
        self.cached_mutation(&request_id, &mutation)
            .await?
            .ok_or_else(|| anyhow!("writer did not retain mutation idempotency record"))
    }

    pub async fn transaction_mutation(
        &self,
        rewrites: Vec<FileRewrite>,
        tx: TxAppend,
        mutation: MutationIdentity,
        mutation_id: String,
    ) -> Result<CachedMutation> {
        self.guard_node_paths(
            rewrites
                .iter()
                .map(|rewrite| rewrite.path.as_path())
                .chain(std::iter::once(tx.tx_path.as_path())),
        )?;
        let written_paths = rewrites
            .iter()
            .map(|rewrite| rewrite.path.clone())
            .chain(std::iter::once(tx.tx_path.clone()))
            .collect::<Vec<_>>();
        let request_id = tx
            .request_id
            .clone()
            .unwrap_or_else(|| Uuid::new_v4().to_string());
        if let Some(cached) = self.cached_mutation(&request_id, &mutation).await? {
            self.publish_paths(Some(&cached.tx_id), written_paths)
                .await?;
            return Ok(cached);
        }
        let (reply, rx) = oneshot::channel();
        self.tx
            .send(WriterCommand::Transaction {
                req: TransactionRequest {
                    rewrites,
                    tx,
                    request_id: request_id.clone(),
                    mutation: Some(mutation.clone()),
                    mutation_id: Some(mutation_id),
                },
                reply,
            })
            .await
            .map_err(|_| anyhow!("writer task is gone"))?;
        let result = rx.await.map_err(|_| anyhow!("writer reply dropped"))??;
        self.publish_paths(Some(&result.tx_id), written_paths)
            .await?;
        self.cached_mutation(&request_id, &mutation)
            .await?
            .ok_or_else(|| anyhow!("writer did not retain mutation idempotency record"))
    }

    pub async fn append_session(&self, req: SessionAppend) -> Result<SessionAppendResult> {
        let (reply, rx) = oneshot::channel();
        #[cfg(test)]
        let injected_failure = self.take_session_append_failure(&req);
        self.tx
            .send(WriterCommand::Session {
                req,
                reply,
                #[cfg(test)]
                injected_failure,
            })
            .await
            .map_err(|_| anyhow!("writer task is gone"))?;
        rx.await.map_err(|_| anyhow!("writer reply dropped"))?
    }

    /// Fail the next matching lifecycle append sent through this exact writer
    /// handle. The test seam fires after the retained `SessionWriter` has been
    /// selected, which proves the lifecycle persistence rollback path without
    /// relying on a filesystem replacement race.
    #[cfg(test)]
    pub(crate) fn fail_next_session_append(
        &self,
        run_id: &str,
        session_path: &Path,
    ) -> Arc<test_hooks::SessionAppendFailure> {
        let failure = Arc::new(test_hooks::SessionAppendFailure::new());
        let mut armed = self
            .session_append_failure
            .lock()
            .expect("writer handle session append failure hook");
        assert!(
            armed.is_none(),
            "a session append failure was already armed for this writer handle"
        );
        *armed = Some(ArmedSessionAppendFailure {
            run_id: run_id.to_string(),
            session_path: session_path.to_path_buf(),
            failure: Arc::clone(&failure),
        });
        failure
    }

    #[cfg(test)]
    fn take_session_append_failure(
        &self,
        req: &SessionAppend,
    ) -> Option<Arc<test_hooks::SessionAppendFailure>> {
        let mut armed = self
            .session_append_failure
            .lock()
            .expect("writer handle session append failure hook");
        let matches = armed.as_ref().is_some_and(|armed| {
            armed.run_id == req.run_id && armed.session_path == req.session_path
        });
        matches.then(|| {
            armed
                .take()
                .expect("matching writer append failure")
                .failure
        })
    }

    /// Take an exclusive hold on a set of session paths for the whole of a
    /// maintenance transaction.
    ///
    /// orgasmic:TASK-FZB6T.2 finding 3 — `run history compact` replaces a
    /// session file by renaming a rewritten sibling over it. The writer caches
    /// one open append handle per run, and a rename leaves that handle pointing
    /// at an ORPHANED INODE: every lifecycle line written afterwards goes to a
    /// file with no name and is lost.
    ///
    /// orgasmic:TASK-FZB6T.3 finding 1 — closing the handle ONCE was not
    /// exclusion. The next append simply reopened the same path, so an append
    /// that landed between the transaction's final fingerprint check and its
    /// `rename` went to the original inode, which the rename immediately
    /// orphaned. The replacement excluded that line and the archive PREDATED
    /// it, so rollback could not recover it either — the "restored byte for
    /// byte" argument does not hold for a byte the archive never saw. It is a
    /// real lifecycle edge: a persisted terminal driver event can make a file
    /// eligible BEFORE the supervisor appends its final `Lifecycle::Release`.
    ///
    /// A lease is exclusion. While it is held, an append for a held path is
    /// DEFERRED before it opens anything, and it runs — against whatever file
    /// the path holds once the transaction is done — when the lease is
    /// released. The final lifecycle line therefore lands in the replacement
    /// instead of in an orphan.
    ///
    /// Ordered behind every write already queued, because it travels the same
    /// channel, so the lease is also a barrier against the appends in flight
    /// when maintenance asked for it. A path already held by another lease is
    /// refused rather than shared.
    /// orgasmic:TASK-FZB6T.4 finding 1 — deliberately `pub(crate)`, and the
    /// only production caller is [`Self::with_detached_session_lease`]. Handing
    /// a bare lease to a request handler is how it acquired the request's
    /// lifetime twice; making that shape unreachable from outside this module's
    /// crate is cheaper than remembering not to write it a third time.
    pub(crate) async fn lease_sessions(&self, paths: Vec<PathBuf>) -> Result<SessionLease> {
        let (reply, rx) = oneshot::channel();
        self.tx
            .send(WriterCommand::LeaseSessions {
                paths: paths.clone(),
                reply,
            })
            .await
            .map_err(|_| anyhow!("writer task is gone"))?;
        rx.await.map_err(|_| anyhow!("writer reply dropped"))??;
        Ok(SessionLease {
            tx: self.tx.clone(),
            paths,
            released: false,
        })
    }

    /// Run one blocking maintenance transaction under a session lease whose
    /// owner is DETACHED from the caller's future.
    ///
    /// orgasmic:TASK-FZB6T.4 finding 1 — the same lifetime mistake, a third
    /// time. Holding the [`SessionLease`] in an HTTP handler and awaiting
    /// `spawn_blocking` from there gives the lease the REQUEST's lifetime and
    /// the transaction its own. A started `spawn_blocking` task is not
    /// cancelled when its join handle is dropped, but dropping the handler DOES
    /// drop the lease, and [`SessionLease::drop`] queues `ReleaseSessions`
    /// immediately. So a client disconnect, a route cancellation or a shutdown
    /// reopened the writer while the transaction was still between its final
    /// fingerprint check and its `rename` — exactly the orphaned-inode window
    /// the lease was introduced to close.
    ///
    /// Here the lease is acquired, held and released inside ONE detached
    /// `tokio::spawn`ed task, alongside the blocking work it authorizes. The
    /// caller may await the outcome, and dropping that await cancels nothing:
    /// the detached task still runs the transaction to its end, still writes
    /// its journal, and still releases the lease afterwards, so the appends
    /// queued behind it land in the replacement rather than in an orphan.
    pub async fn with_detached_session_lease<T, F>(
        &self,
        paths: Vec<PathBuf>,
        work: F,
    ) -> Result<T, LeasedTransactionError>
    where
        F: FnOnce() -> T + Send + 'static,
        T: Send + 'static,
    {
        let writer = self.clone();
        let owner = tokio::spawn(async move {
            let lease = writer
                .lease_sessions(paths)
                .await
                .map_err(|error| LeasedTransactionError::Lease(error.to_string()))?;
            let outcome = tokio::task::spawn_blocking(work).await;
            // Released by the OWNER, only once the transaction and its journal
            // are done — never by whoever happened to be awaiting the result.
            lease.release().await;
            outcome.map_err(|error| LeasedTransactionError::Transaction(error.to_string()))
        });
        owner
            .await
            .map_err(|error| LeasedTransactionError::Transaction(error.to_string()))?
    }

    /// How many appends are currently queued behind a lease. Test-visible so a
    /// regression can act exactly at the pre-rename instant without sleeping.
    #[cfg(test)]
    pub(crate) fn deferred_session_appends(&self) -> usize {
        self.deferred_appends
            .load(std::sync::atomic::Ordering::SeqCst)
    }

    /// Atomic read-modify-write through the writer flock.
    ///
    /// Use this when a caller's "read current value, mutate, write back"
    /// chain must not race with other writers (e.g. partial-update PATCH
    /// over a small overlay file). The transform runs inside the writer
    /// task while the path is flocked; concurrent `mutate_file` calls
    /// against the same path serialize through the writer channel.
    pub async fn mutate_file(&self, req: FileMutate) -> Result<()> {
        let written_path = req.path.clone();
        self.guard_node_paths([written_path.as_path()])?;
        let (reply, rx) = oneshot::channel();
        self.tx
            .send(WriterCommand::Mutate { req, reply })
            .await
            .map_err(|_| anyhow!("writer task is gone"))?;
        rx.await.map_err(|_| anyhow!("writer reply dropped"))??;
        self.publish_paths(None, [written_path]).await
    }

    /// Append one structured entry without exposing journal.org to whole-file
    /// callers.
    pub async fn append_journal_entry(
        &self,
        path: PathBuf,
        node_id: String,
        entry: orgasmic_core::node_kernel::JournalEntry,
    ) -> Result<()> {
        entry.validate()?;
        reject_journal_prose(&entry.body)?;
        let display_path = path.clone();
        self.mutate_file(FileMutate {
            path,
            transform: Box::new(move |current| {
                let entries = orgasmic_core::node_kernel::parse_journal(current, "journal.org")?;
                if entries.iter().any(|item| item.entry_id == entry.entry_id) {
                    bail!("journal entry {} already exists", entry.entry_id);
                }
                let next = orgasmic_core::node_kernel::append_entry(current, &node_id, &entry);
                checked_journal_bytes(&display_path, next)
            }),
        })
        .await
    }

    /// Surgically edit authored prose. OCC compares only the target comment,
    /// so an unrelated append does not make a valid edit stale.
    pub async fn edit_journal_comment(
        &self,
        path: PathBuf,
        entry_id: String,
        expected_body: String,
        new_body: String,
        actor: CommentMutationActor,
        edited_at: String,
    ) -> Result<()> {
        reject_journal_prose(&new_body)?;
        let display_path = path.clone();
        self.mutate_file(FileMutate {
            path,
            transform: Box::new(move |current| {
                require_comment_body(current, &entry_id, &expected_body, &actor)?;
                let next = orgasmic_core::node_kernel::edit_comment_body(
                    current,
                    &entry_id,
                    &new_body,
                    actor.name(),
                    &edited_at,
                )?;
                checked_journal_bytes(&display_path, next)
            }),
        })
        .await
    }

    /// Delete authored prose while preserving its reply-chain identity.
    pub async fn tombstone_journal_comment(
        &self,
        path: PathBuf,
        entry_id: String,
        expected_body: String,
        actor: CommentMutationActor,
        deleted_at: String,
    ) -> Result<()> {
        let display_path = path.clone();
        self.mutate_file(FileMutate {
            path,
            transform: Box::new(move |current| {
                require_comment_body(current, &entry_id, &expected_body, &actor)?;
                let next = orgasmic_core::node_kernel::tombstone_comment(
                    current,
                    &entry_id,
                    actor.name(),
                    &deleted_at,
                )?;
                checked_journal_bytes(&display_path, next)
            }),
        })
        .await
    }

    pub async fn rewrite_file(&self, req: FileRewrite, request_id: Option<String>) -> Result<()> {
        let written_path = req.path.clone();
        self.guard_node_paths([written_path.as_path()])?;
        let request_id = request_id.unwrap_or_else(|| Uuid::new_v4().to_string());
        let cached = {
            let cache = self.idempotency.lock().await;
            matches!(cache.get(&request_id), Some(CachedResponse::Rewrite))
        };
        if cached {
            self.publish_paths(Some(&request_id), [written_path])
                .await?;
            return Ok(());
        }
        let (reply, rx) = oneshot::channel();
        self.tx
            .send(WriterCommand::Rewrite { req, reply })
            .await
            .map_err(|_| anyhow!("writer task is gone"))?;
        rx.await.map_err(|_| anyhow!("writer reply dropped"))??;
        self.idempotency
            .lock()
            .await
            .insert(request_id.clone(), CachedResponse::Rewrite);
        self.publish_paths(Some(&request_id), [written_path])
            .await?;
        Ok(())
    }

    /// Stop the writer, giving up after `budget`.
    ///
    /// orgasmic:TASK-Q07Y5 — this used to be an unbounded `send().await` +
    /// `rx.await`: it queues behind every accepted command, and the one being
    /// executed can sit in a blocked `fsync`. The caller could therefore never
    /// state a finite shutdown cost, so neither could the service manager's
    /// kill timeout. The send is inside the budget too, because a full channel
    /// *is* queueing behind the stuck write.
    ///
    /// A timeout does not abandon the writer task — it keeps running and may
    /// still land its write — but the caller must treat the work as unproven
    /// and record [`WriterShutdownOutcome::TimedOut`] durably before exiting.
    pub async fn shutdown_within(&self, budget: std::time::Duration) -> WriterShutdownOutcome {
        let deadline = tokio::time::Instant::now() + budget;
        let (reply, rx) = oneshot::channel();
        match tokio::time::timeout_at(deadline, self.tx.send(WriterCommand::Shutdown { reply }))
            .await
        {
            Ok(Ok(())) => {}
            Ok(Err(_)) => return WriterShutdownOutcome::AlreadyGone,
            Err(_) => return self.shutdown_timed_out(),
        }
        match tokio::time::timeout_at(deadline, rx).await {
            Ok(Ok(())) => WriterShutdownOutcome::Clean,
            Ok(Err(_)) => WriterShutdownOutcome::AlreadyGone,
            Err(_) => self.shutdown_timed_out(),
        }
    }

    fn shutdown_timed_out(&self) -> WriterShutdownOutcome {
        WriterShutdownOutcome::TimedOut {
            queued: self
                .tx
                .max_capacity()
                .saturating_sub(self.tx.capacity())
                .saturating_add(self.deferred_appends.load(Ordering::SeqCst)),
            in_flight: self
                .in_flight
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .clone(),
        }
    }

    /// Test-only teardown for in-crate fixtures that just want the writer to
    /// stop. Deliberately not public: production shutdown must state a budget
    /// and handle [`WriterShutdownOutcome::TimedOut`] (TASK-Q07Y5).
    #[cfg(test)]
    pub(crate) async fn shutdown(&self) -> WriterShutdownOutcome {
        self.shutdown_within(WRITER_SHUTDOWN_TIMEOUT).await
    }

    /// The write the writer task is executing right now, if any.
    pub fn in_flight_write(&self) -> Option<PendingWrite> {
        self.in_flight
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }

    pub fn status(&self) -> WriterStatus {
        let in_flight = self.in_flight_write();
        let in_flight_age_ms = self
            .metrics
            .in_flight_started
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .as_ref()
            .map(|started| started.elapsed().as_millis().min(u128::from(u64::MAX)) as u64);
        WriterStatus {
            liveness: !self.tx.is_closed(),
            open_session_handles: self.metrics.open_session_handles.load(Ordering::Relaxed),
            queue_depth: self
                .tx
                .max_capacity()
                .saturating_sub(self.tx.capacity())
                .saturating_add(self.deferred_appends.load(Ordering::SeqCst)),
            in_flight_operation: in_flight,
            in_flight_age_ms,
            completed_total: self.metrics.completed_total.load(Ordering::Relaxed),
            failed_total: self.metrics.failed_total.load(Ordering::Relaxed),
            last_duration_ms: self.metrics.last_duration_ms.load(Ordering::Relaxed),
            max_duration_ms: self.metrics.max_duration_ms.load(Ordering::Relaxed),
        }
    }

    #[cfg(test)]
    pub(crate) async fn gate_next_transaction(&self) -> Arc<TestTransactionGate> {
        let gate = Arc::new(TestTransactionGate {
            entered: tokio::sync::Notify::new(),
            release: tokio::sync::Notify::new(),
        });
        *self.transaction_gate.lock().await = Some(Arc::clone(&gate));
        gate
    }
}

fn reject_journal_prose(body: &str) -> Result<()> {
    if let Some((line, _)) = body
        .lines()
        .enumerate()
        .find(|(_, line)| line.starts_with("* "))
    {
        bail!(
            "journal prose line {} starts with a column-0 `* ` heading; nested `**` headings are allowed",
            line + 1
        );
    }
    Ok(())
}

/// Authorship gate for comment edit/delete (KA934.1) over the guarded
/// `:ACTOR:` namespace (dec_Q78QN).
///
/// Authorship is the raw stored `:ACTOR:` string compared verbatim against
/// the member-session name. Rename semantics: a member renamed in
/// `members.org` immediately loses edit/delete rights on comments made
/// under the old name — the stored `:ACTOR:` no longer matches, and no
/// migration rewrites journals on rename. The inverse rename case cuts the
/// other way: a member re-added or renamed INTO a retired member's name
/// inherits edit/delete on that member's old comments (raw `:ACTOR:`
/// equality; accepted).
fn require_comment_body(
    current: &str,
    entry_id: &str,
    expected_body: &str,
    actor: &CommentMutationActor,
) -> Result<()> {
    let entries = orgasmic_core::node_kernel::parse_journal(current, "journal.org")?;
    let entry = entries
        .iter()
        .find(|entry| entry.entry_id == entry_id)
        .ok_or_else(|| CommentNotFound {
            entry_id: entry_id.to_string(),
        })?;
    if entry.ty != "comment" {
        bail!("journal entry {entry_id} is not an editable comment");
    }
    if matches!(actor, CommentMutationActor::Member(name) if entry.actor != *name) {
        return Err(CommentAuthorshipForbidden {
            entry_id: entry_id.to_string(),
        }
        .into());
    }
    if entry.body != expected_body {
        return Err(CommentConflict {
            entry_id: entry_id.to_string(),
        }
        .into());
    }
    Ok(())
}

fn checked_journal_bytes(path: &Path, next: String) -> Result<Vec<u8>> {
    orgasmic_core::node_kernel::parse_journal(&next, "journal.org")?;
    if orgasmic_core::node_kernel::journal_size_lint(&next) {
        warn!(
            path = %path.display(),
            bytes = next.len(),
            threshold = orgasmic_core::node_kernel::JOURNAL_SIZE_LINT_BYTES,
            "journal.org exceeds the v1 size lint threshold"
        );
    }
    Ok(next.into_bytes())
}

/// Boot the writer task and return a clone-able handle.
pub fn spawn(events: EventBus) -> WriterHandle {
    spawn_with_catalog_and_index(events, None, None)
}

/// [`spawn`] with a run catalog attached to the session-append boundary.
///
/// orgasmic:TASK-FZB6T item 1 — the catalog is maintained *through the existing
/// session writer boundary* rather than by a second component watching the
/// filesystem. This is the one place in the daemon that knows a session file
/// was written before anybody asks about it, so a lifecycle append invalidates
/// that run's cached entry here and the next inventory re-derives it.
///
/// Only lifecycle appends invalidate. A driver event changes the file's length
/// (so the fingerprint would catch it anyway on the next refresh) but cannot
/// change a lifecycle verdict, and invalidating on the transcript firehose would
/// make every poll of a chatty run re-read its window — the exact cost the
/// catalog exists to remove.
pub fn spawn_with_catalog(
    events: EventBus,
    catalog: Option<crate::run_catalog::RunCatalog>,
) -> WriterHandle {
    spawn_with_catalog_and_index(events, catalog, None)
}

pub fn spawn_with_catalog_and_index(
    events: EventBus,
    catalog: Option<crate::run_catalog::RunCatalog>,
    index: Option<crate::index::Index>,
) -> WriterHandle {
    spawn_with_catalog_index_and_machine(events, catalog, index, None)
}

pub(crate) fn spawn_with_catalog_index_and_machine(
    events: EventBus,
    catalog: Option<crate::run_catalog::RunCatalog>,
    index: Option<crate::index::Index>,
    machine_id: Option<String>,
) -> WriterHandle {
    let (tx, rx) = mpsc::channel(256);
    let idempotency = Arc::new(Mutex::new(HashMap::new()));
    let in_flight = Arc::new(std::sync::Mutex::new(None));
    let metrics = Arc::new(WriterMetrics::default());
    let deferred_appends = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    tokio::spawn(writer_loop(
        rx,
        events,
        Arc::clone(&idempotency),
        Arc::clone(&in_flight),
        Arc::clone(&metrics),
        catalog,
        Arc::clone(&deferred_appends),
    ));
    WriterHandle {
        tx,
        idempotency,
        in_flight,
        metrics,
        index,
        machine_id,
        deferred_appends,
        apply_failures: Arc::new(std::sync::Mutex::new(HashMap::new())),
        unapplied: Arc::new(std::sync::Mutex::new(Vec::new())),
        #[cfg(test)]
        transaction_gate: Arc::new(Mutex::new(None)),
        #[cfg(test)]
        session_append_failure: Arc::new(std::sync::Mutex::new(None)),
    }
}

/// A node write refused because another dispatch currently holds the claim
/// (TASK-CLM6W). Claims are not a cross-machine barrier between dispatches;
/// overlapping free writes are preserved by the ledger sync conflict path.
/// Typed rather than a bare message so the API can answer 409 naming the holder
/// instead of the generic 500 every other writer error collapses to — the
/// refusal is the operator's answer, not an internal failure. It carries no
/// filesystem path: this text reaches HTTP responses, which must stay path-free.
#[derive(Debug)]
pub(crate) struct ClaimConflict {
    pub node_id: String,
    pub holder: String,
    pub machine: String,
}

impl std::fmt::Display for ClaimConflict {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "node {} is claimed by machine {}; machine {} cannot write it",
            self.node_id, self.holder, self.machine
        )
    }
}

impl std::error::Error for ClaimConflict {}

/// A comment mutation refused because the member is not its author. Kept
/// typed so the API can return 403 without exposing other writer failures.
#[derive(Debug)]
pub(crate) struct CommentAuthorshipForbidden {
    entry_id: String,
}

impl std::fmt::Display for CommentAuthorshipForbidden {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "only the author may edit journal comment {}",
            self.entry_id
        )
    }
}

impl std::error::Error for CommentAuthorshipForbidden {}

#[derive(Debug)]
pub(crate) struct CommentConflict {
    entry_id: String,
}

impl std::fmt::Display for CommentConflict {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "journal comment {} changed since it was read",
            self.entry_id
        )
    }
}

impl std::error::Error for CommentConflict {}

#[derive(Debug)]
pub(crate) struct CommentNotFound {
    entry_id: String,
}

impl std::fmt::Display for CommentNotFound {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "journal entry {} not found", self.entry_id)
    }
}

impl std::error::Error for CommentNotFound {}

fn guard_node_write(path: &Path, machine_id: &str) -> Result<()> {
    let Some(dotorg) = path
        .ancestors()
        .find(|ancestor| ancestor.file_name().and_then(|name| name.to_str()) == Some(".orgasmic"))
    else {
        return Ok(());
    };
    let relative = path.strip_prefix(dotorg).expect("ancestor is a prefix");
    let mut parts = relative.components().filter_map(|part| match part {
        std::path::Component::Normal(value) => value.to_str(),
        _ => None,
    });
    let Some(collection) = parts.next() else {
        return Ok(());
    };
    if DAEMON_OWNED_SURFACES
        .iter()
        .any(|surface| collection.eq_ignore_ascii_case(surface))
    {
        return Ok(());
    }
    let Some(node_id) = parts.next() else {
        return Ok(());
    };
    let Some(project_root) = dotorg.parent() else {
        return Ok(());
    };
    let claims = orgasmic_core::read_claims(project_root)?;
    if let Some(claim) = claims
        .get(node_id)
        .filter(|claim| claim.holder != machine_id)
    {
        return Err(ClaimConflict {
            node_id: node_id.to_string(),
            holder: claim.holder.clone(),
            machine: machine_id.to_string(),
        }
        .into());
    }
    Ok(())
}

/// What a command is, for the shutdown-loss report (TASK-Q07Y5).
fn describe_command(cmd: &WriterCommand) -> PendingWrite {
    match cmd {
        WriterCommand::Tx { req, .. } => PendingWrite {
            kind: "tx".to_string(),
            run_id: None,
            tx_type: Some(req.entry.ty.clone()),
            path: Some(req.tx_path.clone()),
        },
        WriterCommand::Session { req, .. } => PendingWrite {
            kind: "session".to_string(),
            run_id: Some(req.run_id.clone()),
            tx_type: None,
            path: Some(req.session_path.clone()),
        },
        WriterCommand::Transaction { req, .. } => PendingWrite {
            kind: "transaction".to_string(),
            run_id: None,
            tx_type: Some(req.tx.entry.ty.clone()),
            path: Some(req.tx.tx_path.clone()),
        },
        WriterCommand::TransactionMulti { req, .. } => PendingWrite {
            kind: "transaction_multi".to_string(),
            run_id: None,
            tx_type: Some(
                req.txs
                    .iter()
                    .map(|tx| tx.entry.ty.as_str())
                    .collect::<Vec<_>>()
                    .join("+"),
            ),
            path: req.txs.first().map(|tx| tx.tx_path.clone()),
        },
        WriterCommand::TransactionMutate { req, .. } => PendingWrite {
            kind: "transaction_mutate".to_string(),
            run_id: None,
            tx_type: Some(req.tx.entry.ty.clone()),
            path: Some(req.file.path.clone()),
        },
        WriterCommand::Rewrite { req, .. } => PendingWrite {
            kind: "rewrite".to_string(),
            run_id: None,
            tx_type: None,
            path: Some(req.path.clone()),
        },
        WriterCommand::Mutate { req, .. } => PendingWrite {
            kind: "mutate".to_string(),
            run_id: None,
            tx_type: None,
            path: Some(req.path.clone()),
        },
        WriterCommand::LeaseSessions { paths, .. } => PendingWrite {
            kind: "lease_sessions".to_string(),
            run_id: None,
            tx_type: None,
            path: paths.first().cloned(),
        },
        WriterCommand::ReleaseSessions { paths, .. } => PendingWrite {
            kind: "release_sessions".to_string(),
            run_id: None,
            tx_type: None,
            path: paths.first().cloned(),
        },
        WriterCommand::Barrier { .. } => PendingWrite {
            kind: "barrier".to_string(),
            run_id: None,
            tx_type: None,
            path: None,
        },
        WriterCommand::Shutdown { .. } => PendingWrite {
            kind: "shutdown".to_string(),
            run_id: None,
            tx_type: None,
            path: None,
        },
    }
}

fn describe_session_append(req: &SessionAppend) -> PendingWrite {
    PendingWrite {
        kind: "session".to_string(),
        run_id: Some(req.run_id.clone()),
        tx_type: None,
        path: Some(req.session_path.clone()),
    }
}

/// Test-only stall injected in front of a matching write, so a test can hold
/// the writer exactly the way a blocked `fsync` does — on the thread, inside
/// the command, with `shutdown` queued behind it (TASK-Q07Y5).
///
/// `ORGASMIC_TEST_WRITER_STALL_MS` sets the stall;
/// `ORGASMIC_TEST_WRITER_STALL_TX_TYPE` selects which tx type it applies to,
/// so ordinary daemon writes are untouched.
fn injected_write_stall(pending: &PendingWrite) -> Option<std::time::Duration> {
    let millis = std::env::var("ORGASMIC_TEST_WRITER_STALL_MS")
        .ok()?
        .parse::<u64>()
        .ok()
        .filter(|millis| *millis > 0)?;
    let selector = std::env::var("ORGASMIC_TEST_WRITER_STALL_TX_TYPE").ok()?;
    (pending.tx_type.as_deref() == Some(selector.as_str()))
        .then(|| std::time::Duration::from_millis(millis))
}

/// One append queued behind a session lease, with the caller still waiting on
/// its reply (orgasmic:TASK-FZB6T.3 finding 1).
struct DeferredSessionAppend {
    req: SessionAppend,
    reply: oneshot::Sender<Result<SessionAppendResult>>,
    #[cfg(test)]
    injected_failure: Option<Arc<test_hooks::SessionAppendFailure>>,
}

async fn writer_loop(
    mut rx: mpsc::Receiver<WriterCommand>,
    events: EventBus,
    idempotency: Arc<Mutex<HashMap<String, CachedResponse>>>,
    in_flight: Arc<std::sync::Mutex<Option<PendingWrite>>>,
    metrics: Arc<WriterMetrics>,
    catalog: Option<crate::run_catalog::RunCatalog>,
    deferred_appends: Arc<std::sync::atomic::AtomicUsize>,
) {
    use std::sync::atomic::Ordering;

    let mut tx_handles: HashMap<PathBuf, CachedTxWriter> = HashMap::new();
    let mut session_handles: HashMap<String, SessionWriter> = HashMap::new();
    // Session paths a maintenance transaction currently holds.
    let mut leased: HashSet<PathBuf> = HashSet::new();
    let mut deferred: Vec<DeferredSessionAppend> = Vec::new();
    let mut cmd = rx.recv().await;
    while let Some(current) = cmd.take() {
        // orgasmic:TASK-BX5SR — one command must not take the writer with it.
        // A panic inside a command drops that command's reply (the caller sees
        // "writer reply dropped") and the loop keeps serving; without this
        // guard every later write failed with "writer task is gone" until a
        // daemon restart. The block yields `true` only for a clean shutdown.
        let step = std::panic::AssertUnwindSafe(async {
            // orgasmic:TASK-Q07Y5 — publish the head-of-line write before running
            // it. A shutdown that gives up on its budget reads this to say which
            // write it gave up on; without it the report is a bare count.
            let command_started = Instant::now();
            let pending = describe_command(&current);
            let mut command_failed = false;
            let mut record_current = true;
            {
                let stall = injected_write_stall(&pending);
                *in_flight.lock().unwrap_or_else(|e| e.into_inner()) = Some(pending.clone());
                *metrics
                    .in_flight_started
                    .lock()
                    .unwrap_or_else(|error| error.into_inner()) = Some(command_started);
                if let Some(stall) = stall {
                    std::thread::sleep(stall);
                }
            }
            match current {
                WriterCommand::Tx { req, reply } => {
                    let mut batch = vec![(req, reply)];
                    while let Ok(next) = rx.try_recv() {
                        match next {
                            WriterCommand::Tx { req, reply } => batch.push((req, reply)),
                            other => {
                                cmd = Some(other);
                                break;
                            }
                        }
                    }
                    let outcomes = process_tx_batch(&mut tx_handles, batch);
                    for (req, result, reply) in outcomes {
                        command_failed |= result.is_err();
                        if let Ok(ref ok) = result {
                            events.publish(
                                Topic::Daemon,
                                EventPayload::TxAppended {
                                    project_id: req.project_id.clone(),
                                    tx_id: ok.tx_id.clone(),
                                    ty: req.entry.ty.clone(),
                                },
                            );
                        }
                        let _ = reply.send(result);
                    }
                }
                WriterCommand::Session {
                    req,
                    reply,
                    #[cfg(test)]
                    injected_failure,
                } => {
                    // orgasmic:TASK-FZB6T.3 finding 1 — the check is here, BEFORE
                    // anything is opened. A held path's append waits for the
                    // transaction that holds it and then lands in whatever file the
                    // path holds, instead of racing a rename onto an inode the
                    // rename is about to orphan.
                    if leased.contains(&req.session_path) {
                        deferred.push(DeferredSessionAppend {
                            req,
                            reply,
                            #[cfg(test)]
                            injected_failure,
                        });
                        deferred_appends.store(deferred.len(), Ordering::SeqCst);
                        // This command is still pending: it has moved from the
                        // channel into the lease-owned deferred queue, not
                        // completed. Its eventual execution records exactly one
                        // outcome below.
                        record_current = false;
                    } else {
                        command_failed = run_session_append(
                            &mut session_handles,
                            &events,
                            catalog.as_ref(),
                            req,
                            reply,
                            #[cfg(test)]
                            injected_failure,
                        );
                        metrics
                            .open_session_handles
                            .store(session_handles.len(), Ordering::Relaxed);
                    }
                }
                WriterCommand::Rewrite { req, reply } => {
                    let result = rewrite_file_inner(&req);
                    command_failed = result.is_err();
                    let _ = reply.send(result);
                }
                WriterCommand::Mutate { req, reply } => {
                    let result = mutate_file_inner(req);
                    command_failed = result.is_err();
                    let _ = reply.send(result);
                }
                WriterCommand::Transaction { req, reply } => {
                    let cached = {
                        let cache = idempotency.lock().await;
                        match req.mutation.as_ref() {
                            Some(mutation) => {
                                cached_mutation_from_map(&cache, &req.request_id, mutation).map(
                                    |cached| {
                                        cached.map(|cached| TxAppendResult {
                                            tx_id: cached.tx_id,
                                            tx_path: req.tx.tx_path.clone(),
                                        })
                                    },
                                )
                            }
                            None => Err(anyhow!("writer transaction lacks a mutation identity")),
                        }
                    };
                    let mut reply = Some(reply);
                    let execute = match cached {
                        Ok(Some(result)) => {
                            let _ = reply
                                .take()
                                .expect("writer reply available")
                                .send(Ok(result));
                            false
                        }
                        Err(error) => {
                            command_failed = true;
                            let _ = reply
                                .take()
                                .expect("writer reply available")
                                .send(Err(error));
                            false
                        }
                        Ok(None) => true,
                    };
                    if execute {
                        let result = transaction_inner(
                            &mut tx_handles,
                            &req.rewrites,
                            req.tx.clone(),
                            &req.request_id,
                            || Ok(()),
                        );
                        command_failed = result.is_err();
                        if let Ok(ref ok) = result {
                            let mut cache = idempotency.lock().await;
                            cache.insert(
                                req.request_id.clone(),
                                CachedResponse::Tx {
                                    result: ok.clone(),
                                    mutation: req.mutation.clone(),
                                    mutation_id: req.mutation_id.clone(),
                                },
                            );
                            drop(cache);
                            events.publish(
                                Topic::Daemon,
                                EventPayload::TxAppended {
                                    project_id: req.tx.project_id.clone(),
                                    tx_id: ok.tx_id.clone(),
                                    ty: req.tx.entry.ty.clone(),
                                },
                            );
                        }
                        let _ = reply.take().expect("writer reply available").send(result);
                    }
                }
                WriterCommand::TransactionMulti { req, reply } => {
                    let cache_key = multi_cache_key(&req.request_id);
                    let cached = {
                        let cache = idempotency.lock().await;
                        match cached_multi_from_map(
                            &cache,
                            &cache_key,
                            &req.request_id,
                            &req.mutation,
                        ) {
                            Ok(Some(cached)) => Ok(Some(cached)),
                            Err(error) => Err(error),
                            Ok(None) => {
                                let collision = req.txs.iter().find_map(|tx| {
                                    tx.request_id
                                        .as_ref()
                                        .filter(|request_id| cache.contains_key(*request_id))
                                });
                                match collision {
                                Some(request_id) => Err(anyhow!(
                                    "request_id `{request_id}` was already used outside this multi transaction"
                                )),
                                None => Ok(None),
                            }
                            }
                        }
                    };
                    match cached {
                        Ok(Some((results, MultiDurability::Durable))) => {
                            let _ = reply.send(Ok(results));
                        }
                        Ok(Some((results, MultiDurability::SyncUncertain))) => {
                            let sync = results
                                .first()
                                .ok_or_else(|| anyhow!("cached multi transaction has no results"))
                                .and_then(|result| sync_tx_writer(&tx_handles, &result.tx_path));
                            match sync {
                                Ok(()) => {
                                    cache_durable_multi(&idempotency, &cache_key, &req, &results)
                                        .await;
                                    publish_multi_events(&events, &req.txs, &results);
                                    let _ = reply.send(Ok(results));
                                }
                                Err(error) => {
                                    command_failed = true;
                                    let _ = reply.send(Err(anyhow!(
                                        CommittedSyncUncertainError::retry(error)
                                    )));
                                }
                            }
                        }
                        Err(error) => {
                            command_failed = true;
                            let _ = reply.send(Err(error));
                        }
                        Ok(None) => {
                            let result = transaction_multi_inner(
                                &mut tx_handles,
                                &req.rewrites,
                                &req.txs,
                                &req.request_id,
                                true,
                                test_hooks::before_multi_commit,
                            );
                            match result {
                                Ok(MultiTransactionCommit::Durable(results)) => {
                                    cache_durable_multi(&idempotency, &cache_key, &req, &results)
                                        .await;
                                    publish_multi_events(&events, &req.txs, &results);
                                    let _ = reply.send(Ok(results));
                                }
                                Ok(MultiTransactionCommit::SyncUncertain { results, error }) => {
                                    command_failed = true;
                                    let mut cache = idempotency.lock().await;
                                    cache.insert(
                                        cache_key,
                                        CachedResponse::Multi {
                                            results,
                                            mutation: req.mutation.clone(),
                                            durability: MultiDurability::SyncUncertain,
                                        },
                                    );
                                    drop(cache);
                                    let _ = reply.send(Err(anyhow!(
                                        CommittedSyncUncertainError::initial(error)
                                    )));
                                }
                                Err(error) => {
                                    command_failed = true;
                                    let _ = reply.send(Err(error));
                                }
                            }
                        }
                    }
                }
                WriterCommand::TransactionMutate { req, reply } => {
                    let cached = {
                        let cache = idempotency.lock().await;
                        cached_transaction_from_map(&cache, &req.request_id, &req.mutation)
                    };
                    let mut reply = Some(reply);
                    let execute = match cached {
                        Ok(Some(result)) => {
                            let _ = reply
                                .take()
                                .expect("writer reply available")
                                .send(Ok(result));
                            false
                        }
                        Err(error) => {
                            command_failed = true;
                            let _ = reply
                                .take()
                                .expect("writer reply available")
                                .send(Err(error));
                            false
                        }
                        Ok(None) => true,
                    };
                    if execute {
                        let result = transaction_mutate_file_inner(
                            &mut tx_handles,
                            req.file,
                            req.tx.clone(),
                            &req.request_id,
                        );
                        command_failed = result.is_err();
                        if let Ok(ref ok) = result {
                            let mut cache = idempotency.lock().await;
                            cache.insert(
                                req.request_id.clone(),
                                CachedResponse::Tx {
                                    result: ok.clone(),
                                    mutation: Some(req.mutation.clone()),
                                    mutation_id: req.mutation_id.clone(),
                                },
                            );
                            drop(cache);
                            events.publish(
                                Topic::Daemon,
                                EventPayload::TxAppended {
                                    project_id: req.tx.project_id.clone(),
                                    tx_id: ok.tx_id.clone(),
                                    ty: req.tx.entry.ty.clone(),
                                },
                            );
                        }
                        let _ = reply.take().expect("writer reply available").send(result);
                    }
                }
                WriterCommand::LeaseSessions { paths, reply } => {
                    // Two transactions holding the same path would each believe it
                    // had exclusion. Refuse rather than share.
                    let held = paths.iter().find(|path| leased.contains(*path)).cloned();
                    match held {
                        Some(path) => {
                            command_failed = true;
                            let _ = reply.send(Err(anyhow!(
                                "session {} is already held by a maintenance lease",
                                path.display()
                            )));
                        }
                        None => {
                            for path in paths {
                                session_handles.retain(|_, writer| writer.path() != path);
                                leased.insert(path);
                            }
                            metrics
                                .open_session_handles
                                .store(session_handles.len(), Ordering::Relaxed);
                            let _ = reply.send(Ok(()));
                        }
                    }
                }
                WriterCommand::ReleaseSessions { paths, reply } => {
                    for path in &paths {
                        leased.remove(path);
                    }
                    // Arrival order, so a run's lifecycle lines stay in the order
                    // the supervisor wrote them.
                    let mut still_held = Vec::new();
                    let mut ready = Vec::new();
                    for entry in deferred.drain(..) {
                        if leased.contains(&entry.req.session_path) {
                            still_held.push(entry);
                        } else {
                            ready.push(entry);
                        }
                    }
                    deferred = still_held;
                    deferred_appends.store(deferred.len(), Ordering::SeqCst);
                    let _ = reply.send(());
                    let ready_count = ready.len();
                    for (offset, entry) in ready.into_iter().enumerate() {
                        let deferred_pending = describe_session_append(&entry.req);
                        let deferred_started = Instant::now();
                        *in_flight.lock().unwrap_or_else(|e| e.into_inner()) =
                            Some(deferred_pending.clone());
                        *metrics
                            .in_flight_started
                            .lock()
                            .unwrap_or_else(|error| error.into_inner()) = Some(deferred_started);
                        let failed = run_session_append(
                            &mut session_handles,
                            &events,
                            catalog.as_ref(),
                            entry.req,
                            entry.reply,
                            #[cfg(test)]
                            entry.injected_failure,
                        );
                        metrics
                            .open_session_handles
                            .store(session_handles.len(), Ordering::Relaxed);
                        record_writer_command(
                            &metrics,
                            deferred_started.elapsed(),
                            failed,
                            &deferred_pending,
                            rx.len()
                                .saturating_add(deferred.len())
                                .saturating_add(ready_count.saturating_sub(offset + 1)),
                        );
                    }
                }
                WriterCommand::Barrier { run, reply } => {
                    let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(run));
                    let _ = reply.send(());
                }
                WriterCommand::Shutdown { reply } => {
                    tx_handles.clear();
                    session_handles.clear();
                    metrics.open_session_handles.store(0, Ordering::Relaxed);
                    // A deferred append is not written on the way out: the file it
                    // names may be mid-transaction, and reporting the loss is more
                    // honest than appending to whatever inode happens to be there.
                    for entry in deferred.drain(..) {
                        let _ = entry.reply.send(Err(anyhow!(
                            "writer shut down while a maintenance lease held {}",
                            entry.req.session_path.display()
                        )));
                    }
                    deferred_appends.store(0, Ordering::SeqCst);
                    *in_flight.lock().unwrap_or_else(|e| e.into_inner()) = None;
                    *metrics
                        .in_flight_started
                        .lock()
                        .unwrap_or_else(|error| error.into_inner()) = None;
                    record_writer_command(
                        &metrics,
                        command_started.elapsed(),
                        false,
                        &pending,
                        rx.len(),
                    );
                    let _ = reply.send(());
                    return true;
                }
            }
            *in_flight.lock().unwrap_or_else(|e| e.into_inner()) = None;
            *metrics
                .in_flight_started
                .lock()
                .unwrap_or_else(|error| error.into_inner()) = None;
            if record_current {
                record_writer_command(
                    &metrics,
                    command_started.elapsed(),
                    command_failed,
                    &pending,
                    rx.len().saturating_add(deferred.len()),
                );
            }
            false
        });
        match step.catch_unwind().await {
            Ok(true) => break,
            Ok(false) => {}
            Err(panic) => {
                let cause = panic
                    .downcast_ref::<&str>()
                    .map(|cause| cause.to_string())
                    .or_else(|| panic.downcast_ref::<String>().cloned())
                    .unwrap_or_else(|| "non-string panic payload".to_string());
                error!(cause, "writer command panicked; the writer keeps serving");
                *in_flight.lock().unwrap_or_else(|e| e.into_inner()) = None;
                *metrics
                    .in_flight_started
                    .lock()
                    .unwrap_or_else(|error| error.into_inner()) = None;
                metrics.failed_total.fetch_add(1, Ordering::Relaxed);
            }
        }
        if cmd.is_none() {
            cmd = rx.recv().await;
        }
    }
}

fn record_writer_command(
    metrics: &WriterMetrics,
    duration: std::time::Duration,
    failed: bool,
    pending: &PendingWrite,
    queue_depth: usize,
) {
    if failed {
        metrics.failed_total.fetch_add(1, Ordering::Relaxed);
    } else {
        metrics.completed_total.fetch_add(1, Ordering::Relaxed);
    }
    let duration_ms = duration.as_millis().min(u128::from(u64::MAX)) as u64;
    metrics
        .last_duration_ms
        .store(duration_ms, Ordering::Relaxed);
    metrics
        .max_duration_ms
        .fetch_max(duration_ms, Ordering::Relaxed);
    if duration >= std::time::Duration::from_secs(1) {
        warn!(
            target = ?pending.path,
            cause = pending.kind,
            queue_depth,
            duration_ms,
            "slow writer command"
        );
    }
}

/// Run one session append and publish what it produced.
///
/// Extracted so the same path serves an append that arrives while nothing holds
/// its file and one that queued behind a session lease
/// (orgasmic:TASK-FZB6T.3 finding 1) — a deferred append must be the SAME write,
/// not a second implementation of it.
fn run_session_append(
    session_handles: &mut HashMap<String, SessionWriter>,
    events: &EventBus,
    catalog: Option<&crate::run_catalog::RunCatalog>,
    req: SessionAppend,
    reply: oneshot::Sender<Result<SessionAppendResult>>,
    #[cfg(test)] injected_failure: Option<Arc<test_hooks::SessionAppendFailure>>,
) -> bool {
    let SessionAppend {
        run_id,
        session_path,
        identity,
        authority,
        kind,
        event,
    } = req;
    // Lifecycle envelopes carry a `phase` tag (acquire/release/…). Captured
    // before the append moves `event` so run-liveness consumers get a dedicated
    // signal alongside the firehose.
    let lifecycle_phase = (kind == SessionEventKind::Lifecycle)
        .then(|| {
            event
                .get("phase")
                .and_then(serde_json::Value::as_str)
                .map(str::to_string)
        })
        .flatten();
    let result = append_session_inner(
        session_handles,
        &run_id,
        &session_path,
        identity,
        authority,
        kind,
        event,
        #[cfg(test)]
        injected_failure,
    );
    if let Ok(ref ok) = result {
        events.publish(
            Topic::Run,
            EventPayload::RunEvent {
                run_id: run_id.clone(),
                seq: ok.seq,
            },
        );
        // `release` is the run's terminal lifecycle line: nothing appends to
        // this session afterwards in the ordinary course, so the handle goes
        // with it. Holding it leaked one fd per ended run until the daemon hit
        // macOS's 256 soft limit (vscode-orsl letter, 2026-08-22). A late
        // append (recovery note) simply reopens the file.
        if lifecycle_phase.as_deref() == Some("release") {
            session_handles.remove(&run_id);
        }
        if let Some(phase) = lifecycle_phase {
            // orgasmic:TASK-FZB6T — the catalog update runs through this
            // boundary, before the event is published, so no consumer woken by
            // the lifecycle event can read a catalog entry that predates the
            // write it was told about.
            if let Some(catalog) = catalog {
                catalog.invalidate_session(&session_path);
            }
            events.publish(
                Topic::Run,
                EventPayload::RunLifecycle {
                    run_id: run_id.clone(),
                    phase,
                },
            );
        }
    }
    let failed = result.is_err();
    let _ = reply.send(result);
    failed
}

struct PendingTxBatchItem {
    req: TxAppend,
    reply: oneshot::Sender<Result<TxAppendResult>>,
    result: Result<TxAppendResult>,
}

async fn cache_durable_multi(
    idempotency: &Mutex<HashMap<String, CachedResponse>>,
    cache_key: &str,
    req: &TransactionMultiRequest,
    results: &[TxAppendResult],
) {
    let mut cache = idempotency.lock().await;
    for (tx, result) in req.txs.iter().zip(results) {
        if let Some(request_id) = tx.request_id.as_ref() {
            cache.insert(
                request_id.clone(),
                CachedResponse::Tx {
                    result: result.clone(),
                    mutation: Some(transaction_identity(tx, &req.rewrites)),
                    mutation_id: None,
                },
            );
        }
    }
    cache.insert(
        cache_key.to_string(),
        CachedResponse::Multi {
            results: results.to_vec(),
            mutation: req.mutation.clone(),
            durability: MultiDurability::Durable,
        },
    );
}

fn publish_multi_events(events: &EventBus, txs: &[TxAppend], results: &[TxAppendResult]) {
    for (tx, result) in txs.iter().zip(results) {
        events.publish(
            Topic::Daemon,
            EventPayload::TxAppended {
                project_id: tx.project_id.clone(),
                tx_id: result.tx_id.clone(),
                ty: tx.entry.ty.clone(),
            },
        );
    }
}

fn process_tx_batch(
    handles: &mut HashMap<PathBuf, CachedTxWriter>,
    batch: Vec<(TxAppend, oneshot::Sender<Result<TxAppendResult>>)>,
) -> Vec<(
    TxAppend,
    Result<TxAppendResult>,
    oneshot::Sender<Result<TxAppendResult>>,
)> {
    let mut pending: Vec<PendingTxBatchItem> = batch
        .into_iter()
        .map(|(req, reply)| PendingTxBatchItem {
            req,
            reply,
            result: Err(anyhow!("tx append not executed")),
        })
        .collect();

    let mut paths_to_sync = HashSet::new();
    let paths_in_batch: HashSet<PathBuf> = pending
        .iter()
        .map(|item| item.req.tx_path.clone())
        .collect();
    tx_handles_detached_from_paths(handles, &paths_in_batch);
    for item in &mut pending {
        item.result = (|| -> Result<TxAppendResult> {
            let entry = prepare_tx_entry(&item.req)?;
            let res = write_tx_append(handles, &item.req.tx_path, &entry)?;
            paths_to_sync.insert(item.req.tx_path.clone());
            Ok(res)
        })();
    }

    let mut sync_failed_paths = HashSet::new();
    for path in &paths_to_sync {
        if let Err(e) = sync_tx_writer(handles, path) {
            sync_failed_paths.insert(path.clone());
            warn!(path = %path.display(), error = %e, "tx append fsync failed");
        }
    }

    for item in &mut pending {
        if sync_failed_paths.contains(&item.req.tx_path) && item.result.is_ok() {
            item.result = Err(anyhow!(
                "tx append fsync failed for {}",
                item.req.tx_path.display()
            ));
        }
    }

    pending
        .into_iter()
        .map(|item| (item.req, item.result, item.reply))
        .collect()
}

struct CachedTxWriter {
    writer: LedgerWriter,
    identity: FileIdentity,
    event_ids: HashMap<String, String>,
}

enum LedgerWriter {
    Tx(TxWriter),
    Journal(File),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct FileIdentity {
    #[cfg(unix)]
    dev: u64,
    #[cfg(unix)]
    ino: u64,
    #[cfg(not(unix))]
    len: u64,
    #[cfg(not(unix))]
    modified: Option<std::time::SystemTime>,
}

impl FileIdentity {
    fn from_path(path: &Path) -> Result<Self> {
        let metadata =
            std::fs::metadata(path).with_context(|| format!("stat tx path {}", path.display()))?;
        #[cfg(unix)]
        {
            Ok(Self {
                dev: metadata.dev(),
                ino: metadata.ino(),
            })
        }
        #[cfg(not(unix))]
        {
            Ok(Self {
                len: metadata.len(),
                modified: metadata.modified().ok(),
            })
        }
    }
}

impl CachedTxWriter {
    fn open(path: &Path) -> Result<Self> {
        let writer = if path.file_name().and_then(|name| name.to_str()) == Some("journal.org") {
            if let Some(parent) = path.parent() {
                std::fs::create_dir_all(parent)
                    .with_context(|| format!("create {}", parent.display()))?;
            }
            let mut file = OpenOptions::new()
                .create(true)
                .append(true)
                .read(true)
                .open(path)
                .with_context(|| format!("open {}", path.display()))?;
            if file.metadata()?.len() == 0 {
                let node_id = path
                    .parent()
                    .and_then(Path::file_name)
                    .and_then(|name| name.to_str())
                    .ok_or_else(|| anyhow!("journal path has no node id: {}", path.display()))?;
                file.write_all(orgasmic_core::node_kernel::journal_header(node_id).as_bytes())?;
            }
            LedgerWriter::Journal(file)
        } else {
            LedgerWriter::Tx(
                TxWriter::open(path).with_context(|| format!("open {}", path.display()))?,
            )
        };
        let identity = FileIdentity::from_path(path)?;
        let event_ids = read_event_ids(path)?;
        Ok(Self {
            writer,
            identity,
            event_ids,
        })
    }

    fn append(&mut self, entry: &TxEntry) -> Result<String> {
        let event_id = event_id(entry);
        if let Some(existing) = self.event_ids.get(event_id) {
            return Ok(existing.clone());
        }
        match &mut self.writer {
            LedgerWriter::Tx(writer) => writer.append(entry)?,
            LedgerWriter::Journal(file) => {
                let entry = journal_entry(entry);
                entry.validate()?;
                reject_journal_prose(&entry.body)?;
                file.write_all(orgasmic_core::node_kernel::journal_entry_block(&entry).as_bytes())?;
            }
        }
        self.event_ids
            .insert(event_id.to_string(), entry.tx_id.clone());
        Ok(entry.tx_id.clone())
    }

    fn append_many(&mut self, entries: &[TxEntry]) -> Result<Vec<String>> {
        let mut ids = Vec::with_capacity(entries.len());
        for entry in entries {
            ids.push(self.append(entry)?);
        }
        Ok(ids)
    }

    fn sync_data(&self) -> Result<()> {
        match &self.writer {
            LedgerWriter::Tx(writer) => writer.sync_data().map_err(Into::into),
            LedgerWriter::Journal(file) => file.sync_data().map_err(Into::into),
        }
    }

    fn path_still_names_writer(&self, path: &Path) -> bool {
        FileIdentity::from_path(path)
            .map(|current| current == self.identity)
            .unwrap_or(false)
    }
}

fn event_id(entry: &TxEntry) -> &str {
    entry
        .extra
        .iter()
        .find(|(key, _)| key == "EVENT_ID")
        .map(|(_, value)| value.as_str())
        .unwrap_or(&entry.tx_id)
}

fn read_event_ids(path: &Path) -> Result<HashMap<String, String>> {
    let source =
        std::fs::read_to_string(path).with_context(|| format!("read {}", path.display()))?;
    let pairs: Vec<(String, String)> =
        if path.file_name().and_then(|name| name.to_str()) == Some("journal.org") {
            orgasmic_core::node_kernel::parse_journal(&source, &path.to_string_lossy())?
                .into_iter()
                .map(|entry| {
                    (
                        entry
                            .extra("EVENT_ID")
                            .unwrap_or(&entry.entry_id)
                            .to_string(),
                        entry.entry_id,
                    )
                })
                .collect()
        } else {
            parse_tx_file(&source, &path.to_string_lossy())?
                .into_iter()
                .map(|entry| (event_id(&entry).to_string(), entry.tx_id))
                .collect()
        };
    Ok(pairs.into_iter().collect())
}

fn journal_entry(entry: &TxEntry) -> orgasmic_core::node_kernel::JournalEntry {
    let mut extras = Vec::new();
    for (key, value) in [
        ("PROJECT", entry.project.as_deref()),
        ("TASK", entry.task.as_deref()),
        ("TARGET", entry.target.as_deref()),
        ("REASON", entry.reason.as_deref()),
    ] {
        if let Some(value) = value {
            extras.push((key.to_string(), value.to_string()));
        }
    }
    let body = entry
        .extra
        .iter()
        .find(|(key, _)| key == "BODY")
        .map(|(_, value)| unescape_property_value(value))
        .unwrap_or_default();
    extras.extend(entry.extra.iter().filter(|(key, _)| key != "BODY").cloned());
    orgasmic_core::node_kernel::JournalEntry {
        entry_id: entry.tx_id.clone(),
        time: entry.time.clone(),
        ty: entry.ty.clone(),
        actor: entry.actor.clone(),
        machine: entry.machine.clone(),
        extras,
        body,
    }
}

fn unescape_property_value(value: &str) -> String {
    let mut out = String::new();
    let mut chars = value.chars();
    while let Some(ch) = chars.next() {
        match (ch, chars.clone().next()) {
            ('\\', Some('n')) => {
                chars.next();
                out.push('\n');
            }
            ('\\', Some('\\')) => {
                chars.next();
                out.push('\\');
            }
            _ => out.push(ch),
        }
    }
    out
}

fn tx_handles_detached_from_paths(
    handles: &mut HashMap<PathBuf, CachedTxWriter>,
    paths: &HashSet<PathBuf>,
) {
    let mut detached = Vec::new();
    for path in paths {
        if let Some(handle) = handles.get(path) {
            if !handle.path_still_names_writer(path) {
                detached.push(path.clone());
            }
        }
    }
    for path in detached {
        handles.remove(&path);
        warn!(
            path = %path.display(),
            "tx append handle no longer matches path; reopening"
        );
    }
}

fn prepare_tx_entry(req: &TxAppend) -> Result<TxEntry> {
    let mut entry = req.entry.clone();
    if let TxIdPolicy::ProjectSequence { project_id, date } = &req.tx_id_policy {
        entry.tx_id = format!(
            "tx-{date}-{}-{}",
            project_tx_slug(project_id),
            Uuid::new_v4()
        );
    }
    let supplied: Vec<_> = entry
        .extra
        .iter()
        .filter(|(key, _)| key == "EVENT_ID")
        .map(|(_, value)| value.as_str())
        .collect();
    match supplied.as_slice() {
        [] => entry
            .extra
            .push(("EVENT_ID".to_string(), Uuid::new_v4().to_string())),
        [id] => {
            Uuid::parse_str(id).context("EVENT_ID is not a UUID")?;
        }
        _ => bail!("event has duplicate EVENT_ID properties"),
    }
    Ok(entry)
}

fn write_tx_append(
    handles: &mut HashMap<PathBuf, CachedTxWriter>,
    tx_path: &Path,
    entry: &TxEntry,
) -> Result<TxAppendResult> {
    if let Some(parent) = tx_path.parent() {
        std::fs::create_dir_all(parent).with_context(|| format!("create {}", parent.display()))?;
    }
    let writer = match handles.get_mut(tx_path) {
        Some(w) => w,
        None => {
            let w = CachedTxWriter::open(tx_path)?;
            handles.insert(tx_path.to_path_buf(), w);
            handles.get_mut(tx_path).expect("just inserted")
        }
    };
    let tx_id = writer
        .append(entry)
        .with_context(|| format!("append to {}", tx_path.display()))?;
    Ok(TxAppendResult {
        tx_id,
        tx_path: tx_path.to_path_buf(),
    })
}

fn sync_tx_writer(handles: &HashMap<PathBuf, CachedTxWriter>, path: &Path) -> Result<()> {
    test_hooks::before_sync(path)?;
    let writer = handles
        .get(path)
        .ok_or_else(|| anyhow!("no cached tx writer for {}", path.display()))?;
    writer
        .sync_data()
        .with_context(|| format!("fsync {}", path.display()))?;
    test_hooks::after_sync();
    Ok(())
}

fn append_txs_inner(
    handles: &mut HashMap<PathBuf, CachedTxWriter>,
    reqs: &[TxAppend],
) -> Result<Vec<TxAppendResult>> {
    let first = reqs
        .first()
        .ok_or_else(|| anyhow!("multi transaction requires at least one tx"))?;
    if reqs.iter().any(|req| req.tx_path != first.tx_path) {
        bail!("multi transaction tx entries must target one ledger");
    }
    let paths = HashSet::from([first.tx_path.clone()]);
    tx_handles_detached_from_paths(handles, &paths);
    let entries = reqs
        .iter()
        .map(prepare_tx_entry)
        .collect::<Result<Vec<_>>>()?;
    if let Some(parent) = first.tx_path.parent() {
        std::fs::create_dir_all(parent).with_context(|| format!("create {}", parent.display()))?;
    }
    let writer = match handles.get_mut(&first.tx_path) {
        Some(writer) => writer,
        None => {
            let writer = CachedTxWriter::open(&first.tx_path)?;
            handles.insert(first.tx_path.clone(), writer);
            handles.get_mut(&first.tx_path).expect("just inserted")
        }
    };
    let tx_ids = writer
        .append_many(&entries)
        .with_context(|| format!("append to {}", first.tx_path.display()))?;
    Ok(tx_ids
        .into_iter()
        .map(|tx_id| TxAppendResult {
            tx_id,
            tx_path: first.tx_path.clone(),
        })
        .collect())
}

fn project_tx_slug(project_id: &str) -> String {
    let raw = project_id.split('-').next().unwrap_or(project_id);
    let slug: String = raw
        .chars()
        .filter(|c| c.is_ascii_alphanumeric() || *c == '_')
        .collect();
    if slug.is_empty() {
        "proj".to_string()
    } else {
        slug
    }
}

#[cfg_attr(test, allow(clippy::too_many_arguments))]
fn append_session_inner(
    handles: &mut HashMap<String, SessionWriter>,
    run_id: &str,
    session_path: &Path,
    identity: RuntimeIdentity,
    authority: Option<crate::recovery_claim::SessionFile>,
    kind: SessionEventKind,
    event: Value,
    #[cfg(test)] injected_failure: Option<Arc<test_hooks::SessionAppendFailure>>,
) -> Result<SessionAppendResult> {
    let writer = match handles.get_mut(run_id) {
        Some(w) => w,
        None => {
            let w = if let Some(authority) = authority {
                if !authority
                    .authorizes_path(session_path)
                    .map_err(|err| anyhow!("authorized session path check failed: {err:?}"))?
                {
                    bail!("authorized session path changed before first append");
                }
                let file = authority
                    .clone_file_for_append()
                    .map_err(|err| anyhow!("authorized session open failed: {err:?}"))?;
                SessionWriter::from_file(session_path.to_path_buf(), file, identity)
            } else {
                SessionWriter::open(session_path, identity)
                    .with_context(|| format!("open session {}", session_path.display()))?
            };
            handles.insert(run_id.to_string(), w);
            handles.get_mut(run_id).expect("just inserted")
        }
    };
    #[cfg(test)]
    if let Some(injected_failure) = injected_failure {
        injected_failure.fail()?;
    }
    let seq = writer
        .append(kind, event)
        .with_context(|| format!("append session {}", session_path.display()))?;
    Ok(SessionAppendResult { seq })
}

fn mutate_file_inner(req: FileMutate) -> Result<()> {
    validate_rewrite_path(&req.path)?;
    if let Some(parent) = req.path.parent() {
        std::fs::create_dir_all(parent).with_context(|| format!("create {}", parent.display()))?;
    }
    let file = std::fs::OpenOptions::new()
        .create(true)
        .read(true)
        .write(true)
        .truncate(false)
        .open(&req.path)
        .with_context(|| format!("open {}", req.path.display()))?;
    FileExt::try_lock_exclusive(&file)
        .with_context(|| format!("flock contention on {}", req.path.display()))?;
    let result = (|| -> Result<()> {
        let source = std::fs::read_to_string(&req.path)
            .with_context(|| format!("read {}", req.path.display()))?;
        let new_contents = (req.transform)(&source)?;
        let mut tmp = req.path.clone();
        let mut name = tmp
            .file_name()
            .ok_or_else(|| anyhow!("mutate target has no filename"))?
            .to_os_string();
        name.push(".tmp");
        tmp.set_file_name(name);
        std::fs::write(&tmp, &new_contents).with_context(|| format!("write {}", tmp.display()))?;
        std::fs::rename(&tmp, &req.path)
            .with_context(|| format!("rename {} -> {}", tmp.display(), req.path.display()))?;
        Ok(())
    })();
    if let Err(e) = FileExt::unlock(&file) {
        warn!(path = %req.path.display(), error = %e, "flock unlock failed");
    }
    result
}

fn rewrite_file_inner(req: &FileRewrite) -> Result<()> {
    if let Some(parent) = req.path.parent() {
        std::fs::create_dir_all(parent).with_context(|| format!("create {}", parent.display()))?;
    }
    let file = std::fs::OpenOptions::new()
        .create(true)
        .read(true)
        .write(true)
        .truncate(false)
        .open(&req.path)
        .with_context(|| format!("open {}", req.path.display()))?;
    FileExt::try_lock_exclusive(&file)
        .with_context(|| format!("flock contention on {}", req.path.display()))?;
    let result = (|| -> Result<()> {
        let mut tmp = req.path.clone();
        let mut name = tmp
            .file_name()
            .ok_or_else(|| anyhow!("rewrite target has no filename"))?
            .to_os_string();
        name.push(".tmp");
        tmp.set_file_name(name);
        std::fs::write(&tmp, &req.new_contents)
            .with_context(|| format!("write {}", tmp.display()))?;
        std::fs::rename(&tmp, &req.path)
            .with_context(|| format!("rename {} -> {}", tmp.display(), req.path.display()))?;
        Ok(())
    })();
    if let Err(e) = FileExt::unlock(&file) {
        warn!(path = %req.path.display(), error = %e, "flock unlock failed");
    }
    result
}

#[derive(Debug)]
struct StagedRewrite {
    target: PathBuf,
    tmp: PathBuf,
    backup: Option<PathBuf>,
}

enum MultiTransactionCommit {
    Durable(Vec<TxAppendResult>),
    SyncUncertain {
        results: Vec<TxAppendResult>,
        error: anyhow::Error,
    },
}

fn transaction_inner<F>(
    handles: &mut HashMap<PathBuf, CachedTxWriter>,
    rewrites: &[FileRewrite],
    tx: TxAppend,
    request_id: &str,
    verify_before_commit: F,
) -> Result<TxAppendResult>
where
    F: FnOnce() -> Result<()>,
{
    let mut results = transaction_multi_inner(
        handles,
        rewrites,
        std::slice::from_ref(&tx),
        request_id,
        false,
        verify_before_commit,
    )?;
    match &mut results {
        MultiTransactionCommit::Durable(results) => Ok(results.remove(0)),
        MultiTransactionCommit::SyncUncertain { .. } => Err(anyhow!(
            "single transaction unexpectedly retained after sync failure"
        )),
    }
}

fn transaction_multi_inner<F>(
    handles: &mut HashMap<PathBuf, CachedTxWriter>,
    rewrites: &[FileRewrite],
    txs: &[TxAppend],
    request_id: &str,
    retain_rewrites_after_append: bool,
    verify_before_commit: F,
) -> Result<MultiTransactionCommit>
where
    F: FnOnce() -> Result<()>,
{
    if txs.is_empty() {
        bail!("multi transaction requires at least one tx");
    }
    let tx_path = &txs[0].tx_path;
    if txs.iter().any(|tx| tx.tx_path != *tx_path) {
        bail!("multi transaction tx entries must target one ledger");
    }
    let mut request_ids = HashSet::new();
    for tx in txs {
        if let Some(id) = tx.request_id.as_ref() {
            if !request_ids.insert(id) {
                bail!("duplicate request_id in multi transaction: {id}");
            }
        }
    }
    reject_duplicate_rewrites(rewrites)?;
    let mut locks = Vec::new();
    let locked = (|| -> Result<()> {
        for rewrite in rewrites {
            validate_rewrite_path(&rewrite.path)?;
            if rewrite.path.exists() {
                let file = OpenOptions::new()
                    .read(true)
                    .write(true)
                    .open(&rewrite.path)
                    .with_context(|| format!("open {}", rewrite.path.display()))?;
                FileExt::try_lock_exclusive(&file)
                    .with_context(|| format!("flock contention on {}", rewrite.path.display()))?;
                test_hooks::record_flock();
                locks.push((rewrite.path.clone(), file));
            }
        }
        Ok(())
    })();
    let result = locked.and_then(|()| {
        transaction_multi_locked_inner(
            handles,
            rewrites,
            txs,
            request_id,
            retain_rewrites_after_append,
            verify_before_commit,
        )
    });
    for (path, file) in locks {
        if let Err(error) = FileExt::unlock(&file) {
            warn!(path = %path.display(), error = %error, "flock unlock failed");
        }
    }
    result
}

fn transaction_mutate_file_inner(
    handles: &mut HashMap<PathBuf, CachedTxWriter>,
    req: FileMutate,
    tx: TxAppend,
    request_id: &str,
) -> Result<TxAppendResult> {
    validate_rewrite_path(&req.path)?;
    if let Some(parent) = req.path.parent() {
        std::fs::create_dir_all(parent).with_context(|| format!("create {}", parent.display()))?;
    }
    let file = OpenOptions::new()
        .create(true)
        .read(true)
        .write(true)
        .truncate(false)
        .open(&req.path)
        .with_context(|| format!("open {}", req.path.display()))?;
    FileExt::try_lock_exclusive(&file)
        .with_context(|| format!("flock contention on {}", req.path.display()))?;
    let result = (|| -> Result<TxAppendResult> {
        let source = std::fs::read_to_string(&req.path)
            .with_context(|| format!("read {}", req.path.display()))?;
        let rewrite = FileRewrite {
            path: req.path.clone(),
            new_contents: (req.transform)(&source)?,
        };
        transaction_locked_inner(
            handles,
            std::slice::from_ref(&rewrite),
            tx,
            request_id,
            || Ok(()),
        )
    })();
    if let Err(error) = FileExt::unlock(&file) {
        warn!(path = %req.path.display(), error = %error, "flock unlock failed");
    }
    result
}

fn transaction_locked_inner<F>(
    handles: &mut HashMap<PathBuf, CachedTxWriter>,
    rewrites: &[FileRewrite],
    tx: TxAppend,
    request_id: &str,
    verify_before_commit: F,
) -> Result<TxAppendResult>
where
    F: FnOnce() -> Result<()>,
{
    let mut results = transaction_multi_locked_inner(
        handles,
        rewrites,
        std::slice::from_ref(&tx),
        request_id,
        false,
        verify_before_commit,
    )?;
    match &mut results {
        MultiTransactionCommit::Durable(results) => Ok(results.remove(0)),
        MultiTransactionCommit::SyncUncertain { .. } => Err(anyhow!(
            "single transaction unexpectedly retained after sync failure"
        )),
    }
}

fn transaction_multi_locked_inner<F>(
    handles: &mut HashMap<PathBuf, CachedTxWriter>,
    rewrites: &[FileRewrite],
    txs: &[TxAppend],
    request_id: &str,
    retain_rewrites_after_append: bool,
    verify_before_commit: F,
) -> Result<MultiTransactionCommit>
where
    F: FnOnce() -> Result<()>,
{
    let mut staged = Vec::new();
    let result = (|| -> Result<MultiTransactionCommit> {
        for rewrite in rewrites {
            if let Some(parent) = rewrite.path.parent() {
                std::fs::create_dir_all(parent)
                    .with_context(|| format!("create {}", parent.display()))?;
            }
            let tmp = transaction_tmp_path(&rewrite.path, request_id)?;
            let mut file = OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&tmp)
                .with_context(|| format!("create {}", tmp.display()))?;
            staged.push(StagedRewrite {
                target: rewrite.path.clone(),
                tmp: tmp.clone(),
                backup: None,
            });
            file.write_all(&rewrite.new_contents)
                .with_context(|| format!("write {}", tmp.display()))?;
            file.sync_all()
                .with_context(|| format!("fsync {}", tmp.display()))?;
        }
        for rewrite in &mut staged {
            if rewrite.target.exists() {
                let backup = transaction_backup_path(&rewrite.target, request_id)?;
                let mut source = OpenOptions::new()
                    .read(true)
                    .open(&rewrite.target)
                    .with_context(|| format!("open {}", rewrite.target.display()))?;
                let mut backup_file = OpenOptions::new()
                    .write(true)
                    .create_new(true)
                    .open(&backup)
                    .with_context(|| format!("create {}", backup.display()))?;
                rewrite.backup = Some(backup.clone());
                std::io::copy(&mut source, &mut backup_file).with_context(|| {
                    format!("copy {} -> {}", rewrite.target.display(), backup.display())
                })?;
                backup_file
                    .sync_all()
                    .with_context(|| format!("fsync {}", backup.display()))?;
            }
        }
        verify_before_commit()?;
        let mut renamed = Vec::new();
        for (idx, rewrite) in staged.iter().enumerate() {
            if let Err(error) = std::fs::rename(&rewrite.tmp, &rewrite.target).with_context(|| {
                format!(
                    "rename {} -> {}",
                    rewrite.tmp.display(),
                    rewrite.target.display()
                )
            }) {
                rollback_renamed_rewrites(&staged, &renamed);
                return Err(error);
            }
            renamed.push(idx);
        }
        let appended = match append_txs_inner(handles, txs) {
            Ok(appended) => appended,
            Err(error) => {
                rollback_renamed_rewrites(&staged, &renamed);
                return Err(error);
            }
        };
        if let Err(error) = sync_tx_writer(handles, &appended[0].tx_path) {
            if retain_rewrites_after_append {
                return Ok(MultiTransactionCommit::SyncUncertain {
                    results: appended,
                    error,
                });
            }
            rollback_renamed_rewrites(&staged, &renamed);
            return Err(error);
        }
        Ok(MultiTransactionCommit::Durable(appended))
    })();
    if result.is_err() {
        cleanup_staged_rewrites(&staged);
    } else {
        cleanup_committed_backups(&staged);
    }
    result
}

fn reject_duplicate_rewrites(rewrites: &[FileRewrite]) -> Result<()> {
    for (idx, rewrite) in rewrites.iter().enumerate() {
        if rewrites[..idx]
            .iter()
            .any(|prior| prior.path == rewrite.path)
        {
            bail!("duplicate rewrite target: {}", rewrite.path.display());
        }
    }
    Ok(())
}

fn transaction_tmp_path(path: &Path, request_id: &str) -> Result<PathBuf> {
    transaction_sidecar_path(path, "tmp", request_id)
}

fn transaction_backup_path(path: &Path, request_id: &str) -> Result<PathBuf> {
    transaction_sidecar_path(path, "bak", request_id)
}

fn transaction_sidecar_path(path: &Path, kind: &str, request_id: &str) -> Result<PathBuf> {
    let mut tmp = path.to_path_buf();
    let mut name = tmp
        .file_name()
        .ok_or_else(|| anyhow!("rewrite target has no filename"))?
        .to_os_string();
    name.push(".");
    name.push(kind);
    name.push(".");
    name.push(safe_request_id(request_id));
    tmp.set_file_name(name);
    Ok(tmp)
}

fn safe_request_id(request_id: &str) -> String {
    request_id
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '_' || c == '.' {
                c
            } else {
                '_'
            }
        })
        .collect()
}

fn cleanup_staged_rewrites(staged: &[StagedRewrite]) {
    for rewrite in staged {
        remove_transaction_sidecar(&rewrite.tmp, "transaction tmp cleanup failed");
        if let Some(backup) = &rewrite.backup {
            remove_transaction_sidecar(backup, "transaction backup cleanup failed");
        }
    }
}

fn cleanup_committed_backups(staged: &[StagedRewrite]) {
    for rewrite in staged {
        if let Some(backup) = &rewrite.backup {
            remove_transaction_sidecar(backup, "transaction backup cleanup failed");
        }
    }
}

fn rollback_renamed_rewrites(staged: &[StagedRewrite], renamed: &[usize]) {
    for idx in renamed.iter().rev() {
        let rewrite = &staged[*idx];
        if let Some(backup) = &rewrite.backup {
            if let Err(e) = std::fs::rename(backup, &rewrite.target) {
                warn!(
                    target = %rewrite.target.display(),
                    backup = %backup.display(),
                    error = %e,
                    "transaction rollback restore failed"
                );
            }
        } else {
            remove_transaction_sidecar(&rewrite.target, "transaction rollback remove failed");
        }
    }
}

fn remove_transaction_sidecar(path: &Path, message: &'static str) {
    match std::fs::remove_file(path) {
        Ok(()) => {}
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => warn!(path = %path.display(), error = %e, "{}", message),
    }
}

/// Convenience for callers that want to verify the rewrite payload first.
pub fn validate_rewrite_path(path: &Path) -> Result<()> {
    if path.is_dir() {
        bail!("rewrite target is a directory: {}", path.display());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use orgasmic_core::tx::TxEntry;

    fn sample_entry(tx_id: &str) -> TxEntry {
        let mut e = TxEntry::new(
            tx_id,
            "manager.action",
            "[2026-05-21 Thu 21:00:00]",
            "dev@example.com",
            "host.local",
        );
        e.project = Some("orgasmic".into());
        e.reason = Some("test".into());
        e
    }

    #[tokio::test]
    async fn tx_append_round_trip() {
        let tmp = tempfile::tempdir().unwrap();
        let tx_path = tmp.path().join("tx").join("2026-05.org");
        let bus = EventBus::new();
        let handle = spawn(bus);
        let req = TxAppend {
            tx_path: tx_path.clone(),
            entry: sample_entry("tx-test-1"),
            project_id: Some("orgasmic".into()),
            tx_id_policy: TxIdPolicy::Preserve,
            request_id: None,
        };
        let res = handle
            .append_tx(req, Some("req-1".into()))
            .await
            .expect("append");
        assert_eq!(res.tx_id, "tx-test-1");
        let source = std::fs::read_to_string(&tx_path).unwrap();
        assert!(source.contains("tx-test-1"));
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn barrier_runs_before_an_append_queued_during_it() {
        let tmp = tempfile::tempdir().unwrap();
        let tx_path = tmp.path().join("tx/2026-09.org");
        let handle = spawn(EventBus::new());
        let (entered_tx, entered_rx) = std::sync::mpsc::sync_channel(1);
        let (release_tx, release_rx) = std::sync::mpsc::sync_channel(1);
        let barrier_handle = handle.clone();
        let reset_path = tx_path.clone();
        let barrier = tokio::spawn(async move {
            barrier_handle
                .run_barrier(move || {
                    entered_tx.send(()).unwrap();
                    release_rx.recv().unwrap();
                    std::fs::create_dir_all(reset_path.parent().unwrap()).unwrap();
                    std::fs::write(reset_path, "#+title: reset tx\n#+orgasmic_version: 1\n")
                        .unwrap();
                })
                .await
                .unwrap();
        });
        tokio::task::spawn_blocking(move || entered_rx.recv().unwrap())
            .await
            .unwrap();

        let append_handle = handle.clone();
        let append_path = tx_path.clone();
        let append = tokio::spawn(async move {
            append_handle
                .append_tx(
                    TxAppend {
                        tx_path: append_path,
                        entry: sample_entry("tx-after-reset"),
                        project_id: Some("orgasmic".into()),
                        tx_id_policy: TxIdPolicy::Preserve,
                        request_id: None,
                    },
                    None,
                )
                .await
        });
        tokio::time::timeout(std::time::Duration::from_secs(1), async {
            while handle.status().queue_depth != 1 {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();

        release_tx.send(()).unwrap();
        barrier.await.unwrap();
        append.await.unwrap().unwrap();
        let tx = parse_tx_file(&std::fs::read_to_string(tx_path).unwrap(), "barrier tx").unwrap();
        assert_eq!(tx.len(), 1);
        assert_eq!(tx[0].tx_id, "tx-after-reset");
    }

    #[tokio::test]
    async fn panicking_barrier_does_not_stop_the_writer() {
        let handle = spawn(EventBus::new());

        assert!(handle
            .run_barrier(|| panic!("injected panic"))
            .await
            .is_err());
        assert_eq!(handle.run_barrier(|| 7).await.unwrap(), 7);
    }

    #[tokio::test]
    async fn duplicate_event_id_is_a_persistent_no_op() {
        let tmp = tempfile::tempdir().unwrap();
        let tx_path = tmp.path().join("tx/2026-08.org");
        let handle = spawn(EventBus::new());
        let submitted_event_id = Uuid::new_v4().to_string();
        let append = |tx_id: &str| {
            let mut entry = sample_entry(tx_id);
            entry
                .extra
                .push(("EVENT_ID".to_string(), submitted_event_id.clone()));
            TxAppend {
                tx_path: tx_path.clone(),
                entry,
                project_id: Some("orgasmic".into()),
                tx_id_policy: TxIdPolicy::Preserve,
                request_id: None,
            }
        };
        let first = handle.append_tx(append("tx-first"), None).await.unwrap();
        let duplicate = handle.append_tx(append("tx-retry"), None).await.unwrap();

        assert_eq!(duplicate.tx_id, first.tx_id);
        let entries =
            parse_tx_file(&std::fs::read_to_string(tx_path).unwrap(), "2026-08.org").unwrap();
        assert_eq!(entries.len(), 1);
        assert_eq!(event_id(&entries[0]), submitted_event_id);
    }

    /// orgasmic:TASK-Q07Y5 — the defect TASK-WGXKD.2's reviewer named: shutdown
    /// queued behind a write blocked in the writer task and waited forever, so
    /// the SIGTERM path had an unbounded term and no kill timeout could be
    /// proven larger than it. The stall here is a blocking `sleep` *inside* the
    /// writer task, which is what a blocked `fsync` actually is.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn shutdown_gives_up_on_its_budget_and_names_the_write_it_blocked_on() {
        let tmp = tempfile::tempdir().unwrap();
        let stalled = tmp.path().join("stalled-write.org");
        let bus = EventBus::new();
        let handle = spawn(bus);

        let stalling = handle.clone();
        let stalled_path = stalled.clone();
        tokio::spawn(async move {
            stalling
                .mutate_file(FileMutate {
                    path: stalled_path,
                    transform: Box::new(|_| {
                        std::thread::sleep(std::time::Duration::from_secs(3));
                        Ok(b"never observed\n".to_vec())
                    }),
                })
                .await
        });
        // Let the writer pick the mutate up, so shutdown is genuinely queued
        // behind a write in progress rather than racing to get in first.
        while handle.in_flight_write().is_none() {
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }

        let started = std::time::Instant::now();
        let outcome = handle
            .shutdown_within(std::time::Duration::from_millis(300))
            .await;
        let elapsed = started.elapsed();

        assert!(
            elapsed < std::time::Duration::from_secs(1),
            "shutdown must give up on its budget, not on the stalled write: {elapsed:?}"
        );
        let WriterShutdownOutcome::TimedOut { in_flight, .. } = outcome else {
            panic!("a writer stuck in a 3s write cannot report a clean shutdown: {outcome:?}");
        };
        let in_flight = in_flight.expect("the timed-out shutdown must name the write it waited on");
        assert_eq!(in_flight.kind, "mutate");
        assert_eq!(in_flight.path.as_deref(), Some(stalled.as_path()));
    }

    /// The bound must not cost anything in the normal case: an idle writer
    /// still reports a clean stop, which is what lets the caller distinguish
    /// "nothing was lost" from "something might have been".
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn shutdown_reports_clean_when_the_writer_is_not_stuck() {
        let tmp = tempfile::tempdir().unwrap();
        let bus = EventBus::new();
        let handle = spawn(bus);
        handle
            .append_tx(
                TxAppend {
                    tx_path: tmp.path().join("tx").join("2026-07.org"),
                    entry: sample_entry("tx-clean-shutdown"),
                    project_id: Some("orgasmic".into()),
                    tx_id_policy: TxIdPolicy::Preserve,
                    request_id: None,
                },
                None,
            )
            .await
            .expect("append");

        let outcome = handle
            .shutdown_within(std::time::Duration::from_secs(5))
            .await;

        assert_eq!(outcome, WriterShutdownOutcome::Clean);
        assert!(outcome.is_clean());
    }

    #[tokio::test]
    async fn lifecycle_session_append_publishes_run_lifecycle_event() {
        let tmp = tempfile::tempdir().unwrap();
        let bus = EventBus::new();
        let mut rx = bus.subscribe();
        let handle = spawn(bus);
        handle
            .append_session(SessionAppend {
                run_id: "run-lifecycle-test".into(),
                session_path: tmp.path().join("run-lifecycle-test.jsonl"),
                identity: RuntimeIdentity::new("run-lifecycle-test", "boot-test"),
                authority: None,
                kind: SessionEventKind::Lifecycle,
                event: serde_json::json!({
                    "phase": "release",
                    "reason": "test",
                    "outcome": "completed",
                }),
            })
            .await
            .expect("append");
        // The append publishes the firehose RunEvent first, then the
        // dedicated lifecycle signal.
        let first = rx.recv().await.expect("run event");
        assert!(matches!(first.payload, EventPayload::RunEvent { .. }));
        let second = rx.recv().await.expect("lifecycle event");
        match second.payload {
            EventPayload::RunLifecycle { run_id, phase } => {
                assert_eq!(run_id, "run-lifecycle-test");
                assert_eq!(phase, "release");
            }
            other => panic!("expected RunLifecycle, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn release_lifecycle_append_drops_the_session_handle() {
        let tmp = tempfile::tempdir().unwrap();
        let handle = spawn(EventBus::new());
        let append = |run: &str, event: serde_json::Value| SessionAppend {
            run_id: run.into(),
            session_path: tmp.path().join(format!("{run}.jsonl")),
            identity: RuntimeIdentity::new(run, "boot-test"),
            authority: None,
            kind: SessionEventKind::Lifecycle,
            event,
        };
        for run in ["run-a", "run-b"] {
            handle
                .append_session(append(
                    run,
                    serde_json::json!({"phase": "acquire", "task_id": "T", "kind": "worker", "worker_id": "w"}),
                ))
                .await
                .expect("acquire");
        }
        assert_eq!(
            handle.status().open_session_handles,
            2,
            "one handle per live run"
        );
        handle
            .append_session(append(
                "run-a",
                serde_json::json!({"phase": "release", "outcome": "completed", "reason": "done"}),
            ))
            .await
            .expect("release");
        assert_eq!(
            handle.status().open_session_handles,
            1,
            "the released run's handle is gone, the live run's stays"
        );
        // A late append after release reopens and re-holds; it must not panic
        // or lose the line.
        handle
            .append_session(append(
                "run-a",
                serde_json::json!({"phase": "run_meta", "transport": "tmux"}),
            ))
            .await
            .expect("late append");
        let text = std::fs::read_to_string(tmp.path().join("run-a.jsonl")).unwrap();
        assert_eq!(text.lines().count(), 3);
    }

    #[tokio::test]
    async fn deferred_session_append_counts_as_queued_until_it_executes_once() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("deferred.jsonl");
        let handle = spawn(EventBus::new());
        let lease = handle.lease_sessions(vec![path.clone()]).await.unwrap();
        let after_lease = handle.status();

        let append = {
            let handle = handle.clone();
            let path = path.clone();
            tokio::spawn(async move {
                handle
                    .append_session(SessionAppend {
                        run_id: "run-deferred-metrics".into(),
                        session_path: path,
                        identity: RuntimeIdentity::new("run-deferred-metrics", "boot-test"),
                        authority: None,
                        kind: SessionEventKind::Lifecycle,
                        event: serde_json::json!({"phase": "release"}),
                    })
                    .await
            })
        };
        tokio::time::timeout(std::time::Duration::from_secs(1), async {
            while handle.deferred_session_appends() != 1 {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        let deferred = handle.status();
        assert_eq!(deferred.queue_depth, 1, "{deferred:?}");
        assert_eq!(
            deferred.completed_total, after_lease.completed_total,
            "deferral is not completion"
        );

        lease.release().await;
        append.await.unwrap().unwrap();
        let completed = handle.status();
        assert_eq!(completed.queue_depth, 0, "{completed:?}");
        assert_eq!(
            completed.completed_total,
            after_lease.completed_total + 2,
            "release command and deferred append each complete once"
        );
    }

    #[tokio::test]
    async fn tx_append_is_idempotent_for_same_request_id() {
        let tmp = tempfile::tempdir().unwrap();
        let tx_path = tmp.path().join("2026-05.org");
        let bus = EventBus::new();
        let handle = spawn(bus);
        let req = TxAppend {
            tx_path: tx_path.clone(),
            entry: sample_entry("tx-test-2"),
            project_id: None,
            tx_id_policy: TxIdPolicy::Preserve,
            request_id: None,
        };
        let a = handle
            .append_tx(req.clone(), Some("req-dup".into()))
            .await
            .unwrap();
        let b = handle.append_tx(req, Some("req-dup".into())).await.unwrap();
        assert_eq!(a.tx_id, b.tx_id);
        let source = std::fs::read_to_string(&tx_path).unwrap();
        let count = source.matches("tx-test-2").count();
        assert_eq!(count, 1, "duplicate request_id must not double-append");
    }

    #[tokio::test]
    async fn tx_append_syncs_retained_descriptor_after_path_rename() {
        let tmp = tempfile::tempdir().unwrap();
        let tx_path = tmp.path().join("tx").join("2026-08.org");
        let renamed_path = tmp.path().join("tx").join("2026-08-renamed.org");
        let handle = spawn(EventBus::new());

        test_hooks::rename_tx_path_before_next_sync(&tx_path, &renamed_path);
        handle
            .append_tx(
                TxAppend {
                    tx_path: tx_path.clone(),
                    entry: sample_entry("tx-retained-descriptor"),
                    project_id: Some("orgasmic".into()),
                    tx_id_policy: TxIdPolicy::Preserve,
                    request_id: None,
                },
                Some("req-retained-descriptor".into()),
            )
            .await
            .expect("daemon writer append must acknowledge through its retained descriptor");

        assert!(
            !tx_path.exists(),
            "the hook must rename the pathname before sync"
        );
        let source = std::fs::read_to_string(&renamed_path).unwrap();
        assert!(source.contains("tx-retained-descriptor"));
    }

    #[tokio::test]
    async fn rewrite_transaction_syncs_retained_descriptor_after_path_rename() {
        let tmp = tempfile::tempdir().unwrap();
        let target = tmp.path().join("tasks.org");
        let tx_path = tmp.path().join("tx").join("2026-08.org");
        let renamed_path = tmp.path().join("tx").join("2026-08-renamed.org");
        std::fs::write(&target, "before\n").unwrap();
        let handle = spawn(EventBus::new());

        test_hooks::rename_tx_path_before_next_sync(&tx_path, &renamed_path);
        handle
            .transaction(
                vec![FileRewrite {
                    path: target.clone(),
                    new_contents: b"after\n".to_vec(),
                }],
                TxAppend {
                    tx_path: tx_path.clone(),
                    entry: sample_entry("tx-rewrite-retained-descriptor"),
                    project_id: Some("orgasmic".into()),
                    tx_id_policy: TxIdPolicy::Preserve,
                    request_id: Some("req-rewrite-retained-descriptor".into()),
                },
            )
            .await
            .expect("rewrite transaction must acknowledge through its retained descriptor");

        assert!(
            !tx_path.exists(),
            "the hook must rename the pathname before sync"
        );
        assert_eq!(std::fs::read_to_string(target).unwrap(), "after\n");
        let source = std::fs::read_to_string(&renamed_path).unwrap();
        assert!(source.contains("tx-rewrite-retained-descriptor"));
    }

    #[tokio::test]
    async fn malformed_transaction_is_request_local_and_writer_stays_live() {
        let tmp = tempfile::tempdir().unwrap();
        let tx_path = tmp.path().join("tx").join("2026-08.org");
        let handle = spawn(EventBus::new());
        let (reply, rx) = oneshot::channel();

        handle
            .tx
            .send(WriterCommand::Transaction {
                req: TransactionRequest {
                    rewrites: Vec::new(),
                    tx: TxAppend {
                        tx_path: tx_path.clone(),
                        entry: sample_entry("tx-missing-mutation-identity"),
                        project_id: Some("orgasmic".into()),
                        tx_id_policy: TxIdPolicy::Preserve,
                        request_id: Some("req-missing-mutation-identity".into()),
                    },
                    request_id: "req-missing-mutation-identity".into(),
                    mutation: None,
                    mutation_id: None,
                },
                reply,
            })
            .await
            .expect("queue malformed private writer command");
        let error = rx
            .await
            .expect("malformed transaction receives a reply")
            .expect_err("missing mutation identity must be rejected");
        assert_eq!(
            error.to_string(),
            "writer transaction lacks a mutation identity"
        );

        handle
            .append_tx(
                TxAppend {
                    tx_path: tx_path.clone(),
                    entry: sample_entry("tx-after-malformed-transaction"),
                    project_id: Some("orgasmic".into()),
                    tx_id_policy: TxIdPolicy::Preserve,
                    request_id: None,
                },
                Some("req-after-malformed-transaction".into()),
            )
            .await
            .expect("writer accepts the next command");
        let source = std::fs::read_to_string(tx_path).unwrap();
        assert!(source.contains("tx-after-malformed-transaction"));
    }

    #[tokio::test]
    async fn writer_accepts_a_command_after_cached_transaction_replay() {
        let tmp = tempfile::tempdir().unwrap();
        let target = tmp.path().join("tasks.org");
        let tx_path = tmp.path().join("tx").join("2026-08.org");
        std::fs::write(&target, "before\n").unwrap();
        let handle = spawn(EventBus::new());
        let request_id = "req-writer-level-replay".to_string();
        let rewrites = vec![FileRewrite {
            path: target.clone(),
            new_contents: b"committed\n".to_vec(),
        }];
        let tx = TxAppend {
            tx_path: tx_path.clone(),
            entry: sample_entry("tx-writer-level-replay"),
            project_id: Some("orgasmic".into()),
            tx_id_policy: TxIdPolicy::Preserve,
            request_id: Some(request_id.clone()),
        };
        let mutation = MutationIdentity::new("task.create", "orgasmic", "TASK-REPLAY");
        let mutation_id = "TASK-REPLAY".to_string();

        let first = handle
            .transaction_mutation(
                rewrites.clone(),
                tx.clone(),
                mutation.clone(),
                mutation_id.clone(),
            )
            .await
            .expect("initial transaction");
        let (reply, rx) = oneshot::channel();
        handle
            .tx
            .send(WriterCommand::Transaction {
                req: TransactionRequest {
                    rewrites,
                    tx,
                    request_id,
                    mutation: Some(mutation),
                    mutation_id: Some(mutation_id),
                },
                reply,
            })
            .await
            .expect("queue writer-level replay");
        let replay = rx
            .await
            .expect("writer-level replay reply")
            .expect("writer-level replay result");
        assert_eq!(replay.tx_id, first.tx_id);

        // This command queues behind the replay. The old `continue` arm exited
        // the writer loop after replying from cache, so this is the regression
        // assertion rather than another handle-level idempotency check.
        handle
            .rewrite_file(
                FileRewrite {
                    path: target.clone(),
                    new_contents: b"writer-still-live\n".to_vec(),
                },
                None,
            )
            .await
            .expect("writer accepts a subsequent command");
        assert_eq!(
            std::fs::read_to_string(&target).unwrap(),
            "writer-still-live\n"
        );
        assert_eq!(
            std::fs::read_to_string(&tx_path)
                .unwrap()
                .matches("tx-writer-level-replay")
                .count(),
            1,
            "cached replay must not append the transaction twice"
        );
    }

    #[tokio::test]
    async fn writer_accepts_a_command_after_cached_mutation_identity_error() {
        let tmp = tempfile::tempdir().unwrap();
        let target = tmp.path().join("tasks.org");
        let tx_path = tmp.path().join("tx").join("2026-08.org");
        std::fs::write(&target, "before\n").unwrap();
        let handle = spawn(EventBus::new());
        let request_id = "req-cached-without-mutation-id".to_string();
        let rewrites = vec![FileRewrite {
            path: target.clone(),
            new_contents: b"committed\n".to_vec(),
        }];
        let tx = TxAppend {
            tx_path,
            entry: sample_entry("tx-cached-without-mutation-id"),
            project_id: Some("orgasmic".into()),
            tx_id_policy: TxIdPolicy::Preserve,
            request_id: Some(request_id.clone()),
        };
        let mutation = transaction_identity(&tx, &rewrites);

        handle
            .transaction(rewrites.clone(), tx.clone())
            .await
            .expect("initial transaction without a mutation id");

        let (reply, rx) = oneshot::channel();
        handle
            .tx
            .send(WriterCommand::Transaction {
                req: TransactionRequest {
                    rewrites,
                    tx,
                    request_id,
                    mutation: Some(mutation),
                    mutation_id: Some("TASK-IDENTITY".into()),
                },
                reply,
            })
            .await
            .expect("queue the conflicting mutation replay");
        let error = rx
            .await
            .expect("writer replies to the conflicting replay")
            .expect_err("cached entry without a mutation id must be rejected");
        assert_eq!(
            error.to_string(),
            "cached mutation lacks its recorded identity"
        );

        handle
            .rewrite_file(
                FileRewrite {
                    path: target.clone(),
                    new_contents: b"writer-still-live-after-error\n".to_vec(),
                },
                None,
            )
            .await
            .expect("writer accepts a command after the per-request error");
        assert_eq!(
            std::fs::read_to_string(target).unwrap(),
            "writer-still-live-after-error\n"
        );
    }

    #[tokio::test]
    async fn project_sequence_policy_mints_uuid_for_node_journal() {
        let tmp = tempfile::tempdir().unwrap();
        let dotorg = tmp.path().join(".orgasmic");
        let bus = EventBus::new();
        let handle = spawn(bus);
        let target_journal = dotorg.join("tasks/TASK-Y/journal.org");
        let req = TxAppend {
            tx_path: target_journal.clone(),
            entry: sample_entry("placeholder"),
            project_id: Some("orgasmic".into()),
            tx_id_policy: TxIdPolicy::ProjectSequence {
                project_id: "orgasmic".into(),
                date: "20260601".into(),
            },
            request_id: None,
        };
        let res = handle
            .append_tx(req, Some("req-project-seq".into()))
            .await
            .unwrap();
        let uuid = res
            .tx_id
            .strip_prefix("tx-20260601-orgasmic-")
            .expect("date and project slug prefix");
        Uuid::parse_str(uuid).expect("UUID suffix");
        let source = std::fs::read_to_string(target_journal).unwrap();
        let entries = orgasmic_core::node_kernel::parse_journal(&source, "journal.org").unwrap();
        assert_eq!(entries[0].entry_id, res.tx_id);
        assert!(!source.contains("placeholder"));
    }

    #[tokio::test]
    async fn two_writers_cannot_mint_the_same_node_journal_tx_id() {
        let tmp = tempfile::tempdir().unwrap();
        let request = |machine: &str| TxAppend {
            tx_path: tmp
                .path()
                .join(machine)
                .join(".orgasmic/tasks/TASK-X/journal.org"),
            entry: sample_entry("placeholder"),
            project_id: Some("orgasmic".into()),
            tx_id_policy: TxIdPolicy::ProjectSequence {
                project_id: "orgasmic".into(),
                date: "20260901".into(),
            },
            request_id: None,
        };
        let a = spawn(EventBus::new());
        let b = spawn(EventBus::new());
        let (a, b) = tokio::join!(
            a.append_tx(request("machine-a"), None),
            b.append_tx(request("machine-b"), None)
        );
        let (a, b) = (a.unwrap().tx_id, b.unwrap().tx_id);

        assert_ne!(a, b);
        for tx_id in [a, b] {
            Uuid::parse_str(
                tx_id
                    .strip_prefix("tx-20260901-orgasmic-")
                    .expect("date and project slug prefix"),
            )
            .expect("UUID suffix");
        }
    }

    #[tokio::test]
    async fn session_append_writes_jsonl() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("run-x.jsonl");
        let bus = EventBus::new();
        let handle = spawn(bus);
        let req = SessionAppend {
            run_id: "run-x".into(),
            session_path: path.clone(),
            identity: RuntimeIdentity::new("run-x", "boot-1"),
            authority: None,
            kind: SessionEventKind::Lifecycle,
            event: serde_json::json!({"type": "acquire"}),
        };
        let res = handle.append_session(req).await.unwrap();
        assert_eq!(res.seq, 0);
        let raw = std::fs::read_to_string(&path).unwrap();
        assert!(raw.contains("acquire"));
    }

    #[tokio::test]
    async fn handle_local_session_append_failure_cannot_be_consumed_by_another_writer() {
        let tmp = tempfile::tempdir().unwrap();
        let target_path = tmp.path().join("target.jsonl");
        let armed_handle = spawn(EventBus::new());
        let other_handle = spawn(EventBus::new());
        let failure = armed_handle.fail_next_session_append("run-target", &target_path);

        // The tuple is intentionally identical. A process-global target map
        // lets this different writer consume the arm; a WriterHandle-local
        // hook leaves it untouched until the exact handle sends its command.
        other_handle
            .append_session(SessionAppend {
                run_id: "run-target".into(),
                session_path: target_path.clone(),
                identity: RuntimeIdentity::new("run-target", "boot-other"),
                authority: None,
                kind: SessionEventKind::Lifecycle,
                event: serde_json::json!({"phase": "acquire"}),
            })
            .await
            .expect("a distinct WriterHandle must not consume the armed seam");

        let err = armed_handle
            .append_session(SessionAppend {
                run_id: "run-target".into(),
                session_path: target_path.clone(),
                identity: RuntimeIdentity::new("run-target", "boot-test"),
                authority: None,
                kind: SessionEventKind::Lifecycle,
                event: serde_json::json!({"phase": "manager_terminal_claim"}),
            })
            .await
            .expect_err("the exact target must consume its own injected failure");
        assert!(err
            .to_string()
            .contains("injected session lifecycle append failure"));
        assert_eq!(failure.attempt_count(), 1);
        let raw = std::fs::read_to_string(&target_path).unwrap();
        assert!(raw.contains("\"phase\":\"acquire\""));
        assert!(
            !raw.contains("manager_terminal_claim"),
            "the armed handle may open its retained writer, but must not append its lifecycle envelope"
        );
    }

    #[tokio::test]
    async fn rewrite_replaces_file_atomically() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("notes.org");
        std::fs::write(&path, "old").unwrap();
        let bus = EventBus::new();
        let handle = spawn(bus);
        handle
            .rewrite_file(
                FileRewrite {
                    path: path.clone(),
                    new_contents: b"new".to_vec(),
                },
                Some("rw-1".into()),
            )
            .await
            .unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "new");
        // Idempotent: same request_id is a no-op.
        handle
            .rewrite_file(
                FileRewrite {
                    path: path.clone(),
                    new_contents: b"should-be-ignored".to_vec(),
                },
                Some("rw-1".into()),
            )
            .await
            .unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "new");
    }

    #[tokio::test]
    async fn journal_ops_append_edit_with_occ_and_tombstone() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("journal.org");
        let handle = spawn(EventBus::new());
        let entry = orgasmic_core::node_kernel::JournalEntry {
            entry_id: "tx-1".into(),
            time: "[2026-08-26 Wed 10:00:00]".into(),
            ty: "comment".into(),
            actor: "owner".into(),
            machine: "mac".into(),
            extras: vec![],
            body: "first\n\n** Detail\nnested".into(),
        };

        handle
            .append_journal_entry(path.clone(), "TASK-X".into(), entry)
            .await
            .unwrap();
        let parsed = orgasmic_core::node_kernel::parse_journal(
            &std::fs::read_to_string(&path).unwrap(),
            "journal.org",
        )
        .unwrap();
        assert_eq!(parsed[0].body, "first\n\n** Detail\nnested");

        let before_refusal = std::fs::read(&path).unwrap();
        let error = handle
            .edit_journal_comment(
                path.clone(),
                "tx-1".into(),
                parsed[0].body.clone(),
                "replacement\n* forged entry".into(),
                CommentMutationActor::Member("owner".into()),
                "[2026-08-26 Wed 10:01:00]".into(),
            )
            .await
            .unwrap_err();
        assert!(error.to_string().contains("column-0 `* `"));
        assert_eq!(std::fs::read(&path).unwrap(), before_refusal);

        handle
            .edit_journal_comment(
                path.clone(),
                "tx-1".into(),
                parsed[0].body.clone(),
                "edited".into(),
                CommentMutationActor::Member("owner".into()),
                "[2026-08-26 Wed 10:01:00]".into(),
            )
            .await
            .unwrap();
        let edited = std::fs::read_to_string(&path).unwrap();
        assert!(edited.contains(":EDITED_BY: owner"));
        assert!(edited.contains(":EDITED_AT: [2026-08-26 Wed 10:01:00]"));

        let stale = handle
            .tombstone_journal_comment(
                path.clone(),
                "tx-1".into(),
                parsed[0].body.clone(),
                CommentMutationActor::Member("owner".into()),
                "[2026-08-26 Wed 10:02:00]".into(),
            )
            .await
            .unwrap_err();
        assert!(stale.to_string().contains("changed since it was read"));

        handle
            .tombstone_journal_comment(
                path.clone(),
                "tx-1".into(),
                "edited".into(),
                CommentMutationActor::Member("owner".into()),
                "[2026-08-26 Wed 10:02:00]".into(),
            )
            .await
            .unwrap();
        let parsed = orgasmic_core::node_kernel::parse_journal(
            &std::fs::read_to_string(path).unwrap(),
            "journal.org",
        )
        .unwrap();
        assert_eq!(
            (parsed[0].ty.as_str(), parsed[0].body.as_str()),
            ("comment.deleted", "")
        );
        assert_eq!(parsed[0].extra("DELETED_BY"), Some("owner"));
        assert_eq!(
            parsed[0].extra("DELETED_AT"),
            Some("[2026-08-26 Wed 10:02:00]")
        );
    }

    #[test]
    fn transaction_cleans_staged_rewrites_when_verify_hook_fails() {
        let tmp = tempfile::tempdir().unwrap();
        let decision_path = tmp.path().join("decisions.org");
        let architecture_path = tmp.path().join("architecture.org");
        let tx_path = tmp.path().join("tx").join("2026-05.org");
        std::fs::write(&decision_path, "old decision").unwrap();
        std::fs::write(&architecture_path, "old architecture").unwrap();
        let rewrites = vec![
            FileRewrite {
                path: decision_path.clone(),
                new_contents: b"new decision".to_vec(),
            },
            FileRewrite {
                path: architecture_path.clone(),
                new_contents: b"new architecture".to_vec(),
            },
        ];
        let tx = TxAppend {
            tx_path: tx_path.clone(),
            entry: sample_entry("tx-test-rollback"),
            project_id: Some("orgasmic".into()),
            tx_id_policy: TxIdPolicy::Preserve,
            request_id: Some("req-rollback".into()),
        };
        let mut handles = HashMap::new();
        let err = transaction_inner(&mut handles, &rewrites, tx, "req-rollback", || {
            bail!("injected failure after stale propagation")
        })
        .unwrap_err();
        assert!(err
            .to_string()
            .contains("injected failure after stale propagation"));
        assert_eq!(
            std::fs::read_to_string(&decision_path).unwrap(),
            "old decision"
        );
        assert_eq!(
            std::fs::read_to_string(&architecture_path).unwrap(),
            "old architecture"
        );
        assert!(!tx_path.exists(), "tx append must not run on rollback");
        let tmp_files: Vec<_> = std::fs::read_dir(tmp.path())
            .unwrap()
            .filter_map(Result::ok)
            .map(|entry| entry.file_name().to_string_lossy().to_string())
            .filter(|name| name.contains(".tmp.req-rollback"))
            .collect();
        assert!(tmp_files.is_empty(), "staged files should be cleaned up");
    }

    #[test]
    fn transaction_cross_file_move_leaves_heading_in_zero_not_two_on_failure() {
        let tmp = tempfile::tempdir().unwrap();
        let backlog_path = tmp.path().join("backlog.org");
        let in_progress_path = tmp.path().join("in_progress.org");
        let tx_path = tmp.path().join("tx").join("2026-06.org");
        std::fs::write(
            &backlog_path,
            "#+title: backlog\n#+orgasmic_version: 1\n\n* BACKLOG TASK-X Move me\n:PROPERTIES:\n:ID: TASK-X\n:END:\n",
        )
        .unwrap();
        std::fs::write(
            &in_progress_path,
            "#+title: in progress\n#+orgasmic_version: 1\n\n",
        )
        .unwrap();
        let rewrites = vec![
            FileRewrite {
                path: backlog_path.clone(),
                new_contents: b"#+title: backlog\n#+orgasmic_version: 1\n\n".to_vec(),
            },
            FileRewrite {
                path: in_progress_path.clone(),
                new_contents: b"#+title: in progress\n#+orgasmic_version: 1\n\n* IN_PROGRESS TASK-X Move me\n:PROPERTIES:\n:ID: TASK-X\n:END:\n".to_vec(),
            },
        ];
        let tx = TxAppend {
            tx_path: tx_path.clone(),
            entry: sample_entry("tx-cross-file-rollback"),
            project_id: Some("orgasmic".into()),
            tx_id_policy: TxIdPolicy::Preserve,
            request_id: Some("req-cross-file".into()),
        };
        let mut handles = HashMap::new();
        let err = transaction_inner(&mut handles, &rewrites, tx, "req-cross-file", || {
            bail!("injected crash before commit")
        })
        .unwrap_err();
        assert!(err.to_string().contains("injected crash before commit"));
        let backlog = std::fs::read_to_string(&backlog_path).unwrap();
        let in_progress = std::fs::read_to_string(&in_progress_path).unwrap();
        let in_backlog = backlog.contains("TASK-X");
        let in_progress_file = in_progress.contains("TASK-X");
        assert!(
            !(in_backlog && in_progress_file),
            "crash must not leave heading in two files"
        );
        assert!(
            in_backlog || !in_progress_file,
            "crash must leave heading in source (zero-or-two invariant: not duplicated)"
        );
        assert!(!tx_path.exists(), "tx append must not run on rollback");
    }

    #[tokio::test]
    async fn node_write_claimed_by_another_machine_is_refused_with_holder() {
        let tmp = tempfile::tempdir().unwrap();
        let node = tmp.path().join(".orgasmic/tasks/TASK-CLAIM/node.org");
        std::fs::create_dir_all(node.parent().unwrap()).unwrap();
        std::fs::write(&node, "old").unwrap();
        let claims = tmp.path().join(".orgasmic/machines/machine-b/claims.org");
        std::fs::create_dir_all(claims.parent().unwrap()).unwrap();
        let mut event = TxEntry::new(
            "claim-b",
            orgasmic_core::claims::CLAIMED,
            "[2026-08-26 Wed 10:00:00]",
            "test",
            "machine-b",
        );
        event.task = Some("TASK-CLAIM".into());
        let mut claims_writer = TxWriter::open(claims).unwrap();
        claims_writer.append(&event).unwrap();
        drop(claims_writer);

        let writer = spawn_with_catalog_index_and_machine(
            EventBus::new(),
            None,
            None,
            Some("machine-a".into()),
        );
        let error = writer
            .rewrite_file(
                FileRewrite {
                    path: node.clone(),
                    new_contents: b"new".to_vec(),
                },
                None,
            )
            .await
            .unwrap_err();
        assert!(error.to_string().contains("claimed by machine machine-b"));
        assert_eq!(std::fs::read_to_string(node).unwrap(), "old");
        writer.shutdown().await;
    }

    // orgasmic:TASK-BX5SR
    #[tokio::test]
    async fn panicking_mutation_fails_its_request_and_leaves_the_writer_serving() {
        let tmp = tempfile::tempdir().unwrap();
        let target = tmp.path().join("panic.org");
        let handle = spawn(EventBus::new());

        let error = handle
            .mutate_file(FileMutate {
                path: target.clone(),
                transform: Box::new(|_| panic!("injected mutation panic")),
            })
            .await
            .expect_err("a panicking mutation must fail its own request");
        assert!(
            error.to_string().contains("writer reply dropped"),
            "{error}"
        );

        // The next, independent write lands: the loop survived the panic.
        handle
            .mutate_file(FileMutate {
                path: target.clone(),
                transform: Box::new(|_| Ok(b"* survived\n".to_vec())),
            })
            .await
            .expect("the write after a panicking mutation must succeed");
        assert_eq!(std::fs::read_to_string(&target).unwrap(), "* survived\n");
        let status = handle.status();
        assert!(status.liveness, "status must report the writer alive");
        assert_eq!(
            status.failed_total, 1,
            "the panic counts as one failed command"
        );

        // And the liveness field goes false once the task is really gone.
        assert_eq!(
            handle
                .shutdown_within(std::time::Duration::from_secs(5))
                .await,
            WriterShutdownOutcome::Clean
        );
        tokio::task::yield_now().await;
        assert!(
            !handle.status().liveness,
            "status must report a dead writer"
        );
    }
}
