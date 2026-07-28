use std::process::Stdio;
use std::time::Duration;

use orgasmic_core::{DriverEvent, RuntimeIdentity};
use orgasmic_drivers::modes::rmux::test_tooling::{
    assert_not_degraded, assert_required_test_tooling, live_session_guard, skip_test_if_missing,
    test_environment_lock, StallableRmuxEndpoint, ToolRequirement,
};
use orgasmic_drivers::{
    probe_rmux_binary, DriverConfig, DriverContext, RmuxBinaryProbe, RmuxDriver, RunKind,
    ShellAdapter, WorkerDriver,
};
use serde_json::json;

// orgasmic:task_R2HDN,task_69CW6
/// How many tests in this binary cannot run without a usable `rmux`. Every live
/// test below is gated; the sentinel itself is not. Adding another gated test
/// to this file means bumping this number, nothing else.
///
/// This sentinel *is* the declared fail-closed lane for absent rmux (TASK-69CW6
/// direction 2). It runs by default in `cargo test -p orgasmic-drivers`, needs
/// no environment variable to arm, and fails — rather than skips — when rmux is
/// missing. The previous `ORGASMIC_REQUIRE_LIVE_RMUX` opt-in was deleted rather
/// than wired: set nowhere in the tree, it made the lane look covered while
/// never running.
const RMUX_GATED_TESTS: usize = 3;

/// Probe `rmux` under the shared environment lock, so a test that mutates
/// process-global `PATH` can never make this binary see a missing tool.
/// `blocking_lock` panics inside a runtime, hence the two spellings.
fn probe_rmux_under_environment_lock() -> RmuxBinaryProbe {
    let _environment = test_environment_lock().blocking_lock();
    probe_rmux_binary()
}

async fn probe_rmux_under_environment_lock_async() -> RmuxBinaryProbe {
    let _environment = test_environment_lock().lock().await;
    probe_rmux_binary()
}

// orgasmic:task_R2HDN
/// The one sentinel this default-running binary was missing. Without it, a host
/// with no `rmux` produced a clean pass from two tests that printed `SKIPPED`
/// and returned — the exact false green TASK-RRT4T set out to remove.
#[test]
fn required_test_tooling_is_present() {
    assert_required_test_tooling(&[ToolRequirement::new(
        "rmux",
        RMUX_GATED_TESTS,
        probe_rmux_under_environment_lock().usable(),
    )]);
}

struct SessionGuard {
    rmux_bin: String,
    session: String,
}

impl Drop for SessionGuard {
    fn drop(&mut self) {
        let _ = std::process::Command::new(&self.rmux_bin)
            .args(["kill-session", "-t", &self.session])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();
    }
}

async fn rmux_session_exists(rmux_bin: &str, session: &str) -> Result<bool, String> {
    let mut command = tokio::process::Command::new(rmux_bin);
    command
        .args(["has-session", "-t", session])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .kill_on_drop(true);
    tokio::time::timeout(Duration::from_secs(5), command.status())
        .await
        .map_err(|_| format!("rmux has-session timed out for {session}"))?
        .map(|status| status.success())
        .map_err(|error| format!("rmux has-session failed for {session}: {error}"))
}

#[tokio::test]
async fn release_reaps_live_rmux_session() {
    let _live_guard =
        live_session_guard().owning(format!("run-release-reap-test-{}", std::process::id()));
    let probe = probe_rmux_under_environment_lock_async().await;
    if skip_test_if_missing(
        "release_reaps_live_rmux_session",
        &[("rmux", probe.usable())],
    ) {
        // `skip_test_if_missing` has printed the per-test diagnostic for
        // `--nocapture`; `required_test_tooling_is_present` is what fails the
        // binary, so this return can no longer be a silent green.
        return;
    }
    let rmux_bin = probe.path.expect("found rmux probe reports its path");
    let identity = RuntimeIdentity::new(
        format!("run-release-reap-test-{}", std::process::id()),
        "boot-release-reap-test",
    );
    let ctx = DriverContext {
        identity,
        run_kind: RunKind::Worker,
        task_id: "TASK-8W30B".into(),
        worker_id: "rmux-release-regression".into(),
        project_id: Some("orgasmic".into()),
        worktree: None,
        babysitter_target: None,
    };
    let driver = RmuxDriver::new(Box::new(ShellAdapter::new()));
    let config = DriverConfig::from_value(json!({
        "command": "sh",
        // Keep the lifecycle stream issuing ordered cursor requests instead of
        // sitting in its empty-stream backoff. The old abort-before-kill
        // ordering then cancels the shared SDK transport.
        "args": ["-c", "while :; do printf 'release-reap\\n'; done"],
    }));
    let mut driver_session = driver.acquire(ctx, config).await.expect("acquire rmux run");
    let ready = tokio::time::timeout(Duration::from_secs(10), driver_session.events.recv())
        .await
        .expect("timed out waiting for rmux Ready")
        .expect("rmux event stream closed before Ready");
    let DriverEvent::Ready { capabilities, .. } = ready else {
        panic!("expected rmux Ready, got {ready:?}");
    };
    // A usable rmux binary is not an acquired session: `run_live_session`
    // converts every SDK/daemon startup error into an inert `Ready`. There is
    // no session to release or reap on that path, so reporting success would be
    // a false green — fail unconditionally, not only under the opt-in gate
    // (TASK-R2HDN).
    if capabilities["inert"] == true {
        assert_not_degraded("release_reaps_live_rmux_session", true);
    }
    let session = capabilities["session"]
        .as_str()
        .expect("live rmux Ready reports a session")
        .to_string();
    let _session_guard = SessionGuard {
        rmux_bin: rmux_bin.clone(),
        session: session.clone(),
    };
    assert!(
        rmux_session_exists(&rmux_bin, &session)
            .await
            .expect("probe live rmux session before release"),
        "rmux session was not live before release"
    );
    tokio::time::sleep(Duration::from_millis(100)).await;

    driver_session
        .control
        .release("regression cleanup")
        .await
        .expect("release must reap the rmux session");

    assert!(
        !rmux_session_exists(&rmux_bin, &session)
            .await
            .expect("probe rmux session after release"),
        "rmux session survived explicit release: {session}"
    );
}

// orgasmic:task_69CW6
/// The explicit-release cancellation boundary, and the only regression that
/// distinguishes the TASK-6FNAY fix from the bug it replaced.
///
/// `RmuxControl::release` borrows `self.session` across the cancellable reap and
/// only `take()`s it afterwards. Restore the original `take()`-before-`await`
/// and nothing observable changes on the happy path — the reap still runs, the
/// session still dies. It changes exactly one thing: an *aborted* release hands
/// the sole SDK handle to a future nobody is polling, so the `Drop` backstop
/// finds `None` and the session outlives the run.
///
/// The supervisor lane cannot reach this. Its 5s budget deliberately exceeds
/// the 2s + 2s reap budget, so `Supervisor::release` never cancels a release
/// that is merely slow; only an external abort does. This test is that abort.
#[tokio::test]
async fn a_cancelled_release_still_reaps_through_the_drop_backstop() {
    const TEST: &str = "a_cancelled_release_still_reaps_through_the_drop_backstop";
    // Lock order is flock-then-environment everywhere in this binary; the
    // fixture mutates process-global rmux discovery, so both are held for the
    // whole body rather than just the probe. The probe below is therefore the
    // bare one — `probe_rmux_under_environment_lock_async` would deadlock on a
    // lock this test already holds.
    let _live_guard = live_session_guard();
    let _environment = test_environment_lock().lock().await;
    if skip_test_if_missing(TEST, &[("rmux", probe_rmux_binary().usable())]) {
        return;
    }
    let endpoint = StallableRmuxEndpoint::start()
        .await
        .expect("private stallable rmux endpoint");

    let ctx = DriverContext {
        identity: RuntimeIdentity::new(
            format!("run-release-cancel-{}", std::process::id()),
            "boot-release-cancel-test",
        ),
        run_kind: RunKind::Worker,
        task_id: "TASK-69CW6".into(),
        worker_id: "rmux-release-cancellation".into(),
        project_id: Some("orgasmic".into()),
        worktree: None,
        babysitter_target: None,
    };
    let driver = RmuxDriver::new(Box::new(ShellAdapter::new()));
    let config = DriverConfig::from_value(json!({
        "command": "sh",
        "args": ["-c", "while :; do printf 'release-cancel\\n'; sleep 0.05; done"],
    }));
    let mut driver_session = driver.acquire(ctx, config).await.expect("acquire rmux run");
    let ready = tokio::time::timeout(Duration::from_secs(20), driver_session.events.recv())
        .await
        .expect("timed out waiting for rmux Ready")
        .expect("rmux event stream closed before Ready");
    let DriverEvent::Ready { capabilities, .. } = ready else {
        panic!("expected rmux Ready, got {ready:?}");
    };
    assert_not_degraded(TEST, capabilities["inert"] == true);
    let session = capabilities["session"]
        .as_str()
        .expect("live rmux Ready reports a session")
        .to_string();
    assert!(
        endpoint.session_exists(&session),
        "rmux session was not live before the cancelled release"
    );

    // From here the SDK's ordered transport answers nothing, so `release` parks
    // inside its own 2s SDK budget with the session handle borrowed.
    endpoint.stall_sdk_transport();
    let mut control = driver_session.control;
    let cancelled = tokio::time::timeout(
        Duration::from_millis(300),
        control.release("externally aborted release"),
    )
    .await;
    assert!(
        cancelled.is_err(),
        "the stalled SDK kill must still be in flight when the caller aborts"
    );
    drop(control);

    // `Drop` re-runs the whole reap: the SDK half stalls again, then the
    // endpoint-exact CLI fallback opens a fresh connection and reaps for real.
    let deadline = std::time::Instant::now() + Duration::from_secs(15);
    while endpoint.session_exists(&session) {
        assert!(
            std::time::Instant::now() < deadline,
            "session {session} survived a cancelled release: the Drop backstop had no handle"
        );
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
}

fn rmux_session_exists_blocking(rmux_bin: &str, session: &str) -> bool {
    std::process::Command::new(rmux_bin)
        .args(["has-session", "-t", session])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_ok_and(|status| status.success())
}

// orgasmic:task_Z3093
/// The acceptance criterion for TASK-Z3093, and the only one that reproduces
/// the bug: a live-session test that panics *above* its trailing cleanup must
/// still leave no rmux session behind.
///
/// Before the fix this was the production leak path. `LiveSessionGuard::drop`
/// unlocked a flock and returned, and every reap was a trailing statement in
/// the test body, so any panic — including the load-induced TASK-STWVB
/// failures — skipped it and orphaned a session, its pty and its harness
/// process (observed twice on 2026-07-28 at ages 3h18m and 2h46m).
///
/// Deliberately synchronous: the guard's reap must not depend on a tokio
/// runtime, and this proves it on the plainest possible path.
#[test]
fn live_session_guard_reaps_registered_session_when_the_body_panics() {
    let probe = probe_rmux_under_environment_lock();
    if skip_test_if_missing(
        "live_session_guard_reaps_registered_session_when_the_body_panics",
        &[("rmux", probe.usable())],
    ) {
        return;
    }
    let rmux_bin = probe.path.expect("found rmux probe reports its path");
    // Match the production naming scheme so the guard's run-scoped reap
    // (`orgasmic-rmux-<run_id>-*`) is what gets exercised, not an exact-name
    // shortcut. Process-scoped so concurrent binaries cannot collide.
    let run_id = format!("run-guard-panic-{}", std::process::id());
    let session = format!("orgasmic-rmux-{run_id}-runtime-panic");

    let unwound = std::panic::catch_unwind(|| {
        let _live_guard = live_session_guard().owning(&run_id);
        let created = std::process::Command::new(&rmux_bin)
            .args(["new-session", "-d", "-s", &session, "sh", "-c", "sleep 600"])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .expect("spawn rmux new-session");
        assert!(created.success(), "rmux new-session failed for {session}");
        assert!(
            rmux_session_exists_blocking(&rmux_bin, &session),
            "fixture session should be live before the panic"
        );
        // Stands in for any assertion that fails mid-body. Everything below it
        // in a real test — including the trailing release — never runs.
        panic!("deliberate mid-body panic inside the live-session guard's scope");
    });

    assert!(unwound.is_err(), "the fixture panic must have propagated");
    let leaked = rmux_session_exists_blocking(&rmux_bin, &session);
    if leaked {
        // Best effort so a failing assertion does not itself leak the fixture.
        let _ = std::process::Command::new(&rmux_bin)
            .args(["kill-session", "-t", &session])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();
    }
    assert!(
        !leaked,
        "session {session} survived a panic inside the guard's scope: the guard did not reap"
    );
}
