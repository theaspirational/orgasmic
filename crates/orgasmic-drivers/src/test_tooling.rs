//! Shared support for tests whose assertions require host tooling.
//!
//! This lives in the normal library because integration-test crates cannot
//! import `#[cfg(test)]` modules. It is deliberately transport-neutral.

use std::collections::BTreeSet;
#[cfg(unix)]
use std::io::Write as _;
use std::process::{Command, Stdio};
use std::sync::Mutex;
use std::time::Duration;

pub const ALLOW_MISSING_TOOLS_ENV: &str = "ORGASMIC_ALLOW_MISSING_TOOLS";
pub const ALLOW_BILLED_TESTS_ENV: &str = "ORGASMIC_ALLOW_BILLED_TESTS";

/// Tool probes and tests that temporarily mutate process environment share
/// this lock so a sentinel never observes another test's synthetic PATH.
#[must_use]
pub fn test_environment_lock() -> &'static tokio::sync::Mutex<()> {
    static LOCK: std::sync::OnceLock<tokio::sync::Mutex<()>> = std::sync::OnceLock::new();
    LOCK.get_or_init(|| tokio::sync::Mutex::new(()))
}

#[derive(Debug, Clone, Copy)]
pub struct ToolRequirement {
    tool: &'static str,
    gated_tests: usize,
    available: bool,
}

impl ToolRequirement {
    #[must_use]
    pub const fn new(tool: &'static str, gated_tests: usize, available: bool) -> Self {
        Self {
            tool,
            gated_tests,
            available,
        }
    }
}

#[must_use]
pub fn command_available(tool: &str) -> bool {
    let Some(paths) = std::env::var_os("PATH") else {
        return false;
    };
    std::env::split_paths(&paths)
        .map(|dir| dir.join(tool))
        .any(|candidate| candidate.is_file())
}

#[must_use]
pub fn command_succeeds(tool: &str, args: &[&str]) -> bool {
    Command::new(tool)
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_ok_and(|status| status.success())
}

#[must_use]
pub fn billed_tests_allowed() -> bool {
    std::env::var(ALLOW_BILLED_TESTS_ENV).is_ok_and(|value| value.trim() == "1")
}

#[must_use]
pub fn skip_unless_billing_allowed(test_name: &str) -> bool {
    if billed_tests_allowed() {
        return false;
    }
    emit_visible_notice(&format!(
        "skipping {test_name}: it submits a real, billed provider turn; \
         set {ALLOW_BILLED_TESTS_ENV}=1 to opt in"
    ));
    true
}

pub fn report_billed_tests(tests: &[&str]) {
    if tests.is_empty() {
        return;
    }
    let noun = if tests.len() == 1 { "test" } else { "tests" };
    let names = tests.join(", ");
    let count = tests.len();
    if billed_tests_allowed() {
        emit_visible_notice(&format!(
            "warning: {ALLOW_BILLED_TESTS_ENV}=1 arms {count} billed {noun}: {names}; \
             running with `--ignored` will spend real money"
        ));
    } else {
        emit_visible_notice(&format!(
            "notice: {count} billed {noun} withheld by default and counted by no tool \
             requirement above: {names}; each is `#[ignore]`d and additionally needs \
             {ALLOW_BILLED_TESTS_ENV}=1"
        ));
    }
}

#[must_use]
pub fn skip_test_if_missing(test_name: &str, tooling: &[(&str, bool)]) -> bool {
    let missing = tooling
        .iter()
        .filter_map(|(tool, available)| (!available).then_some(*tool))
        .collect::<Vec<_>>();
    if missing.is_empty() {
        return false;
    }
    eprintln!(
        "skipping {test_name}: missing test tooling: {}",
        missing.join(", ")
    );
    true
}

pub fn assert_not_degraded(test_name: &str, degraded: bool) {
    assert!(
        !degraded,
        "{test_name}: live driver degraded to inert instead of exercising its assertions"
    );
}

pub fn assert_required_test_tooling(requirements: &[ToolRequirement]) {
    let missing = requirements
        .iter()
        .filter(|requirement| !requirement.available)
        .collect::<Vec<_>>();
    if missing.is_empty() {
        return;
    }

    let allowed = std::env::var(ALLOW_MISSING_TOOLS_ENV)
        .unwrap_or_default()
        .split(',')
        .map(str::trim)
        .filter(|tool| !tool.is_empty())
        .map(ToOwned::to_owned)
        .collect::<BTreeSet<_>>();
    let allowed_missing = missing
        .iter()
        .copied()
        .filter(|requirement| allowed.contains(requirement.tool))
        .collect::<Vec<_>>();
    let required_missing = missing
        .iter()
        .copied()
        .filter(|requirement| !allowed.contains(requirement.tool))
        .collect::<Vec<_>>();

    if !allowed_missing.is_empty() {
        emit_visible_notice(&format!(
            "warning: {ALLOW_MISSING_TOOLS_ENV} explicitly allows missing test tooling: \
             {}; those gated tests did not run",
            format_requirements(&allowed_missing)
        ));
    }
    if required_missing.is_empty() {
        return;
    }

    let required_details = format_requirements(&required_missing);
    let opt_out = required_missing
        .iter()
        .map(|requirement| requirement.tool)
        .collect::<Vec<_>>()
        .join(",");
    panic!(
        "required test tooling is missing: {required_details}; gated tests did not run. \
         Install the tooling or explicitly acknowledge only these skips with \
         {ALLOW_MISSING_TOOLS_ENV}={opt_out}"
    );
}

fn format_requirements(requirements: &[&ToolRequirement]) -> String {
    requirements
        .iter()
        .map(|requirement| {
            let noun = if requirement.gated_tests == 1 {
                "test"
            } else {
                "tests"
            };
            format!(
                "{} (gates {} {noun})",
                requirement.tool, requirement.gated_tests
            )
        })
        .collect::<Vec<_>>()
        .join(", ")
}

const LIVE_SESSION_LOCK_FILE: &str = "orgasmic-live-session-tests.lock";

/// Serialize real-terminal tests across all workspace test binaries.
#[must_use]
pub fn live_session_guard() -> LiveSessionGuard {
    let path = std::env::temp_dir().join(LIVE_SESSION_LOCK_FILE);
    let lock = std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(&path)
        .expect("open live-session lock file");
    fs2::FileExt::lock_exclusive(&lock).expect("flock live-session lock");
    LiveSessionGuard {
        lock,
        owned_sessions: Mutex::new(Vec::new()),
        owned_groups: Mutex::new(Vec::new()),
    }
}

pub struct LiveSessionGuard {
    lock: std::fs::File,
    owned_sessions: Mutex<Vec<OwnedTmuxSession>>,
    owned_groups: Mutex<Vec<u32>>,
}

enum OwnedTmuxSession {
    All,
    Run(String),
    Named(String),
}

impl LiveSessionGuard {
    /// Reap every non-keepalive session on this process's isolated tmux server.
    pub fn owns_all_tmux_sessions(&self) -> &Self {
        self.owned_sessions
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .push(OwnedTmuxSession::All);
        self
    }

    /// Reap every tmux session created for `run_id` when the guard drops.
    pub fn owns(&self, run_id: impl Into<String>) -> &Self {
        self.owned_sessions
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .push(OwnedTmuxSession::Run(run_id.into()));
        self
    }

    /// Reap one exact tmux session when the guard drops.
    pub fn owns_session(&self, name: impl Into<String>) -> &Self {
        self.owned_sessions
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .push(OwnedTmuxSession::Named(name.into()));
        self
    }

    /// Reap this whole process group when the guard drops.
    pub fn owns_process_group(&self, pgid: u32) -> &Self {
        self.owned_groups
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .push(pgid);
        self
    }
}

impl Drop for LiveSessionGuard {
    fn drop(&mut self) {
        let owned_sessions = std::mem::take(
            &mut *self
                .owned_sessions
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner),
        );
        let groups = std::mem::take(
            &mut *self
                .owned_groups
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner),
        );
        for pgid in groups {
            reap_process_group_blocking(pgid);
        }
        let sessions = tmux_session_names();
        for owned in owned_sessions {
            match owned {
                OwnedTmuxSession::All => {
                    for session in sessions
                        .iter()
                        .filter(|name| !name.starts_with("orgasmic-test-keepalive-"))
                    {
                        kill_tmux_session(session);
                    }
                }
                OwnedTmuxSession::Run(run_id) => {
                    let prefix = crate::modes::tmux::tmux_session_prefix(&run_id);
                    for session in sessions.iter().filter(|name| name.starts_with(&prefix)) {
                        kill_tmux_session(session);
                    }
                }
                OwnedTmuxSession::Named(name) => kill_tmux_session(&name),
            }
        }
        let _ = fs2::FileExt::unlock(&self.lock);
    }
}

#[must_use]
pub fn tmux_session_names() -> Vec<String> {
    crate::modes::tmux::tmux_command()
        .args(["list-sessions", "-F", "#{session_name}"])
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .output()
        .ok()
        .filter(|output| output.status.success())
        .map(|output| {
            String::from_utf8_lossy(&output.stdout)
                .lines()
                .map(str::trim)
                .filter(|line| !line.is_empty())
                .map(ToOwned::to_owned)
                .collect()
        })
        .unwrap_or_default()
}

fn kill_tmux_session(session: &str) {
    let _ = crate::modes::tmux::tmux_command()
        .args(["kill-session", "-t", session])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status();
}

const PROCESS_GROUP_REAP_GRACE: Duration = Duration::from_millis(250);

#[must_use = "the group is reaped when this guard drops; binding it to `_` reaps immediately"]
pub struct OwnedProcessGroup {
    pgid: Option<u32>,
}

#[must_use = "the group is reaped when this guard drops; binding it to `_` reaps immediately"]
pub fn owned_process_group(pgid: u32) -> OwnedProcessGroup {
    OwnedProcessGroup { pgid: Some(pgid) }
}

impl OwnedProcessGroup {
    #[must_use]
    pub fn pgid(&self) -> Option<u32> {
        self.pgid
    }

    pub fn reap(&mut self) {
        if let Some(pgid) = self.pgid.take() {
            reap_process_group_blocking(pgid);
        }
    }
}

impl Drop for OwnedProcessGroup {
    fn drop(&mut self) {
        self.reap();
    }
}

#[cfg(unix)]
fn reap_process_group_blocking(pgid: u32) {
    const SIGTERM: i32 = 15;
    const SIGKILL: i32 = 9;

    let Ok(pgid) = i32::try_from(pgid) else {
        return;
    };
    if pgid <= 1 {
        emit_visible_notice(&format!(
            "owned-process-group guard: refusing to signal group {pgid}; \
             a fixture process tree may have leaked"
        ));
        return;
    }

    unsafe {
        libc::kill(-pgid, SIGTERM);
    }
    let deadline = std::time::Instant::now() + PROCESS_GROUP_REAP_GRACE;
    while std::time::Instant::now() < deadline {
        if unsafe { libc::kill(-pgid, 0) } != 0 {
            return;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    unsafe {
        libc::kill(-pgid, SIGKILL);
    }
}

#[cfg(not(unix))]
fn reap_process_group_blocking(_pgid: u32) {}

fn emit_visible_notice(message: &str) {
    #[cfg(unix)]
    {
        if let Ok(mut stderr) = std::fs::OpenOptions::new().write(true).open("/dev/stderr") {
            let _ = writeln!(stderr, "{message}");
            return;
        }
    }
    eprintln!("{message}");
}

#[cfg(test)]
mod tests {
    use super::*;

    // orgasmic:TASK-RRT4T
    /// A missing tool must fail the sentinel loudly, naming the tool and the
    /// opt-out, and the opt-out must be the only thing that turns it back
    /// into a skip.
    #[test]
    fn missing_tool_panics_unless_explicitly_allowed() {
        let _environment = test_environment_lock().blocking_lock();
        const TOOL: &str = "orgasmic-test-tool-that-does-not-exist";
        let requirement = [ToolRequirement::new(TOOL, 3, false)];

        let previous = std::env::var_os(ALLOW_MISSING_TOOLS_ENV);
        std::env::remove_var(ALLOW_MISSING_TOOLS_ENV);
        let panic = std::panic::catch_unwind(|| assert_required_test_tooling(&requirement))
            .expect_err("missing tool without an opt-out must panic");
        let message = panic
            .downcast_ref::<String>()
            .cloned()
            .expect("panic payload is the formatted message");
        assert!(message.contains(TOOL), "message names the tool: {message}");
        assert!(message.contains("gates 3 tests"), "{message}");
        assert!(
            message.contains(&format!("{ALLOW_MISSING_TOOLS_ENV}={TOOL}")),
            "message names the exact opt-out: {message}"
        );

        std::env::set_var(ALLOW_MISSING_TOOLS_ENV, format!(" other , {TOOL}"));
        assert_required_test_tooling(&requirement);

        match previous {
            Some(value) => std::env::set_var(ALLOW_MISSING_TOOLS_ENV, value),
            None => std::env::remove_var(ALLOW_MISSING_TOOLS_ENV),
        }
    }
}
