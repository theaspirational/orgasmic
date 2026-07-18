use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use async_trait::async_trait;
use orgasmic_core::{read_session_file, DriverEvent, SessionEventKind, TextStream};
use orgasmic_daemon::supervisor::{AcquireRequest, Supervisor};
use orgasmic_daemon::{spawn_writer, BootIdentity, EventBus};
use orgasmic_drivers::{
    BabysitterAck, BabysitterRequest, DriverConfig, DriverContext, DriverControl, DriverError,
    DriverSession, RunKind, TransitionAck, TransitionRequest, WorkerDriver,
};
use serde_json::json;
use tokio::sync::mpsc;

struct MockDriver {
    transport: &'static str,
    events: Mutex<Option<mpsc::Receiver<DriverEvent>>>,
    control_events: Mutex<Option<mpsc::Sender<DriverEvent>>>,
    releases: Arc<Mutex<Vec<String>>>,
}

impl MockDriver {
    fn new(
        transport: &'static str,
        events: mpsc::Receiver<DriverEvent>,
        control_events: mpsc::Sender<DriverEvent>,
    ) -> (Self, Arc<Mutex<Vec<String>>>) {
        let releases = Arc::new(Mutex::new(Vec::new()));
        (
            Self {
                transport,
                events: Mutex::new(Some(events)),
                control_events: Mutex::new(Some(control_events)),
                releases: releases.clone(),
            },
            releases,
        )
    }
}

#[async_trait]
impl WorkerDriver for MockDriver {
    fn transport(&self) -> &'static str {
        self.transport
    }

    fn harness(&self) -> Option<&'static str> {
        Some("mock")
    }

    async fn acquire(
        &self,
        ctx: DriverContext,
        _config: DriverConfig,
    ) -> Result<DriverSession, DriverError> {
        let events = self
            .events
            .lock()
            .unwrap()
            .take()
            .expect("mock driver acquired once");
        let control_events = self
            .control_events
            .lock()
            .unwrap()
            .take()
            .expect("mock control acquired once");
        Ok(DriverSession {
            identity: ctx.identity,
            pid: None,
            events,
            control: Box::new(MockControl {
                events: Some(control_events),
                releases: self.releases.clone(),
            }),
            native_runtime: None,
        })
    }
}

struct MockControl {
    events: Option<mpsc::Sender<DriverEvent>>,
    releases: Arc<Mutex<Vec<String>>>,
}

#[async_trait]
impl DriverControl for MockControl {
    async fn transition_state(
        &mut self,
        _req: TransitionRequest,
    ) -> Result<TransitionAck, DriverError> {
        Ok(TransitionAck {
            accepted: true,
            message: None,
        })
    }

    async fn babysitter_action(
        &mut self,
        _req: BabysitterRequest,
    ) -> Result<BabysitterAck, DriverError> {
        Ok(BabysitterAck {
            accepted: true,
            message: None,
        })
    }

    async fn release(&mut self, reason: &str) -> Result<(), DriverError> {
        self.releases.lock().unwrap().push(reason.to_string());
        self.events.take();
        Ok(())
    }
}

fn make_supervisor() -> (Supervisor, tempfile::TempDir) {
    let dir = tempfile::tempdir().unwrap();
    let writer = spawn_writer(EventBus::new());
    let boot = Arc::new(BootIdentity::new());
    (Supervisor::new(writer, boot), dir)
}

fn request(
    task_id: &str,
    session_path: PathBuf,
    stall_timeout_secs: Option<u32>,
) -> AcquireRequest {
    AcquireRequest {
        task_id: task_id.to_string(),
        kind: RunKind::Worker,
        worker_id: "implementer-rmux-mock".into(),
        role: "implementer".into(),
        project_id: Some("orgasmic".into()),
        worktree: None,
        last_path: None,
        stdout_path: None,
        session_path,
        driver_config: DriverConfig::empty(),
        babysitter_target: None,
        stall_timeout_secs,
        max_run_duration_secs: Some(0),
        idle_timeout_secs: None,
        babysitter: None,
        planned_identity: None,
    }
}

async fn wait_until_released(supervisor: &Supervisor, run_id: &str, timeout: Duration) {
    let deadline = Instant::now() + timeout;
    loop {
        let snapshot = supervisor.snapshot().await;
        if snapshot.runs.iter().all(|run| run.run_id != run_id) {
            return;
        }
        assert!(
            Instant::now() < deadline,
            "run {run_id} still live after {timeout:?}"
        );
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
}

async fn wait_for_release_event(path: &Path, timeout: Duration) -> serde_json::Value {
    let deadline = Instant::now() + timeout;
    loop {
        let envelopes = read_session_file(path).unwrap_or_default();
        if let Some(event) = envelopes.into_iter().find_map(|envelope| {
            (envelope.kind == SessionEventKind::Lifecycle
                && envelope.event.get("phase").and_then(|value| value.as_str()) == Some("release"))
            .then_some(envelope.event)
        }) {
            return event;
        }
        assert!(
            Instant::now() < deadline,
            "release event not written to {}",
            path.display()
        );
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
}

#[tokio::test]
async fn rmux_run_complete_releases_promptly_with_completion_reason() {
    let (supervisor, dir) = make_supervisor();
    let session_path = dir.path().join("rmux-complete.jsonl");
    let (tx, rx) = mpsc::channel(8);
    let (driver, releases) = MockDriver::new("rmux", rx, tx.clone());

    let resp = supervisor
        .acquire(
            &driver,
            request("TASK-RMUX-COMPLETE", session_path.clone(), Some(600)),
        )
        .await
        .expect("acquire");

    tx.send(DriverEvent::TextChunk {
        stream: TextStream::Assistant,
        chunk: "final worker summary".into(),
        seq: 1,
    })
    .await
    .unwrap();
    tx.send(DriverEvent::RunComplete {
        summary: Some("final worker summary".into()),
    })
    .await
    .unwrap();
    drop(tx);

    wait_until_released(&supervisor, &resp.run_id, Duration::from_secs(2)).await;
    let release = wait_for_release_event(&session_path, Duration::from_secs(2)).await;

    assert_eq!(release.get("outcome"), Some(&json!("completed")));
    assert_eq!(release.get("reason"), Some(&json!("driver terminal event")));
    assert_ne!(
        release.get("reason"),
        Some(&json!("stall_timeout_exceeded"))
    );
    assert_eq!(
        releases.lock().unwrap().as_slice(),
        &["driver terminal event".to_string()]
    );
}

#[tokio::test]
async fn stall_detector_still_releases_non_completed_rmux_runs() {
    let (supervisor, dir) = make_supervisor();
    let session_path = dir.path().join("rmux-stalled.jsonl");
    let (tx, rx) = mpsc::channel(8);
    let (driver, releases) = MockDriver::new("rmux", rx, tx.clone());

    let resp = supervisor
        .acquire(
            &driver,
            request("TASK-RMUX-STALLED", session_path.clone(), Some(1)),
        )
        .await
        .expect("acquire");
    drop(tx);

    wait_until_released(&supervisor, &resp.run_id, Duration::from_secs(4)).await;
    let release = wait_for_release_event(&session_path, Duration::from_secs(7)).await;

    assert_eq!(release.get("outcome"), Some(&json!("failed")));
    assert_eq!(
        release.get("reason"),
        Some(&json!("stall_timeout_exceeded"))
    );
    assert_eq!(
        releases.lock().unwrap().as_slice(),
        &["stall_timeout_exceeded".to_string()]
    );
}
