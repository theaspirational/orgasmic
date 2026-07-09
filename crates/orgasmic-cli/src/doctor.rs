// arch: arch_C87Z9.5
// orgasmic:arch_WZFAX,dec_2D5BC
//! Diagnose the local orgasmic install — missing home dirs, missing shipped
//! files, broken binary symlink.

use std::path::Path;
use std::process::Command;
use std::time::{Duration, SystemTime};

use chrono::{DateTime, SecondsFormat, Utc};
use reqwest::StatusCode;
use serde::Deserialize;

use crate::content_lifecycle::{self, RegistryFinding};
use crate::daemon_client;
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

    let bin = home.bin_orgasmic();
    match std::fs::symlink_metadata(&bin) {
        Ok(meta) if meta.file_type().is_symlink() => match std::fs::read_link(&bin) {
            Ok(target) => {
                if target.exists() || home.bin().join(&target).exists() {
                    out.push(Finding::Ok(format!(
                        "binary symlink ok: {} -> {}",
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
        Ok(_) => out.push(Finding::Warn(format!(
            "{} exists but is not a symlink (run scripts/install.sh)",
            bin.display()
        ))),
        Err(_) => out.push(Finding::Warn(format!(
            "binary symlink missing: {} (run scripts/install.sh)",
            bin.display()
        ))),
    }

    push_cli_path_findings(&mut out, home);
    push_daemon_findings(&mut out, home);
    push_daemon_path_findings(&mut out);

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
    "skills/orgasmic/scaffold/.gitignore",
    "skills/orgasmic/scaffold/entry.org",
    "skills/orgasmic/scaffold/project.org",
    "skills/orgasmic/scaffold/decisions.org",
    "skills/orgasmic/scaffold/tasks/backlog.org",
    "skills/orgasmic/scaffold/tasks/todo.org",
    "skills/orgasmic/scaffold/tasks/in_progress.org",
    "skills/orgasmic/scaffold/tasks/in_review.org",
    "skills/orgasmic/scaffold/tasks/done.org",
    "skills/orgasmic/scaffold/tasks/cancelled.org",
    "skills/orgasmic/scaffold/tasks/goal.org",
    "skills/orgasmic/scaffold/tasks/handoff.org",
    "skills/orgasmic/scaffold/gotchas.org",
    "skills/orgasmic/scaffold/conventions/contributing.org",
    "skills/orgasmic/scaffold/conventions/no-skill-installed.org",
    "skills/orgasmic/scaffold/conventions/orgasmic-tooling.org",
];

#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
pub struct DaemonStatus {
    started_at: DateTime<Utc>,
    boot_id: String,
    pid: u32,
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

fn push_daemon_findings(out: &mut Vec<Finding>, home: &Home) {
    match daemon_status(home) {
        DaemonLiveness::Running(status) => {
            if let Some(finding) = diagnose_daemon_staleness(home, &status) {
                out.push(finding);
            }
        }
        DaemonLiveness::Unavailable => out.push(Finding::Warn(
            "daemon not running (`orgasmic status` auto-starts the local daemon)".to_string(),
        )),
        DaemonLiveness::Unauthorized => out.push(Finding::Warn(
            "daemon auth token mismatch (check $ORGASMIC_HOME/user/auth/token)".to_string(),
        )),
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

    fn assert_human_duration(secs: u64, expected: &str) {
        assert_eq!(human_duration(Duration::from_secs(secs)), expected);
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
}
