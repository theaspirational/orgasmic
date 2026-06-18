// orgasmic:task_RCP69
//! Integration tests for opt-in `body_format: raw` on body-edit ops (TASK-156).

mod common;

use std::path::Path;

use orgasmic_core::{wrap_raw_body, Home, OrgFile};
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
         * BACKLOG TASK-G01 Raw body test task :work:\n\
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

fn heading_ids(file: &OrgFile) -> Vec<String> {
    fn collect(headings: &[orgasmic_core::Heading], out: &mut Vec<String>) {
        for h in headings {
            if let Some(id) = h.property("ID") {
                out.push(id.to_string());
            }
            collect(&h.sections, out);
        }
    }
    let mut out = Vec::new();
    collect(&file.headings, &mut out);
    out
}

#[tokio::test]
async fn raw_body_format_wraps_escapes_and_roundtrips() {
    let tmp = tempfile::tempdir().unwrap();
    let home = Home::at(tmp.path().join("home"));
    home.ensure().unwrap();
    let project_root = tmp.path().join("proj");
    seed_project_with_sections(&home, &project_root, "rawbodytest");

    let running = Daemon::run(home.clone(), test_options())
        .await
        .expect("boot daemon");
    let token = read_token(&home);
    let client = reqwest::Client::new();
    let base = format!("http://{}", running.addr);

    let payload = "* Raw heading line\n#+end_example\nplain trailing\n";
    let expected_on_disk = wrap_raw_body(payload);

    let resp = client
        .post(format!("{base}/api/org/node/TASK-G01/edit"))
        .bearer_auth(&token)
        .json(&serde_json::json!({
            "project": "rawbodytest",
            "kind": "task",
            "base_version": task_base_version(&client, &base, &token, "rawbodytest").await,
            "request_id": "raw-body-roundtrip-test",
            "ops": [
                {
                    "op": "set_section_body",
                    "title": "Description",
                    "body": payload,
                    "body_format": "raw"
                }
            ]
        }))
        .send()
        .await
        .unwrap();

    let status = resp.status();
    let body_text = resp.text().await.unwrap();
    assert!(
        status.is_success(),
        "raw body_format must be accepted: {status} {body_text}"
    );

    let sprint_path = project_root.join(".orgasmic/tasks/backlog.org");
    let before_text = "* BACKLOG TASK-G01 Raw body test task :work:\n\
         :PROPERTIES:\n\
         :ID:               TASK-G01\n\
         \
         \
         :END:\n\n\
         ** Description\nOriginal description.\n\n\
         ** Acceptance Criteria\n- [ ] Item.\n";
    let before = OrgFile::parse(before_text, "backlog.org").unwrap();

    let on_disk = std::fs::read_to_string(&sprint_path).unwrap();
    assert!(
        on_disk.contains(&format!("** Description\n{expected_on_disk}\n")),
        "wrapped+escaped body must be byte-stable on disk: {on_disk}"
    );

    let after = OrgFile::parse(&on_disk, sprint_path.to_string_lossy()).unwrap();
    assert_eq!(
        heading_ids(&before),
        heading_ids(&after),
        "heading ids must be unchanged after raw body edit"
    );
    assert!(
        !on_disk.lines().any(|line| line == "* Raw heading line"),
        "unescaped column-0 phantom heading must not appear on disk: {on_disk}"
    );

    let _ = running.shutdown.send(());
    let _ = running.join.await;
}

#[tokio::test]
async fn raw_body_format_adversarial_end_example_cannot_escape_wrapper() {
    let tmp = tempfile::tempdir().unwrap();
    let home = Home::at(tmp.path().join("home"));
    home.ensure().unwrap();
    let project_root = tmp.path().join("proj");
    seed_project_with_sections(&home, &project_root, "rawescapefail");

    let running = Daemon::run(home.clone(), test_options())
        .await
        .expect("boot daemon");
    let token = read_token(&home);
    let client = reqwest::Client::new();
    let base = format!("http://{}", running.addr);

    let payload = "#+end_example\n* Phantom Heading\n";
    let expected_wrapped = wrap_raw_body(payload);

    let resp = client
        .post(format!("{base}/api/org/node/TASK-G01/edit"))
        .bearer_auth(&token)
        .json(&serde_json::json!({
            "project": "rawescapefail",
            "kind": "task",
            "base_version": task_base_version(&client, &base, &token, "rawescapefail").await,
            "request_id": "raw-body-escape-test",
            "ops": [
                {
                    "op": "set_section_body",
                    "title": "Description",
                    "body": payload,
                    "body_format": "raw"
                }
            ]
        }))
        .send()
        .await
        .unwrap();

    assert!(
        resp.status().is_success(),
        "adversarial raw payload must be accepted when escaped: {}",
        resp.text().await.unwrap()
    );

    let on_disk =
        std::fs::read_to_string(project_root.join(".orgasmic/tasks/backlog.org")).unwrap();
    assert!(
        on_disk.contains(&expected_wrapped),
        "payload must be comma-escaped inside example block: {on_disk}"
    );
    assert!(
        on_disk.contains(",#+end_example\n,* Phantom Heading\n"),
        "adversarial lines must be comma-escaped: {on_disk}"
    );
    assert!(
        !on_disk.contains("* Phantom Heading\n\n** Acceptance Criteria"),
        "phantom heading must not escape wrapper: {on_disk}"
    );

    let file = OrgFile::parse(&on_disk, "backlog.org").unwrap();
    assert_eq!(
        heading_ids(&file),
        vec!["TASK-G01".to_string()],
        "structure must be unchanged"
    );

    let _ = running.shutdown.send(());
    let _ = running.join.await;
}

#[tokio::test]
async fn default_body_format_still_rejects_phantom_heading() {
    let tmp = tempfile::tempdir().unwrap();
    let home = Home::at(tmp.path().join("home"));
    home.ensure().unwrap();
    let project_root = tmp.path().join("proj");
    seed_project_with_sections(&home, &project_root, "rawdefaultreject");

    let running = Daemon::run(home.clone(), test_options())
        .await
        .expect("boot daemon");
    let token = read_token(&home);
    let client = reqwest::Client::new();
    let base = format!("http://{}", running.addr);

    let resp = client
        .post(format!("{base}/api/org/node/TASK-G01/edit"))
        .bearer_auth(&token)
        .json(&serde_json::json!({
            "project": "rawdefaultreject",
            "kind": "task",
            "base_version": task_base_version(&client, &base, &token, "rawdefaultreject").await,
            "request_id": "raw-default-reject-test",
            "ops": [
                {
                    "op": "set_section_body",
                    "title": "Description",
                    "body": "* Phantom Heading\nsome text\n"
                }
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
        "default mode must still reject phantom heading: {body}"
    );
    common::assert_body_rejects_paths(&body, &[&project_root]);

    let _ = running.shutdown.send(());
    let _ = running.join.await;
}
