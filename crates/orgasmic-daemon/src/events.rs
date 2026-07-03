// arch: arch_C87Z9.2
// orgasmic:arch_C87Z9, arch_Z3Z3V
//! Daemon-wide event bus.
//!
//! Topics fan out to WebSocket subscribers (board, task, run, manager,
//! graph, daemon) and also let internal modules react to changes without
//! holding the index lock. Backed by `tokio::sync::broadcast` so a slow
//! subscriber drops old events instead of blocking the whole system.

use std::path::PathBuf;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use tokio::sync::broadcast;

const CHANNEL_CAPACITY: usize = 256;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Topic {
    Board,
    Task,
    Run,
    Manager,
    Graph,
    Daemon,
    Artifact,
}

impl Topic {
    pub const ALL: [Topic; 7] = [
        Topic::Board,
        Topic::Task,
        Topic::Run,
        Topic::Manager,
        Topic::Graph,
        Topic::Daemon,
        Topic::Artifact,
    ];

    pub fn parse(s: &str) -> Option<Topic> {
        Some(match s {
            "board" => Topic::Board,
            "task" => Topic::Task,
            "run" => Topic::Run,
            "manager" => Topic::Manager,
            "graph" => Topic::Graph,
            "daemon" => Topic::Daemon,
            "artifact" => Topic::Artifact,
            _ => return None,
        })
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum EventPayload {
    BoardRefreshed,
    ProjectIndexed {
        project_id: String,
    },
    ProjectParseError {
        project_id: Option<String>,
        path: PathBuf,
        message: String,
    },
    TaskUpdated {
        project_id: String,
        task_id: String,
    },
    TxAppended {
        project_id: Option<String>,
        tx_id: String,
        ty: String,
    },
    RunEvent {
        run_id: String,
        seq: u64,
    },
    /// A run crossed a lifecycle boundary (acquire, release, reattach, …).
    /// Distinct from the per-envelope `RunEvent` firehose so UI surfaces that
    /// only care about run liveness (the dock pinning a manager run, agent
    /// lists) can refresh on this without subscribing to every event.
    RunLifecycle {
        run_id: String,
        phase: String,
    },
    ManagerNotice {
        message: String,
    },
    GraphChanged {
        node_id: String,
    },
    GraphNodeCreated {
        project_id: String,
        layer: String,
        node_id: String,
        tx_id: String,
    },
    GraphNodeRevised {
        project_id: String,
        layer: String,
        node_id: String,
        action: String,
        tx_id: String,
    },
    StageRequested {
        stage: String,
        project_id: String,
        task_id: Option<String>,
        run_id: String,
        tx_id: String,
        snapshot_id: Option<String>,
    },
    StageCompleted {
        stage: String,
        project_id: String,
        task_id: String,
        run_id: String,
        tx_id: String,
    },
    StageFailed {
        stage: String,
        project_id: String,
        task_id: String,
        run_id: String,
        tx_id: String,
    },
    DaemonStarted {
        boot_id: String,
    },
    DaemonRestartRequested,
    DaemonHeartbeat,
    ArtifactChanged {
        project_id: String,
        artifact_id: String,
        state: String,
    },
    ArtifactCommentAdded {
        project_id: String,
        artifact_id: String,
        cid: String,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Event {
    pub seq: u64,
    pub time: DateTime<Utc>,
    pub topic: Topic,
    pub payload: EventPayload,
}

#[derive(Debug, Clone)]
pub struct EventBus {
    inner: broadcast::Sender<Event>,
    next_seq: std::sync::Arc<std::sync::atomic::AtomicU64>,
}

impl EventBus {
    pub fn new() -> Self {
        let (tx, _rx) = broadcast::channel(CHANNEL_CAPACITY);
        Self {
            inner: tx,
            next_seq: std::sync::Arc::new(std::sync::atomic::AtomicU64::new(0)),
        }
    }

    pub fn subscribe(&self) -> broadcast::Receiver<Event> {
        self.inner.subscribe()
    }

    pub fn publish(&self, topic: Topic, payload: EventPayload) {
        let seq = self
            .next_seq
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let event = Event {
            seq,
            time: Utc::now(),
            topic,
            payload,
        };
        let _ = self.inner.send(event);
    }
}

impl Default for EventBus {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn publish_round_trip() {
        let bus = EventBus::new();
        let mut rx = bus.subscribe();
        bus.publish(
            Topic::Daemon,
            EventPayload::DaemonStarted {
                boot_id: "test".into(),
            },
        );
        let event = rx.recv().await.unwrap();
        assert_eq!(event.topic, Topic::Daemon);
        assert!(matches!(event.payload, EventPayload::DaemonStarted { .. }));
        assert_eq!(event.seq, 0);
    }

    #[test]
    fn topic_round_trip() {
        for t in Topic::ALL {
            let json = serde_json::to_string(&t).unwrap();
            let s: String = serde_json::from_str(&json).unwrap();
            assert_eq!(Topic::parse(&s).unwrap(), t);
        }
    }

    #[test]
    fn artifact_payloads_round_trip() {
        let changed = EventPayload::ArtifactChanged {
            project_id: "orgasmic".into(),
            artifact_id: "ART-XYZAB".into(),
            state: "submitted".into(),
        };
        let j = serde_json::to_value(&changed).unwrap();
        assert_eq!(j["kind"], "artifact_changed");
        assert_eq!(j["artifact_id"], "ART-XYZAB");

        let added = EventPayload::ArtifactCommentAdded {
            project_id: "orgasmic".into(),
            artifact_id: "ART-XYZAB".into(),
            cid: "CID-abc12345".into(),
        };
        let j = serde_json::to_value(&added).unwrap();
        assert_eq!(j["kind"], "artifact_comment_added");
    }

    #[test]
    fn graph_node_created_payload_round_trip_shape() {
        let payload = EventPayload::GraphNodeCreated {
            project_id: "orgasmic".into(),
            layer: "architecture".into(),
            node_id: "arch_008".into(),
            tx_id: "tx-1".into(),
        };

        let json = serde_json::to_value(&payload).unwrap();
        assert_eq!(json["kind"], "graph_node_created");
        assert_eq!(json["project_id"], "orgasmic");
        assert_eq!(json["layer"], "architecture");
        assert_eq!(json["node_id"], "arch_008");
        assert_eq!(json["tx_id"], "tx-1");

        let decoded: EventPayload = serde_json::from_value(json).unwrap();
        assert!(matches!(
            decoded,
            EventPayload::GraphNodeCreated {
                project_id,
                layer,
                node_id,
                tx_id,
            } if project_id == "orgasmic"
                && layer == "architecture"
                && node_id == "arch_008"
                && tx_id == "tx-1"
        ));
    }

    #[test]
    fn stage_completion_payloads_round_trip_shape() {
        for (payload, kind) in [
            (
                EventPayload::StageCompleted {
                    stage: "grill".into(),
                    project_id: "orgasmic".into(),
                    task_id: "TASK-036".into(),
                    run_id: "run-1".into(),
                    tx_id: "tx-1".into(),
                },
                "stage_completed",
            ),
            (
                EventPayload::StageFailed {
                    stage: "grill".into(),
                    project_id: "orgasmic".into(),
                    task_id: "TASK-036".into(),
                    run_id: "run-1".into(),
                    tx_id: "tx-2".into(),
                },
                "stage_failed",
            ),
        ] {
            let json = serde_json::to_value(&payload).unwrap();
            assert_eq!(json["kind"], kind);
            let decoded: EventPayload = serde_json::from_value(json).unwrap();
            assert!(matches!(
                decoded,
                EventPayload::StageCompleted { .. } | EventPayload::StageFailed { .. }
            ));
        }
    }
}
