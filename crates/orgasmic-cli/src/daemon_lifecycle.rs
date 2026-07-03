// orgasmic:arch_C87Z9
//! CLI-owned local daemon lifecycle.
//!
//! `orgasmic serve` remains the foreground daemon primitive. This module owns
//! the CLI-only case: daemon-backed commands can ensure a local daemon exists
//! without asking the user to keep a shell, tmux pane, or terminal window alive.

use std::fs::OpenOptions;
use std::net::IpAddr;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use anyhow::{anyhow, bail, Context, Result};
use reqwest::StatusCode;
use serde::Deserialize;
use serde_yaml::Value as YamlValue;

use crate::daemon_runtime;
use crate::daemon_service::{self, ServiceStart};
use crate::home::Home;

// Generous enough to cover the daemon's startup bind-retry (up to ~8s while a
// draining predecessor releases the port during a runtime-swap restart).
const START_TIMEOUT: Duration = Duration::from_secs(20);
const STOP_TIMEOUT: Duration = Duration::from_secs(10);

#[derive(Debug, Clone, Deserialize)]
pub struct DaemonStatus {
    pub boot_id: String,
    pub pid: u32,
}

#[derive(Debug, Clone)]
pub struct DaemonStarting {
    pub pid: u32,
}

#[derive(Debug, Clone)]
pub enum LocalDaemonState {
    Running(DaemonStatus),
    Starting(DaemonStarting),
    Down,
    Unauthorized,
}

#[derive(Debug, Clone)]
pub enum DaemonStartOutcome {
    Running(DaemonStatus),
    StillBooting(DaemonStarting),
}

#[derive(Debug, Clone)]
pub enum AuthRepairOutcome {
    NotNeeded,
    Repaired(DaemonStartOutcome),
}

pub fn ensure_running(home: &Home) -> Result<()> {
    if explicit_daemon_url().is_some() {
        return Ok(());
    }
    ensure_running_local(home, true)
}

fn ensure_running_local(home: &Home, repair_unauthorized: bool) -> Result<()> {
    match probe_local(home)? {
        LocalDaemonState::Running(_) => Ok(()),
        LocalDaemonState::Starting(_) => wait_until_running(home).map(|_| ()),
        LocalDaemonState::Unauthorized if repair_unauthorized => {
            match repair_unauthorized_local_daemon(home)? {
                AuthRepairOutcome::Repaired(_) => Ok(()),
                AuthRepairOutcome::NotNeeded => ensure_running_local(home, false),
            }
        }
        LocalDaemonState::Unauthorized => bail_auth_token_mismatch(),
        LocalDaemonState::Down => {
            let spawned_pid = start_via_selected_adapter(home)?;
            wait_until_running_after_start(home, spawned_pid, START_TIMEOUT).map(|_| ())
        }
    }
}

pub async fn ensure_running_async(home: &Home) -> Result<()> {
    if explicit_daemon_url().is_some() {
        return Ok(());
    }
    ensure_running_local_async(home, true).await
}

async fn ensure_running_local_async(home: &Home, repair_unauthorized: bool) -> Result<()> {
    match probe_local_async(home).await? {
        LocalDaemonState::Running(_) => Ok(()),
        LocalDaemonState::Starting(_) => wait_until_running_async(home).await.map(|_| ()),
        LocalDaemonState::Unauthorized if repair_unauthorized => {
            match repair_unauthorized_local_daemon_async(home).await? {
                AuthRepairOutcome::Repaired(_) => Ok(()),
                AuthRepairOutcome::NotNeeded => {
                    ensure_running_local_async_without_repair(home).await
                }
            }
        }
        LocalDaemonState::Unauthorized => bail_auth_token_mismatch(),
        LocalDaemonState::Down => {
            start_via_selected_adapter(home)?;
            wait_until_running_after_start_async(home, START_TIMEOUT)
                .await
                .map(|_| ())
        }
    }
}

async fn ensure_running_local_async_without_repair(home: &Home) -> Result<()> {
    match probe_local_async(home).await? {
        LocalDaemonState::Running(_) => Ok(()),
        LocalDaemonState::Starting(_) => wait_until_running_async(home).await.map(|_| ()),
        LocalDaemonState::Unauthorized => bail_auth_token_mismatch(),
        LocalDaemonState::Down => {
            start_via_selected_adapter(home)?;
            wait_until_running_after_start_async(home, START_TIMEOUT)
                .await
                .map(|_| ())
        }
    }
}

pub fn status(home: &Home) -> Result<LocalDaemonState> {
    probe_local(home)
}

pub fn local_lifecycle_externally_owned() -> bool {
    explicit_daemon_url().is_some()
}

pub fn start(home: &Home) -> Result<DaemonStartOutcome> {
    refuse_explicit_daemon_url()?;
    start_local(home, true)
}

fn start_local(home: &Home, repair_unauthorized: bool) -> Result<DaemonStartOutcome> {
    match probe_local(home)? {
        LocalDaemonState::Running(status) => Ok(DaemonStartOutcome::Running(status)),
        LocalDaemonState::Starting(starting) => Ok(DaemonStartOutcome::StillBooting(starting)),
        LocalDaemonState::Unauthorized if repair_unauthorized => {
            match repair_unauthorized_local_daemon(home)? {
                AuthRepairOutcome::Repaired(outcome) => Ok(outcome),
                AuthRepairOutcome::NotNeeded => start_local(home, false),
            }
        }
        LocalDaemonState::Unauthorized => bail_auth_token_mismatch(),
        LocalDaemonState::Down => {
            let spawned_pid = start_via_selected_adapter(home)?;
            wait_until_running_after_start(home, spawned_pid, START_TIMEOUT)
        }
    }
}

pub fn stop_with_force(home: &Home, force: bool) -> Result<Option<DaemonStatus>> {
    stop_inner(home, force, true)
}

pub fn restart_with_force(home: &Home, force: bool) -> Result<DaemonStartOutcome> {
    let _ = stop_inner(home, force, false)?;
    start_local(home, false)
}

pub fn repair_unauthorized_local_daemon(home: &Home) -> Result<AuthRepairOutcome> {
    refuse_explicit_daemon_url()?;
    match probe_local(home)? {
        LocalDaemonState::Unauthorized => {}
        LocalDaemonState::Running(_) | LocalDaemonState::Starting(_) | LocalDaemonState::Down => {
            return Ok(AuthRepairOutcome::NotNeeded);
        }
    }
    let _ = stop_inner(home, true, false)?;
    let outcome = start_local(home, false)?;
    Ok(AuthRepairOutcome::Repaired(outcome))
}

async fn repair_unauthorized_local_daemon_async(home: &Home) -> Result<AuthRepairOutcome> {
    let home = home.clone();
    tokio::task::spawn_blocking(move || repair_unauthorized_local_daemon(&home))
        .await
        .context("join daemon auth repair task")?
}

fn stop_inner(
    home: &Home,
    force: bool,
    protect_live_manager: bool,
) -> Result<Option<DaemonStatus>> {
    refuse_explicit_daemon_url()?;
    match probe_local(home)? {
        LocalDaemonState::Running(status) => {
            if protect_live_manager && !force {
                refuse_if_live_manager(home)?;
            }
            let _ = request_restart_drain(home);
            daemon_service::stop(home)?;
            if process_alive(status.pid) {
                stop_pid(status.pid)?;
            }
            wait_until_down(home)?;
            remove_pid_file(home);
            Ok(Some(status))
        }
        LocalDaemonState::Starting(starting) => {
            daemon_service::stop(home)?;
            if process_alive(starting.pid) {
                stop_pid(starting.pid)?;
            }
            wait_until_down(home)?;
            remove_pid_file(home);
            Ok(None)
        }
        LocalDaemonState::Down => {
            daemon_service::stop(home)?;
            remove_pid_file(home);
            Ok(None)
        }
        LocalDaemonState::Unauthorized => {
            daemon_service::stop(home)?;
            if let Some(pid) = read_pid_file(home).filter(|pid| process_alive(*pid)) {
                let _ = stop_pid(pid);
            }
            remove_pid_file(home);
            Ok(None)
        }
    }
}

/// Refuse a plain stop while a live interactive manager run exists. Controlled
/// restarts are recovery-aware and may proceed without force; stale leases are
/// cleared via `orgasmic manager lease-release`, never by stopping the daemon.
pub(crate) fn refuse_if_live_manager(home: &Home) -> Result<()> {
    let manager_runs = match live_manager_runs(home) {
        Ok(runs) => runs,
        // Probe failures (daemon mid-shutdown, auth churn) must not wedge the
        // lifecycle commands; the guard is best-effort.
        Err(_) => return Ok(()),
    };
    if manager_runs.is_empty() {
        return Ok(());
    }
    bail!(
        "refusing to stop the daemon: live manager run(s) {} need restart recovery.\n\
         If you are clearing a stale lease, use `orgasmic manager lease-release` instead.\n\
         If you really mean to stop without an immediate restart, pass --force.",
        manager_runs.join(", ")
    )
}

/// Live `manager.launch:` run ids reported by the local daemon.
fn live_manager_runs(home: &Home) -> Result<Vec<String>> {
    let Some(base_url) = local_base_url(home)? else {
        return Ok(Vec::new());
    };
    let Some(token) = read_token(home) else {
        return Ok(Vec::new());
    };
    let runtime = tokio::runtime::Runtime::new().context("create tokio runtime")?;
    runtime.block_on(async {
        let client = reqwest::Client::builder()
            .timeout(Duration::from_secs(5))
            .build()
            .context("build daemon runs client")?;
        let body: serde_json::Value = client
            .get(daemon_url(&base_url, "/runs"))
            .bearer_auth(token)
            .send()
            .await
            .context("query daemon runs")?
            .error_for_status()
            .context("daemon runs status")?
            .json()
            .await
            .context("parse daemon runs")?;
        let runs = body
            .get("live")
            .and_then(|live| live.as_array())
            .map(|live| {
                live.iter()
                    .filter(|run| {
                        run.get("task_id")
                            .and_then(|t| t.as_str())
                            .is_some_and(|t| t.starts_with("manager.launch:"))
                    })
                    .filter_map(|run| run.get("run_id").and_then(|r| r.as_str()))
                    .map(str::to_string)
                    .collect()
            })
            .unwrap_or_default();
        Ok(runs)
    })
}

pub fn persistence_status(home: &Home) -> daemon_service::PersistenceStatus {
    if explicit_daemon_url().is_some() {
        daemon_service::PersistenceStatus {
            adapter: "external-url",
            installed: false,
            enabled: false,
            detail: Some(
                "ORGASMIC_DAEMON_URL is set; local daemon lifecycle is externally owned"
                    .to_string(),
            ),
        }
    } else {
        daemon_service::persistence_status(home)
    }
}

fn refuse_explicit_daemon_url() -> Result<()> {
    if explicit_daemon_url().is_some() {
        bail!("ORGASMIC_DAEMON_URL is set; local daemon lifecycle is externally owned");
    }
    Ok(())
}

fn start_via_selected_adapter(home: &Home) -> Result<Option<u32>> {
    match daemon_service::start(home)? {
        ServiceStart::Persistent => Ok(None),
        ServiceStart::DetachedFallback => spawn_detached(home).map(Some),
    }
}

fn spawn_detached(home: &Home) -> Result<u32> {
    home.ensure()?;
    std::fs::create_dir_all(home.logs())
        .with_context(|| format!("create {}", home.logs().display()))?;
    let runtime_override = daemon_runtime::active(home)?;
    let exe = match &runtime_override {
        Some(runtime) => runtime.binary.clone(),
        None => std::env::current_exe().context("resolve current executable")?,
    };
    let stdout = OpenOptions::new()
        .create(true)
        .append(true)
        .open(home.logs().join("daemon.out.log"))
        .context("open daemon stdout log")?;
    let stderr = OpenOptions::new()
        .create(true)
        .append(true)
        .open(home.logs().join("daemon.err.log"))
        .context("open daemon stderr log")?;
    let cwd = runtime_override
        .map(|runtime| runtime.source_checkout)
        .unwrap_or_else(|| {
            if home.source().is_dir() {
                home.source()
            } else {
                home.root.clone()
            }
        });
    let mut command = Command::new(exe);
    command
        .arg("serve")
        .env("ORGASMIC_HOME", &home.root)
        .current_dir(cwd)
        .stdin(Stdio::null())
        .stdout(Stdio::from(stdout))
        .stderr(Stdio::from(stderr));
    platform_detach(&mut command);
    let child = command.spawn().context("spawn daemon")?;
    let pid = child.id();
    write_pid_file(home, pid)?;
    Ok(pid)
}

#[cfg(unix)]
fn platform_detach(command: &mut Command) {
    use std::os::unix::process::CommandExt;
    unsafe {
        command.pre_exec(|| {
            if libc::setsid() == -1 {
                Err(std::io::Error::last_os_error())
            } else {
                Ok(())
            }
        });
    }
}

#[cfg(windows)]
fn platform_detach(command: &mut Command) {
    use std::os::windows::process::CommandExt;
    const DETACHED_PROCESS: u32 = 0x0000_0008;
    const CREATE_NEW_PROCESS_GROUP: u32 = 0x0000_0200;
    const CREATE_NO_WINDOW: u32 = 0x0800_0000;
    command.creation_flags(DETACHED_PROCESS | CREATE_NEW_PROCESS_GROUP | CREATE_NO_WINDOW);
}

#[cfg(not(any(unix, windows)))]
fn platform_detach(_command: &mut Command) {}

fn stop_pid(pid: u32) -> Result<()> {
    #[cfg(unix)]
    {
        let rc = unsafe { libc::kill(pid as libc::pid_t, libc::SIGINT) };
        if rc == -1 {
            return Err(std::io::Error::last_os_error())
                .with_context(|| format!("signal daemon pid {pid}"));
        }
        Ok(())
    }
    #[cfg(windows)]
    {
        let status = Command::new("taskkill")
            .args(["/PID", &pid.to_string(), "/T"])
            .status()
            .with_context(|| format!("taskkill daemon pid {pid}"))?;
        if !status.success() {
            bail!("taskkill daemon pid {pid} failed with {status}");
        }
        Ok(())
    }
    #[cfg(not(any(unix, windows)))]
    {
        let _ = pid;
        bail!("daemon stop is not implemented on this platform")
    }
}

fn wait_until_running(home: &Home) -> Result<DaemonStatus> {
    loop {
        match probe_local(home)? {
            LocalDaemonState::Running(status) => return Ok(status),
            LocalDaemonState::Starting(_) => std::thread::sleep(Duration::from_millis(200)),
            LocalDaemonState::Unauthorized => return bail_auth_token_mismatch(),
            LocalDaemonState::Down => return bail_start_failed(home, START_TIMEOUT),
        }
    }
}

#[cfg(test)]
fn wait_until_running_for_start_with_timeout(
    home: &Home,
    spawned_pid: u32,
    timeout: Duration,
) -> Result<DaemonStartOutcome> {
    wait_until_running_after_start(home, Some(spawned_pid), timeout)
}

fn wait_until_running_after_start(
    home: &Home,
    spawned_pid: Option<u32>,
    timeout: Duration,
) -> Result<DaemonStartOutcome> {
    let started = Instant::now();
    loop {
        match probe_local(home)? {
            LocalDaemonState::Running(status) => return Ok(DaemonStartOutcome::Running(status)),
            LocalDaemonState::Starting(starting) => {
                if started.elapsed() >= timeout {
                    return Ok(DaemonStartOutcome::StillBooting(starting));
                }
                std::thread::sleep(Duration::from_millis(200));
            }
            LocalDaemonState::Unauthorized => return bail_auth_token_mismatch(),
            LocalDaemonState::Down => {
                if let Some(pid) = spawned_pid {
                    if !process_alive(pid) {
                        return bail_start_failed(home, started.elapsed());
                    }
                    if started.elapsed() >= timeout {
                        return Ok(DaemonStartOutcome::StillBooting(DaemonStarting { pid }));
                    }
                } else if started.elapsed() >= timeout {
                    return bail_start_failed(home, started.elapsed());
                }
                std::thread::sleep(Duration::from_millis(200));
            }
        }
    }
}

async fn wait_until_running_async(home: &Home) -> Result<DaemonStatus> {
    loop {
        match probe_local_async(home).await? {
            LocalDaemonState::Running(status) => return Ok(status),
            LocalDaemonState::Starting(_) => tokio::time::sleep(Duration::from_millis(200)).await,
            LocalDaemonState::Unauthorized => return bail_auth_token_mismatch(),
            LocalDaemonState::Down => return bail_start_failed(home, START_TIMEOUT),
        }
    }
}

async fn wait_until_running_after_start_async(
    home: &Home,
    timeout: Duration,
) -> Result<DaemonStatus> {
    let started = Instant::now();
    loop {
        match probe_local_async(home).await? {
            LocalDaemonState::Running(status) => return Ok(status),
            LocalDaemonState::Starting(_) | LocalDaemonState::Down => {
                if started.elapsed() >= timeout {
                    return bail_start_failed(home, started.elapsed());
                }
                tokio::time::sleep(Duration::from_millis(200)).await;
            }
            LocalDaemonState::Unauthorized => return bail_auth_token_mismatch(),
        }
    }
}

fn bail_auth_token_mismatch<T>() -> Result<T> {
    bail!("daemon auth token mismatch (check $ORGASMIC_HOME/user/auth/token; run `orgasmic doctor --fix` to repair a stale local daemon)")
}

fn bail_start_failed<T>(home: &Home, elapsed: Duration) -> Result<T> {
    bail!(
        "daemon process exited before becoming ready after {}s; check {} and {}",
        elapsed.as_secs(),
        home.logs().join("daemon.out.log").display(),
        home.logs().join("daemon.err.log").display()
    )
}

fn wait_until_down(home: &Home) -> Result<()> {
    let started = Instant::now();
    loop {
        match probe_local(home)? {
            LocalDaemonState::Down => return Ok(()),
            LocalDaemonState::Unauthorized => return Ok(()),
            LocalDaemonState::Starting(starting) => {
                if started.elapsed() >= STOP_TIMEOUT {
                    bail!(
                        "daemon pid {} did not stop after {}s",
                        starting.pid,
                        STOP_TIMEOUT.as_secs()
                    );
                }
                std::thread::sleep(Duration::from_millis(200));
            }
            LocalDaemonState::Running(status) => {
                if started.elapsed() >= STOP_TIMEOUT {
                    bail!(
                        "daemon pid {} did not stop after {}s",
                        status.pid,
                        STOP_TIMEOUT.as_secs()
                    );
                }
                std::thread::sleep(Duration::from_millis(200));
            }
        }
    }
}

fn probe_local(home: &Home) -> Result<LocalDaemonState> {
    let runtime = tokio::runtime::Runtime::new().context("create tokio runtime")?;
    runtime.block_on(async { probe_local_async(home).await })
}

async fn probe_local_async(home: &Home) -> Result<LocalDaemonState> {
    let Some(base_url) = local_base_url(home)? else {
        return Ok(starting_from_pid_file(home).unwrap_or(LocalDaemonState::Down));
    };
    let Some(token) = read_token(home) else {
        return Ok(starting_from_pid_file(home).unwrap_or(LocalDaemonState::Down));
    };
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(2))
        .build()
        .context("build daemon probe client")?;
    let response = match client
        .get(daemon_url(&base_url, "/daemon/status"))
        .bearer_auth(token)
        .send()
        .await
    {
        Ok(response) => response,
        Err(error) if error.is_connect() || error.is_timeout() => {
            return Ok(starting_from_pid_file(home).unwrap_or(LocalDaemonState::Down));
        }
        Err(error) => return Err(anyhow!("probe daemon: {error}")),
    };
    if response.status() == StatusCode::UNAUTHORIZED {
        return Ok(LocalDaemonState::Unauthorized);
    }
    if !response.status().is_success() {
        return Ok(starting_from_pid_file(home).unwrap_or(LocalDaemonState::Down));
    }
    let status = response
        .json::<DaemonStatus>()
        .await
        .context("decode daemon status")?;
    Ok(LocalDaemonState::Running(status))
}

fn request_restart_drain(home: &Home) -> Result<()> {
    let Some(base_url) = local_base_url(home)? else {
        return Ok(());
    };
    let Some(token) = read_token(home) else {
        return Ok(());
    };
    let runtime = tokio::runtime::Runtime::new().context("create tokio runtime")?;
    runtime.block_on(async {
        let client = reqwest::Client::builder()
            .timeout(Duration::from_secs(5))
            .build()
            .context("build daemon restart client")?;
        let _ = client
            .post(daemon_url(&base_url, "/daemon/restart"))
            .bearer_auth(token)
            .json(&serde_json::json!({
                "reason": "cli lifecycle restart",
            }))
            .send()
            .await;
        Ok::<(), anyhow::Error>(())
    })
}

fn explicit_daemon_url() -> Option<String> {
    std::env::var("ORGASMIC_DAEMON_URL")
        .ok()
        .map(|url| url.trim_end_matches('/').to_string())
        .filter(|url| !url.is_empty())
}

fn local_base_url(home: &Home) -> Result<Option<String>> {
    if explicit_daemon_url().is_some() {
        return Ok(None);
    }
    let (bind, port) = read_bind_port(&home.config())?;
    let host = if bind.is_unspecified() {
        "127.0.0.1".to_string()
    } else {
        bind.to_string()
    };
    Ok(Some(format!("http://{host}:{port}")))
}

fn read_bind_port(config: &Path) -> Result<(IpAddr, u16)> {
    let mut bind: IpAddr = "127.0.0.1".parse().unwrap();
    let mut port: u16 = 4848;
    if config.exists() {
        let raw = std::fs::read_to_string(config)
            .with_context(|| format!("read {}", config.display()))?;
        let value: YamlValue =
            serde_yaml::from_str(&raw).with_context(|| format!("parse {}", config.display()))?;
        if let Some(b) = value
            .get("bind_host")
            .or_else(|| value.get("bind"))
            .and_then(YamlValue::as_str)
        {
            if let Ok(addr) = b.parse() {
                bind = addr;
            }
        }
        if let Some(p) = value
            .get("bind_port")
            .or_else(|| value.get("port"))
            .and_then(YamlValue::as_u64)
        {
            if let Ok(p) = u16::try_from(p) {
                port = p;
            }
        }
    }
    Ok((bind, port))
}

fn daemon_url(base: &str, path: &str) -> String {
    let path = api_path(path);
    if path.starts_with('/') {
        format!("{base}{path}")
    } else {
        format!("{base}/{path}")
    }
}

fn api_path(path: &str) -> String {
    let normalized = if path.starts_with('/') {
        path.to_string()
    } else {
        format!("/{path}")
    };
    if normalized == "/api" || normalized.starts_with("/api/") {
        normalized
    } else {
        format!("/api{normalized}")
    }
}

fn read_token(home: &Home) -> Option<String> {
    let raw = std::fs::read_to_string(home.auth_token()).ok()?;
    let token = raw.trim().to_string();
    if token.is_empty() {
        None
    } else {
        Some(token)
    }
}

fn pid_file(home: &Home) -> PathBuf {
    home.state().join("daemon.pid")
}

fn write_pid_file(home: &Home, pid: u32) -> Result<()> {
    std::fs::create_dir_all(home.state())
        .with_context(|| format!("create {}", home.state().display()))?;
    std::fs::write(pid_file(home), format!("{pid}\n")).context("write daemon pid file")
}

fn read_pid_file(home: &Home) -> Option<u32> {
    let raw = std::fs::read_to_string(pid_file(home)).ok()?;
    raw.trim().parse::<u32>().ok()
}

fn starting_from_pid_file(home: &Home) -> Option<LocalDaemonState> {
    let pid = read_pid_file(home)?;
    if process_alive(pid) {
        Some(LocalDaemonState::Starting(DaemonStarting { pid }))
    } else {
        remove_pid_file(home);
        None
    }
}

fn process_alive(pid: u32) -> bool {
    #[cfg(unix)]
    {
        let rc = unsafe { libc::kill(pid as libc::pid_t, 0) };
        rc == 0 || std::io::Error::last_os_error().raw_os_error() == Some(libc::EPERM)
    }
    #[cfg(windows)]
    {
        let output = Command::new("tasklist")
            .args(["/FI", &format!("PID eq {pid}"), "/FO", "CSV", "/NH"])
            .output();
        output
            .ok()
            .and_then(|out| String::from_utf8(out.stdout).ok())
            .map(|stdout| stdout.contains(&format!("\"{pid}\"")))
            .unwrap_or(false)
    }
    #[cfg(not(any(unix, windows)))]
    {
        let _ = pid;
        false
    }
}

fn remove_pid_file(home: &Home) {
    let _ = std::fs::remove_file(pid_file(home));
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Mutex, MutexGuard, OnceLock};

    fn env_guard() -> MutexGuard<'static, ()> {
        static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
        LOCK.get_or_init(|| Mutex::new(())).lock().unwrap()
    }

    struct ScopedEnv {
        key: &'static str,
        prior: Option<String>,
    }

    impl ScopedEnv {
        fn set(key: &'static str, value: &str) -> Self {
            let prior = std::env::var(key).ok();
            std::env::set_var(key, value);
            Self { key, prior }
        }
    }

    impl Drop for ScopedEnv {
        fn drop(&mut self) {
            match &self.prior {
                Some(value) => std::env::set_var(self.key, value),
                None => std::env::remove_var(self.key),
            }
        }
    }

    #[cfg(unix)]
    struct ChildGuard(std::process::Child);

    #[cfg(unix)]
    impl Drop for ChildGuard {
        fn drop(&mut self) {
            let _ = self.0.kill();
            let _ = self.0.wait();
        }
    }

    #[cfg(unix)]
    fn sleeping_child() -> ChildGuard {
        ChildGuard(
            Command::new("/bin/sh")
                .args(["-c", "sleep 30"])
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .spawn()
                .expect("spawn sleeping child"),
        )
    }

    #[test]
    #[cfg(unix)]
    fn status_reports_starting_when_pid_alive_but_port_unbound() {
        let tmp = tempfile::tempdir().unwrap();
        let home = Home::at(tmp.path().join("home"));
        home.ensure().unwrap();
        std::fs::write(home.config(), "bind_host: 127.0.0.1\nbind_port: 65533\n").unwrap();
        let child = sleeping_child();
        write_pid_file(&home, child.0.id()).unwrap();

        match status(&home).unwrap() {
            LocalDaemonState::Starting(starting) => assert_eq!(starting.pid, child.0.id()),
            other => panic!("expected starting state, got {other:?}"),
        }
    }

    #[test]
    #[cfg(unix)]
    fn start_timeout_returns_still_booting_for_alive_unbound_pid() {
        let tmp = tempfile::tempdir().unwrap();
        let home = Home::at(tmp.path().join("home"));
        home.ensure().unwrap();
        std::fs::write(home.config(), "bind_host: 127.0.0.1\nbind_port: 65533\n").unwrap();
        let child = sleeping_child();
        write_pid_file(&home, child.0.id()).unwrap();

        let outcome = wait_until_running_for_start_with_timeout(
            &home,
            child.0.id(),
            Duration::from_millis(0),
        )
        .unwrap();
        match outcome {
            DaemonStartOutcome::StillBooting(starting) => assert_eq!(starting.pid, child.0.id()),
            other => panic!("expected still-booting start outcome, got {other:?}"),
        }
    }

    #[test]
    #[cfg(unix)]
    fn start_timeout_fails_when_spawned_pid_exited() {
        let tmp = tempfile::tempdir().unwrap();
        let home = Home::at(tmp.path().join("home"));
        home.ensure().unwrap();
        std::fs::write(home.config(), "bind_host: 127.0.0.1\nbind_port: 65533\n").unwrap();
        let mut child = Command::new("/bin/sh")
            .args(["-c", "exit 0"])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("spawn exiting child");
        let pid = child.id();
        let _ = child.wait();

        let err = wait_until_running_for_start_with_timeout(&home, pid, Duration::from_millis(0))
            .expect_err("exited daemon should fail");
        assert!(
            err.to_string()
                .contains("daemon process exited before becoming ready"),
            "unexpected error: {err:?}"
        );
    }

    #[test]
    fn persistence_status_names_selected_adapter() {
        let tmp = tempfile::tempdir().unwrap();
        let home = Home::at(tmp.path().join("home"));
        let status = persistence_status(&home);
        assert!(!status.adapter.is_empty());
    }

    #[test]
    fn repair_refuses_external_daemon_url() {
        let _guard = env_guard();
        let _env = ScopedEnv::set("ORGASMIC_DAEMON_URL", "http://127.0.0.1:9999");
        let tmp = tempfile::tempdir().unwrap();
        let home = Home::at(tmp.path().join("home"));

        let err = repair_unauthorized_local_daemon(&home).expect_err("external URL is not local");

        assert!(
            err.to_string()
                .contains("local daemon lifecycle is externally owned"),
            "unexpected error: {err:?}"
        );
    }

    #[test]
    fn daemon_url_joins_paths() {
        assert_eq!(
            daemon_url("http://127.0.0.1:4848", "/daemon/status"),
            "http://127.0.0.1:4848/api/daemon/status"
        );
        assert_eq!(
            daemon_url("http://127.0.0.1:4848", "daemon/status"),
            "http://127.0.0.1:4848/api/daemon/status"
        );
        assert_eq!(
            daemon_url("http://127.0.0.1:4848", "/api/daemon/status"),
            "http://127.0.0.1:4848/api/daemon/status"
        );
    }
}
