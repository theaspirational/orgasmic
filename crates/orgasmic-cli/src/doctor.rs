// orgasmic:arch_WZFAX,dec_2D5BC
//! Diagnose the local orgasmic install — missing home dirs, missing shipped
//! files, broken binary symlink.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, SystemTime};

use anyhow::Context;
use chrono::{DateTime, SecondsFormat, Utc};
use orgasmic_core::paths::project_sessions_dir;
use orgasmic_core::{retired, Lifecycle, SessionEventKind, SessionScanBudget};
use orgasmic_drivers::TranscriptRoots;
use reqwest::StatusCode;
use serde::Deserialize;

use crate::content_lifecycle::{self, RegistryFinding};
use crate::daemon_client;
use crate::daemon_lifecycle::LedgerSyncStatus;
use crate::daemon_service;
use crate::home::Home;
use crate::install_state::{self, InstallMode};
use crate::path_env;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Finding {
    Ok(String),
    Warn(String),
    Fail(String),
}

impl Finding {
    #[allow(dead_code)]
    pub fn is_fail(&self) -> bool {
        matches!(self, Finding::Fail(_))
    }
}

pub fn diagnose(home: &Home) -> Vec<Finding> {
    let mut out = Vec::new();
    push_dir_check(&mut out, &home.root, "ORGASMIC_HOME");
    for d in home.required_dirs() {
        push_dir_check(&mut out, &d, &d.display().to_string());
    }
    push_file_check(&mut out, &home.config(), "config.yaml");

    let source = home.source();
    let install_mode = install_state::read(home)
        .ok()
        .flatten()
        .map(|state| state.mode)
        .unwrap_or(InstallMode::Source);
    let content_label = match install_mode {
        InstallMode::Bundle => "runtime content root",
        InstallMode::Source => "source checkout",
    };
    if source.exists() {
        out.push(Finding::Ok(format!(
            "{content_label} present: {}",
            source.display()
        )));
        for rel in REQUIRED_SHIPPED {
            let p = source.join("shipped").join(rel);
            push_file_check(&mut out, &p, &format!("shipped/{}", rel));
        }
    } else {
        out.push(Finding::Warn(format!(
            "{content_label} missing: {} (run scripts/install.sh)",
            source.display()
        )));
    }

    // A real file here is the healthy shape, not a degraded one: macOS keys
    // permission grants to the executed path, so a link would resolve to a
    // per-version path and cost the operator an approval per release
    // (TASK-9P810). A surviving link still works, so it is a warning with the
    // remedy, not a failure.
    let bin = home.bin_orgasmic();
    match std::fs::symlink_metadata(&bin) {
        Ok(meta) if meta.file_type().is_symlink() => match std::fs::read_link(&bin) {
            Ok(target) => {
                if target.exists() || home.bin().join(&target).exists() {
                    out.push(Finding::Warn(format!(
                        "{} is a symlink to {}; the executed path changes with every runtime \
                         version, so macOS re-prompts for file access on each update. \
                         Run `orgasmic update` to install it as a real binary.",
                        bin.display(),
                        target.display()
                    )));
                } else {
                    out.push(Finding::Fail(format!(
                        "binary symlink dangling: {} -> {} (run `orgasmic doctor --fix`)",
                        bin.display(),
                        target.display()
                    )));
                }
            }
            Err(e) => out.push(Finding::Fail(format!(
                "read symlink {}: {}",
                bin.display(),
                e
            ))),
        },
        Ok(meta) if meta.is_file() => {
            out.push(Finding::Ok(format!("binary ok: {}", bin.display())));
        }
        Ok(_) => out.push(Finding::Fail(format!(
            "{} exists but is not a regular file (run scripts/install.sh)",
            bin.display()
        ))),
        Err(_) => out.push(Finding::Warn(format!(
            "binary missing: {} (run scripts/install.sh)",
            bin.display()
        ))),
    }

    push_cli_path_findings(&mut out, home);
    push_retired_content_findings(&mut out, home);
    push_tracked_views_findings(&mut out, home);
    // One status probe shared by the daemon findings and the member/actor
    // collision check below — the probe is a blocking HTTP round trip.
    let daemon_liveness = daemon_status(home);
    push_daemon_findings(&mut out, home, &daemon_liveness);
    push_member_actor_collision_findings(&mut out, home, &daemon_liveness);
    push_daemon_path_findings(&mut out);
    push_vendor_transcript_findings(&mut out, home);

    for finding in content_lifecycle::diagnose(home) {
        match finding {
            RegistryFinding::Warn(message) => out.push(Finding::Warn(message)),
            RegistryFinding::Fail(message) => out.push(Finding::Fail(message)),
        }
    }

    out
}

const REQUIRED_SHIPPED: &[&str] = &[
    "schema/tx.org",
    "prompt-studio/slots.org",
    "schema/state-machine.org",
    "entry/router.org",
    "workflows/default.org",
    "project-scaffold/.gitignore",
    "project-scaffold/entry.org",
    "project-scaffold/project.org",
    "project-scaffold/tasks/goal.org",
    "project-scaffold/tasks/handoff.org",
    "project-scaffold/gotchas.org",
];

#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
pub(crate) struct DaemonStatus {
    started_at: DateTime<Utc>,
    boot_id: String,
    pid: u32,
    #[serde(default)]
    ledger_sync: std::collections::BTreeMap<String, LedgerSyncStatus>,
    /// The actor the running daemon stamps on journal writes; `None` when the
    /// daemon predates the field (TASK-KA934.3.2).
    #[serde(default)]
    pub(crate) actor: Option<String>,
    /// The daemon's configured `manager.actor` fallback (TASK-KA934.3.2).
    #[serde(default)]
    pub(crate) manager_actor: Option<String>,
    /// The daemon's single writer task; `None` when the daemon predates the
    /// field (TASK-BX5SR).
    #[serde(default)]
    writer: Option<DaemonWriterStatus>,
}

#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
struct DaemonWriterStatus {
    liveness: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum DaemonLiveness {
    Running(DaemonStatus),
    Unavailable,
    Unauthorized,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct GitCommit {
    sha: String,
    subject: String,
}

/// Whether a bare `orgasmic` command resolves: the bin dir must be on PATH, and
/// (for new shells) the managed env file must exist and be sourced from startup.
fn push_cli_path_findings(out: &mut Vec<Finding>, home: &Home) {
    let bin_dir = home.bin();
    if path_env::bin_on_path(home) {
        out.push(Finding::Ok(format!(
            "cli on PATH: {} is on $PATH",
            bin_dir.display()
        )));
        return;
    }
    if let Some(link) = path_env::shim_on_path(home) {
        out.push(Finding::Ok(format!(
            "cli on PATH via shim: {} resolves orgasmic in this shell",
            link.display()
        )));
        return;
    }
    if path_env::env_file_ok(home) && path_env::rc_sourced(home) {
        out.push(Finding::Warn(format!(
            "cli not on PATH in this shell, but startup files are wired — \
             open a new terminal or run `. {}`",
            home.env_file().display()
        )));
    } else {
        out.push(Finding::Warn(format!(
            "cli not on PATH: {} is not on $PATH (run `orgasmic doctor --fix` to wire it)",
            bin_dir.display()
        )));
    }
}

// orgasmic:dec_WDR5K
/// Retired content still on disk. A hard cutover stops reading a content family
/// but leaves the operator's files where they were, so the files keep looking
/// like live configuration — and the only signal they are dead used to be a
/// daemon log line no agent reads (TASK-8ED6V). The finding therefore has to
/// carry the three things a reader needs to stop reasoning from the file: that
/// it is inert, which decision made it inert, and how to get rid of it.
///
/// It is a warning, not a failure: the files are the operator's data and an
/// install that still has them is healthy, just misleading.
fn push_retired_content_findings(out: &mut Vec<Finding>, home: &Home) {
    for retired in retired::present(home) {
        out.push(Finding::Warn(format!(
            "retired content on disk: {}\n  \
             what it was: {}\n  \
             retired by:  {} — the runtime does not read this path; anything in it is \
             inert, including any model or transport it appears to configure\n  \
             rationale:   orgasmic decision get {}\n  \
             remove it:   orgasmic doctor --remove-retired (never removed for you)",
            retired.path(home).display(),
            retired.summary,
            retired.deciding_node,
            retired.deciding_node,
        )));
    }
}

/// Remove retired residue, one path at a time, reporting each removal. Only ever
/// called from `orgasmic doctor --remove-retired`: these are the operator's
/// files, so removal is opt-in and never a side effect of an upgrade.
pub fn remove_retired_content(home: &Home) -> anyhow::Result<Vec<PathBuf>> {
    let mut removed = Vec::new();
    for retired in retired::present(home) {
        let path = retired.path(home);
        let meta =
            std::fs::symlink_metadata(&path).with_context(|| format!("stat {}", path.display()))?;
        if meta.is_dir() && !meta.file_type().is_symlink() {
            std::fs::remove_dir_all(&path).with_context(|| format!("remove {}", path.display()))?;
        } else {
            std::fs::remove_file(&path).with_context(|| format!("remove {}", path.display()))?;
        }
        removed.push(path);
    }
    Ok(removed)
}

// orgasmic:dec_AF61D,dec_XH2XY
/// Derived views are rendered on demand and never written to disk anymore, so
/// a registered project that still carries `.orgasmic/views/` — tracked or
/// merely present — is a straggler from the old regime. The daemon never
/// mutates the index of a repo it only observes, so the remedy is the explicit
/// operator verb, and the warning carries it. The git tracking probe only runs
/// inside a work tree; every registered project is still checked for a
/// leftover directory.
pub(crate) fn push_tracked_views_findings(out: &mut Vec<Finding>, home: &Home) {
    for entry in orgasmic_core::projects::read_board(home).unwrap_or_default() {
        let root = entry.path;
        let dir_present = root.join(".orgasmic/views").is_dir();
        let tracked = if is_git_work_tree(&root) {
            Command::new("git")
                .arg("-C")
                .arg(&root)
                .args(["ls-files", "--", ".orgasmic/views"])
                .output()
                .ok()
                .filter(|output| output.status.success())
                .map(|output| String::from_utf8_lossy(&output.stdout).trim().to_string())
                .unwrap_or_default()
        } else {
            String::new()
        };
        if tracked.is_empty() && !dir_present {
            continue;
        }
        let state = if tracked.is_empty() {
            "still present"
        } else {
            "tracked in git"
        };
        out.push(Finding::Warn(format!(
            "{}: .orgasmic/views/* {} — run: orgasmic project migrate",
            root.display(),
            state
        )));
    }
}

fn is_git_work_tree(root: &Path) -> bool {
    Command::new("git")
        .arg("-C")
        .arg(root)
        .args(["rev-parse", "--is-inside-work-tree"])
        .output()
        .ok()
        .filter(|output| output.status.success())
        .map(|output| String::from_utf8_lossy(&output.stdout).trim() == "true")
        .unwrap_or(false)
}

// orgasmic:dec_Q78QN,TASK-KA934.3.2
/// Inverse half of the `:ACTOR:` namespace guard. `member add` now refuses
/// daemon-actor names, but a member added before that guard — or before the
/// daemon's actor changed under it — poisons every admin journal comment
/// write: the daemon refuses to stamp an actor that shares a member name, so
/// comment moderation 403s. The finding names the colliding member and both
/// remedies. Runs off the same status probe as `push_daemon_findings`; a
/// daemon that predates the status fields reports no actor and is skipped.
fn push_member_actor_collision_findings(
    out: &mut Vec<Finding>,
    home: &Home,
    liveness: &DaemonLiveness,
) {
    let DaemonLiveness::Running(status) = liveness else {
        return;
    };
    let members = match orgasmic_core::read_members(home) {
        Ok(members) => members,
        // A parse error already breaks login and the forward guard logs it;
        // doctor has no remedy to offer that those paths do not.
        Err(_) => return,
    };
    for member in members {
        let collision = [
            ("actor", status.actor.as_deref()),
            ("manager_actor", status.manager_actor.as_deref()),
        ]
        .into_iter()
        .find(|(_, actor)| *actor == Some(member.name.as_str()))
        .and_then(|(label, actor)| actor.map(|actor| (label, actor)));
        if let Some((label, actor)) = collision {
            out.push(Finding::Warn(format!(
                "members.org name `{}` collides with the live daemon {} (`{}`) — admin journal \
                 comment writes stamping that actor are refused, so admin comment moderation 403s\n  \
                 fix: revoke and re-add the member under another name, or change the daemon \
                 actor (`manager.actor` in config.yaml) and run `orgasmic restart`",
                member.name, label, actor
            )));
        }
    }
}

fn push_daemon_path_findings(out: &mut Vec<Finding>) {
    out.extend(diagnose_daemon_path_binaries(
        &daemon_service::daemon_service_path(),
    ));
}

fn diagnose_daemon_path_binaries(path: &str) -> Vec<Finding> {
    let mut out = Vec::new();
    for binary in daemon_service::DAEMON_DRIVER_BINARIES {
        if !daemon_service::binary_resolves_on_path(binary, path) {
            out.push(Finding::Warn(format!(
                "daemon service PATH missing driver binary: {binary} \
                 (install {binary} or ensure it is on your login-shell PATH, \
                 then run `orgasmic restart` to regenerate the service definition)"
            )));
        }
    }
    for binary in daemon_service::DAEMON_HARNESS_BINARIES {
        if !daemon_service::binary_resolves_on_path(binary, path) {
            out.push(Finding::Warn(format!(
                "daemon service PATH missing harness binary: {binary} \
                 (install {binary} or ensure it is on your login-shell PATH, \
                 then run `orgasmic restart` to regenerate the service definition)"
            )));
        }
    }
    out
}

fn push_daemon_findings(out: &mut Vec<Finding>, home: &Home, liveness: &DaemonLiveness) {
    match liveness {
        DaemonLiveness::Running(status) => {
            if let Some(finding) = diagnose_daemon_staleness(home, status) {
                out.push(finding);
            }
            // orgasmic:TASK-BX5SR — reads stay healthy while every write fails
            // with `writer task is gone`, so the dead writer must be loud here.
            if status
                .writer
                .as_ref()
                .is_some_and(|writer| !writer.liveness)
            {
                out.push(Finding::Fail(
                    "daemon writer task is dead: every write fails with `writer task is gone` \
                     while reads still answer\n  fix: orgasmic restart"
                        .to_string(),
                ));
            }
            push_ledger_sync_findings(out, &status.ledger_sync);
        }
        DaemonLiveness::Unavailable => out.push(Finding::Warn(
            "daemon not running (`orgasmic status` auto-starts the local daemon)".to_string(),
        )),
        DaemonLiveness::Unauthorized => out.push(Finding::Warn(
            "daemon auth token mismatch (check $ORGASMIC_HOME/user/auth/token)".to_string(),
        )),
    }
}

fn push_ledger_sync_findings(
    out: &mut Vec<Finding>,
    statuses: &std::collections::BTreeMap<String, LedgerSyncStatus>,
) {
    for (path, status) in statuses {
        let error = status
            .error
            .as_deref()
            .unwrap_or("unknown error")
            .lines()
            .next()
            .unwrap_or("unknown error");
        match status.outcome.as_str() {
            "conflict" => out.push(Finding::Warn(format!(
                "ledger sync: {path} (conflict): {error}"
            ))),
            "failed" | "backed_off" => out.push(Finding::Warn(format!(
                "ledger sync: {path} ({} failures): {error}",
                status.consecutive_failures
            ))),
            _ => {}
        }
    }
}

fn diagnose_daemon_staleness(home: &Home, status: &DaemonStatus) -> Option<Finding> {
    let binary_mtime = binary_mtime(home);
    let commits = recent_daemon_code_commits(&home.source(), status.started_at);
    check_daemon_staleness(status, binary_mtime, &commits, SystemTime::now())
}

/// Staleness warning for `orgasmic status` when the daemon is running and predates
/// a newer binary or daemon-code commits since boot. Returns `None` when the
/// running instance is fresh.
pub fn check_daemon_for_status_with_status(home: &Home, status: &DaemonStatus) -> Option<String> {
    match diagnose_daemon_staleness(home, status)? {
        Finding::Warn(message) => Some(message),
        _ => None,
    }
}

/// Staleness warning for `orgasmic status` when the daemon is running and predates
/// a newer binary or daemon-code commits since boot. Returns `None` if the daemon
/// is down, unauthorized, or the running instance is fresh.
#[cfg_attr(not(test), allow(dead_code))]
pub fn check_daemon_for_status(home: &Home) -> Option<String> {
    let DaemonLiveness::Running(status) = daemon_status(home) else {
        return None;
    };
    check_daemon_for_status_with_status(home, &status)
}

/// The parsed daemon status when a daemon is up and authorized — for CLI verbs
/// that need the live daemon identity without doctor's full finding list
/// (the `member add` inverse guard, TASK-KA934.3.2).
pub(crate) fn live_daemon_status(home: &Home) -> Option<DaemonStatus> {
    match daemon_status(home) {
        DaemonLiveness::Running(status) => Some(status),
        _ => None,
    }
}

fn daemon_status(home: &Home) -> DaemonLiveness {
    let Ok(runtime) = tokio::runtime::Runtime::new() else {
        return DaemonLiveness::Unavailable;
    };
    runtime.block_on(async { daemon_status_async(home).await })
}

async fn daemon_status_async(home: &Home) -> DaemonLiveness {
    let Some(token) = read_daemon_token(home) else {
        return DaemonLiveness::Unauthorized;
    };
    let Some(base_url) = daemon_base_url(home) else {
        return DaemonLiveness::Unavailable;
    };
    let Ok(client) = reqwest::Client::builder()
        .timeout(Duration::from_secs(10))
        .build()
    else {
        return DaemonLiveness::Unavailable;
    };
    let response = match client
        .get(daemon_url(&base_url, "/daemon/status"))
        .bearer_auth(token)
        .send()
        .await
    {
        Ok(response) => response,
        Err(error) => {
            if error.status() == Some(StatusCode::UNAUTHORIZED) {
                return DaemonLiveness::Unauthorized;
            }
            if error.is_connect() || error.is_timeout() {
                return DaemonLiveness::Unavailable;
            }
            return DaemonLiveness::Unavailable;
        }
    };
    if response.status() == StatusCode::UNAUTHORIZED {
        return DaemonLiveness::Unauthorized;
    }
    if !response.status().is_success() {
        return DaemonLiveness::Unavailable;
    }
    match response.json::<DaemonStatus>().await {
        Ok(status) => DaemonLiveness::Running(status),
        Err(_) => DaemonLiveness::Unauthorized,
    }
}

fn read_daemon_token(home: &Home) -> Option<String> {
    daemon_client::read_bearer_token(home).ok()
}

fn daemon_base_url(home: &Home) -> Option<String> {
    if let Ok(url) = std::env::var("ORGASMIC_DAEMON_URL") {
        if !url.is_empty() {
            return Some(url);
        }
    }
    let (bind, port) = read_bind_port(&home.config())?;
    let host = if bind.is_unspecified() {
        "127.0.0.1".to_string()
    } else {
        bind.to_string()
    };
    Some(format!("http://{host}:{port}"))
}

fn read_bind_port(config: &Path) -> Option<(std::net::IpAddr, u16)> {
    let mut bind: std::net::IpAddr = "127.0.0.1".parse().ok()?;
    let mut port: u16 = 4848;
    if config.exists() {
        let raw = std::fs::read_to_string(config).ok()?;
        let value: serde_yaml::Value = serde_yaml::from_str(&raw).ok()?;
        if let Some(b) = value
            .get("bind_host")
            .or_else(|| value.get("bind"))
            .and_then(serde_yaml::Value::as_str)
        {
            if let Ok(addr) = b.parse() {
                bind = addr;
            }
        }
        if let Some(p) = value
            .get("bind_port")
            .or_else(|| value.get("port"))
            .and_then(serde_yaml::Value::as_u64)
        {
            if let Ok(p) = u16::try_from(p) {
                port = p;
            }
        }
    }
    Some((bind, port))
}

fn daemon_url(base: &str, path: &str) -> String {
    let path = api_path(path);
    if path.starts_with('/') {
        format!("{}{}", base, path)
    } else {
        format!("{}/{}", base, path)
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

fn binary_mtime(home: &Home) -> Option<SystemTime> {
    let resolved = std::fs::canonicalize(home.bin_orgasmic()).ok()?;
    std::fs::metadata(resolved).ok()?.modified().ok()
}

fn recent_daemon_code_commits(source: &Path, started_at: DateTime<Utc>) -> Vec<GitCommit> {
    if !source.join(".git").is_dir() {
        return Vec::new();
    }
    let since = format!(
        "--since={}",
        started_at.to_rfc3339_opts(SecondsFormat::Secs, true)
    );
    let output = Command::new("git")
        .arg("-C")
        .arg(source)
        .args([
            "log",
            &since,
            "--oneline",
            "--no-merges",
            "--",
            "crates/orgasmic-daemon/",
            "crates/orgasmic-core/",
        ])
        .output();
    let Ok(output) = output else {
        return Vec::new();
    };
    if !output.status.success() {
        return Vec::new();
    }
    let raw = String::from_utf8_lossy(&output.stdout);
    parse_git_oneline(&raw)
}

fn parse_git_oneline(raw: &str) -> Vec<GitCommit> {
    raw.lines()
        .filter_map(|line| {
            let line = line.trim();
            if line.is_empty() {
                return None;
            }
            let mut parts = line.splitn(2, ' ');
            let sha = parts.next()?.to_string();
            let subject = parts.next().unwrap_or("").trim().to_string();
            Some(GitCommit { sha, subject })
        })
        .collect()
}

fn check_daemon_staleness(
    status: &DaemonStatus,
    binary_mtime: Option<SystemTime>,
    git_commits_since_boot: &[GitCommit],
    now: SystemTime,
) -> Option<Finding> {
    let started_system: SystemTime = status.started_at.into();
    let binary_is_newer = binary_mtime
        .map(|mtime| mtime > started_system)
        .unwrap_or(false);
    if !binary_is_newer && git_commits_since_boot.is_empty() {
        return None;
    }

    let uptime = now
        .duration_since(started_system)
        .unwrap_or_else(|_| Duration::from_secs(0));
    let started = status.started_at.to_rfc3339_opts(SecondsFormat::Secs, true);
    let binary_built = binary_mtime
        .map(format_system_time)
        .unwrap_or_else(|| "unavailable".to_string());
    let commit_noun = if git_commits_since_boot.len() == 1 {
        "commit"
    } else {
        "commits"
    };

    let mut message = format!(
        "running daemon predates recent daemon-code merges\n  daemon uptime: {} (pid {}, boot {}, started {})\n  binary built:  {}\n  {} daemon-code {} since boot:",
        human_duration(uptime),
        status.pid,
        status.boot_id,
        started,
        binary_built,
        git_commits_since_boot.len(),
        commit_noun,
    );
    for commit in git_commits_since_boot.iter().take(3) {
        message.push_str(&format!("\n    {} {}", commit.sha, commit.subject));
    }
    if git_commits_since_boot.len() > 3 {
        let remaining = git_commits_since_boot.len() - 3;
        message.push_str(&format!("\n    ...and {} more", remaining));
    }
    message.push_str("\n  restart recommended (orgasmic restart)");

    Some(Finding::Warn(message))
}

fn format_system_time(time: SystemTime) -> String {
    let dt: DateTime<Utc> = time.into();
    dt.to_rfc3339_opts(SecondsFormat::Secs, true)
}

fn human_duration(duration: Duration) -> String {
    let secs = duration.as_secs();
    if secs < 60 {
        return "<1m".to_string();
    }
    let days = secs / 86_400;
    let hours = (secs % 86_400) / 3_600;
    let minutes = (secs % 3_600) / 60;

    if days > 0 {
        if hours > 0 {
            format!("{days}d {hours}h")
        } else {
            format!("{days}d")
        }
    } else if hours > 0 {
        if minutes > 0 {
            format!("{hours}h {minutes}m")
        } else {
            format!("{hours}h")
        }
    } else {
        format!("{minutes}m")
    }
}

// orgasmic:TASK-E3K1B, dec_Y5MPK items 5 and 6
/// Per-harness vendor transcript inventory. Report only: orgasmic never
/// prunes a vendor store, so the operator who owns retention gets the numbers.
///
/// A transcript is orgasmic-attributed exactly when its native session id
/// appears in a recorded `NativeRuntime` lifecycle event (item 6). Today only
/// claude mints its own id; the other harnesses are honestly "not computable"
/// rather than a misleading 0/N until their session-id follow-ups land.
fn push_vendor_transcript_findings(out: &mut Vec<Finding>, home: &Home) {
    let Some(roots) = TranscriptRoots::from_env_home() else {
        return;
    };
    // Recorded events live per project (`.orgasmic/tmp/sessions`) plus the
    // legacy home-level dir that boot migration may have left behind.
    let mut session_dirs = vec![home.sessions()];
    session_dirs.extend(
        orgasmic_core::projects::read_board(home)
            .unwrap_or_default()
            .iter()
            .map(|entry| project_sessions_dir(&entry.path)),
    );
    out.extend(vendor_transcript_findings(
        &roots,
        &session_dirs,
        SystemTime::now(),
    ));
}

struct VendorFile {
    path: PathBuf,
    bytes: u64,
    modified: Option<SystemTime>,
}

fn vendor_transcript_findings(
    roots: &TranscriptRoots,
    session_dirs: &[PathBuf],
    now: SystemTime,
) -> Vec<Finding> {
    let recorded = recorded_native_session_ids(session_dirs);
    // (harness, root, follow-up that blocks attribution)
    let harnesses = [
        ("claude", roots.claude_projects.clone(), None),
        (
            "codex",
            roots.codex_home.join("sessions"),
            Some("TASK-F9VEZ"),
        ),
        (
            "cursor-agent",
            roots.cursor_projects.clone(),
            Some("TASK-B6D8W"),
        ),
        ("hermes", roots.hermes_sessions.clone(), Some("TASK-2B215")),
    ];
    let mut out = Vec::new();
    for (harness, root, blocker) in harnesses {
        if !root.exists() {
            continue;
        }
        let mut files = Vec::new();
        if let Err(e) = collect_jsonl(&root, &mut files) {
            out.push(Finding::Warn(format!(
                "vendor transcripts {harness}: unreadable root {}: {e}",
                root.display()
            )));
            continue;
        }
        let split = match blocker {
            Some(task) => format!("attribution: not computable ({task})"),
            None => {
                let ids = recorded.get(harness).cloned().unwrap_or_default();
                let (ours, theirs): (Vec<&VendorFile>, Vec<&VendorFile>) = files
                    .iter()
                    .partition(|f| transcript_is_recorded(&root, &f.path, &ids));
                format!(
                    "orgasmic-attributed {}; unattributed {}",
                    summarize_vendor_files(&ours, now),
                    summarize_vendor_files(&theirs, now)
                )
            }
        };
        out.push(Finding::Ok(format!(
            "vendor transcripts {harness}: {} at {}; {split}",
            summarize_vendor_files(&files.iter().collect::<Vec<_>>(), now),
            root.display()
        )));
    }
    out
}

/// `N files, SIZE (<1d a, 1-7d b, 7-30d c, >30d d)` by mtime.
fn summarize_vendor_files(files: &[&VendorFile], now: SystemTime) -> String {
    const DAY: u64 = 24 * 60 * 60;
    let mut buckets = [0usize; 4];
    let mut bytes = 0u64;
    for f in files {
        bytes += f.bytes;
        let days = f
            .modified
            .and_then(|m| now.duration_since(m).ok())
            .map_or(0, |age| age.as_secs() / DAY);
        let bucket = match days {
            0 => 0,
            1..=6 => 1,
            7..=29 => 2,
            _ => 3,
        };
        buckets[bucket] += 1;
    }
    format!(
        "{} files, {} (<1d {}, 1-7d {}, 7-30d {}, >30d {})",
        files.len(),
        crate::manager::format_bytes(bytes),
        buckets[0],
        buckets[1],
        buckets[2],
        buckets[3]
    )
}

/// Every regular `.jsonl` under `root`, recursively; symlinks are not followed.
fn collect_jsonl(root: &Path, out: &mut Vec<VendorFile>) -> std::io::Result<()> {
    for entry in std::fs::read_dir(root)? {
        let entry = entry?;
        let file_type = entry.file_type()?;
        let path = entry.path();
        if file_type.is_dir() {
            collect_jsonl(&path, out)?;
        } else if file_type.is_file() && path.extension().and_then(|e| e.to_str()) == Some("jsonl")
        {
            let meta = entry.metadata()?;
            out.push(VendorFile {
                path,
                bytes: meta.len(),
                modified: meta.modified().ok(),
            });
        }
    }
    Ok(())
}

/// The file itself (`<id>.jsonl`) or a directory between the root and the
/// file (claude subagent transcripts nest under `<id>/`) names a recorded id.
fn transcript_is_recorded(
    root: &Path,
    path: &Path,
    ids: &std::collections::HashSet<String>,
) -> bool {
    let Ok(rel) = path.strip_prefix(root) else {
        return false;
    };
    let mut parts: Vec<&str> = rel
        .parent()
        .into_iter()
        .flat_map(|p| p.iter())
        .filter_map(|c| c.to_str())
        .collect();
    parts.extend(rel.file_stem().and_then(|s| s.to_str()));
    parts.iter().any(|p| ids.contains(*p))
}

/// provider -> native session ids from recorded `NativeRuntime` events across
/// every orgasmic session JSONL in `session_dirs`. Bounded scan: the event is
/// written at launch, so it sits in the prefix window. Unreadable or
/// malformed files are skipped — this is an inventory, not recovery.
fn recorded_native_session_ids(
    session_dirs: &[PathBuf],
) -> std::collections::HashMap<String, std::collections::HashSet<String>> {
    let mut by_provider: std::collections::HashMap<String, std::collections::HashSet<String>> =
        Default::default();
    for dir in session_dirs {
        let Ok(entries) = std::fs::read_dir(dir) else {
            continue;
        };
        for path in entries.flatten().map(|e| e.path()) {
            if path.extension().and_then(|e| e.to_str()) != Some("jsonl") {
                continue;
            }
            let Ok(scan) = orgasmic_core::scan_session_lifecycle(&path, SessionScanBudget::DEFAULT)
            else {
                continue;
            };
            for env in scan.envelopes {
                if env.kind != SessionEventKind::Lifecycle {
                    continue;
                }
                if let Ok(Lifecycle::NativeRuntime {
                    provider,
                    session_id: Some(id),
                    ..
                }) = serde_json::from_value::<Lifecycle>(env.event)
                {
                    by_provider
                        .entry(provider.trim().to_ascii_lowercase())
                        .or_default()
                        .insert(id);
                }
            }
        }
    }
    by_provider
}

fn push_dir_check(out: &mut Vec<Finding>, path: &Path, label: &str) {
    if path.is_dir() {
        out.push(Finding::Ok(format!("dir present: {}", label)));
    } else if path.exists() {
        out.push(Finding::Fail(format!(
            "expected dir, found file: {}",
            path.display()
        )));
    } else {
        out.push(Finding::Fail(format!(
            "dir missing: {} (run orgasmic init)",
            path.display()
        )));
    }
}

fn push_file_check(out: &mut Vec<Finding>, path: &Path, label: &str) {
    if path.is_file() {
        out.push(Finding::Ok(format!("file present: {}", label)));
    } else if path.exists() {
        out.push(Finding::Fail(format!(
            "expected file, found other: {}",
            path.display()
        )));
    } else {
        out.push(Finding::Fail(format!("file missing: {}", path.display())));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Duration as ChronoDuration;
    use orgasmic_daemon::{Daemon, DaemonOptions};
    // Shared with daemon_client tests: env is process-global, and these tests
    // exercise production paths that read ORGASMIC_DAEMON_URL / token vars.
    // Serialize against every other env-touching test in the crate and clear
    // the daemon env so ambient/leaked values can't reach the reads (TASK-SJQ9V).
    use crate::test_support::{env_guard, ScopedEnv};

    /// Env keys read by the daemon-status production paths these tests drive.
    const DAEMON_ENV_KEYS: &[&str] = &[
        "ORGASMIC_DAEMON_URL",
        "ORGASMIC_DAEMON_TOKEN",
        "ORGASMIC_DAEMON_TOKEN_FILE",
    ];

    fn status_started_at(started_at: DateTime<Utc>) -> DaemonStatus {
        DaemonStatus {
            started_at,
            boot_id: "boot-test".to_string(),
            pid: 42,
            ledger_sync: Default::default(),
            actor: None,
            manager_actor: None,
            writer: None,
        }
    }

    fn status_with_actor(
        started_at: DateTime<Utc>,
        actor: &str,
        manager_actor: Option<&str>,
    ) -> DaemonStatus {
        DaemonStatus {
            actor: Some(actor.to_string()),
            manager_actor: manager_actor.map(str::to_string),
            ..status_started_at(started_at)
        }
    }

    fn system_time(dt: DateTime<Utc>) -> SystemTime {
        dt.into()
    }

    fn utc(raw: &str) -> DateTime<Utc> {
        DateTime::parse_from_rfc3339(raw)
            .unwrap()
            .with_timezone(&Utc)
    }

    fn warn_message(finding: Option<Finding>) -> String {
        match finding {
            Some(Finding::Warn(message)) => message,
            other => panic!("expected warn, got {other:?}"),
        }
    }

    fn write(path: &Path, contents: &str) {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).unwrap();
        }
        std::fs::write(path, contents).unwrap();
    }

    fn run_git(repo: &Path, args: &[&str]) -> String {
        // Isolate git from ambient global/system config and any interactive
        // prompt so a developer's or CI's `~/.gitconfig` (commit.gpgsign,
        // log.showSignature, credential prompts, hook templates) can't perturb
        // these test git ops — one of the workspace-concurrency flake vectors
        // for TASK-SJQ9V. A fixed commit date makes `git log --since` filtering
        // fully deterministic regardless of wall clock or CPU scheduling.
        let output = Command::new("git")
            .arg("-C")
            .arg(repo)
            .args(args)
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_SYSTEM", "/dev/null")
            .env("GIT_TERMINAL_PROMPT", "0")
            .env("GIT_AUTHOR_DATE", "2026-01-01T00:00:00+00:00")
            .env("GIT_COMMITTER_DATE", "2026-01-01T00:00:00+00:00")
            .output()
            .expect("run git");
        assert!(
            output.status.success(),
            "git {:?} failed\nstdout={}\nstderr={}",
            args,
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        String::from_utf8_lossy(&output.stdout).trim().to_string()
    }

    fn daemon_options() -> DaemonOptions {
        DaemonOptions {
            bind_override: Some("127.0.0.1".parse().unwrap()),
            port_override: Some(0),
            ..DaemonOptions::default()
        }
    }

    fn unused_port() -> u16 {
        let listener = std::net::TcpListener::bind(("127.0.0.1", 0)).unwrap();
        listener.local_addr().unwrap().port()
    }

    fn write_config_port(home: &Home, port: u16) {
        write(
            &home.config(),
            &format!("bind_host: 127.0.0.1\nbind_port: {port}\n"),
        );
    }

    fn write_required_shipped(root: &Path) {
        for rel in REQUIRED_SHIPPED {
            write(&root.join("shipped").join(rel), "ok\n");
        }
    }

    #[cfg(unix)]
    fn symlink(target: impl AsRef<Path>, link: impl AsRef<Path>) {
        let _ = std::fs::remove_file(link.as_ref());
        std::os::unix::fs::symlink(target, link).unwrap();
    }

    fn count_warns(findings: &[Finding], needle: &str) -> usize {
        findings
            .iter()
            .filter(|finding| matches!(finding, Finding::Warn(message) if message.contains(needle)))
            .count()
    }

    #[test]
    fn ledger_sync_failures_are_doctor_warnings() {
        let statuses = serde_json::from_value(serde_json::json!({
            "/tmp/backed-off": {
                "outcome": "backed_off",
                "error": "push failed\nsecond line",
                "consecutive_failures": 3
            },
            "/tmp/conflict": {
                "outcome": "conflict",
                "error": "paths parked\nsecond line",
                "consecutive_failures": 0
            },
            "/tmp/healthy": {
                "outcome": "synced",
                "consecutive_failures": 0
            }
        }))
        .unwrap();
        let mut findings = Vec::new();

        push_ledger_sync_findings(&mut findings, &statuses);

        assert_eq!(
            findings,
            vec![
                Finding::Warn("ledger sync: /tmp/backed-off (3 failures): push failed".into()),
                Finding::Warn("ledger sync: /tmp/conflict (conflict): paths parked".into()),
            ]
        );
    }

    fn assert_human_duration(secs: u64, expected: &str) {
        assert_eq!(human_duration(Duration::from_secs(secs)), expected);
    }

    fn running(status: DaemonStatus) -> DaemonLiveness {
        DaemonLiveness::Running(status)
    }

    // orgasmic:TASK-BX5SR
    #[test]
    fn doctor_fails_when_the_daemon_writer_is_dead() {
        let tmp = tempfile::tempdir().unwrap();
        let home = Home::at(tmp.path().join("home"));
        home.ensure().unwrap();
        let dead: DaemonStatus = serde_json::from_value(serde_json::json!({
            "started_at": "2026-09-02T00:00:00Z",
            "boot_id": "boot-test",
            "pid": 42,
            "writer": { "liveness": false, "queue_depth": 0 }
        }))
        .unwrap();
        let alive = DaemonStatus {
            writer: Some(DaemonWriterStatus { liveness: true }),
            ..status_started_at(utc("2026-09-02T00:00:00Z"))
        };

        let mut findings = Vec::new();
        push_daemon_findings(&mut findings, &home, &running(dead));
        let fails: Vec<_> = findings.iter().filter(|f| f.is_fail()).collect();
        assert_eq!(fails.len(), 1, "{findings:?}");
        let Finding::Fail(message) = fails[0] else {
            unreachable!()
        };
        assert!(message.contains("writer task is dead"), "{message}");
        assert!(message.contains("orgasmic restart"), "{message}");

        let mut findings = Vec::new();
        push_daemon_findings(&mut findings, &home, &running(alive));
        assert!(findings.iter().all(|f| !f.is_fail()), "{findings:?}");
        let mut findings = Vec::new();
        push_daemon_findings(
            &mut findings,
            &home,
            &running(status_started_at(utc("2026-09-02T00:00:00Z"))),
        );
        assert!(findings.iter().all(|f| !f.is_fail()), "{findings:?}");
    }

    // orgasmic:dec_Q78QN,TASK-KA934.3.2
    #[test]
    fn doctor_warns_when_member_name_equals_live_daemon_actor() {
        let tmp = tempfile::tempdir().unwrap();
        let home = Home::at(tmp.path().join("home"));
        home.ensure().unwrap();
        orgasmic_core::add_member(
            &home,
            "alice",
            &[("proj-a".to_string(), "viewer".to_string())],
        )
        .unwrap();

        let mut findings = Vec::new();
        push_member_actor_collision_findings(
            &mut findings,
            &home,
            &running(status_with_actor(
                utc("2026-09-02T00:00:00Z"),
                "alice",
                None,
            )),
        );

        assert_eq!(findings.len(), 1, "{findings:?}");
        let Finding::Warn(message) = &findings[0] else {
            panic!("expected warn, got {findings:?}")
        };
        assert!(message.contains("alice"), "{message}");
        assert!(message.contains("collides"), "{message}");
        assert!(message.contains("daemon actor"), "{message}");
        assert!(message.contains("manager.actor"), "{message}");
    }

    #[test]
    fn doctor_warns_when_member_name_equals_manager_actor() {
        let tmp = tempfile::tempdir().unwrap();
        let home = Home::at(tmp.path().join("home"));
        home.ensure().unwrap();
        orgasmic_core::add_member(&home, "bob", &[("*".to_string(), "editor".to_string())])
            .unwrap();

        let mut findings = Vec::new();
        push_member_actor_collision_findings(
            &mut findings,
            &home,
            &running(status_with_actor(
                utc("2026-09-02T00:00:00Z"),
                "carol",
                Some("bob"),
            )),
        );

        assert_eq!(findings.len(), 1, "{findings:?}");
        let Finding::Warn(message) = &findings[0] else {
            panic!("expected warn, got {findings:?}")
        };
        assert!(message.contains("bob"), "{message}");
        assert!(message.contains("manager_actor"), "{message}");
    }

    #[test]
    fn doctor_member_actor_no_collision_stays_silent() {
        let tmp = tempfile::tempdir().unwrap();
        let home = Home::at(tmp.path().join("home"));
        home.ensure().unwrap();
        orgasmic_core::add_member(
            &home,
            "alice",
            &[("proj-a".to_string(), "viewer".to_string())],
        )
        .unwrap();

        // Live daemon actors name nobody on file…
        let mut findings = Vec::new();
        push_member_actor_collision_findings(
            &mut findings,
            &home,
            &running(status_with_actor(
                utc("2026-09-02T00:00:00Z"),
                "daemon-admin",
                None,
            )),
        );
        assert!(findings.is_empty(), "{findings:?}");

        // …and so does an old daemon that predates the status fields.
        let mut findings = Vec::new();
        push_member_actor_collision_findings(
            &mut findings,
            &home,
            &running(status_started_at(utc("2026-09-02T00:00:00Z"))),
        );
        assert!(findings.is_empty(), "{findings:?}");

        // A down daemon has no live actor to collide with.
        let mut findings = Vec::new();
        push_member_actor_collision_findings(&mut findings, &home, &DaemonLiveness::Unavailable);
        assert!(findings.is_empty(), "{findings:?}");
    }

    #[test]
    fn daemon_staleness_warns_for_newer_binary_mtime() {
        let now = utc("2026-05-24T12:00:00Z");
        let started_at = now - ChronoDuration::hours(1);
        let status = status_started_at(started_at);
        let binary_mtime = system_time(now);

        let message = warn_message(check_daemon_staleness(
            &status,
            Some(binary_mtime),
            &[],
            system_time(now),
        ));

        assert!(message.contains("running daemon predates"));
        assert!(message.contains("daemon uptime: 1h"));
        assert!(message.contains("binary built:"));
        assert!(message.contains("0 daemon-code commits since boot"));
        assert!(message.contains("restart recommended (orgasmic restart)"));
    }

    #[test]
    fn daemon_staleness_warns_for_git_commits_since_boot() {
        // Serialize against every other heavy real-subprocess test in the
        // workspace: this test spawns 6 real `git` subprocesses whose
        // `run_git` panics on any transient spawn failure under load (TASK-X0ZVE
        // flock class; TASK-SJQ9V residual).
        let _live_guard = crate::test_support::live_session_guard();
        let tmp = tempfile::tempdir().unwrap();
        run_git(tmp.path(), &["init"]);
        run_git(tmp.path(), &["config", "user.email", "tester@example.com"]);
        run_git(tmp.path(), &["config", "user.name", "Test User"]);
        write(
            &tmp.path().join("crates/orgasmic-daemon/foo.rs"),
            "pub fn route() {}\n",
        );
        run_git(tmp.path(), &["add", "."]);
        run_git(tmp.path(), &["commit", "-m", "TASK-052 daemon route"]);
        let sha = run_git(tmp.path(), &["rev-parse", "--short", "HEAD"]);

        // The commit is pinned to 2026-01-01T00:00:00Z (GIT_COMMITTER_DATE in
        // run_git); start the window an hour before it so `git log --since`
        // deterministically includes it, with no dependence on the wall clock.
        let started_at = utc("2026-01-01T00:00:00Z") - ChronoDuration::hours(1);
        // `recent_daemon_code_commits` spawns `git log` and silently yields an
        // empty vec on any git non-success; under heavy `cargo test --workspace`
        // load that git subprocess can transiently fail (CPU/process pressure),
        // so retry a few times — the commit is guaranteed present here, an empty
        // result means a transient failure, not "no commits" (TASK-SJQ9V).
        let mut commits = recent_daemon_code_commits(tmp.path(), started_at);
        for _ in 0..8 {
            if !commits.is_empty() {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(50));
            commits = recent_daemon_code_commits(tmp.path(), started_at);
        }
        let status = status_started_at(started_at);
        let binary_mtime = system_time(started_at - ChronoDuration::hours(1));

        let message = warn_message(check_daemon_staleness(
            &status,
            Some(binary_mtime),
            &commits,
            system_time(started_at + ChronoDuration::hours(1)),
        ));

        assert_eq!(commits.len(), 1);
        assert!(message.contains("1 daemon-code commit since boot"));
        assert!(message.contains(&sha));
        assert!(message.contains("TASK-052 daemon route"));
    }

    #[test]
    fn daemon_staleness_clean_state_is_silent() {
        let started_at = utc("2026-05-24T12:00:00Z");
        let status = status_started_at(started_at);
        let binary_mtime = system_time(started_at - ChronoDuration::hours(2));

        let finding =
            check_daemon_staleness(&status, Some(binary_mtime), &[], system_time(started_at));

        assert_eq!(finding, None);
    }

    #[test]
    fn human_duration_formats_zero_as_less_than_one_minute() {
        assert_human_duration(0, "<1m");
    }

    #[test]
    fn human_duration_formats_59s_as_less_than_one_minute() {
        assert_human_duration(59, "<1m");
    }

    #[test]
    fn human_duration_formats_60s_as_one_minute() {
        assert_human_duration(60, "1m");
    }

    #[test]
    fn human_duration_formats_hour_and_minutes() {
        assert_human_duration(3_840, "1h 4m");
    }

    #[test]
    fn human_duration_formats_days_and_hours() {
        assert_human_duration(183_600, "2d 3h");
    }

    #[test]
    fn human_duration_formats_30d_5h() {
        assert_human_duration(2_610_000, "30d 5h");
    }

    #[test]
    fn human_duration_formats_year_scale_uptime() {
        assert_human_duration(31_557_600, "365d 6h");
    }

    #[test]
    fn daemon_staleness_caps_commit_details_at_three() {
        let now = utc("2026-05-24T12:00:00Z");
        let started_at = now - ChronoDuration::hours(1);
        let status = status_started_at(started_at);
        let commits = vec![
            GitCommit {
                sha: "aaa1111".to_string(),
                subject: "first".to_string(),
            },
            GitCommit {
                sha: "bbb2222".to_string(),
                subject: "second".to_string(),
            },
            GitCommit {
                sha: "ccc3333".to_string(),
                subject: "third".to_string(),
            },
            GitCommit {
                sha: "ddd4444".to_string(),
                subject: "fourth".to_string(),
            },
        ];

        let message = warn_message(check_daemon_staleness(
            &status,
            None,
            &commits,
            system_time(now),
        ));

        assert!(message.contains("4 daemon-code commits since boot"));
        assert!(message.contains("aaa1111 first"));
        assert!(message.contains("bbb2222 second"));
        assert!(message.contains("ccc3333 third"));
        assert!(!message.contains("ddd4444 fourth"));
        assert!(message.contains("    ...and 1 more"));
    }

    #[test]
    fn daemon_status_connection_refused_emits_one_liveness_warn() {
        let _env_guard = env_guard();
        let _env = ScopedEnv::clear(DAEMON_ENV_KEYS);
        let tmp = tempfile::tempdir().unwrap();
        let home = Home::at(tmp.path().join("home"));
        home.ensure().unwrap();
        write_config_port(&home, unused_port());
        write(&home.auth_token(), "test-token\n");

        assert_eq!(daemon_status(&home), DaemonLiveness::Unavailable);

        let findings = diagnose(&home);
        assert_eq!(count_warns(&findings, "daemon not running"), 1);
        assert_eq!(count_warns(&findings, "running daemon predates"), 0);
    }

    #[test]
    fn daemon_status_token_mismatch_emits_one_liveness_warn() {
        let _env_guard = env_guard();
        let _env = ScopedEnv::clear(DAEMON_ENV_KEYS);
        let tmp = tempfile::tempdir().unwrap();
        let home = Home::at(tmp.path().join("home"));
        let runtime = tokio::runtime::Runtime::new().unwrap();
        let running = runtime
            .block_on(Daemon::run(home.clone(), daemon_options()))
            .expect("boot daemon");
        write_config_port(&home, running.addr.port());
        write(&home.auth_token(), "wrong-token\n");

        assert_eq!(daemon_status(&home), DaemonLiveness::Unauthorized);

        let findings = diagnose(&home);
        assert_eq!(count_warns(&findings, "daemon auth token mismatch"), 1);
        assert_eq!(count_warns(&findings, "running daemon predates"), 0);

        let _ = running.shutdown.send(());
        runtime.block_on(running.join).unwrap();
    }

    #[test]
    fn fresh_home_passes_layout_checks_but_warns_no_content_root() {
        let tmp = tempfile::tempdir().unwrap();
        let home = Home::at(tmp.path().join("home"));
        home.ensure().unwrap();
        let findings = diagnose(&home);
        // All home/required-dir checks pass.
        assert!(findings
            .iter()
            .any(|f| matches!(f, Finding::Ok(s) if s.contains("config.yaml"))));
        // Content root missing → Warn, not Fail.
        assert!(findings
            .iter()
            .any(|f| matches!(f, Finding::Warn(s) if s.contains("source checkout missing"))));
        // No FAILs from the layout checks.
        let fails: Vec<&Finding> = findings.iter().filter(|f| f.is_fail()).collect();
        assert!(fails.iter().all(|f| matches!(f, Finding::Fail(s) if s.contains("binary symlink") || s.contains("file missing"))));
    }

    #[test]
    #[cfg(unix)]
    fn bundle_runtime_content_root_without_git_is_healthy() {
        let tmp = tempfile::tempdir().unwrap();
        let home = Home::at(tmp.path().join("home"));
        home.ensure().unwrap();
        let runtime = home.runtimes().join("1.0.0-darwin-aarch64");
        write_required_shipped(&runtime);
        write(&runtime.join("bin/orgasmic"), "#!/bin/sh\n");
        symlink("runtimes/1.0.0-darwin-aarch64", home.current_runtime());
        symlink("current", home.source());
        symlink("../current/bin/orgasmic", home.bin_orgasmic());
        install_state::write(
            &home,
            &crate::install_state::InstallState {
                mode: InstallMode::Bundle,
                channel: Some("nightly".to_string()),
                version: Some("1.0.0".to_string()),
                target: Some("darwin-aarch64".to_string()),
                manifest_url: None,
                runtime_dir: Some(runtime),
                source_checkout: None,
            },
        )
        .unwrap();

        let findings = diagnose(&home);

        assert!(findings
            .iter()
            .any(|f| matches!(f, Finding::Ok(s) if s.contains("runtime content root present"))));
        assert!(!findings
            .iter()
            .any(|f| matches!(f, Finding::Warn(s) if s.contains("source checkout missing"))));
    }

    #[test]
    fn missing_home_dir_is_failure() {
        let tmp = tempfile::tempdir().unwrap();
        let home = Home::at(tmp.path().join("nope"));
        // do NOT call ensure
        let findings = diagnose(&home);
        assert!(findings.iter().any(Finding::is_fail));
    }

    #[test]
    fn check_daemon_for_status_none_when_unauthorized() {
        let _env_guard = env_guard();
        let _env = ScopedEnv::clear(DAEMON_ENV_KEYS);
        let tmp = tempfile::tempdir().unwrap();
        let home = Home::at(tmp.path().join("home"));
        let runtime = tokio::runtime::Runtime::new().unwrap();
        let running = runtime
            .block_on(Daemon::run(home.clone(), daemon_options()))
            .expect("boot daemon");
        write_config_port(&home, running.addr.port());
        write(&home.auth_token(), "wrong-token\n");

        assert_eq!(check_daemon_for_status(&home), None);

        let _ = running.shutdown.send(());
        runtime.block_on(running.join).unwrap();
    }

    #[test]
    fn check_daemon_for_status_none_when_daemon_down() {
        let _env_guard = env_guard();
        let _env = ScopedEnv::clear(DAEMON_ENV_KEYS);
        let tmp = tempfile::tempdir().unwrap();
        let home = Home::at(tmp.path().join("home"));
        home.ensure().unwrap();
        write_config_port(&home, unused_port());
        write(&home.auth_token(), "test-token\n");

        assert_eq!(check_daemon_for_status(&home), None);
    }

    #[test]
    #[cfg(unix)]
    fn check_daemon_for_status_none_when_fresh() {
        let _env_guard = env_guard();
        let _env = ScopedEnv::clear(DAEMON_ENV_KEYS);
        let tmp = tempfile::tempdir().unwrap();
        let home = Home::at(tmp.path().join("home"));
        home.ensure().unwrap();
        let runtime = tokio::runtime::Runtime::new().unwrap();
        let running = runtime
            .block_on(Daemon::run(home.clone(), daemon_options()))
            .expect("boot daemon");
        write_config_port(&home, running.addr.port());

        assert_eq!(check_daemon_for_status(&home), None);

        let _ = running.shutdown.send(());
        runtime.block_on(running.join).unwrap();
    }

    #[test]
    #[cfg(unix)]
    fn check_daemon_for_status_some_when_stale() {
        let _env_guard = env_guard();
        let _env = ScopedEnv::clear(DAEMON_ENV_KEYS);
        let tmp = tempfile::tempdir().unwrap();
        let home = Home::at(tmp.path().join("home"));
        home.ensure().unwrap();
        let runtime = tokio::runtime::Runtime::new().unwrap();
        let running = runtime
            .block_on(Daemon::run(home.clone(), daemon_options()))
            .expect("boot daemon");
        write_config_port(&home, running.addr.port());

        std::thread::sleep(Duration::from_millis(1_100));
        let stub = tmp.path().join("orgasmic-stub");
        write(&stub, "#!/bin/sh\nexit 0\n");
        std::os::unix::fs::symlink(&stub, home.bin_orgasmic()).unwrap();

        let message = check_daemon_for_status(&home).expect("expected staleness warn");
        assert!(message.contains("running daemon predates"));
        assert!(message.contains("restart recommended (orgasmic restart)"));

        let _ = running.shutdown.send(());
        runtime.block_on(running.join).unwrap();
    }

    #[test]
    fn dangling_binary_symlink_is_failure() {
        let tmp = tempfile::tempdir().unwrap();
        let home = Home::at(tmp.path().join("home"));
        home.ensure().unwrap();
        let bin = home.bin_orgasmic();
        // create a symlink to a path that does not exist
        #[cfg(unix)]
        std::os::unix::fs::symlink(tmp.path().join("does-not-exist/orgasmic"), &bin).unwrap();
        let findings = diagnose(&home);
        assert!(findings
            .iter()
            .any(|f| matches!(f, Finding::Fail(s) if s.contains("dangling"))));
    }

    #[test]
    fn doctor_warns_when_daemon_path_missing_binaries() {
        let empty_path = tempfile::tempdir()
            .unwrap()
            .path()
            .join("empty-bin")
            .to_string_lossy()
            .into_owned();
        std::fs::create_dir_all(&empty_path).unwrap();

        let findings = diagnose_daemon_path_binaries(&empty_path);
        assert!(findings.iter().any(|finding| {
            matches!(
                finding,
                Finding::Warn(message)
                    if message.contains("daemon service PATH missing driver binary: tmux")
            )
        }));
        assert!(findings.iter().any(|finding| {
            matches!(
                finding,
                Finding::Warn(message)
                    if message.contains("daemon service PATH missing harness binary: claude")
            )
        }));
        assert!(findings.iter().all(|finding| {
            matches!(
                finding,
                Finding::Warn(message) if message.contains("orgasmic restart")
            )
        }));
    }

    // ---- vendor transcript inventory (TASK-E3K1B) ----

    fn write_native_runtime_session(dir: &Path, provider: &str, session_id: &str) {
        let envelope = orgasmic_core::SessionEnvelope {
            seq: 0,
            time: Utc::now(),
            run_id: "run-1".into(),
            runtime_id: "rt-1".into(),
            boot_id: "boot-1".into(),
            kind: SessionEventKind::Lifecycle,
            event: serde_json::to_value(Lifecycle::NativeRuntime {
                provider: provider.into(),
                session_id: Some(session_id.into()),
                session_path: None,
                launch_argv: vec![],
                resume_argv: vec![],
            })
            .unwrap(),
        };
        let line = serde_json::to_string(&envelope).unwrap();
        write(&dir.join("run-1.jsonl"), &format!("{line}\n"));
    }

    fn vendor_findings(findings: &[Finding]) -> Vec<&Finding> {
        findings
            .iter()
            .filter(|f| matches!(f, Finding::Ok(s) | Finding::Warn(s) if s.starts_with("vendor transcripts")))
            .collect()
    }

    #[test]
    fn doctor_transcript_inventory_splits_claude_by_recorded_native_runtime() {
        let tmp = tempfile::tempdir().unwrap();
        let roots = TranscriptRoots::from_home(tmp.path().join("vendor"));
        let slug = roots.claude_projects.join("-Users-me-proj");
        write(&slug.join("aaaa-1111.jsonl"), "0123456789");
        write(&slug.join("bbbb-2222.jsonl"), "01234567890123456789");
        // Ten days old → the 7-30d bucket.
        let old = std::fs::OpenOptions::new()
            .write(true)
            .open(slug.join("bbbb-2222.jsonl"))
            .unwrap();
        old.set_modified(SystemTime::now() - Duration::from_secs(10 * 24 * 60 * 60))
            .unwrap();
        let sessions = tmp.path().join("sessions");
        write_native_runtime_session(&sessions, "claude", "aaaa-1111");

        let findings = vendor_transcript_findings(&roots, &[sessions], SystemTime::now());
        let lines = vendor_findings(&findings);
        assert_eq!(lines.len(), 1, "{findings:?}");
        let Finding::Ok(line) = lines[0] else {
            panic!("expected info finding: {:?}", lines[0]);
        };
        assert!(
            line.starts_with(
                "vendor transcripts claude: 2 files, 30B (<1d 1, 1-7d 0, 7-30d 1, >30d 0) at "
            ),
            "{line}"
        );
        assert!(
            line.ends_with(
                "; orgasmic-attributed 1 files, 10B (<1d 1, 1-7d 0, 7-30d 0, >30d 0); \
                 unattributed 1 files, 20B (<1d 0, 1-7d 0, 7-30d 1, >30d 0)"
            ),
            "{line}"
        );
    }

    #[test]
    fn doctor_transcript_inventory_codex_attribution_is_not_computable() {
        let tmp = tempfile::tempdir().unwrap();
        let roots = TranscriptRoots::from_home(tmp.path().join("vendor"));
        write(
            &roots.codex_home.join("sessions/2026/09/01/rollout-x.jsonl"),
            "{}\n",
        );
        // A recorded codex id must not flip the line to 0/N: the split is
        // only honest once TASK-F9VEZ lands.
        let sessions = tmp.path().join("sessions");
        write_native_runtime_session(&sessions, "codex", "x");

        let findings = vendor_transcript_findings(&roots, &[sessions], SystemTime::now());
        let lines = vendor_findings(&findings);
        assert_eq!(lines.len(), 1, "{findings:?}");
        let Finding::Ok(line) = lines[0] else {
            panic!("expected info finding: {:?}", lines[0]);
        };
        assert!(
            line.starts_with("vendor transcripts codex: 1 files, 3B"),
            "{line}"
        );
        assert!(
            line.ends_with("; attribution: not computable (TASK-F9VEZ)"),
            "{line}"
        );
    }

    #[test]
    fn doctor_transcript_inventory_missing_roots_emit_nothing() {
        let tmp = tempfile::tempdir().unwrap();
        let roots = TranscriptRoots::from_home(tmp.path().join("vendor"));
        let findings = vendor_transcript_findings(&roots, &[], SystemTime::now());
        assert!(vendor_findings(&findings).is_empty(), "{findings:?}");
    }
}
