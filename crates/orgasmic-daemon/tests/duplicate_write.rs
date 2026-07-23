//! TASK-N4TGD: glossary uniqueness, create retry idempotency, node delete.
mod common;

use std::path::Path;

use orgasmic_core::Home;
use orgasmic_daemon::{Daemon, DaemonOptions, RunningDaemon};

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

fn seed_project(home: &Home, project_root: &Path, project_id: &str) {
    symlink_repo_source(home);
    write(
        &project_root.join(".orgasmic/project.org"),
        format!(
            "#+title: {project_id}\n#+orgasmic_version: 1\n\n* PROJECT {project_id}\n:PROPERTIES:\n:ID:               {project_id}\n:END:\n"
        ),
    );
    write(
        &project_root.join(".orgasmic/tasks/backlog.org"),
        "#+title: sprint\n#+orgasmic_version: 1\n\n",
    );
    write(
        &home.board(),
        format!(
            "#+title: orgasmic board\n#+orgasmic_version: 1\n\n* PROJECT {project_id}\n:PROPERTIES:\n:ID:               {project_id}\n:PATH:             {}\n:BRANCH:           main\n:STATUS:           active\n:END:\n",
            project_root.display()
        ),
    );
    write(
        &project_root.join(".orgasmic/glossary.org"),
        "#+title: glossary\n#+orgasmic_version: 1\n\n",
    );
    write(
        &project_root.join(".orgasmic/decisions.org"),
        "#+title: decisions\n#+orgasmic_version: 1\n\n",
    );
}

fn count_glossary_headings(project_root: &Path) -> usize {
    let raw = std::fs::read_to_string(project_root.join(".orgasmic/glossary.org")).unwrap();
    raw.lines()
        .filter(|line| line.starts_with("* term_") || line.starts_with("* term:"))
        .count()
}

fn read_project_tx(project_root: &Path) -> String {
    let mut raw = String::new();
    let tx_dir = project_root.join(".orgasmic/tx");
    if let Ok(entries) = std::fs::read_dir(&tx_dir) {
        for entry in entries.flatten() {
            if entry.path().extension().and_then(|ext| ext.to_str()) == Some("org") {
                raw.push_str(&std::fs::read_to_string(entry.path()).unwrap_or_default());
            }
        }
    }
    raw
}

#[tokio::test]
async fn glossary_create_retry_is_idempotent_and_rejects_distinct_duplicates() {
    let tmp = tempfile::tempdir().unwrap();
    let home = Home::at(tmp.path().join("home"));
    home.ensure().unwrap();
    let project_root = tmp.path().join("proj");
    seed_project(&home, &project_root, "orgasmic");

    let running = boot(home.clone()).await;
    let token = read_token(&home);
    let client = reqwest::Client::new();
    let base = format!("http://{}", running.addr);

    let payload = serde_json::json!({
        "project": "orgasmic",
        "request_id": "retry-vertical-slice",
        "title": "Vertical Slice",
        "properties": {
            "CANONICAL": "Vertical Slice",
            "DEFINITION": "A thin end-to-end feature cut."
        }
    });
    let first = client
        .post(format!("{base}/api/glossary"))
        .bearer_auth(&token)
        .json(&payload)
        .send()
        .await
        .unwrap();
    assert!(
        first.status().is_success(),
        "first create: {}",
        first.status()
    );
    let first_body: serde_json::Value = first.json().await.unwrap();
    let first_id = first_body["id"].as_str().unwrap().to_string();

    let retry = client
        .post(format!("{base}/api/glossary"))
        .bearer_auth(&token)
        .json(&payload)
        .send()
        .await
        .unwrap();
    assert!(retry.status().is_success(), "retry: {}", retry.status());
    let retry_body: serde_json::Value = retry.json().await.unwrap();
    assert_eq!(retry_body["id"], first_id);
    assert_eq!(retry_body["tx_id"], first_body["tx_id"]);
    assert_eq!(count_glossary_headings(&project_root), 1);

    let dup_title = client
        .post(format!("{base}/api/glossary"))
        .bearer_auth(&token)
        .json(&serde_json::json!({
            "project": "orgasmic",
            "request_id": "dup-title",
            "title": "vertical slice",
            "properties": { "CANONICAL": "other", "DEFINITION": "dup title" }
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(dup_title.status(), reqwest::StatusCode::BAD_REQUEST);
    let dup_title_body: serde_json::Value = dup_title.json().await.unwrap();
    let err = dup_title_body["error"].as_str().unwrap_or_default();
    assert!(
        err.contains(&first_id),
        "title reject names survivor: {err}"
    );

    let dup_canonical = client
        .post(format!("{base}/api/glossary"))
        .bearer_auth(&token)
        .json(&serde_json::json!({
            "project": "orgasmic",
            "request_id": "dup-canonical",
            "title": "Distinct Homonym",
            "properties": { "CANONICAL": "  VERTICAL SLICE ", "DEFINITION": "dup canonical" }
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(dup_canonical.status(), reqwest::StatusCode::BAD_REQUEST);
    let dup_canonical_body: serde_json::Value = dup_canonical.json().await.unwrap();
    let err = dup_canonical_body["error"].as_str().unwrap_or_default();
    assert!(
        err.contains(&first_id),
        "canonical reject names survivor: {err}"
    );

    let forced = client
        .post(format!("{base}/api/glossary"))
        .bearer_auth(&token)
        .json(&serde_json::json!({
            "project": "orgasmic",
            "request_id": "forced-homonym",
            "title": "Vertical Slice",
            "force": true,
            "properties": { "CANONICAL": "Vertical Slice", "DEFINITION": "deliberate homonym" }
        }))
        .send()
        .await
        .unwrap();
    assert!(forced.status().is_success(), "force: {}", forced.status());
    assert_eq!(count_glossary_headings(&project_root), 2);

    let _ = running.shutdown.send(());
    let _ = running.join.await;
}

#[tokio::test]
async fn org_node_delete_requires_occ_records_tx_and_survives_reindex() {
    let tmp = tempfile::tempdir().unwrap();
    let home = Home::at(tmp.path().join("home"));
    home.ensure().unwrap();
    let project_root = tmp.path().join("proj");
    seed_project(&home, &project_root, "orgasmic");
    write(
        &project_root.join(".orgasmic/glossary.org"),
        "#+title: glossary\n#+orgasmic_version: 1\n\n* term_DEL01 Delete Me\n:PROPERTIES:\n:ID: term_DEL01\n:CANONICAL: delete me\n:DEFINITION: doomed\n:END:\n",
    );

    let running = boot(home.clone()).await;
    let token = read_token(&home);
    let client = reqwest::Client::new();
    let base = format!("http://{}", running.addr);

    let doc: serde_json::Value = client
        .get(format!("{base}/api/org/node"))
        .bearer_auth(&token)
        .query(&[
            ("id", "term_DEL01"),
            ("project", "orgasmic"),
            ("kind", "glossary"),
        ])
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let base_version = doc["source"]["base_version"].as_str().unwrap().to_string();

    let conflict = client
        .post(format!("{base}/api/org/node/term_DEL01/delete"))
        .bearer_auth(&token)
        .json(&serde_json::json!({
            "project": "orgasmic",
            "kind": "glossary",
            "base_version": "stale-token",
            "request_id": "delete-stale"
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(conflict.status(), reqwest::StatusCode::CONFLICT);

    let deleted = client
        .post(format!("{base}/api/org/node/term_DEL01/delete"))
        .bearer_auth(&token)
        .json(&serde_json::json!({
            "project": "orgasmic",
            "kind": "glossary",
            "base_version": base_version,
            "request_id": "delete-ok"
        }))
        .send()
        .await
        .unwrap();
    assert!(
        deleted.status().is_success(),
        "delete: {}",
        deleted.status()
    );
    let deleted_body: serde_json::Value = deleted.json().await.unwrap();
    assert_eq!(deleted_body["id"], "term_DEL01");
    assert!(!deleted_body["tx_id"].as_str().unwrap().is_empty());

    let glossary = std::fs::read_to_string(project_root.join(".orgasmic/glossary.org")).unwrap();
    assert!(!glossary.contains("term_DEL01"));
    let tx = read_project_tx(&project_root);
    assert!(
        tx.contains("graph.glossary.deleted") || tx.contains("deleted glossary"),
        "expected delete tx, got:\n{tx}"
    );

    let reindex = client
        .post(format!("{base}/api/reindex/orgasmic"))
        .bearer_auth(&token)
        .send()
        .await
        .unwrap();
    assert!(reindex.status().is_success());
    let missing = client
        .get(format!("{base}/api/org/node"))
        .bearer_auth(&token)
        .query(&[
            ("id", "term_DEL01"),
            ("project", "orgasmic"),
            ("kind", "glossary"),
        ])
        .send()
        .await
        .unwrap();
    assert_eq!(missing.status(), reqwest::StatusCode::NOT_FOUND);

    let ambiguous = client
        .post(format!("{base}/api/org/node/term_DEL01/delete"))
        .bearer_auth(&token)
        .json(&serde_json::json!({
            "kind": "decision",
            "base_version": "anything",
            "request_id": "delete-wrong-kind"
        }))
        .send()
        .await
        .unwrap();
    assert!(
        ambiguous.status().is_client_error(),
        "wrong kind/project must fail safely: {}",
        ambiguous.status()
    );

    let _ = running.shutdown.send(());
    let _ = running.join.await;
}
