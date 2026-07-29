// orgasmic:arch_A53QX, arch_C87Z9, arch_MK2Q2, arch_V4DKF, dec_PC7M0
//! Version-locked rmux terminal driver.
//!
//! Detached sessions are driven through the typed [`rmux_sdk`] facade while
//! relying on a separately provisioned, exact-version `rmux` CLI/daemon. The
//! driver discovers that binary via `RMUX_SDK_DAEMON_BINARY` or PATH, verifies
//! it matches the SDK release, and keeps that check distinct from the wrapped
//! harness binary check.
//!
//! ### Lifecycle over the typed SDK (TASK-AFE5Q)
//!
//! Supported TUI harnesses receive the compiled prompt as an initial-prompt
//! argv element; hermes/custom keep paste fallback. The driver does **not**
//! scrape render/scrollback for transcript or completion — live viewing stays
//! on the PTY-attach WebSocket. Pane/process exit (stream end) is a terminal
//! signal only; finalize remains the success authority.
//!
//! ### Availability contract
//!
//! rmux remains an opt-in transport and does not replace the tmux default.
//! When its binary is missing or incompatible, its SDK cannot reach the
//! daemon, or a capability is unavailable, the driver records the exact reason
//! in the `Ready` capabilities payload and degrades to inert mode instead of
//! faking a working integration. On macOS, Cursor additionally requires a
//! successful disposable Keychain preflight through the launchd-owned daemon.
//!
//! ### Token hygiene
//!
//! Web Share operator URLs and pairing tokens grant live shell access. The
//! driver only ever surfaces **redacted** operator material in events/logs and
//! never persists tokens. Spectator URLs are read-only and may be surfaced in
//! full. See [`RmuxWebShareProof`].

use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::process::{Command as StdCommand, Stdio};
use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc,
};
use std::time::Duration;

use async_trait::async_trait;
use serde::Deserialize;
use serde_json::{json, Value};
use tokio::sync::mpsc;
use tokio::task::JoinHandle;

use orgasmic_core::{DriverEvent, RuntimeIdentity};

use crate::catalog::TransportInteraction;
use crate::modes::tmux::{
    accept_cursor_workspace_trust_with_capture, argv_prompt_delivery_applies,
    cancel_and_join_driver_task, claude_native_runtime, claude_session_id,
    cursor_argv_needs_startup_trust, default_input_ready_timeout, deserialize_duration_secs,
    harness_launch_env, is_dispatch_placeholder, pane_has_input_prompt, pane_requests_folder_trust,
    push_initial_prompt_argv, SendChildOwner,
};

use crate::r#trait::{
    preflight_via_adapter, AttachOutcome, Attached, BabysitterAck, BabysitterRequest, DriverConfig,
    DriverContext, DriverControl, DriverError, DriverSession, HarnessEventAdapter,
    NativeRuntimeMeta, PreflightOutcome, RunKind, TransitionAck, TransitionRequest, UserInputAck,
    UserInputRequest, WorkerDriver,
};

const MODE: &str = "rmux";

/// Default mode binary name probed on PATH when `RMUX_SDK_DAEMON_BINARY` is
/// unset. Matches the crate published as `rmux` on crates.io.
const RMUX_BINARY: &str = "rmux";

/// Exact external RMUX release paired with the SDK dependency. RMUX's
/// detached RPC protocol does not promise cross-release compatibility, so the
/// binary probe fails closed instead of allowing Cargo's semver range and the
/// host CLI to drift independently.
pub const RMUX_REQUIRED_VERSION: &str = "0.9.0";

/// Environment variable the rmux SDK uses to locate the daemon binary it spawns
/// on `connect_or_start`. Mirrored here so the driver's separate binary check
/// honors the same override the SDK would.
const RMUX_SDK_DAEMON_BINARY_ENV: &str = "RMUX_SDK_DAEMON_BINARY";

/// The supervisor gives the whole driver release five seconds. Keep both reap
/// attempts strictly inside that caller budget so a stalled SDK transport
/// cannot consume the CLI fallback's opportunity to run.
const RMUX_SESSION_SDK_REAP_TIMEOUT: Duration = Duration::from_secs(2);
const RMUX_SESSION_CLI_REAP_TIMEOUT: Duration = Duration::from_secs(2);

/// How often pane output is coalesced into one [`DriverEvent::PaneActivity`]
/// (TASK-RWCRN). A working TUI writes bytes continuously, so this only has to
/// be short enough that the supervisor's 600 s `DEFAULT_STALL_TIMEOUT` cannot
/// expire between two events — 30 s matches the acp-stdio heartbeat cadence and
/// leaves 20x headroom. It must stay long enough that a chatty pane cannot
/// re-create the JSONL bloat `dec_WDR5K` item 7 removed: at 30 s a four-hour
/// run adds at most 480 content-free lines.
const PANE_ACTIVITY_INTERVAL: Duration = Duration::from_secs(30);

pub struct RmuxDriver {
    adapter: Box<dyn HarnessEventAdapter>,
}

impl RmuxDriver {
    pub fn new(adapter: Box<dyn HarnessEventAdapter>) -> Self {
        Self { adapter }
    }
}

#[derive(Debug, Clone, Deserialize, Default)]
struct RmuxConfig {
    /// Command to run inside the detached session. Defaults to a bounded
    /// harness smoke command when unset.
    #[serde(default)]
    command: Option<String>,
    #[serde(default)]
    args: Vec<String>,
    #[serde(default)]
    cwd: Option<PathBuf>,
    #[serde(default)]
    harness: Option<String>,
    #[serde(default)]
    model: Option<String>,
    #[serde(default)]
    effort: Option<String>,
    #[serde(default)]
    reasoning_effort: Option<String>,
    /// Extra argv appended verbatim to the harness CLI (worker
    /// `:HARNESS_ARGS:` / launch request). Appended before the guarded flag
    /// pushes below, so an explicit `--model` here wins over `model`.
    #[serde(default)]
    harness_args: Vec<String>,
    /// Compiled dispatch prompt. Supported TUI harnesses receive it as an
    /// initial-prompt argv element; hermes/custom keep paste delivery.
    #[serde(default)]
    prompt_bundle_text: Option<String>,
    /// How long to wait for the wrapped TUI's input prompt before pasting the
    /// dispatch prompt anyway. Mirrors the tmux driver knob.
    #[serde(
        default = "default_input_ready_timeout",
        deserialize_with = "deserialize_duration_secs"
    )]
    input_ready_timeout: Duration,
    /// Force inert mode even when an rmux binary is present. Test-only knob.
    #[serde(default)]
    force_inert: bool,
    /// Attempt a Web Share smoke (spectator + operator URL mint) once the
    /// session is live. Off by default so plain session smokes do not require
    /// the web feature/tunnel wiring.
    #[serde(default)]
    web_share: bool,
    /// Historical knob (render vs line stream). Ignored after TASK-AFE5Q —
    /// drivers no longer capture pane output as transcript/completion truth.
    #[serde(default)]
    #[allow(dead_code)]
    force_render: Option<bool>,
    /// Spawn the session "system-wide": detached from the orgasmic daemon so it
    /// survives a daemon restart/rebuild. The rmux SDK already starts its daemon
    /// in its own session (`setsid`) on a stable per-user socket, so the session
    /// itself outlives us; this flag additionally suppresses the
    /// kill-session-on-drop backstop so a graceful daemon shutdown can never
    /// reap the session. Explicit `release` (operator stop) still tears it down.
    /// Defaults ON for the manager (set by the UI), OFF otherwise.
    #[serde(default)]
    system_wide: bool,
}

/// Result of the separate rmux-binary discovery (kept distinct from the
/// harness binary so the catalog can report each independently).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RmuxBinaryProbe {
    /// Whether an rmux executable was found, independent of its version.
    pub found: bool,
    /// Whether the executable reports the exact SDK-paired release.
    pub compatible: bool,
    /// Resolved path or binary name, when found.
    pub path: Option<String>,
    /// Where the binary was resolved from: `env` or `path`.
    pub source: Option<&'static str>,
    /// Parsed release reported by `rmux -V`, when available.
    pub version: Option<String>,
    /// Bounded diagnostic when the version could not be read or did not match.
    pub version_error: Option<String>,
}

impl RmuxBinaryProbe {
    fn missing() -> Self {
        Self {
            found: false,
            compatible: false,
            path: None,
            source: None,
            version: None,
            version_error: None,
        }
    }

    /// Whether this binary can safely speak to the compiled SDK.
    #[must_use]
    pub const fn usable(&self) -> bool {
        self.found && self.compatible
    }
}

/// Discover the rmux daemon binary the SDK would spawn, **separately** from any
/// wrapped harness binary. Honors `RMUX_SDK_DAEMON_BINARY` first (the same
/// override `rmux_sdk` consults), then falls back to a PATH probe for `rmux`.
pub fn probe_rmux_binary() -> RmuxBinaryProbe {
    if let Some(explicit) = std::env::var_os(RMUX_SDK_DAEMON_BINARY_ENV) {
        let path = PathBuf::from(&explicit);
        // An explicit override may be an absolute path or a bare name resolved
        // on PATH. Treat an existing file as found; otherwise still report the
        // configured value so the catalog/event shows what was attempted.
        let found = path.is_file() || binary_on_path(&path.to_string_lossy());
        return inspect_rmux_binary(path.to_string_lossy().into_owned(), Some("env"), found);
    }
    if let Some(resolved) = which_on_path(RMUX_BINARY) {
        return inspect_rmux_binary(resolved, Some("path"), true);
    }
    RmuxBinaryProbe::missing()
}

fn inspect_rmux_binary(path: String, source: Option<&'static str>, found: bool) -> RmuxBinaryProbe {
    if !found {
        return RmuxBinaryProbe {
            found: false,
            compatible: false,
            path: Some(path),
            source,
            version: None,
            version_error: None,
        };
    }

    let output = StdCommand::new(&path)
        .arg("-V")
        .stdin(Stdio::null())
        .output();
    let (version, version_error) = match output {
        Ok(output) if output.status.success() => {
            let stdout = String::from_utf8_lossy(&output.stdout);
            match parse_rmux_version(&stdout) {
                Some(version) if version == RMUX_REQUIRED_VERSION => (Some(version), None),
                Some(version) => {
                    let error = format!(
                        "rmux version mismatch: expected {RMUX_REQUIRED_VERSION}, found {version}"
                    );
                    (Some(version), Some(error))
                }
                None => (
                    None,
                    Some(format!(
                        "rmux version unreadable: expected `rmux {RMUX_REQUIRED_VERSION}`"
                    )),
                ),
            }
        }
        Ok(output) => (None, Some(format!("rmux -V exited with {}", output.status))),
        Err(error) => (None, Some(format!("rmux -V failed: {error}"))),
    };
    let compatible = version_error.is_none();
    RmuxBinaryProbe {
        found: true,
        compatible,
        path: Some(path),
        source,
        version,
        version_error,
    }
}

fn parse_rmux_version(output: &str) -> Option<String> {
    let mut words = output.split_whitespace();
    (words.next()? == "rmux")
        .then(|| words.next().map(ToOwned::to_owned))
        .flatten()
}

fn which_on_path(binary: &str) -> Option<String> {
    let paths = std::env::var_os("PATH")?;
    std::env::split_paths(&paths)
        .map(|dir| dir.join(binary))
        .find(|candidate| candidate.is_file())
        .map(|p| p.to_string_lossy().into_owned())
}

fn binary_on_path(binary: &str) -> bool {
    if Path::new(binary).is_absolute() {
        return Path::new(binary).is_file();
    }
    which_on_path(binary).is_some()
}

// orgasmic:task_RRT4T
/// Shared support for tests whose assertions require host tooling.
///
/// Integration-test crates cannot import `#[cfg(test)]` library modules, so
/// this deliberately lives in the normal library behind a doc-hidden module.
/// Every tooling guard routes through this module, and every affected test
/// binary has one `required_test_tooling_is_present` sentinel that reports the
/// number of tests gated by each tool.
#[doc(hidden)]
pub mod test_tooling {
    use std::collections::BTreeSet;
    use std::ffi::OsString;
    use std::io::Write as _;
    use std::process::{Command, Stdio};
    use std::sync::Mutex;
    use std::time::Duration;

    pub const ALLOW_MISSING_TOOLS_ENV: &str = "ORGASMIC_ALLOW_MISSING_TOOLS";

    // orgasmic:task_MFJZ7
    /// Arms tests that submit a real, billed provider turn.
    ///
    /// This is the opposite direction from [`ALLOW_MISSING_TOOLS_ENV`], and the
    /// distinction is the whole point of having two names. That one *waives* a
    /// missing binary so a run that skipped work can still go green;
    /// `ORGASMIC_ALLOW_BILLED_TESTS` *arms* work that is off by default because
    /// running it costs money. It is not the "this test must really run"
    /// fail-closed lane that `ORGASMIC_REQUIRE_LIVE_RMUX` was and that
    /// `.orgasmic/gotchas.org` forbids reintroducing: nothing fails when it is
    /// unset, which is exactly the safe default a billed test needs.
    ///
    /// A billed test carries two locks, not one. `#[ignore]` keeps it out of a
    /// bare `cargo test`; this variable keeps `--include-ignored` from arming
    /// it. `--include-ignored` is the flag someone reaches for to run "the slow
    /// ones", and it must not silently also mean "and charge me".
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
        super::which_on_path(tool).is_some()
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

    // orgasmic:task_MFJZ7
    /// Whether [`ALLOW_BILLED_TESTS_ENV`] arms billed tests for this process.
    ///
    /// Exactly `1`. Anything else — including `true`, `yes`, or an empty
    /// value — leaves billing disarmed, so a stray export cannot half-mean it.
    #[must_use]
    pub fn billed_tests_allowed() -> bool {
        std::env::var(ALLOW_BILLED_TESTS_ENV).is_ok_and(|value| value.trim() == "1")
    }

    // orgasmic:task_MFJZ7
    /// The second lock on a billed test: returns `true` (skip) unless the
    /// operator armed [`ALLOW_BILLED_TESTS_ENV`], and says so where a default
    /// `cargo test` run can see it.
    ///
    /// The first lock is `#[ignore]` on the test itself. Both are required
    /// because they close different doors: `#[ignore]` closes a bare run, this
    /// closes `--include-ignored`.
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

    // orgasmic:task_MFJZ7
    /// Name, on every default run, the tests this binary withholds because they
    /// bill.
    ///
    /// [`assert_required_test_tooling`] counts tests gated by *missing tooling*.
    /// A billed test is gated by policy on a host where the tooling is present,
    /// so it belongs to none of those counts — and would therefore vanish from
    /// the default output entirely, which is the accounting hole this exists to
    /// close. Sentinels call it alongside their tool requirements.
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

    /// Preserve the useful per-test diagnostic for `--nocapture` while the
    /// binary-level sentinel supplies the default-output failure signal.
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

    /// A live-driver test that degrades to inert has found a defect, not a
    /// reason to skip its assertions.
    pub fn assert_not_degraded(test_name: &str, degraded: bool) {
        assert!(
            !degraded,
            "{test_name}: live driver degraded to inert instead of exercising its assertions"
        );
    }

    /// Fail one clearly named sentinel test per binary when any required tool
    /// is absent. A comma-separated, per-tool environment opt-out keeps the
    /// suite runnable on constrained hosts without allowing one missing tool
    /// to hide another.
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
            let details = format_requirements(&allowed_missing);
            emit_visible_notice(&format!(
                "warning: {ALLOW_MISSING_TOOLS_ENV} explicitly allows missing test tooling: \
                 {details}; those gated tests did not run"
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

    // orgasmic:task_Z3093
    /// Shared temp path of the advisory flock that serializes live mux tests
    /// across every test binary in the workspace.
    const LIVE_SESSION_LOCK_FILE: &str = "orgasmic-live-session-tests.lock";

    /// Upper bound on the whole `Drop`-time reap. `rmux kill-session` is a
    /// local RPC that answers in milliseconds; the bound exists only so a
    /// wedged rmux daemon degrades to a warning instead of hanging the test
    /// binary forever.
    const REAP_TIMEOUT: Duration = Duration::from_secs(10);

    /// An rmux session a [`LiveSessionGuard`] must reap when it drops.
    enum OwnedSession {
        /// Every session named `orgasmic-rmux-<run_id>-*`. Tests know the run
        /// id but usually not the runtime id, which the supervisor mints.
        Run {
            endpoint: rmux_sdk::RmuxEndpoint,
            run_id: String,
        },
        /// One exact session name.
        Named {
            endpoint: rmux_sdk::RmuxEndpoint,
            name: String,
        },
    }

    /// Serialize real-tmux/rmux tests across ALL test binaries, and reap the
    /// rmux sessions the holding test created.
    ///
    /// The flock half (TASK-X0ZVE) is unchanged: live tests spawn real mux
    /// daemons and contend under `cargo test --workspace`, so an advisory lock
    /// on a shared temp path lets at most one run at a time, cross-process.
    /// Held for the whole test via the returned guard.
    ///
    /// The reap half (TASK-Z3093) exists because session cleanup used to be a
    /// *trailing statement* in the test body. Any panic above it — including
    /// the load-induced failures TASK-STWVB records — skipped the reap, and the
    /// session, its pty and its harness process outlived the test binary (the
    /// `artifact-ready` fixture never exits on its own). `Drop` runs on the
    /// panic path, so registering a session with the guard makes cleanup
    /// unconditional.
    ///
    /// Registration is opt-in per test: a guard with nothing registered behaves
    /// exactly as it did before, which is what the many heavy-but-sessionless
    /// callers (git, daemon-boot, tmux) want.
    ///
    /// ```ignore
    /// let live = live_session_guard();
    /// let resp = post_artifact_generate(..).await.unwrap();
    /// live.owns(&resp.run_id); // reaped even if the next assert panics
    /// ```
    #[must_use]
    pub fn live_session_guard() -> LiveSessionGuard {
        let path = std::env::temp_dir().join(LIVE_SESSION_LOCK_FILE);
        let lock = std::fs::OpenOptions::new()
            .create(true)
            .truncate(false)
            .write(true)
            .open(&path)
            .expect("open live-session lock file");
        // MSRV 1.87: call fs2 explicitly — std's File::lock_exclusive (1.89) shadows it.
        fs2::FileExt::lock_exclusive(&lock).expect("flock live-session lock");
        LiveSessionGuard {
            lock,
            owned: Mutex::new(Vec::new()),
            owned_groups: Mutex::new(Vec::new()),
        }
    }

    /// RAII drop-guard that reaps the rmux sessions registered on it and then
    /// releases the [`live_session_guard`] advisory flock.
    ///
    /// One definition, in the normal library rather than behind `#[cfg(test)]`,
    /// because an integration-test binary cannot import a `#[cfg(test)]` item
    /// from a lib — and half the live tests live in `tests/`.
    pub struct LiveSessionGuard {
        lock: std::fs::File,
        /// Interior mutability so `let live = live_session_guard();` needs no
        /// `mut`: registration is a fact about the test, not a mutation the
        /// ~85 existing call sites should have to declare.
        owned: Mutex<Vec<OwnedSession>>,
        // orgasmic:task_BCYMM
        /// Process groups spawned by the holding test, reaped with the
        /// sessions and before the flock releases.
        owned_groups: Mutex<Vec<u32>>,
    }

    impl LiveSessionGuard {
        /// [`Self::owns`] as a consuming builder, for the common case where the
        /// run id is a literal the test already knows at acquisition:
        /// `let _live = live_session_guard().owning("run-attach");`
        #[must_use]
        pub fn owning(self, run_id: impl Into<String>) -> Self {
            self.owns(run_id);
            self
        }

        /// Reap every `orgasmic-rmux-<run_id>-*` session on the default rmux
        /// endpoint when this guard drops. Register as soon as the run id is
        /// known — before the first assertion that can panic.
        pub fn owns(&self, run_id: impl Into<String>) -> &Self {
            self.push(OwnedSession::Run {
                endpoint: rmux_sdk::RmuxEndpoint::Default,
                run_id: run_id.into(),
            })
        }

        /// Reap this exact session name on the default rmux endpoint.
        pub fn owns_session(&self, name: impl Into<String>) -> &Self {
            self.push(OwnedSession::Named {
                endpoint: rmux_sdk::RmuxEndpoint::Default,
                name: name.into(),
            })
        }

        /// Reap this exact session name at a specific endpoint — for a test
        /// that drove the SDK itself and can hand over `session.endpoint()`.
        pub fn owns_session_at(
            &self,
            endpoint: rmux_sdk::RmuxEndpoint,
            name: impl Into<String>,
        ) -> &Self {
            self.push(OwnedSession::Named {
                endpoint,
                name: name.into(),
            })
        }

        // orgasmic:task_BCYMM
        /// Reap this whole process group when the guard drops — the process
        /// half of [`Self::owns`], for a test that holds the flock *and*
        /// spawns a fixture tree.
        ///
        /// `pgid` must be a group this test created (spawn with
        /// `process_group(0)`, so the child's pid is its group id). A test that
        /// does not otherwise need the flock should use the standalone
        /// [`owned_process_group`] instead of taking the lock for this.
        pub fn owns_process_group(&self, pgid: u32) -> &Self {
            self.owned_groups
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .push(pgid);
            self
        }

        fn push(&self, entry: OwnedSession) -> &Self {
            self.owned
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .push(entry);
            self
        }
    }

    impl Drop for LiveSessionGuard {
        fn drop(&mut self) {
            // Reap before unlocking: the next test binary blocked on the flock
            // must not inherit this test's session — or its process tree.
            let owned = std::mem::take(
                &mut *self
                    .owned
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
            reap_owned_sessions(owned);
            let _ = fs2::FileExt::unlock(&self.lock);
        }
    }

    // orgasmic:task_BCYMM
    /// Grace between the group TERM and the group KILL.
    ///
    /// Much shorter than the production `GROUP_REAP_GRACE` (2s) because the
    /// members are test fixtures — shells and `sleep`s with no flush-on-exit
    /// work to do — and this cost is paid on every fixture teardown in the
    /// suite. The unconditional KILL below makes the window a courtesy, not a
    /// correctness bound.
    const PROCESS_GROUP_REAP_GRACE: Duration = Duration::from_millis(250);

    // orgasmic:task_BCYMM
    /// RAII drop-guard that reaps one spawned process GROUP.
    ///
    /// The process sibling of [`LiveSessionGuard`], and it exists separately
    /// because most fixture-spawning tests are not live-mux tests: making them
    /// take the workspace-wide flock just to get a reap would serialize them
    /// against every live test in every crate.
    ///
    /// Reaping the *group* rather than the pid is the whole point. Fixture
    /// arms background their children (`/bin/sleep 300 &`), so signalling only
    /// the foreground process orphans those children to init, where they
    /// outlive the test binary — the exact signature TASK-BCYMM measured.
    #[must_use = "the group is reaped when this guard drops; binding it to `_` reaps immediately"]
    pub struct OwnedProcessGroup {
        pgid: Option<u32>,
    }

    // orgasmic:task_BCYMM
    /// Reap process group `pgid` when the returned guard drops.
    ///
    /// Register the group as soon as the child is spawned — before the first
    /// assertion that can panic — exactly as [`LiveSessionGuard::owns`] wants
    /// its run id.
    #[must_use = "the group is reaped when this guard drops; binding it to `_` reaps immediately"]
    pub fn owned_process_group(pgid: u32) -> OwnedProcessGroup {
        OwnedProcessGroup { pgid: Some(pgid) }
    }

    impl OwnedProcessGroup {
        /// The group this guard will reap, or `None` once it has been reaped.
        #[must_use]
        pub fn pgid(&self) -> Option<u32> {
            self.pgid
        }

        /// Reap now and disarm, so a caller that must observe the post-reap
        /// state (or reap in a specific order relative to its own `wait`) can
        /// do so without waiting for `Drop`. Idempotent.
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

    // orgasmic:task_BCYMM
    /// TERM the whole group, give it a short grace, then KILL whatever is left.
    ///
    /// The same idiom production release already uses
    /// (`subprocess_stream_json::reap_process_group`, TASK-104.3 / TASK-J1XCB /
    /// TASK-HAREX); this is its blocking `Drop`-callable twin, because `Drop`
    /// cannot `.await` (see [`reap_owned_sessions`] for why neither async
    /// escape works here).
    ///
    /// `pgid <= 1` is refused rather than signalled: `kill(-1, …)` is a
    /// broadcast to every process this user may signal, and `kill(0, …)`
    /// targets *our own* group — the test binary. A test that recorded a bad
    /// pgid must leak, not take the machine down with it.
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
            // `kill(-pgid, 0)` succeeds while any member — including a
            // not-yet-waited zombie leader — still exists. Callers that own the
            // leader's `Child` wait it themselves; here an early exit is a
            // bonus, and the unconditional KILL below is the guarantee.
            if unsafe { libc::kill(-pgid, 0) } != 0 {
                return;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        unsafe {
            libc::kill(-pgid, SIGKILL);
        }
    }

    /// Non-unix fallback: no process groups to reap.
    #[cfg(not(unix))]
    fn reap_process_group_blocking(_pgid: u32) {}

    /// `Drop` cannot `.await`, and both async escapes are wrong here:
    ///
    /// * `tokio::spawn` is a *silent* no-op on the path that matters. A
    ///   `#[tokio::test]` drops its runtime the moment the test body unwinds,
    ///   which is the same moment the guard drops, so a freshly spawned task is
    ///   cancelled before its first poll — a reap that looks fixed and leaks
    ///   anyway.
    /// * `Handle::block_on` panics when called from a runtime worker thread,
    ///   which is exactly where a `#[tokio::test]` body drops the guard.
    ///
    /// So the reap is plain blocking `std::process::Command` work. It runs on a
    /// dedicated std thread purely so the join can be bounded; no runtime is
    /// involved, which is also why the same guard works in the sync `#[test]`
    /// and `tests/` integration binaries.
    fn reap_owned_sessions(owned: Vec<OwnedSession>) {
        if owned.is_empty() {
            return;
        }
        let count = owned.len();
        let probe = super::probe_rmux_binary();
        let Some(rmux_bin) = probe.path.filter(|_| probe.found) else {
            emit_visible_notice(&format!(
                "live-session guard: no rmux binary available; cannot reap {count} owned session(s)"
            ));
            return;
        };
        let (tx, rx) = std::sync::mpsc::channel();
        let spawned = std::thread::Builder::new()
            .name("live-session-reap".into())
            .spawn(move || {
                let _ = tx.send(reap_owned_sessions_blocking(&rmux_bin, &owned));
            });
        match spawned {
            // The handle is intentionally dropped rather than joined: the
            // bounded `recv_timeout` below is the join, so a wedged rmux
            // daemon cannot stall the test binary's teardown.
            Ok(_detached) => match rx.recv_timeout(REAP_TIMEOUT) {
                Ok(Ok(())) => {}
                Ok(Err(error)) => emit_visible_notice(&format!(
                    "live-session guard: reap failed, session may have leaked: {error}"
                )),
                Err(_) => emit_visible_notice(&format!(
                    "live-session guard: reap of {count} owned session(s) did not finish within \
                     {REAP_TIMEOUT:?}; a session may have leaked"
                )),
            },
            Err(error) => emit_visible_notice(&format!(
                "live-session guard: could not spawn reap thread: {error}"
            )),
        }
    }

    /// Names of every session live on the default rmux endpoint, or an empty
    /// list if rmux is absent or has no daemon. For tests that must prove the
    /// production path reaped a session whose run id they never observed.
    #[must_use]
    pub fn rmux_session_names() -> Vec<String> {
        let probe = super::probe_rmux_binary();
        let Some(rmux_bin) = probe.path.filter(|_| probe.found) else {
            return Vec::new();
        };
        list_sessions_blocking(&rmux_bin, &rmux_sdk::RmuxEndpoint::Default).unwrap_or_default()
    }

    fn reap_owned_sessions_blocking(rmux_bin: &str, owned: &[OwnedSession]) -> Result<(), String> {
        let mut errors = Vec::new();
        for entry in owned {
            let (endpoint, targets) = match entry {
                OwnedSession::Named { endpoint, name } => (endpoint, vec![name.clone()]),
                OwnedSession::Run { endpoint, run_id } => {
                    let prefix = format!("orgasmic-rmux-{run_id}-");
                    match list_sessions_blocking(rmux_bin, endpoint) {
                        Ok(names) => (
                            endpoint,
                            names
                                .into_iter()
                                .filter(|name| name.starts_with(&prefix))
                                .collect::<Vec<_>>(),
                        ),
                        // No daemon, or no sessions at all: by construction
                        // nothing this guard owns is still alive. Never widen
                        // this to "kill anything that looks like ours" — a real
                        // dispatch's session can be live on the same endpoint.
                        Err(_) => continue,
                    }
                }
            };
            for target in targets {
                if let Err(error) = kill_session_blocking(rmux_bin, endpoint, &target) {
                    errors.push(error);
                }
            }
        }
        if errors.is_empty() {
            Ok(())
        } else {
            Err(errors.join("; "))
        }
    }

    fn list_sessions_blocking(
        rmux_bin: &str,
        endpoint: &rmux_sdk::RmuxEndpoint,
    ) -> Result<Vec<String>, String> {
        let mut args = super::rmux_endpoint_args(endpoint).map_err(|error| error.to_string())?;
        args.extend([
            OsString::from("list-sessions"),
            OsString::from("-F"),
            OsString::from("#{session_name}"),
        ]);
        let output = Command::new(rmux_bin)
            .args(&args)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .output()
            .map_err(|error| format!("rmux list-sessions: {error}"))?;
        if !output.status.success() {
            return Err(format!(
                "rmux list-sessions failed: {}",
                String::from_utf8_lossy(&output.stderr).trim()
            ));
        }
        Ok(String::from_utf8_lossy(&output.stdout)
            .lines()
            .map(str::trim)
            .filter(|line| !line.is_empty())
            .map(ToOwned::to_owned)
            .collect())
    }

    /// Kill one session, addressed at the endpoint the caller owned. Reuses the
    /// production CLI-fallback arg shape (TASK-6FNAY) rather than a bare
    /// `rmux kill-session` against whatever daemon the CLI would resolve.
    ///
    /// A failing `kill-session` is not by itself an error: the session may
    /// already be gone because the test's own trailing `release` succeeded. The
    /// authority is `has-session` afterwards.
    fn kill_session_blocking(
        rmux_bin: &str,
        endpoint: &rmux_sdk::RmuxEndpoint,
        name: &str,
    ) -> Result<(), String> {
        let session_name = rmux_sdk::SessionName::new(name.to_string())
            .map_err(|error| format!("rmux session name {name}: {error}"))?;
        let kill_args =
            super::rmux_session_reap_args(endpoint, &session_name).map_err(|e| e.to_string())?;
        let _ = Command::new(rmux_bin)
            .args(&kill_args)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();
        let mut probe_args = super::rmux_endpoint_args(endpoint).map_err(|e| e.to_string())?;
        probe_args.extend([
            OsString::from("has-session"),
            OsString::from("-t"),
            OsString::from(name),
        ]);
        let still_alive = Command::new(rmux_bin)
            .args(&probe_args)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .is_ok_and(|status| status.success());
        if still_alive {
            return Err(format!("rmux session {name} survived kill-session"));
        }
        Ok(())
    }

    // orgasmic:task_69CW6
    /// A private rmux endpoint whose SDK transport can be frozen mid-run while
    /// freshly opened CLI connections keep working.
    ///
    /// This exists because the reap contract (TASK-6FNAY) is *two* paths with
    /// one identity: `Session::kill` over the SDK's single ordered transport,
    /// and — only when that stalls — an endpoint-exact `rmux -S <endpoint>
    /// kill-session` fallback. A double that simulates the stall proves
    /// nothing; the stall has to happen to a real `rmux_sdk::Session` addressing
    /// a real daemon, and the fallback has to reap a real session.
    ///
    /// Layout, all inside one temp dir exported as `RMUX_TMPDIR` so nothing
    /// here can touch the developer's own daemon:
    ///
    /// ```text
    ///   <root>/rmux-<uid>/default    <- proxy listener (the resolved endpoint)
    ///   <root>/rmux-<uid>/upstream   <- the real rmux daemon
    /// ```
    ///
    /// [`Self::stall_sdk_transport`] freezes every connection that was already
    /// open — which is exactly the SDK's — and leaves connections opened
    /// afterwards untouched, which is exactly the CLI fallback's. That
    /// asymmetry is the whole fixture: it is what makes "SDK stalled, fallback
    /// reached, session actually gone" observable instead of asserted.
    ///
    /// `RMUX_SDK_DAEMON_BINARY` points at a shim that records every `rmux`
    /// argv the driver runs before exec'ing the real binary, so the fallback's
    /// endpoint-exact argv is evidence rather than inference.
    #[cfg(unix)]
    pub struct StallableRmuxEndpoint {
        root: tempfile::TempDir,
        endpoint_path: std::path::PathBuf,
        upstream_path: std::path::PathBuf,
        argv_dir: std::path::PathBuf,
        rmux_bin: String,
        stalled: std::sync::Arc<std::sync::atomic::AtomicBool>,
        proxy: tokio::task::JoinHandle<()>,
        prior_tmpdir: Option<OsString>,
        prior_daemon_binary: Option<OsString>,
    }

    #[cfg(unix)]
    impl StallableRmuxEndpoint {
        /// Session created purely so the upstream daemon outlives the run under
        /// test; a daemon that exits with its last session would make "the
        /// session is gone" ambiguous.
        const KEEPALIVE_SESSION: &'static str = "orgasmic-stall-fixture-keepalive";

        /// Bring up the isolated daemon, the recording shim and the proxy, and
        /// point this process's rmux discovery at them.
        ///
        /// The caller must hold both [`test_environment_lock`] (this mutates
        /// process environment) and [`live_session_guard`] (this starts a real
        /// daemon) for the whole lifetime of the returned fixture.
        pub async fn start() -> Result<Self, String> {
            use std::os::unix::fs::PermissionsExt as _;

            let probe = super::probe_rmux_binary();
            let rmux_bin = probe
                .path
                .clone()
                .filter(|_| probe.usable())
                .ok_or_else(|| "no usable rmux binary".to_string())?;

            let root = tempfile::TempDir::new().map_err(|e| format!("stall fixture root: {e}"))?;
            // `rmux-ipc` resolves the socket root through the real path, so a
            // `/var/...` temp dir surfaces as `/private/var/...` in
            // `Session::endpoint()`. Canonicalize up front or every
            // endpoint-exact comparison compares two spellings of one socket.
            let root_path = std::fs::canonicalize(root.path())
                .map_err(|e| format!("canonicalize stall fixture root: {e}"))?;
            let socket_dir = root_path.join(format!("rmux-{}", unsafe { libc::getuid() }));
            std::fs::create_dir_all(&socket_dir)
                .and_then(|()| {
                    std::fs::set_permissions(&socket_dir, std::fs::Permissions::from_mode(0o700))
                })
                .map_err(|e| format!("stall fixture socket dir: {e}"))?;
            let endpoint_path = socket_dir.join("default");
            let upstream_path = socket_dir.join("upstream");

            let argv_dir = root_path.join("argv");
            std::fs::create_dir_all(&argv_dir)
                .map_err(|e| format!("stall fixture argv dir: {e}"))?;
            let shim = root_path.join("rmux-shim");
            std::fs::write(
                &shim,
                format!(
                    "#!/bin/sh\n\
                     f=$(mktemp {argv}/argv.XXXXXXXX)\n\
                     for a in \"$@\"; do printf '%s\\n' \"$a\"; done > \"$f\"\n\
                     exec {rmux} \"$@\"\n",
                    argv = argv_dir.display(),
                    rmux = rmux_bin,
                ),
            )
            .and_then(|()| std::fs::set_permissions(&shim, std::fs::Permissions::from_mode(0o755)))
            .map_err(|e| format!("stall fixture shim: {e}"))?;

            let started = Command::new(&rmux_bin)
                .args([
                    OsString::from("-S"),
                    upstream_path.clone().into_os_string(),
                    OsString::from("new-session"),
                    OsString::from("-d"),
                    OsString::from("-s"),
                    OsString::from(Self::KEEPALIVE_SESSION),
                    OsString::from("--"),
                    OsString::from("sh"),
                    OsString::from("-c"),
                    OsString::from("sleep 600"),
                ])
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::piped())
                .output()
                .map_err(|e| format!("start upstream rmux daemon: {e}"))?;
            if !started.status.success() {
                return Err(format!(
                    "start upstream rmux daemon: {}",
                    String::from_utf8_lossy(&started.stderr).trim()
                ));
            }

            let listener = tokio::net::UnixListener::bind(&endpoint_path)
                .map_err(|e| format!("bind stall proxy at {}: {e}", endpoint_path.display()))?;
            let stalled = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
            let proxy = tokio::spawn(run_stall_proxy(
                listener,
                upstream_path.clone(),
                std::sync::Arc::clone(&stalled),
            ));

            // Repoint process-global rmux discovery only once every fallible
            // step has succeeded: nothing above this line needs it (the daemon
            // is addressed with an explicit `-S`), and only the returned value
            // knows how to put it back.
            let prior_tmpdir = std::env::var_os(RMUX_TMPDIR_ENV);
            let prior_daemon_binary = std::env::var_os(super::RMUX_SDK_DAEMON_BINARY_ENV);
            std::env::set_var(RMUX_TMPDIR_ENV, &root_path);
            std::env::set_var(super::RMUX_SDK_DAEMON_BINARY_ENV, &shim);

            Ok(Self {
                root,
                endpoint_path,
                upstream_path,
                argv_dir,
                rmux_bin,
                stalled,
                proxy,
                prior_tmpdir,
                prior_daemon_binary,
            })
        }

        /// The endpoint the driver's SDK resolves, and therefore the one its
        /// CLI fallback must address exactly.
        #[must_use]
        pub fn endpoint_path(&self) -> &std::path::Path {
            &self.endpoint_path
        }

        /// Freeze every already-open connection. The SDK's ordered transport is
        /// open by now, so its next request — the release-time `Session::kill` —
        /// never reaches the daemon and never answers.
        pub fn stall_sdk_transport(&self) {
            self.stalled
                .store(true, std::sync::atomic::Ordering::SeqCst);
        }

        /// Ask the upstream daemon directly, bypassing the proxy, so a stalled
        /// transport cannot make a live session look reaped.
        #[must_use]
        pub fn session_exists(&self, name: &str) -> bool {
            Command::new(&self.rmux_bin)
                .args([
                    OsString::from("-S"),
                    self.upstream_path.clone().into_os_string(),
                    OsString::from("has-session"),
                    OsString::from("-t"),
                    OsString::from(name),
                ])
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .status()
                .is_ok_and(|status| status.success())
        }

        /// Every `rmux` argv the process under test ran through the shim, in no
        /// particular order.
        #[must_use]
        pub fn recorded_cli_invocations(&self) -> Vec<Vec<String>> {
            let Ok(entries) = std::fs::read_dir(&self.argv_dir) else {
                return Vec::new();
            };
            entries
                .filter_map(std::result::Result::ok)
                .filter_map(|entry| std::fs::read_to_string(entry.path()).ok())
                .map(|body| body.lines().map(ToOwned::to_owned).collect())
                .collect()
        }
    }

    #[cfg(unix)]
    impl Drop for StallableRmuxEndpoint {
        fn drop(&mut self) {
            self.proxy.abort();
            let _ = Command::new(&self.rmux_bin)
                .args([
                    OsString::from("-S"),
                    self.upstream_path.clone().into_os_string(),
                    OsString::from("kill-server"),
                ])
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .status();
            match self.prior_tmpdir.take() {
                Some(value) => std::env::set_var(RMUX_TMPDIR_ENV, value),
                None => std::env::remove_var(RMUX_TMPDIR_ENV),
            }
            match self.prior_daemon_binary.take() {
                Some(value) => std::env::set_var(super::RMUX_SDK_DAEMON_BINARY_ENV, value),
                None => std::env::remove_var(super::RMUX_SDK_DAEMON_BINARY_ENV),
            }
            // TempDir removal is best effort: the daemon owns files under it.
            let _ = std::fs::remove_dir_all(self.root.path());
        }
    }

    /// Socket-root override honored by `rmux-ipc`'s endpoint resolution, and
    /// therefore by both the in-process SDK and every spawned `rmux` CLI.
    #[cfg(unix)]
    const RMUX_TMPDIR_ENV: &str = "RMUX_TMPDIR";

    #[cfg(unix)]
    async fn run_stall_proxy(
        listener: tokio::net::UnixListener,
        upstream: std::path::PathBuf,
        stalled: std::sync::Arc<std::sync::atomic::AtomicBool>,
    ) {
        loop {
            let Ok((client, _)) = listener.accept().await else {
                return;
            };
            // A connection opened while stalled belongs to the CLI fallback,
            // which must succeed. Only connections that predate the stall — the
            // SDK's — are freezable.
            let freezable = !stalled.load(std::sync::atomic::Ordering::SeqCst);
            let stalled = std::sync::Arc::clone(&stalled);
            let upstream = upstream.clone();
            tokio::spawn(async move {
                let Ok(server) = tokio::net::UnixStream::connect(&upstream).await else {
                    return;
                };
                let (client_read, client_write) = client.into_split();
                let (server_read, server_write) = server.into_split();
                let freeze = freezable.then(|| std::sync::Arc::clone(&stalled));
                tokio::join!(
                    pump_until_frozen(client_read, server_write, freeze.clone()),
                    pump_until_frozen(server_read, client_write, freeze),
                );
            });
        }
    }

    /// Copy bytes one read at a time, checking the freeze flag *after* the read
    /// and before the write. Checking only before the read would forward the
    /// very request the test wants stalled: the pump is parked in `read` when
    /// the flag flips, and the kill request is what wakes it.
    #[cfg(unix)]
    async fn pump_until_frozen<R, W>(
        mut from: R,
        mut to: W,
        freeze: Option<std::sync::Arc<std::sync::atomic::AtomicBool>>,
    ) where
        R: tokio::io::AsyncRead + Unpin,
        W: tokio::io::AsyncWrite + Unpin,
    {
        use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};

        let mut buf = vec![0u8; 8192];
        loop {
            let Ok(read) = from.read(&mut buf).await else {
                return;
            };
            if read == 0 {
                return;
            }
            if freeze
                .as_ref()
                .is_some_and(|flag| flag.load(std::sync::atomic::Ordering::SeqCst))
            {
                // Hold the socket open and answer nothing, forever: what an
                // unresponsive daemon looks like from inside the SDK.
                std::future::pending::<()>().await;
            }
            if to.write_all(&buf[..read]).await.is_err() {
                return;
            }
        }
    }

    /// libtest captures `eprintln!` from passing tests. Write to the process
    /// stderr device so an explicitly opted-out green run remains visibly
    /// different in default `cargo test` output.
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
}

/// Whether the wrapped harness binary is available. Distinct from the rmux
/// binary probe (acceptance criterion: catalog checks them separately).
fn harness_binary_available(command: &str) -> bool {
    StdCommand::new("which")
        .arg(command)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

/// The reason a smoke run cannot drive a real rmux session, if any.
fn inert_reason(cfg: &RmuxConfig, probe: &RmuxBinaryProbe, command: &str) -> Option<String> {
    if cfg.force_inert {
        return Some("force_inert".to_string());
    }
    if !probe.found {
        return Some("rmux_binary_missing".to_string());
    }
    if !probe.compatible {
        return Some(
            probe
                .version_error
                .clone()
                .unwrap_or_else(|| "rmux_version_unreadable".to_string()),
        );
    }
    if !harness_binary_available(command) {
        return Some(format!("harness_binary_missing:{command}"));
    }
    None
}

#[derive(Debug, Clone)]
struct RmuxSpawnPlan {
    command: String,
    args: Vec<String>,
    cwd: PathBuf,
    /// Prompt to paste after spawn. `None` when delivered via argv or absent.
    paste_prompt: Option<String>,
    native_runtime: Option<NativeRuntimeMeta>,
    /// This run's id, exported as `ORGASMIC_RUN_ID` into the spawned pane's
    /// environment so a manager session recognises "I am already supervised"
    /// (`orgasmic manager register`, dec_3Y2E1).
    run_id: String,
    /// Harness-specific environment exported into the spawned pane. Carried on
    /// the plan (not applied at the rmux call site) so the stamp a transcript
    /// finder depends on is provable without a live rmux daemon.
    // orgasmic:TASK-GT91X
    harness_env: Vec<(String, String)>,
}

fn build_spawn_plan(cfg: &RmuxConfig, ctx: &DriverContext, harness: &str) -> RmuxSpawnPlan {
    let cwd = cfg
        .cwd
        .clone()
        .or_else(|| ctx.worktree.clone())
        .unwrap_or_else(|| std::env::current_dir().unwrap_or_else(|_| PathBuf::from("/tmp")));
    // Trim only to detect emptiness; argv/paste delivery must preserve bytes.
    let prompt_text = cfg
        .prompt_bundle_text
        .clone()
        .filter(|bundle| !bundle.trim().is_empty());

    // The daemon's dispatch path stages a placeholder command for every
    // worker; like the tmux driver, swap it for the real harness invocation
    // instead of executing the placeholder verbatim. For the `custom` harness
    // the real invocation IS `harness_args` (argv[0] + args) — the template's
    // `:HARNESS_ARGS:` is the whole wrapped command line.
    let staged_placeholder = is_dispatch_placeholder(cfg.command.as_deref(), &cfg.args);
    let mut harness_args_consumed = false;
    // The dispatch placeholder is the daemon's "swap me for the real harness"
    // sentinel; honor it for any harness (codex included), not just claude/custom.
    let (command, mut args) = if cfg.command.is_none() || staged_placeholder {
        if harness == "custom" && !cfg.harness_args.is_empty() {
            harness_args_consumed = true;
            (cfg.harness_args[0].clone(), cfg.harness_args[1..].to_vec())
        } else {
            default_command_for_harness(harness)
        }
    } else {
        (
            cfg.command.clone().unwrap_or_else(|| "sh".to_string()),
            cfg.args.clone(),
        )
    };

    // Worker/launch-supplied harness argv rides along whenever we are running
    // a real harness CLI (not the inert dispatch placeholder). It lands before
    // the guarded pushes below so user-specified flags take precedence.
    if !harness_args_consumed
        && !cfg.harness_args.is_empty()
        && !is_dispatch_placeholder(Some(command.as_str()), &args)
    {
        args.extend(cfg.harness_args.iter().cloned());
    }

    let is_claude = harness == "claude" && command == "claude";
    if is_claude {
        if !args
            .iter()
            .any(|arg| arg == "--dangerously-skip-permissions")
        {
            args.push("--dangerously-skip-permissions".to_string());
        }
        // A dispatched worker gets none of the operator's MCP servers. The acp
        // path has isolated its worker since it was written (adapters/claude.rs,
        // `ClaudeCredentialMode::NativeLogin`) and the TUI path did not, so the
        // same persona ran with a different blast radius depending only on which
        // mode carried it. This machine configures nine servers — Gmail, Drive,
        // Calendar and Notion among them — and the pane runs
        // --dangerously-skip-permissions unattended.
        //
        // `--strict-mcp-config` alone, deliberately: rmux launches are
        // `NativeLogin` (they resolve no credential mode at all), so `--bare`
        // would restrict auth to ANTHROPIC_API_KEY/apiKeyHelper and break every
        // subscription-auth worker. `--safe-mode` is the acp path's separate
        // claim about hooks/plugins/CLAUDE.md and is not this task's argument.
        // orgasmic:TASK-NYF7Z
        if !args.iter().any(|arg| arg == "--strict-mcp-config") {
            args.push("--strict-mcp-config".to_string());
        }
        if let Some(model) = cfg.model.as_ref() {
            if !args.iter().any(|arg| arg == "--model") {
                args.push("--model".to_string());
                args.push(model.clone());
            }
        }
        if let Some(effort) = cfg.effort.as_ref().or(cfg.reasoning_effort.as_ref()) {
            if !args.iter().any(|arg| arg == "--effort") {
                args.push("--effort".to_string());
                args.push(effort.clone());
            }
        }
        // Deterministic native Claude session identity (mirrors tmux): pin
        // --session-id to the run's runtime UUID so recovery can resume it.
        let session_id = claude_session_id(&ctx.identity.runtime_id);
        if !args.iter().any(|arg| arg == "--session-id") {
            args.push("--session-id".to_string());
            args.push(session_id);
        }
    }
    // No MCP-isolation counterpart exists for codex, so the reviewer stage runs
    // with the operator's servers by design-gap rather than by choice. Measured
    // against codex-cli 0.144.5 (TASK-NYF7Z), reading help text only:
    // `codex --help` exposes no `--strict-mcp-config` equivalent (`--strict-config`
    // is unrelated — it errors on unrecognised config.toml fields). `-c` cannot
    // stand in for one either: it deep-merges, so `-c mcp_servers={}` leaves every
    // server in place while `-c mcp_servers.<name>.command=...` still edits and
    // adds entries. Only `CODEX_HOME=<empty dir>` yields "No MCP servers
    // configured", and CODEX_HOME also holds `auth.json` — the same auth-breaking
    // trade `--bare` makes for claude, rejected on the same grounds.
    // orgasmic:TASK-NYF7Z
    if matches!(harness, "codex" | "cursor-agent" | "hermes") {
        if let Some(model) = cfg.model.as_ref() {
            if !args.iter().any(|arg| arg == "--model" || arg == "-m") {
                args.push("--model".to_string());
                args.push(model.clone());
            }
        }
    }

    // orgasmic:TASK-AFE5Q
    let paste_prompt = match prompt_text {
        Some(prompt) if argv_prompt_delivery_applies(harness, &command) => {
            push_initial_prompt_argv(&mut args, &prompt);
            None
        }
        other => other,
    };

    let native_runtime = if is_claude {
        let session_id = claude_session_id(&ctx.identity.runtime_id);
        Some(claude_native_runtime(&session_id, &cwd, &command, &args))
    } else {
        let mut launch_argv = vec![command.clone()];
        launch_argv.extend(args.iter().cloned());
        Some(NativeRuntimeMeta {
            provider: harness.to_string(),
            session_id: None,
            session_path: None,
            launch_argv,
            resume_argv: Vec::new(),
            // Interactive mux launches build their own argv and resolve no
            // credential mode (TASK-S0QRM).
            credential_mode: None,
        })
    };
    RmuxSpawnPlan {
        command,
        args,
        cwd,
        paste_prompt,
        native_runtime,
        run_id: ctx.identity.run_id.clone(),
        harness_env: harness_launch_env(harness),
    }
}

/// A `custom` dispatch (compiled prompt staged) with no `harness_args` would
/// spawn the fallback shell and paste the brief into it — executing prose as
/// shell commands. Refuse the config instead. Template parsing already
/// enforces `:HARNESS_ARGS:`; this guards hand-rolled driver configs.
fn custom_dispatch_misconfig(harness: &str, cfg: &RmuxConfig) -> Option<String> {
    let has_prompt = cfg
        .prompt_bundle_text
        .as_deref()
        .map(|bundle| !bundle.trim().is_empty())
        .unwrap_or(false);
    (harness == "custom" && has_prompt && cfg.harness_args.is_empty()).then(|| {
        "custom harness dispatch requires harness_args (the wrapped CLI argv); \
         refusing to paste a dispatch prompt into a bare shell"
            .to_string()
    })
}

/// Bounded default command per harness. Kept intentionally small: the smoke
/// proves session lifecycle, not a full agent turn.
fn default_command_for_harness(harness: &str) -> (String, Vec<String>) {
    match harness {
        "codex" => ("codex".to_string(), Vec::new()),
        "claude" => (
            "claude".to_string(),
            vec!["--dangerously-skip-permissions".to_string()],
        ),
        "cursor-agent" => ("cursor-agent".to_string(), Vec::new()),
        "hermes" => (
            "hermes".to_string(),
            vec!["chat".to_string(), "--tui".to_string()],
        ),
        // Bare terminal session: the operator's login shell, no agent CLI.
        // They drive any tool by hand through the attached xterm.
        "custom" => (
            std::env::var("SHELL").unwrap_or_else(|_| "sh".to_string()),
            Vec::new(),
        ),
        _ => ("sh".to_string(), Vec::new()),
    }
}

/// Stable rmux session name for a run. rmux's `SessionName::new` sanitizes `.`
/// and `:`, so this is already conservative.
pub fn rmux_session_name(identity: &RuntimeIdentity) -> String {
    format!("orgasmic-rmux-{}-{}", identity.run_id, identity.runtime_id)
}

/// Web Share smoke outcome. Carries only **redacted** operator material.
#[derive(Debug, Clone, Default)]
struct RmuxWebShareProof {
    attempted: bool,
    /// Full spectator URL — read-only, safe to surface.
    spectator_url: Option<String>,
    /// Whether an operator URL was minted. We never surface the raw URL/token.
    operator_minted: bool,
    /// Redacted operator URL form (scheme/host kept, token elided).
    operator_url_redacted: Option<String>,
    /// Exact limitation captured when a URL could not be produced.
    limitation: Option<String>,
}

impl RmuxWebShareProof {
    fn to_capabilities(&self) -> Value {
        json!({
            "attempted": self.attempted,
            "spectator_url": self.spectator_url,
            "operator_minted": self.operator_minted,
            "operator_url_redacted": self.operator_url_redacted,
            "limitation": self.limitation,
        })
    }
}

/// Redact an operator URL so logs/events never carry the live token. Keeps the
/// scheme + host and the path shape, replacing any token-bearing query/fragment
/// with a placeholder.
fn redact_operator_url(url: &str) -> String {
    let (head, _tail) = match url.find(['?', '#']) {
        Some(idx) => url.split_at(idx),
        None => (url, ""),
    };
    format!("{head}#<operator-token-redacted>")
}

#[async_trait]
impl WorkerDriver for RmuxDriver {
    fn transport(&self) -> &'static str {
        MODE
    }

    fn harness(&self) -> Option<&'static str> {
        Some(self.adapter.harness())
    }

    /// The harness runs as its own TUI inside an rmux pane an operator can
    /// attach to; the pane runtime must exist for the run to start at all.
    fn interaction(&self) -> TransportInteraction {
        TransportInteraction::TerminalPane
    }

    fn validate(&self, config: &DriverConfig) -> Result<(), DriverError> {
        let cfg: RmuxConfig = serde_json::from_value(config.0.clone())
            .map_err(|e| DriverError::InvalidConfig(e.to_string()))?;
        if let Some(cwd) = cfg.cwd.as_ref() {
            if !cwd.exists() {
                return Err(DriverError::InvalidConfig(format!(
                    "cwd does not exist: {}",
                    cwd.display()
                )));
            }
        }
        Ok(())
    }

    /// Readiness is the harness's question, not the transport's (see
    /// [`preflight_via_adapter`]).
    async fn preflight(&self, ctx: &DriverContext, config: &DriverConfig) -> PreflightOutcome {
        preflight_via_adapter(self.adapter.as_ref(), ctx, config).await
    }

    async fn acquire(
        &self,
        ctx: DriverContext,
        config: DriverConfig,
    ) -> Result<DriverSession, DriverError> {
        let cfg: RmuxConfig = serde_json::from_value(config.0.clone())
            .map_err(|e| DriverError::InvalidConfig(e.to_string()))?;
        let (tx, rx) = mpsc::channel(64);
        let harness = cfg
            .harness
            .as_deref()
            .unwrap_or_else(|| self.adapter.harness());
        if let Some(reason) = custom_dispatch_misconfig(harness, &cfg) {
            return Err(DriverError::InvalidConfig(reason));
        }
        let plan = build_spawn_plan(&cfg, &ctx, harness);
        let probe = if cfg.force_inert {
            RmuxBinaryProbe::missing()
        } else {
            probe_rmux_binary()
        };
        let inert_reason = inert_reason(&cfg, &probe, &plan.command);
        let inert = inert_reason.is_some();
        let session_name = rmux_session_name(&ctx.identity);
        let terminal_emitted = Arc::new(AtomicBool::new(false));
        let startup_cancel = Arc::new(AtomicBool::new(false));
        let send_child = SendChildOwner::new();

        let live = if inert {
            None
        } else {
            // Attempt to drive a real detached session through the SDK. Any
            // failure here is captured honestly and degrades to inert; it never
            // fabricates success.
            let rmux_bin = probe
                .path
                .clone()
                .unwrap_or_else(|| RMUX_BINARY.to_string());
            match run_live_session(
                &session_name,
                &rmux_bin,
                harness,
                &plan,
                &cfg,
                tx.clone(),
                terminal_emitted.clone(),
                startup_cancel.clone(),
                send_child.clone(),
                ctx.worktree.as_deref(),
            )
            .await
            {
                Ok(live) => Some(live),
                Err(err) => {
                    // Fall back to inert, surfacing the precise SDK/daemon error.
                    let _ = tx
                        .send(DriverEvent::Ready {
                            protocol_version: "rmux-smoke/1".into(),
                            capabilities: ready_capabilities(
                                true,
                                Some(format!("sdk_unavailable:{err}")),
                                &ctx,
                                &plan,
                                &probe,
                                None,
                                &RmuxWebShareProof::default(),
                            ),
                        })
                        .await;
                    return Ok(DriverSession {
                        identity: ctx.identity.clone(),
                        pid: None,
                        events: rx,
                        control: Box::new(RmuxControl::inert(tx, ctx.run_kind)),
                        producer: None,
                        native_runtime: plan.native_runtime.clone(),
                    });
                }
            }
        };

        let (web_share, lifecycle_task, startup_task, session, session_target) = match live {
            Some(live) => {
                let session_target = live.session.name().as_str().to_string();
                (
                    live.web_share,
                    Some(live.lifecycle_task),
                    live.startup_task,
                    Some(live.session),
                    Some(session_target),
                )
            }
            None => (RmuxWebShareProof::default(), None, None, None, None),
        };
        let lifecycle_abort = lifecycle_task.as_ref().map(JoinHandle::abort_handle);

        let _ = tx
            .send(DriverEvent::Ready {
                protocol_version: "rmux-smoke/1".into(),
                capabilities: ready_capabilities(
                    inert,
                    inert_reason,
                    &ctx,
                    &plan,
                    &probe,
                    session_target.clone(),
                    &web_share,
                ),
            })
            .await;

        // A live (non-inert) run owns a detached rmux session that must be
        // reaped on release/drop, or it lingers on the rmux daemon. The typed
        // `Session` handle is the primary teardown path and the CLI target is
        // its bounded fallback; inert runs own no session.
        let rmux_bin = probe
            .path
            .clone()
            .unwrap_or_else(|| RMUX_BINARY.to_string());
        Ok(DriverSession {
            identity: ctx.identity.clone(),
            pid: None,
            events: rx,
            control: if inert {
                Box::new(RmuxControl::inert(tx, ctx.run_kind))
            } else {
                Box::new(RmuxControl {
                    events: tx,
                    kind: ctx.run_kind,
                    lifecycle_abort,
                    startup_task,
                    startup_cancel,
                    send_child,
                    terminal_emitted,
                    released: false,
                    session,
                    // A system-wide session must survive a daemon shutdown, so the
                    // implicit Drop backstop must not reap it.
                    kill_on_drop: !cfg.system_wide,
                    rmux_bin: Some(rmux_bin),
                    session_target,
                    run_id: Some(ctx.identity.run_id.clone()),
                    harness_command: Some(plan.command.clone()),
                    input_ready_timeout: cfg.input_ready_timeout,
                })
            },
            producer: lifecycle_task,
            native_runtime: plan.native_runtime,
        })
    }

    async fn attach(
        &self,
        ctx: DriverContext,
        config: DriverConfig,
    ) -> Result<AttachOutcome, DriverError> {
        let cfg: RmuxConfig = serde_json::from_value(config.0.clone())
            .map_err(|e| DriverError::InvalidConfig(e.to_string()))?;
        if cfg.force_inert {
            return Ok(AttachOutcome::NotReattachable);
        }
        // A reattachable session lives in the (already running) rmux daemon. Use
        // the SDK's `connect` (never `connect_or_start`) so a *missing* daemon
        // is reported as not-reattachable instead of silently spawning a fresh
        // empty one.
        let rmux = match rmux_sdk::Rmux::builder()
            .default_timeout(Duration::from_secs(5))
            .connect()
            .await
        {
            Ok(rmux) => rmux,
            Err(e) => {
                tracing::info!(error = %e, "rmux attach: no live daemon to connect to");
                return Ok(AttachOutcome::NotReattachable);
            }
        };

        let session_name = rmux_sdk::SessionName::new(rmux_session_name(&ctx.identity))
            .map_err(|e| DriverError::Transport(format!("rmux session name: {e}")))?;
        let session_name_str = session_name.as_str().to_string();
        match rmux.has_session(session_name.clone()).await {
            Ok(true) => {}
            Ok(false) => return Ok(AttachOutcome::NotReattachable),
            Err(e) => {
                tracing::info!(error = %e, "rmux attach: has_session probe failed");
                return Ok(AttachOutcome::NotReattachable);
            }
        }

        let session = rmux
            .session(session_name)
            .await
            .map_err(|e| DriverError::Transport(format!("rmux attach session: {e}")))?;
        // Recovery already proved a compatible, live rmux daemon through the
        // SDK. Do not run the synchronous `rmux -V` discovery on this bounded
        // attach path; the configured daemon binary (or PATH name) is enough
        // for the best-effort mouse option command below.
        let rmux_bin =
            std::env::var(RMUX_SDK_DAEMON_BINARY_ENV).unwrap_or_else(|_| RMUX_BINARY.to_string());
        if let Err(err) = enable_rmux_mouse(&rmux_bin, &session_name_str).await {
            tracing::warn!(
                ?err,
                session = %session_name_str,
                "failed to enable rmux mouse mode during reattach"
            );
        }

        // Reattach: watch pane/process exit only. No paste, no capture.
        let harness = cfg
            .harness
            .as_deref()
            .unwrap_or_else(|| self.adapter.harness());
        let plan = build_spawn_plan(&cfg, &ctx, harness);

        let (tx, rx) = mpsc::channel(64);
        let terminal_emitted = Arc::new(AtomicBool::new(false));
        let pane = session.pane(0, 0);
        let lifecycle_task =
            spawn_pane_exit_watch(&pane, tx.clone(), terminal_emitted.clone()).await?;
        let lifecycle_abort = lifecycle_task.abort_handle();

        // No paste on reattach: the harness is already mid-conversation.
        let _ = tx
            .send(DriverEvent::Ready {
                protocol_version: "rmux-smoke/1".into(),
                capabilities: json!({
                    "inert": false,
                    "reattached": true,
                    "kind": ctx.run_kind,
                    "session": session_name_str,
                    "command": plan.command,
                }),
            })
            .await;

        Ok(AttachOutcome::Attached(Attached {
            session: Box::new(DriverSession {
                identity: ctx.identity.clone(),
                pid: None,
                events: rx,
                control: Box::new(RmuxControl {
                    events: tx,
                    kind: ctx.run_kind,
                    lifecycle_abort: Some(lifecycle_abort),
                    startup_task: None,
                    startup_cancel: Arc::new(AtomicBool::new(false)),
                    send_child: SendChildOwner::new(),
                    terminal_emitted,
                    released: false,
                    session: Some(session),
                    // A reattached session is, by definition, one we want to
                    // outlive the daemon — never reap it on an implicit Drop.
                    kill_on_drop: false,
                    rmux_bin: Some(rmux_bin),
                    session_target: Some(session_name_str.clone()),
                    run_id: Some(ctx.identity.run_id.clone()),
                    harness_command: Some(plan.command.clone()),
                    input_ready_timeout: cfg.input_ready_timeout,
                }),
                producer: Some(lifecycle_task),
                native_runtime: plan.native_runtime,
            }),
        }))
    }
}

#[allow(clippy::too_many_arguments)]
fn ready_capabilities(
    inert: bool,
    inert_reason: Option<String>,
    ctx: &DriverContext,
    plan: &RmuxSpawnPlan,
    probe: &RmuxBinaryProbe,
    session: Option<String>,
    web_share: &RmuxWebShareProof,
) -> Value {
    json!({
        "inert": inert,
        "inert_reason": inert_reason,
        "kind": ctx.run_kind,
        "session": session,
        "command": plan.command,
        "args": plan.args,
        "rmux_binary": {
            "found": probe.found,
            "compatible": probe.compatible,
            "path": probe.path,
            "source": probe.source,
            "version": probe.version,
            "required_version": RMUX_REQUIRED_VERSION,
            "version_error": probe.version_error,
        },
        "web_share": web_share.to_capabilities(),
        "smoke": true,
    })
}

/// State for a live (non-inert) rmux session.
struct LiveSession {
    lifecycle_task: JoinHandle<()>,
    startup_task: Option<JoinHandle<()>>,
    web_share: RmuxWebShareProof,
    /// Typed session handle retained as the primary teardown path. A bounded
    /// CLI `kill-session` is the backstop if this handle's transport is gone.
    session: rmux_sdk::Session,
}

/// Drive a real detached session via the rmux SDK, watch for pane/process exit,
/// and optionally mint Web Share URLs. No render/scrollback capture
/// (TASK-AFE5Q). Returns an error (caller degrades to inert) when the
/// SDK/daemon is unreachable.
#[allow(clippy::too_many_arguments)]
async fn run_live_session(
    session_name: &str,
    rmux_bin: &str,
    harness: &str,
    plan: &RmuxSpawnPlan,
    cfg: &RmuxConfig,
    events: mpsc::Sender<DriverEvent>,
    terminal_emitted: Arc<AtomicBool>,
    startup_cancel: Arc<AtomicBool>,
    send_child: SendChildOwner,
    workspace: Option<&Path>,
) -> Result<LiveSession, DriverError> {
    use rmux_sdk::{EnsureSession, EnsureSessionPolicy, ProcessSpec, Rmux, TerminalSizeSpec};

    let session_name = rmux_sdk::SessionName::new(session_name.to_string())
        .map_err(|e| DriverError::Transport(format!("rmux session name: {e}")))?;
    let session_target = session_name.as_str().to_string();
    let rmux = Rmux::builder()
        .default_timeout(Duration::from_secs(5))
        .connect_or_start()
        .await
        .map_err(|e| DriverError::Transport(format!("rmux connect_or_start: {e}")))?;

    if harness == "cursor-agent" {
        preflight_cursor_keychain(&rmux, &session_target, &plan.cwd).await?;
    }

    // Create an addressable pane first, then respawn it with remain-on-exit.
    // RMUX 0.9 retains the real exit code/signal only for a dead pane that is
    // kept around; creating the agent process directly could destroy the last
    // pane and session before the lifecycle watcher reads that terminal state.
    let session = rmux
        .ensure_session(
            EnsureSession::named(session_name)
                .policy(EnsureSessionPolicy::CreateOrReuse)
                .detached(true)
                .working_directory(plan.cwd.to_string_lossy().into_owned())
                .size(TerminalSizeSpec::new(200, 50))
                .process(ProcessSpec::default()),
        )
        .await
        .map_err(|e| DriverError::Transport(format!("rmux ensure_session: {e}")))?;

    let pane = session.pane(0, 0);
    let mut spawn = pane
        .spawn(std::iter::once(plan.command.clone()).chain(plan.args.iter().cloned()))
        .cwd(plan.cwd.clone())
        .env("ORGASMIC_RUN_ID", &plan.run_id);
    // orgasmic:TASK-GT91X
    for (key, value) in &plan.harness_env {
        spawn = spawn.env(key, value);
    }
    spawn
        .kill_existing(true)
        .keep_alive_on_exit(true)
        .await
        .map_err(|e| DriverError::Transport(format!("rmux spawn pane: {e}")))?;

    // Let attached terminal emulators report real mouse events to rmux. Its
    // default WheelUpPane binding enters copy mode and subsequent wheel events
    // scroll there, instead of leaking cursor-arrow sequences into the TUI.
    if let Err(err) = enable_rmux_mouse(rmux_bin, &session_target).await {
        tracing::warn!(?err, session = %session_target, "failed to enable rmux mouse mode");
    }

    let web_share = if cfg.web_share {
        mint_web_share(&session).await
    } else {
        RmuxWebShareProof::default()
    };

    let lifecycle_task =
        spawn_pane_exit_watch(&pane, events.clone(), terminal_emitted.clone()).await?;

    // Paste fallback only (hermes/custom). Supported TUIs already have the
    // prompt in argv. Deliver in the background so `acquire` returns promptly.
    let startup_task = if let Some(prompt) = plan.paste_prompt.clone() {
        let bin = rmux_bin.to_string();
        let session = session_target.clone();
        let command = plan.command.clone();
        let timeout = cfg.input_ready_timeout;
        let deliver_tx = events.clone();
        let deliver_terminal = terminal_emitted.clone();
        let send_child = send_child.clone();
        let cancel = startup_cancel.clone();
        Some(tokio::spawn(async move {
            if command == "claude" {
                if let Err(e) = wait_for_input_ready(
                    &bin,
                    &session,
                    timeout,
                    Some(&send_child),
                    Some(cancel.as_ref()),
                )
                .await
                {
                    tracing::warn!(
                        ?e,
                        "rmux TUI input field not detected within timeout; pasting anyway"
                    );
                }
            } else if let Err(e) = wait_for_pane_stable(&bin, &session, timeout).await {
                tracing::warn!(
                    ?e,
                    "rmux pane did not settle within timeout; pasting anyway"
                );
            }
            if let Err(e) =
                paste_text_and_submit(&bin, &session, &prompt, Some(&send_child), Some(&cancel))
                    .await
            {
                emit_fatal_driver_error_once(
                    &deliver_tx,
                    &deliver_terminal,
                    format!("dispatch prompt delivery failed: {e}"),
                )
                .await;
            }
        }))
    } else if cursor_argv_needs_startup_trust(harness, &plan.paste_prompt) {
        let bin = rmux_bin.to_string();
        let session = session_target.clone();
        let workspace = workspace
            .map(|path| path.display().to_string())
            .unwrap_or_default();
        let timeout = cfg.input_ready_timeout;
        let cancel = startup_cancel.clone();
        let send_child = send_child.clone();
        Some(tokio::spawn(async move {
            if let Err(e) = accept_cursor_workspace_trust_rmux(
                &bin,
                &session,
                &workspace,
                timeout,
                Some(cancel),
                Some(send_child),
            )
            .await
            {
                tracing::warn!(?e, "cursor workspace trust gate not cleared within timeout");
            }
        }))
    } else {
        None
    };

    Ok(LiveSession {
        lifecycle_task,
        startup_task,
        web_share,
        session,
    })
}

#[cfg(target_os = "macos")]
async fn preflight_cursor_keychain(
    rmux: &rmux_sdk::Rmux,
    session_target: &str,
    cwd: &Path,
) -> Result<(), DriverError> {
    use rmux_sdk::{EnsureSession, EnsureSessionPolicy, PaneOutputStart, ProcessSpec, SessionName};

    const MAX_OUTPUT_BYTES: usize = 8 * 1024;
    let probe_name = SessionName::new(format!("{session_target}-keychain-preflight"))
        .map_err(|e| DriverError::Transport(format!("rmux keychain preflight name: {e}")))?;
    let session = rmux
        .ensure_session(
            EnsureSession::named(probe_name)
                .policy(EnsureSessionPolicy::CreateOnly)
                .detached(true)
                .working_directory(cwd.to_string_lossy().into_owned())
                .process(ProcessSpec::default()),
        )
        .await
        .map_err(|e| DriverError::Transport(format!("rmux keychain preflight session: {e}")))?;
    let pane = session.pane(0, 0);

    let mut argv = vec![
        "/usr/bin/security".to_string(),
        "show-keychain-info".to_string(),
    ];
    if let Some(home) = std::env::var_os("HOME") {
        let login = PathBuf::from(home)
            .join("Library")
            .join("Keychains")
            .join("login.keychain-db");
        if login.is_file() {
            argv.push(login.to_string_lossy().into_owned());
        }
    }
    let spawn = pane
        .spawn(argv)
        .cwd(cwd.to_path_buf())
        .kill_existing(true)
        .keep_alive_on_exit(true)
        .await;
    if let Err(error) = spawn {
        let _ = session.kill().await;
        return Err(DriverError::Transport(format!(
            "rmux keychain preflight spawn: {error}"
        )));
    }

    let result = pane
        .collect_output_until_exit_starting_at(PaneOutputStart::Oldest, MAX_OUTPUT_BYTES)
        .await;
    let _ = session.kill().await;
    let collection = result.map_err(|error| {
        DriverError::Transport(format!(
            "macOS Keychain preflight inside rmux did not complete: {error}; {}",
            keychain_recovery_action()
        ))
    })?;
    classify_macos_keychain_preflight(&collection.bytes, collection.exit_state.as_ref())
        .map_err(DriverError::Transport)
}

#[cfg(not(target_os = "macos"))]
async fn preflight_cursor_keychain(
    _rmux: &rmux_sdk::Rmux,
    _session_target: &str,
    _cwd: &Path,
) -> Result<(), DriverError> {
    Ok(())
}

fn keychain_recovery_action() -> &'static str {
    "the rmux daemon is stale or not owned by the orgasmic.rmux user LaunchAgent; preserve any needed sessions, run `rmux kill-server`, then restart the orgasmic daemon service"
}

fn classify_macos_keychain_preflight(
    output: &[u8],
    exit: Option<&rmux_sdk::PaneExitState>,
) -> Result<(), String> {
    let text = String::from_utf8_lossy(output);
    let lower = text.to_ascii_lowercase();
    let err_sec_param = lower.contains("secitemcopymatching failed -50")
        || lower.contains("seckeychaincopysettings")
        || lower.contains("errsecparam")
        || lower.contains("one or more parameters passed to a function were not valid");
    if err_sec_param {
        return Err(format!(
            "macOS Keychain rejected the rmux process context with errSecParam (-50); {}",
            keychain_recovery_action()
        ));
    }

    match exit {
        Some(exit) if exit.code == Some(0) && exit.signal.is_none() => Ok(()),
        Some(exit) => Err(format!(
            "macOS Keychain preflight inside rmux failed (code={:?}, signal={:?}); {}",
            exit.code,
            exit.signal,
            keychain_recovery_action()
        )),
        None => Err(format!(
            "macOS Keychain preflight pane disappeared without an exit status; {}",
            keychain_recovery_action()
        )),
    }
}

/// Watch pane/process exit via the raw byte stream. Drain without synthesizing
/// TextChunks or scanning markers — live view stays on the PTY WebSocket.
///
/// Raw bytes rather than [`rmux_sdk::PaneLineStream`] on purpose: the line
/// stream buffers until LF, so a full-screen TUI repainting in place with
/// ANSI/CR yields no item for the whole stall window and the liveness signal
/// this drain exists to publish never fires (TASK-RWCRN.1).
// orgasmic:TASK-AFE5Q
async fn spawn_pane_exit_watch(
    pane: &rmux_sdk::Pane,
    events: mpsc::Sender<DriverEvent>,
    terminal_emitted: Arc<AtomicBool>,
) -> Result<JoinHandle<()>, DriverError> {
    let output_stream = pane
        .output_stream_starting_at(rmux_sdk::PaneOutputStart::Oldest)
        .await
        .map_err(|e| DriverError::Transport(format!("rmux output_stream: {e}")))?;
    Ok(tokio::spawn(watch_output_stream_exit(
        pane.clone(),
        output_stream,
        events,
        terminal_emitted,
        PANE_ACTIVITY_INTERVAL,
    )))
}

/// Run an rmux CLI verb against the daemon. The rmux CLI is tmux-compatible
/// for the buffer/send-keys verb set (the daemon's ws bridge relies on the
/// same contract).
async fn run_rmux_cli(bin: &str, args: &[&str]) -> Result<(), DriverError> {
    run_rmux_cli_with_owner(bin, args, None, None).await
}

async fn run_rmux_cli_os(bin: &str, args: &[OsString]) -> Result<(), DriverError> {
    let mut command = tokio::process::Command::new(bin);
    command.kill_on_drop(true);
    let child = command
        .args(args)
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| {
            DriverError::Transport(format!(
                "rmux {}: {e}",
                args.first()
                    .map(|arg| arg.to_string_lossy())
                    .unwrap_or_default()
            ))
        })?;
    wait_for_rmux_child(child, None).await
}

async fn run_rmux_cli_with_owner(
    bin: &str,
    args: &[&str],
    send_child: Option<&SendChildOwner>,
    cancel: Option<&AtomicBool>,
) -> Result<(), DriverError> {
    if let Some(owner) = send_child {
        let args = args.iter().map(|arg| arg.to_string()).collect::<Vec<_>>();
        owner
            .spawn_register_and_wait(cancel, || {
                let mut cmd = tokio::process::Command::new(bin);
                for arg in &args {
                    cmd.arg(arg);
                }
                cmd.stdout(Stdio::null())
                    .stderr(Stdio::piped())
                    .kill_on_drop(true);
                Ok(cmd)
            })
            .await
    } else {
        let mut command = tokio::process::Command::new(bin);
        command.kill_on_drop(true);
        let child = command
            .args(args)
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .spawn()
            .map_err(|e| {
                DriverError::Transport(format!("rmux {}: {e}", args.first().unwrap_or(&"")))
            })?;
        wait_for_rmux_child(child, cancel).await
    }
}

async fn wait_for_rmux_child(
    mut child: tokio::process::Child,
    cancel: Option<&AtomicBool>,
) -> Result<(), DriverError> {
    loop {
        if cancel.is_some_and(|flag| flag.load(Ordering::SeqCst)) {
            let _ = child.kill().await;
            let _ = child.wait().await;
            return Ok(());
        }
        match child.try_wait() {
            Ok(Some(status)) => {
                if status.success() {
                    return Ok(());
                }
                return Err(DriverError::Transport(format!(
                    "rmux send child exited with {status}"
                )));
            }
            Ok(None) => {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
            Err(e) => {
                return Err(DriverError::Transport(format!("rmux send child wait: {e}")));
            }
        }
    }
}

fn rmux_mouse_args(session: &str) -> [&str; 5] {
    ["set-option", "-t", session, "mouse", "on"]
}

async fn enable_rmux_mouse(bin: &str, session: &str) -> Result<(), DriverError> {
    run_rmux_cli(bin, &rmux_mouse_args(session)).await
}

/// The leading `rmux` CLI flags that address one specific SDK endpoint, so a
/// CLI fallback cannot drift to whatever daemon a bare `rmux` would resolve.
fn rmux_endpoint_args(endpoint: &rmux_sdk::RmuxEndpoint) -> Result<Vec<OsString>, DriverError> {
    let mut args = Vec::with_capacity(2);
    match endpoint {
        rmux_sdk::RmuxEndpoint::Default => {}
        rmux_sdk::RmuxEndpoint::UnixSocket(path) => {
            args.push(OsString::from("-S"));
            args.push(path.as_os_str().to_owned());
        }
        rmux_sdk::RmuxEndpoint::WindowsPipe(pipe) => {
            args.push(OsString::from("-S"));
            args.push(OsString::from(pipe));
        }
        _ => {
            return Err(DriverError::Transport(format!(
                "rmux CLI fallback does not support SDK endpoint {endpoint:?}"
            )));
        }
    }
    Ok(args)
}

fn rmux_session_reap_args(
    endpoint: &rmux_sdk::RmuxEndpoint,
    name: &rmux_sdk::SessionName,
) -> Result<Vec<OsString>, DriverError> {
    let mut args = rmux_endpoint_args(endpoint)?;
    args.reserve(3);
    args.extend([
        OsString::from("kill-session"),
        OsString::from("-t"),
        OsString::from(name.as_str()),
    ]);
    Ok(args)
}

async fn reap_rmux_session_with<SdkFuture, CliFallback, CliFuture>(
    sdk_kill: SdkFuture,
    cli_fallback: Option<CliFallback>,
    sdk_timeout: Duration,
    cli_timeout: Duration,
) -> Result<(), DriverError>
where
    SdkFuture: std::future::Future<Output = Result<(), String>>,
    CliFallback: FnOnce() -> CliFuture,
    CliFuture: std::future::Future<Output = Result<(), String>>,
{
    let sdk_error = match tokio::time::timeout(sdk_timeout, sdk_kill).await {
        Ok(Ok(())) => return Ok(()),
        Ok(Err(error)) => error,
        Err(_) => format!("timed out after {sdk_timeout:?}"),
    };

    let Some(cli_fallback) = cli_fallback else {
        return Err(DriverError::Transport(format!(
            "rmux session reap failed through SDK ({sdk_error}); CLI fallback unavailable"
        )));
    };
    match tokio::time::timeout(cli_timeout, cli_fallback()).await {
        Ok(Ok(())) => Ok(()),
        Ok(Err(cli_error)) => Err(DriverError::Transport(format!(
            "rmux session reap failed through SDK ({sdk_error}) and CLI fallback ({cli_error})"
        ))),
        Err(_) => Err(DriverError::Transport(format!(
            "rmux session reap failed through SDK ({sdk_error}); CLI fallback timed out after \
             {cli_timeout:?}"
        ))),
    }
}

async fn reap_rmux_session(
    session: &rmux_sdk::Session,
    rmux_bin: Option<String>,
) -> Result<(), DriverError> {
    // The SDK handle is the identity authority. Capture its resolved endpoint
    // and sanitized protocol-owned name before the primary request so fallback
    // cannot drift to a different daemon or pre-sanitization target.
    let cli_args = rmux_session_reap_args(session.endpoint(), session.name());
    let cli_fallback = rmux_bin.map(|rmux_bin| {
        move || async move {
            let cli_args = cli_args.map_err(|error| error.to_string())?;
            run_rmux_cli_os(&rmux_bin, &cli_args)
                .await
                .map_err(|error| error.to_string())
        }
    });
    reap_rmux_session_with(
        async {
            session
                .kill()
                .await
                .map(|_| ())
                .map_err(|error| error.to_string())
        },
        cli_fallback,
        RMUX_SESSION_SDK_REAP_TIMEOUT,
        RMUX_SESSION_CLI_REAP_TIMEOUT,
    )
    .await
}

async fn rmux_capture_pane(bin: &str, session: &str) -> Result<String, DriverError> {
    let mut last_error = None;
    // Cursor renders its startup/trust UI in the alternate screen. Probe that
    // first, then fall back to the normal history buffer for plain CLIs.
    for alternate in [true, false] {
        let mut command = tokio::process::Command::new(bin);
        command.arg("capture-pane");
        if alternate {
            command.arg("-a");
        }
        let output = command
            .args(["-p", "-t", session])
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .output()
            .await
            .map_err(|e| DriverError::Transport(format!("rmux capture-pane: {e}")))?;
        if output.status.success() {
            let pane = String::from_utf8_lossy(&output.stdout).into_owned();
            if !pane.trim().is_empty() || !alternate {
                return Ok(pane);
            }
        } else {
            last_error = Some(String::from_utf8_lossy(&output.stderr).trim().to_string());
        }
    }
    Err(DriverError::Transport(format!(
        "rmux capture-pane failed: {}",
        last_error.unwrap_or_else(|| "no screen buffer available".into())
    )))
}

async fn rmux_capture_pane_bounded(
    bin: &str,
    session: &str,
    timeout: Duration,
) -> Result<String, DriverError> {
    tokio::time::timeout(timeout, rmux_capture_pane(bin, session))
        .await
        .map_err(|_| DriverError::Transport("rmux capture-pane timed out".into()))?
}

/// Paste `text` into the session's pane and press Enter, via the rmux CLI's
/// tmux-compatible buffer verbs (the same path the daemon's composer uses).
async fn paste_text_and_submit(
    bin: &str,
    session: &str,
    text: &str,
    send_child: Option<&SendChildOwner>,
    cancel: Option<&AtomicBool>,
) -> Result<(), DriverError> {
    if text.is_empty() {
        return Ok(());
    }
    let buffer = format!("orgasmic-dispatch-{session}");
    run_rmux_cli_with_owner(
        bin,
        &["set-buffer", "-b", &buffer, "--", text],
        send_child,
        cancel,
    )
    .await?;
    let paste = run_rmux_cli_with_owner(
        bin,
        &["paste-buffer", "-p", "-b", &buffer, "-t", session],
        send_child,
        cancel,
    )
    .await;
    let _ =
        run_rmux_cli_with_owner(bin, &["delete-buffer", "-b", &buffer], send_child, cancel).await;
    paste?;
    run_rmux_cli_with_owner(
        bin,
        &["send-keys", "-t", session, "Enter"],
        send_child,
        cancel,
    )
    .await
}

async fn rmux_session_alive(bin: &str, session: &str) -> bool {
    let mut command = tokio::process::Command::new(bin);
    command.kill_on_drop(true);
    command
        .args(["has-session", "-t", session])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .await
        .map(|status| status.success())
        .unwrap_or(false)
}

fn abort_rmux_task(task: Option<JoinHandle<()>>) {
    if let Some(task) = task {
        task.abort();
    }
}

async fn accept_cursor_workspace_trust_rmux(
    bin: &str,
    session: &str,
    workspace_path: &str,
    timeout: Duration,
    cancel: Option<Arc<AtomicBool>>,
    send_child: Option<SendChildOwner>,
) -> Result<(), DriverError> {
    let bin = bin.to_string();
    let session = session.to_string();
    let workspace_path = workspace_path.to_string();
    accept_cursor_workspace_trust_with_capture(
        &workspace_path,
        timeout,
        Duration::from_millis(250),
        {
            let bin = bin.clone();
            let session = session.clone();
            move || {
                let bin = bin.clone();
                let session = session.clone();
                async move { rmux_capture_pane(&bin, &session).await }
            }
        },
        {
            let bin = bin.clone();
            let session = session.clone();
            move || {
                let bin = bin.clone();
                let session = session.clone();
                async move { rmux_session_alive(&bin, &session).await }
            }
        },
        {
            let bin = bin.clone();
            let session = session.clone();
            let send_child = send_child.clone();
            let cancel_for_send = cancel.clone();
            move |key| {
                let bin = bin.clone();
                let session = session.clone();
                let key = key.to_string();
                let send_child = send_child.clone();
                let cancel_for_send = cancel_for_send.clone();
                async move {
                    run_rmux_cli_with_owner(
                        &bin,
                        &["send-keys", "-t", &session, &key],
                        send_child.as_ref(),
                        cancel_for_send.as_ref().map(|flag| flag.as_ref()),
                    )
                    .await
                }
            }
        },
        cancel,
    )
    .await
}

/// Poll the rendered pane until the wrapped TUI shows its input prompt.
async fn wait_for_input_ready(
    bin: &str,
    session: &str,
    timeout: Duration,
    send_child: Option<&SendChildOwner>,
    cancel: Option<&AtomicBool>,
) -> Result<(), DriverError> {
    let deadline = tokio::time::Instant::now() + timeout;
    let mut poll = tokio::time::interval(Duration::from_millis(250));
    poll.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    poll.tick().await; // first tick is immediate; skip it
    loop {
        if tokio::time::Instant::now() >= deadline {
            return Err(DriverError::InputNotReady(timeout));
        }
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        let capture_timeout = remaining.min(Duration::from_secs(2));
        if let Ok(pane) = rmux_capture_pane_bounded(bin, session, capture_timeout).await {
            // Accept Claude's folder-trust dialog (default "Yes,
            // proceed") so a fresh worktree reaches its composer.
            if pane_requests_folder_trust(&pane) {
                let _ = run_rmux_cli_with_owner(
                    bin,
                    &["send-keys", "-t", session, "Enter"],
                    send_child,
                    cancel,
                )
                .await;
            } else if pane_has_input_prompt(&pane) {
                return Ok(());
            }
        }
        if tokio::time::Instant::now() >= deadline {
            return Err(DriverError::InputNotReady(timeout));
        }
        poll.tick().await;
    }
}

/// Poll the rendered pane until it is non-blank and identical across two
/// consecutive captures — a harness-agnostic "the wrapped TUI finished
/// booting" signal for CLIs we have no composer heuristic for (the `custom`
/// harness, e.g. opencode). The caller pastes anyway on timeout, mirroring
/// the claude input-ready fallback.
async fn wait_for_pane_stable(
    bin: &str,
    session: &str,
    timeout: Duration,
) -> Result<(), DriverError> {
    let deadline = tokio::time::Instant::now() + timeout;
    let mut poll = tokio::time::interval(Duration::from_millis(400));
    poll.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    poll.tick().await; // first tick is immediate; skip it
    let mut previous: Option<String> = None;
    loop {
        tokio::select! {
            _ = tokio::time::sleep_until(deadline) => {
                return Err(DriverError::InputNotReady(timeout));
            }
            _ = poll.tick() => {
                if let Ok(pane) = rmux_capture_pane_bounded(
                    bin,
                    session,
                    Duration::from_secs(2),
                )
                .await
                {
                    if !pane.trim().is_empty() && previous.as_deref() == Some(pane.as_str()) {
                        return Ok(());
                    }
                    previous = Some(pane);
                }
            }
        }
    }
}

/// Coalesces raw pane byte observations into at most one
/// [`DriverEvent::PaneActivity`] per `interval` (TASK-RWCRN).
///
/// The window opens on the first observed chunk and only restarts when an event
/// is emitted, so a pane that goes quiet for ten minutes and then writes again
/// publishes activity on that first chunk instead of waiting out another
/// interval. Time is a parameter rather than read from the clock so the cadence
/// is unit-testable without sleeping.
///
/// Only the byte *count* crosses this boundary. The bytes themselves are never
/// retained or forwarded (dec_WDR5K item 7; the 2.2 GiB `text_chunk` incident).
struct PaneActivityThrottle {
    interval: Duration,
    window_started_at: Option<tokio::time::Instant>,
    bytes: u64,
    seq: u64,
}

impl PaneActivityThrottle {
    fn new(interval: Duration) -> Self {
        Self {
            interval,
            window_started_at: None,
            bytes: 0,
            seq: 0,
        }
    }

    /// Record `bytes` of pane output seen at `now`. A zero-byte observation
    /// still counts as activity: an SDK lag notice proves the pane wrote
    /// output even though the bytes themselves were dropped by the daemon.
    fn observe_bytes(&mut self, bytes: u64, now: tokio::time::Instant) -> Option<DriverEvent> {
        self.bytes = self.bytes.saturating_add(bytes);
        let window_started_at = *self.window_started_at.get_or_insert(now);
        if now.duration_since(window_started_at) < self.interval {
            return None;
        }
        self.window_started_at = Some(now);
        let event = DriverEvent::PaneActivity {
            seq: self.seq,
            bytes: std::mem::take(&mut self.bytes),
        };
        self.seq += 1;
        Some(event)
    }
}

/// Drain the pane's raw byte stream until process exit. No TextChunk synthesis
/// and no marker scanning (TASK-AFE5Q): the only thing derived from the drained
/// chunks is their length, feeding the coalesced
/// [`DriverEvent::PaneActivity`] liveness signal the supervisor's stall detector
/// reads (TASK-RWCRN). `Ok(None)` means the pane process exited; the daemon maps
/// that terminal event through the finalize contract
/// (`protocol_end_without_finalize` when a declaration was required).
async fn watch_output_stream_exit(
    pane: rmux_sdk::Pane,
    mut output: rmux_sdk::PaneOutputStream,
    events: mpsc::Sender<DriverEvent>,
    terminal_emitted: Arc<AtomicBool>,
    activity_interval: Duration,
) {
    let mut activity = PaneActivityThrottle::new(activity_interval);
    loop {
        match output.next().await {
            Ok(Some(chunk)) => {
                if terminal_emitted.load(Ordering::SeqCst) {
                    break;
                }
                // Length only — the chunk's bytes are dropped here, never
                // buffered and never published.
                let observed = match &chunk {
                    rmux_sdk::PaneOutputChunk::Bytes { bytes, .. } => bytes.len() as u64,
                    // A gap report carries no reliable volume; it is liveness
                    // evidence with an unknown count.
                    _ => 0,
                };
                drop(chunk);
                if let Some(event) = activity.observe_bytes(observed, tokio::time::Instant::now()) {
                    if events.send(event).await.is_err() {
                        break;
                    }
                }
                continue;
            }
            Ok(None) => {
                emit_pane_exit(&pane, &events, &terminal_emitted).await;
                break;
            }
            Err(err) => {
                emit_fatal_driver_error_once(
                    &events,
                    &terminal_emitted,
                    format!("rmux output stream error: {err}"),
                )
                .await;
                break;
            }
        }
    }
}

async fn emit_pane_exit(
    pane: &rmux_sdk::Pane,
    events: &mpsc::Sender<DriverEvent>,
    terminal_emitted: &AtomicBool,
) {
    let exit = match pane.info().await {
        Ok(info) => info.panes.first().and_then(|pane| pane.exit_state.clone()),
        Err(error) => {
            emit_fatal_driver_error_once(
                events,
                terminal_emitted,
                format!("rmux pane exit status unavailable: {error}"),
            )
            .await;
            return;
        }
    };
    match classify_pane_exit(exit.as_ref()) {
        PaneTerminal::Clean(summary) => {
            emit_run_complete_once(events, terminal_emitted, Some(summary)).await;
        }
        PaneTerminal::Failed(message) => {
            emit_fatal_driver_error_once(events, terminal_emitted, message).await;
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum PaneTerminal {
    Clean(String),
    Failed(String),
}

fn classify_pane_exit(exit: Option<&rmux_sdk::PaneExitState>) -> PaneTerminal {
    let Some(exit) = exit else {
        return PaneTerminal::Failed(
            "rmux pane disappeared without retained exit status; treating the run as failed"
                .to_string(),
        );
    };
    if let Some(signal) = exit.signal {
        let shell_code = 128_i32.saturating_add(signal);
        let name = if signal == 11 { " (SIGSEGV)" } else { "" };
        return PaneTerminal::Failed(format!(
            "rmux pane exited by signal {signal}{name}; equivalent shell exit code {shell_code}"
        ));
    }
    match exit.code {
        Some(0) => PaneTerminal::Clean("rmux pane exited with code 0".to_string()),
        Some(code) => {
            let suffix = if code == 139 { " (SIGSEGV)" } else { "" };
            PaneTerminal::Failed(format!("rmux pane exited with code {code}{suffix}"))
        }
        None => PaneTerminal::Failed(exit.message.clone().unwrap_or_else(|| {
            "rmux pane exited without a code or signal; treating the run as failed".to_string()
        })),
    }
}

async fn emit_run_complete_once(
    events: &mpsc::Sender<DriverEvent>,
    terminal_emitted: &AtomicBool,
    summary: Option<String>,
) {
    spawn_run_complete_once(events, terminal_emitted, summary);
}

fn spawn_run_complete_once(
    events: &mpsc::Sender<DriverEvent>,
    terminal_emitted: &AtomicBool,
    summary: Option<String>,
) {
    if !terminal_emitted.swap(true, Ordering::SeqCst) {
        let events = events.clone();
        // The task is the durable publication owner. If the supervisor's
        // release timeout cancels the caller while the event channel is full,
        // this send remains alive and the drain still receives RunComplete.
        tokio::spawn(async move {
            let _ = events.send(DriverEvent::RunComplete { summary }).await;
        });
    }
}

async fn emit_fatal_driver_error_once(
    events: &mpsc::Sender<DriverEvent>,
    terminal_emitted: &AtomicBool,
    message: String,
) {
    if !terminal_emitted.swap(true, Ordering::SeqCst) {
        let _ = events
            .send(DriverEvent::DriverError {
                fatal: true,
                message,
            })
            .await;
    }
}

/// Attempt to mint spectator + operator Web Share URLs, recording the exact
/// limitation when either cannot be produced. Operator material is redacted.
async fn mint_web_share(session: &rmux_sdk::Session) -> RmuxWebShareProof {
    let mut proof = RmuxWebShareProof {
        attempted: true,
        ..RmuxWebShareProof::default()
    };
    match session.share().await {
        Ok(handle) => {
            proof.spectator_url = handle.spectator_url().map(str::to_string);
            if let Some(operator_url) = handle.operator_url() {
                proof.operator_minted = true;
                proof.operator_url_redacted = Some(redact_operator_url(operator_url));
            }
            if proof.spectator_url.is_none() && !proof.operator_minted {
                proof.limitation =
                    Some("web-share create returned neither spectator nor operator URL".into());
            }
            // Stop the share immediately; the smoke only proves URL minting.
            let _ = handle.stop().await;
        }
        Err(err) => {
            proof.limitation = Some(format!("web-share unavailable: {err}"));
        }
    }
    proof
}

struct RmuxControl {
    events: mpsc::Sender<DriverEvent>,
    kind: RunKind,
    /// Watches pane/process end only — never scrollback capture (TASK-AFE5Q).
    lifecycle_abort: Option<tokio::task::AbortHandle>,
    startup_task: Option<JoinHandle<()>>,
    startup_cancel: Arc<AtomicBool>,
    send_child: SendChildOwner,
    terminal_emitted: Arc<AtomicBool>,
    released: bool,
    /// Typed session handle for a live run and the primary `release`/`Drop`
    /// teardown path. `None` for inert runs, which own no rmux session.
    session: Option<rmux_sdk::Session>,
    /// Whether an implicit `Drop` (e.g. daemon shutdown) should reap the rmux
    /// session. `false` for system-wide and reattached runs, whose sessions are
    /// meant to outlive the daemon. Explicit `release` always reaps regardless.
    ///
    // orgasmic:task_69CW6
    /// Cancellation boundary, deliberately asymmetric and therefore stated
    /// rather than tested: for `kill_on_drop = true` runs an aborted `release`
    /// is retried by the `Drop` backstop, and
    /// `a_cancelled_release_still_reaps_through_the_drop_backstop` is that
    /// regression. For `kill_on_drop = false` runs there is no retry, by
    /// construction — `Drop` cannot distinguish "the operator's explicit stop
    /// was cancelled" from "the daemon is shutting down", and reaping on the
    /// second would destroy exactly the sessions these kinds exist to preserve.
    /// An aborted explicit release of a system-wide or reattached run therefore
    /// leaves the session alive and addressable, which is the recoverable
    /// outcome: the operator can stop it again. `Supervisor::release` never
    /// produces that abort on its own — its `DRIVER_RELEASE_TIMEOUT` exceeds the
    /// 2s + 2s reap budget — so this is a boundary for external aborts only.
    kill_on_drop: bool,
    /// rmux CLI binary for paste-buffer/send-keys delivery and reap fallback.
    /// `None` on inert runs (no live session to address).
    rmux_bin: Option<String>,
    /// Detached session target name for CLI verbs. `None` on inert runs.
    session_target: Option<String>,
    /// Run id retained for diagnostics / reattach identity.
    #[allow(dead_code)]
    run_id: Option<String>,
    /// Wrapped harness command (`claude`, `codex`, …) — recorded for diagnostics
    /// and future harness-specific followup heuristics.
    #[allow(dead_code)]
    harness_command: Option<String>,
    /// How long to wait for the harness composer before rejecting a followup as
    /// busy. Mirrors the dispatch-paste knob.
    input_ready_timeout: Duration,
}

impl RmuxControl {
    fn inert(events: mpsc::Sender<DriverEvent>, kind: RunKind) -> Self {
        Self {
            events,
            kind,
            lifecycle_abort: None,
            startup_task: None,
            startup_cancel: Arc::new(AtomicBool::new(false)),
            send_child: SendChildOwner::new(),
            terminal_emitted: Arc::new(AtomicBool::new(false)),
            released: false,
            session: None,
            kill_on_drop: true,
            rmux_bin: None,
            session_target: None,
            run_id: None,
            harness_command: None,
            input_ready_timeout: default_input_ready_timeout(),
        }
    }
}

/// Poll until the harness shows a composer input prompt. Followup delivery
/// gates on this (not pane stability) so mid-stream paste cannot corrupt an
/// in-flight turn — streaming output without a prompt is rejected.
async fn wait_for_followup_ready(
    bin: &str,
    session: &str,
    timeout: Duration,
    send_child: &SendChildOwner,
    cancel: &AtomicBool,
) -> Result<(), DriverError> {
    wait_for_input_ready(bin, session, timeout, Some(send_child), Some(cancel)).await
}

#[async_trait]
impl DriverControl for RmuxControl {
    async fn transition_state(
        &mut self,
        req: TransitionRequest,
    ) -> Result<TransitionAck, DriverError> {
        if self.kind == RunKind::Babysitter {
            return Err(DriverError::WorkerToolBlocked("transition_state".into()));
        }
        let _ = self
            .events
            .send(DriverEvent::TransitionState {
                from: req.from.clone(),
                to: req.to.clone(),
                reason: req.reason.clone(),
            })
            .await;
        Ok(TransitionAck {
            accepted: true,
            message: None,
        })
    }

    async fn babysitter_action(
        &mut self,
        req: BabysitterRequest,
    ) -> Result<BabysitterAck, DriverError> {
        if self.kind == RunKind::Worker {
            return Err(DriverError::BabysitterToolBlocked(req.tool.as_str().into()));
        }
        let _ = self
            .events
            .send(DriverEvent::ToolCall {
                call_id: format!(
                    "bs-{}",
                    chrono::Utc::now().timestamp_nanos_opt().unwrap_or(0)
                ),
                name: req.tool.as_str().into(),
                args: req.payload.clone(),
                seq: 0,
            })
            .await;
        Ok(BabysitterAck {
            accepted: true,
            message: None,
        })
    }

    async fn send_input(&mut self, req: UserInputRequest) -> Result<UserInputAck, DriverError> {
        let (bin, session) = match (self.rmux_bin.as_deref(), self.session_target.as_deref()) {
            (Some(bin), Some(session)) => (bin, session),
            _ => return Err(DriverError::Unsupported("send_input")),
        };

        // Mid-turn policy: reject rather than queue. Pasting while the harness is
        // still streaming its previous turn corrupts the in-flight turn. Gate on
        // composer input-readiness and return a clear ack when the prompt is not
        // visible yet — never paste blindly mid-stream.
        if wait_for_followup_ready(
            bin,
            session,
            self.input_ready_timeout,
            &self.send_child,
            &self.startup_cancel,
        )
        .await
        .is_err()
        {
            return Ok(UserInputAck {
                accepted: false,
                message: Some("harness busy".into()),
            });
        }

        paste_text_and_submit(
            bin,
            session,
            &req.input,
            Some(&self.send_child),
            Some(&self.startup_cancel),
        )
        .await?;
        Ok(UserInputAck {
            accepted: true,
            message: None,
        })
    }

    async fn release(&mut self, reason: &str) -> Result<(), DriverError> {
        if self.released {
            return Ok(());
        }
        self.released = true;
        // Publish the release-owned terminal claim before the fallible reap.
        // Its detached sender survives cancellation of this release future.
        spawn_run_complete_once(
            &self.events,
            &self.terminal_emitted,
            Some(reason.to_string()),
        );

        // The lifecycle watcher owns a Pane handle on the same ordered SDK
        // transport as Session. Killing first is mandatory: aborting the
        // watcher while its response is pending cancels that shared transport.
        let reap_result = match self.session.as_ref() {
            Some(session) => reap_rmux_session(session, self.rmux_bin.clone()).await,
            None => Ok(()),
        };
        // Retain the sole SDK handle across every cancellable await above. If
        // the caller cancels release, Drop still owns a durable retry path.
        self.session.take();

        if let Some(abort) = self.lifecycle_abort.take() {
            abort.abort();
        }
        cancel_and_join_driver_task(
            &self.startup_cancel,
            self.startup_task.take(),
            Some(&self.send_child),
        )
        .await;
        reap_result
    }
}

impl Drop for RmuxControl {
    fn drop(&mut self) {
        self.startup_cancel.store(true, Ordering::SeqCst);
        let lifecycle_abort = self.lifecycle_abort.take();
        let startup_task = self.startup_task.take();
        // System-wide / reattached runs intentionally outlive the daemon: never
        // reap their session on an implicit Drop (only explicit `release`
        // does). Dropping the `Session` handle does not reap the session — only
        // an explicit `Session::kill` does — so simply let the field drop.
        if !self.kill_on_drop {
            if let Some(abort) = lifecycle_abort {
                abort.abort();
            }
            abort_rmux_task(startup_task);
            return;
        }
        // Backstop when release() never ran (panic / early drop): retain the
        // lifecycle task until the kill finishes because its Pane shares the
        // Session's ordered SDK transport. `take()` means a prior release()
        // already cleared this.
        if let Some(session) = self.session.take() {
            match tokio::runtime::Handle::try_current() {
                Ok(handle) => {
                    let rmux_bin = self.rmux_bin.take();
                    handle.spawn(async move {
                        let reap_result = reap_rmux_session(&session, rmux_bin).await;
                        if let Some(abort) = lifecycle_abort {
                            abort.abort();
                        }
                        abort_rmux_task(startup_task);
                        if let Err(err) = reap_result {
                            tracing::error!(?err, "rmux session reap failed during drop backstop");
                        }
                    });
                }
                Err(_) => {
                    if let Some(abort) = lifecycle_abort {
                        abort.abort();
                    }
                    abort_rmux_task(startup_task);
                    tracing::warn!(
                        "rmux control dropped without release and no runtime handle; \
                         detached session left for daemon reaping"
                    );
                }
            }
        } else {
            if let Some(abort) = lifecycle_abort {
                abort.abort();
            }
            abort_rmux_task(startup_task);
        }
    }
}

/// Find a live rmux session whose name starts with `prefix`, connecting to an
/// already-running rmux daemon (never starting one). Used by the daemon's WS
/// bridge as a fallback when the supervisor holds no run record but a
/// system-wide session may have survived a daemon restart.
pub async fn find_live_session_with_prefix(prefix: &str) -> Option<String> {
    let rmux = rmux_sdk::Rmux::builder()
        .default_timeout(Duration::from_secs(5))
        .connect()
        .await
        .ok()?;
    let sessions = rmux.list_sessions().await.ok()?;
    sessions
        .into_iter()
        .map(|s| s.to_string())
        .find(|name| name.starts_with(prefix))
}

/// Convenience constructor for tests + supervisor smoke runs.
pub fn driver() -> RmuxDriver {
    RmuxDriver::new(Box::new(crate::adapters::CodexAdapter::new()))
}

/// Inert-mode config (no real rmux interaction) for smoke tests / missing rmux.
pub fn inert_config() -> DriverConfig {
    DriverConfig::from_value(json!({"force_inert": true}))
}

#[cfg(test)]
async fn wait_for_input_ready_with_capture<C, Fut>(
    timeout: Duration,
    poll_interval: Duration,
    mut capture: C,
) -> Result<(), DriverError>
where
    C: FnMut() -> Fut,
    Fut: std::future::Future<Output = Result<String, DriverError>>,
{
    let deadline = tokio::time::Instant::now() + timeout;
    let mut poll = tokio::time::interval(poll_interval);
    poll.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    poll.tick().await;
    loop {
        tokio::select! {
            _ = tokio::time::sleep_until(deadline) => {
                return Err(DriverError::InputNotReady(timeout));
            }
            _ = poll.tick() => {
                if let Ok(pane) = capture().await {
                    if pane_requests_folder_trust(&pane) {
                        continue;
                    }
                    if pane_has_input_prompt(&pane) {
                        return Ok(());
                    }
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::test_tooling::{
        assert_not_degraded, live_session_guard, skip_test_if_missing, test_environment_lock,
    };
    use super::*;
    use crate::modes::tmux::{
        classify_cursor_startup_frame, cursor_trust_dialog_frame, CursorStartupFrame,
    };
    use std::collections::VecDeque;

    async fn live_rmux_probe() -> RmuxBinaryProbe {
        let _environment = test_environment_lock().lock().await;
        probe_rmux_binary()
    }

    fn ctx(run_id: &str, kind: RunKind) -> DriverContext {
        DriverContext {
            identity: RuntimeIdentity::new(run_id, "boot-test"),
            run_kind: kind,
            task_id: "TASK-104".into(),
            worker_id: "implementer-codex-rmux".into(),
            project_id: Some("orgasmic".into()),
            worktree: None,
            babysitter_target: None,
        }
    }

    #[test]
    fn transport_name_is_stable() {
        assert_eq!(driver().transport(), "rmux");
    }

    #[test]
    fn mouse_mode_is_scoped_to_the_rmux_session() {
        assert_eq!(
            rmux_mouse_args("orgasmic-rmux-run-1-runtime-1"),
            [
                "set-option",
                "-t",
                "orgasmic-rmux-run-1-runtime-1",
                "mouse",
                "on"
            ]
        );
    }

    #[test]
    fn redact_operator_url_elides_token() {
        let redacted = redact_operator_url("https://share.example/op/abc?token=SECRET");
        assert!(!redacted.contains("SECRET"));
        assert!(redacted.starts_with("https://share.example/op/abc"));
        assert!(redacted.contains("operator-token-redacted"));
        // Fragment-bearing operator URLs are also elided.
        let frag = redact_operator_url("https://share.example/op/abc#k=SECRET");
        assert!(!frag.contains("SECRET"));
    }

    #[test]
    fn probe_honors_explicit_env_override() {
        let _environment = test_environment_lock().blocking_lock();
        // SAFETY: single-threaded test; we restore the prior value.
        let prior = std::env::var_os(RMUX_SDK_DAEMON_BINARY_ENV);
        std::env::set_var(RMUX_SDK_DAEMON_BINARY_ENV, "/nonexistent/rmux-binary-xyz");
        let probe = probe_rmux_binary();
        assert_eq!(probe.source, Some("env"));
        assert_eq!(probe.path.as_deref(), Some("/nonexistent/rmux-binary-xyz"));
        assert!(!probe.found, "nonexistent override must not be 'found'");
        match prior {
            Some(v) => std::env::set_var(RMUX_SDK_DAEMON_BINARY_ENV, v),
            None => std::env::remove_var(RMUX_SDK_DAEMON_BINARY_ENV),
        }
    }

    #[test]
    fn rmux_version_parser_requires_the_cli_identity_prefix() {
        assert_eq!(parse_rmux_version("rmux 0.9.0\n").as_deref(), Some("0.9.0"));
        assert_eq!(
            parse_rmux_version("rmux 0.9.0 (release)\n").as_deref(),
            Some("0.9.0")
        );
        assert_eq!(parse_rmux_version("tmux 3.6a\n"), None);
        assert_eq!(parse_rmux_version(""), None);
    }

    #[test]
    fn inert_reason_rejects_cli_sdk_version_mismatch_before_harness_probe() {
        let cfg = RmuxConfig::default();
        let probe = RmuxBinaryProbe {
            found: true,
            compatible: false,
            path: Some("/usr/local/bin/rmux".into()),
            source: Some("path"),
            version: Some("0.5.0".into()),
            version_error: Some("rmux version mismatch: expected 0.9.0, found 0.5.0".into()),
        };
        assert_eq!(
            inert_reason(&cfg, &probe, "definitely-not-a-real-binary-xyz").as_deref(),
            Some("rmux version mismatch: expected 0.9.0, found 0.5.0")
        );
    }

    #[test]
    fn keychain_preflight_classifier_rejects_err_sec_param_even_with_zero_exit() {
        let exit = rmux_sdk::PaneExitState::from_code(0);
        let error = classify_macos_keychain_preflight(
            b"ERROR: SecItemCopyMatching failed -50\r\n",
            Some(&exit),
        )
        .expect_err("errSecParam must fail closed");
        assert!(error.contains("errSecParam (-50)"), "{error}");
        assert!(error.contains("rmux kill-server"), "{error}");
    }

    #[test]
    fn keychain_preflight_classifier_accepts_a_clean_probe() {
        let exit = rmux_sdk::PaneExitState::from_code(0);
        assert!(classify_macos_keychain_preflight(b"Keychain no-timeout\r\n", Some(&exit)).is_ok());
    }

    #[test]
    fn pane_exit_classifier_preserves_sigsegv_and_exit_139() {
        assert_eq!(
            classify_pane_exit(Some(&rmux_sdk::PaneExitState::from_signal(11))),
            PaneTerminal::Failed(
                "rmux pane exited by signal 11 (SIGSEGV); equivalent shell exit code 139".into()
            )
        );
        assert_eq!(
            classify_pane_exit(Some(&rmux_sdk::PaneExitState::from_code(139))),
            PaneTerminal::Failed("rmux pane exited with code 139 (SIGSEGV)".into())
        );
        assert!(matches!(
            classify_pane_exit(None),
            PaneTerminal::Failed(message) if message.contains("disappeared")
        ));
    }

    #[test]
    fn inert_reason_reports_missing_rmux_binary_separately() {
        let cfg = RmuxConfig::default();
        let probe = RmuxBinaryProbe::missing();
        // rmux binary missing dominates and is reported on its own, not as a
        // harness-binary problem (acceptance criterion: separate checks).
        assert_eq!(
            inert_reason(&cfg, &probe, "codex"),
            Some("rmux_binary_missing".to_string())
        );
    }

    #[test]
    fn inert_reason_reports_missing_harness_when_rmux_present() {
        let cfg = RmuxConfig::default();
        let probe = RmuxBinaryProbe {
            found: true,
            compatible: true,
            path: Some("/usr/local/bin/rmux".into()),
            source: Some("path"),
            version: Some(RMUX_REQUIRED_VERSION.into()),
            version_error: None,
        };
        let reason = inert_reason(&cfg, &probe, "definitely-not-a-real-binary-xyz");
        assert_eq!(
            reason.as_deref(),
            Some("harness_binary_missing:definitely-not-a-real-binary-xyz")
        );
    }

    #[test]
    fn force_inert_short_circuits_probes() {
        let cfg = RmuxConfig {
            force_inert: true,
            ..RmuxConfig::default()
        };
        let probe = RmuxBinaryProbe {
            found: true,
            compatible: true,
            path: Some("/usr/local/bin/rmux".into()),
            source: Some("path"),
            version: Some(RMUX_REQUIRED_VERSION.into()),
            version_error: None,
        };
        assert_eq!(
            inert_reason(&cfg, &probe, "codex"),
            Some("force_inert".to_string())
        );
    }

    /// A codex pane must carry the originator override, because the transcript
    /// finder's cwd scan gates on it and codex offers no session id to fall
    /// back on. Reviewers run through this path (TASK-GT91X).
    // orgasmic:TASK-GT91X
    #[test]
    fn codex_rmux_pane_exports_transcript_finder_originator() {
        let cfg = RmuxConfig {
            harness: Some("codex".into()),
            ..RmuxConfig::default()
        };
        let plan = build_spawn_plan(&cfg, &ctx("run-codex-originator", RunKind::Worker), "codex");
        assert!(
            plan.harness_env
                .iter()
                .any(|(key, value)| key == crate::CODEX_ORIGINATOR_ENV
                    && value == crate::CODEX_ORIGINATOR),
            "codex rmux pane must export {}={} or its transcript is unreachable; got {:?}",
            crate::CODEX_ORIGINATOR_ENV,
            crate::CODEX_ORIGINATOR,
            plan.harness_env
        );

        // Scoped to codex: the claude and cursor-agent finders are untouched.
        for harness in ["claude", "cursor-agent"] {
            let cfg = RmuxConfig {
                harness: Some(harness.into()),
                ..RmuxConfig::default()
            };
            let plan = build_spawn_plan(&cfg, &ctx("run-other", RunKind::Worker), harness);
            assert!(plan.harness_env.is_empty(), "{harness} needs no stamp");
        }
    }

    #[test]
    fn default_command_is_bounded_per_harness() {
        assert_eq!(default_command_for_harness("codex").0, "codex");
        assert_eq!(default_command_for_harness("claude").0, "claude");
        assert_eq!(
            default_command_for_harness("cursor-agent").0,
            "cursor-agent"
        );
        let hermes = default_command_for_harness("hermes");
        assert_eq!(hermes.0, "hermes");
        assert_eq!(hermes.1, vec!["chat".to_string(), "--tui".to_string()]);
        assert_eq!(default_command_for_harness("unknown").0, "sh");
    }

    #[test]
    fn prompt_bytes_preserved_with_leading_trailing_whitespace() {
        let bundle = "\n  do the task  \n";
        for harness in ["claude", "codex", "cursor-agent"] {
            let cfg = RmuxConfig {
                harness: Some(harness.into()),
                prompt_bundle_text: Some(bundle.to_string()),
                ..RmuxConfig::default()
            };
            let plan = build_spawn_plan(&cfg, &ctx("run-bytes", RunKind::Worker), harness);
            assert_eq!(plan.args.last().map(String::as_str), Some(bundle));
            assert_eq!(plan.paste_prompt.as_deref(), None);
        }
        let hermes_cfg = RmuxConfig {
            harness: Some("hermes".into()),
            prompt_bundle_text: Some(bundle.to_string()),
            ..RmuxConfig::default()
        };
        let hermes = build_spawn_plan(
            &hermes_cfg,
            &ctx("run-hermes-bytes", RunKind::Worker),
            "hermes",
        );
        assert_eq!(hermes.paste_prompt.as_deref(), Some(bundle));
    }

    #[tokio::test]
    async fn accept_cursor_workspace_trust_sends_a_without_pasting_prompt() {
        let trust = cursor_trust_dialog_frame("/tmp/worktree");
        let ready = "cursor-agent\n❯ \n";
        let mut panes =
            VecDeque::from([Ok(trust.clone()), Ok(trust.clone()), Ok(ready.to_string())]);
        let mut sent = Vec::new();
        let result = accept_cursor_workspace_trust_with_capture(
            "/tmp/worktree",
            Duration::from_millis(50),
            Duration::from_millis(1),
            || {
                let pane = panes.pop_front().unwrap_or_else(|| Ok(ready.to_string()));
                async move { pane }
            },
            || async { true },
            |key: &str| {
                sent.push(key.to_string());
                async { Ok(()) }
            },
            None,
        )
        .await;
        assert!(result.is_ok());
        assert_eq!(sent, vec!["a"]);
    }

    #[tokio::test]
    async fn accept_cursor_workspace_trust_rmux_waits_through_loading() {
        let loading = "starting cursor-agent\n";
        let trust = cursor_trust_dialog_frame("/tmp/worktree");
        let mut panes = VecDeque::from([
            Ok(loading.to_string()),
            Ok(trust.clone()),
            Ok(trust.clone()),
        ]);
        let mut sent = Vec::new();
        let result = accept_cursor_workspace_trust_with_capture(
            "/tmp/worktree",
            Duration::from_millis(50),
            Duration::from_millis(1),
            || {
                let pane = panes.pop_front().unwrap_or_else(|| Ok(trust.clone()));
                async move { pane }
            },
            || async { true },
            |key: &str| {
                sent.push(key.to_string());
                async { Ok(()) }
            },
            None,
        )
        .await;
        assert!(result.is_ok());
        assert_eq!(sent, vec!["a"]);
    }

    #[tokio::test]
    async fn accept_cursor_workspace_trust_rmux_prompt_prose_sends_nothing() {
        let prose = "TASK-756WX\nWorkspace Trust Required\n[a] Trust this workspace\n\n❯ ";
        let mut sent = Vec::new();
        let result = accept_cursor_workspace_trust_with_capture(
            "/tmp/worktree",
            Duration::from_millis(50),
            Duration::from_millis(1),
            || async { Ok(prose.to_string()) },
            || async { true },
            |key: &str| {
                sent.push(key.to_string());
                async { Ok(()) }
            },
            None,
        )
        .await;
        assert!(result.is_ok());
        assert!(sent.is_empty());
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn rmux_send_child_owner_release_kills_blocked_fake_cli() {
        use std::os::unix::fs::PermissionsExt;

        let tmp = tempfile::tempdir().unwrap();
        let bin = tmp.path().join("fake-rmux");
        std::fs::write(&bin, "#!/bin/sh\nsleep 300\n").unwrap();
        let mut perms = std::fs::metadata(&bin).unwrap().permissions();
        perms.set_mode(0o755);
        std::fs::set_permissions(&bin, perms).unwrap();

        let owner = SendChildOwner::new();
        let cancel = Arc::new(AtomicBool::new(false));
        let cancel_for_task = cancel.clone();
        let owner_for_task = owner.clone();
        let task = tokio::spawn(async move {
            let _ = run_rmux_cli_with_owner(
                bin.to_str().unwrap(),
                &["send-keys", "a"],
                Some(&owner_for_task),
                Some(cancel_for_task.as_ref()),
            )
            .await;
        });
        tokio::time::sleep(Duration::from_millis(100)).await;
        let joined = tokio::time::timeout(
            Duration::from_secs(2),
            cancel_and_join_driver_task(cancel.as_ref(), Some(task), Some(&owner)),
        )
        .await;
        assert!(
            joined.is_ok(),
            "release must kill/join a blocked fake rmux CLI child promptly"
        );
    }

    #[test]
    fn rmux_reap_fallback_uses_sdk_owned_endpoint_and_sanitized_name() {
        let endpoint =
            rmux_sdk::RmuxEndpoint::UnixSocket(PathBuf::from("/tmp/rmux-custom-endpoint.sock"));
        let name = rmux_sdk::SessionName::new("planned.name:with-separators").unwrap();

        assert_eq!(
            rmux_session_reap_args(&endpoint, &name).unwrap(),
            vec![
                OsString::from("-S"),
                OsString::from("/tmp/rmux-custom-endpoint.sock"),
                OsString::from("kill-session"),
                OsString::from("-t"),
                OsString::from("planned_name_with-separators"),
            ]
        );
    }

    #[tokio::test]
    async fn rmux_reap_runs_cli_fallback_after_sdk_failure() {
        let cli_ran = Arc::new(AtomicBool::new(false));
        let cli_probe = Arc::clone(&cli_ran);
        reap_rmux_session_with(
            async { Err("sdk transport gone".to_string()) },
            Some(move || async move {
                cli_probe.store(true, Ordering::SeqCst);
                Ok(())
            }),
            Duration::from_millis(50),
            Duration::from_millis(50),
        )
        .await
        .expect("CLI fallback should recover an SDK failure");
        assert!(cli_ran.load(Ordering::SeqCst));
    }

    #[tokio::test]
    async fn rmux_reap_times_out_stalled_sdk_and_reserves_cli_fallback_time() {
        let cli_ran = Arc::new(AtomicBool::new(false));
        let cli_probe = Arc::clone(&cli_ran);
        reap_rmux_session_with(
            std::future::pending::<Result<(), String>>(),
            Some(move || async move {
                cli_probe.store(true, Ordering::SeqCst);
                Ok(())
            }),
            Duration::from_millis(10),
            Duration::from_millis(50),
        )
        .await
        .expect("stalled SDK kill should fall back within the total budget");
        assert!(cli_ran.load(Ordering::SeqCst));
    }

    #[tokio::test]
    async fn rmux_reap_reports_both_sdk_and_cli_failures() {
        let error = reap_rmux_session_with(
            async { Err("sdk refused".to_string()) },
            Some(|| async { Err("cli refused".to_string()) }),
            Duration::from_millis(50),
            Duration::from_millis(50),
        )
        .await
        .expect_err("two failed reap attempts must remain visible");
        let message = error.to_string();
        assert!(message.contains("sdk refused"), "{message}");
        assert!(message.contains("cli refused"), "{message}");
    }

    /// TASK-RWCRN. A pane that keeps writing must publish activity often enough
    /// that the supervisor's stall clock can never expire, and it must coalesce:
    /// one event per interval, not one per chunk. 10 chunks/s for 95 s of a
    /// working claude TUI is 950 chunks and must produce exactly 3 events.
    #[test]
    fn pane_activity_is_coalesced_to_one_event_per_interval() {
        let interval = Duration::from_secs(30);
        let mut throttle = PaneActivityThrottle::new(interval);
        let start = tokio::time::Instant::now();
        let mut events = Vec::new();
        for tick in 0..950u64 {
            let now = start + Duration::from_millis(100 * tick);
            if let Some(event) = throttle.observe_bytes(64, now) {
                events.push((now.duration_since(start), event));
            }
        }

        assert_eq!(events.len(), 3, "{events:?}");
        for (elapsed, event) in &events {
            assert!(
                *elapsed < DEFAULT_STALL_TIMEOUT_FOR_TESTS,
                "the first event must land well inside the supervisor's stall window, got {elapsed:?}"
            );
            assert!(matches!(event, DriverEvent::PaneActivity { .. }));
        }
        // Sequence numbers are monotonic and `bytes` reports the window's real
        // volume, which is diagnostic only — see the manager-dispatch convention
        // for why it must not be read as a progress classifier.
        assert!(matches!(
            events[0].1,
            DriverEvent::PaneActivity {
                seq: 0,
                bytes: 19_264
            }
        ));
        assert!(matches!(
            events[1].1,
            DriverEvent::PaneActivity {
                seq: 1,
                bytes: 19_200
            }
        ));
        assert!(matches!(
            events[2].1,
            DriverEvent::PaneActivity {
                seq: 2,
                bytes: 19_200
            }
        ));
    }

    /// TASK-RWCRN.1, the reviewer's ship blocker. A full-screen TUI repaints in
    /// place with CR/ANSI and emits no LF at all; the old line-terminated
    /// observation saw nothing and the pane was killed at 600 s. Byte
    /// observation must publish on exactly the same cadence for output that
    /// never contains `\n`.
    #[test]
    fn pane_activity_fires_for_a_redrawing_tui_that_never_emits_a_newline() {
        let interval = Duration::from_secs(30);
        let mut throttle = PaneActivityThrottle::new(interval);
        let start = tokio::time::Instant::now();
        // A CR-returned progress line, then ANSI cursor repositioning: the exact
        // shape the line stream buffers forever below its 1 MiB safety flush.
        let frames = [
            b"\r  42% ###......".as_slice(),
            b"\x1b[2;1H\x1b[K  43% ####.....".as_slice(),
        ];
        let mut events = Vec::new();
        for tick in 0..950u64 {
            let frame = frames[(tick % 2) as usize];
            assert!(!frame.contains(&b'\n'), "fixture must never emit LF");
            let now = start + Duration::from_millis(100 * tick);
            if let Some(event) = throttle.observe_bytes(frame.len() as u64, now) {
                events.push((now.duration_since(start), event));
            }
        }

        assert_eq!(
            events.len(),
            3,
            "a redrawing TUI must publish on the same cadence as a line-writing one, got {events:?}"
        );
        let (first_elapsed, first) = &events[0];
        assert!(
            *first_elapsed < DEFAULT_STALL_TIMEOUT_FOR_TESTS,
            "the first event must land inside the stall window, got {first_elapsed:?}"
        );
        let DriverEvent::PaneActivity { seq, bytes } = first else {
            panic!("expected PaneActivity, got {first:?}");
        };
        assert_eq!(*seq, 0);
        assert!(*bytes > 0, "newline-free output must still be counted");
    }

    /// The supervisor's `DEFAULT_STALL_TIMEOUT`, restated here so the drivers
    /// crate can assert its cadence is safely inside it without depending on the
    /// daemon crate.
    const DEFAULT_STALL_TIMEOUT_FOR_TESTS: Duration = Duration::from_secs(600);

    /// TASK-RWCRN. A pane that produces nothing must publish nothing: the whole
    /// point of choosing pane output over an unconditional periodic event is
    /// that a wedged pane stays silent and is still released as stalled.
    #[test]
    fn a_silent_pane_publishes_no_activity() {
        let mut throttle = PaneActivityThrottle::new(Duration::from_secs(30));
        let start = tokio::time::Instant::now();
        // No `observe_bytes` calls at all: nothing to emit, however long we wait.
        assert!(throttle.window_started_at.is_none());
        assert_eq!(throttle.bytes, 0);

        // One burst, then silence: the burst's own window never completes, so a
        // pane that dies after its startup banner publishes nothing either.
        assert!(throttle.observe_bytes(512, start).is_none());
        assert!(throttle
            .observe_bytes(512, start + Duration::from_millis(50))
            .is_none());
    }

    /// TASK-RWCRN. After a long quiet stretch the very next chunk publishes
    /// immediately rather than waiting out another full interval, so liveness
    /// resumes as soon as the pane does.
    #[test]
    fn pane_activity_resumes_on_the_first_chunk_after_a_quiet_stretch() {
        let mut throttle = PaneActivityThrottle::new(Duration::from_secs(30));
        let start = tokio::time::Instant::now();
        assert!(throttle.observe_bytes(8, start).is_none());
        assert!(throttle
            .observe_bytes(8, start + Duration::from_secs(500))
            .is_some());
    }

    /// TASK-RWCRN.1. A lag notice carries no reliable byte count but does prove
    /// the pane wrote output the daemon could not retain, so it must still
    /// count as liveness rather than being silently dropped.
    #[test]
    fn a_zero_byte_observation_still_counts_as_liveness() {
        let mut throttle = PaneActivityThrottle::new(Duration::from_secs(30));
        let start = tokio::time::Instant::now();
        assert!(throttle.observe_bytes(0, start).is_none());
        assert!(throttle.window_started_at.is_some());
        assert!(matches!(
            throttle.observe_bytes(0, start + Duration::from_secs(31)),
            Some(DriverEvent::PaneActivity { seq: 0, bytes: 0 })
        ));
    }

    #[tokio::test]
    async fn run_complete_publication_survives_release_owner_cancellation() {
        let (events, mut rx) = mpsc::channel(1);
        events
            .send(DriverEvent::Ready {
                protocol_version: "test/1".into(),
                capabilities: json!({}),
            })
            .await
            .unwrap();
        let terminal_emitted = Arc::new(AtomicBool::new(false));
        let owner_events = events.clone();
        let owner_terminal = Arc::clone(&terminal_emitted);
        let owner = tokio::spawn(async move {
            spawn_run_complete_once(
                &owner_events,
                owner_terminal.as_ref(),
                Some("release-owned".into()),
            );
            std::future::pending::<()>().await;
        });
        tokio::task::yield_now().await;
        owner.abort();
        let _ = owner.await;

        assert!(matches!(rx.recv().await, Some(DriverEvent::Ready { .. })));
        assert!(matches!(
            tokio::time::timeout(Duration::from_secs(1), rx.recv()).await,
            Ok(Some(DriverEvent::RunComplete { summary }))
                if summary.as_deref() == Some("release-owned")
        ));
    }

    #[tokio::test]
    async fn cursor_trust_rmux_probe_fresh_worktree_when_enabled() {
        if std::env::var("ORGASMIC_PROBE_CURSOR_TRUST").as_deref() != Ok("1") {
            eprintln!(
                "SKIP cursor_trust_rmux_probe_fresh_worktree_when_enabled: set ORGASMIC_PROBE_CURSOR_TRUST=1"
            );
            return;
        }
        let tmp = tempfile::tempdir().unwrap();
        let git_init = StdCommand::new("git")
            .args(["init", "--quiet"])
            .current_dir(tmp.path())
            .status()
            .expect("initialize fresh probe worktree");
        assert!(git_init.success(), "fresh probe worktree must initialize");
        let session = format!("orgasmic-rmux-trust-probe-{}", std::process::id());
        let rmux_bin = std::env::var_os("RMUX_SDK_DAEMON_BINARY")
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from("rmux"));
        let cursor_binary = StdCommand::new("which")
            .arg("cursor-agent")
            .output()
            .expect("resolve cursor-agent for trust probe");
        assert!(
            cursor_binary.status.success(),
            "cursor-agent must be on PATH"
        );
        let cursor_binary = String::from_utf8(cursor_binary.stdout)
            .expect("cursor-agent path is UTF-8")
            .trim()
            .to_string();
        let rmux = rmux_sdk::Rmux::builder()
            .default_timeout(Duration::from_secs(5))
            .connect_or_start()
            .await
            .expect("connect rmux for trust probe");
        let session_name =
            rmux_sdk::SessionName::new(session.clone()).expect("valid rmux probe session name");
        let mut cursor_process = rmux_sdk::ProcessSpec::argv([cursor_binary]);
        cursor_process.environment = Some(vec!["TERM=xterm-256color".into()]);
        let session_handle = rmux
            .ensure_session(
                rmux_sdk::EnsureSession::named(session_name)
                    .policy(rmux_sdk::EnsureSessionPolicy::CreateOrReuse)
                    .detached(true)
                    .working_directory(tmp.path().to_string_lossy().into_owned())
                    .size(rmux_sdk::TerminalSizeSpec::new(200, 50))
                    .process(cursor_process),
            )
            .await
            .expect("ensure rmux probe session");
        let workspace = tmp.path().display().to_string();
        let deadline = std::time::Instant::now() + Duration::from_secs(10);
        let (pane, frame) = loop {
            let pane = rmux_capture_pane(rmux_bin.to_str().expect("rmux bin path"), &session)
                .await
                .expect("capture rmux probe pane");
            let frame = classify_cursor_startup_frame(&pane, &workspace);
            if !matches!(frame, CursorStartupFrame::BlankOrLoading)
                || std::time::Instant::now() >= deadline
            {
                break (pane, frame);
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        };
        let _ = session_handle.kill().await;
        assert!(
            matches!(
                frame,
                CursorStartupFrame::TrustDialog | CursorStartupFrame::Ready
            ),
            "fresh cursor-agent rmux pane should be trust dialog or composer-ready, got {frame:?}\n{pane}"
        );
    }

    #[test]
    fn cursor_argv_delivery_skips_paste_prompt() {
        let cfg = RmuxConfig {
            harness: Some("cursor-agent".into()),
            prompt_bundle_text: Some("do the task".into()),
            ..RmuxConfig::default()
        };
        let plan = build_spawn_plan(&cfg, &ctx("run-cursor", RunKind::Worker), "cursor-agent");
        assert!(plan.paste_prompt.is_none());
        assert!(plan
            .args
            .windows(2)
            .any(|pair| pair == ["--", "do the task"]));
        assert!(cursor_argv_needs_startup_trust(
            "cursor-agent",
            &plan.paste_prompt
        ));
    }

    /// Dispatch placeholder + claude harness must spawn the real claude TUI
    /// with the dispatch prompt staged for delivery — never run the
    /// placeholder verbatim (the bug that made rmux worker dispatches
    /// complete instantly with only the placeholder echo).
    #[test]
    fn dispatch_placeholder_swaps_to_real_claude_invocation() {
        let cfg: RmuxConfig = serde_json::from_value(json!({
            "command": "sh",
            "args": ["-lc", "echo orgasmic pipeline stage acquired; exec sh"],
            "harness": "claude",
            "model": "claude-sonnet-4-6",
            "prompt_bundle_text": "do the task",
        }))
        .unwrap();
        let ctx = ctx("run-dispatch", RunKind::Worker);
        let plan = build_spawn_plan(&cfg, &ctx, "claude");
        assert_eq!(plan.command, "claude");
        assert!(plan
            .args
            .iter()
            .any(|arg| arg == "--dangerously-skip-permissions"));
        assert!(plan
            .args
            .windows(2)
            .any(|pair| pair == ["--model", "claude-sonnet-4-6"]));
        assert!(plan.args.iter().any(|arg| arg == "--session-id"));
        assert!(plan.paste_prompt.is_none(), "claude uses argv delivery");
        assert!(plan
            .args
            .windows(2)
            .any(|pair| pair == ["--", "do the task"]));
        assert!(!plan.args.iter().any(|arg| arg.contains("orgasmic-eot")));
        assert!(!plan
            .args
            .iter()
            .any(|arg| arg.contains("end-of-turn marker")));
        let native = plan.native_runtime.expect("claude native runtime");
        assert_eq!(native.provider, "claude");
        assert!(!native.resume_argv.is_empty());
    }

    #[test]
    fn pty_model_and_effort_preserve_exact_option_bytes() {
        let cfg = RmuxConfig {
            harness: Some("claude".into()),
            model: Some("  custom-model  ".into()),
            effort: Some(" XHIGH ".into()),
            ..RmuxConfig::default()
        };
        let plan = build_spawn_plan(&cfg, &ctx("run-verbatim", RunKind::Worker), "claude");
        assert!(plan
            .args
            .windows(2)
            .any(|pair| pair == ["--model", "  custom-model  "]));
        assert!(plan
            .args
            .windows(2)
            .any(|pair| pair == ["--effort", " XHIGH "]));
    }

    #[test]
    fn dispatch_placeholder_swaps_to_real_codex_invocation() {
        // Regression: the swap gate was `claude || custom` only, so codex
        // workers executed the placeholder `sh` and the prompt was typed into
        // a bare shell. The daemon sentinel must swap to real `codex`.
        let cfg: RmuxConfig = serde_json::from_value(json!({
            "command": "sh",
            "args": ["-lc", "echo orgasmic pipeline stage acquired; exec sh"],
            "harness": "codex",
            "model": "gpt-5.5",
            "prompt_bundle_text": "do the task",
        }))
        .unwrap();
        let ctx = ctx("run-dispatch-codex", RunKind::Worker);
        let plan = build_spawn_plan(&cfg, &ctx, "codex");
        assert_eq!(plan.command, "codex");
        assert!(!is_dispatch_placeholder(
            Some(plan.command.as_str()),
            &plan.args
        ));
        assert!(plan.paste_prompt.is_none(), "codex uses argv delivery");
        assert!(plan
            .args
            .windows(2)
            .any(|pair| pair == ["--", "do the task"]));
        let native = plan.native_runtime.expect("codex native runtime");
        assert_eq!(native.provider, "codex");
    }

    /// Worker `:HARNESS_ARGS:` ride along on the real harness argv, and a
    /// user-supplied `--model` there beats the worker default (the guarded
    /// push skips when the flag is already present). The inert dispatch
    /// placeholder never receives them.
    #[test]
    fn harness_args_extend_claude_argv_and_win_over_model() {
        let cfg: RmuxConfig = serde_json::from_value(json!({
            "command": "sh",
            "args": ["-lc", "echo orgasmic pipeline stage acquired; exec sh"],
            "harness": "claude",
            "model": "claude-sonnet-4-6",
            "harness_args": ["--model", "claude-haiku-4-5", "--betas", "context-1m"],
            "prompt_bundle_text": "do the task",
        }))
        .unwrap();
        let ctx = ctx("run-dispatch", RunKind::Worker);
        let plan = build_spawn_plan(&cfg, &ctx, "claude");
        assert_eq!(plan.command, "claude");
        assert!(plan
            .args
            .windows(2)
            .any(|pair| pair == ["--model", "claude-haiku-4-5"]));
        assert!(!plan
            .args
            .windows(2)
            .any(|pair| pair == ["--model", "claude-sonnet-4-6"]));
        assert!(plan
            .args
            .windows(2)
            .any(|pair| pair == ["--betas", "context-1m"]));
    }

    /// A dispatched rmux/claude worker is MCP-isolated by default, and an
    /// operator who names the flag in `harness_args` opts that pane back in
    /// (the guarded push must not duplicate it).
    // orgasmic:TASK-NYF7Z
    #[test]
    fn claude_argv_is_mcp_isolated_by_default_and_operator_supplied_wins() {
        let base = json!({
            "command": "sh",
            "args": ["-lc", "echo orgasmic pipeline stage acquired; exec sh"],
            "harness": "claude",
            "model": "claude-opus-5",
            "prompt_bundle_text": "do the task",
        });
        let ctx = ctx("run-dispatch-mcp", RunKind::Worker);

        let cfg: RmuxConfig = serde_json::from_value(base.clone()).unwrap();
        let plan = build_spawn_plan(&cfg, &ctx, "claude");
        assert_eq!(plan.command, "claude");
        assert_eq!(
            plan.args
                .iter()
                .filter(|arg| *arg == "--strict-mcp-config")
                .count(),
            1,
            "default rmux/claude argv must isolate MCP exactly once: {:?}",
            plan.args
        );
        // The flag this task explicitly refused: rmux is NativeLogin, and
        // `--bare` would break subscription auth.
        assert!(!plan.args.iter().any(|arg| arg == "--bare"), "{:?}", plan.args);
        // The RECORDED argv carries it too. `native_runtime.launch_argv` is what
        // run state surfaces after the process is gone, so this is the surface a
        // manager reads to confirm "a live dispatch shows the flag in its argv"
        // — the acceptance criterion no test can reach by spawning a real pane.
        let native = plan.native_runtime.expect("claude native runtime");
        assert!(
            native
                .launch_argv
                .iter()
                .any(|arg| arg == "--strict-mcp-config"),
            "recorded launch argv must show the flag: {:?}",
            native.launch_argv
        );

        let mut opted_back_in = base;
        opted_back_in["harness_args"] = json!(["--strict-mcp-config"]);
        let cfg: RmuxConfig = serde_json::from_value(opted_back_in).unwrap();
        let plan = build_spawn_plan(&cfg, &ctx, "claude");
        assert_eq!(
            plan.args
                .iter()
                .filter(|arg| *arg == "--strict-mcp-config")
                .count(),
            1,
            "operator-supplied flag must suppress the guarded push: {:?}",
            plan.args
        );
    }

    /// The recorded codex answer, pinned as an assertion: codex-cli has no
    /// `--strict-mcp-config` counterpart (see the comment in `build_spawn_plan`),
    /// so orgasmic composes no isolation flag for it and `harness_args` stays the
    /// only channel by which an operator could add one.
    // orgasmic:TASK-NYF7Z
    #[test]
    fn codex_argv_composes_no_mcp_isolation_flag_because_none_exists() {
        let base = json!({
            "command": "sh",
            "args": ["-lc", "echo orgasmic pipeline stage acquired; exec sh"],
            "harness": "codex",
            "model": "gpt-5.1-codex-max",
            "prompt_bundle_text": "do the task",
        });
        let ctx = ctx("run-dispatch-codex-mcp", RunKind::Worker);

        let cfg: RmuxConfig = serde_json::from_value(base.clone()).unwrap();
        let plan = build_spawn_plan(&cfg, &ctx, "codex");
        assert_eq!(plan.command, "codex");
        assert!(
            !plan
                .args
                .iter()
                .any(|arg| arg == "--strict-mcp-config" || arg == "--strict-config"),
            "codex has no MCP-isolation flag; orgasmic must not invent one: {:?}",
            plan.args
        );

        // Operator-supplied argv still rides along, so a pane can carry whatever
        // codex-side isolation a future codex-cli grows.
        let mut supplied = base;
        supplied["harness_args"] = json!(["-c", "mcp_servers={}"]);
        let cfg: RmuxConfig = serde_json::from_value(supplied).unwrap();
        let plan = build_spawn_plan(&cfg, &ctx, "codex");
        assert!(
            plan.args
                .windows(2)
                .any(|pair| pair == ["-c", "mcp_servers={}"]),
            "{:?}",
            plan.args
        );
    }

    /// Custom-harness dispatch: the staged placeholder is swapped for the
    /// template's `:HARNESS_ARGS:` command line (argv[0] + args), the compiled
    /// prompt is staged for paste delivery, and the rendered-screen output
    /// path is the default (the wrapped CLI is an interactive agent TUI).
    #[test]
    fn custom_dispatch_placeholder_runs_harness_args_as_command() {
        let cfg: RmuxConfig = serde_json::from_value(json!({
            "command": "sh",
            "args": ["-lc", "echo orgasmic pipeline stage acquired; exec sh"],
            "harness": "custom",
            "harness_args": ["opencode", "--print-logs"],
            "prompt_bundle_text": "do the task",
        }))
        .unwrap();
        let ctx = ctx("run-dispatch-custom", RunKind::Worker);
        let plan = build_spawn_plan(&cfg, &ctx, "custom");
        assert_eq!(plan.command, "opencode");
        assert_eq!(plan.args, vec!["--print-logs"]);
        assert_eq!(plan.paste_prompt.as_deref(), Some("do the task"));
        assert!(!plan
            .paste_prompt
            .as_deref()
            .unwrap()
            .contains("orgasmic-eot"));
        let native = plan.native_runtime.expect("native runtime meta");
        assert_eq!(native.provider, "custom");
        assert_eq!(native.launch_argv, vec!["opencode", "--print-logs"]);
    }

    /// A custom launch with no harness args stays the bare-terminal session
    /// (manager escape hatch): login shell, line-oriented output, no prompt.
    #[test]
    fn custom_without_harness_args_stays_bare_shell() {
        let cfg: RmuxConfig = serde_json::from_value(json!({
            "harness": "custom",
        }))
        .unwrap();
        let plan = build_spawn_plan(&cfg, &ctx("run-bare", RunKind::Worker), "custom");
        let shell = std::env::var("SHELL").unwrap_or_else(|_| "sh".to_string());
        assert_eq!(plan.command, shell);
        assert!(plan.args.is_empty());
        assert!(plan.paste_prompt.is_none());
    }

    /// A custom dispatch (prompt staged) without harness args must be refused:
    /// pasting the brief into the fallback shell would execute it.
    #[test]
    fn custom_dispatch_without_harness_args_is_refused() {
        let with_prompt: RmuxConfig = serde_json::from_value(json!({
            "harness": "custom",
            "prompt_bundle_text": "do the task",
        }))
        .unwrap();
        assert!(custom_dispatch_misconfig("custom", &with_prompt).is_some());

        let with_args: RmuxConfig = serde_json::from_value(json!({
            "harness": "custom",
            "harness_args": ["opencode"],
            "prompt_bundle_text": "do the task",
        }))
        .unwrap();
        assert!(custom_dispatch_misconfig("custom", &with_args).is_none());

        let no_prompt: RmuxConfig = serde_json::from_value(json!({
            "harness": "custom",
        }))
        .unwrap();
        assert!(custom_dispatch_misconfig("custom", &no_prompt).is_none());
        assert!(custom_dispatch_misconfig("claude", &with_prompt).is_none());
    }

    /// An explicit non-placeholder command is honored verbatim.
    #[test]
    fn explicit_command_is_not_swapped() {
        let cfg: RmuxConfig = serde_json::from_value(json!({
            "command": "sleep",
            "args": ["5"],
            "harness": "claude",
        }))
        .unwrap();
        let plan = build_spawn_plan(&cfg, &ctx("run-explicit", RunKind::Worker), "claude");
        assert_eq!(plan.command, "sleep");
        assert!(plan.paste_prompt.is_none());
    }

    #[test]
    fn session_name_is_run_scoped() {
        let id = RuntimeIdentity::new("run-x", "boot-1");
        let name = rmux_session_name(&id);
        assert!(name.starts_with("orgasmic-rmux-run-x-"));
    }

    #[tokio::test]
    async fn inert_acquire_emits_ready_and_completes() {
        let d = driver();
        let mut s = d
            .acquire(ctx("run-inert", RunKind::Worker), inert_config())
            .await
            .unwrap();
        let ev = s.events.recv().await.unwrap();
        let DriverEvent::Ready { capabilities, .. } = ev else {
            panic!("expected Ready, got {ev:?}");
        };
        assert_eq!(capabilities["inert"], true);
        assert_eq!(capabilities["inert_reason"], "force_inert");
        assert_eq!(capabilities["smoke"], true);
        // rmux binary is reported as a separate, distinct field.
        assert!(capabilities["rmux_binary"].is_object());
        assert_eq!(capabilities["rmux_binary"]["found"], false);
        s.control.release("done").await.unwrap();
        let ev2 = s.events.recv().await.unwrap();
        assert!(matches!(ev2, DriverEvent::RunComplete { .. }));
    }

    #[tokio::test]
    async fn attach_force_inert_is_not_reattachable() {
        let d = driver();
        let out = d
            .attach(ctx("run-no-attach", RunKind::Worker), inert_config())
            .await
            .unwrap();
        assert!(matches!(out, AttachOutcome::NotReattachable));
    }

    /// Live reattach smoke (boot auto-reattach path). Acquire a real session,
    /// then `attach` with the same identity must return a second live handle
    /// that streams from the same rmux session. Skipped without an rmux binary.
    #[tokio::test]
    async fn live_rmux_attach_reattaches_when_available() {
        let _live_guard = live_session_guard().owning("run-attach");
        let probe = live_rmux_probe().await;
        if skip_test_if_missing(
            "live_rmux_attach_reattaches_when_available",
            &[("rmux", probe.found)],
        ) {
            return;
        }
        let d = driver();
        let context = ctx("run-attach", RunKind::Worker);
        let cfg = DriverConfig::from_value(json!({
            "command": "sleep",
            "args": ["30"],
        }));
        let mut s = d.acquire(context.clone(), cfg.clone()).await.unwrap();
        let ev = s.events.recv().await.unwrap();
        let DriverEvent::Ready { capabilities, .. } = ev else {
            panic!("expected Ready, got {ev:?}");
        };
        assert_not_degraded(
            "live_rmux_attach_reattaches_when_available",
            capabilities["inert"] == true,
        );

        let out = d.attach(context.clone(), cfg).await.unwrap();
        let AttachOutcome::Attached(attached) = out else {
            panic!("expected Attached for a live session");
        };
        let mut s2 = *attached.session;
        let ev2 = s2.events.recv().await.unwrap();
        let DriverEvent::Ready { capabilities, .. } = ev2 else {
            panic!("expected Ready from attach, got {ev2:?}");
        };
        assert_eq!(capabilities["reattached"], true);
        assert_eq!(
            capabilities["session"],
            json!(rmux_session_name(&context.identity))
        );

        // Tear down through the original handle; the attached handle must not
        // reap the session on drop (kill_on_drop=false), only stop streaming.
        drop(s2);
        s.control.release("test done").await.unwrap();

        // The session is gone after an explicit release.
        let out = d
            .attach(
                context,
                DriverConfig::from_value(json!({"command": "sleep", "args": ["30"]})),
            )
            .await
            .unwrap();
        assert!(matches!(out, AttachOutcome::NotReattachable));
    }

    /// TASK-RWCRN production-path smoke: a real rmux pane writing real output
    /// must publish `PaneActivity` on the driver's own event channel — the whole
    /// path the daemon consumes (`acquire` → `run_live_session` →
    /// `spawn_pane_exit_watch` → `watch_output_stream_exit`). The unit tests
    /// above cover the cadence but cannot prove the pane is wired to it, because
    /// `PaneOutputStream` cannot be constructed outside the SDK.
    ///
    /// Returns the first observed `(seq, bytes)`, or `None` if the channel
    /// closed first; panics if nothing arrived within two intervals.
    async fn live_pane_activity_for(test_name: &'static str, shell: &str) -> Option<(u64, u64)> {
        let probe = live_rmux_probe().await;
        assert!(probe.found, "{test_name} needs rmux on PATH");
        let d = driver();
        let cfg = DriverConfig::from_value(json!({
            "command": "/bin/sh",
            "args": ["-c", shell],
        }));
        let mut s = d
            .acquire(ctx("run-pane-activity", RunKind::Worker), cfg)
            .await
            .unwrap();
        let ready = s.events.recv().await.expect("ready event");
        let DriverEvent::Ready { capabilities, .. } = ready else {
            panic!("expected Ready, got {ready:?}");
        };
        assert_not_degraded(test_name, capabilities["inert"] == true);

        let observed = tokio::time::timeout(PANE_ACTIVITY_INTERVAL * 2, async {
            loop {
                match s.events.recv().await {
                    Some(DriverEvent::PaneActivity { seq, bytes }) => break Some((seq, bytes)),
                    Some(_) => continue,
                    None => break None,
                }
            }
        })
        .await;

        // Reap before asserting: a failed assertion must not leak the session.
        s.control.release("test done").await.unwrap();

        observed.expect("a writing pane must publish PaneActivity within 2 intervals")
    }

    /// `#[ignore]` because it spawns a real rmux daemon and waits one full
    /// `PANE_ACTIVITY_INTERVAL` (TASK-RRT4T: live smokes are ignored so the
    /// default summary counts them instead of silently passing). Run with
    /// `cargo test -p orgasmic-drivers -- --ignored live_rmux_pane_publishes`.
    #[tokio::test]
    #[ignore = "live rmux smoke: real rmux session, waits one PANE_ACTIVITY_INTERVAL"]
    async fn live_rmux_pane_publishes_pane_activity_while_it_writes() {
        let _live_guard = live_session_guard().owning("run-pane-activity");
        let observed = live_pane_activity_for(
            "live_rmux_pane_publishes_pane_activity_while_it_writes",
            "while :; do echo tick; sleep 0.05; done",
        )
        .await;
        let (seq, bytes) = observed.expect("event channel closed before any pane activity");
        assert_eq!(seq, 0);
        assert!(
            bytes > 100,
            "a pane writing ~20 lines/s across one {PANE_ACTIVITY_INTERVAL:?} window should \
             report many bytes, got {bytes}"
        );
    }

    /// TASK-RWCRN.1, the ship blocker's live proof. The smoke above uses
    /// `echo tick`, which guarantees an LF per update and therefore proves
    /// nothing about a full-screen harness. This fixture writes a CR-returned
    /// progress line plus an ANSI cursor reposition and *never* an LF for
    /// longer than `PANE_ACTIVITY_INTERVAL` — under the old
    /// `PaneLineStream` drain the SDK buffered every byte below its 1 MiB
    /// safety flush and this test would time out with zero events.
    #[tokio::test]
    #[ignore = "live rmux smoke: real rmux session, waits one PANE_ACTIVITY_INTERVAL"]
    async fn live_rmux_pane_publishes_pane_activity_for_newline_free_redraws() {
        let _live_guard = live_session_guard().owning("run-pane-activity");
        // `printf` with no trailing \n; the pane sees CR, ANSI, and text only.
        let observed = live_pane_activity_for(
            "live_rmux_pane_publishes_pane_activity_for_newline_free_redraws",
            "i=0; while :; do i=$((i+1)); \
             printf '\\r\\033[K%s%% working' \"$i\"; sleep 0.05; done",
        )
        .await;
        let (seq, bytes) = observed.expect("event channel closed before any pane activity");
        assert_eq!(
            seq, 0,
            "a redrawing pane must publish its first activity on the same cadence"
        );
        assert!(
            bytes > 100,
            "a pane repainting ~20x/s with no LF across one {PANE_ACTIVITY_INTERVAL:?} window \
             should report many bytes, got {bytes}"
        );
    }

    #[tokio::test]
    async fn inert_release_is_idempotent() {
        let d = driver();
        let mut s = d
            .acquire(ctx("run-idem", RunKind::Worker), inert_config())
            .await
            .unwrap();
        let _ = s.events.recv().await;
        s.control.release("a").await.unwrap();
        s.control.release("b").await.unwrap();
    }

    #[tokio::test]
    async fn implementer_transition_state_accepted_then_event() {
        let d = driver();
        let mut s = d
            .acquire(ctx("run-tx", RunKind::Worker), inert_config())
            .await
            .unwrap();
        let _ready = s.events.recv().await.unwrap();
        let ack = s
            .control
            .transition_state(TransitionRequest {
                from: "ready".into(),
                to: "in_progress".into(),
                reason: "starting".into(),
            })
            .await
            .unwrap();
        assert!(ack.accepted);
        let ev = s.events.recv().await.unwrap();
        assert!(matches!(ev, DriverEvent::TransitionState { .. }));
    }

    #[tokio::test]
    async fn babysitter_cannot_transition_state() {
        let d = driver();
        let mut s = d
            .acquire(ctx("run-bs", RunKind::Babysitter), inert_config())
            .await
            .unwrap();
        let _ready = s.events.recv().await.unwrap();
        let err = s
            .control
            .transition_state(TransitionRequest {
                from: "ready".into(),
                to: "in_progress".into(),
                reason: "x".into(),
            })
            .await
            .unwrap_err();
        assert!(matches!(err, DriverError::WorkerToolBlocked(_)));
    }

    /// Live rmux smoke. Skipped unless a real rmux binary is discoverable
    /// (RMUX_SDK_DAEMON_BINARY or PATH). On hosts without rmux this returns
    /// early so CI stays green; the honest inert path is covered above.
    #[tokio::test]
    async fn live_rmux_session_lifecycle_when_available() {
        let _live_guard = live_session_guard().owning("run-live");
        let probe = live_rmux_probe().await;
        if skip_test_if_missing(
            "live_rmux_session_lifecycle_when_available",
            &[("rmux", probe.found)],
        ) {
            return;
        }
        let d = driver();
        let cfg = DriverConfig::from_value(json!({
            "command": "sleep",
            "args": ["30"],
            "web_share": true,
        }));
        let mut s = d
            .acquire(ctx("run-live", RunKind::Worker), cfg)
            .await
            .unwrap();
        let ev = s.events.recv().await.unwrap();
        let DriverEvent::Ready { capabilities, .. } = ev else {
            panic!("expected Ready, got {ev:?}");
        };
        // A compatible rmux on PATH is not proof that a session was acquired:
        // `run_live_session` turns every SDK/daemon startup error into an inert
        // `Ready`. Degrading here means this test never exercised its named
        // behaviour, so it is a failure, not a second honest outcome
        // (TASK-R2HDN). Release first so the assert cannot leak a session.
        if capabilities["inert"] == true {
            s.control.release("cleanup").await.unwrap();
            assert_not_degraded("live_rmux_session_lifecycle_when_available", true);
        }
        let session = capabilities["session"]
            .as_str()
            .expect("live rmux run reports its session");
        let rmux_bin = probe.path.as_deref().unwrap_or(RMUX_BINARY);
        let mouse = tokio::process::Command::new(rmux_bin)
            .args(["show-options", "-v", "-t", session, "mouse"])
            .output()
            .await
            .expect("query rmux mouse option");
        assert!(
            mouse.status.success(),
            "show-options mouse failed: {}",
            String::from_utf8_lossy(&mouse.stderr).trim()
        );
        assert_eq!(String::from_utf8_lossy(&mouse.stdout).trim(), "on");
        // Web Share proof: a spectator URL and/or operator URL, or an exact
        // recorded limitation. Never expose a raw operator token.
        let ws = &capabilities["web_share"];
        assert_eq!(ws["attempted"], true);
        let produced_url =
            ws["spectator_url"].is_string() || ws["operator_url_redacted"].is_string();
        assert!(
            produced_url || ws["limitation"].is_string(),
            "web-share must produce a URL or record a limitation: {ws}"
        );
        // Operator material is only ever surfaced redacted.
        if let Some(redacted) = ws["operator_url_redacted"].as_str() {
            assert!(
                redacted.contains("operator-token-redacted"),
                "operator url must be redacted: {redacted}"
            );
        }
        s.control.release("cleanup").await.unwrap();
    }

    /// Live rmux output + lifecycle smoke. Drives a short command that prints a
    /// line and exits, then proves the new SDK path: the line arrives as a
    /// `TextChunk` over `Pane::line_stream`, and the stream ending (process
    /// exit) emits `RunComplete` on its own — with no EOT marker, no
    /// `capture-pane` poll, and no `kill-session` shell-out. Skipped without a
    /// real rmux binary so CI stays green.
    #[tokio::test]
    async fn live_rmux_streams_output_and_completes_on_exit() {
        let _live_guard = live_session_guard().owning("run-stream");
        let probe = live_rmux_probe().await;
        if skip_test_if_missing(
            "live_rmux_streams_output_and_completes_on_exit",
            &[("rmux", probe.found)],
        ) {
            return;
        }
        const SENTINEL: &str = "orgasmic-rmux-line-stream-sentinel";
        let d = driver();
        let cfg = DriverConfig::from_value(json!({
            "command": "sh",
            "args": ["-c", format!("printf '{SENTINEL}\\n'; exit 0")],
        }));
        let mut s = d
            .acquire(ctx("run-stream", RunKind::Worker), cfg)
            .await
            .unwrap();

        let ev = s.events.recv().await.unwrap();
        let DriverEvent::Ready { capabilities, .. } = ev else {
            panic!("expected Ready, got {ev:?}");
        };
        // A degraded acquisition has nothing to stream, so this test would pass
        // without ever exercising the line stream. That is the false green
        // TASK-R2HDN removes: fail instead of returning early.
        if capabilities["inert"] == true {
            s.control.release("cleanup").await.unwrap();
            assert_not_degraded("live_rmux_streams_output_and_completes_on_exit", true);
        }

        let mut saw_text = false;
        let mut saw_complete = false;
        for _ in 0..40 {
            let ev = tokio::time::timeout(Duration::from_secs(5), s.events.recv())
                .await
                .expect("timed out waiting for rmux stream event")
                .expect("event stream closed");
            match ev {
                DriverEvent::TextChunk { .. } => saw_text = true,
                DriverEvent::RunComplete { summary } => {
                    saw_complete = true;
                    assert_eq!(
                        summary.as_deref(),
                        Some("rmux pane exited with code 0"),
                        "process exit should drive completion, not a marker"
                    );
                    break;
                }
                DriverEvent::DriverError { fatal, message } => {
                    panic!("unexpected driver error (fatal={fatal}): {message}");
                }
                other => panic!("unexpected event before completion: {other:?}"),
            }
        }
        assert!(!saw_text, "capture removal must not emit TextChunk");
        assert!(
            saw_complete,
            "expected RunComplete when the pane process exited"
        );
        // release() after natural completion is idempotent (terminal already
        // emitted) and tears the session down via the typed Session::kill.
        s.control.release("cleanup").await.unwrap();
    }

    /// A retained non-zero pane status must become a terminal driver failure
    /// immediately; supervisors must never poll a vanished subprocess.
    #[tokio::test]
    async fn live_rmux_exit_139_emits_a_fatal_terminal_event() {
        let _live_guard = live_session_guard().owning("run-exit-139");
        let probe = live_rmux_probe().await;
        if skip_test_if_missing(
            "live_rmux_exit_139_emits_a_fatal_terminal_event",
            &[("rmux", probe.usable())],
        ) {
            return;
        }
        let d = driver();
        let cfg = DriverConfig::from_value(json!({
            "command": "sh",
            "args": ["-c", "exit 139"],
        }));
        let mut s = d
            .acquire(ctx("run-exit-139", RunKind::Worker), cfg)
            .await
            .unwrap();

        let ev = s.events.recv().await.unwrap();
        assert!(
            matches!(ev, DriverEvent::Ready { .. }),
            "expected Ready, got {ev:?}"
        );
        let ev = tokio::time::timeout(Duration::from_secs(5), s.events.recv())
            .await
            .expect("timed out waiting for rmux exit 139 event")
            .expect("event stream closed");
        match ev {
            DriverEvent::DriverError { fatal, message } => {
                assert!(fatal);
                assert_eq!(message, "rmux pane exited with code 139 (SIGSEGV)");
            }
            other => panic!("expected fatal exit 139 event, got {other:?}"),
        }
        s.control.release("cleanup").await.unwrap();
    }

    /// `orgasmic manager register` (dec_3Y2E1) recognises "I am already
    /// supervised" by reading ORGASMIC_RUN_ID from its own environment —
    /// prove the spawned rmux pane actually has it set, not just that the
    /// spawn plan carries a run id. Skipped without a real rmux binary.
    #[tokio::test]
    async fn live_rmux_session_exports_orgasmic_run_id() {
        let _live_guard = live_session_guard().owning("run-env-export-test");
        let probe = live_rmux_probe().await;
        if skip_test_if_missing(
            "live_rmux_session_exports_orgasmic_run_id",
            &[("rmux", probe.found)],
        ) {
            return;
        }
        let out_dir = tempfile::tempdir().unwrap();
        let out_path = out_dir.path().join("run-id.txt");
        let d = driver();
        let cfg = DriverConfig::from_value(json!({
            "command": "sh",
            "args": ["-c", format!("printf '%s' \"$ORGASMIC_RUN_ID\" > {}", out_path.display())],
        }));
        let mut s = d
            .acquire(ctx("run-env-export-test", RunKind::Worker), cfg)
            .await
            .unwrap();

        let ev = s.events.recv().await.unwrap();
        let DriverEvent::Ready { capabilities, .. } = ev else {
            panic!("expected Ready, got {ev:?}");
        };
        // No live pane means no exported environment to read back; a degraded
        // acquisition must fail this test rather than skip its assertion
        // (TASK-R2HDN).
        if capabilities["inert"] == true {
            s.control.release("cleanup").await.unwrap();
            assert_not_degraded("live_rmux_session_exports_orgasmic_run_id", true);
        }

        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        let mut body = String::new();
        while std::time::Instant::now() < deadline {
            if let Ok(contents) = std::fs::read_to_string(&out_path) {
                if !contents.is_empty() {
                    body = contents;
                    break;
                }
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        assert_eq!(body, "run-env-export-test");
        s.control.release("cleanup").await.unwrap();
    }

    /// Process exit (stream end) emits RunComplete — no TextChunk capture.
    #[tokio::test]
    async fn live_rmux_process_exit_emits_run_complete_without_text_chunks() {
        let _live_guard = live_session_guard().owning("run-exit-only");
        let probe = live_rmux_probe().await;
        if skip_test_if_missing(
            "live_rmux_process_exit_emits_run_complete_without_text_chunks",
            &[("rmux", probe.found)],
        ) {
            return;
        }
        let d = driver();
        let cfg = DriverConfig::from_value(json!({
            "command": "sh",
            "args": ["-c", "printf 'bye\\n'; exit 0"],
        }));
        let mut s = d
            .acquire(ctx("run-exit-only", RunKind::Worker), cfg)
            .await
            .unwrap();
        let ev = s.events.recv().await.unwrap();
        let DriverEvent::Ready { capabilities, .. } = ev else {
            panic!("expected Ready, got {ev:?}");
        };
        // Without a live pane there is no process exit to observe, so this test
        // must fail rather than report success on a degraded session
        // (TASK-R2HDN).
        if capabilities["inert"] == true {
            s.control.release("cleanup").await.unwrap();
            assert_not_degraded(
                "live_rmux_process_exit_emits_run_complete_without_text_chunks",
                true,
            );
        }

        let mut saw_text = false;
        let mut saw_complete = false;
        for _ in 0..40 {
            let ev = tokio::time::timeout(Duration::from_secs(5), s.events.recv())
                .await
                .expect("timed out waiting for rmux exit event")
                .expect("event stream closed");
            match ev {
                DriverEvent::TextChunk { .. } => saw_text = true,
                DriverEvent::RunComplete { summary } => {
                    saw_complete = true;
                    assert_eq!(summary.as_deref(), Some("rmux pane exited with code 0"));
                    break;
                }
                DriverEvent::DriverError { fatal, message } => {
                    panic!("unexpected driver error (fatal={fatal}): {message}");
                }
                other => panic!("unexpected event: {other:?}"),
            }
        }
        assert!(!saw_text, "capture removal must not emit TextChunk");
        assert!(saw_complete, "expected RunComplete on process exit");
        s.control.release("cleanup").await.unwrap();
    }

    /// Persistent hot sessions complete on process exit only (no marker path).
    #[tokio::test]
    async fn live_rmux_persistent_run_completes_on_process_exit() {
        let _live_guard = live_session_guard().owning("run-persistent-exit");
        let probe = live_rmux_probe().await;
        if skip_test_if_missing(
            "live_rmux_persistent_run_completes_on_process_exit",
            &[("rmux", probe.found)],
        ) {
            return;
        }
        let run_id = "run-persistent-exit";
        let d = driver();
        let cfg = DriverConfig::from_value(json!({
            "command": "sh",
            "args": ["-c", "printf 'persistent\n'; exit 0"],
            "prompt_bundle_text": "do the task",
            "persistent": true,
        }));
        let mut s = d.acquire(ctx(run_id, RunKind::Worker), cfg).await.unwrap();
        let ev = s.events.recv().await.unwrap();
        let DriverEvent::Ready { capabilities, .. } = ev else {
            panic!("expected Ready, got {ev:?}");
        };
        if capabilities["inert"] == true {
            s.control.release("cleanup").await.unwrap();
            assert_not_degraded("live_rmux_persistent_run_completes_on_process_exit", true);
        }

        let mut saw_complete = false;
        for _ in 0..40 {
            let ev = tokio::time::timeout(Duration::from_secs(5), s.events.recv())
                .await
                .expect("timed out waiting for rmux exit event")
                .expect("event stream closed");
            match ev {
                DriverEvent::AgentTurnComplete { .. } => {}
                DriverEvent::RunComplete { summary } => {
                    saw_complete = true;
                    assert_eq!(
                        summary.as_deref(),
                        Some("rmux pane exited with code 0"),
                        "persistent run should complete on process exit"
                    );
                    break;
                }
                DriverEvent::DriverError { fatal, message } => {
                    panic!("unexpected driver error (fatal={fatal}): {message}");
                }
                DriverEvent::TextChunk { .. } => {
                    panic!("persistent run must not emit TextChunk after capture removal")
                }
                _ => {}
            }
        }
        assert!(
            saw_complete,
            "expected RunComplete when the persistent run's process exited"
        );
        s.control.release("cleanup").await.unwrap();
    }

    #[tokio::test]
    async fn inert_send_input_returns_unsupported() {
        let d = driver();
        let mut s = d
            .acquire(ctx("run-inert-input", RunKind::Worker), inert_config())
            .await
            .unwrap();
        let _ = s.events.recv().await;
        let err = s
            .control
            .send_input(UserInputRequest {
                input: "followup".into(),
            })
            .await
            .unwrap_err();
        assert!(matches!(err, DriverError::Unsupported("send_input")));
    }

    #[tokio::test]
    async fn wait_for_input_ready_returns_ok_when_mock_pane_has_prompt() {
        let mut ready = false;
        let result = wait_for_input_ready_with_capture(
            Duration::from_secs(1),
            Duration::from_millis(10),
            || {
                ready = true;
                async move {
                    Ok(if ready {
                        "> followup prompt\n".to_string()
                    } else {
                        "booting harness\n".to_string()
                    })
                }
            },
        )
        .await;
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn wait_for_input_ready_returns_input_not_ready_on_timeout() {
        let timeout = Duration::from_millis(50);
        let err = wait_for_input_ready_with_capture(timeout, Duration::from_millis(10), || async {
            Ok("streaming assistant output\n".to_string())
        })
        .await
        .unwrap_err();
        assert!(matches!(err, DriverError::InputNotReady(_)));
    }

    /// Live rmux followup delivery. Drives a minimal interactive harness that
    /// shows a composer prompt, accepts the dispatch brief, then accepts a
    /// followup via `send_input`. Proves the exact followup bytes land in the
    /// pane without any synthetic completion marker.
    /// Skipped without a real rmux binary.
    #[tokio::test]
    async fn live_rmux_send_input_delivers_followup_turn() {
        let _live_guard = live_session_guard().owning("run-send-input");
        let probe = live_rmux_probe().await;
        if skip_test_if_missing(
            "live_rmux_send_input_delivers_followup_turn",
            &[("rmux", probe.found)],
        ) {
            return;
        }
        const INITIAL: &str = "ORGASMIC_INITIAL_SENTINEL";
        const FOLLOWUP: &str = "ORGASMIC_FOLLOWUP_SENTINEL";
        let run_id = "run-send-input";
        let harness =
            "while true; do echo '> ready'; IFS= read -r line || exit 0; echo \"ECHO:$line\"; done";
        let d = driver();
        let cfg = DriverConfig::from_value(json!({
            "command": "sh",
            "args": ["-c", harness],
            "prompt_bundle_text": INITIAL,
            "input_ready_timeout": 5,
        }));
        let mut s = d.acquire(ctx(run_id, RunKind::Worker), cfg).await.unwrap();
        let ev = s.events.recv().await.unwrap();
        let DriverEvent::Ready { capabilities, .. } = ev else {
            panic!("expected Ready, got {ev:?}");
        };
        if capabilities["inert"] == true {
            s.control.release("cleanup").await.unwrap();
            assert_not_degraded("live_rmux_send_input_delivers_followup_turn", true);
        }
        let session_name = rmux_session_name(&s.identity);
        let bin = probe.path.as_deref().unwrap_or(RMUX_BINARY);

        let deadline = std::time::Instant::now() + Duration::from_secs(20);
        let mut pane = String::new();
        while std::time::Instant::now() < deadline {
            pane = rmux_capture_pane(bin, &session_name)
                .await
                .unwrap_or_default();
            let dispatch_done =
                pane.contains("ECHO:run_id:") || pane.contains("ECHO:ORGASMIC_INITIAL");
            if dispatch_done && pane.lines().any(pane_has_input_prompt) {
                break;
            }
            tokio::time::sleep(Duration::from_millis(150)).await;
        }
        assert!(
            pane.contains(INITIAL) && pane.lines().any(pane_has_input_prompt),
            "harness should finish dispatch and show composer prompt, got {pane}"
        );

        let ack = tokio::time::timeout(
            Duration::from_secs(8),
            s.control.send_input(UserInputRequest {
                input: FOLLOWUP.into(),
            }),
        )
        .await
        .expect("send_input timed out")
        .unwrap();
        assert!(ack.accepted, "followup should be accepted when ready");

        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        while std::time::Instant::now() < deadline {
            pane = rmux_capture_pane(bin, &session_name)
                .await
                .unwrap_or_default();
            if pane.contains(FOLLOWUP) {
                break;
            }
            tokio::time::sleep(Duration::from_millis(150)).await;
        }
        assert!(
            pane.contains(FOLLOWUP),
            "followup should land as a user turn, got {pane}"
        );
        s.control.release("cleanup").await.unwrap();
    }

    /// Live rmux mid-turn guard: while the harness is streaming (no input
    /// prompt), `send_input` must reject rather than paste mid-stream.
    /// Skipped without a real rmux binary.
    #[tokio::test]
    async fn live_rmux_send_input_rejects_while_harness_busy() {
        let _live_guard = live_session_guard().owning("run-busy");
        let probe = live_rmux_probe().await;
        if skip_test_if_missing(
            "live_rmux_send_input_rejects_while_harness_busy",
            &[("rmux", probe.found)],
        ) {
            return;
        }
        let d = driver();
        let cfg = DriverConfig::from_value(json!({
            "command": "sh",
            "args": ["-c", "i=0; while [ $i -lt 30 ]; do echo streaming-$i; i=$((i+1)); done"],
            "input_ready_timeout": 1,
        }));
        let mut s = d
            .acquire(ctx("run-busy", RunKind::Worker), cfg)
            .await
            .unwrap();
        let ev = s.events.recv().await.unwrap();
        let DriverEvent::Ready { capabilities, .. } = ev else {
            panic!("expected Ready, got {ev:?}");
        };
        if capabilities["inert"] == true {
            s.control.release("cleanup").await.unwrap();
            assert_not_degraded("live_rmux_send_input_rejects_while_harness_busy", true);
        }

        let ack = tokio::time::timeout(
            Duration::from_secs(5),
            s.control.send_input(UserInputRequest {
                input: "should-not-paste".into(),
            }),
        )
        .await
        .expect("send_input timed out")
        .unwrap();
        assert!(!ack.accepted, "busy harness must reject followup");
        assert_eq!(ack.message.as_deref(), Some("harness busy"));

        s.control.release("cleanup").await.unwrap();
    }
}
