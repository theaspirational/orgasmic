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

pub mod api;
pub mod artifacts;
pub mod auth;
pub mod authz;
pub mod config;
pub mod content;
pub mod events;
pub mod governance;
pub mod index;
pub mod manager_registration;
pub mod prompt_compiler;
pub mod recovery_claim;
pub mod runtime;
pub mod supervisor;
pub mod watcher;
pub mod writer;
pub mod ws;

use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;
use std::{error, fmt};

use anyhow::{Context, Result};
use axum::Router;
use orgasmic_core::Home;
use orgasmic_drivers::modes::tmux;
use tokio::net::TcpListener;
use tracing::{info, warn};
use tracing_subscriber::EnvFilter;

pub use crate::api::embedded_ui_asset_hash;
pub use crate::api::{router, ApiState};
pub use crate::artifacts::{ArtifactSummary, BLOCK_TYPES};
pub use crate::auth::AuthState;
pub use crate::config::DaemonConfig;
pub use crate::content::{SkillView, WorkerView};
pub use crate::events::{Event, EventBus, EventPayload, Topic};
pub use crate::index::{
    ActivityEntry, ActivityKind, BoardEntry, Index, IndexSnapshot, ParseError, ParseErrorKind,
    ProjectIndex, TaskId, TaskOwner, TaskSummary, TxRecord,
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

pub fn init_tracing(default_filter: &str) {
    let filter = EnvFilter::try_from_default_env()
        .or_else(|_| EnvFilter::try_new(default_filter))
        .unwrap_or_else(|_| EnvFilter::new("info"));
    let _ = tracing_subscriber::fmt().with_env_filter(filter).try_init();
}

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
}

impl Default for DaemonOptions {
    fn default() -> Self {
        let actor = std::env::var("USER").unwrap_or_else(|_| "unknown".into());
        let machine = hostname().unwrap_or_else(|| "unknown".into());
        Self {
            bind_override: None,
            port_override: None,
            actor,
            machine,
            dispatch_watcher_grace: std::time::Duration::from_secs(30),
            fs_watcher_enabled: true,
            tmux_input_ready_timeout_secs: None,
            dispatch_response_delay: None,
        }
    }
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

fn hostname() -> Option<String> {
    std::process::Command::new("hostname")
        .output()
        .ok()
        .and_then(|o| String::from_utf8(o.stdout).ok())
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
}

pub struct Daemon;

impl Daemon {
    pub async fn run(home: Home, opts: DaemonOptions) -> Result<RunningDaemon> {
        home.ensure().context("ensure orgasmic home")?;
        let mut cfg = DaemonConfig::load(&home)?;
        if let Some(bind) = opts.bind_override {
            cfg = cfg.with_bind(bind);
        }
        if let Some(port) = opts.port_override {
            cfg = cfg.with_port(port);
        }
        init_tracing(&cfg.log_level);
        for key in &cfg.unrecognized_keys {
            warn!(
                key = %key,
                path = %home.config().display(),
                "unrecognized key in orgasmic config.yaml (ignored)"
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

        let token = auth::load_or_generate(&home)?;
        let auth_state = AuthState::new(token);

        let prebind_addr = SocketAddr::new(cfg.bind, cfg.port);
        info!(
            address = %prebind_addr,
            boot_id = %boot.boot_id,
            home = %home.root.display(),
            "orgasmic daemon starting pre-bind boot work"
        );

        let index = Index::new(home.clone());
        // AC #1: rebuild before serving normal reads.
        index.rebuild().await;
        let initial_snapshot = index.snapshot().await;
        if let Some(error) = initial_snapshot.first_historical_tx_parse_error().cloned() {
            return Err(HistoricalTxStartupError::from_parse_error(error).into());
        }

        // One-shot relocation of legacy home-level session transcripts into each
        // project's `.orgasmic/tmp/sessions/` (per-project tmp). No-op once the
        // legacy dir is drained.
        let migrate_projects: Vec<(String, PathBuf)> = initial_snapshot
            .board
            .iter()
            .map(|entry| (entry.id.clone(), entry.path.clone()))
            .collect();
        api::migrate_legacy_home_sessions(&home, &migrate_projects);

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
            actor: opts.actor.clone(),
            machine: opts.machine.clone(),
            bind_host: cfg.bind.to_string(),
            bind_port: cfg.port,
            ui_asset_hash: api::embedded_ui_asset_hash(),
            shutdown: shutdown_signal_rx,
            dispatch_watcher_grace: opts.dispatch_watcher_grace,
            tmux_input_ready_timeout_secs: opts.tmux_input_ready_timeout_secs,
            dispatch_response_delay: opts.dispatch_response_delay,
            artifact_write_locks: Arc::new(std::sync::Mutex::new(std::collections::HashMap::new())),
            recovery_claim_locks: Arc::new(std::sync::Mutex::new(std::collections::HashMap::new())),
        };

        // Boot auto-reattach: rehydrate still-live runs (notably the operator's
        // manager terminal) against their surviving mux sessions so a daemon
        // restart/rebuild is transparent. Runs whose mux session is gone are
        // skipped, not interrupted. Reattached dispatch runs get their
        // completion watcher respawned (TASK-567JG).
        api::reattach_live_runs_on_boot(&api_state, &reattach_roots).await;

        let app: Router = router(api_state);
        let addr = SocketAddr::new(cfg.bind, cfg.port);
        if let Some(delay) = bind_delay_for_tests() {
            tokio::time::sleep(delay).await;
        }
        let listener = bind_listener_with_retry(addr)
            .await
            .with_context(|| format!("bind {addr}"))?;
        let local_addr = listener.local_addr().context("local_addr")?;
        info!(
            address = %local_addr,
            boot_id = %boot.boot_id,
            home = %home.root.display(),
            "orgasmic daemon listening"
        );
        let (shutdown_tx, shutdown_rx) = tokio::sync::oneshot::channel::<()>();
        let writer_for_shutdown = writer.clone();
        let join = tokio::spawn(async move {
            let serve = axum::serve(listener, app).with_graceful_shutdown(async move {
                let _ = shutdown_rx.await;
                // Wake long-lived connection tasks before draining connections.
                let _ = shutdown_signal_tx.send(true);
            });
            if let Err(err) = serve.await {
                tracing::error!(error = %err, "orgasmic daemon exited with error");
            }
            writer_for_shutdown.shutdown().await;
        });

        Ok(RunningDaemon {
            addr: local_addr,
            boot_id: boot.boot_id.clone(),
            shutdown: shutdown_tx,
            join,
            _watcher: watcher,
        })
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
