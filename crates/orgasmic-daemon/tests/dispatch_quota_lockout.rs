//! A remembered provider quota lockout refuses dispatch before acquire, and
//! the deliberate override is recorded on the dispatch tx (TASK-40ZMJ).

use std::path::{Path, PathBuf};

use chrono::{Duration, Utc};
use orgasmic_core::Home;
use orgasmic_daemon::provider_quota::{remember, ProviderLockout};
use orgasmic_daemon::{Daemon, DaemonOptions};

fn write(path: &Path, contents: impl AsRef<str>) {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).unwrap();
    }
    std::fs::write(path, contents.as_ref()).unwrap();
}

fn repo_root() -> PathBuf {
    let mut here = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    while !here.join("shipped/entry/router.org").is_file() {
        assert!(here.pop(), "could not locate repo root");
    }
    here
}

fn project_tx(project_root: &Path) -> String {
    let mut raw = String::new();
    for root in [
        project_root.join(".orgasmic/tx"),
        project_root.join(".orgasmic/machines"),
    ] {
        for entry in walkdir(&root) {
            if entry.extension().and_then(|ext| ext.to_str()) == Some("org") {
                raw.push_str(&std::fs::read_to_string(entry).unwrap());
            }
        }
    }
    raw
}

fn walkdir(root: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    let Ok(entries) = std::fs::read_dir(root) else {
        return out;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            out.extend(walkdir(&path));
        } else {
            out.push(path);
        }
    }
    out
}

#[tokio::test]
async fn quota_lockout_refuses_and_force_preflight_is_recorded() {
    let tmp = tempfile::tempdir().unwrap();
    let home = Home::at(tmp.path().join("home"));
    home.ensure().unwrap();
    std::os::unix::fs::symlink(repo_root(), home.source()).unwrap();
    let project_root = tmp.path().join("project");
    let task_id = "TASK-QUOTA-LOCKOUT";
    write(
        &project_root.join(format!(".orgasmic/tasks/{task_id}/node.org")),
        format!(
            "#+title: task\n#+orgasmic_version: 2\n\n* BACKLOG {task_id} Quota lockout\n:PROPERTIES:\n:ID: {task_id}\n:END:\n"
        ),
    );
    write(
        &home.board(),
        format!(
            "#+title: board\n#+orgasmic_version: 1\n\n* PROJECT quota-test\n:PROPERTIES:\n:ID: quota-test\n:PATH: {}\n:BRANCH: main\n:STATUS: active\n:END:\n",
            project_root.display()
        ),
    );
    let now = Utc::now();
    remember(
        &home,
        ProviderLockout {
            provider: "hermes".into(),
            locked_until: now + Duration::minutes(10),
            observed_at: now,
            run_id: "run-quota-source".into(),
            signal: "exit_reason.retry_after".into(),
        },
    )
    .unwrap();

    let running = Daemon::run(
        home.clone(),
        DaemonOptions {
            bind_override: Some("127.0.0.1".parse().unwrap()),
            port_override: Some(0),
            fs_watcher_enabled: false,
            ..DaemonOptions::default()
        },
    )
    .await
    .unwrap();
    let token = std::fs::read_to_string(home.auth_token()).unwrap();
    let brief = tmp.path().join("brief.md");
    let worktree = tmp.path().join("worktree");
    write(&brief, "quota lockout test\n");
    std::fs::create_dir_all(&worktree).unwrap();
    let request = |force_preflight| {
        serde_json::json!({
            "kind": "implementer",
            "runtime": "legacy",
            "mode": "ws",
            "harness": "hermes",
            "brief_path": brief,
            "worktree_path": worktree,
            "last_path": tmp.path().join("quota-last.txt"),
            "stdout_path": tmp.path().join("quota-stdout.log"),
            "branch": "task-quota-lockout",
            "liveness": "deadbeef",
            "allow_simulated": true,
            "force_preflight": force_preflight,
        })
    };
    let client = reqwest::Client::new();
    let url = format!(
        "http://{}/api/projects/quota-test/tasks/{task_id}/dispatch",
        running.addr
    );

    let refused = client
        .post(&url)
        .bearer_auth(token.trim())
        .json(&request(false))
        .send()
        .await
        .unwrap();
    assert_eq!(refused.status(), reqwest::StatusCode::BAD_REQUEST);
    let body = refused.text().await.unwrap();
    assert!(
        body.contains("provider_quota: hermes locked until"),
        "{body}"
    );
    assert!(walkdir(&project_root.join(".orgasmic/tmp/sessions")).is_empty());

    let forced = client
        .post(&url)
        .bearer_auth(token.trim())
        .json(&request(true))
        .send()
        .await
        .unwrap();
    let status = forced.status();
    let body = forced.text().await.unwrap();
    assert_eq!(status, reqwest::StatusCode::OK, "{body}");
    let tx = project_tx(&project_root);
    assert!(tx.contains(":FORCE_PREFLIGHT: true"), "{tx}");

    let _ = running.shutdown.send(());
    let _ = running.join.await;
}
