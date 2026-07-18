// orgasmic:dec_3Y2E1
//! External manager self-registration (dec_3Y2E1).
//!
//! A manager session started outside the app (a plain terminal, even outside
//! rmux) registers itself here so it appears in Running Agents as a
//! supervised run. Registration acquires a REAL supervisor run — driver
//! `external`, no PTY — on the same `manager.launch:<project>` lease the
//! app's own manager launch uses, so the manager singleton is enforced by one
//! mechanism, not two.
//!
//! Liveness is polled out-of-band from the generic stall/max-duration
//! detectors (which the `manager.launch:` task id prefix already disables,
//! `is_interactive_manager_task`): [`ManagerRegistry::sweep`] checks the
//! registrant's terminal session-leader PID roughly every 30s and releases
//! the run on death, or — when no PID was supplied — expires a TTL that
//! re-registering refreshes. This is the part that must never fail open: a
//! wedged external lease 500s the app's own manager launch.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use orgasmic_core::{DriverEvent, ReleaseOutcome};
use orgasmic_drivers::{
    BabysitterAck, BabysitterRequest, DriverConfig, DriverContext, DriverControl, DriverError,
    DriverSession, RunKind, TransitionAck, TransitionRequest, UserInputAck, UserInputRequest,
    WorkerDriver,
};
use tokio::sync::Mutex;
use tokio::time::Instant;

use crate::supervisor::{subprocess_exited, AcquireRequest, Supervisor, SupervisorError};

/// How often the background sweep polls registered PIDs / checks TTL expiry.
pub(crate) const LIVENESS_POLL_INTERVAL: Duration = Duration::from_secs(30);
/// TTL for a PID-less registration. Refreshed by re-registering.
pub(crate) const TTL_NO_PID: Duration = Duration::from_secs(15 * 60);

/// Driver for an external manager's presence record. There is no PTY and no
/// agent CLI behind it — `acquire` returns immediately and the run lives
/// until [`ManagerRegistry`] releases it (PID death, TTL expiry, or explicit
/// `orgasmic manager release` / UI End).
pub struct ExternalManagerDriver;

#[async_trait]
impl WorkerDriver for ExternalManagerDriver {
    fn transport(&self) -> &'static str {
        "external"
    }

    fn harness(&self) -> Option<&'static str> {
        // Deliberately not `custom` (the bare-terminal pseudo-harness): the
        // UI's `isTerminalRun` filter drops `manager.launch` + `custom` runs
        // from Running Agents, and an external registration must pass that
        // filter to be visible.
        Some("external")
    }

    async fn acquire(
        &self,
        ctx: DriverContext,
        _config: DriverConfig,
    ) -> Result<DriverSession, DriverError> {
        let (tx, rx) = tokio::sync::mpsc::channel(1);
        Ok(DriverSession {
            identity: ctx.identity,
            // No owned subprocess — liveness is tracked separately by
            // `ManagerRegistry`, not the supervisor's generic pid watcher.
            pid: None,
            events: rx,
            // The sender must outlive acquire(): dropping it here would close
            // the channel and make the drain task see stream-end immediately,
            // auto-releasing the run before it ever goes live.
            control: Box::new(ExternalManagerControl { _events: tx }),
            native_runtime: None,
        })
    }
}

struct ExternalManagerControl {
    _events: tokio::sync::mpsc::Sender<DriverEvent>,
}

#[async_trait]
impl DriverControl for ExternalManagerControl {
    async fn transition_state(
        &mut self,
        _req: TransitionRequest,
    ) -> Result<TransitionAck, DriverError> {
        Err(DriverError::Unsupported("transition_state"))
    }

    async fn babysitter_action(
        &mut self,
        _req: BabysitterRequest,
    ) -> Result<BabysitterAck, DriverError> {
        Err(DriverError::Unsupported("babysitter_action"))
    }

    async fn send_input(&mut self, _req: UserInputRequest) -> Result<UserInputAck, DriverError> {
        Err(DriverError::Unsupported("send_input"))
    }

    async fn release(&mut self, _reason: &str) -> Result<(), DriverError> {
        Ok(())
    }
}

#[derive(Clone)]
struct Registration {
    run_id: String,
    pid: Option<u32>,
    /// Opaque daemon-minted identity for the PID-less fallback. A second
    /// PID-less terminal must present this exact token to refresh the holder.
    holder_token: Option<String>,
    /// `Some` only when `pid` is `None` — the PID-less TTL fallback.
    expires_at: Option<Instant>,
}

/// Outcome of a register call.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RegisterOutcome {
    Registered {
        run_id: String,
        holder_token: Option<String>,
    },
    /// Same-session re-register: idempotent refresh, doubles as heartbeat.
    Refreshed {
        run_id: String,
        holder_token: Option<String>,
    },
    /// A different holder (app manager or another external session) already
    /// has the lease. Never a takeover.
    Refused { message: String },
}

/// Outcome of a release call.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ManagerReleaseOutcome {
    Released { run_id: String },
    NotRegistered,
}

/// In-memory registry of live external manager registrations, keyed by
/// project id. Entries are auxiliary bookkeeping (PID / TTL) on top of the
/// supervisor's own lease — every read reconciles against
/// [`Supervisor::snapshot`] first, so a run released through any other path
/// (the generic `/runs/:id/release`, a stall sweep, a daemon restart) is
/// never trusted stale.
#[derive(Clone)]
pub struct ManagerRegistry {
    inner: Arc<Mutex<HashMap<String, Registration>>>,
}

impl Default for ManagerRegistry {
    fn default() -> Self {
        Self::new()
    }
}

impl ManagerRegistry {
    pub fn new() -> Self {
        Self {
            inner: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    /// Drop `project_id`'s entry from `map` if the run it names is no longer
    /// a live external run in `supervisor`'s snapshot. Returns the surviving
    /// entry, if any. Callers must already hold `map`'s lock.
    async fn reconcile_locked(
        supervisor: &Supervisor,
        map: &mut HashMap<String, Registration>,
        project_id: &str,
    ) -> Option<Registration> {
        let entry = map.get(project_id)?.clone();
        let snapshot = supervisor.snapshot().await;
        let still_live = snapshot
            .runs
            .iter()
            .any(|run| run.run_id == entry.run_id && run.driver == "external");
        if still_live {
            Some(entry)
        } else {
            map.remove(project_id);
            None
        }
    }

    /// Register (or idempotently refresh) an external manager for
    /// `project_id`. `pid` is the registrant's terminal session-leader PID,
    /// when known. PID-less refreshes require the opaque token returned by the
    /// first registration so unrelated terminals cannot extend its TTL.
    pub async fn register(
        &self,
        supervisor: &Supervisor,
        project_id: &str,
        project_root: PathBuf,
        session_path: PathBuf,
        pid: Option<u32>,
        holder_token: Option<String>,
    ) -> Result<RegisterOutcome, SupervisorError> {
        let holder_token = holder_token.filter(|token| !token.trim().is_empty());
        let mut map = self.inner.lock().await;
        if let Some(existing) = Self::reconcile_locked(supervisor, &mut map, project_id).await {
            let same_session = match (existing.pid, pid) {
                (Some(a), Some(b)) => a == b,
                (None, None) => {
                    existing.holder_token.as_deref() == holder_token.as_deref()
                        && existing.holder_token.is_some()
                }
                _ => false,
            };
            if same_session {
                let run_id = existing.run_id.clone();
                let holder_token = existing.holder_token.clone();
                let expires_at = pid.is_none().then(|| Instant::now() + TTL_NO_PID);
                map.insert(
                    project_id.to_string(),
                    Registration {
                        run_id: run_id.clone(),
                        pid,
                        holder_token: holder_token.clone(),
                        expires_at,
                    },
                );
                return Ok(RegisterOutcome::Refreshed {
                    run_id,
                    holder_token,
                });
            }
            let message = match existing.pid {
                Some(holder_pid) => format!(
                    "a manager for {project_id} is already registered externally (pid {holder_pid})"
                ),
                None => {
                    format!("a manager for {project_id} is already registered externally")
                }
            };
            return Ok(RegisterOutcome::Refused { message });
        }

        let task_id = format!("manager.launch:{project_id}");
        let acquire = supervisor
            .acquire(
                &ExternalManagerDriver,
                AcquireRequest {
                    task_id,
                    kind: RunKind::Worker,
                    worker_id: "manager".into(),
                    role: "manager".into(),
                    project_id: Some(project_id.to_string()),
                    worktree: Some(project_root),
                    last_path: None,
                    stdout_path: None,
                    session_path,
                    driver_config: DriverConfig::empty(),
                    babysitter_target: None,
                    // Same as the app's own manager launch: interactive,
                    // operator-paced, never a stall/max-duration candidate.
                    stall_timeout_secs: Some(0),
                    max_run_duration_secs: Some(0),
                    idle_timeout_secs: None,
                    babysitter: None,
                },
            )
            .await;
        match acquire {
            Ok(resp) => {
                let holder_token = pid.is_none().then(|| uuid::Uuid::new_v4().to_string());
                let expires_at = pid.is_none().then(|| Instant::now() + TTL_NO_PID);
                map.insert(
                    project_id.to_string(),
                    Registration {
                        run_id: resp.run_id.clone(),
                        pid,
                        holder_token: holder_token.clone(),
                        expires_at,
                    },
                );
                Ok(RegisterOutcome::Registered {
                    run_id: resp.run_id,
                    holder_token,
                })
            }
            Err(SupervisorError::LeaseHeld { .. }) => Ok(RegisterOutcome::Refused {
                message: format!("a manager for {project_id} is already running in the app"),
            }),
            Err(e) => Err(e),
        }
    }

    /// Explicit deregistration (`orgasmic manager release`). A no-op —
    /// [`ManagerReleaseOutcome::NotRegistered`] — when nothing is registered
    /// for `project_id` (including when the entry was already stale).
    pub async fn release(
        &self,
        supervisor: &Supervisor,
        project_id: &str,
    ) -> ManagerReleaseOutcome {
        let mut map = self.inner.lock().await;
        let Some(entry) = Self::reconcile_locked(supervisor, &mut map, project_id).await else {
            return ManagerReleaseOutcome::NotRegistered;
        };
        map.remove(project_id);
        drop(map);
        // orgasmic:TASK-S52X9 — `orgasmic manager release` is the manager's
        // terminal declaration: write the finalize tombstone and Completed
        // (not Cancelled). Unexpected protocol death without release stays
        // the anomaly path elsewhere.
        let _ = supervisor
            .release_with_finalization(
                &entry.run_id,
                "manager_released",
                ReleaseOutcome::Completed,
                true,
                None,
            )
            .await;
        ManagerReleaseOutcome::Released {
            run_id: entry.run_id,
        }
    }

    /// One liveness pass: reconcile every entry against the supervisor's live
    /// runs (dropping ones released elsewhere), then release any whose PID
    /// has died or whose PID-less TTL has lapsed.
    pub async fn sweep(&self, supervisor: &Supervisor) {
        let snapshot = supervisor.snapshot().await;
        let now = Instant::now();
        let mut to_release: Vec<(String, &'static str)> = Vec::new();
        {
            let mut map = self.inner.lock().await;
            map.retain(|_project_id, reg| {
                let still_live = snapshot
                    .runs
                    .iter()
                    .any(|run| run.run_id == reg.run_id && run.driver == "external");
                if !still_live {
                    return false;
                }
                let dead = match (reg.pid, reg.expires_at) {
                    (Some(pid), _) => subprocess_exited(pid),
                    (None, Some(expires_at)) => now >= expires_at,
                    (None, None) => false,
                };
                if dead {
                    let reason = if reg.pid.is_some() {
                        "external manager process exited"
                    } else {
                        "external manager registration expired (TTL lapsed, no PID)"
                    };
                    to_release.push((reg.run_id.clone(), reason));
                    false
                } else {
                    true
                }
            });
        }
        for (run_id, reason) in to_release {
            if let Err(e) = supervisor
                .release(&run_id, reason, ReleaseOutcome::Interrupted)
                .await
            {
                tracing::warn!(run_id = %run_id, error = %e, "external manager liveness release failed");
            }
        }
    }
}

/// Spawn the background liveness loop for the lifetime of the daemon
/// process. Tests call [`ManagerRegistry::sweep`] directly instead — it is
/// deterministic where waiting on a real 30s tick would not be.
pub fn spawn_liveness_loop(registry: ManagerRegistry, supervisor: Supervisor) {
    tokio::spawn(async move {
        loop {
            tokio::time::sleep(LIVENESS_POLL_INTERVAL).await;
            registry.sweep(&supervisor).await;
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::events::EventBus;
    use crate::runtime::BootIdentity;
    use crate::writer::spawn as spawn_writer;
    use orgasmic_core::{read_session_file, Lifecycle, SessionEventKind};
    use std::process::{Command, Stdio};

    fn make_supervisor() -> (Supervisor, tempfile::TempDir) {
        let dir = tempfile::tempdir().unwrap();
        let writer = spawn_writer(EventBus::new());
        let boot = Arc::new(BootIdentity::new());
        let sup = Supervisor::new(writer, boot);
        (sup, dir)
    }

    fn session_path(dir: &tempfile::TempDir, name: &str) -> PathBuf {
        dir.path().join(format!("{name}.jsonl"))
    }

    /// Spawn a short-lived child and return its pid. Caller must `wait()` it
    /// (or let the OS reap it) before asserting on liveness.
    fn spawn_short_lived() -> std::process::Child {
        Command::new("sh")
            .arg("-c")
            .arg("exit 0")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("spawn short-lived process")
    }

    fn app_manager_request(dir: &tempfile::TempDir, name: &str) -> AcquireRequest {
        AcquireRequest {
            task_id: "manager.launch:proj".into(),
            kind: RunKind::Worker,
            worker_id: "manager".into(),
            role: "manager".into(),
            project_id: Some("proj".into()),
            worktree: Some(dir.path().to_path_buf()),
            last_path: None,
            stdout_path: None,
            session_path: session_path(dir, name),
            driver_config: DriverConfig::empty(),
            babysitter_target: None,
            stall_timeout_secs: Some(0),
            max_run_duration_secs: Some(0),
            idle_timeout_secs: None,
            babysitter: None,
        }
    }

    #[tokio::test]
    async fn register_makes_run_visible_with_external_driver() {
        let (sup, dir) = make_supervisor();
        let registry = ManagerRegistry::new();
        let outcome = registry
            .register(
                &sup,
                "proj",
                dir.path().to_path_buf(),
                session_path(&dir, "manager-proj"),
                Some(4242),
                None,
            )
            .await
            .unwrap();
        let RegisterOutcome::Registered { run_id, .. } = outcome else {
            panic!("expected Registered, got {outcome:?}");
        };
        let snap = sup.snapshot().await;
        let run = snap.runs.iter().find(|r| r.run_id == run_id).unwrap();
        assert_eq!(run.driver, "external");
        assert_eq!(run.harness.as_deref(), Some("external"));
        assert_eq!(run.task_id, "manager.launch:proj");
    }

    #[tokio::test]
    async fn app_launch_while_external_registered_is_refused() {
        let (sup, dir) = make_supervisor();
        let registry = ManagerRegistry::new();
        registry
            .register(
                &sup,
                "proj",
                dir.path().to_path_buf(),
                session_path(&dir, "manager-proj"),
                Some(4242),
                None,
            )
            .await
            .unwrap();

        // Simulate the app's own manager launch: same stable task id, same
        // RunKind, a different (non-external) driver.
        struct FakeAppManagerDriver;
        #[async_trait::async_trait]
        impl WorkerDriver for FakeAppManagerDriver {
            fn transport(&self) -> &'static str {
                "tmux"
            }
            fn harness(&self) -> Option<&'static str> {
                Some("claude")
            }
            async fn acquire(
                &self,
                ctx: DriverContext,
                _config: DriverConfig,
            ) -> Result<DriverSession, DriverError> {
                let (tx, rx) = tokio::sync::mpsc::channel(1);
                Ok(DriverSession {
                    identity: ctx.identity,
                    pid: None,
                    events: rx,
                    control: Box::new(ExternalManagerControl { _events: tx }),
                    native_runtime: None,
                })
            }
        }

        let err = sup
            .acquire(
                &FakeAppManagerDriver,
                AcquireRequest {
                    task_id: "manager.launch:proj".into(),
                    kind: RunKind::Worker,
                    worker_id: "manager".into(),
                    role: "manager".into(),
                    project_id: Some("proj".into()),
                    worktree: Some(dir.path().to_path_buf()),
                    last_path: None,
                    stdout_path: None,
                    session_path: session_path(&dir, "manager-proj-app"),
                    driver_config: DriverConfig::empty(),
                    babysitter_target: None,
                    stall_timeout_secs: Some(0),
                    max_run_duration_secs: Some(0),
                    idle_timeout_secs: None,
                    babysitter: None,
                },
            )
            .await
            .unwrap_err();
        assert!(matches!(err, SupervisorError::LeaseHeld { .. }));
    }

    #[tokio::test]
    async fn register_while_app_manager_live_is_refused_with_readable_message() {
        let (sup, dir) = make_supervisor();
        let registry = ManagerRegistry::new();

        struct FakeAppManagerDriver;
        #[async_trait::async_trait]
        impl WorkerDriver for FakeAppManagerDriver {
            fn transport(&self) -> &'static str {
                "tmux"
            }
            fn harness(&self) -> Option<&'static str> {
                Some("claude")
            }
            async fn acquire(
                &self,
                ctx: DriverContext,
                _config: DriverConfig,
            ) -> Result<DriverSession, DriverError> {
                let (tx, rx) = tokio::sync::mpsc::channel(1);
                Ok(DriverSession {
                    identity: ctx.identity,
                    pid: None,
                    events: rx,
                    control: Box::new(ExternalManagerControl { _events: tx }),
                    native_runtime: None,
                })
            }
        }

        sup.acquire(
            &FakeAppManagerDriver,
            AcquireRequest {
                task_id: "manager.launch:proj".into(),
                kind: RunKind::Worker,
                worker_id: "manager".into(),
                role: "manager".into(),
                project_id: Some("proj".into()),
                worktree: Some(dir.path().to_path_buf()),
                last_path: None,
                stdout_path: None,
                session_path: session_path(&dir, "manager-proj-app"),
                driver_config: DriverConfig::empty(),
                babysitter_target: None,
                stall_timeout_secs: Some(0),
                max_run_duration_secs: Some(0),
                idle_timeout_secs: None,
                babysitter: None,
            },
        )
        .await
        .unwrap();

        let outcome = registry
            .register(
                &sup,
                "proj",
                dir.path().to_path_buf(),
                session_path(&dir, "manager-proj-ext"),
                Some(4242),
                None,
            )
            .await
            .unwrap();
        let RegisterOutcome::Refused { message } = outcome else {
            panic!("expected Refused, got {outcome:?}");
        };
        assert!(message.contains("already running in the app"), "{message}");
    }

    #[tokio::test]
    async fn same_pid_reregister_is_idempotent_refresh() {
        let (sup, dir) = make_supervisor();
        let registry = ManagerRegistry::new();
        let first = registry
            .register(
                &sup,
                "proj",
                dir.path().to_path_buf(),
                session_path(&dir, "manager-proj"),
                Some(4242),
                None,
            )
            .await
            .unwrap();
        let RegisterOutcome::Registered {
            run_id: first_run_id,
            ..
        } = first
        else {
            panic!("expected Registered");
        };
        let second = registry
            .register(
                &sup,
                "proj",
                dir.path().to_path_buf(),
                session_path(&dir, "manager-proj-2"),
                Some(4242),
                None,
            )
            .await
            .unwrap();
        let RegisterOutcome::Refreshed {
            run_id: second_run_id,
            ..
        } = second
        else {
            panic!("expected Refreshed, got {second:?}");
        };
        assert_eq!(first_run_id, second_run_id);

        let snap = sup.snapshot().await;
        assert_eq!(
            snap.runs.iter().filter(|r| r.driver == "external").count(),
            1
        );
    }

    #[tokio::test]
    async fn different_pid_reregister_is_refused_never_a_takeover() {
        let (sup, dir) = make_supervisor();
        let registry = ManagerRegistry::new();
        registry
            .register(
                &sup,
                "proj",
                dir.path().to_path_buf(),
                session_path(&dir, "manager-proj"),
                Some(4242),
                None,
            )
            .await
            .unwrap();
        let outcome = registry
            .register(
                &sup,
                "proj",
                dir.path().to_path_buf(),
                session_path(&dir, "manager-proj-2"),
                Some(9999),
                None,
            )
            .await
            .unwrap();
        let RegisterOutcome::Refused { message } = outcome else {
            panic!("expected Refused, got {outcome:?}");
        };
        assert!(
            message.contains("already registered externally"),
            "{message}"
        );

        let snap = sup.snapshot().await;
        assert_eq!(
            snap.runs.iter().filter(|r| r.driver == "external").count(),
            1
        );
    }

    #[tokio::test]
    async fn pidless_registration_requires_matching_holder_token_to_refresh() {
        let (sup, dir) = make_supervisor();
        let registry = ManagerRegistry::new();
        let first = registry
            .register(
                &sup,
                "proj",
                dir.path().to_path_buf(),
                session_path(&dir, "manager-proj"),
                None,
                None,
            )
            .await
            .unwrap();
        let RegisterOutcome::Registered {
            run_id,
            holder_token: Some(token),
        } = first
        else {
            panic!("PID-less registration must return a holder token: {first:?}");
        };

        let unrelated = registry
            .register(
                &sup,
                "proj",
                dir.path().to_path_buf(),
                session_path(&dir, "manager-proj-other"),
                None,
                None,
            )
            .await
            .unwrap();
        assert!(matches!(unrelated, RegisterOutcome::Refused { .. }));

        let refreshed = registry
            .register(
                &sup,
                "proj",
                dir.path().to_path_buf(),
                session_path(&dir, "manager-proj-refresh"),
                None,
                Some(token.clone()),
            )
            .await
            .unwrap();
        assert_eq!(
            refreshed,
            RegisterOutcome::Refreshed {
                run_id,
                holder_token: Some(token),
            }
        );
    }

    #[tokio::test]
    async fn register_and_app_launch_race_has_exactly_one_winner() {
        let (sup, dir) = make_supervisor();
        let registry = ManagerRegistry::new();
        let register = registry.register(
            &sup,
            "proj",
            dir.path().to_path_buf(),
            session_path(&dir, "manager-proj-external"),
            Some(4242),
            None,
        );
        let app_launch = sup.acquire(
            &ExternalManagerDriver,
            app_manager_request(&dir, "manager-proj-app"),
        );

        let (registered, launched) = tokio::join!(register, app_launch);
        match (registered.unwrap(), launched) {
            (RegisterOutcome::Registered { .. }, Err(SupervisorError::LeaseHeld { .. })) => {}
            (RegisterOutcome::Refused { message }, Ok(_)) => {
                assert!(message.contains("already running in the app"), "{message}");
            }
            (registration, launch) => {
                panic!("race must have one winner: registration={registration:?} launch={launch:?}")
            }
        }
        assert_eq!(
            sup.snapshot()
                .await
                .runs
                .iter()
                .filter(|run| run.task_id == "manager.launch:proj")
                .count(),
            1
        );
    }

    #[tokio::test]
    async fn pid_death_releases_run_on_next_sweep() {
        let (sup, dir) = make_supervisor();
        let registry = ManagerRegistry::new();
        let mut child = spawn_short_lived();
        let pid = child.id();
        let _ = child.wait();
        // Give the OS a moment past `wait()`'s reap.
        tokio::time::sleep(Duration::from_millis(50)).await;

        let outcome = registry
            .register(
                &sup,
                "proj",
                dir.path().to_path_buf(),
                session_path(&dir, "manager-proj"),
                Some(pid),
                None,
            )
            .await
            .unwrap();
        let RegisterOutcome::Registered { run_id, .. } = outcome else {
            panic!("expected Registered");
        };

        registry.sweep(&sup).await;

        let snap = sup.snapshot().await;
        assert!(!snap.runs.iter().any(|r| r.run_id == run_id));
    }

    #[tokio::test(start_paused = true)]
    async fn production_liveness_loop_releases_dead_pid_after_poll_tick() {
        let (sup, dir) = make_supervisor();
        let registry = ManagerRegistry::new();
        let mut child = spawn_short_lived();
        let pid = child.id();
        child.wait().unwrap();

        let outcome = registry
            .register(
                &sup,
                "proj",
                dir.path().to_path_buf(),
                session_path(&dir, "manager-proj-loop"),
                Some(pid),
                None,
            )
            .await
            .unwrap();
        let RegisterOutcome::Registered { run_id, .. } = outcome else {
            panic!("expected Registered");
        };

        spawn_liveness_loop(registry, sup.clone());
        tokio::task::yield_now().await;
        tokio::time::advance(LIVENESS_POLL_INTERVAL).await;
        for _ in 0..10 {
            if !sup
                .snapshot()
                .await
                .runs
                .iter()
                .any(|run| run.run_id == run_id)
            {
                return;
            }
            tokio::task::yield_now().await;
        }
        panic!("production liveness loop did not release dead PID registration");
    }

    #[tokio::test]
    async fn ttl_expiry_releases_pidless_registration() {
        let (sup, dir) = make_supervisor();
        let registry = ManagerRegistry::new();
        let outcome = registry
            .register(
                &sup,
                "proj",
                dir.path().to_path_buf(),
                session_path(&dir, "manager-proj"),
                None,
                None,
            )
            .await
            .unwrap();
        let RegisterOutcome::Registered { run_id, .. } = outcome else {
            panic!("expected Registered");
        };

        // Force the TTL to have already lapsed instead of waiting 15 minutes.
        {
            let mut map = registry.inner.lock().await;
            let entry = map.get_mut("proj").unwrap();
            entry.expires_at = Some(Instant::now() - Duration::from_secs(1));
        }

        registry.sweep(&sup).await;

        let snap = sup.snapshot().await;
        assert!(!snap.runs.iter().any(|r| r.run_id == run_id));
    }

    #[tokio::test]
    async fn explicit_release_deregisters_and_frees_lease() {
        // orgasmic:TASK-S52X9
        let (sup, dir) = make_supervisor();
        let registry = ManagerRegistry::new();
        let registered = registry
            .register(
                &sup,
                "proj",
                dir.path().to_path_buf(),
                session_path(&dir, "manager-proj"),
                Some(4242),
                None,
            )
            .await
            .unwrap();
        let run_id = match registered {
            RegisterOutcome::Registered { run_id, .. } => run_id,
            other => panic!("expected Registered, got {other:?}"),
        };

        let outcome = registry.release(&sup, "proj").await;
        assert!(matches!(outcome, ManagerReleaseOutcome::Released { .. }));

        // Operator-driven release writes the finalize tombstone as Completed.
        let path = session_path(&dir, "manager-proj");
        let envelopes = read_session_file(&path).unwrap();
        let release = envelopes.iter().rev().find_map(|envelope| {
            if envelope.kind != SessionEventKind::Lifecycle {
                return None;
            }
            serde_json::from_value::<Lifecycle>(envelope.event.clone()).ok()
        });
        match release {
            Some(Lifecycle::Release {
                reason,
                outcome,
                finalized_by_worker,
            }) => {
                assert_eq!(reason, "manager_released");
                assert_eq!(outcome, ReleaseOutcome::Completed);
                assert!(finalized_by_worker);
            }
            other => panic!("expected manager_released finalize tombstone, got {other:?}"),
        }
        assert!(!sup.snapshot().await.runs.iter().any(|r| r.run_id == run_id));

        // A no-op the second time.
        let second = registry.release(&sup, "proj").await;
        assert_eq!(second, ManagerReleaseOutcome::NotRegistered);

        // Lease is free: a fresh register succeeds instead of refreshing.
        let reregistered = registry
            .register(
                &sup,
                "proj",
                dir.path().to_path_buf(),
                session_path(&dir, "manager-proj-2"),
                Some(4242),
                None,
            )
            .await
            .unwrap();
        assert!(matches!(reregistered, RegisterOutcome::Registered { .. }));
    }

    #[tokio::test]
    async fn stale_registry_entry_self_heals_after_out_of_band_release() {
        // A run released through the generic /runs/:id/release path (the UI
        // End control) rather than through ManagerRegistry::release must not
        // wedge a subsequent register call.
        let (sup, dir) = make_supervisor();
        let registry = ManagerRegistry::new();
        let outcome = registry
            .register(
                &sup,
                "proj",
                dir.path().to_path_buf(),
                session_path(&dir, "manager-proj"),
                Some(4242),
                None,
            )
            .await
            .unwrap();
        let RegisterOutcome::Registered { run_id, .. } = outcome else {
            panic!("expected Registered");
        };

        sup.release(&run_id, "manual stop", ReleaseOutcome::Cancelled)
            .await
            .unwrap();

        let reregistered = registry
            .register(
                &sup,
                "proj",
                dir.path().to_path_buf(),
                session_path(&dir, "manager-proj-2"),
                Some(4242),
                None,
            )
            .await
            .unwrap();
        assert!(matches!(reregistered, RegisterOutcome::Registered { .. }));
    }
}
