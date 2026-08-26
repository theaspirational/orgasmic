//! Minted-id integration: creation paths, board listing, subtask derivation.

mod common;

use std::path::Path;
use std::time::Duration;

use orgasmic_core::{mint_node_id_for_class, Home, NodeIdClass};
use orgasmic_daemon::{Daemon, DaemonOptions, RunningDaemon};


/// Concatenated `node.org` bodies of one collection (node-dir layout,
/// dec_E01MC retired the aggregate `decisions.org` / `glossary.org` /
/// `tasks/backlog.org` files).
fn read_collection(project_root: &std::path::Path, collection: &str) -> String {
    let dir = project_root.join(".orgasmic").join(collection);
    let mut nodes: Vec<std::path::PathBuf> = match std::fs::read_dir(&dir) {
        Ok(entries) => entries
            .flatten()
            .map(|entry| entry.path().join("node.org"))
            .filter(|node| node.is_file())
            .collect(),
        Err(_) => Vec::new(),
    };
    nodes.sort();
    let mut raw = String::new();
    for node in nodes {
        raw.push_str(&std::fs::read_to_string(&node).unwrap_or_default());
        raw.push('\n');
    }
    raw
}

/// Where this project's tx files land: the legacy `.orgasmic/tx/` unless a
/// per-machine `.orgasmic/machines/<id>/tx/` exists (TASK-MSYN4).
fn project_tx_dir(project_root: &std::path::Path) -> std::path::PathBuf {
    let dotorg = project_root.join(".orgasmic");
    if let Ok(machines) = std::fs::read_dir(dotorg.join("machines")) {
        for machine in machines.flatten() {
            let candidate = machine.path().join("tx");
            if candidate.is_dir() {
                return candidate;
            }
        }
    }
    dotorg.join("tx")
}

fn test_options() -> DaemonOptions {
    DaemonOptions {
        bind_override: Some("127.0.0.1".parse().unwrap()),
        port_override: Some(0),
        fs_watcher_enabled: false,
        ..DaemonOptions::default()
    }
}

async fn boot(home: Home) -> RunningDaemon {
    Daemon::run(home, test_options())
        .await
        .expect("boot daemon")
}

fn read_token(home: &Home) -> String {
    std::fs::read_to_string(home.auth_token())
        .or_else(|_| std::fs::read_to_string(home.user().join("auth/token")))
        .expect("token file")
        .trim()
        .to_string()
}

fn write(path: &Path, contents: impl AsRef<str>) {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).unwrap();
    }
    std::fs::write(path, contents.as_ref()).unwrap();
}

fn repo_root() -> std::path::PathBuf {
    let mut here = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    loop {
        if here.join(".orgasmic").is_dir() && here.join("shipped").is_dir() {
            return here;
        }
        if !here.pop() {
            panic!("could not locate orgasmic repo root");
        }
    }
}

fn symlink_repo_source(home: &Home) {
    std::os::unix::fs::symlink(repo_root(), home.source()).unwrap();
}

fn read_project_tx(project_root: &Path) -> String {
    let mut raw = String::new();
    let tx_dir = project_tx_dir(project_root);
    if let Ok(entries) = std::fs::read_dir(&tx_dir) {
        for entry in entries.flatten() {
            if entry.path().extension().and_then(|ext| ext.to_str()) == Some("org") {
                raw.push_str(&std::fs::read_to_string(entry.path()).unwrap_or_default());
            }
        }
    }
    raw
}

fn seed_implementer_worker(home: &Home, worker_id: &str) {
    write(
        &home.user().join(format!("workers/{worker_id}.org")),
        format!(
            "* WORKER {worker_id}\n:PROPERTIES:\n:ID:                          {worker_id}\n:KIND:             implementer\n:DRIVER:                      subprocess-stream-json\n:HARNESS:                     codex\n:PROVIDERS:                   openai\n:DEFAULT_PROVIDER:            openai\n:APPLICABLE_STATES:           backlog\n:MAX_ITERATIONS:              1\n:END:\n\n** Persona\nTest worker.\n",
        ),
    );
}

fn seed_synthetic_open_dispatch(
    project_root: &Path,
    project_id: &str,
    task_id: &str,
    dispatch_tx_id: &str,
) {
    let tx_dir = project_tx_dir(project_root);
    std::fs::create_dir_all(&tx_dir).unwrap();
    let tx_file = tx_dir.join(format!("{}.org", chrono::Utc::now().format("%Y-%m")));
    let entry = format!(
        "\n\n* TX 2026-06-12 Thu manager.dispatch_started {task_id}\n:PROPERTIES:\n:TX_ID:        {dispatch_tx_id}\n:TIME:         [2026-06-12 Thu 10:00:00]\n:TYPE:         manager.dispatch_started\n:ACTOR:        test@example.com\n:MACHINE:      test\n:PROJECT:      {project_id}\n:TASK:         {task_id}\n:WORKTREE:     /tmp/test-worktree\n:BRANCH:       task-test-impl\n:STARTED_AT:   [2026-06-12 Thu 10:00:00]\n:END:\n"
    );
    if tx_file.exists() {
        let mut raw = std::fs::read_to_string(&tx_file).unwrap();
        raw.push_str(&entry);
        write(&tx_file, raw);
    } else {
        write(
            &tx_file,
            format!("#+title: tx\n#+orgasmic_version: 1{entry}"),
        );
    }
}

async fn post_dispatch_close_tx(
    running: &RunningDaemon,
    token: &str,
    project_id: &str,
    task_id: &str,
    closed_tx: &str,
) -> reqwest::Response {
    reqwest::Client::new()
        .post(format!("http://{}/api/tx", running.addr))
        .bearer_auth(token)
        .json(&serde_json::json!({
            "type": "implementer.done",
            "project": project_id,
            "task": task_id,
            "extra": [
                ["CLOSED_TX", closed_tx],
                ["KIND", "implementer"],
            ],
        }))
        .send()
        .await
        .unwrap()
}

#[allow(clippy::too_many_arguments)]
async fn assert_dispatch_and_close_resolve(
    running: &RunningDaemon,
    token: &str,
    client: &reqwest::Client,
    project_root: &Path,
    project_id: &str,
    task_id: &str,
    worker_id: &str,
    brief: &Path,
    request_slug: &str,
) {
    let dispatch_resp = client
        .post(format!(
            "http://{}/api/projects/{project_id}/tasks/{task_id}/dispatch",
            running.addr
        ))
        .bearer_auth(token)
        .json(&serde_json::json!({
            "kind": "implementer",
            "worker": worker_id,
            "brief_path": brief.display().to_string(),
            "request_id": request_slug,
        }))
        .send()
        .await
        .unwrap();
    assert_ne!(
        dispatch_resp.status(),
        reqwest::StatusCode::NOT_FOUND,
        "{task_id} must resolve on dispatch route"
    );

    let closed_tx = if dispatch_resp.status().is_success() {
        let body: serde_json::Value = dispatch_resp.json().await.unwrap();
        let dispatch_tx_id = body["dispatch_tx_id"]
            .as_str()
            .unwrap_or("missing-dispatch-tx")
            .to_string();
        if let Some(run_id) = body.get("run_id").and_then(|v| v.as_str()) {
            let _ = client
                .post(format!("http://{}/api/runs/{run_id}/release", running.addr))
                .bearer_auth(token)
                .json(&serde_json::json!({ "request_id": format!("{request_slug}-release") }))
                .send()
                .await;
        }
        dispatch_tx_id
    } else {
        let synthetic_tx_id = format!("tx-synthetic-{request_slug}");
        seed_synthetic_open_dispatch(project_root, project_id, task_id, &synthetic_tx_id);
        synthetic_tx_id
    };

    let close_resp = post_dispatch_close_tx(running, token, project_id, task_id, &closed_tx).await;
    assert_ne!(
        close_resp.status(),
        reqwest::StatusCode::NOT_FOUND,
        "{task_id} must resolve on dispatch-close (/tx implementer.done)"
    );
    assert!(
        close_resp.status().is_success(),
        "{task_id} dispatch-close: {}",
        close_resp.status()
    );

    let raw = read_project_tx(project_root);
    assert!(
        raw.contains(":TYPE:         implementer.done"),
        "expected implementer.done in project tx log"
    );
    assert!(
        raw.contains(&format!(":TASK:         {task_id}")),
        "expected close tx keyed to {task_id}"
    );
}

fn seed_project(home: &Home, project_root: &Path, project_id: &str) {
    symlink_repo_source(home);
    write(
        &project_root.join(".orgasmic/project.org"),
        format!(
            "#+title: {project_id}\n#+orgasmic_version: 1\n\n* PROJECT {project_id}\n:PROPERTIES:\n:ID:               {project_id}\n:END:\n"
        ),
    );
    std::fs::create_dir_all(project_root.join(".orgasmic/tasks")).unwrap();
    write(
        &home.board(),
        format!(
            "#+title: orgasmic board\n#+orgasmic_version: 1\n\n* PROJECT {project_id}\n:PROPERTIES:\n:ID:               {project_id}\n:PATH:             {}\n:BRANCH:           main\n:STATUS:           active\n:END:\n",
            project_root.display()
        ),
    );
}

#[tokio::test]
async fn graph_create_rejects_legacy_numeric_decision_id() {
    let tmp = tempfile::tempdir().unwrap();
    let home = Home::at(tmp.path().join("home"));
    home.ensure().unwrap();
    let project_root = tmp.path().join("proj");
    seed_project(&home, &project_root, "orgasmic");
    write(
        &project_root.join(".orgasmic/decisions/dec_001/node.org"),
        "#+title: orgasmic decision dec_001\n#+orgasmic_version: 2\n\n* dec_001 Existing\n:PROPERTIES:\n:ID: dec_001\n:END:\n",
    );

    let running = boot(home.clone()).await;
    let token = read_token(&home);
    let client = reqwest::Client::new();
    let resp = client
        .post(format!("http://{}/api/decisions", running.addr))
        .bearer_auth(&token)
        .json(&serde_json::json!({
            "project": "orgasmic",
            "request_id": "reject-legacy-dec",
            "id": "dec_099",
            "title": "legacy attempt",
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), reqwest::StatusCode::BAD_REQUEST);
    let body = resp.text().await.unwrap();
    assert!(body.contains("legacy sequential"));

    let _ = running.shutdown.send(());
    let _ = running.join.await;
}

#[tokio::test]
async fn graph_create_auto_mints_decision_id() {
    let tmp = tempfile::tempdir().unwrap();
    let home = Home::at(tmp.path().join("home"));
    home.ensure().unwrap();
    let project_root = tmp.path().join("proj");
    seed_project(&home, &project_root, "orgasmic");
    std::fs::create_dir_all(project_root.join(".orgasmic/decisions")).unwrap();

    let running = boot(home.clone()).await;
    let token = read_token(&home);
    let client = reqwest::Client::new();
    let resp = client
        .post(format!("http://{}/api/decisions", running.addr))
        .bearer_auth(&token)
        .json(&serde_json::json!({
            "project": "orgasmic",
            "request_id": "auto-mint-dec",
            "title": "Minted decision",
            "properties": { "DECIDED_AT": "[2026-06-12 Fri]" }
        }))
        .send()
        .await
        .unwrap();
    assert!(resp.status().is_success(), "create: {}", resp.status());
    let body: serde_json::Value = resp.json().await.unwrap();
    let id = body["id"].as_str().unwrap();
    assert!(id.starts_with("dec_"));
    assert!(id[id.find('_').unwrap() + 1..]
        .chars()
        .any(|c| c.is_ascii_alphabetic()));

    let _ = running.shutdown.send(());
    let _ = running.join.await;
}

#[tokio::test]
async fn graph_create_auto_mints_glossary_term_without_term_prefix() {
    let tmp = tempfile::tempdir().unwrap();
    let home = Home::at(tmp.path().join("home"));
    home.ensure().unwrap();
    let project_root = tmp.path().join("proj");
    seed_project(&home, &project_root, "orgasmic");
    std::fs::create_dir_all(project_root.join(".orgasmic/glossary")).unwrap();

    let running = boot(home.clone()).await;
    let token = read_token(&home);
    let client = reqwest::Client::new();
    let resp = client
        .post(format!("http://{}/api/glossary", running.addr))
        .bearer_auth(&token)
        .json(&serde_json::json!({
            "project": "orgasmic",
            "request_id": "auto-mint-glossary",
            "title": "Minted term",
            "properties": { "CANONICAL": "minted" }
        }))
        .send()
        .await
        .unwrap();
    assert!(resp.status().is_success(), "create: {}", resp.status());
    let body: serde_json::Value = resp.json().await.unwrap();
    let id = body["id"].as_str().unwrap();
    assert!(id.starts_with("term_"));

    let glossary = read_collection(&project_root, "glossary");
    assert!(
        glossary.contains(&format!("* {id} Minted term")),
        "expected minted heading without term: prefix, got:\n{glossary}"
    );
    assert!(
        !glossary.contains(&format!("term:{id}")),
        "legacy term: prefix must not appear in minted heading"
    );

    let _ = running.shutdown.send(());
    let _ = running.join.await;
}

#[tokio::test]
async fn daemon_create_verbs_emit_heading_tokens_without_identity_lint() {
    let tmp = tempfile::tempdir().unwrap();
    let home = Home::at(tmp.path().join("home"));
    home.ensure().unwrap();
    let project_root = tmp.path().join("proj");
    seed_project(&home, &project_root, "orgasmic");
    std::fs::create_dir_all(project_root.join(".orgasmic/decisions")).unwrap();
    std::fs::create_dir_all(project_root.join(".orgasmic/glossary")).unwrap();

    let running = boot(home.clone()).await;
    let token = read_token(&home);
    let client = reqwest::Client::new();
    let base = format!("http://{}", running.addr);

    let decision: serde_json::Value = client
        .post(format!("{base}/api/decisions"))
        .bearer_auth(&token)
        .json(&serde_json::json!({
            "project": "orgasmic",
            "request_id": "create-token-decision",
            "title": "Tokened decision"
        }))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let glossary: serde_json::Value = client
        .post(format!("{base}/api/glossary"))
        .bearer_auth(&token)
        .json(&serde_json::json!({
            "project": "orgasmic",
            "request_id": "create-token-glossary",
            "title": "Tokened glossary"
        }))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let task: serde_json::Value = client
        .post(format!("{base}/api/projects/orgasmic/tasks"))
        .bearer_auth(&token)
        .json(&serde_json::json!({
            "request_id": "create-token-task",
            "title": "Tokened task"
        }))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();

    let decision_id = decision["id"].as_str().unwrap();
    let glossary_id = glossary["id"].as_str().unwrap();
    let task_id = task["id"].as_str().unwrap();

    let decisions = read_collection(&project_root, "decisions");
    assert!(decisions.contains(&format!("* {decision_id} Tokened decision")));
    let glossary = read_collection(&project_root, "glossary");
    assert!(glossary.contains(&format!("* {glossary_id} Tokened glossary")));
    let tasks = read_collection(&project_root, "tasks");
    assert!(tasks.contains(&format!("* BACKLOG {task_id} Tokened task")));

    let errors: Vec<serde_json::Value> = client
        .get(format!("{base}/api/graph/parse-errors?full=true"))
        .bearer_auth(&token)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let heading_token: Vec<_> = errors
        .iter()
        .filter_map(|e| e["message"].as_str())
        .filter(|m| m.contains("heading-token/:ID: mismatch"))
        .collect();
    assert!(
        heading_token.is_empty(),
        "create verbs must not trip heading-token lint:\n{}",
        heading_token.join("\n")
    );

    let _ = running.shutdown.send(());
    let _ = running.join.await;
}

#[tokio::test]
async fn minted_task_round_trips_board_get_and_subtask_derivation() {
    let task_id = mint_node_id_for_class(NodeIdClass::Task);
    let tmp = tempfile::tempdir().unwrap();
    let home = Home::at(tmp.path().join("home"));
    home.ensure().unwrap();
    let project_root = tmp.path().join("proj");
    seed_project(&home, &project_root, "orgasmic");
    write(
        &project_root.join(format!(".orgasmic/tasks/{task_id}/node.org")),
        format!(
            "#+title: orgasmic task {task_id}\n#+orgasmic_version: 2\n\n* BACKLOG {task_id} Minted parent\n:PROPERTIES:\n:ID:               {task_id}\n:END:\n"
        ),
    );

    let running = boot(home.clone()).await;
    let token = read_token(&home);
    let client = reqwest::Client::new();

    let resp = client
        .get(format!(
            "http://{}/api/projects/orgasmic/tasks/{task_id}",
            running.addr
        ))
        .bearer_auth(&token)
        .send()
        .await
        .unwrap();
    assert!(resp.status().is_success(), "get task: {}", resp.status());
    let task: serde_json::Value = resp.json().await.unwrap();
    assert_eq!(task["id"], task_id);

    let resp = client
        .get(format!(
            "http://{}/api/projects/orgasmic/tasks",
            running.addr
        ))
        .bearer_auth(&token)
        .send()
        .await
        .unwrap();
    assert!(resp.status().is_success());
    let tasks: serde_json::Value = resp.json().await.unwrap();
    let listed = tasks.as_array().unwrap().iter().any(|t| t["id"] == task_id);
    assert!(listed, "minted task missing from project task listing");

    let resp = client
        .post(format!(
            "http://{}/api/tasks/{task_id}/subtasks",
            running.addr
        ))
        .bearer_auth(&token)
        .json(&serde_json::json!({
            "title": "Minted child",
            "request_id": "minted-subtask"
        }))
        .send()
        .await
        .unwrap();
    assert!(resp.status().is_success(), "subtask: {}", resp.status());
    let sub: serde_json::Value = resp.json().await.unwrap();
    assert_eq!(sub["id"], format!("{task_id}.1"));

    let _ = running.shutdown.send(());
    let _ = running.join.await;
}

#[tokio::test]
async fn minted_task_dispatch_and_close_round_trip() {
    let task_id = mint_node_id_for_class(NodeIdClass::Task);
    let worker_id = "implementer-codex-appserver";
    let tmp = tempfile::tempdir().unwrap();
    let home = Home::at(tmp.path().join("home"));
    home.ensure().unwrap();
    let project_root = tmp.path().join("proj");
    seed_project(&home, &project_root, "orgasmic");
    seed_implementer_worker(&home, worker_id);
    write(
        &project_root.join(format!(".orgasmic/tasks/{task_id}/node.org")),
        format!(
            "#+title: orgasmic task {task_id}\n#+orgasmic_version: 2\n\n* BACKLOG {task_id} Minted dispatch\n:PROPERTIES:\n:ID:               {task_id}\n:END:\n"
        ),
    );

    let running = boot(home.clone()).await;
    let token = read_token(&home);
    let client = reqwest::Client::new();
    let brief = project_root.join(".orgasmic/tmp/dispatch/minted-dispatch-brief.md");
    write(&brief, "minted dispatch + close probe\n");

    assert_dispatch_and_close_resolve(
        &running,
        &token,
        &client,
        &project_root,
        "orgasmic",
        &task_id,
        worker_id,
        &brief,
        "minted-dispatch-close",
    )
    .await;

    tokio::time::sleep(Duration::from_millis(100)).await;
    let _ = running.shutdown.send(());
    let _ = running.join.await;
}

#[tokio::test]
async fn legacy_numeric_task_resolves_dispatch_and_close() {
    let tmp = tempfile::tempdir().unwrap();
    let home = Home::at(tmp.path().join("home"));
    home.ensure().unwrap();
    let project_root = tmp.path().join("proj");
    seed_project(&home, &project_root, "orgasmic");
    write(
        &project_root.join(".orgasmic/tasks/TASK-001/node.org"),
        "#+title: orgasmic task TASK-001\n#+orgasmic_version: 2\n\n* BACKLOG TASK-001 Legacy task\n:PROPERTIES:\n:ID:               TASK-001\n:END:\n",
    );
    let worker_id = "implementer-codex-appserver";
    seed_implementer_worker(&home, worker_id);

    let running = boot(home.clone()).await;
    let token = read_token(&home);
    let client = reqwest::Client::new();

    let resp = client
        .get(format!(
            "http://{}/api/projects/orgasmic/tasks/TASK-001",
            running.addr
        ))
        .bearer_auth(&token)
        .send()
        .await
        .unwrap();
    assert!(resp.status().is_success(), "legacy get: {}", resp.status());

    let brief = project_root.join(".orgasmic/tmp/dispatch/task-001-brief.md");
    write(&brief, "legacy dispatch + close probe\n");
    assert_dispatch_and_close_resolve(
        &running,
        &token,
        &client,
        &project_root,
        "orgasmic",
        "TASK-001",
        worker_id,
        &brief,
        "legacy-dispatch-close",
    )
    .await;

    tokio::time::sleep(Duration::from_millis(100)).await;
    let _ = running.shutdown.send(());
    let _ = running.join.await;
}
