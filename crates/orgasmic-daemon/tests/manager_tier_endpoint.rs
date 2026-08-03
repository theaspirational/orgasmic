//! `POST`/`GET /api/manager/tier` — the manager's recorded process tier
//! (TASK-3CM0Q).
//!
//! `shipped/workflows/default.org` computes the tier from four countable
//! triggers and tells the manager to do so before its first source edit. That
//! is a *reading* obligation, and the failure this endpoint exists to close is
//! an agent under execution momentum skimming one: ~900 lines of P0 daemon
//! recovery code landed with no tier ever classified and no trace of the
//! omission.
//!
//! What is pinned here is the writing obligation, and specifically the two
//! properties the task's acceptance names:
//!
//! - the declaration is on the append-only ledger, so the *absence* of one is
//!   detectable after the run rather than merely unnoticed during it;
//! - the triggers travel with the tier, so a reader can check the arithmetic
//!   instead of taking an asserted tier on trust.
//!
//! Plus the cheapness property, because a discipline that costs a round-trip is
//! a discipline that gets skipped: `trivial` is one call carrying nothing but
//! the task and the tier.

use std::path::Path;

use orgasmic_core::Home;
use orgasmic_daemon::{Daemon, DaemonOptions, RunningDaemon};
use serde_json::Value;

fn test_options() -> DaemonOptions {
    DaemonOptions {
        bind_override: Some("127.0.0.1".parse().unwrap()),
        port_override: Some(0),
        fs_watcher_enabled: false,
        ..DaemonOptions::default()
    }
}

fn write(path: &Path, contents: impl AsRef<str>) {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).unwrap();
    }
    std::fs::write(path, contents.as_ref()).unwrap();
}

fn seed_board(home: &Home, project_root: &Path, project_id: &str) {
    write(
        &project_root.join(".orgasmic/project.org"),
        format!(
            "#+title: {project_id}\n#+orgasmic_version: 1\n\n* PROJECT {project_id}\n:PROPERTIES:\n:ID:               {project_id}\n:END:\n"
        ),
    );
    write(
        &project_root.join(".orgasmic/tasks/todo.org"),
        "#+title: todo\n#+orgasmic_version: 1\n\n* TODO TASK-TIER Declare a tier :work:\n:PROPERTIES:\n:ID:               TASK-TIER\n:END:\n",
    );
    write(
        &home.board(),
        format!(
            "#+title: orgasmic board\n#+orgasmic_version: 1\n\n* PROJECT {project_id}\n:PROPERTIES:\n:ID:               {project_id}\n:PATH:             {}\n:BRANCH:           main\n:STATUS:           active\n:END:\n",
            project_root.display()
        ),
    );
}

/// The token is generated during boot, not by `ensure()`, so this is read after
/// `Daemon::run` and tolerates the file appearing a moment late.
fn read_token(home: &Home) -> String {
    for _ in 0..40 {
        if let Ok(token) = std::fs::read_to_string(home.auth_token()) {
            let token = token.trim().to_string();
            if !token.is_empty() {
                return token;
            }
        }
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
    panic!("daemon never wrote an auth token");
}

struct Fixture {
    _tmp: tempfile::TempDir,
    running: RunningDaemon,
    token: String,
    client: reqwest::Client,
}

impl Fixture {
    async fn boot() -> Self {
        let tmp = tempfile::tempdir().unwrap();
        let home = Home::at(tmp.path().join("home"));
        home.ensure().unwrap();
        write(
            &home.config(),
            "bind_host: 127.0.0.1\nbind_port: 4848\nmanager:\n  actor: manager@example.com\n",
        );
        seed_board(&home, &tmp.path().join("proj"), "proj");
        let running = Daemon::run(home.clone(), test_options())
            .await
            .expect("boot daemon");
        let token = read_token(&home);
        Self {
            _tmp: tmp,
            running,
            token,
            client: reqwest::Client::new(),
        }
    }

    async fn declare(&self, body: Value) -> (reqwest::StatusCode, Value) {
        let resp = self
            .client
            .post(format!("http://{}/api/manager/tier", self.running.addr))
            .bearer_auth(&self.token)
            .json(&body)
            .send()
            .await
            .unwrap();
        let status = resp.status();
        (status, resp.json().await.unwrap_or(Value::Null))
    }

    async fn read(&self, task: &str) -> Value {
        let resp = self
            .client
            .get(format!(
                "http://{}/api/manager/tier?project=proj&task={task}",
                self.running.addr
            ))
            .bearer_auth(&self.token)
            .send()
            .await
            .unwrap();
        assert!(resp.status().is_success(), "read tier: {}", resp.status());
        resp.json().await.unwrap()
    }

    /// Every `manager.tier` entry the ledger holds, as the `/api/tx` reader
    /// sees them. Read through the API rather than off disk on purpose: the
    /// acceptance is that a *reader* can detect the omission, and this is the
    /// surface a reader has.
    async fn ledger(&self) -> Vec<Value> {
        let resp = self
            .client
            .get(format!(
                "http://{}/api/tx?project=proj&limit=50",
                self.running.addr
            ))
            .bearer_auth(&self.token)
            .send()
            .await
            .unwrap();
        assert!(resp.status().is_success(), "read tx: {}", resp.status());
        let list: Value = resp.json().await.unwrap();
        list.as_array()
            .unwrap()
            .iter()
            .filter(|item| item["entry"]["ty"] == "manager.tier")
            .cloned()
            .collect()
    }

    async fn shutdown(self) {
        let _ = self.running.shutdown.send(());
        let _ = self.running.join.await;
    }
}

/// The whole point, in one test: an undeclared task reads as undeclared, a
/// declaration costs one call, and what lands on the ledger carries the
/// arithmetic.
#[tokio::test]
async fn a_trivial_declaration_costs_one_call_and_lands_on_the_ledger() {
    let fx = Fixture::boot().await;

    // Before anything: the out-of-policy state, and it is visible as such.
    let before = fx.read("TASK-TIER").await;
    assert_eq!(before["declared"], false);
    assert!(before["current"].is_null());
    assert_eq!(before["declarations"], 0);
    assert!(
        fx.ledger().await.is_empty(),
        "nothing declared yet, so the ledger holds no manager.tier"
    );

    // One call. No triggers, no reason, no round-trip: this is the cheap path,
    // and a discipline that costs more than this is one that gets skipped.
    let (status, body) = fx
        .declare(serde_json::json!({
            "project": "proj",
            "task": "TASK-TIER",
            "tier": "trivial",
        }))
        .await;
    assert!(status.is_success(), "declare trivial: {status} {body}");
    assert_eq!(body["status"], "declared");
    assert_eq!(body["tier"], "trivial");
    assert!(body["previous_tier"].is_null());
    assert_eq!(body["lowered"], false);
    let tx_id = body["tx_id"].as_str().unwrap().to_string();

    let after = fx.read("TASK-TIER").await;
    assert_eq!(after["declared"], true);
    assert_eq!(after["current"]["tier"], "trivial");
    assert_eq!(after["current"]["tx_id"], tx_id.as_str());
    assert_eq!(after["declarations"], 1);

    // And it is on the ledger, not in a cache: same entry, same tx id, with the
    // tier and the (empty) trigger set carried as properties a reader can read.
    let ledger = fx.ledger().await;
    assert_eq!(ledger.len(), 1, "one declaration, one entry: {ledger:?}");
    assert_eq!(ledger[0]["entry"]["tx_id"], tx_id.as_str());
    assert_eq!(ledger[0]["entry"]["task"], "TASK-TIER");
    let extra = ledger[0]["entry"]["extra"].as_array().unwrap();
    let prop = |key: &str| {
        extra
            .iter()
            .find(|pair| pair[0] == key)
            .map(|pair| pair[1].as_str().unwrap().to_string())
    };
    assert_eq!(prop("TIER").as_deref(), Some("trivial"));
    assert_eq!(prop("TRIGGERS").as_deref(), Some("none"));

    fx.shutdown().await;
}

/// A tier above the floor with no trigger named is arithmetic nobody can check,
/// which is exactly the assertion-without-evidence this verb replaces.
#[tokio::test]
async fn a_tier_above_the_floor_must_name_the_triggers_that_raised_it() {
    let fx = Fixture::boot().await;

    let (status, body) = fx
        .declare(serde_json::json!({
            "project": "proj",
            "task": "TASK-TIER",
            "tier": "risky",
        }))
        .await;
    assert_eq!(
        status,
        reqwest::StatusCode::BAD_REQUEST,
        "risky with no trigger should be refused: {body}"
    );
    assert!(
        fx.ledger().await.is_empty(),
        "a refused declaration writes nothing"
    );

    let (status, body) = fx
        .declare(serde_json::json!({
            "project": "proj",
            "task": "TASK-TIER",
            "tier": "ordinary",
            "triggers": ["coupling", "priority"],
        }))
        .await;
    assert!(status.is_success(), "declare ordinary: {status} {body}");
    assert_eq!(
        body["triggers"],
        serde_json::json!(["coupling", "priority"])
    );

    let ledger = fx.ledger().await;
    let extra = ledger[0]["entry"]["extra"].as_array().unwrap();
    assert!(
        extra
            .iter()
            .any(|pair| pair[0] == "TRIGGERS" && pair[1] == "coupling, priority"),
        "the triggers travel with the tier: {extra:?}"
    );

    // An invented trigger is refused too — the four are countable because they
    // are a closed set.
    let (status, _) = fx
        .declare(serde_json::json!({
            "project": "proj",
            "task": "TASK-TIER",
            "tier": "risky",
            "triggers": ["it_felt_big"],
        }))
        .await;
    assert_eq!(status, reqwest::StatusCode::BAD_REQUEST);

    fx.shutdown().await;
}

/// Mid-task tier change (item 4): scope that grew re-declares upward and needs
/// no permission, because the tier is computed and there is nothing to escalate.
/// Going the other way is where the failure lives — "it looked smaller once I
/// was inside it" is the exact rationalization the floor rule forbids — so a
/// downgrade is refused unless it is explicitly recorded as a correction.
#[tokio::test]
async fn scope_that_grew_redeclares_upward_and_a_downgrade_must_say_it_is_one() {
    let fx = Fixture::boot().await;

    let (status, _) = fx
        .declare(serde_json::json!({
            "project": "proj",
            "task": "TASK-TIER",
            "tier": "trivial",
        }))
        .await;
    assert!(status.is_success());

    // Upward: no flag, no question.
    let (status, body) = fx
        .declare(serde_json::json!({
            "project": "proj",
            "task": "TASK-TIER",
            "tier": "risky",
            "triggers": ["blast_radius", "breadth"],
            "reason": "the diff reached writer durability and three crates",
        }))
        .await;
    assert!(status.is_success(), "re-declare upward: {status} {body}");
    assert_eq!(body["status"], "redeclared");
    assert_eq!(body["previous_tier"], "trivial");
    assert_eq!(body["lowered"], false);

    // Both declarations survive: the raise is the audit trail, so the first
    // entry is not overwritten by the second.
    let after = fx.read("TASK-TIER").await;
    assert_eq!(after["current"]["tier"], "risky");
    assert_eq!(after["declarations"], 2);
    assert_eq!(fx.ledger().await.len(), 2);

    // Downward without saying so: refused.
    let (status, body) = fx
        .declare(serde_json::json!({
            "project": "proj",
            "task": "TASK-TIER",
            "tier": "ordinary",
            "triggers": ["breadth"],
        }))
        .await;
    assert_eq!(
        status,
        reqwest::StatusCode::BAD_REQUEST,
        "a silent downgrade is the failure the floor rule names: {body}"
    );
    assert_eq!(fx.ledger().await.len(), 2, "the refusal wrote nothing");

    // Downward as a stated correction: allowed, and recorded as a downgrade
    // rather than looking like a fresh computation.
    let (status, body) = fx
        .declare(serde_json::json!({
            "project": "proj",
            "task": "TASK-TIER",
            "tier": "ordinary",
            "triggers": ["breadth"],
            "lower": true,
            "reason": "miscounted: the crate boundary was a re-export, not a second crate",
        }))
        .await;
    assert!(status.is_success(), "stated downgrade: {status} {body}");
    assert_eq!(body["lowered"], true);
    assert_eq!(body["previous_tier"], "risky");

    let ledger = fx.ledger().await;
    assert_eq!(ledger.len(), 3);
    let extra = ledger
        .iter()
        .find(|item| item["entry"]["tx_id"] == body["tx_id"])
        .unwrap()["entry"]["extra"]
        .as_array()
        .unwrap()
        .clone();
    assert!(
        extra
            .iter()
            .any(|pair| pair[0] == "LOWERED" && pair[1] == "yes"),
        "a downgrade is marked as one on the ledger: {extra:?}"
    );
    assert!(
        extra
            .iter()
            .any(|pair| pair[0] == "PREVIOUS_TIER" && pair[1] == "risky"),
        "and says what it came down from: {extra:?}"
    );

    fx.shutdown().await;
}

/// Declarations do not leak across tasks. The omission has to be detectable
/// *per task*, or a manager could declare one thing and edit another.
#[tokio::test]
async fn a_declaration_covers_only_the_task_it_names() {
    let fx = Fixture::boot().await;

    let (status, _) = fx
        .declare(serde_json::json!({
            "project": "proj",
            "task": "TASK-TIER",
            "tier": "trivial",
        }))
        .await;
    assert!(status.is_success());

    assert_eq!(fx.read("TASK-TIER").await["declared"], true);
    assert_eq!(fx.read("TASK-OTHER").await["declared"], false);

    fx.shutdown().await;
}
