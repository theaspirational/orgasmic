use std::process::Stdio;
use std::time::Duration;

use orgasmic_core::{DriverEvent, RuntimeIdentity};
use orgasmic_drivers::modes::rmux::test_tooling::live_session_guard;
use orgasmic_drivers::{
    probe_rmux_binary, DriverConfig, DriverContext, RmuxDriver, RunKind, ShellAdapter, WorkerDriver,
};
use serde_json::json;

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
    // Opt-in gate lane: unlike the ordinary developer smoke, this must fail
    // closed when its declared rmux prerequisite is absent.
    let rmux_required = std::env::var("ORGASMIC_REQUIRE_LIVE_RMUX").as_deref() == Ok("1");
    let probe = probe_rmux_binary();
    if !probe.found || !probe.compatible {
        assert!(
            !rmux_required,
            "ORGASMIC_REQUIRE_LIVE_RMUX=1 but compatible rmux is unavailable ({:?})",
            probe.version_error
        );
        eprintln!(
            "SKIPPED release_reaps_live_rmux_session: compatible rmux unavailable ({:?})",
            probe.version_error
        );
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
    if capabilities["inert"] == true {
        assert!(
            !rmux_required,
            "ORGASMIC_REQUIRE_LIVE_RMUX=1 but acquire was inert ({})",
            capabilities["inert_reason"]
        );
        eprintln!(
            "SKIPPED release_reaps_live_rmux_session: rmux daemon unavailable ({})",
            capabilities["inert_reason"]
        );
        return;
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
    let probe = probe_rmux_binary();
    if !probe.found || !probe.compatible {
        eprintln!(
            "SKIPPED live_session_guard_reaps_registered_session_when_the_body_panics: \
             compatible rmux unavailable ({:?})",
            probe.version_error
        );
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
