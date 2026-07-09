//! Regression for TASK-7R3NS: dispatch worktrees under `.orgasmic/tmp/dispatch/`
//! contain a nested `.orgasmic/` checkout that must not be indexed or parsed.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

use orgasmic_core::{mint_node_id, Home, NodeIdClass};
use orgasmic_daemon::{Daemon, DaemonOptions};

fn test_options() -> DaemonOptions {
    DaemonOptions {
        bind_override: Some("127.0.0.1".parse().unwrap()),
        port_override: Some(0),
        fs_watcher_enabled: true,
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
    std::fs::read_to_string(home.auth_token())
        .expect("auth token")
        .trim()
        .to_string()
}

fn run_git(project_root: &Path, args: &[&str]) {
    let output = Command::new("git")
        .args(args)
        .current_dir(project_root)
        .output()
        .expect("run git");
    assert!(
        output.status.success(),
        "git {:?} failed\nstdout={}\nstderr={}",
        args,
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

fn init_git_project(project_root: &Path) {
    run_git(project_root, &["init", "-b", "main"]);
    run_git(
        project_root,
        &["config", "user.email", "tester@example.com"],
    );
    run_git(project_root, &["config", "user.name", "Test User"]);
    run_git(project_root, &["add", "."]);
    run_git(project_root, &["commit", "-m", "init"]);
}

fn add_dispatch_worktree(project_root: &Path, stem: &str, branch: &str) -> PathBuf {
    let stem_dir = project_root.join(".orgasmic/tmp/dispatch").join(stem);
    std::fs::create_dir_all(&stem_dir).unwrap();
    let worktree = stem_dir.join("worktree");
    run_git(
        project_root,
        &[
            "worktree",
            "add",
            "-b",
            branch,
            worktree.to_str().unwrap(),
            "HEAD",
        ],
    );
    worktree
}

fn seed_board(home: &Home, project_root: &Path, project_id: &str) {
    write(
        &home.board(),
        format!(
            "#+title: board\n\n* {project_id} Test\n:PROPERTIES:\n:ID: {project_id}\n:PATH: {}\n:BRANCH: main\n:STATUS: active\n:END:\n",
            project_root.display()
        ),
    );
}

fn seed_project(project_root: &Path) {
    write(
        &project_root.join(".orgasmic/project.org"),
        "#+title: orgasmic\n#+orgasmic_version: 1\n\n* PROJECT orgasmic\n:PROPERTIES:\n:ID: orgasmic\n:END:\n",
    );
    write(
        &project_root.join(".orgasmic/tasks/backlog.org"),
        "#+title: sprint\n#+orgasmic_version: 1\n\n* BACKLOG TASK-NESTED Parent task\n:PROPERTIES:\n:ID: TASK-NESTED\n:END:\n",
    );
}

async fn parse_error_messages(running_addr: &str, token: &str) -> Vec<String> {
    let client = reqwest::Client::new();
    let resp = client
        .get(format!("http://{running_addr}/api/graph/parse-errors"))
        .bearer_auth(token)
        .send()
        .await
        .expect("parse errors");
    assert!(resp.status().is_success());
    let errors: Vec<serde_json::Value> = resp.json().await.expect("json");
    errors
        .iter()
        .filter_map(|e| e["message"].as_str().map(str::to_string))
        .collect()
}

#[tokio::test]
async fn nested_dispatch_worktree_org_is_not_indexed_or_parsed() {
    let tmp = tempfile::tempdir().unwrap();
    let home = Home::at(tmp.path().join("home"));
    home.ensure().unwrap();
    let project_root = tmp.path().join("project");
    std::fs::create_dir_all(&project_root).unwrap();
    seed_project(&project_root);
    init_git_project(&project_root);
    seed_board(&home, &project_root, "proj-nested");

    let worktree = add_dispatch_worktree(&project_root, "task-nested", "task-nested-impl");
    assert!(worktree.join(".orgasmic/tasks/backlog.org").is_file());

    let decoy_id = mint_node_id(NodeIdClass::Task);
    write(
        &worktree.join(".orgasmic/tasks/backlog.org"),
        format!(
            "#+title: nested backlog\n#+orgasmic_version: 1\n\n* BACKLOG {decoy_id} Nested decoy\n:PROPERTIES:\n:ID: {decoy_id}\n:END:\n"
        ),
    );
    write(
        &worktree.join("crates/decoy/src/lib.rs"),
        "// orgasmic:dec_999\npub fn nested_marker() {}\n",
    );

    let running = Daemon::run(home.clone(), test_options())
        .await
        .expect("boot daemon");
    let token = read_token(&home);
    let client = reqwest::Client::new();

    let projects: Vec<serde_json::Value> = client
        .get(format!("http://{}/api/projects", running.addr))
        .bearer_auth(&token)
        .send()
        .await
        .expect("projects")
        .json()
        .await
        .expect("projects json");
    assert_eq!(
        projects.len(),
        1,
        "nested worktree .orgasmic must not register as a second project"
    );

    let tasks: Vec<serde_json::Value> = client
        .get(format!(
            "http://{}/api/projects/proj-nested/tasks",
            running.addr
        ))
        .bearer_auth(&token)
        .send()
        .await
        .expect("tasks")
        .json()
        .await
        .expect("tasks json");
    let task_ids: Vec<String> = tasks
        .iter()
        .filter_map(|task| task["id"].as_str().map(str::to_string))
        .collect();
    assert!(
        task_ids.contains(&"TASK-NESTED".to_string()),
        "parent project tasks should still load: {task_ids:?}"
    );
    assert!(
        !task_ids.contains(&decoy_id),
        "nested worktree org files must not be parsed into the index: {task_ids:?}"
    );

    let msgs = parse_error_messages(&running.addr.to_string(), &token).await;
    assert!(
        !msgs
            .iter()
            .any(|m| m.contains("duplicate identity") && m.contains(&decoy_id)),
        "nested duplicate identity must not surface via index lint: {msgs:?}"
    );
    assert!(
        !msgs.iter().any(|m| m.contains("dec_999")),
        "nested worktree code markers must not be scanned (dangling marker leak): {msgs:?}"
    );

    write(
        &worktree.join(".orgasmic/tasks/backlog.org"),
        "#+title: nested sprint\n#+orgasmic_version: 1\n\n* BACKLOG TASK-TOUCH Watcher touch\n:PROPERTIES:\n:ID: TASK-TOUCH\n:END:\n",
    );
    tokio::time::sleep(Duration::from_millis(800)).await;

    let msgs_after_touch = parse_error_messages(&running.addr.to_string(), &token).await;
    assert!(
        !msgs_after_touch
            .iter()
            .any(|m| m.contains("duplicate identity") && m.contains(&decoy_id)),
        "watcher refresh must not parse nested worktree org files: {msgs_after_touch:?}"
    );
    let tasks_after_touch: Vec<serde_json::Value> = client
        .get(format!(
            "http://{}/api/projects/proj-nested/tasks",
            running.addr
        ))
        .bearer_auth(&token)
        .send()
        .await
        .expect("tasks after touch")
        .json()
        .await
        .expect("tasks json");
    let task_ids_after_touch: Vec<String> = tasks_after_touch
        .iter()
        .filter_map(|task| task["id"].as_str().map(str::to_string))
        .collect();
    assert!(
        !task_ids_after_touch.contains(&"TASK-TOUCH".to_string()),
        "watcher must not ingest nested backlog.org edits: {task_ids_after_touch:?}"
    );

    let _ = running.shutdown.send(());
    running.join.await.unwrap();
}
