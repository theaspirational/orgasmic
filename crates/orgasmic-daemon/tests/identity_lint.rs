//! Identity lint via `/graph/parse-errors` (TASK-QRD3Y).

use orgasmic_core::{mint_node_id, Home, NodeIdClass};
use orgasmic_daemon::{Daemon, DaemonOptions};

fn write(path: &std::path::Path, contents: &str) {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).unwrap();
    }
    std::fs::write(path, contents).unwrap();
}

fn read_token(home: &Home) -> String {
    std::fs::read_to_string(home.auth_token())
        .expect("auth token")
        .trim()
        .to_string()
}

fn test_options() -> DaemonOptions {
    DaemonOptions {
        bind_override: Some("127.0.0.1".parse().unwrap()),
        port_override: Some(0),
        ..DaemonOptions::default()
    }
}

fn seed_board(home: &Home, project_root: &std::path::Path, project_id: &str) {
    write(
        &home.board(),
        &format!(
            "#+title: board\n\n* {project_id} Test\n:PROPERTIES:\n:ID: {project_id}\n:PATH: {}\n:BRANCH: main\n:STATUS: active\n:END:\n",
            project_root.display()
        ),
    );
}

fn seed_project(project_root: &std::path::Path, task_org: &str) {
    write(&project_root.join(".orgasmic/tasks/backlog.org"), task_org);
}

#[tokio::test]
async fn duplicate_identity_surfaces_via_parse_errors() {
    let tmp = tempfile::tempdir().unwrap();
    let home = Home::at(tmp.path().join("home"));
    home.ensure().unwrap();
    let project_root = tmp.path().join("proj");
    let dup = mint_node_id(NodeIdClass::Task);
    seed_board(&home, &project_root, "proj-lint");
    write(
        &project_root.join(".orgasmic/tasks/backlog.org"),
        &format!(
            "#+title: backlog\n#+orgasmic_version: 1\n\n* BACKLOG {dup} One\n:PROPERTIES:\n:ID: {dup}\n:END:\n\n* BACKLOG {dup} Two\n:PROPERTIES:\n:ID: {dup}\n:END:\n"
        ),
    );

    let home_for_token = home.clone();
    let running = Daemon::run(home, test_options())
        .await
        .expect("boot daemon");
    let token = read_token(&home_for_token);
    let client = reqwest::Client::new();
    let resp = client
        .get(format!("http://{}/api/graph/parse-errors", running.addr))
        .bearer_auth(&token)
        .send()
        .await
        .expect("parse errors");
    assert!(resp.status().is_success());
    let errors: Vec<serde_json::Value> = resp.json().await.expect("json");
    let msgs: Vec<String> = errors
        .iter()
        .filter_map(|e| e["message"].as_str().map(str::to_string))
        .collect();
    assert!(
        msgs.iter()
            .any(|m| m.contains("duplicate identity") && m.contains(&dup)),
        "expected duplicate lint, got {msgs:?}"
    );

    let _ = running.shutdown.send(());
    running.join.await.unwrap();
}

#[tokio::test]
async fn malformed_identity_surfaces_via_parse_errors() {
    let tmp = tempfile::tempdir().unwrap();
    let home = Home::at(tmp.path().join("home"));
    home.ensure().unwrap();
    let project_root = tmp.path().join("proj");
    seed_board(&home, &project_root, "proj-malformed");
    let anchor = mint_node_id(NodeIdClass::Decision);
    seed_project(
        &project_root,
        "#+title: sprint\n#+orgasmic_version: 1\n\n* BACKLOG TASK-001 Legacy\n:PROPERTIES:\n:ID: TASK-001\n:END:\n",
    );
    write(
        &project_root.join(".orgasmic/decisions.org"),
        &format!("#+title: decisions\n\n* {anchor} Anchor\n:PROPERTIES:\n:ID: {anchor}\n:END:\n"),
    );

    let home_for_token = home.clone();
    let running = Daemon::run(home, test_options())
        .await
        .expect("boot daemon");
    let token = read_token(&home_for_token);
    let client = reqwest::Client::new();
    let resp = client
        .get(format!("http://{}/api/graph/parse-errors", running.addr))
        .bearer_auth(&token)
        .send()
        .await
        .expect("parse errors");
    let errors: Vec<serde_json::Value> = resp.json().await.expect("json");
    let msgs: Vec<String> = errors
        .iter()
        .filter_map(|e| e["message"].as_str().map(str::to_string))
        .collect();
    assert!(
        msgs.iter()
            .any(|m| m.contains("malformed identity") && m.contains("TASK-001")),
        "expected malformed lint, got {msgs:?}"
    );

    let _ = running.shutdown.send(());
    running.join.await.unwrap();
}

#[tokio::test]
async fn heading_id_token_mismatch_surfaces_via_parse_errors() {
    let tmp = tempfile::tempdir().unwrap();
    let home = Home::at(tmp.path().join("home"));
    home.ensure().unwrap();
    let project_root = tmp.path().join("proj");
    seed_board(&home, &project_root, "proj-heading-lint");
    seed_project(
        &project_root,
        "#+title: sprint\n#+orgasmic_version: 1\n\n\
         * BACKLOG TASK-WRONG Drifted title\n\
         :PROPERTIES:\n:ID: TASK-RIGHT\n:END:\n",
    );
    let anchor = mint_node_id(NodeIdClass::Decision);
    write(
        &project_root.join(".orgasmic/decisions.org"),
        &format!("#+title: decisions\n\n* dec_WRONG Anchor\n:PROPERTIES:\n:ID: {anchor}\n:END:\n"),
    );
    write(
        &project_root.join(".orgasmic/architecture.org"),
        "#+title: architecture\n\n* arch_WRONG Node\n:PROPERTIES:\n:ID: arch_RIGHT\n:END:\n",
    );
    write(
        &project_root.join(".orgasmic/glossary.org"),
        "#+title: glossary\n\n* human slug title\n:PROPERTIES:\n:ID: term_ABCDE\n:END:\n",
    );

    let home_for_token = home.clone();
    let running = Daemon::run(home, test_options())
        .await
        .expect("boot daemon");
    let token = read_token(&home_for_token);
    let client = reqwest::Client::new();
    let resp = client
        .get(format!("http://{}/api/graph/parse-errors", running.addr))
        .bearer_auth(&token)
        .send()
        .await
        .expect("parse errors");
    let errors: Vec<serde_json::Value> = resp.json().await.expect("json");
    let msgs: Vec<String> = errors
        .iter()
        .filter_map(|e| e["message"].as_str().map(str::to_string))
        .collect();
    assert!(
        msgs.iter()
            .any(|m| m.contains("heading-token/:ID: mismatch") && m.contains("TASK-WRONG")),
        "expected task mismatch lint, got {msgs:?}"
    );
    assert!(
        msgs.iter()
            .any(|m| m.contains("heading-token/:ID: mismatch") && m.contains("dec_WRONG")),
        "expected decision mismatch lint, got {msgs:?}"
    );
    assert!(
        msgs.iter()
            .any(|m| m.contains("heading-token/:ID: mismatch") && m.contains("arch_WRONG")),
        "expected arch mismatch lint, got {msgs:?}"
    );
    assert!(
        !msgs.iter().any(|m| m.contains("human slug title")),
        "glossary slug titles must be exempt: {msgs:?}"
    );

    let _ = running.shutdown.send(());
    running.join.await.unwrap();
}

#[tokio::test]
async fn mismatched_task_heading_still_indexes_under_drawer_id() {
    let tmp = tempfile::tempdir().unwrap();
    let home = Home::at(tmp.path().join("home"));
    home.ensure().unwrap();
    let project_root = tmp.path().join("proj");
    seed_board(&home, &project_root, "proj-index-mismatch");
    seed_project(
        &project_root,
        "#+title: sprint\n#+orgasmic_version: 1\n\n\
         * BACKLOG TASK-WRONG Drifted title\n\
         :PROPERTIES:\n:ID: TASK-RIGHT\n:END:\n",
    );

    let home_for_token = home.clone();
    let running = Daemon::run(home, test_options())
        .await
        .expect("boot daemon");
    let token = read_token(&home_for_token);
    let client = reqwest::Client::new();
    let tasks: serde_json::Value = client
        .get(format!(
            "http://{}/api/projects/proj-index-mismatch/tasks",
            running.addr
        ))
        .bearer_auth(&token)
        .send()
        .await
        .expect("tasks")
        .json()
        .await
        .expect("tasks json");
    let ids: Vec<&str> = tasks
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|t| t["id"].as_str())
        .collect();
    assert!(
        ids.contains(&"TASK-RIGHT"),
        "task must index under drawer :ID:, got {ids:?}"
    );
    assert!(
        !ids.contains(&"TASK-WRONG"),
        "heading token must not become canonical id, got {ids:?}"
    );

    let _ = running.shutdown.send(());
    running.join.await.unwrap();
}

#[tokio::test]
#[ignore = "live corpus probe against checkout; run for TASK-KY80Q gate"]
async fn live_corpus_heading_token_lint_adds_zero_findings() {
    let tmp = tempfile::tempdir().unwrap();
    let home = Home::at(tmp.path().join("home"));
    home.ensure().unwrap();
    let project_root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    let project_root = project_root.canonicalize().expect("worktree root");
    seed_board(&home, &project_root, "orgasmic-live");

    let home_for_token = home.clone();
    let running = Daemon::run(home, test_options())
        .await
        .expect("boot daemon");
    let token = read_token(&home_for_token);
    let client = reqwest::Client::new();
    let resp = client
        .get(format!("http://{}/api/graph/parse-errors", running.addr))
        .bearer_auth(&token)
        .send()
        .await
        .expect("parse errors");
    let errors: Vec<serde_json::Value> = resp.json().await.expect("json");
    let heading_token: Vec<_> = errors
        .iter()
        .filter_map(|e| e["message"].as_str())
        .filter(|m| m.contains("heading-token/:ID: mismatch"))
        .collect();
    assert!(
        heading_token.is_empty(),
        "live corpus must not trip heading-token lint:\n{}",
        heading_token.join("\n")
    );
    assert_eq!(
        errors.len(),
        56,
        "parse-errors baseline must stay at 56 after heading-token lint"
    );

    let _ = running.shutdown.send(());
    running.join.await.unwrap();
}
