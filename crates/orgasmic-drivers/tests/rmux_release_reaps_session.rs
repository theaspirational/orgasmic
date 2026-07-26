use std::fs::OpenOptions;
use std::process::Stdio;
use std::time::Duration;

use orgasmic_core::{DriverEvent, RuntimeIdentity};
use orgasmic_drivers::{
    probe_rmux_binary, DriverConfig, DriverContext, RmuxDriver, RunKind, ShellAdapter, WorkerDriver,
};
use serde_json::json;

fn live_session_guard() -> LiveSessionGuard {
    let path = std::env::temp_dir().join("orgasmic-live-session-tests.lock");
    let file = OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(&path)
        .expect("open live-session lock file");
    fs2::FileExt::lock_exclusive(&file).expect("flock live-session lock");
    LiveSessionGuard(file)
}

struct LiveSessionGuard(std::fs::File);

impl Drop for LiveSessionGuard {
    fn drop(&mut self) {
        let _ = fs2::FileExt::unlock(&self.0);
    }
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
    let _live_guard = live_session_guard();
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
