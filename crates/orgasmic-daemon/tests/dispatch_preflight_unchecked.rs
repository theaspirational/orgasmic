//! An admitted-but-unchecked preflight must leave its trace in the dispatch's
//! own evidence (TASK-AP298).
//!
//! When the credential probe cannot answer — the harness binary is missing, or
//! it does not answer inside the probe's budget — the dispatch is admitted per
//! dec_7P79C. That is deliberate and unchanged here. What was missing is the
//! record: `RunMeta` and the `manager.dispatch_started` tx said only that a
//! plan was pinned, so a run that died at startup could not say "admitted
//! without a credential check" without the operator correlating daemon-log
//! timestamps. This binary drives both unchecked outcomes through the real
//! dispatch endpoint and reads the verdict back from both surfaces.
//!
//! One test function, two dispatches in sequence, because each phase pins
//! `PATH` — process-global state shared by every test in a binary
//! (`.orgasmic/gotchas.org`). The timeout phase costs the probe's full budget
//! (two attempts of the status timeout) by construction: the thing under test
//! is a child that never answers.

use std::path::{Path, PathBuf};

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

fn write(path: &Path, contents: impl AsRef<str>) {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).unwrap();
    }
    std::fs::write(path, contents.as_ref()).unwrap();
}

fn repo_root() -> PathBuf {
    let mut here = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    loop {
        if here.join("shipped/entry/router.org").is_file() {
            return here;
        }
        if !here.pop() {
            panic!("could not locate orgasmic repo root from CARGO_MANIFEST_DIR");
        }
    }
}

fn read_token(home: &Home) -> String {
    std::fs::read_to_string(home.auth_token())
        .expect("token file")
        .trim()
        .to_string()
}

fn seed_worker(home: &Home, id: &str) {
    write(
        &home.user().join(format!("workers/{id}.org")),
        format!(
            "* WORKER {id}\n:PROPERTIES:\n:ID:                          {id}\n:KIND:                        implementer\n:DRIVER:                      stdio\n:HARNESS:                     claude\n:PROVIDERS:                   anthropic\n:DEFAULT_PROVIDER:            anthropic\n:LINKED_SKILLS:\n:APPLICABLE_STATES:           working, done, blocked, cancelled\n:MAX_ITERATIONS:              1\n:CONTEXT_BUDGET:              4000\n:VERSION:                     1\n:END:\n\n** Persona\nUnchecked-preflight test worker.\n\n** Operating Rules\n- Keep the test run minimal.\n"
        ),
    );
}

fn seed_project(home: &Home, project_root: &Path, project_id: &str, task_ids: &[&str]) {
    if !home.source().exists() {
        std::os::unix::fs::symlink(repo_root(), home.source()).unwrap();
    }
    for task_id in task_ids {
        write(
            &project_root.join(format!(".orgasmic/tasks/{task_id}/node.org")),
            format!(
                "#+title: orgasmic task {task_id}\n#+orgasmic_version: 2\n\n* BACKLOG {task_id} Unchecked preflight task\n:PROPERTIES:\n:ID:               {task_id}\n:END:\n"
            ),
        );
    }
    write(
        &home.board(),
        format!(
            "#+title: orgasmic board\n#+orgasmic_version: 1\n\n* PROJECT {project_id}\n:PROPERTIES:\n:ID:               {project_id}\n:PATH:             {}\n:BRANCH:           main\n:STATUS:           active\n:END:\n",
            project_root.display()
        ),
    );
}

/// A `claude` whose `auth status` never answers, and which otherwise behaves
/// as a worker that completes at once. The probe gives up after its budget and
/// the dispatch is admitted; the launch then runs this same binary.
fn write_silent_status_claude_stub(dir: &Path) {
    let stub = dir.join("claude");
    std::fs::write(
        &stub,
        r#"#!/bin/sh
if [ "$1" = "auth" ] && [ "$2" = "status" ]; then
  sleep 120
  exit 0
fi
if [ "$1" = "--version" ]; then
  exit 0
fi
printf '%s\n' '{"type":"system","subtype":"init","session_id":"stub-session","model":"stub-model","claude_code_version":"stub"}'
printf '%s\n' '{"type":"result","subtype":"success","result":"stub complete"}'
"#,
    )
    .unwrap();
    use std::os::unix::fs::PermissionsExt;
    let mut perms = std::fs::metadata(&stub).unwrap().permissions();
    perms.set_mode(0o755);
    std::fs::set_permissions(&stub, perms).unwrap();
    // Pay the first-exec cost here rather than inside the probe's budget
    // (TASK-GEZHQ); `--version` is the one argv the stub answers instantly.
    let output = std::process::Command::new(&stub)
        .arg("--version")
        .output()
        .expect("the stub must be executable");
    assert!(output.status.success(), "the stub must run its own script");
}

/// Project tx, from the legacy `.orgasmic/tx/` and the per-machine
/// `.orgasmic/machines/<machine-id>/tx/` the daemon writes since TASK-MSYN4.
fn read_project_tx(project_root: &Path) -> String {
    let dotorg = project_root.join(".orgasmic");
    let mut tx_dirs = vec![dotorg.join("tx")];
    if let Ok(machines) = std::fs::read_dir(dotorg.join("machines")) {
        tx_dirs.extend(machines.flatten().map(|machine| machine.path().join("tx")));
    }
    let mut raw = String::new();
    for tx_dir in tx_dirs {
        if let Ok(entries) = std::fs::read_dir(&tx_dir) {
            for entry in entries.flatten() {
                if entry.path().extension().and_then(|ext| ext.to_str()) == Some("org") {
                    raw.push_str(&std::fs::read_to_string(entry.path()).unwrap_or_default());
                }
            }
        }
    }
    raw
}

fn session_lines(project_root: &Path) -> Vec<serde_json::Value> {
    let sessions_dir = project_root.join(".orgasmic/tmp/sessions");
    let mut out = Vec::new();
    let Ok(entries) = std::fs::read_dir(&sessions_dir) else {
        return out;
    };
    for entry in entries.flatten() {
        let raw = std::fs::read_to_string(entry.path()).unwrap_or_default();
        for line in raw.lines() {
            if let Ok(value) = serde_json::from_str::<serde_json::Value>(line) {
                out.push(value);
            }
        }
    }
    out
}

/// The `manager.dispatch_started` entry for `task_id`, as raw org text.
fn dispatch_started_entry(tx: &str, task_id: &str) -> String {
    tx.split("\n* ")
        .find(|entry| entry.contains("manager.dispatch_started") && entry.contains(task_id))
        .unwrap_or_else(|| panic!("no manager.dispatch_started for {task_id} in:\n{tx}"))
        .to_string()
}

/// The value of an org property on one tx entry.
fn property(entry: &str, key: &str) -> Option<String> {
    let prefix = format!(":{key}:");
    entry
        .lines()
        .find_map(|line| line.trim_start().strip_prefix(&prefix))
        .map(|value| value.trim().to_string())
}

/// Dispatch `task_id` and return the admitted run's `RunMeta` event.
async fn dispatch_and_read_run_meta(
    running: &RunningDaemon,
    token: &str,
    tmp: &Path,
    project_root: &Path,
    worker_id: &str,
    task_id: &str,
) -> serde_json::Value {
    let brief = tmp.join(format!("{task_id}-brief.md"));
    let worktree = tmp.join(format!("{task_id}-worktree"));
    write(&brief, "unchecked preflight dispatch brief\n");
    std::fs::create_dir_all(&worktree).unwrap();
    let response = reqwest::Client::new()
        .post(format!(
            "http://{}/api/projects/proj-unchecked/tasks/{task_id}/dispatch",
            running.addr
        ))
        .bearer_auth(token)
        .json(&serde_json::json!({
            "kind": "implementer",
            "mode": "stdio",
            "harness": "claude",
            "brief_path": brief,
            "worktree_path": worktree,
            "last_path": tmp.join(format!("{task_id}-last.txt")),
            "stdout_path": tmp.join(format!("{task_id}-stdout.log")),
            "worker_id": worker_id,
            "branch": format!("branch-{task_id}"),
            "liveness": "deadbeef",
            "reason": "unchecked preflight test",
        }))
        .send()
        .await
        .unwrap();
    let status = response.status();
    let body = response.text().await.unwrap();
    assert_eq!(
        status,
        reqwest::StatusCode::OK,
        "an inconclusive probe must still admit the dispatch (dec_7P79C): {body}"
    );
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(20);
    loop {
        let found = session_lines(project_root).into_iter().find(|line| {
            line["event"]["phase"] == "run_meta"
                && line["event"]["last_path"]
                    .as_str()
                    .is_some_and(|path| path.contains(task_id))
        });
        if let Some(found) = found {
            return found;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "no RunMeta lifecycle event was written for {task_id}"
        );
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }
}

/// Both unchecked outcomes, read back from the run's own record: `RunMeta`
/// says the verdict, and so does the `manager.dispatch_started` tx.
#[tokio::test]
async fn an_unchecked_admission_is_written_into_run_meta_and_the_dispatch_tx() {
    // Process-global, and safe here only because this binary holds one test.
    std::env::remove_var("ORGASMIC_DRIVER_SIMULATE");
    std::env::remove_var("ANTHROPIC_API_KEY");
    let settings_dir = tempfile::tempdir().unwrap();
    std::env::set_var("CLAUDE_CONFIG_DIR", settings_dir.path());
    let saved_path = std::env::var("PATH").unwrap_or_default();

    let tmp = tempfile::tempdir().unwrap();
    let home = Home::at(tmp.path().join("home"));
    home.ensure().unwrap();
    let project_root = tmp.path().join("proj");
    let worker_id = "implementer-claude-stdio";
    let spawn_failed_task = "TASK-PREFLIGHT-SPAWN-FAILED";
    let timeout_task = "TASK-PREFLIGHT-TIMEOUT";
    seed_worker(&home, worker_id);
    seed_project(
        &home,
        &project_root,
        "proj-unchecked",
        &[spawn_failed_task, timeout_task],
    );

    let running = Daemon::run(home.clone(), test_options())
        .await
        .expect("boot daemon");
    let token = read_token(&home);

    // Phase 1: no `claude` anywhere on PATH. The probe cannot spawn it, and the
    // launch falls back to the simulated worker for the same reason.
    let empty_dir = tempfile::tempdir().unwrap();
    std::env::set_var(
        "PATH",
        format!("{}:/usr/bin:/bin", empty_dir.path().display()),
    );
    let spawn_failed_meta = dispatch_and_read_run_meta(
        &running,
        &token,
        tmp.path(),
        &project_root,
        worker_id,
        spawn_failed_task,
    )
    .await;

    // Phase 2: a `claude` that never answers `auth status`.
    let harness_dir = tempfile::tempdir().unwrap();
    write_silent_status_claude_stub(harness_dir.path());
    std::env::set_var(
        "PATH",
        format!("{}:/usr/bin:/bin", harness_dir.path().display()),
    );
    let timeout_meta = dispatch_and_read_run_meta(
        &running,
        &token,
        tmp.path(),
        &project_root,
        worker_id,
        timeout_task,
    )
    .await;

    std::env::set_var("PATH", &saved_path);
    std::env::remove_var("CLAUDE_CONFIG_DIR");

    assert_eq!(
        spawn_failed_meta["event"]["preflight"], "unchecked:spawn_failed",
        "RunMeta must say the probe could not be spawned: {spawn_failed_meta}"
    );
    assert_eq!(
        timeout_meta["event"]["preflight"], "unchecked:timeout",
        "RunMeta must say the probe timed out: {timeout_meta}"
    );

    let tx = read_project_tx(&project_root);
    let spawn_failed_tx = dispatch_started_entry(&tx, spawn_failed_task);
    assert_eq!(
        property(&spawn_failed_tx, "PREFLIGHT").as_deref(),
        Some("unchecked:spawn_failed"),
        "the dispatch tx must say the probe could not be spawned: {spawn_failed_tx}"
    );
    let timeout_tx = dispatch_started_entry(&tx, timeout_task);
    assert_eq!(
        property(&timeout_tx, "PREFLIGHT").as_deref(),
        Some("unchecked:timeout"),
        "the dispatch tx must say the probe timed out: {timeout_tx}"
    );

    let _ = running.shutdown.send(());
    let _ = running.join.await;
}
