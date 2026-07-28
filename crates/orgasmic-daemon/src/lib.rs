// arch: arch_C87Z9.3, arch_Z3Z3V.1
// orgasmic:arch_BVH7M, arch_C87Z9, arch_Z3Z3V
//! orgasmic daemon — HTTP/WS server, serialized writer, watcher, and the
//! materialized read index over project + home state.
//!
//! Public surface:
//! - [`Daemon::run`] boots the daemon and serves until shutdown.
//! - [`DaemonConfig`] is loaded from `$ORGASMIC_HOME/config.yaml` and may be
//!   overridden in the CLI (`--bind`, `--port`).
//! - [`ApiState`] is exposed so integration tests can spin up the router
//!   without a real listener.

pub mod addressing;
pub mod api;
pub mod artifacts;
pub mod auth;
pub mod authz;
pub mod boot_state;
pub mod config;
pub mod content;
pub mod events;
pub mod governance;
pub mod index;
pub mod logging;
pub mod manager_registration;
pub mod prompt_compiler;
pub mod recovery_claim;
pub mod runtime;
pub mod supervisor;
pub mod watcher;
pub mod writer;
pub mod ws;

#[cfg(test)]
pub(crate) mod test_fixtures;

use std::fs::{File, OpenOptions};
use std::io::{Seek, SeekFrom, Write};
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;
use std::{error, fmt};

use anyhow::{Context, Result};
use axum::Router;
use orgasmic_core::Home;
use orgasmic_drivers::modes::tmux;
use serde::Deserialize;
use tokio::net::TcpListener;
use tracing::{info, warn};

pub use crate::api::embedded_ui_asset_hash;
pub use crate::api::{router, ApiState};
pub use crate::artifacts::{ArtifactSummary, BLOCK_TYPES};
pub use crate::auth::AuthState;
pub use crate::boot_state::{
    boot_state_path, clear_boot_state_if_owner, read_boot_state, BootProgress, DaemonBootState,
    BOOT_STATE_FILE,
};
pub use crate::config::DaemonConfig;
pub use crate::content::SkillView;
pub use crate::events::{Event, EventBus, EventPayload, Topic};
pub use crate::index::{
    ActivityEntry, ActivityKind, BoardEntry, Index, IndexSnapshot, ParseError, ParseErrorKind,
    ProjectIndex, TaskId, TaskOwner, TaskSummary, TxRecord,
};
pub use crate::logging::{
    dropped_log_writes, ignore_sigpipe, init_tracing, init_tracing_to, LogMirror, DAEMON_OUT_LOG,
};
pub use crate::prompt_compiler::{
    CompiledPrompt, ContextPackView, PromptCompileRequest, PromptDiagnostic, PromptPartSaveRequest,
    PromptPartView, PromptSourceMapEntry, PromptSpecSaveRequest, PromptSpecView,
};
pub use crate::runtime::BootIdentity;
pub use crate::watcher::{spawn as spawn_watcher, WatcherConfig, WatcherHandle};
pub use crate::writer::{
    spawn as spawn_writer, FileRewrite, TxAppend, TxAppendResult, TxIdPolicy, WriterHandle,
};

/// Boot result that returns the bound socket address and a shutdown handle.
pub struct RunningDaemon {
    pub addr: SocketAddr,
    pub boot_id: String,
    pub shutdown: tokio::sync::oneshot::Sender<()>,
    pub join: tokio::task::JoinHandle<()>,
    // Keep the sender alive; dropping it closes command_loop and drops notify.
    _watcher: WatcherHandle,
}

#[derive(Debug, Clone)]
pub struct DaemonAlreadyRunning {
    pub addr: SocketAddr,
    pub boot_id: String,
    pub pid: u32,
}

impl fmt::Display for DaemonAlreadyRunning {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "orgasmic daemon already running at http://{} (pid={}, boot_id={})",
            self.addr, self.pid, self.boot_id
        )
    }
}

impl error::Error for DaemonAlreadyRunning {}

#[derive(Debug, Clone)]
pub struct DaemonInstanceLockHeld {
    pub path: PathBuf,
    pub detail: String,
}

impl fmt::Display for DaemonInstanceLockHeld {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "daemon instance lock {} is held, but the incumbent is not healthy: {}. Refusing to start a competing daemon",
            self.path.display(),
            self.detail
        )
    }
}

impl error::Error for DaemonInstanceLockHeld {}

#[derive(Debug, Clone)]
pub struct HistoricalTxStartupError {
    pub path: PathBuf,
    pub line: Option<usize>,
    pub message: String,
}

impl HistoricalTxStartupError {
    fn from_parse_error(error: ParseError) -> Self {
        Self {
            path: error.path,
            line: error.line,
            message: error.message,
        }
    }
}

impl fmt::Display for HistoricalTxStartupError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let line = self
            .line
            .map(|line| line.to_string())
            .unwrap_or_else(|| "line unknown".to_string());
        write!(
            f,
            "historical tx parse error blocks daemon start: {}:{}: {}. Revert the modified tx file (use git for project tx), or perform an explicit tx reseal before restarting.",
            self.path.display(),
            line,
            self.message,
        )
    }
}

impl error::Error for HistoricalTxStartupError {}

#[derive(Debug, Clone)]
pub struct DaemonOptions {
    pub bind_override: Option<std::net::IpAddr>,
    pub port_override: Option<u16>,
    pub actor: String,
    pub machine: String,
    /// How long the dispatch completion watcher lets a released run's session
    /// file settle before flushing artifacts without a terminal marker. Tests
    /// shrink this so grace-path coverage doesn't wait out the real window.
    pub dispatch_watcher_grace: std::time::Duration,
    /// Whether to spin up the `notify` filesystem watcher at boot. Production
    /// always wants this on; integration tests that never assert on
    /// watcher-driven index refresh disable it to avoid the per-watch macOS
    /// FSEvents registration latency (~0.8s each) that otherwise dominates the
    /// daemon boot critical path.
    pub fs_watcher_enabled: bool,
    /// Optional override (in whole seconds) for the tmux/claude driver's
    /// "input ready" detection timeout. Production leaves this `None` (the
    /// driver's own 10s default applies). Tests that drive a real claude tmux
    /// stage but only assert on the acquire-failure error shape shrink this so
    /// they don't wait out the full TUI-detection window.
    pub tmux_input_ready_timeout_secs: Option<u64>,
    /// Artificial delay before the dispatch HTTP handler returns. Production
    /// leaves this `None`. Tests use it to force the CLI dispatch timeout path
    /// while the daemon has already spawned the worker.
    pub dispatch_response_delay: Option<std::time::Duration>,
    /// Artificial delay between a run release and the terminal tx that release
    /// carries. Production leaves this `None` (the two are consecutive awaits).
    /// Tests widen the gap so a caller can be made to vanish inside it
    /// (TASK-WGXKD).
    pub release_terminal_tx_delay: Option<std::time::Duration>,
    /// Artificial delay between release admission and the detached spawn
    /// (tests only). See [`api::ApiState::release_admission_delay`].
    pub release_admission_delay: Option<std::time::Duration>,
    /// Trusted host-supplied executable implementing the internal
    /// `__exec-pinned` boundary. Production leaves this unset and uses the
    /// running orgasmic executable; process-isolated integration hosts may
    /// inject an owned executable wrapper.
    pub trusted_exec_wrapper_override: Option<PathBuf>,
    /// Override for the shutdown budgets this daemon spends after its shutdown
    /// signal. Production leaves this `None`, which means
    /// [`ShutdownBudgets::default`]. Tests shrink it so they can watch a real
    /// daemon outlive many drain budgets, or exhaust one, without waiting out
    /// the production numbers (TASK-R74E8).
    pub shutdown_budgets: Option<ShutdownBudgets>,
}

impl Default for DaemonOptions {
    fn default() -> Self {
        let actor = std::env::var("USER").unwrap_or_else(|_| "unknown".into());
        // `DaemonOptions::default()` is built before `Daemon::run` can take the
        // single-instance lock, so machine discovery must never spawn.
        let machine = resolve_machine_name(std::env::var("HOSTNAME").ok(), os_machine_name);
        Self {
            bind_override: None,
            port_override: None,
            actor,
            machine,
            dispatch_watcher_grace: std::time::Duration::from_secs(30),
            fs_watcher_enabled: true,
            tmux_input_ready_timeout_secs: None,
            dispatch_response_delay: None,
            release_terminal_tx_delay: None,
            release_admission_delay: None,
            trusted_exec_wrapper_override: None,
            shutdown_budgets: None,
        }
    }
}

fn resolve_machine_name(
    hostname_env: Option<String>,
    os_source: impl FnOnce() -> Option<String>,
) -> String {
    hostname_env
        .filter(|name| !name.trim().is_empty())
        .or_else(os_source)
        .filter(|name| !name.trim().is_empty())
        .unwrap_or_else(|| "unknown".to_string())
}

#[cfg(unix)]
fn os_machine_name() -> Option<String> {
    let mut buffer = [0_u8; 256];
    // SAFETY: `buffer` is valid for its full length and `gethostname` writes
    // at most that many bytes.
    if unsafe { libc::gethostname(buffer.as_mut_ptr().cast(), buffer.len()) } != 0 {
        return None;
    }
    let end = buffer
        .iter()
        .position(|byte| *byte == 0)
        .unwrap_or(buffer.len());
    Some(String::from_utf8_lossy(&buffer[..end]).trim().to_string())
}

#[cfg(not(unix))]
fn os_machine_name() -> Option<String> {
    std::env::var("COMPUTERNAME").ok()
}

const INCUMBENT_PROBE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(1);
const DAEMON_LOCK_RETRY_BUDGET: std::time::Duration = std::time::Duration::from_millis(125);
const DAEMON_LOCK_RETRY_STEP: std::time::Duration = std::time::Duration::from_millis(10);

#[derive(Debug, Deserialize)]
struct IncumbentStatus {
    boot_id: String,
    pid: u32,
    home: PathBuf,
}

fn daemon_lock_path(home: &Home) -> PathBuf {
    home.root.join("daemon.lock")
}

fn open_and_try_lock_daemon(home: &Home) -> Result<std::result::Result<File, PathBuf>> {
    std::fs::create_dir_all(&home.root)
        .with_context(|| format!("create {}", home.root.display()))?;
    let path = daemon_lock_path(home);
    let mut file = OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(&path)
        .with_context(|| format!("open {}", path.display()))?;
    match fs2::FileExt::try_lock_exclusive(&file) {
        Ok(()) => {
            file.set_len(0)
                .with_context(|| format!("truncate {}", path.display()))?;
            file.seek(SeekFrom::Start(0))
                .with_context(|| format!("seek {}", path.display()))?;
            writeln!(file, "{}", std::process::id())
                .with_context(|| format!("write {}", path.display()))?;
            file.sync_data()
                .with_context(|| format!("sync {}", path.display()))?;
            Ok(Ok(file))
        }
        Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => Ok(Err(path)),
        Err(error) => Err(error).with_context(|| format!("lock {}", path.display())),
    }
}

async fn acquire_daemon_lock(
    home: &Home,
    opts: &DaemonOptions,
) -> Result<std::result::Result<File, DaemonAlreadyRunning>> {
    let deadline = std::time::Instant::now() + DAEMON_LOCK_RETRY_BUDGET;
    let lock_path = loop {
        match open_and_try_lock_daemon(home)? {
            Ok(file) => return Ok(Ok(file)),
            Err(path) if std::time::Instant::now() >= deadline => break path,
            Err(_) => tokio::time::sleep(DAEMON_LOCK_RETRY_STEP).await,
        }
    };

    let mut cfg = DaemonConfig::load(home).map_err(|error| DaemonInstanceLockHeld {
        path: lock_path.clone(),
        detail: format!("cannot load incumbent address from config: {error}"),
    })?;
    if let Some(bind) = opts.bind_override {
        cfg = cfg.with_bind(bind);
    }
    if let Some(port) = opts.port_override {
        cfg = cfg.with_port(port);
    }
    let probe_host = if cfg.bind.is_unspecified() {
        if cfg.bind.is_ipv4() {
            "127.0.0.1".parse().expect("valid IPv4 loopback")
        } else {
            "::1".parse().expect("valid IPv6 loopback")
        }
    } else {
        cfg.bind
    };
    let addr = SocketAddr::new(probe_host, cfg.port);
    let token = std::fs::read_to_string(home.auth_token())
        .ok()
        .map(|token| token.trim().to_string())
        .filter(|token| !token.is_empty());
    let Some(token) = token else {
        return Err(DaemonInstanceLockHeld {
            path: lock_path,
            detail: "the incumbent has not created an auth token yet (it may still be booting)"
                .to_string(),
        }
        .into());
    };
    if cfg.port == 0 {
        return Err(DaemonInstanceLockHeld {
            path: lock_path,
            detail: "the configured port is 0, so the incumbent address cannot be probed"
                .to_string(),
        }
        .into());
    }
    let client = reqwest::Client::builder()
        .timeout(INCUMBENT_PROBE_TIMEOUT)
        .build()
        .context("build incumbent daemon probe client")?;
    let response = client
        .get(format!("http://{addr}/api/daemon/status"))
        .bearer_auth(token)
        .send()
        .await;
    let response = match response {
        Ok(response) => response,
        Err(error) => {
            return Err(DaemonInstanceLockHeld {
                path: lock_path,
                detail: format!("HTTP health probe at http://{addr} failed: {error}"),
            }
            .into())
        }
    };
    if !response.status().is_success() {
        return Err(DaemonInstanceLockHeld {
            path: lock_path,
            detail: format!(
                "HTTP health probe at http://{addr} returned {}",
                response.status()
            ),
        }
        .into());
    }
    let status: IncumbentStatus = response
        .json()
        .await
        .context("parse incumbent daemon status")?;
    if status.home != home.root {
        return Err(DaemonInstanceLockHeld {
            path: lock_path,
            detail: format!(
                "HTTP health probe reached a daemon for {}, not {}",
                status.home.display(),
                home.root.display()
            ),
        }
        .into());
    }
    Ok(Err(DaemonAlreadyRunning {
        addr,
        boot_id: status.boot_id,
        pid: status.pid,
    }))
}

fn bind_delay_for_tests() -> Option<std::time::Duration> {
    std::env::var("ORGASMIC_TEST_BIND_DELAY_MS")
        .ok()
        .and_then(|raw| raw.parse::<u64>().ok())
        .filter(|millis| *millis > 0)
        .map(std::time::Duration::from_millis)
}

/// Bind the daemon listener, tolerating a briefly-held port during a
/// runtime-swap restart. When `orgasmic update` stops the old daemon and starts
/// the new one, the predecessor drains gracefully and its listener (or lingering
/// connections) can keep the port occupied for a moment after it stops answering
/// health probes. Retry `AddrInUse` for a few seconds instead of exiting, so the
/// stop→start handoff does not race the OS releasing the port.
async fn bind_listener_with_retry(addr: SocketAddr) -> std::io::Result<TcpListener> {
    const RETRY_BUDGET: std::time::Duration = std::time::Duration::from_secs(8);
    const RETRY_STEP: std::time::Duration = std::time::Duration::from_millis(200);
    let deadline = std::time::Instant::now() + RETRY_BUDGET;
    loop {
        match TcpListener::bind(addr).await {
            Ok(listener) => return Ok(listener),
            Err(err)
                if err.kind() == std::io::ErrorKind::AddrInUse
                    && std::time::Instant::now() < deadline =>
            {
                info!(%addr, "port busy during startup (predecessor still releasing it); retrying bind");
                tokio::time::sleep(RETRY_STEP).await;
            }
            Err(err) => return Err(err),
        }
    }
}

pub struct Daemon;

impl Daemon {
    pub async fn run(home: Home, opts: DaemonOptions) -> Result<RunningDaemon> {
        let instance_lock = match acquire_daemon_lock(&home, &opts).await? {
            Ok(lock) => lock,
            Err(incumbent) => return Err(incumbent.into()),
        };
        // Lock ownership precedes boot work: publish heartbeat before any slow
        // pre-bind phase so the CLI can distinguish progress from death.
        let mut boot_progress = boot_state::BootProgress::start(&home, "loading config")?;
        home.ensure().context("ensure orgasmic home")?;
        let mut cfg = DaemonConfig::load(&home)?;
        if let Some(bind) = opts.bind_override {
            cfg = cfg.with_bind(bind);
        }
        if let Some(port) = opts.port_override {
            cfg = cfg.with_port(port);
        }
        // Durable file sink under $ORGASMIC_HOME/logs; stdout is a best-effort
        // mirror so a closed pipe cannot poison request handling (TASK-FZF2D).
        let daemon_log = home.logs().join(DAEMON_OUT_LOG);
        init_tracing_to(&cfg.log_level, Some(&daemon_log), LogMirror::Stdout);
        for key in &cfg.unrecognized_keys {
            warn!(
                key = %key,
                path = %home.config().display(),
                "unrecognized key in orgasmic config.yaml (ignored)"
            );
        }
        let legacy_workers = home.user().join("workers");
        if legacy_workers.is_dir() {
            warn!(
                path = %legacy_workers.display(),
                "legacy user/workers directory ignored; worker templates were retired"
            );
        }
        let boot = Arc::new(BootIdentity::new());
        let events = EventBus::new();
        events.publish(
            Topic::Daemon,
            EventPayload::DaemonStarted {
                boot_id: boot.boot_id.clone(),
            },
        );

        boot_progress.set_phase("loading auth")?;
        let token = auth::load_or_generate(&home)?;
        let auth_state = AuthState::new(token);

        let prebind_addr = SocketAddr::new(cfg.bind, cfg.port);
        info!(
            address = %prebind_addr,
            boot_id = %boot.boot_id,
            home = %home.root.display(),
            "orgasmic daemon starting pre-bind boot work"
        );

        boot_progress.set_phase("scanning projects")?;
        boot_progress.start_refresh_loop(boot_state::default_refresh_interval());
        let index = Index::new(home.clone());
        // AC #1: rebuild before serving normal reads.
        index.rebuild().await;
        boot_progress.stop_refresh_loop();
        let initial_snapshot = index.snapshot().await;
        if let Some(error) = initial_snapshot.first_historical_tx_parse_error().cloned() {
            return Err(HistoricalTxStartupError::from_parse_error(error).into());
        }

        // One-shot relocation of legacy home-level session transcripts into each
        // project's `.orgasmic/tmp/sessions/` (per-project tmp). No-op once the
        // legacy dir is drained.
        boot_progress.set_phase("migrating sessions")?;
        let migrate_projects: Vec<(String, PathBuf)> = initial_snapshot
            .board
            .iter()
            .map(|entry| (entry.id.clone(), entry.path.clone()))
            .collect();
        api::migrate_legacy_home_sessions(&home, &migrate_projects);

        boot_progress.set_phase("starting runtime")?;
        let writer = spawn_writer(events.clone());
        let supervisor = supervisor::Supervisor::new(writer.clone(), boot.clone());
        let manager_registry = manager_registration::ManagerRegistry::new();
        manager_registration::spawn_liveness_loop(manager_registry.clone(), supervisor.clone());
        index.spawn_tx_listener(events.clone());

        // Project roots for boot auto-reattach, run once `api_state` exists
        // below (the dispatch completion watcher it may respawn needs the
        // full `ApiState`, not just `home`/`supervisor`).
        let reattach_roots: Vec<PathBuf> = migrate_projects
            .iter()
            .map(|(_, root)| root.clone())
            .collect();

        boot_progress.set_phase("attaching watchers")?;
        let watcher = spawn_watcher(
            home.clone(),
            index.clone(),
            events.clone(),
            WatcherConfig {
                debounce: std::time::Duration::from_millis(cfg.watcher_debounce_ms),
                enabled: opts.fs_watcher_enabled,
            },
        )?;
        for entry in initial_snapshot.board {
            if let Err(e) = watcher.watch_project(entry.path.clone()).await {
                tracing::warn!(project = %entry.id, error = %e, "watch project failed");
            }
        }

        let default_tx_path = home.tx().join("YYYY-MM.org");
        // Graceful-shutdown fanout: ws/PTY connection tasks watch this so the
        // axum drain phase can't deadlock behind a still-connected client.
        let (shutdown_signal_tx, shutdown_signal_rx) = tokio::sync::watch::channel(false);
        let api_state = ApiState {
            home: home.clone(),
            index: index.clone(),
            writer: writer.clone(),
            supervisor,
            manager_driver: Arc::new(tmux::driver()),
            manager_registry,
            events: events.clone(),
            boot: boot.clone(),
            auth: auth_state,
            default_tx_path,
            tx_commit_to_project: cfg.tx_commit_to_project,
            manager_actor: cfg.manager_actor.clone(),
            auto_commit_signal: cfg.auto_commit_signal,
            driver_defaults: cfg.driver_defaults.clone(),
            dispatch_governance: cfg.dispatch_governance.clone(),
            actor: opts.actor.clone(),
            machine: opts.machine.clone(),
            bind_host: cfg.bind.to_string(),
            bind_port: cfg.port,
            ui_asset_hash: api::embedded_ui_asset_hash(),
            shutdown: shutdown_signal_rx,
            dispatch_watcher_grace: opts.dispatch_watcher_grace,
            tmux_input_ready_timeout_secs: opts.tmux_input_ready_timeout_secs,
            dispatch_response_delay: opts.dispatch_response_delay,
            release_terminal_tx_delay: opts.release_terminal_tx_delay,
            release_admission_delay: opts.release_admission_delay,
            artifact_write_locks: Arc::new(std::sync::Mutex::new(std::collections::HashMap::new())),
            recovery_claim_locks: Arc::new(std::sync::Mutex::new(std::collections::HashMap::new())),
            recovery_status_lock: Arc::new(tokio::sync::Mutex::new(())),
            trusted_claude_binary: api::pin_trusted_claude_binary(&home),
            trusted_exec_wrapper: opts.trusted_exec_wrapper_override.clone(),
            release_tasks: api::ReleaseTaskTracker::new(),
        };

        // Boot auto-reattach runs *after* the listener is bound (see below). It
        // reads every session file in every project, so a single unreadable one
        // used to hold the whole runtime pre-bind: no `status`, no UI, no CLI —
        // launchd respawning replacements that refuse on the instance lock, and
        // a WARN in a log nobody was watching as the only signal. Reproduced
        // 2026-07-25 and confirmed by stack sample: blocked in
        // `read_session_file` -> `read_to_string` on the main thread inside
        // `block_on`, which also starved the boot-progress heartbeat, so even
        // the phase readout froze. TASK-KKGKM.
        let reattach_state = api_state.clone();
        // orgasmic:TASK-WGXKD.1 — shutdown's handle on the detached release
        // finalizations. Taken before the state moves into the router.
        let release_tasks = api_state.release_tasks.clone();

        let app: Router = router(api_state);
        let addr = SocketAddr::new(cfg.bind, cfg.port);
        // The pre-bind delay can be deliberately slow in tests and can also
        // cover a draining predecessor in production. Report it as a phase
        // and keep liveness fresh, but leave it to the CLI to decide whether
        // phase progress has actually occurred.
        boot_progress.set_phase("waiting to bind listener")?;
        boot_progress.start_refresh_loop(boot_state::default_refresh_interval());
        if let Some(delay) = bind_delay_for_tests() {
            tokio::time::sleep(delay).await;
        }
        if let Some(hold) = boot_state::prebind_hold_for_tests() {
            boot_progress.set_phase("test boot hold")?;
            tokio::time::sleep(hold).await;
        }
        boot_progress.set_phase("binding listener")?;
        let listener = bind_listener_with_retry(addr)
            .await
            .with_context(|| format!("bind {addr}"))?;
        boot_progress.stop_refresh_loop();
        let local_addr = listener.local_addr().context("local_addr")?;
        info!(
            address = %local_addr,
            boot_id = %boot.boot_id,
            home = %home.root.display(),
            "orgasmic daemon listening"
        );
        // Ready: retire boot heartbeat so readers do not keep reporting phases.
        boot_progress.retire();

        // Rehydrate still-live runs (notably the operator's manager terminal)
        // against their surviving mux sessions so a daemon restart is
        // transparent. Runs whose mux session is gone are skipped, not
        // interrupted; reattached dispatch runs get their completion watcher
        // respawned (TASK-567JG).
        //
        // Deliberately after bind and off the runtime threads: this is
        // best-effort recovery, while answering `status` is what an operator
        // needs in order to diagnose anything at all. A project that cannot be
        // read now costs its own runs' reattachment and nothing else.
        tokio::spawn(async move {
            api::reattach_live_runs_on_boot(&reattach_state, &reattach_roots).await;
        });
        let (shutdown_tx, shutdown_rx) = tokio::sync::oneshot::channel::<()>();
        let writer_for_shutdown = writer.clone();
        let home_for_shutdown = home.clone();
        let boot_id_for_shutdown = boot.boot_id.clone();
        let budgets = opts.shutdown_budgets.unwrap_or_default();
        let join = tokio::spawn(async move {
            // The exclusive home lock must outlive every daemon-owned task and
            // listener. Keeping it in the serve task releases it only after
            // graceful shutdown has completed.
            let _instance_lock = instance_lock;
            // orgasmic:TASK-R74E8 — the drain begins when the shutdown signal
            // fires, and that is the only interval the drain budget may cover.
            // `axum::serve(..).with_graceful_shutdown(..)` resolves only after
            // shutdown has been *requested* and the connections have then
            // drained, so as a future it measures the entire life of the
            // server. TASK-Q07Y5 wrapped that future in the drain budget, which
            // killed a healthy, serving daemon exactly `connection_drain` after
            // it bound — on every boot, with launchd restarting it into the same
            // fate. This oneshot marks the real start of the drain so the budget
            // starts where the drain does.
            let (drain_started_tx, drain_started_rx) = tokio::sync::oneshot::channel::<()>();
            let serve_task = tokio::spawn(async move {
                axum::serve(listener, app)
                    .with_graceful_shutdown(async move {
                        let _ = shutdown_rx.await;
                        // Wake long-lived connection tasks before draining connections.
                        let _ = shutdown_signal_tx.send(true);
                        let _ = drain_started_tx.send(());
                    })
                    .await
            });
            let serve_abort = serve_task.abort_handle();
            // Deliberately unbounded: this is the wait *for* the signal, and a
            // daemon nobody has asked to stop must never stop. It also resolves
            // (as an `Err`) if the serve task ends on its own — an accept-loop
            // failure drops the sender — in which case the awaited task below is
            // already finished and the timeout is satisfied immediately.
            let _ = drain_started_rx.await;
            // orgasmic:TASK-Q07Y5 — the connection drain is bounded for the
            // same reason the writer shutdown now is: a still-connected client
            // (ws, PTY, SSE) would otherwise make the SIGTERM path unbounded,
            // and the service-manager kill timeout is derived from this sum.
            // Giving up abandons the remaining connection tasks — they die with
            // the process moments later — rather than waiting on a client that
            // will not let go. The release finalizations are detached tasks and
            // are drained next.
            match tokio::time::timeout(budgets.connection_drain, serve_task).await {
                Ok(Ok(Ok(()))) => {}
                Ok(Ok(Err(err))) => {
                    tracing::error!(error = %err, "orgasmic daemon exited with error");
                }
                Ok(Err(err)) => {
                    tracing::error!(error = %err, "orgasmic daemon serve task ended abnormally");
                }
                Err(_) => {
                    serve_abort.abort();
                    tracing::error!(
                        budget_secs = budgets.connection_drain.as_secs(),
                        "connection drain did not finish within its budget; \
                         abandoning remaining connections and continuing shutdown"
                    );
                }
            }
            graceful_shutdown(
                &home_for_shutdown,
                &boot_id_for_shutdown,
                &release_tasks,
                &writer_for_shutdown,
                budgets,
            )
            .await;
        });
        index.spawn_repo_url_refresh();

        Ok(RunningDaemon {
            addr: local_addr,
            boot_id: boot.boot_id.clone(),
            shutdown: shutdown_tx,
            join,
            _watcher: watcher,
        })
    }
}

/// Ceiling on axum's connection drain, the first phase of the shutdown path.
///
/// orgasmic:TASK-Q07Y5 — long-lived connections (ws, PTY, SSE) are woken by the
/// shutdown signal, but "woken" is not "finished". This is the term that keeps
/// the phase finite so the total below is a real number.
pub const CONNECTION_DRAIN_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(10);

/// Every budget the SIGTERM/Ctrl+C path may spend, in the order it spends them.
///
/// orgasmic:TASK-Q07Y5 — this type exists so the service-manager kill timeout
/// can be *derived* from the shutdown cost instead of guessed against it
/// (TASK-WGXKD.2 finding 1), and so a test can drive the real shutdown
/// composition with short budgets instead of testing its phases in isolation
/// (finding 2). Production always uses [`ShutdownBudgets::default`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ShutdownBudgets {
    pub connection_drain: std::time::Duration,
    pub release_drain: std::time::Duration,
    pub writer_shutdown: std::time::Duration,
}

impl Default for ShutdownBudgets {
    fn default() -> Self {
        Self {
            connection_drain: CONNECTION_DRAIN_TIMEOUT,
            release_drain: api::RELEASE_FINALIZATION_DRAIN_TIMEOUT,
            writer_shutdown: writer::WRITER_SHUTDOWN_TIMEOUT,
        }
    }
}

impl ShutdownBudgets {
    /// Worst-case wall clock from signal to exit, excluding only the constant
    /// cost of writing the loss record and unwinding the process.
    pub fn total(&self) -> std::time::Duration {
        self.connection_drain + self.release_drain + self.writer_shutdown
    }
}

/// What a graceful shutdown could not prove it wrote.
///
/// orgasmic:TASK-Q07Y5 — the in-memory warnings the restart endpoint returns
/// die with the process, and a SIGTERM shutdown has no client to return them
/// to at all. This record is written straight to disk (not through the writer,
/// which is the component that just failed to stop) before the process exits,
/// so the runs at risk survive the shutdown that put them at risk.
#[derive(Debug, Clone, serde::Serialize, Deserialize)]
pub struct ShutdownLossRecord {
    pub boot_id: String,
    pub recorded_at: chrono::DateTime<chrono::Utc>,
    pub writer_shutdown: writer::WriterShutdownOutcome,
    pub writer_shutdown_budget_ms: u64,
    /// Runs whose release finalization was still in flight when the release
    /// drain expired: their terminal tx was not observed to land.
    #[serde(default)]
    pub outstanding_release_runs: Vec<String>,
    /// Runs whose release finalization ended without its terminal tx.
    #[serde(default)]
    pub lost_release_finalizations: Vec<api::LostReleaseFinalization>,
    pub rescue: String,
}

/// Directory holding [`ShutdownLossRecord`]s, one file per boot that lost work.
pub fn shutdown_loss_dir(home: &Home) -> PathBuf {
    home.state().join("shutdown-loss")
}

/// Persist a loss record with the durability the lost writes did not get:
/// `fsync` on the file and on its directory before returning.
fn write_shutdown_loss_record(
    home: &Home,
    record: &ShutdownLossRecord,
) -> std::io::Result<PathBuf> {
    let dir = shutdown_loss_dir(home);
    std::fs::create_dir_all(&dir)?;
    let path = dir.join(format!("{}.json", record.boot_id));
    let body = serde_json::to_vec_pretty(record)
        .map_err(|err| std::io::Error::new(std::io::ErrorKind::InvalidData, err))?;
    let mut file = File::create(&path)?;
    file.write_all(&body)?;
    file.write_all(b"\n")?;
    file.sync_all()?;
    // A record the crash-consistent directory entry never reached is not a
    // record. `fsync` on the directory is best-effort: some filesystems refuse
    // to open a directory for the purpose, which is not a reason to lose the
    // file itself.
    if let Ok(dir_handle) = File::open(&dir) {
        let _ = dir_handle.sync_all();
    }
    Ok(path)
}

/// The daemon's own graceful shutdown, after the listener has stopped serving.
///
/// Order is load-bearing (TASK-WGXKD.1): a release finalization outlives its
/// request, so outstanding releases must reach the writer before the writer is
/// stopped. Every phase is bounded (TASK-Q07Y5), and anything still unproven at
/// the end is written to a durable [`ShutdownLossRecord`] naming the runs.
///
/// Returns the record path when one was written.
pub async fn graceful_shutdown(
    home: &Home,
    boot_id: &str,
    release_tasks: &api::ReleaseTaskTracker,
    writer: &WriterHandle,
    budgets: ShutdownBudgets,
) -> Option<PathBuf> {
    // orgasmic:TASK-WGXKD.1 — a release finalization outlives its request by
    // design, so axum's connection drain says nothing about it. Stop accepting
    // new ones, let the outstanding ones reach the writer, and only then stop
    // the writer. Doing this after the writer shutdown (or not at all) loses
    // the terminal tx of any finalize that was mid-teardown when the restart
    // landed.
    release_tasks.close();
    let outstanding = match release_tasks.wait_idle(budgets.release_drain).await {
        Ok(()) => Vec::new(),
        Err(outstanding) => {
            // orgasmic:TASK-WGXKD.2 — name the runs. "3 outstanding" is not
            // something an operator can act on; a run id is.
            tracing::error!(
                outstanding = outstanding.len(),
                run_ids = %outstanding.join(", "),
                "shutting down with release finalizations still in flight; \
                 their terminal tx may be lost — rescue with `orgasmic recovery \
                 status` then `orgasmic run recover <run_id>`"
            );
            outstanding
        }
    };
    let lost = release_tasks.lost_finalizations();
    for entry in &lost {
        tracing::error!(
            run_id = %entry.run_id,
            terminal_tx_type = entry.terminal_tx_type.as_deref().unwrap_or("-"),
            reason = %entry.reason,
            "release finalization on this daemon ended without its terminal tx"
        );
    }
    let writer_shutdown = writer.shutdown_within(budgets.writer_shutdown).await;
    if let writer::WriterShutdownOutcome::TimedOut { queued, in_flight } = &writer_shutdown {
        tracing::error!(
            budget_secs = budgets.writer_shutdown.as_secs(),
            queued = queued,
            in_flight = ?in_flight,
            "writer did not stop within its shutdown budget; writes it had \
             accepted are not proven durable"
        );
    }
    if writer_shutdown.is_clean() && outstanding.is_empty() && lost.is_empty() {
        return None;
    }
    let record = ShutdownLossRecord {
        boot_id: boot_id.to_string(),
        recorded_at: chrono::Utc::now(),
        writer_shutdown,
        writer_shutdown_budget_ms: budgets.writer_shutdown.as_millis() as u64,
        outstanding_release_runs: outstanding,
        lost_release_finalizations: lost,
        rescue: "orgasmic recovery status, then `orgasmic run recover <run_id>` \
                 for each run named here"
            .to_string(),
    };
    match write_shutdown_loss_record(home, &record) {
        Ok(path) => {
            tracing::error!(
                path = %path.display(),
                "shutdown could not prove every accepted write landed; \
                 recorded what is at risk"
            );
            Some(path)
        }
        Err(error) => {
            tracing::error!(
                error = %error,
                dir = %shutdown_loss_dir(home).display(),
                "failed to write the shutdown loss record; the runs at risk are \
                 in this log only"
            );
            None
        }
    }
}

/// Default tx path the daemon will write to from RPC posts: one file per
/// calendar month under `$ORGASMIC_HOME/state/tx/`.
pub fn default_home_tx_path(home: &Home) -> PathBuf {
    let now = chrono::Utc::now();
    home.tx().join(format!("{}.org", now.format("%Y-%m")))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn machine_name_uses_non_spawning_os_source_when_hostname_is_absent() {
        let machine = resolve_machine_name(None, || Some("stable-os-machine".to_string()));

        assert_eq!(machine, "stable-os-machine");
        assert_ne!(machine, "unknown");
    }

    #[test]
    fn machine_name_prefers_explicit_environment_value() {
        let machine = resolve_machine_name(Some("explicit-machine".to_string()), || {
            panic!("OS source must not replace an explicit machine name")
        });

        assert_eq!(machine, "explicit-machine");
    }

    /// orgasmic:TASK-R74E8 — the regression that no gate could see.
    ///
    /// TASK-Q07Y5 bounded the connection drain by wrapping
    /// `axum::serve(..).with_graceful_shutdown(..)` in `timeout(connection_drain,
    /// ..)`. That future is not the drain; it is the whole life of the server.
    /// The daemon therefore shut itself down `connection_drain` after it bound —
    /// 10s in production — on every boot, and launchd restarted it into the same
    /// fate. Every existing daemon test tore its daemon down in well under a
    /// second, so the entire suite stayed green.
    ///
    /// This test is the missing one: a real daemon on a real listener, still
    /// answering requests after many multiples of its drain budget. It fails on
    /// the first probe past the budget if the timeout is ever put back around the
    /// server future.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn daemon_serves_far_beyond_its_connection_drain_budget() {
        /// Short enough to keep the test fast, long enough that the bad shape
        /// cannot be mistaken for a slow boot.
        const DRAIN: std::time::Duration = std::time::Duration::from_millis(400);
        /// How many drain budgets the daemon must survive while serving.
        const MULTIPLES: u32 = 8;

        let tmp = tempfile::tempdir().unwrap();
        let home = Home::at(tmp.path().join("home"));
        let budgets = ShutdownBudgets {
            connection_drain: DRAIN,
            release_drain: DRAIN,
            writer_shutdown: DRAIN,
        };
        let running = Daemon::run(
            home,
            DaemonOptions {
                bind_override: Some("127.0.0.1".parse().unwrap()),
                port_override: Some(0),
                fs_watcher_enabled: false,
                shutdown_budgets: Some(budgets),
                ..Default::default()
            },
        )
        .await
        .unwrap();
        let addr = running.addr;
        let boot_id = running.boot_id.clone();
        let token = std::fs::read_to_string(tmp.path().join("home/user/auth/token")).unwrap();
        let client = reqwest::Client::new();

        let started = std::time::Instant::now();
        for multiple in 1..=MULTIPLES {
            tokio::time::sleep(DRAIN).await;
            // The serve task must still be running. Checked before the request
            // so a daemon that died is reported as dead rather than as a
            // connection error somewhere in reqwest.
            assert!(
                !running.join.is_finished(),
                "the daemon shut itself down after {:?} of uptime ({multiple} x its \
                 {DRAIN:?} connection-drain budget) without being asked to stop; \
                 the drain budget is bounding the server's lifetime instead of \
                 its drain",
                started.elapsed()
            );
            let resp = client
                .get(format!("http://{addr}/api/daemon/status"))
                .bearer_auth(token.trim())
                .send()
                .await
                .unwrap_or_else(|err| {
                    panic!(
                        "daemon stopped answering after {:?} of uptime \
                         ({multiple} x {DRAIN:?}): {err}",
                        started.elapsed()
                    )
                });
            assert!(
                resp.status().is_success(),
                "status at {:?} of uptime: {:?}",
                started.elapsed(),
                resp.status()
            );
            let body: serde_json::Value = resp.json().await.unwrap();
            // Same boot throughout: a launchd-style respawn would answer here
            // too, but under a new boot id.
            assert_eq!(
                body["boot_id"].as_str().unwrap(),
                boot_id,
                "boot id changed at {:?} of uptime; the daemon restarted",
                started.elapsed()
            );
        }

        // The other half of the claim: the bound is still real. Asked to stop,
        // this daemon does so inside the budget it was given, rather than the
        // fix having simply removed the drain ceiling.
        let stop_started = std::time::Instant::now();
        let _ = running.shutdown.send(());
        tokio::time::timeout(
            budgets.total() + std::time::Duration::from_secs(5),
            running.join,
        )
        .await
        .unwrap_or_else(|_| {
            panic!(
                "shutdown did not complete within {:?}; the drain is unbounded",
                budgets.total()
            )
        })
        .expect("shutdown task panicked");
        assert!(
            stop_started.elapsed() < budgets.total() + std::time::Duration::from_secs(5),
            "shutdown took {:?}",
            stop_started.elapsed()
        );
    }

    /// orgasmic:TASK-R74E8 — the other side of the same coin: moving the budget
    /// off the server future must not have made the drain unbounded again.
    ///
    /// The client here sends request headers announcing a body and then stops
    /// writing, so hyper holds the connection open waiting for bytes that never
    /// arrive. Nothing wakes it: the shutdown signal reaches the daemon's own
    /// long-lived connection tasks, not a peer that has simply gone quiet
    /// mid-request. Without a ceiling on the drain, the shutdown never returns
    /// and the derived `ExitTimeOut` bounds nothing.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn shutdown_gives_up_on_a_connection_that_never_finishes_its_request() {
        use tokio::io::AsyncWriteExt as _;

        const DRAIN: std::time::Duration = std::time::Duration::from_millis(400);

        let tmp = tempfile::tempdir().unwrap();
        let home = Home::at(tmp.path().join("home"));
        let budgets = ShutdownBudgets {
            connection_drain: DRAIN,
            release_drain: DRAIN,
            writer_shutdown: DRAIN,
        };
        let running = Daemon::run(
            home,
            DaemonOptions {
                bind_override: Some("127.0.0.1".parse().unwrap()),
                port_override: Some(0),
                fs_watcher_enabled: false,
                shutdown_budgets: Some(budgets),
                ..Default::default()
            },
        )
        .await
        .unwrap();
        let addr = running.addr;
        let token = std::fs::read_to_string(tmp.path().join("home/user/auth/token")).unwrap();

        // Authenticated, so the request reaches a handler that wants the body
        // rather than being rejected before the connection is ever in flight.
        let mut stuck = tokio::net::TcpStream::connect(addr).await.unwrap();
        stuck
            .write_all(
                format!(
                    "POST /api/tx HTTP/1.1\r\nHost: {addr}\r\nAuthorization: Bearer {}\r\n\
                     Content-Type: application/json\r\nContent-Length: 4096\r\n\r\n{{",
                    token.trim()
                )
                .as_bytes(),
            )
            .await
            .unwrap();
        stuck.flush().await.unwrap();
        // Let the daemon accept it and start reading the body it will never get.
        tokio::time::sleep(std::time::Duration::from_millis(250)).await;

        let started = std::time::Instant::now();
        let _ = running.shutdown.send(());
        // The ceiling under test is `connection_drain`; the margin covers the
        // rest of the shutdown path plus scheduling on a loaded test binary.
        let ceiling = budgets.total() + std::time::Duration::from_secs(5);
        tokio::time::timeout(ceiling, running.join)
            .await
            .expect(
                "shutdown never returned while a peer held a half-sent request open; \
                 the connection drain has no ceiling, so the derived ExitTimeOut \
                 bounds nothing and launchd's SIGKILL lands mid-shutdown",
            )
            .expect("shutdown task panicked");
        let elapsed = started.elapsed();

        assert!(
            elapsed >= DRAIN,
            "shutdown returned in {elapsed:?}, before its own drain budget {DRAIN:?} — \
             the stuck connection was not actually holding the drain, so this test \
             would not notice if the ceiling were removed"
        );
        assert!(
            elapsed < ceiling,
            "shutdown took {elapsed:?}, past its whole budget {:?}",
            budgets.total()
        );
        drop(stuck);
    }

    #[tokio::test]
    async fn daemon_boots_and_status_reports_boot_id() {
        let tmp = tempfile::tempdir().unwrap();
        let home = Home::at(tmp.path().join("home"));
        let running = Daemon::run(
            home,
            DaemonOptions {
                bind_override: Some("127.0.0.1".parse().unwrap()),
                port_override: Some(0),
                ..Default::default()
            },
        )
        .await
        .unwrap();
        let addr = running.addr;
        let boot_id = running.boot_id.clone();

        // Read the generated token from the tempdir.
        let token = std::fs::read_to_string(tmp.path().join("home/user/auth/token")).unwrap();

        let client = reqwest::Client::new();
        let resp = client
            .get(format!("http://{}/api/daemon/status", addr))
            .bearer_auth(token.trim())
            .send()
            .await
            .unwrap();
        assert!(resp.status().is_success(), "status: {:?}", resp.status());
        let body: serde_json::Value = resp.json().await.unwrap();
        assert_eq!(body["boot_id"].as_str().unwrap(), boot_id);
        assert!(body["pid"].as_u64().is_some());

        let _ = running.shutdown.send(());
        let _ = running.join.await;
    }

    /// orgasmic:TASK-Q07Y5 — the whole shutdown composition, not its phases in
    /// isolation (TASK-WGXKD.2 finding 2).
    ///
    /// A release finalization is admitted and then blocks writing its terminal
    /// tx behind a write the writer cannot finish. It is still in flight when
    /// the release drain expires, and the writer is still stuck when the writer
    /// budget expires — the exact sequence the reviewer said "60s covers it"
    /// could not account for. Nothing about the terminal tx is durable
    /// afterwards, so what must be durable is the record of that: it is on disk,
    /// naming the run, before the function returns.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn shutdown_bounds_a_stuck_terminal_tx_and_records_the_run_at_risk() {
        let tmp = tempfile::tempdir().unwrap();
        let home = Home::at(tmp.path().join("home"));
        home.ensure().unwrap();
        let writer = spawn_writer(EventBus::new());
        let release_tasks = api::ReleaseTaskTracker::new();

        // Block the writer the way a stalled fsync does: inside the task.
        let stalling = writer.clone();
        let stalled_path = tmp.path().join("stalled.org");
        tokio::spawn(async move {
            stalling
                .mutate_file(writer::FileMutate {
                    path: stalled_path,
                    transform: Box::new(|_| {
                        std::thread::sleep(std::time::Duration::from_secs(5));
                        Ok(b"never observed\n".to_vec())
                    }),
                })
                .await
        });
        while writer.in_flight_write().is_none() {
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }

        // A real admitted release whose terminal tx queues behind that write.
        let admission = release_tasks
            .try_admit("run-terminal-tx-stuck", Some("WorkerFinalize".to_string()))
            .expect("tracker is open");
        let releasing = writer.clone();
        let tx_path = home.tx().join("2026-07.org");
        release_tasks.spawn_release(admission, async move {
            let mut entry = orgasmic_core::tx::TxEntry::new(
                "tx-terminal-stuck",
                "WorkerFinalize",
                "[2026-07-28 Tue 10:00:00]",
                "tester@example.com",
                "test-machine",
            );
            entry.project = Some("orgasmic".into());
            releasing
                .append_tx(
                    TxAppend {
                        tx_path,
                        entry,
                        project_id: Some("orgasmic".into()),
                        tx_id_policy: TxIdPolicy::Preserve,
                        request_id: None,
                    },
                    None,
                )
                .await
                .expect("the stalled writer never answers this append");
            Ok(("run-terminal-tx-stuck".to_string(), None))
        });

        let budgets = ShutdownBudgets {
            connection_drain: std::time::Duration::from_millis(200),
            release_drain: std::time::Duration::from_millis(300),
            writer_shutdown: std::time::Duration::from_millis(300),
        };
        let started = std::time::Instant::now();
        let record_path = graceful_shutdown(
            &home,
            "boot-shutdown-loss-test",
            &release_tasks,
            &writer,
            budgets,
        )
        .await;
        let elapsed = started.elapsed();

        assert!(
            elapsed < std::time::Duration::from_secs(3),
            "shutdown must stay inside its budgets ({:?}); took {elapsed:?}",
            budgets.total()
        );
        let record_path = record_path.expect("a shutdown that lost work must write a record");
        assert_eq!(
            record_path,
            shutdown_loss_dir(&home).join("boot-shutdown-loss-test.json")
        );
        let record: ShutdownLossRecord =
            serde_json::from_slice(&std::fs::read(&record_path).unwrap()).unwrap();
        assert_eq!(
            record.outstanding_release_runs,
            vec!["run-terminal-tx-stuck".to_string()],
            "the record has to name the run whose terminal tx is unproven"
        );
        assert!(
            matches!(
                record.writer_shutdown,
                writer::WriterShutdownOutcome::TimedOut { .. }
            ),
            "writer shutdown outcome: {:?}",
            record.writer_shutdown
        );
        assert!(record.rescue.contains("orgasmic run recover"));
    }

    /// The other half of the same claim: a shutdown with nothing outstanding
    /// leaves no record, so the presence of one always means something is
    /// actually at risk.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn clean_shutdown_writes_no_loss_record() {
        let tmp = tempfile::tempdir().unwrap();
        let home = Home::at(tmp.path().join("home"));
        home.ensure().unwrap();
        let writer = spawn_writer(EventBus::new());
        let release_tasks = api::ReleaseTaskTracker::new();

        let record = graceful_shutdown(
            &home,
            "boot-clean",
            &release_tasks,
            &writer,
            ShutdownBudgets::default(),
        )
        .await;

        assert!(record.is_none(), "clean shutdown wrote {record:?}");
        assert!(!shutdown_loss_dir(&home).exists());
    }

    /// The derived-timeout chain has to start from a real number: every phase
    /// of the shutdown path must be bounded, or the service manager's kill
    /// timeout is derived from an unbounded sum (TASK-WGXKD.2 finding 1).
    #[test]
    fn shutdown_budget_is_the_sum_of_every_bounded_phase() {
        let budgets = ShutdownBudgets::default();

        assert_eq!(budgets.connection_drain, CONNECTION_DRAIN_TIMEOUT);
        assert_eq!(
            budgets.release_drain,
            api::RELEASE_FINALIZATION_DRAIN_TIMEOUT
        );
        assert_eq!(budgets.writer_shutdown, writer::WRITER_SHUTDOWN_TIMEOUT);
        assert_eq!(
            budgets.total(),
            CONNECTION_DRAIN_TIMEOUT
                + api::RELEASE_FINALIZATION_DRAIN_TIMEOUT
                + writer::WRITER_SHUTDOWN_TIMEOUT
        );
    }

    #[tokio::test]
    async fn second_start_preserves_healthy_lock_owner() {
        let tmp = tempfile::tempdir().unwrap();
        let home = Home::at(tmp.path().join("home"));
        std::fs::create_dir_all(&home.root).unwrap();
        let reservation = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = reservation.local_addr().unwrap().port();
        drop(reservation);
        std::fs::write(
            home.config(),
            format!("bind_host: 127.0.0.1\nbind_port: {port}\n"),
        )
        .unwrap();
        let options = DaemonOptions {
            fs_watcher_enabled: false,
            ..DaemonOptions::default()
        };
        let running = Daemon::run(home.clone(), options.clone())
            .await
            .expect("boot first daemon");

        let started = std::time::Instant::now();
        let error = match Daemon::run(home, options).await {
            Ok(_) => panic!("second daemon must not boot"),
            Err(error) => error,
        };
        let incumbent = error
            .downcast_ref::<DaemonAlreadyRunning>()
            .expect("healthy incumbent classification");

        assert_eq!(incumbent.addr, running.addr);
        assert_eq!(incumbent.boot_id, running.boot_id);
        assert!(
            started.elapsed() < std::time::Duration::from_secs(2),
            "second start did not fail closed quickly"
        );
        let _ = running.shutdown.send(());
        let _ = running.join.await;
    }

    #[tokio::test]
    async fn daemon_lock_retries_a_transient_probe_hold() {
        let tmp = tempfile::tempdir().unwrap();
        let home = Home::at(tmp.path().join("home"));
        std::fs::create_dir_all(&home.root).unwrap();
        let lock_path = daemon_lock_path(&home);
        let held_lock = OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .open(lock_path)
            .unwrap();
        fs2::FileExt::lock_exclusive(&held_lock).unwrap();

        let release = tokio::spawn(async move {
            tokio::time::sleep(std::time::Duration::from_millis(25)).await;
            fs2::FileExt::unlock(&held_lock).unwrap();
        });

        let acquired = acquire_daemon_lock(&home, &DaemonOptions::default())
            .await
            .expect("transient lock hold must not fail daemon acquisition")
            .expect("transient lock hold must not classify as an incumbent");
        release.await.unwrap();
        fs2::FileExt::unlock(&acquired).unwrap();
    }

    #[tokio::test]
    async fn daemon_lock_continuously_held_fails_closed() {
        let tmp = tempfile::tempdir().unwrap();
        let home = Home::at(tmp.path().join("home"));
        std::fs::create_dir_all(&home.root).unwrap();
        let lock_path = daemon_lock_path(&home);
        let held_lock = OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .open(lock_path)
            .unwrap();
        fs2::FileExt::lock_exclusive(&held_lock).unwrap();

        let started = std::time::Instant::now();
        let error = acquire_daemon_lock(&home, &DaemonOptions::default())
            .await
            .expect_err("continuously held lock must not be bypassed");

        assert!(
            started.elapsed() >= DAEMON_LOCK_RETRY_BUDGET,
            "lock acquisition must retry before failing closed"
        );
        assert!(
            error.downcast_ref::<DaemonInstanceLockHeld>().is_some(),
            "continuously held lock must retain incumbent handling: {error:#}"
        );
        fs2::FileExt::unlock(&held_lock).unwrap();
    }

    #[tokio::test]
    async fn unauth_request_is_rejected() {
        let tmp = tempfile::tempdir().unwrap();
        let home = Home::at(tmp.path().join("home"));
        let running = Daemon::run(
            home,
            DaemonOptions {
                bind_override: Some("127.0.0.1".parse().unwrap()),
                port_override: Some(0),
                ..Default::default()
            },
        )
        .await
        .unwrap();
        let resp = reqwest::get(format!("http://{}/api/daemon/status", running.addr))
            .await
            .unwrap();
        assert_eq!(resp.status(), reqwest::StatusCode::UNAUTHORIZED);
        let _ = running.shutdown.send(());
        let _ = running.join.await;
    }

    #[tokio::test]
    async fn root_spa_serves_deep_links_and_old_routes_are_hard_cut() {
        let tmp = tempfile::tempdir().unwrap();
        let home = Home::at(tmp.path().join("home"));
        let running = Daemon::run(
            home,
            DaemonOptions {
                bind_override: Some("127.0.0.1".parse().unwrap()),
                port_override: Some(0),
                ..Default::default()
            },
        )
        .await
        .unwrap();
        let token = std::fs::read_to_string(tmp.path().join("home/user/auth/token")).unwrap();
        let client = reqwest::Client::new();

        for path in ["/", "/projects/orgasmic/graph"] {
            let resp = client
                .get(format!("http://{}{}", running.addr, path))
                .header(reqwest::header::ACCEPT, "text/html")
                .send()
                .await
                .unwrap();
            assert_eq!(resp.status(), reqwest::StatusCode::OK, "{path}");
            assert_eq!(
                resp.headers()
                    .get(reqwest::header::CONTENT_TYPE)
                    .unwrap()
                    .to_str()
                    .unwrap(),
                "text/html; charset=utf-8"
            );
            let body = resp.text().await.unwrap();
            assert!(
                body.contains("<div id=\"root\"></div>") || body.contains("placeholder UI"),
                "{path}: {body}"
            );
        }

        let old_app = client
            .get(format!("http://{}/app/", running.addr))
            .header(reqwest::header::ACCEPT, "text/html")
            .send()
            .await
            .unwrap();
        assert_eq!(old_app.status(), reqwest::StatusCode::NOT_FOUND);

        let old_root_api = client
            .get(format!("http://{}/projects", running.addr))
            .bearer_auth(token.trim())
            .header(reqwest::header::ACCEPT, "application/json")
            .send()
            .await
            .unwrap();
        assert_eq!(old_root_api.status(), reqwest::StatusCode::NOT_FOUND);

        let _ = running.shutdown.send(());
        let _ = running.join.await;
    }

    #[tokio::test]
    async fn ui_session_cookie_authenticates_same_origin_api() {
        let tmp = tempfile::tempdir().unwrap();
        let home = Home::at(tmp.path().join("home"));
        let running = Daemon::run(
            home,
            DaemonOptions {
                bind_override: Some("127.0.0.1".parse().unwrap()),
                port_override: Some(0),
                ..Default::default()
            },
        )
        .await
        .unwrap();
        let token = std::fs::read_to_string(tmp.path().join("home/user/auth/token")).unwrap();
        let client = reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .unwrap();

        let app = client
            .get(format!("http://{}/", running.addr))
            .send()
            .await
            .unwrap();
        assert_eq!(app.status(), reqwest::StatusCode::OK);
        assert_eq!(
            app.headers()
                .get(reqwest::header::CONTENT_TYPE)
                .unwrap()
                .to_str()
                .unwrap(),
            "text/html; charset=utf-8"
        );

        let unauth_ticket = client
            .post(format!("http://{}/api/auth/ui-session", running.addr))
            .json(&serde_json::json!({}))
            .send()
            .await
            .unwrap();
        assert_eq!(unauth_ticket.status(), reqwest::StatusCode::UNAUTHORIZED);

        let ticket: serde_json::Value = client
            .post(format!("http://{}/api/auth/ui-session", running.addr))
            .bearer_auth(token.trim())
            .json(&serde_json::json!({}))
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        let path = ticket["path"].as_str().unwrap();
        let session = client
            .get(format!("http://{}{}", running.addr, path))
            .send()
            .await
            .unwrap();
        assert_eq!(session.status(), reqwest::StatusCode::SEE_OTHER);
        let cookie = session
            .headers()
            .get(reqwest::header::SET_COOKIE)
            .unwrap()
            .to_str()
            .unwrap()
            .split(';')
            .next()
            .unwrap()
            .to_string();

        let status = client
            .get(format!("http://{}/api/daemon/status", running.addr))
            .header(reqwest::header::COOKIE, cookie)
            .send()
            .await
            .unwrap();
        assert_eq!(status.status(), reqwest::StatusCode::OK);

        let _ = running.shutdown.send(());
        let _ = running.join.await;
    }

    #[tokio::test]
    async fn filesystem_browser_lists_and_validates_daemon_host_paths() {
        let tmp = tempfile::tempdir().unwrap();
        let home = Home::at(tmp.path().join("home"));
        let browse_root = tmp.path().join("browse");
        let project_root = browse_root.join("demo");
        std::fs::create_dir_all(project_root.join(".orgasmic")).unwrap();
        std::fs::write(
            project_root.join(".orgasmic/project.org"),
            "#+title: project\n#+orgasmic_version: 1\n\n* PROJECT demo\n:PROPERTIES:\n:ID:                  demo\n:END:\n",
        )
        .unwrap();
        let running = Daemon::run(
            home,
            DaemonOptions {
                bind_override: Some("127.0.0.1".parse().unwrap()),
                port_override: Some(0),
                ..Default::default()
            },
        )
        .await
        .unwrap();
        let token = std::fs::read_to_string(tmp.path().join("home/user/auth/token")).unwrap();
        let client = reqwest::Client::new();

        let entries: serde_json::Value = client
            .get(format!("http://{}/api/filesystem/entries", running.addr))
            .bearer_auth(token.trim())
            .query(&[("path", browse_root.to_string_lossy().to_string())])
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        let entries = entries.as_array().unwrap();
        assert!(entries.iter().any(|entry| {
            entry["display_name"] == "demo"
                && entry["kind"] == "directory"
                && entry["orgasmic_project"] == true
                && entry["project_id"] == "demo"
        }));

        let validated: serde_json::Value = client
            .post(format!(
                "http://{}/api/filesystem/validate-project",
                running.addr
            ))
            .bearer_auth(token.trim())
            .json(&serde_json::json!({ "path": project_root }))
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        assert_eq!(validated["orgasmic_project"], true);
        assert_eq!(validated["project_id"], "demo");

        let _ = running.shutdown.send(());
        let _ = running.join.await;
    }
}
