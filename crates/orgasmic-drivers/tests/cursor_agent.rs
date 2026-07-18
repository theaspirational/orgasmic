use std::process::{Command, Stdio};

use orgasmic_core::{DriverEvent, RuntimeIdentity};
use orgasmic_drivers::{
    driver_for, CursorAgentDriver, DriverConfig, DriverContext, RunKind, WorkerDriver, TRANSPORTS,
};
use serde_json::json;
use tokio::time::{timeout, Duration};

fn ctx(id: &str, kind: RunKind) -> DriverContext {
    DriverContext {
        identity: RuntimeIdentity::new(id, "boot-test"),
        run_kind: kind,
        task_id: "TASK-056".into(),
        worker_id: "implementer-composer".into(),
        project_id: Some("orgasmic".into()),
        worktree: None,
        babysitter_target: None,
    }
}

fn cursor_agent_available() -> bool {
    Command::new("which")
        .arg("cursor-agent")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map(|status| status.success())
        .unwrap_or(false)
}

/// Drop-guard that reaps the cursor-agent process *group* on every exit path
/// (success, assert-failure unwinding, or panic). The driver spawns cursor-agent
/// with `setsid` (its pid is the pgid), and cursor-agent in turn forks a node
/// `worker-server` child. `release` only kills the direct child, so the
/// worker-server is orphaned (TASK-104.2). Killing the whole group via
/// `kill -- -<pgid>` reaps the subtree unconditionally; null stdio so the kill
/// process itself does not hold a piped test-runner fd open. The kill is a no-op
/// once the group has already exited.
struct ProcessGroupGuard(Option<u32>);

impl Drop for ProcessGroupGuard {
    fn drop(&mut self) {
        if let Some(pid) = self.0 {
            let _ = Command::new("kill")
                .arg("--")
                .arg(format!("-{pid}"))
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .status();
        }
    }
}

#[test]
fn registry_covers_every_transport() {
    for transport in TRANSPORTS {
        let driver = driver_for(transport).expect("known transport");
        assert_eq!(driver.transport(), *transport);
    }
    assert!(TRANSPORTS.contains(&"cursor-agent"));
    assert!(driver_for("unknown").is_none());
}

#[tokio::test]
async fn simulated_acquire_emits_deterministic_flow() {
    let driver = CursorAgentDriver;
    let mut session = driver
        .acquire(
            ctx("run-cursor-sim", RunKind::Worker),
            DriverConfig::empty(),
        )
        .await
        .unwrap();

    let ready = session.events.recv().await.unwrap();
    let DriverEvent::Ready { capabilities, .. } = ready else {
        panic!("expected Ready");
    };
    assert_eq!(capabilities["simulated"], true);
    assert_eq!(capabilities["model"], "composer-2.5-fast");

    let system = session.events.recv().await.unwrap();
    assert!(
        matches!(system, DriverEvent::TextChunk { chunk, .. } if chunk.contains("cursor-agent simulated run"))
    );
    let assistant = session.events.recv().await.unwrap();
    assert!(
        matches!(assistant, DriverEvent::TextChunk { chunk, .. } if chunk == "cursor-agent simulated response")
    );
    let complete = session.events.recv().await.unwrap();
    assert!(matches!(complete, DriverEvent::RunComplete { .. }));
}

#[test]
fn missing_api_key_with_endpoint_fails_validation() {
    let driver = CursorAgentDriver;
    std::env::remove_var("ORGASMIC_TEST_MISSING_CURSOR_KEY");
    let cfg = DriverConfig::from_value(json!({
        "endpoint": "stdio",
        "api_key_env": "ORGASMIC_TEST_MISSING_CURSOR_KEY"
    }));
    assert!(driver.validate(&cfg).is_err());
}

#[test]
fn unsupported_endpoint_fails_validation() {
    let driver = CursorAgentDriver;
    let cfg = DriverConfig::from_value(json!({
        "endpoint": "http://nowhere"
    }));
    let err = driver.validate(&cfg).unwrap_err();
    assert!(err
        .to_string()
        .contains("endpoint must be 'stdio' or empty"));
}

/// Live smoke against the real `cursor-agent` CLI. Double-gated so it never runs
/// (and never flakes on auth/network) by accident: it requires BOTH an explicit
/// opt-in env var `ORGASMIC_LIVE_CURSOR_SMOKE=1` AND `cursor-agent` on PATH. The
/// env gate exists because a host can have cursor-agent installed but not
/// authenticated, which otherwise makes this test time out environmentally in
/// CI (TASK-104.2). Run it deliberately with:
///   ORGASMIC_LIVE_CURSOR_SMOKE=1 cargo test -p orgasmic-drivers real_cursor_agent
#[tokio::test]
async fn real_cursor_agent_stream_json_bridge_emits_events_and_releases() {
    if std::env::var("ORGASMIC_LIVE_CURSOR_SMOKE").as_deref() != Ok("1") {
        eprintln!(
            "skipping real_cursor_agent_stream_json_bridge_emits_events_and_releases: \
             set ORGASMIC_LIVE_CURSOR_SMOKE=1 to opt in"
        );
        return;
    }
    if !cursor_agent_available() {
        eprintln!(
            "skipping real_cursor_agent_stream_json_bridge_emits_events_and_releases: cursor-agent not on PATH"
        );
        return;
    }

    let driver = CursorAgentDriver;
    let cfg = DriverConfig::from_value(json!({
        "endpoint": "stdio",
        "model": "composer-2.5-fast",
        "sandbox": "enabled",
        "force": true,
        "prompt_bundle_text": "Reply exactly ORGASMIC_CURSOR_DRIVER_OK. Do not run commands."
    }));
    let mut session = driver
        .acquire(ctx("run-real-cursor", RunKind::Worker), cfg)
        .await
        .unwrap();
    // Reap the cursor-agent process group (incl. the forked worker-server child)
    // on every exit path, including the assert-failure/panic unwinds below.
    let _reap = ProcessGroupGuard(session.pid);

    let mut saw_ready = false;
    let mut saw_progress = false;
    let mut terminal_count = 0;
    for _ in 0..20 {
        let ev = timeout(Duration::from_secs(15), session.events.recv())
            .await
            .expect("real cursor-agent smoke timed out")
            .expect("event stream closed before cursor-agent completed");
        match ev {
            DriverEvent::Ready { capabilities, .. } => {
                saw_ready = capabilities["simulated"] == false;
            }
            DriverEvent::TextChunk { .. } | DriverEvent::ToolCall { .. } => {
                saw_progress = true;
            }
            DriverEvent::RunComplete { .. } => {
                terminal_count += 1;
            }
            DriverEvent::RunFail {
                error_code,
                error_markdown,
            } => {
                panic!("cursor-agent run failed {error_code}: {error_markdown}");
            }
            DriverEvent::DriverError { fatal, message } => {
                panic!("cursor-agent driver error fatal={fatal}: {message}");
            }
            _ => {}
        }
        if saw_ready && saw_progress && terminal_count > 0 {
            break;
        }
    }

    session.control.release("test cleanup").await.unwrap();
    session.control.release("test cleanup").await.unwrap();
    loop {
        match timeout(Duration::from_secs(5), session.events.recv()).await {
            Ok(Some(DriverEvent::RunComplete { .. } | DriverEvent::RunFail { .. })) => {
                terminal_count += 1;
            }
            Ok(Some(DriverEvent::DriverError { fatal, message })) => {
                panic!("cursor-agent driver error after terminal fatal={fatal}: {message}");
            }
            Ok(Some(_)) => {}
            Ok(None) => break,
            Err(_) => panic!("cursor-agent stream did not close after release"),
        }
    }
    assert!(saw_ready, "cursor-agent should emit non-simulated Ready");
    assert!(saw_progress, "cursor-agent should emit progress");
    assert_eq!(
        terminal_count, 1,
        "cursor-agent should emit exactly one terminal result"
    );
}
