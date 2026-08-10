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
use std::fs::OpenOptions;
use std::io::Write;
#[cfg(unix)]
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::Arc;

use anyhow::{anyhow, bail, Context, Result};
use fs2::FileExt;
use orgasmic_core::session::{RuntimeIdentity, SessionEventKind, SessionWriter};
use orgasmic_core::tx::{parse_tx_file, TxEntry, TxWriter};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use tokio::sync::{mpsc, oneshot, Mutex};
use tracing::warn;
use uuid::Uuid;

use crate::events::{EventBus, EventPayload, Topic};

/// Test-only counters and injectors for writer durability tests.
#[doc(hidden)]
pub mod test_hooks {
    use super::*;

    static SYNC_COUNT: AtomicU64 = AtomicU64::new(0);
    static SYNC_ATTEMPT_COUNT: AtomicU64 = AtomicU64::new(0);
    static SCAN_COUNT: AtomicU64 = AtomicU64::new(0);
    static FAIL_NEXT_SYNC: AtomicUsize = AtomicUsize::new(0);

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
        SCAN_COUNT.store(0, Ordering::SeqCst);
        FAIL_NEXT_SYNC.store(0, Ordering::SeqCst);
    }

    pub fn sync_count() -> u64 {
        SYNC_COUNT.load(Ordering::SeqCst)
    }

    pub fn sync_attempt_count() -> u64 {
        SYNC_ATTEMPT_COUNT.load(Ordering::SeqCst)
    }

    pub fn scan_count() -> u64 {
        SCAN_COUNT.load(Ordering::SeqCst)
    }

    pub fn fail_next_sync(count: usize) {
        FAIL_NEXT_SYNC.store(count, Ordering::SeqCst);
    }

    pub(crate) fn before_sync() -> Result<()> {
        SYNC_ATTEMPT_COUNT.fetch_add(1, Ordering::SeqCst);
        if FAIL_NEXT_SYNC
            .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |remaining| {
                remaining.checked_sub(1)
            })
            .is_ok()
        {
            bail!("injected tx append fsync failure");
        }
        Ok(())
    }

    pub(crate) fn after_sync() {
        SYNC_COUNT.fetch_add(1, Ordering::SeqCst);
    }

    pub(crate) fn record_scan() {
        SCAN_COUNT.fetch_add(1, Ordering::SeqCst);
    }
}

type ProjectMonthKey = (String, String);

#[derive(Debug, Default)]
struct ProjectTxSeqCache {
    by_project_month: HashMap<ProjectMonthKey, u32>,
    project_max: HashMap<String, u32>,
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

#[derive(Debug)]
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
    /// `tx`, `session`, `transaction`, `mutate`, `rewrite`, or `shutdown`.
    pub kind: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub run_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tx_type: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub path: Option<PathBuf>,
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
    /// Appends currently queued behind a session lease
    /// (orgasmic:TASK-FZB6T.3 finding 1). Read only by the regression that
    /// schedules an append at the pre-rename instant; the writer publishes it
    /// unconditionally so the two builds cannot diverge.
    #[cfg_attr(not(test), allow(dead_code))]
    deferred_appends: Arc<std::sync::atomic::AtomicUsize>,
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
    Rewrite,
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
    /// Append a tx entry through the daemon writer. Re-using `request_id`
    /// is safe — the second call returns the same result.
    pub async fn append_tx(
        &self,
        req: TxAppend,
        request_id: Option<String>,
    ) -> Result<TxAppendResult> {
        let request_id = request_id
            .or_else(|| req.request_id.clone())
            .unwrap_or_else(|| Uuid::new_v4().to_string());
        {
            let cache = self.idempotency.lock().await;
            if let Some(CachedResponse::Tx { result, .. }) = cache.get(&request_id) {
                return Ok(result.clone());
            }
        }
        let (reply, rx) = oneshot::channel();
        self.tx
            .send(WriterCommand::Tx { req, reply })
            .await
            .map_err(|_| anyhow!("writer task is gone"))?;
        let res = rx.await.map_err(|_| anyhow!("writer reply dropped"))??;
        let mut cache = self.idempotency.lock().await;
        cache.insert(
            request_id,
            CachedResponse::Tx {
                result: res.clone(),
                mutation: None,
                mutation_id: None,
            },
        );
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
        {
            let cache = self.idempotency.lock().await;
            if let Some(result) = cached_transaction_from_map(&cache, &request_id, &mutation)? {
                return Ok(result.tx_id.clone());
            }
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
        Ok(res.tx_id)
    }

    pub async fn transaction_mutation(
        &self,
        rewrites: Vec<FileRewrite>,
        tx: TxAppend,
        mutation: MutationIdentity,
        mutation_id: String,
    ) -> Result<CachedMutation> {
        let request_id = tx
            .request_id
            .clone()
            .unwrap_or_else(|| Uuid::new_v4().to_string());
        if let Some(cached) = self.cached_mutation(&request_id, &mutation).await? {
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
        let _ = rx.await.map_err(|_| anyhow!("writer reply dropped"))??;
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
        let (reply, rx) = oneshot::channel();
        self.tx
            .send(WriterCommand::Mutate { req, reply })
            .await
            .map_err(|_| anyhow!("writer task is gone"))?;
        rx.await.map_err(|_| anyhow!("writer reply dropped"))?
    }

    pub async fn rewrite_file(&self, req: FileRewrite, request_id: Option<String>) -> Result<()> {
        let request_id = request_id.unwrap_or_else(|| Uuid::new_v4().to_string());
        {
            let cache = self.idempotency.lock().await;
            if matches!(cache.get(&request_id), Some(CachedResponse::Rewrite)) {
                return Ok(());
            }
        }
        let (reply, rx) = oneshot::channel();
        self.tx
            .send(WriterCommand::Rewrite { req, reply })
            .await
            .map_err(|_| anyhow!("writer task is gone"))?;
        rx.await.map_err(|_| anyhow!("writer reply dropped"))??;
        let mut cache = self.idempotency.lock().await;
        cache.insert(request_id, CachedResponse::Rewrite);
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
            queued: self.tx.max_capacity().saturating_sub(self.tx.capacity()),
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

/// Boot the writer task and return a clone-able handle.
pub fn spawn(events: EventBus) -> WriterHandle {
    spawn_with_catalog(events, None)
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
    let (tx, rx) = mpsc::channel(256);
    let idempotency = Arc::new(Mutex::new(HashMap::new()));
    let in_flight = Arc::new(std::sync::Mutex::new(None));
    let deferred_appends = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    tokio::spawn(writer_loop(
        rx,
        events,
        Arc::clone(&idempotency),
        Arc::clone(&in_flight),
        catalog,
        Arc::clone(&deferred_appends),
    ));
    WriterHandle {
        tx,
        idempotency,
        in_flight,
        deferred_appends,
        #[cfg(test)]
        transaction_gate: Arc::new(Mutex::new(None)),
        #[cfg(test)]
        session_append_failure: Arc::new(std::sync::Mutex::new(None)),
    }
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
        WriterCommand::Shutdown { .. } => PendingWrite {
            kind: "shutdown".to_string(),
            run_id: None,
            tx_type: None,
            path: None,
        },
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
    catalog: Option<crate::run_catalog::RunCatalog>,
    deferred_appends: Arc<std::sync::atomic::AtomicUsize>,
) {
    use std::sync::atomic::Ordering;

    let mut tx_handles: HashMap<PathBuf, CachedTxWriter> = HashMap::new();
    let mut session_handles: HashMap<String, SessionWriter> = HashMap::new();
    // Session paths a maintenance transaction currently holds.
    let mut leased: HashSet<PathBuf> = HashSet::new();
    let mut deferred: Vec<DeferredSessionAppend> = Vec::new();
    let mut seq_cache = ProjectTxSeqCache::default();
    let mut cmd = rx.recv().await;
    while let Some(current) = cmd.take() {
        // orgasmic:TASK-Q07Y5 — publish the head-of-line write before running
        // it. A shutdown that gives up on its budget reads this to say which
        // write it gave up on; without it the report is a bare count.
        {
            let pending = describe_command(&current);
            let stall = injected_write_stall(&pending);
            *in_flight.lock().unwrap_or_else(|e| e.into_inner()) = Some(pending);
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
                let outcomes = process_tx_batch(&mut tx_handles, &mut seq_cache, batch);
                for (req, result, reply) in outcomes {
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
                } else {
                    run_session_append(
                        &mut session_handles,
                        &events,
                        catalog.as_ref(),
                        req,
                        reply,
                        #[cfg(test)]
                        injected_failure,
                    );
                }
            }
            WriterCommand::Rewrite { req, reply } => {
                let result = rewrite_file_inner(&req);
                if result.is_ok() {
                    events.publish(
                        Topic::Graph,
                        EventPayload::GraphChanged {
                            node_id: req.path.display().to_string(),
                        },
                    );
                }
                let _ = reply.send(result);
            }
            WriterCommand::Mutate { req, reply } => {
                let path = req.path.clone();
                let result = mutate_file_inner(req);
                if result.is_ok() {
                    events.publish(
                        Topic::Graph,
                        EventPayload::GraphChanged {
                            node_id: path.display().to_string(),
                        },
                    );
                }
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
                        None => unreachable!("all writer transactions carry an identity"),
                    }
                };
                match cached {
                    Ok(Some(result)) => {
                        let _ = reply.send(Ok(result));
                        continue;
                    }
                    Err(error) => {
                        let _ = reply.send(Err(error));
                        continue;
                    }
                    Ok(None) => {}
                }
                let result = transaction_inner(
                    &mut tx_handles,
                    &mut seq_cache,
                    &req.rewrites,
                    req.tx.clone(),
                    &req.request_id,
                    || Ok(()),
                );
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
                    for rewrite in &req.rewrites {
                        events.publish(
                            Topic::Graph,
                            EventPayload::GraphChanged {
                                node_id: rewrite.path.display().to_string(),
                            },
                        );
                    }
                    events.publish(
                        Topic::Daemon,
                        EventPayload::TxAppended {
                            project_id: req.tx.project_id.clone(),
                            tx_id: ok.tx_id.clone(),
                            ty: req.tx.entry.ty.clone(),
                        },
                    );
                }
                let _ = reply.send(result);
            }
            WriterCommand::LeaseSessions { paths, reply } => {
                // Two transactions holding the same path would each believe it
                // had exclusion. Refuse rather than share.
                let held = paths.iter().find(|path| leased.contains(*path)).cloned();
                match held {
                    Some(path) => {
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
                for entry in ready {
                    run_session_append(
                        &mut session_handles,
                        &events,
                        catalog.as_ref(),
                        entry.req,
                        entry.reply,
                        #[cfg(test)]
                        entry.injected_failure,
                    );
                }
            }
            WriterCommand::Shutdown { reply } => {
                tx_handles.clear();
                session_handles.clear();
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
                let _ = reply.send(());
                break;
            }
        }
        *in_flight.lock().unwrap_or_else(|e| e.into_inner()) = None;
        if cmd.is_none() {
            cmd = rx.recv().await;
        }
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
) {
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
    let _ = reply.send(result);
}

struct PendingTxBatchItem {
    req: TxAppend,
    reply: oneshot::Sender<Result<TxAppendResult>>,
    result: Result<TxAppendResult>,
}

fn process_tx_batch(
    handles: &mut HashMap<PathBuf, CachedTxWriter>,
    seq_cache: &mut ProjectTxSeqCache,
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
    if tx_handles_detached_from_paths(handles, &paths_in_batch) {
        seq_cache.clear();
    }
    for item in &mut pending {
        item.result = (|| -> Result<TxAppendResult> {
            let entry = prepare_tx_entry(seq_cache, &item.req)?;
            let res = write_tx_append(handles, &item.req.tx_path, &entry)?;
            paths_to_sync.insert(item.req.tx_path.clone());
            Ok(res)
        })();
    }

    let mut sync_failed_paths = HashSet::new();
    for path in &paths_to_sync {
        if let Err(e) = sync_tx_path(path) {
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
    writer: TxWriter,
    identity: FileIdentity,
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
        let writer = TxWriter::open(path).with_context(|| format!("open {}", path.display()))?;
        let identity = FileIdentity::from_path(path)?;
        Ok(Self { writer, identity })
    }

    fn path_still_names_writer(&self, path: &Path) -> bool {
        FileIdentity::from_path(path)
            .map(|current| current == self.identity)
            .unwrap_or(false)
    }
}

impl ProjectTxSeqCache {
    fn clear(&mut self) {
        self.by_project_month.clear();
        self.project_max.clear();
    }
}

fn tx_handles_detached_from_paths(
    handles: &mut HashMap<PathBuf, CachedTxWriter>,
    paths: &HashSet<PathBuf>,
) -> bool {
    let mut detached = Vec::new();
    for path in paths {
        if let Some(handle) = handles.get(path) {
            if !handle.path_still_names_writer(path) {
                detached.push(path.clone());
            }
        }
    }
    let had_detached = !detached.is_empty();
    for path in detached {
        handles.remove(&path);
        warn!(
            path = %path.display(),
            "tx append handle no longer matches path; reopening and invalidating tx sequence cache"
        );
    }
    had_detached
}

fn prepare_tx_entry(seq_cache: &mut ProjectTxSeqCache, req: &TxAppend) -> Result<TxEntry> {
    let mut entry = req.entry.clone();
    if let TxIdPolicy::ProjectSequence { project_id, date } = &req.tx_id_policy {
        let tx_dir = req
            .tx_path
            .parent()
            .ok_or_else(|| anyhow!("tx path has no parent: {}", req.tx_path.display()))?;
        entry.tx_id = next_project_tx_id(seq_cache, project_id, tx_dir, date)?;
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
    writer
        .writer
        .append(entry)
        .with_context(|| format!("append to {}", tx_path.display()))?;
    Ok(TxAppendResult {
        tx_id: entry.tx_id.clone(),
        tx_path: tx_path.to_path_buf(),
    })
}

fn sync_tx_path(path: &Path) -> Result<()> {
    test_hooks::before_sync()?;
    let file = OpenOptions::new()
        .read(true)
        .open(path)
        .with_context(|| format!("open {} for fsync", path.display()))?;
    file.sync_data()
        .with_context(|| format!("fsync {}", path.display()))?;
    test_hooks::after_sync();
    Ok(())
}

fn append_tx_inner(
    handles: &mut HashMap<PathBuf, CachedTxWriter>,
    seq_cache: &mut ProjectTxSeqCache,
    req: TxAppend,
) -> Result<TxAppendResult> {
    let paths = HashSet::from([req.tx_path.clone()]);
    if tx_handles_detached_from_paths(handles, &paths) {
        seq_cache.clear();
    }
    let entry = prepare_tx_entry(seq_cache, &req)?;
    write_tx_append(handles, &req.tx_path, &entry)
}

fn next_project_tx_id(
    cache: &mut ProjectTxSeqCache,
    project_id: &str,
    tx_dir: &Path,
    date: &str,
) -> Result<String> {
    let month = date.get(..6).unwrap_or(date).to_string();
    let key = (project_id.to_string(), month);
    let next = if let Some(&cached) = cache.by_project_month.get(&key) {
        cached + 1
    } else if let Some(&max) = cache.project_max.get(project_id) {
        max + 1
    } else {
        let max_seen = scan_project_tx_max_seq(project_id, tx_dir)?;
        max_seen + 1
    };
    cache.by_project_month.insert(key, next);
    cache.project_max.insert(project_id.to_string(), next);
    let slug = project_tx_slug(project_id);
    Ok(format!("tx-{date}-{slug}-{next:04}"))
}

fn scan_project_tx_max_seq(project_id: &str, tx_dir: &Path) -> Result<u32> {
    test_hooks::record_scan();
    let slug = project_tx_slug(project_id);
    let mut max_seen = 0_u32;
    if tx_dir.is_dir() {
        let dir_entries =
            std::fs::read_dir(tx_dir).with_context(|| format!("read {}", tx_dir.display()))?;
        for entry in dir_entries {
            let entry = match entry {
                Ok(e) => e,
                Err(e) => {
                    warn!(
                        dir = %tx_dir.display(),
                        error = %e,
                        "skip unreadable tx dir entry during sequence scan"
                    );
                    continue;
                }
            };
            let path = entry.path();
            if path.extension().and_then(|e| e.to_str()) != Some("org") {
                continue;
            }
            let source = match std::fs::read_to_string(&path) {
                Ok(s) => s,
                Err(e) => {
                    warn!(
                        path = %path.display(),
                        error = %e,
                        "skip unreadable tx file during sequence scan"
                    );
                    continue;
                }
            };
            let entries = match parse_tx_file(&source, &path.to_string_lossy()) {
                Ok(e) => e,
                Err(e) => {
                    warn!(
                        path = %path.display(),
                        error = %e,
                        "skip corrupt tx file during sequence scan"
                    );
                    continue;
                }
            };
            for entry in entries {
                if let Some(seq) = project_tx_sequence(&entry.tx_id, &slug) {
                    max_seen = max_seen.max(seq);
                }
            }
        }
    }
    Ok(max_seen)
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

fn project_tx_sequence(tx_id: &str, slug: &str) -> Option<u32> {
    let mut parts = tx_id.split('-');
    let prefix = parts.next()?;
    let date = parts.next()?;
    let got_slug = parts.next()?;
    let seq = parts.next()?;
    if parts.next().is_some()
        || prefix != "tx"
        || date.len() != 8
        || !date.chars().all(|c| c.is_ascii_digit())
        || got_slug != slug
        || seq.len() != 4
        || !seq.chars().all(|c| c.is_ascii_digit())
    {
        return None;
    }
    seq.parse().ok()
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

fn transaction_inner<F>(
    handles: &mut HashMap<PathBuf, CachedTxWriter>,
    seq_cache: &mut ProjectTxSeqCache,
    rewrites: &[FileRewrite],
    tx: TxAppend,
    request_id: &str,
    verify_before_commit: F,
) -> Result<TxAppendResult>
where
    F: FnOnce() -> Result<()>,
{
    reject_duplicate_rewrites(rewrites)?;
    let mut locks = Vec::new();
    let mut staged = Vec::new();
    let result = (|| -> Result<TxAppendResult> {
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
                locks.push((rewrite.path.clone(), file));
            }
        }
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
            if let Err(e) = std::fs::rename(&rewrite.tmp, &rewrite.target).with_context(|| {
                format!(
                    "rename {} -> {}",
                    rewrite.tmp.display(),
                    rewrite.target.display()
                )
            }) {
                rollback_renamed_rewrites(&staged, &renamed);
                return Err(e);
            }
            renamed.push(idx);
        }
        let appended = append_tx_inner(handles, seq_cache, tx);
        if appended.is_err() {
            rollback_renamed_rewrites(&staged, &renamed);
            return appended;
        }
        let appended = appended?;
        if let Err(e) = sync_tx_path(&appended.tx_path) {
            rollback_renamed_rewrites(&staged, &renamed);
            return Err(e);
        }
        Ok(appended)
    })();
    for (path, file) in locks {
        if let Err(e) = FileExt::unlock(&file) {
            warn!(path = %path.display(), error = %e, "flock unlock failed");
        }
    }
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
    async fn project_sequence_ids_are_assigned_inside_writer() {
        let tmp = tempfile::tempdir().unwrap();
        let tx_path = tmp.path().join("tx").join("2026-06.org");
        std::fs::create_dir_all(tx_path.parent().unwrap()).unwrap();
        std::fs::write(
            tx_path.parent().unwrap().join("2026-05.org"),
            "#+title: orgasmic project tx 2026-05\n#+orgasmic_version: 1\n\n* TX 2026-05-21 22:10 manager.action orgasmic\n:PROPERTIES:\n:TX_ID:        tx-20260521-orgasmic-0036\n:TIME:         [2026-05-21 Thu 22:10:00]\n:TYPE:         manager.action\n:ACTOR:        dev@example.com\n:MACHINE:      host.local\n:PROJECT:      orgasmic\n:END:\n",
        )
        .unwrap();
        let bus = EventBus::new();
        let handle = spawn(bus);
        let req = TxAppend {
            tx_path: tx_path.clone(),
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
        assert_eq!(res.tx_id, "tx-20260601-orgasmic-0037");
        let source = std::fs::read_to_string(&tx_path).unwrap();
        assert!(source.contains(":TX_ID:        tx-20260601-orgasmic-0037"));
        assert!(!source.contains("placeholder"));
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
        let mut seq_cache = ProjectTxSeqCache::default();
        let err = transaction_inner(
            &mut handles,
            &mut seq_cache,
            &rewrites,
            tx,
            "req-rollback",
            || bail!("injected failure after stale propagation"),
        )
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
        let mut seq_cache = ProjectTxSeqCache::default();
        let err = transaction_inner(
            &mut handles,
            &mut seq_cache,
            &rewrites,
            tx,
            "req-cross-file",
            || bail!("injected crash before commit"),
        )
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
}
