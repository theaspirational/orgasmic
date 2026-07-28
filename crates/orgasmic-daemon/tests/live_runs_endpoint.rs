//! `GET /api/runs/live` — the supervisor-local liveness answer.
//!
//! The control-path question "is anything running right now" used to be
//! answered by the recovery inventory, which reads every session file on the
//! board to classify durable history. That made a restart/update/close fence
//! depend on unrelated historical runs being readable.
//!
//! This regression pins the split at the endpoint: on a board whose session
//! files cannot be read at all, the live answer is still correct and complete,
//! while the inventory — asked about the same board in the same test — reports
//! that it could not read those files. Same daemon, same disk, two questions.

use std::path::Path;
use std::time::Duration;

use orgasmic_core::Home;
use orgasmic_daemon::{Daemon, DaemonOptions};
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
        &home.board(),
        format!(
            "#+title: orgasmic board\n#+orgasmic_version: 1\n\n* PROJECT {project_id}\n:PROPERTIES:\n:ID:               {project_id}\n:PATH:             {}\n:BRANCH:           main\n:STATUS:           active\n:END:\n",
            project_root.display()
        ),
    );
}

fn read_token(home: &Home) -> String {
    std::fs::read_to_string(home.auth_token())
        .expect("token file")
        .trim()
        .to_string()
}

/// Strip read permission from every session file on the board, leaving the
/// directory listable. This is the "unreadable durable history" board: the
/// inventory can see the records exist and cannot classify any of them.
#[cfg(unix)]
fn make_session_files_unreadable(project_root: &Path) -> usize {
    use std::os::unix::fs::PermissionsExt;
    let sessions = project_root.join(".orgasmic/tmp/sessions");
    let mut count = 0;
    for entry in std::fs::read_dir(&sessions).expect("sessions dir must exist") {
        let path = entry.unwrap().path();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o000)).unwrap();
        count += 1;
    }
    count
}

async fn get_json(addr: std::net::SocketAddr, token: &str, path: &str) -> (u16, Value) {
    let response = reqwest::Client::new()
        .get(format!("http://{addr}{path}"))
        .bearer_auth(token)
        .timeout(Duration::from_secs(10))
        .send()
        .await
        .unwrap_or_else(|error| panic!("GET {path}: {error}"));
    let status = response.status().as_u16();
    let body = response.text().await.unwrap();
    (
        status,
        serde_json::from_str(&body).unwrap_or_else(|error| panic!("GET {path} body {body}: {error}")),
    )
}

// orgasmic:task_6HJYT
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[cfg(unix)]
async fn live_runs_answers_from_the_supervisor_when_no_session_file_can_be_read() {
    let tmp = tempfile::tempdir().unwrap();
    let home = Home::at(tmp.path().join("home"));
    home.ensure().unwrap();
    let project_root = tmp.path().join("project");
    seed_board(&home, &project_root, "orgasmic");

    let running = Daemon::run(home.clone(), test_options())
        .await
        .expect("boot daemon");
    let token = read_token(&home);

    // One live run of exactly the shape the stop/restart fence guards against:
    // an interactive manager terminal.
    let registered: Value = reqwest::Client::new()
        .post(format!("http://{}/api/manager/register", running.addr))
        .bearer_auth(&token)
        .json(&serde_json::json!({
            "project_id": "orgasmic",
            "pid": std::process::id(),
        }))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(
        registered["status"], "registered",
        "manager register: {registered}"
    );
    let run_id = registered["run_id"].as_str().expect("run id").to_string();

    let unreadable = make_session_files_unreadable(&project_root);
    assert!(
        unreadable > 0,
        "the board must carry at least one session file for this to mean anything"
    );

    let (status, live) = get_json(running.addr, &token, "/api/runs/live").await;
    assert_eq!(status, 200, "live runs: {live}");
    let live_ids: Vec<&str> = live["live"]
        .as_array()
        .expect("live must be an array")
        .iter()
        .map(|run| run["run_id"].as_str().unwrap())
        .collect();
    assert!(
        live_ids.contains(&run_id.as_str()),
        "the live manager run must be reported when no session file is readable: {live}"
    );
    assert!(
        live["live"][0]["task_id"]
            .as_str()
            .is_some_and(|task| task.starts_with("manager.launch:")),
        "liveness answer must carry run identity the fence filters on: {live}"
    );
    // The liveness answer is not the inventory: no classifications, no scan.
    for absent in ["interrupted", "reattached", "failed_recoverable", "ambiguous", "inventory"] {
        assert!(
            live.get(absent).is_none(),
            "liveness answer must not carry `{absent}`: {live}"
        );
    }

    // Same board, same moment, the other question: the inventory genuinely
    // cannot read this history. That is what the fence no longer depends on.
    let (status, inventory) = get_json(running.addr, &token, "/api/runs").await;
    assert_eq!(status, 200, "inventory: {inventory}");
    assert!(
        inventory["inventory"]["unreadable_sessions"]
            .as_u64()
            .unwrap_or(0)
            > 0,
        "the inventory must report this board as unreadable, or the contrast is vacuous: {inventory}"
    );

    let _ = running.shutdown.send(());
    let _ = running.join.await;
}
