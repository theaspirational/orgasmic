// orgasmic:task_HC7PW
//! Integration tests for the body-write structural invariant guard and
//! phantom-heading lint (TASK-148).
//!
//! Scope:
//! - Column-0 `* X` in a body-edit payload is rejected (400).
//! - Space-indented ` * X` in a body-edit payload is accepted (200).
//! - A task-file heading that lacks `:ID: TASK-*` or a TODO state keyword
//!   surfaces as a parse error via `/graph/parse-errors`.

mod common;

use std::path::Path;

use orgasmic_core::Home;
use orgasmic_daemon::{Daemon, DaemonOptions};

fn test_options() -> DaemonOptions {
    DaemonOptions {
        bind_override: Some("127.0.0.1".parse().unwrap()),
        port_override: Some(0),
        fs_watcher_enabled: false,
        tmux_input_ready_timeout_secs: Some(1),
        ..DaemonOptions::default()
    }
}

fn write(path: &Path, contents: impl AsRef<str>) {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).unwrap();
    }
    std::fs::write(path, contents.as_ref()).unwrap();
}

fn read_token(home: &Home) -> String {
    let path = home.auth_token();
    for _ in 0..20 {
        if path.exists() {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
    std::fs::read_to_string(&path)
        .unwrap_or_else(|_| {
            std::fs::read_to_string(home.user().join("auth/token")).expect("token file")
        })
        .trim()
        .to_string()
}

async fn task_base_version(
    client: &reqwest::Client,
    base: &str,
    token: &str,
    project: &str,
) -> String {
    let doc: serde_json::Value = client
        .get(format!("{base}/api/org/node"))
        .bearer_auth(token)
        .query(&[("project", project), ("id", "TASK-G01"), ("kind", "task")])
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    doc["source"]["base_version"].as_str().unwrap().to_string()
}

async fn post_task_edit(
    client: &reqwest::Client,
    base: &str,
    token: &str,
    project: &str,
    request_id: &str,
    ops: serde_json::Value,
) -> reqwest::Response {
    let base_version = task_base_version(client, base, token, project).await;
    client
        .post(format!("{base}/api/org/node/TASK-G01/edit"))
        .bearer_auth(token)
        .json(&serde_json::json!({
            "project": project,
            "kind": "task",
            "base_version": base_version,
            "request_id": request_id,
            "ops": ops,
        }))
        .send()
        .await
        .unwrap()
}

/// Seed a minimal project with a task that has a Description section so
/// `set_section_body` (not just `append_section`) is exercised.
fn seed_project_with_sections(home: &Home, project_root: &Path, project_id: &str) {
    write(
        &project_root.join(".orgasmic/project.org"),
        format!(
            "#+title: {project_id}\n#+orgasmic_version: 1\n\n\
             * PROJECT {project_id}\n:PROPERTIES:\n:ID:               {project_id}\n:END:\n"
        ),
    );
    write(
        &project_root.join(".orgasmic/tasks/backlog.org"),
        "#+title: sprint\n#+orgasmic_version: 1\n\n\
         * BACKLOG TASK-G01 Guard test task :work:\n\
         :PROPERTIES:\n\
         :ID:               TASK-G01\n\
         \
         \
         :END:\n\n\
         ** Description\nOriginal description.\n\n\
         ** Acceptance Criteria\n- [ ] Item.\n",
    );
    write(
        &home.board(),
        format!(
            "#+title: orgasmic board\n#+orgasmic_version: 1\n\n\
             * PROJECT {project_id}\n:PROPERTIES:\n:ID:               {project_id}\n\
             :PATH:             {}\n:BRANCH:           main\n:END:\n",
            project_root.display()
        ),
    );
}

#[tokio::test]
async fn add_section_rejects_column0_star_heading() {
    let tmp = tempfile::tempdir().unwrap();
    let home = Home::at(tmp.path().join("home"));
    home.ensure().unwrap();
    let project_root = tmp.path().join("proj");
    seed_project_with_sections(&home, &project_root, "guardappendtest");

    let running = Daemon::run(home.clone(), test_options())
        .await
        .expect("boot daemon");
    let token = read_token(&home);
    let client = reqwest::Client::new();
    let base = format!("http://{}", running.addr);

    let resp = post_task_edit(
        &client,
        &base,
        &token,
        "guardappendtest",
        "guard-add-section-reject-test",
        serde_json::json!([
            { "op": "add_section", "title": "Evidence", "body": "* Phantom Heading\nsome text\n" }
        ]),
    )
    .await;

    let status = resp.status();
    let body = resp.text().await.unwrap();
    assert_eq!(
        status,
        reqwest::StatusCode::BAD_REQUEST,
        "add_section with body heading injection must be rejected: {body}"
    );
    common::assert_body_rejects_paths(&body, &[&project_root]);

    let on_disk =
        std::fs::read_to_string(project_root.join(".orgasmic/tasks/backlog.org")).unwrap();
    assert!(
        !on_disk.contains("Phantom Heading") && !on_disk.contains("** Evidence"),
        "file must be unmodified after append rejection: {on_disk}"
    );

    let _ = running.shutdown.send(());
    let _ = running.join.await;
}

// ---------------------------------------------------------------------------
// Write-time guard: column-0 star is rejected
// ---------------------------------------------------------------------------

#[tokio::test]
async fn body_edit_rejects_column0_star_heading() {
    let tmp = tempfile::tempdir().unwrap();
    let home = Home::at(tmp.path().join("home"));
    home.ensure().unwrap();
    let project_root = tmp.path().join("proj");
    seed_project_with_sections(&home, &project_root, "guardtest");

    let running = Daemon::run(home.clone(), test_options())
        .await
        .expect("boot daemon");
    let token = read_token(&home);
    let client = reqwest::Client::new();
    let base = format!("http://{}", running.addr);

    // Read base_version for TASK-G01.
    let doc: serde_json::Value = client
        .get(format!("{base}/api/org/node"))
        .bearer_auth(&token)
        .query(&[
            ("project", "guardtest"),
            ("id", "TASK-G01"),
            ("kind", "task"),
        ])
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let base_version = doc["source"]["base_version"].as_str().unwrap().to_string();

    // Attempt to inject a column-0 heading into the Description section body.
    let resp = client
        .post(format!("{base}/api/org/node/TASK-G01/edit"))
        .bearer_auth(&token)
        .json(&serde_json::json!({
            "project": "guardtest",
            "kind": "task",
            "base_version": base_version,
            "request_id": "guard-reject-test",
            "ops": [
                { "op": "set_section_body", "title": "Description", "body": "* Phantom Heading\nsome text\n" }
            ]
        }))
        .send()
        .await
        .unwrap();

    let status = resp.status();
    let body = resp.text().await.unwrap();
    assert_eq!(
        status,
        reqwest::StatusCode::BAD_REQUEST,
        "column-0 star heading in body must be rejected: {body}"
    );
    common::assert_body_rejects_paths(&body, &[&project_root]);

    // Confirm the file on disk is unchanged.
    let on_disk =
        std::fs::read_to_string(project_root.join(".orgasmic/tasks/backlog.org")).unwrap();
    assert!(
        on_disk.contains("** Description\nOriginal description.\n"),
        "file must be unmodified after rejection"
    );

    let _ = running.shutdown.send(());
    let _ = running.join.await;
}

// ---------------------------------------------------------------------------
// Write-time guard: space-indented star is accepted
// ---------------------------------------------------------------------------

#[tokio::test]
async fn body_edit_accepts_indented_star_and_roundtrips() {
    let tmp = tempfile::tempdir().unwrap();
    let home = Home::at(tmp.path().join("home"));
    home.ensure().unwrap();
    let project_root = tmp.path().join("proj");
    seed_project_with_sections(&home, &project_root, "guardtest2");

    let running = Daemon::run(home.clone(), test_options())
        .await
        .expect("boot daemon");
    let token = read_token(&home);
    let client = reqwest::Client::new();
    let base = format!("http://{}", running.addr);

    // Read base_version.
    let doc: serde_json::Value = client
        .get(format!("{base}/api/org/node"))
        .bearer_auth(&token)
        .query(&[
            ("project", "guardtest2"),
            ("id", "TASK-G01"),
            ("kind", "task"),
        ])
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let base_version = doc["source"]["base_version"].as_str().unwrap().to_string();

    // Space-indented * is not a heading — must be accepted.
    let resp = client
        .post(format!("{base}/api/org/node/TASK-G01/edit"))
        .bearer_auth(&token)
        .json(&serde_json::json!({
            "project": "guardtest2",
            "kind": "task",
            "base_version": base_version,
            "request_id": "guard-accept-test",
            "ops": [
                { "op": "set_section_body", "title": "Description", "body": " * Not a heading\nSome prose.\n" }
            ]
        }))
        .send()
        .await
        .unwrap();

    let status = resp.status();
    let body_text = resp.text().await.unwrap();
    assert!(
        status.is_success(),
        "space-indented star must be accepted: {status} {body_text}"
    );

    // Byte-stable round-trip: the escaped text must appear verbatim on disk.
    let on_disk =
        std::fs::read_to_string(project_root.join(".orgasmic/tasks/backlog.org")).unwrap();
    assert!(
        on_disk.contains(" * Not a heading\nSome prose."),
        "indented star must survive round-trip: {on_disk}"
    );

    let _ = running.shutdown.send(());
    let _ = running.join.await;
}

#[tokio::test]
async fn body_edit_accepts_src_block_column0_star() {
    let tmp = tempfile::tempdir().unwrap();
    let home = Home::at(tmp.path().join("home"));
    home.ensure().unwrap();
    let project_root = tmp.path().join("proj");
    seed_project_with_sections(&home, &project_root, "guardsrcblocktest");

    let running = Daemon::run(home.clone(), test_options())
        .await
        .expect("boot daemon");
    let token = read_token(&home);
    let client = reqwest::Client::new();
    let base = format!("http://{}", running.addr);
    let body = "#+begin_src org\n* Not a parsed heading\n#+end_src\nAfter block.\n";

    let resp = post_task_edit(
        &client,
        &base,
        &token,
        "guardsrcblocktest",
        "guard-src-block-test",
        serde_json::json!([
            { "op": "set_section_body", "title": "Description", "body": body }
        ]),
    )
    .await;

    let status = resp.status();
    let body_text = resp.text().await.unwrap();
    assert!(
        status.is_success(),
        "src block column-0 star must be accepted: {status} {body_text}"
    );

    let on_disk =
        std::fs::read_to_string(project_root.join(".orgasmic/tasks/backlog.org")).unwrap();
    assert!(
        on_disk.contains("#+begin_src org\n* Not a parsed heading\n#+end_src\nAfter block.\n"),
        "src block body must survive round-trip: {on_disk}"
    );

    let _ = running.shutdown.send(());
    let _ = running.join.await;
}

// ---------------------------------------------------------------------------
// Write-time guard: set_body (node body) also rejects column-0 star
// ---------------------------------------------------------------------------

#[tokio::test]
async fn node_body_edit_rejects_column0_star_heading() {
    let tmp = tempfile::tempdir().unwrap();
    let home = Home::at(tmp.path().join("home"));
    home.ensure().unwrap();
    let project_root = tmp.path().join("proj");
    seed_project_with_sections(&home, &project_root, "guardbodytest");

    let running = Daemon::run(home.clone(), test_options())
        .await
        .expect("boot daemon");
    let token = read_token(&home);
    let client = reqwest::Client::new();
    let base = format!("http://{}", running.addr);

    let doc: serde_json::Value = client
        .get(format!("{base}/api/org/node"))
        .bearer_auth(&token)
        .query(&[
            ("project", "guardbodytest"),
            ("id", "TASK-G01"),
            ("kind", "task"),
        ])
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let base_version = doc["source"]["base_version"].as_str().unwrap().to_string();

    let resp = client
        .post(format!("{base}/api/org/node/TASK-G01/edit"))
        .bearer_auth(&token)
        .json(&serde_json::json!({
            "project": "guardbodytest",
            "kind": "task",
            "base_version": base_version,
            "request_id": "guard-node-body-test",
            "ops": [
                { "op": "set_body", "body": "* Injected top-level\n" }
            ]
        }))
        .send()
        .await
        .unwrap();

    assert_eq!(
        resp.status(),
        reqwest::StatusCode::BAD_REQUEST,
        "set_body with column-0 star must be rejected"
    );

    let _ = running.shutdown.send(());
    let _ = running.join.await;
}

#[tokio::test]
async fn node_body_edit_accepts_escaped_leading_star_and_preserves_bytes() {
    let tmp = tempfile::tempdir().unwrap();
    let home = Home::at(tmp.path().join("home"));
    home.ensure().unwrap();
    let project_root = tmp.path().join("proj");
    seed_project_with_sections(&home, &project_root, "guardbodyescapetest");

    let running = Daemon::run(home.clone(), test_options())
        .await
        .expect("boot daemon");
    let token = read_token(&home);
    let client = reqwest::Client::new();
    let base = format!("http://{}", running.addr);

    let body = " * Not a heading\n\tIndented detail.";
    let resp = post_task_edit(
        &client,
        &base,
        &token,
        "guardbodyescapetest",
        "guard-node-body-escape-test",
        serde_json::json!([
            { "op": "set_body", "body": body }
        ]),
    )
    .await;

    let status = resp.status();
    let body_text = resp.text().await.unwrap();
    assert!(
        status.is_success(),
        "set_body escaped leading star must be accepted: {status} {body_text}"
    );

    let on_disk =
        std::fs::read_to_string(project_root.join(".orgasmic/tasks/backlog.org")).unwrap();
    assert!(
        on_disk.contains("\n * Not a heading\n\tIndented detail.\n\n** Description\n"),
        "set_body must preserve leading whitespace bytes: {on_disk}"
    );

    let _ = running.shutdown.send(());
    let _ = running.join.await;
}

#[tokio::test]
async fn section_body_edit_preserves_escaped_leading_bytes() {
    let tmp = tempfile::tempdir().unwrap();
    let home = Home::at(tmp.path().join("home"));
    home.ensure().unwrap();
    let project_root = tmp.path().join("proj");
    seed_project_with_sections(&home, &project_root, "guardsectionbytestest");

    let running = Daemon::run(home.clone(), test_options())
        .await
        .expect("boot daemon");
    let token = read_token(&home);
    let client = reqwest::Client::new();
    let base = format!("http://{}", running.addr);

    let body = "  leading spaces\n * Not a heading\n\tTabbed detail.";
    let resp = post_task_edit(
        &client,
        &base,
        &token,
        "guardsectionbytestest",
        "guard-section-bytes-test",
        serde_json::json!([
            { "op": "set_section_body", "title": "Description", "body": body }
        ]),
    )
    .await;

    let status = resp.status();
    let body_text = resp.text().await.unwrap();
    assert!(
        status.is_success(),
        "set_section_body escaped leading bytes must be accepted: {status} {body_text}"
    );

    let on_disk =
        std::fs::read_to_string(project_root.join(".orgasmic/tasks/backlog.org")).unwrap();
    assert!(
        on_disk.contains("** Description\n  leading spaces\n * Not a heading\n\tTabbed detail.\n\n** Acceptance Criteria\n"),
        "set_section_body must preserve leading whitespace bytes: {on_disk}"
    );

    let _ = running.shutdown.send(());
    let _ = running.join.await;
}

// ---------------------------------------------------------------------------
// Read-time lint: phantom heading surfaces via /graph/parse-errors
// ---------------------------------------------------------------------------

#[tokio::test]
async fn phantom_heading_surfaces_via_parse_errors() {
    let tmp = tempfile::tempdir().unwrap();
    let home = Home::at(tmp.path().join("home"));
    home.ensure().unwrap();
    let project_root = tmp.path().join("proj");

    // Seed board with project entry.
    write(
        &project_root.join(".orgasmic/project.org"),
        "#+title: linttest\n#+orgasmic_version: 1\n\n\
         * PROJECT linttest\n:PROPERTIES:\n:ID:               linttest\n:END:\n",
    );
    write(
        &home.board(),
        format!(
            "#+title: orgasmic board\n#+orgasmic_version: 1\n\n\
             * PROJECT linttest\n:PROPERTIES:\n:ID:               linttest\n\
             :PATH:             {}\n:BRANCH:           main\n:END:\n",
            project_root.display()
        ),
    );
    // Sprint file with one valid task and one phantom heading (no :ID:, no state).
    write(
        &project_root.join(".orgasmic/tasks/backlog.org"),
        "#+title: sprint\n#+orgasmic_version: 1\n\n\
         * BACKLOG TASK-001 Valid task :work:\n\
         :PROPERTIES:\n:ID:               TASK-001\n:END:\n\n\
         * Phantom heading from daemon-free write\n\
         Some body text that injected a spurious heading.\n",
    );

    let running = Daemon::run(home.clone(), test_options())
        .await
        .expect("boot daemon");
    let token = read_token(&home);
    let client = reqwest::Client::new();

    let resp = client
        .get(format!("http://{}/api/graph/parse-errors", running.addr))
        .bearer_auth(&token)
        .send()
        .await
        .unwrap();
    assert!(resp.status().is_success());
    let errors: serde_json::Value = resp.json().await.unwrap();
    let msgs: Vec<&str> = errors
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|e| e["message"].as_str())
        .collect();
    assert!(
        msgs.iter()
            .any(|m| m.contains("Phantom heading from daemon-free write")
                && m.contains("phantom heading")),
        "expected lint message for phantom heading, got: {msgs:?}"
    );
    // Valid task must not be flagged.
    assert!(
        !msgs.iter().any(|m| m.contains("Valid task")),
        "valid task must not be flagged: {msgs:?}"
    );

    let _ = running.shutdown.send(());
    let _ = running.join.await;
}
