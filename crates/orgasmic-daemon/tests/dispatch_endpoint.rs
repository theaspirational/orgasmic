use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

mod common;

use common::{assert_path_free_error, assert_path_free_error_response};
use orgasmic_core::{
    read_session_file, Home, Lifecycle, SandboxAllowlist, SessionEventKind, WorkerKind,
};
use orgasmic_daemon::{Daemon, DaemonOptions, RunningDaemon};
use orgasmic_drivers::test_tooling::{
    assert_required_test_tooling, skip_test_if_missing, test_environment_lock, ToolRequirement,
};
use orgasmic_drivers::{allowlist_from_driver_config, DriverConfig};

#[cfg(unix)]
#[test]
fn required_test_tooling_is_present() {
    assert_required_test_tooling(&[ToolRequirement::new("cc", 2, cc_available_for_test())]);
}

fn test_options() -> DaemonOptions {
    DaemonOptions {
        bind_override: Some("127.0.0.1".parse().unwrap()),
        port_override: Some(0),
        // Dispatch tests poll the project tx/session files directly from disk;
        // none assert on the `notify` board watcher, so disabling it avoids the
        // ~0.8s-per-watch macOS FSEvents registration latency at boot.
        fs_watcher_enabled: false,
        ..DaemonOptions::default()
    }
}

async fn boot(home: Home) -> RunningDaemon {
    Daemon::run(home, test_options())
        .await
        .expect("boot daemon")
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
        if here.join(".orgasmic").is_dir() && here.join("shipped").is_dir() {
            return here;
        }
        if !here.pop() {
            panic!("could not locate orgasmic repo root from CARGO_MANIFEST_DIR");
        }
    }
}

fn symlink_repo_source(home: &Home) {
    if home.source().exists() {
        return;
    }
    std::os::unix::fs::symlink(repo_root(), home.source()).unwrap();
}

fn read_token(home: &Home) -> String {
    std::fs::read_to_string(home.auth_token())
        .expect("token file")
        .trim()
        .to_string()
}

/// The label the daemon will mint for a dispatch address.
///
/// There is nothing to seed: worker templates were retired, `user/workers/` is
/// ignored at boot, and `DispatchRequest` has no `worker_id` field. The label
/// on the response, the tx record, and the run inventory is derived from
/// `(kind, mode, harness)` alone, so tests derive it the same way rather than
/// hardcoding a name that can silently stop matching.
fn worker_label(kind: WorkerKind, mode: &str, harness: &str) -> String {
    orgasmic_daemon::addressing::compatibility_worker_id(kind, mode, harness)
}

fn seed_project(
    home: &Home,
    project_root: &Path,
    project_id: &str,
    _worker_id: &str,
    task_id: &str,
) {
    symlink_repo_source(home);
    write(
        &project_root.join(format!(".orgasmic/tasks/{task_id}/node.org")),
        format!(
            "#+title: orgasmic task {task_id}\n#+orgasmic_version: 2\n\n* BACKLOG {task_id} Dispatch endpoint task\n:PROPERTIES:\n:ID:               {task_id}\n:END:\n"
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

fn seed_project_with_task_worker(
    home: &Home,
    project_root: &Path,
    project_id: &str,
    _pipeline_worker_id: &str,
    task_worker_id: &str,
    task_id: &str,
) {
    symlink_repo_source(home);
    write(
        &project_root.join(format!(".orgasmic/tasks/{task_id}/node.org")),
        format!(
            "#+title: orgasmic task {task_id}\n#+orgasmic_version: 2\n\n* BACKLOG {task_id} Task with pinned worker\n:PROPERTIES:\n:ID:               {task_id}\n:WORKER:           {task_worker_id}\n:END:\n"
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

async fn get_runs_json(running: &RunningDaemon, token: &str) -> serde_json::Value {
    reqwest::Client::new()
        .get(format!("http://{}/api/runs", running.addr))
        .bearer_auth(token)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap()
}

fn collect_project_tx(project_root: &Path) -> String {
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

fn auto_spawn_session_paths(project_root: &Path) -> Vec<PathBuf> {
    let sessions_dir = project_root.join(".orgasmic/tmp/sessions");
    let mut paths = Vec::new();
    if let Ok(entries) = std::fs::read_dir(&sessions_dir) {
        for entry in entries.flatten() {
            let path = entry.path();
            if path
                .file_name()
                .and_then(|name| name.to_str())
                .is_some_and(|name| name.starts_with("auto-spawn-"))
            {
                paths.push(path);
            }
        }
    }
    paths
}

fn live_runs_for_task(runs: &serde_json::Value, task_id: &str) -> usize {
    runs["live"]
        .as_array()
        .unwrap_or(&Vec::new())
        .iter()
        .filter(|run| run["task_id"].as_str() == Some(task_id))
        .count()
}

fn sandbox_from_session_run_meta(session_path: &Path) -> SandboxAllowlist {
    let envelopes = read_session_file(session_path).expect("read session");
    for envelope in envelopes {
        if envelope.kind != SessionEventKind::Lifecycle {
            continue;
        }
        if let Ok(Lifecycle::RunMeta { driver_config, .. }) = serde_json::from_value(envelope.event)
        {
            let cfg = DriverConfig::from_value(driver_config);
            return allowlist_from_driver_config(&cfg).expect("sandbox_permissions in RunMeta");
        }
    }
    panic!("RunMeta missing from {}", session_path.display());
}

async fn post_dispatch(
    running: &RunningDaemon,
    token: &str,
    project_id: &str,
    task_id: &str,
    body: serde_json::Value,
) -> reqwest::Response {
    reqwest::Client::new()
        .post(format!(
            "http://{}/api/projects/{}/tasks/{}/dispatch",
            running.addr, project_id, task_id
        ))
        .bearer_auth(token)
        .json(&body)
        .send()
        .await
        .unwrap()
}

fn transport_for_worker_label(worker_id: Option<&str>) -> (&'static str, &'static str) {
    match worker_id {
        Some(id) if id.contains("cursor") => ("subprocess-stream-json", "cursor-agent"),
        Some(id) if id.contains("stdio") => ("stdio", "codex"),
        _ => ("ws", "codex"),
    }
}

fn dispatch_body(
    kind: &str,
    brief: &Path,
    worktree: &Path,
    last: &Path,
    stdout: &Path,
    worker_id: Option<&str>,
) -> serde_json::Value {
    // `worker_id` selects the transport the caller wants and names the label
    // the daemon will mint back. It is deliberately NOT sent: `DispatchRequest`
    // has no such field, so a `worker_id` in the body is silently dropped —
    // which is how these tests came to assert a name nothing produced.
    let (mode, harness) = transport_for_worker_label(worker_id);
    serde_json::json!({
        "kind": kind,
        "mode": mode,
        "harness": harness,
        "brief_path": brief,
        "worktree_path": worktree,
        "last_path": last,
        "stdout_path": stdout,
        "branch": "task-dispatch-impl",
        "liveness": "deadbeef",
        "reason": "dispatch endpoint test",
    })
}

async fn wait_for_run_created(project_root: &Path, worker_id: &str) -> String {
    let deadline = std::time::Instant::now() + Duration::from_secs(2);
    while std::time::Instant::now() < deadline {
        let mut raw = String::new();
        let tx_dir = project_root.join(".orgasmic/tx");
        if let Ok(entries) = std::fs::read_dir(&tx_dir) {
            for entry in entries.flatten() {
                if entry.path().extension().and_then(|ext| ext.to_str()) == Some("org") {
                    raw.push_str(&std::fs::read_to_string(entry.path()).unwrap_or_default());
                }
            }
        }
        if raw.contains(":TYPE:         run.created")
            && raw.contains(":ORIGIN:")
            && raw.contains("cli_dispatch")
            && raw.contains(worker_id)
        {
            return raw;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    panic!("timed out waiting for cli_dispatch run.created");
}

fn cursor_agent_available() -> bool {
    Command::new("which")
        .arg("cursor-agent")
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .map(|status| status.success())
        .unwrap_or(false)
}

fn codex_available() -> bool {
    Command::new("which")
        .arg("codex")
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .map(|status| status.success())
        .unwrap_or(false)
}

fn terminate_pid(pid: u64) {
    if pid == 0 {
        return;
    }
    let _ = Command::new("kill")
        .arg("-TERM")
        .arg(pid.to_string())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status();
}

/// Pins every label this file asserts on to the daemon's own naming rule.
///
/// These tests previously named workers that `user/workers/` was expected to
/// supply. Templates were retired, the directory is ignored at boot, and the
/// assertions kept passing a name the daemon never mints — so three tests
/// failed on every run for a reason unrelated to what they were testing. This
/// test fails first, and loudly, if the rule moves again.
#[test]
fn asserted_worker_labels_match_the_daemons_naming_rule() {
    for (label, kind, mode, harness) in [
        (
            "implementer-codex-ws",
            WorkerKind::Implementer,
            "ws",
            "codex",
        ),
        (
            "implementer-codex-stdio",
            WorkerKind::Implementer,
            "stdio",
            "codex",
        ),
        (
            "implementer-cursor-agent-subprocess-stream-json",
            WorkerKind::Implementer,
            "subprocess-stream-json",
            "cursor-agent",
        ),
        ("reviewer-codex-ws", WorkerKind::Reviewer, "ws", "codex"),
    ] {
        assert_eq!(worker_label(kind, mode, harness), label);
    }
}

#[tokio::test]
async fn dispatch_endpoint_routes_codex_through_supervisor_and_emits_run_created() {
    let tmp = tempfile::tempdir().unwrap();
    let home = Home::at(tmp.path().join("home"));
    home.ensure().unwrap();
    symlink_repo_source(&home);
    let project_root = tmp.path().join("proj");
    let worker_id = "implementer-codex-ws";
    let task_id = "TASK-CODEX";
    seed_project(&home, &project_root, "proj-dispatch", worker_id, task_id);
    let brief = tmp.path().join("brief.md");
    let worktree = tmp.path().join("worktree");
    let last = tmp.path().join("last.txt");
    let stdout = tmp.path().join("stdout.log");
    write(&brief, "codex dispatch brief\n");
    std::fs::create_dir_all(&worktree).unwrap();

    let running = boot(home.clone()).await;
    let token = read_token(&home);
    let response = post_dispatch(
        &running,
        &token,
        "proj-dispatch",
        task_id,
        dispatch_body(
            "implementer",
            &brief,
            &worktree,
            &last,
            &stdout,
            Some(worker_id),
        ),
    )
    .await;
    assert!(
        response.status().is_success(),
        "dispatch: {}",
        response.status()
    );
    let body: serde_json::Value = response.json().await.unwrap();
    assert_eq!(body["worker_id"], worker_id);
    assert_eq!(body["driver"], "ws");
    assert_eq!(body["harness"], "codex");
    assert!(!body["run_id"].as_str().unwrap().is_empty());
    assert!(PathBuf::from(body["session_path"].as_str().unwrap()).exists());
    assert_eq!(body["pid"], 0, "ws does not own a child process");

    let raw = wait_for_run_created(&project_root, worker_id).await;
    assert!(raw.contains(":DRIVER:"));
    assert!(raw.contains("ws"));
    assert!(raw.contains(":HARNESS:"));
    assert!(raw.contains("codex"));

    let _ = running.shutdown.send(());
    let _ = running.join.await;
}

#[tokio::test]
async fn state_flip_to_in_progress_does_not_spawn_worker() {
    let tmp = tempfile::tempdir().unwrap();
    let home = Home::at(tmp.path().join("home"));
    home.ensure().unwrap();
    let project_root = tmp.path().join("proj");
    let pipeline_worker = "implementer-codex-ws";
    let task_worker = "pinned-implementer";
    let task_id = "TASK-NO-AUTOSPAWN";
    seed_project_with_task_worker(
        &home,
        &project_root,
        "proj-dispatch",
        pipeline_worker,
        task_worker,
        task_id,
    );

    let running = boot(home.clone()).await;
    let token = read_token(&home);
    let client = reqwest::Client::new();
    let resp = client
        .post(format!(
            "http://{}/api/projects/proj-dispatch/tasks/{task_id}?json=true",
            running.addr
        ))
        .bearer_auth(&token)
        .json(&serde_json::json!({
            "state": "in_progress",
            "request_id": "state-flip-no-autospawn",
        }))
        .send()
        .await
        .unwrap();
    assert!(resp.status().is_success(), "state flip: {}", resp.status());
    let updated: serde_json::Value = resp.json().await.unwrap();
    assert_eq!(updated["lifecycle_stage"], "in_progress");

    tokio::time::sleep(Duration::from_millis(250)).await;

    let runs = get_runs_json(&running, &token).await;
    assert_eq!(
        live_runs_for_task(&runs, task_id),
        0,
        "state flip must not create a live run: {runs}"
    );

    let tx_raw = collect_project_tx(&project_root);
    assert!(
        !tx_raw.contains("run.created"),
        "state flip must not emit run.created tx entries"
    );
    assert!(
        !tx_raw.contains("auto_spawn"),
        "state flip must not emit auto-spawn tx artifacts"
    );
    assert!(
        auto_spawn_session_paths(&project_root).is_empty(),
        "state flip must not create auto-spawn session files"
    );

    let _ = running.shutdown.send(());
    let _ = running.join.await;
}

#[tokio::test]
async fn dispatch_with_task_worker_property_still_spawns_via_explicit_path() {
    let tmp = tempfile::tempdir().unwrap();
    let home = Home::at(tmp.path().join("home"));
    home.ensure().unwrap();
    let project_root = tmp.path().join("proj");
    let pipeline_worker = "implementer-codex-ws";
    // A stale `:WORKER:` pin left on the task. It is a label the task carries,
    // never routing authority — the dispatch address decides, and the run must
    // come back under the address-derived id, not this one.
    let task_worker = "pinned-implementer";
    let dispatched_label = "implementer-codex-ws";
    let task_id = "TASK-WORKER-PIN";
    seed_project_with_task_worker(
        &home,
        &project_root,
        "proj-dispatch",
        pipeline_worker,
        task_worker,
        task_id,
    );
    let brief = tmp.path().join("brief.md");
    let worktree = tmp.path().join("worktree");
    let last = tmp.path().join("last.txt");
    let stdout = tmp.path().join("stdout.log");
    write(&brief, "explicit dispatch with task worker pin\n");
    std::fs::create_dir_all(&worktree).unwrap();

    let running = boot(home.clone()).await;
    let token = read_token(&home);
    let response = post_dispatch(
        &running,
        &token,
        "proj-dispatch",
        task_id,
        dispatch_body(
            "implementer",
            &brief,
            &worktree,
            &last,
            &stdout,
            Some(task_worker),
        ),
    )
    .await;
    assert!(
        response.status().is_success(),
        "dispatch: {}",
        response.status()
    );
    let body: serde_json::Value = response.json().await.unwrap();
    assert_eq!(
        body["worker_id"], dispatched_label,
        "the task's :WORKER: pin must not override the dispatch address"
    );
    assert_ne!(body["worker_id"], task_worker);
    assert!(!body["run_id"].as_str().unwrap().is_empty());
    assert!(PathBuf::from(body["session_path"].as_str().unwrap()).exists());

    let raw = wait_for_run_created(&project_root, dispatched_label).await;
    assert!(raw.contains("cli_dispatch"));
    assert!(!raw.contains("auto_spawn"));

    let runs = get_runs_json(&running, &token).await;
    assert_eq!(
        live_runs_for_task(&runs, task_id),
        1,
        "explicit dispatch must create a live run: {runs}"
    );

    let _ = running.shutdown.send(());
    let _ = running.join.await;
}

#[tokio::test]
async fn dispatch_endpoint_accepts_task_sandbox_override() {
    let tmp = tempfile::tempdir().unwrap();
    let home = Home::at(tmp.path().join("home"));
    home.ensure().unwrap();
    symlink_repo_source(&home);
    let project_root = tmp.path().join("proj");
    let worker_id = "implementer-codex-ws";
    let task_id = "TASK-SANDBOX-OVERRIDE";
    write(
        &home.user().join(format!("workers/{worker_id}.org")),
        format!(
            "* WORKER {worker_id}\n:PROPERTIES:\n:ID:                          {worker_id}\n:KIND:             implementer\n:DRIVER:                      ws\n:HARNESS:                     codex\n:PROVIDERS:                   openai\n:DEFAULT_PROVIDER:            openai\n:SANDBOX_PERMISSIONS:         allow_exec=true,allow_patch=true,allow_network=true,allow_writes_outside_cwd=true\n:LINKED_SKILLS:\n:APPLICABLE_STATES:           claimed, analyzing, implementing, testing, fixing\n:MAX_ITERATIONS:              1\n:CONTEXT_BUDGET:              4000\n:VERSION:                     1\n:END:\n\n** Persona\nTest dispatch worker.\n\n** Operating Rules\n- Keep the test run minimal.\n"
        ),
    );
    write(
        &project_root.join(format!(".orgasmic/tasks/{task_id}/node.org")),
        format!(
            "#+title: orgasmic task {task_id}\n#+orgasmic_version: 2\n\n* BACKLOG {task_id} Sandbox override task\n:PROPERTIES:\n:ID:               {task_id}\n:SANDBOX_PERMISSIONS: allow_exec=false,allow_patch=true,allow_network=true,allow_writes_outside_cwd=false\n:END:\n"
        ),
    );
    write(
        &home.board(),
        format!(
            "#+title: orgasmic board\n#+orgasmic_version: 1\n\n* PROJECT proj-dispatch\n:PROPERTIES:\n:ID:               proj-dispatch\n:PATH:             {}\n:BRANCH:           main\n:STATUS:           active\n:END:\n",
            project_root.display()
        ),
    );
    let brief = tmp.path().join("brief.md");
    let worktree = tmp.path().join("worktree");
    let last = tmp.path().join("last.txt");
    let stdout = tmp.path().join("stdout.log");
    write(&brief, "sandbox override dispatch brief\n");
    std::fs::create_dir_all(&worktree).unwrap();

    let running = boot(home.clone()).await;
    let token = read_token(&home);
    let response = post_dispatch(
        &running,
        &token,
        "proj-dispatch",
        task_id,
        dispatch_body(
            "implementer",
            &brief,
            &worktree,
            &last,
            &stdout,
            Some(worker_id),
        ),
    )
    .await;
    assert!(
        response.status().is_success(),
        "dispatch with task sandbox override: {}",
        response.status()
    );

    let _ = running.shutdown.send(());
    let _ = running.join.await;
}

#[tokio::test]
async fn dispatch_endpoint_sandbox_matrix_blocks_higher_layer_widening() {
    let tmp = tempfile::tempdir().unwrap();
    let home = Home::at(tmp.path().join("home"));
    home.ensure().unwrap();
    write(
        &home.config(),
        r#"
dispatch:
  implementer:
    sandbox_permissions:
      allow_network: false
  "implementer,codex":
    sandbox_permissions:
      allow_exec: false
      allow_patch: false
"#,
    );
    symlink_repo_source(&home);
    let project_root = tmp.path().join("proj");
    let task_id = "TASK-SANDBOX-MATRIX";
    write(
        &project_root.join(format!(".orgasmic/tasks/{task_id}/node.org")),
        format!(
            "#+title: orgasmic task {task_id}\n#+orgasmic_version: 2\n\n* BACKLOG {task_id} Sandbox matrix\n:PROPERTIES:\n:ID:               {task_id}\n:SANDBOX_PERMISSIONS: allow_exec=true,allow_patch=true,allow_network=true,allow_writes_outside_cwd=true\n:END:\n"
        ),
    );
    write(
        &home.board(),
        format!(
            "#+title: orgasmic board\n#+orgasmic_version: 1\n\n* PROJECT proj-dispatch\n:PROPERTIES:\n:ID:               proj-dispatch\n:PATH:             {}\n:BRANCH:           main\n:STATUS:           active\n:END:\n",
            project_root.display()
        ),
    );
    let brief = tmp.path().join("brief.md");
    let worktree = tmp.path().join("worktree");
    let last = tmp.path().join("last.txt");
    let stdout = tmp.path().join("stdout.log");
    write(&brief, "sandbox matrix dispatch brief\n");
    std::fs::create_dir_all(&worktree).unwrap();

    let running = boot(home.clone()).await;
    let token = read_token(&home);
    let mut body = dispatch_body("implementer", &brief, &worktree, &last, &stdout, None);
    body["mode"] = serde_json::json!("ws");
    body["harness"] = serde_json::json!("codex");
    body["governance"] = serde_json::json!({
        "sandbox_permissions": {
            "allow_network": true,
            "allow_exec": true,
            "allow_patch": true,
            "allow_writes_outside_cwd": false
        }
    });
    let response = post_dispatch(&running, &token, "proj-dispatch", task_id, body).await;
    assert!(
        response.status().is_success(),
        "dispatch with sandbox matrix: {}",
        response.status()
    );
    let resp: serde_json::Value = response.json().await.unwrap();
    let session_path = PathBuf::from(resp["session_path"].as_str().unwrap());
    wait_for_session_nonempty(&session_path).await;
    let sandbox = sandbox_from_session_run_meta(&session_path);
    assert!(!sandbox.allow_exec, "kind,harness layer must stay false");
    assert!(!sandbox.allow_network, "kind layer must stay false");
    assert!(
        !sandbox.allow_patch,
        "dispatch true must not widen kind,harness false"
    );
    assert!(
        !sandbox.allow_writes_outside_cwd,
        "dispatch false must restrict even when task is true"
    );

    let _ = running.shutdown.send(());
    let _ = running.join.await;
}

#[tokio::test]
async fn dispatch_endpoint_lease_held_returns_409() {
    let tmp = tempfile::tempdir().unwrap();
    let home = Home::at(tmp.path().join("home"));
    home.ensure().unwrap();
    let project_root = tmp.path().join("proj");
    let worker_id = "implementer-codex-ws";
    let task_id = "TASK-LEASE";
    seed_project(&home, &project_root, "proj-dispatch", worker_id, task_id);
    let brief = tmp.path().join("brief.md");
    let worktree = tmp.path().join("worktree");
    let last = tmp.path().join("last.txt");
    let stdout = tmp.path().join("stdout.log");
    write(&brief, "lease dispatch brief\n");
    std::fs::create_dir_all(&worktree).unwrap();

    let running = boot(home.clone()).await;
    let token = read_token(&home);
    let first = post_dispatch(
        &running,
        &token,
        "proj-dispatch",
        task_id,
        dispatch_body(
            "implementer",
            &brief,
            &worktree,
            &last,
            &stdout,
            Some(worker_id),
        ),
    )
    .await;
    assert!(
        first.status().is_success(),
        "first dispatch: {}",
        first.status()
    );

    let second = post_dispatch(
        &running,
        &token,
        "proj-dispatch",
        task_id,
        dispatch_body(
            "implementer",
            &brief,
            &worktree,
            &last,
            &stdout,
            Some(worker_id),
        ),
    )
    .await;
    assert_eq!(second.status(), reqwest::StatusCode::CONFLICT);
    let body: serde_json::Value = second.json().await.unwrap();
    assert!(body["error"]
        .as_str()
        .unwrap()
        .contains("another dispatch is already active"));

    let _ = running.shutdown.send(());
    let _ = running.join.await;
}

#[tokio::test]
#[ignore = "requires cursor-agent installed"]
async fn dispatch_endpoint_routes_cursor_agent_through_supervisor_and_emits_run_created() {
    let available = cursor_agent_available();
    if skip_test_if_missing(
        "dispatch_endpoint_routes_cursor_agent_through_supervisor_and_emits_run_created",
        &[("cursor-agent", available)],
    ) {
        assert_required_test_tooling(&[ToolRequirement::new("cursor-agent", 1, available)]);
        return;
    }

    let tmp = tempfile::tempdir().unwrap();
    let home = Home::at(tmp.path().join("home"));
    home.ensure().unwrap();
    let project_root = tmp.path().join("proj");
    let worker_id = "implementer-cursor-agent-subprocess-stream-json";
    let task_id = "TASK-CURSOR";
    seed_project(&home, &project_root, "proj-dispatch", worker_id, task_id);
    let brief = tmp.path().join("brief.md");
    let worktree = tmp.path().join("worktree");
    let last = tmp.path().join("last.txt");
    let stdout = tmp.path().join("stdout.log");
    write(&brief, "cursor dispatch brief\n");
    std::fs::create_dir_all(&worktree).unwrap();

    let running = boot(home.clone()).await;
    let token = read_token(&home);
    let response = post_dispatch(
        &running,
        &token,
        "proj-dispatch",
        task_id,
        dispatch_body(
            "implementer",
            &brief,
            &worktree,
            &last,
            &stdout,
            Some(worker_id),
        ),
    )
    .await;
    assert!(
        response.status().is_success(),
        "dispatch: {}",
        response.status()
    );
    let body: serde_json::Value = response.json().await.unwrap();
    assert_eq!(body["worker_id"], worker_id);
    assert_eq!(body["driver"], "subprocess-stream-json");
    assert_eq!(body["harness"], "cursor-agent");
    let pid = body["pid"].as_u64().unwrap();
    assert!(pid > 0);
    terminate_pid(pid);

    let raw = wait_for_run_created(&project_root, worker_id).await;
    assert!(raw.contains(":DRIVER:"));
    assert!(raw.contains("subprocess-stream-json"));
    assert!(raw.contains(":HARNESS:"));
    assert!(raw.contains("cursor-agent"));

    let _ = running.shutdown.send(());
    let _ = running.join.await;
}

#[tokio::test]
#[ignore = "requires codex installed"]
async fn dispatch_endpoint_routes_stdio_codex_through_supervisor() {
    let available = codex_available();
    if skip_test_if_missing(
        "dispatch_endpoint_routes_stdio_codex_through_supervisor",
        &[("codex", available)],
    ) {
        assert_required_test_tooling(&[ToolRequirement::new("codex", 1, available)]);
        return;
    }

    let tmp = tempfile::tempdir().unwrap();
    let home = Home::at(tmp.path().join("home"));
    home.ensure().unwrap();
    let project_root = tmp.path().join("proj");
    let worker_id = "implementer-codex-stdio";
    let task_id = "TASK-CODEX-STDIO";
    seed_project(&home, &project_root, "proj-dispatch", worker_id, task_id);
    let brief = tmp.path().join("brief.md");
    let worktree = tmp.path().join("worktree");
    let last = tmp.path().join("last.txt");
    let stdout = tmp.path().join("stdout.log");
    write(&brief, "codex stdio dispatch brief\n");
    std::fs::create_dir_all(&worktree).unwrap();

    let running = boot(home.clone()).await;
    let token = read_token(&home);
    let response = post_dispatch(
        &running,
        &token,
        "proj-dispatch",
        task_id,
        dispatch_body(
            "implementer",
            &brief,
            &worktree,
            &last,
            &stdout,
            Some(worker_id),
        ),
    )
    .await;
    assert!(
        response.status().is_success(),
        "dispatch: {}",
        response.status()
    );
    let body: serde_json::Value = response.json().await.unwrap();
    assert_eq!(body["worker_id"], worker_id);
    assert_eq!(body["driver"], "stdio");
    assert_eq!(body["harness"], "codex");
    let pid = body["pid"].as_u64().unwrap();
    assert!(pid > 0);
    terminate_pid(pid);

    let raw = wait_for_run_created(&project_root, worker_id).await;
    assert!(raw.contains(":DRIVER:"));
    assert!(raw.contains("stdio"));
    assert!(raw.contains(":HARNESS:"));
    assert!(raw.contains("codex"));

    let _ = running.shutdown.send(());
    let _ = running.join.await;
}

#[tokio::test]
async fn dispatch_unsupported_transport_is_path_free() {
    let tmp = tempfile::tempdir().unwrap();
    let home = Home::at(tmp.path().join("home"));
    home.ensure().unwrap();
    let project_root = tmp.path().join("proj");
    let worker_id = "implementer-codex-ws";
    let task_id = "TASK-UNSUPPORTED-TRANSPORT";
    seed_project(&home, &project_root, "proj-dispatch", worker_id, task_id);
    let brief = project_root.join("brief.txt");
    write(&brief, "unsupported transport dispatch brief\n");
    let worktree = tmp.path().join("worktree");
    std::fs::create_dir_all(&worktree).unwrap();
    let last = project_root.join("last.txt");
    write(&last, "");
    let stdout = project_root.join("stdout.txt");
    write(&stdout, "");

    let running = boot(home.clone()).await;
    let token = read_token(&home);
    let mut body = dispatch_body(
        "implementer",
        &brief,
        &worktree,
        &last,
        &stdout,
        Some(worker_id),
    );
    if let Some(obj) = body.as_object_mut() {
        obj.insert("mode".into(), serde_json::json!("stdio"));
        obj.insert("harness".into(), serde_json::json!("custom"));
    }
    let resp = post_dispatch(&running, &token, "proj-dispatch", task_id, body).await;
    let reject = [
        project_root.as_path(),
        home.root.as_path(),
        brief.as_path(),
        worktree.as_path(),
    ];
    let status = resp.status();
    let body = resp.text().await.unwrap();
    assert_eq!(status, reqwest::StatusCode::BAD_REQUEST);
    assert_path_free_error(&body, "unsupported mode/harness", &reject);

    let _ = running.shutdown.send(());
    let _ = running.join.await;
}

#[tokio::test]
async fn dispatch_rejects_missing_worktree_with_sanitized_error() {
    let tmp = tempfile::tempdir().unwrap();
    let home = Home::at(tmp.path().join("home"));
    home.ensure().unwrap();
    let project_root = tmp.path().join("proj");
    let worker_id = "implementer-codex-ws";
    let task_id = "TASK-MISSING-WT";
    seed_project(&home, &project_root, "proj-dispatch", worker_id, task_id);
    let brief = tmp.path().join("brief.md");
    let missing_worktree = tmp.path().join("missing-worktree");
    let last = tmp.path().join("last.txt");
    let stdout = tmp.path().join("stdout.log");
    write(&brief, "missing worktree brief\n");

    let running = boot(home.clone()).await;
    let token = read_token(&home);
    let resp = post_dispatch(
        &running,
        &token,
        "proj-dispatch",
        task_id,
        dispatch_body(
            "implementer",
            &brief,
            &missing_worktree,
            &last,
            &stdout,
            Some(worker_id),
        ),
    )
    .await;
    let reject = [
        project_root.as_path(),
        home.root.as_path(),
        brief.as_path(),
        missing_worktree.as_path(),
    ];
    assert_path_free_error(
        &resp.text().await.unwrap(),
        "worktree path does not exist or is not a directory",
        &reject,
    );

    let _ = running.shutdown.send(());
    let _ = running.join.await;
}

#[tokio::test]
async fn dispatch_rejects_file_worktree_with_sanitized_error() {
    let tmp = tempfile::tempdir().unwrap();
    let home = Home::at(tmp.path().join("home"));
    home.ensure().unwrap();
    let project_root = tmp.path().join("proj");
    let worker_id = "implementer-codex-ws";
    let task_id = "TASK-FILE-WT";
    seed_project(&home, &project_root, "proj-dispatch", worker_id, task_id);
    let brief = tmp.path().join("brief.md");
    let file_worktree = tmp.path().join("not-a-dir");
    let last = tmp.path().join("last.txt");
    let stdout = tmp.path().join("stdout.log");
    write(&brief, "file worktree brief\n");
    write(&file_worktree, "not a directory\n");

    let running = boot(home.clone()).await;
    let token = read_token(&home);
    let resp = post_dispatch(
        &running,
        &token,
        "proj-dispatch",
        task_id,
        dispatch_body(
            "implementer",
            &brief,
            &file_worktree,
            &last,
            &stdout,
            Some(worker_id),
        ),
    )
    .await;
    let reject = [
        project_root.as_path(),
        home.root.as_path(),
        brief.as_path(),
        file_worktree.as_path(),
    ];
    assert_path_free_error(
        &resp.text().await.unwrap(),
        "worktree path does not exist or is not a directory",
        &reject,
    );

    let _ = running.shutdown.send(());
    let _ = running.join.await;
}

#[tokio::test]
async fn dispatch_override_off_list_model_passes_through_with_warn() {
    let tmp = tempfile::tempdir().unwrap();
    let home = Home::at(tmp.path().join("home"));
    home.ensure().unwrap();
    let project_root = tmp.path().join("proj");
    let worker_id = "implementer-codex-ws";
    let task_id = "TASK-OVERRIDE";
    seed_project(&home, &project_root, "proj-dispatch", worker_id, task_id);
    let brief = tmp.path().join("brief.md");
    let worktree = tmp.path().join("worktree");
    let last = tmp.path().join("last.txt");
    let stdout = tmp.path().join("stdout.log");
    write(&brief, "override dispatch brief\n");
    std::fs::create_dir_all(&worktree).unwrap();

    let running = boot(home.clone()).await;
    let token = read_token(&home);
    let mut body = dispatch_body(
        "implementer",
        &brief,
        &worktree,
        &last,
        &stdout,
        Some(worker_id),
    );
    body["model_override"] = serde_json::Value::String("gpt-99".into());
    let response = post_dispatch(&running, &token, "proj-dispatch", task_id, body).await;
    assert!(
        response.status().is_success(),
        "off-list override should pass through: {}",
        response.status()
    );

    let raw = wait_for_run_created(&project_root, worker_id).await;
    assert!(raw.contains(":MODEL_OVERRIDE:"));
    assert!(raw.contains("gpt-99"));

    let _ = running.shutdown.send(());
    let _ = running.join.await;
}

fn init_git_worktree(worktree: &Path) {
    let output = Command::new("git")
        .args(["init", "-b", "main"])
        .current_dir(worktree)
        .output()
        .expect("git init");
    assert!(output.status.success(), "git init failed");
    Command::new("git")
        .args(["config", "user.email", "tester@example.com"])
        .current_dir(worktree)
        .status()
        .unwrap();
    Command::new("git")
        .args(["config", "user.name", "Test User"])
        .current_dir(worktree)
        .status()
        .unwrap();
}

async fn wait_for_nonempty_file(path: &Path, timeout: Duration) -> String {
    let deadline = std::time::Instant::now() + timeout;
    while std::time::Instant::now() < deadline {
        if path.exists() {
            let body = std::fs::read_to_string(path).unwrap_or_default();
            if !body.is_empty() {
                return body;
            }
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    std::fs::read_to_string(path).unwrap_or_default()
}

fn append_session_driver_event(session_path: &Path, event: serde_json::Value) {
    let envelopes = read_session_file(session_path).expect("read session");
    let template = envelopes
        .first()
        .expect("session should contain at least one envelope");
    let seq = envelopes.iter().map(|e| e.seq).max().unwrap_or(0) + 1;
    let envelope = serde_json::json!({
        "seq": seq,
        "time": "2026-05-25T06:00:00.000000Z",
        "run_id": template.run_id,
        "runtime_id": template.runtime_id,
        "boot_id": template.boot_id,
        "kind": "driver_event",
        "event": event,
    });
    let mut file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(session_path)
        .expect("open session for append");
    writeln!(file, "{envelope}").expect("append session envelope");
}

async fn wait_for_session_nonempty(session_path: &Path) {
    let deadline = std::time::Instant::now() + Duration::from_secs(15);
    while std::time::Instant::now() < deadline {
        if read_session_file(session_path)
            .map(|envelopes| !envelopes.is_empty())
            .unwrap_or(false)
        {
            return;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    panic!("timed out waiting for session file");
}

/// Wait for the first driver event of a real dispatched run.
///
/// This is the most expensive wait in the file and it had the smallest bound
/// (5s): reaching `ready` means the daemon booted, the dispatch landed, and a
/// freshly written fake agent executable was exec'd for the first time — which
/// on macOS queues behind a system-wide serialized Gatekeeper evaluation
/// (.orgasmic/gotchas.org). Under full-suite load that queue, not the daemon,
/// is what spent the 5s: measured here 2026-07-29, `dispatch_endpoint` beside
/// a looping `orgasmic-daemon --lib` at 64 threads under 12 cpu hogs, this
/// helper produced `timed out waiting for session ready` for
/// `dispatch_system_only_session_without_finalize_orphans_not_scrapes` — the
/// TASK-5FEN5 third candidate, and the same failure TASK-HQ970's gate saw.
///
/// The bound now matches what the rest of this file already spends on lesser
/// waits (`wait_for_session_nonempty` 15s, run-complete 30s), so it is a hang
/// guard rather than a bet on how fast a loaded machine execs a new file.
///
/// The cost itself is gone as of TASK-Z7VQK: no test on this path execs a file
/// this file has not already exec'd. See [`FAKE_AGENT_DISPATCHER`] — one shared
/// executable, pre-warmed at install time, hard-linked into every fixture, with
/// per-test behavior sourced from data beside the link. The 30s is now what it
/// says it is, a hang guard, not a budget Gatekeeper is quietly spending.
async fn wait_for_session_ready(session_path: &Path) {
    let waited = Duration::from_secs(30);
    let deadline = std::time::Instant::now() + waited;
    while std::time::Instant::now() < deadline {
        if let Ok(envelopes) = read_session_file(session_path) {
            if envelopes.iter().any(|envelope| {
                envelope.kind == SessionEventKind::DriverEvent
                    && envelope.event.get("type").and_then(|value| value.as_str()) == Some("ready")
            }) {
                return;
            }
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    panic!(
        "timed out waiting for session ready after {waited:?} in {}",
        session_path.display()
    );
}

async fn release_dispatch_run(
    client: &reqwest::Client,
    addr: std::net::SocketAddr,
    token: &str,
    run_id: &str,
) {
    release_dispatch_run_with_reason(
        client,
        addr,
        token,
        run_id,
        "dispatch endpoint test release",
    )
    .await;
}

async fn release_dispatch_run_with_reason(
    client: &reqwest::Client,
    addr: std::net::SocketAddr,
    token: &str,
    run_id: &str,
    reason: &str,
) {
    let release = client
        .post(format!("http://{addr}/api/runs/{run_id}/release"))
        .bearer_auth(token)
        .json(&serde_json::json!({
            "reason": reason,
            "request_id": "dispatch-endpoint-release"
        }))
        .send()
        .await
        .unwrap();
    assert!(
        release.status().is_success(),
        "release dispatch run: {}",
        release.status()
    );
}

async fn assert_no_finalize_artifacts(last: &Path, stdout: &Path) {
    tokio::time::sleep(Duration::from_millis(300)).await;
    let last_body = std::fs::read_to_string(last).unwrap_or_default();
    assert!(
        last_body.is_empty(),
        "release without worker finalize must not write last_path: {last_body}"
    );
    let stdout_body = std::fs::read_to_string(stdout).unwrap_or_default();
    assert!(
        stdout_body.is_empty(),
        "release without worker finalize must not write stdout_path: {stdout_body}"
    );
}

async fn assert_orphan_without_finalize_artifacts(project_root: &Path, last: &Path, stdout: &Path) {
    wait_for_orphan_tx(project_root).await;
    assert_no_finalize_artifacts(last, stdout).await;
}

async fn wait_for_session_failed_release(session_path: &Path, reason: &str) {
    use orgasmic_core::Lifecycle;
    let deadline = std::time::Instant::now() + Duration::from_secs(15);
    while std::time::Instant::now() < deadline {
        if let Ok(envelopes) = read_session_file(session_path) {
            for envelope in envelopes.iter().rev() {
                if envelope.kind != SessionEventKind::Lifecycle {
                    continue;
                }
                if let Ok(Lifecycle::Release {
                    outcome,
                    reason: release_reason,
                    ..
                }) = serde_json::from_value(envelope.event.clone())
                {
                    if outcome == orgasmic_core::ReleaseOutcome::Failed && release_reason == reason
                    {
                        return;
                    }
                }
            }
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    panic!("timed out waiting for failed release reason={reason}");
}

async fn wait_for_orphan_tx(project_root: &Path) {
    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    while std::time::Instant::now() < deadline {
        if read_project_tx(project_root).contains(":TYPE:         manager.dispatch_orphaned") {
            return;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    panic!("timed out waiting for manager.dispatch_orphaned tx");
}

#[cfg(unix)]
#[tokio::test]
async fn dispatch_protocol_end_without_finalize_orphans_and_leaves_artifacts_empty() {
    let _environment = test_environment_lock().lock().await;
    let tmp = tempfile::tempdir().unwrap();
    let home = Home::at(tmp.path().join("home"));
    home.ensure().unwrap();
    let project_root = tmp.path().join("proj");
    let worker_id = "implementer-cursor-agent-subprocess-stream-json";
    let task_id = "TASK-LAST";
    seed_project(&home, &project_root, "proj-dispatch", worker_id, task_id);
    let brief = tmp.path().join("brief.md");
    let worktree = tmp.path().join("worktree");
    let last = tmp.path().join("last.txt");
    let stdout = tmp.path().join("stdout.log");
    write(&brief, "last path dispatch brief\n");
    std::fs::create_dir_all(&worktree).unwrap();
    init_git_worktree(&worktree);
    write(&worktree.join("dirty.txt"), "uncommitted change\n");

    let script = fake_cursor_protocol_exit_script("orphan-last");
    let bin = install_fake_cursor_agent(tmp.path(), &script);
    let _path_guard = PrependPathGuard::new(&bin);

    let running = boot(home.clone()).await;
    let token = read_token(&home);
    let response = post_dispatch(
        &running,
        &token,
        "proj-dispatch",
        task_id,
        dispatch_body(
            "implementer",
            &brief,
            &worktree,
            &last,
            &stdout,
            Some(worker_id),
        ),
    )
    .await;
    assert!(
        response.status().is_success(),
        "dispatch: {}",
        response.status()
    );
    let body: serde_json::Value = response.json().await.unwrap();
    let session_path = PathBuf::from(body["session_path"].as_str().unwrap());
    wait_for_session_run_complete_summary(&session_path, Duration::from_secs(30)).await;
    wait_for_session_failed_release(&session_path, "protocol_end_without_finalize").await;
    assert_orphan_without_finalize_artifacts(&project_root, &last, &stdout).await;

    let _ = running.shutdown.send(());
    let _ = running.join.await;
}

/// TASK-066: delayed protocol end without worker finalize orphans and leaves
/// artifacts empty.
#[cfg(unix)]
#[tokio::test]
async fn dispatch_delayed_protocol_end_without_finalize_orphans() {
    let _environment = test_environment_lock().lock().await;
    let tmp = tempfile::tempdir().unwrap();
    let home = Home::at(tmp.path().join("home"));
    home.ensure().unwrap();
    let project_root = tmp.path().join("proj");
    let worker_id = "implementer-cursor-agent-subprocess-stream-json";
    let task_id = "TASK-DELAYED-ARTIFACTS";
    seed_project(&home, &project_root, "proj-dispatch", worker_id, task_id);
    let brief = tmp.path().join("brief-delayed.md");
    let worktree = tmp.path().join("worktree-delayed");
    let last = tmp.path().join("last-delayed.txt");
    let stdout = tmp.path().join("stdout-delayed.log");
    write(&brief, "delayed artifact dispatch brief\n");
    std::fs::create_dir_all(&worktree).unwrap();
    init_git_worktree(&worktree);

    let mut script = fake_cursor_protocol_exit_script("orphan-delayed");
    script.insert_str(
        script.find("cat >/dev/null\n").unwrap() + "cat >/dev/null\n".len(),
        "sleep 2\n",
    );
    let bin = install_fake_cursor_agent(tmp.path(), &script);
    let _path_guard = PrependPathGuard::new(&bin);

    let running = boot(home.clone()).await;
    let token = read_token(&home);
    let response = post_dispatch(
        &running,
        &token,
        "proj-dispatch",
        task_id,
        dispatch_body(
            "implementer",
            &brief,
            &worktree,
            &last,
            &stdout,
            Some(worker_id),
        ),
    )
    .await;
    assert!(
        response.status().is_success(),
        "dispatch: {}",
        response.status()
    );
    let body: serde_json::Value = response.json().await.unwrap();
    let session_path = PathBuf::from(body["session_path"].as_str().unwrap());
    wait_for_session_run_complete_summary(&session_path, Duration::from_secs(30)).await;
    wait_for_session_failed_release(&session_path, "protocol_end_without_finalize").await;
    assert_orphan_without_finalize_artifacts(&project_root, &last, &stdout).await;

    let _ = running.shutdown.send(());
    let _ = running.join.await;
}

/// TASK-066.1: cursor-shaped sessions without worker finalize must not scrape
/// assistant chunks into completion artifacts.
#[cfg(unix)]
#[tokio::test]
async fn dispatch_cursor_shaped_session_without_finalize_orphans_not_scrapes() {
    let _environment = test_environment_lock().lock().await;
    let tmp = tempfile::tempdir().unwrap();
    let home = Home::at(tmp.path().join("home"));
    home.ensure().unwrap();
    let project_root = tmp.path().join("proj");
    let worker_id = "implementer-cursor-agent-subprocess-stream-json";
    let task_id = "TASK-CURSOR-SHAPED";
    seed_project(&home, &project_root, "proj-dispatch", worker_id, task_id);
    let brief = tmp.path().join("brief-cursor-shaped.md");
    let worktree = tmp.path().join("worktree-cursor-shaped");
    let last = tmp.path().join("last-cursor-shaped.txt");
    let stdout = tmp.path().join("stdout-cursor-shaped.log");
    write(&brief, "cursor-shaped dispatch brief\n");
    std::fs::create_dir_all(&worktree).unwrap();
    init_git_worktree(&worktree);
    write(&worktree.join("dirty.txt"), "uncommitted change\n");

    let script = fake_cursor_protocol_exit_script("orphan-cursor");
    let bin = install_fake_cursor_agent(tmp.path(), &script);
    let _path_guard = PrependPathGuard::new(&bin);

    let running = boot(home.clone()).await;
    let token = read_token(&home);
    let response = post_dispatch(
        &running,
        &token,
        "proj-dispatch",
        task_id,
        dispatch_body(
            "implementer",
            &brief,
            &worktree,
            &last,
            &stdout,
            Some(worker_id),
        ),
    )
    .await;
    assert!(
        response.status().is_success(),
        "dispatch: {}",
        response.status()
    );
    let body: serde_json::Value = response.json().await.unwrap();
    let session_path = PathBuf::from(body["session_path"].as_str().unwrap());

    wait_for_session_ready(&session_path).await;
    append_session_driver_event(
        &session_path,
        serde_json::json!({
            "type": "text_chunk",
            "stream": "assistant",
            "chunk": "hello world",
            "seq": 0,
        }),
    );
    append_session_driver_event(
        &session_path,
        serde_json::json!({
            "type": "run_complete",
            "summary": "hello world cursor agent summary",
        }),
    );
    wait_for_session_run_complete_summary(&session_path, Duration::from_secs(30)).await;
    wait_for_session_failed_release(&session_path, "protocol_end_without_finalize").await;
    assert_orphan_without_finalize_artifacts(&project_root, &last, &stdout).await;

    let _ = running.shutdown.send(());
    let _ = running.join.await;
}

#[cfg(unix)]
#[tokio::test]
async fn dispatch_clean_worktree_protocol_end_without_finalize_orphans() {
    let _environment = test_environment_lock().lock().await;
    let tmp = tempfile::tempdir().unwrap();
    let home = Home::at(tmp.path().join("home"));
    home.ensure().unwrap();
    let project_root = tmp.path().join("proj");
    let worker_id = "implementer-cursor-agent-subprocess-stream-json";
    let task_id = "TASK-CLEAN";
    seed_project(&home, &project_root, "proj-dispatch", worker_id, task_id);
    let brief = tmp.path().join("brief.md");
    let worktree = tmp.path().join("worktree-clean");
    let last = tmp.path().join("last-clean.txt");
    let stdout = tmp.path().join("stdout-clean.log");
    write(&brief, "clean worktree dispatch brief\n");
    std::fs::create_dir_all(&worktree).unwrap();
    init_git_worktree(&worktree);
    Command::new("git")
        .args(["add", "."])
        .current_dir(&worktree)
        .status()
        .unwrap();
    Command::new("git")
        .args(["commit", "-m", "init"])
        .current_dir(&worktree)
        .status()
        .unwrap();

    let script = fake_cursor_protocol_exit_script("orphan-clean");
    let bin = install_fake_cursor_agent(tmp.path(), &script);
    let _path_guard = PrependPathGuard::new(&bin);

    let running = boot(home.clone()).await;
    let token = read_token(&home);
    let response = post_dispatch(
        &running,
        &token,
        "proj-dispatch",
        task_id,
        dispatch_body(
            "implementer",
            &brief,
            &worktree,
            &last,
            &stdout,
            Some(worker_id),
        ),
    )
    .await;
    assert!(
        response.status().is_success(),
        "dispatch: {}",
        response.status()
    );
    let body: serde_json::Value = response.json().await.unwrap();
    let session_path = PathBuf::from(body["session_path"].as_str().unwrap());
    wait_for_session_run_complete_summary(&session_path, Duration::from_secs(30)).await;
    wait_for_session_failed_release(&session_path, "protocol_end_without_finalize").await;
    assert_orphan_without_finalize_artifacts(&project_root, &last, &stdout).await;
    let raw = read_project_tx(&project_root);
    assert!(
        !raw.contains(":TYPE:         implementer.commit_pending"),
        "clean worktree protocol end without finalize must not emit commit_pending"
    );

    let _ = running.shutdown.send(());
    let _ = running.join.await;
}

/// Run ids the daemon still holds a lease for. The lease, not the session
/// file, is what "released" means to every caller (TASK-37TAF).
async fn live_run_ids(running: &RunningDaemon, token: &str) -> Vec<String> {
    let body: serde_json::Value = reqwest::Client::new()
        .get(format!("http://{}/api/runs/live", running.addr))
        .bearer_auth(token)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    body["live"]
        .as_array()
        .map(|runs| {
            runs.iter()
                .filter_map(|run| run["run_id"].as_str().map(str::to_string))
                .collect()
        })
        .unwrap_or_default()
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

fn count_occurrences(raw: &str, needle: &str) -> usize {
    raw.match_indices(needle).count()
}

fn seed_dual_kind_project(
    home: &Home,
    project_root: &Path,
    project_id: &str,
    _implement_worker: &str,
    _review_worker: &str,
    task_id: &str,
) {
    symlink_repo_source(home);
    write(
        &project_root.join(format!(".orgasmic/tasks/{task_id}/node.org")),
        format!(
            "#+title: orgasmic task {task_id}\n#+orgasmic_version: 2\n\n* BACKLOG {task_id} Dispatch endpoint task\n:PROPERTIES:\n:ID:               {task_id}\n:END:\n"
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

async fn post_dispatch_close_tx(
    running: &RunningDaemon,
    token: &str,
    project_id: &str,
    task_id: &str,
    ty: &str,
    closed_tx: &str,
) {
    let response = reqwest::Client::new()
        .post(format!("http://{}/api/tx", running.addr))
        .bearer_auth(token)
        .json(&serde_json::json!({
            "type": ty,
            "project": project_id,
            "task": task_id,
            "extra": [
                ["CLOSED_TX", closed_tx],
                ["KIND", if ty == "reviewer.done" { "reviewer" } else { "implementer" }],
            ],
        }))
        .send()
        .await
        .unwrap();
    assert!(
        response.status().is_success(),
        "dispatch close tx {ty}: {}",
        response.status()
    );
}

async fn wait_for_dispatch_txs(project_root: &Path, dispatch_started_count: usize) -> String {
    let deadline = std::time::Instant::now() + Duration::from_secs(2);
    while std::time::Instant::now() < deadline {
        let raw = read_project_tx(project_root);
        if count_occurrences(&raw, ":TYPE:         manager.dispatch_started")
            >= dispatch_started_count
            && count_occurrences(&raw, ":TYPE:         run.created") >= dispatch_started_count
        {
            return raw;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    panic!(
        "timed out waiting for {dispatch_started_count} dispatch_started + run.created txs; got {}",
        read_project_tx(project_root)
    );
}

#[tokio::test]
async fn dispatch_back_to_back_implementer_and_reviewer_emit_dispatch_started_and_run_created() {
    let tmp = tempfile::tempdir().unwrap();
    let home = Home::at(tmp.path().join("home"));
    home.ensure().unwrap();
    let project_root = tmp.path().join("proj");
    let worker_id = "implementer-codex-ws";
    let reviewer_worker_id = "reviewer-codex-ws";
    let task_id = "TASK-BACKTOBACK";
    seed_dual_kind_project(
        &home,
        &project_root,
        "proj-dispatch",
        worker_id,
        reviewer_worker_id,
        task_id,
    );

    let impl_brief = tmp.path().join("impl-brief.md");
    let impl_worktree = tmp.path().join("worktree-impl");
    let review_brief = tmp.path().join("review-brief.md");
    let review_worktree = tmp.path().join("worktree-review");
    let impl_last = tmp.path().join("impl-last.txt");
    let review_last = tmp.path().join("review-last.txt");
    let impl_stdout = tmp.path().join("impl-stdout.log");
    let review_stdout = tmp.path().join("review-stdout.log");
    write(&impl_brief, "implementer dispatch brief\n");
    write(&review_brief, "reviewer dispatch brief\n");
    std::fs::create_dir_all(&impl_worktree).unwrap();
    std::fs::create_dir_all(&review_worktree).unwrap();

    let running = boot(home.clone()).await;
    let token = read_token(&home);
    let client = reqwest::Client::new();

    let impl_response = post_dispatch(
        &running,
        &token,
        "proj-dispatch",
        task_id,
        dispatch_body(
            "implementer",
            &impl_brief,
            &impl_worktree,
            &impl_last,
            &impl_stdout,
            Some(worker_id),
        ),
    )
    .await;
    assert!(
        impl_response.status().is_success(),
        "implementer dispatch: {}",
        impl_response.status()
    );
    let impl_body: serde_json::Value = impl_response.json().await.unwrap();
    let impl_run_id = impl_body["run_id"].as_str().unwrap().to_string();
    let impl_dispatch_tx = impl_body["dispatch_tx_id"].as_str().unwrap().to_string();
    assert!(!impl_dispatch_tx.is_empty());

    let raw_after_impl = wait_for_dispatch_txs(&project_root, 1).await;
    assert!(raw_after_impl.contains(":KIND:         implementer"));
    assert!(raw_after_impl.contains(":BRANCH:       task-dispatch-impl"));

    release_dispatch_run(&client, running.addr, &token, &impl_run_id).await;
    post_dispatch_close_tx(
        &running,
        &token,
        "proj-dispatch",
        task_id,
        "implementer.done",
        &impl_dispatch_tx,
    )
    .await;

    let review_response = post_dispatch(
        &running,
        &token,
        "proj-dispatch",
        task_id,
        serde_json::json!({
            "kind": "reviewer",
            "mode": "ws",
            "harness": "codex",
            "brief_path": review_brief,
            "worktree_path": review_worktree,
            "last_path": review_last,
            "stdout_path": review_stdout,
            "worker_id": reviewer_worker_id,
            "branch": "task-backtoback-review",
            "liveness": "deadbeef",
            "reason": "dispatch endpoint test",
        }),
    )
    .await;
    assert!(
        review_response.status().is_success(),
        "reviewer dispatch: {}",
        review_response.status()
    );
    let review_body: serde_json::Value = review_response.json().await.unwrap();
    let review_run_id = review_body["run_id"].as_str().unwrap().to_string();
    let review_dispatch_tx = review_body["dispatch_tx_id"].as_str().unwrap().to_string();
    assert_ne!(review_dispatch_tx, impl_dispatch_tx);

    let raw_after_review = wait_for_dispatch_txs(&project_root, 2).await;
    assert_eq!(
        count_occurrences(&raw_after_review, ":TYPE:         manager.dispatch_started"),
        2
    );
    assert_eq!(
        count_occurrences(&raw_after_review, ":TYPE:         run.created"),
        2
    );
    assert!(raw_after_review.contains(":KIND:         reviewer"));
    assert!(raw_after_review.contains(":BRANCH:       task-backtoback-review"));
    assert!(raw_after_review.contains(":DISPATCH_TX:"));
    assert!(raw_after_review.contains(&impl_dispatch_tx));
    assert!(raw_after_review.contains(&review_dispatch_tx));

    release_dispatch_run(&client, running.addr, &token, &review_run_id).await;
    post_dispatch_close_tx(
        &running,
        &token,
        "proj-dispatch",
        task_id,
        "reviewer.done",
        &review_dispatch_tx,
    )
    .await;

    let raw_after_close = read_project_tx(&project_root);
    assert_eq!(
        count_occurrences(&raw_after_close, ":TYPE:         manager.dispatch_started"),
        2,
        "dispatch_started txs remain in log after close"
    );
    assert_eq!(
        count_occurrences(&raw_after_close, ":TYPE:         implementer.done"),
        1
    );
    assert_eq!(
        count_occurrences(&raw_after_close, ":TYPE:         reviewer.done"),
        1
    );

    let _ = running.shutdown.send(());
    let _ = running.join.await;
}

#[tokio::test]
async fn dispatch_missing_skill_precedes_missing_brief() {
    let tmp = tempfile::tempdir().unwrap();
    let home = Home::at(tmp.path().join("home"));
    home.ensure().unwrap();
    // Governance-linked skills are spawn authority (dec_WDR5K); unresolved
    // skills must still precede brief IO so path-bearing brief errors stay secondary.
    write(
        &home.config(),
        "dispatch:\n  implementer:\n    linked_skills:\n      - missing-skill\n",
    );
    let project_root = tmp.path().join("proj");
    let worker_id = "implementer-codex-ws";
    let task_id = "TASK-SKILL-BEFORE-BRIEF";
    seed_project(&home, &project_root, "proj-dispatch", worker_id, task_id);
    let missing_brief = tmp.path().join("missing-brief.md");
    let worktree = tmp.path().join("worktree");
    std::fs::create_dir_all(&worktree).unwrap();
    let last = tmp.path().join("last.txt");
    let stdout = tmp.path().join("stdout.log");
    write(&last, "");
    write(&stdout, "");

    let running = boot(home.clone()).await;
    let token = read_token(&home);
    let resp = post_dispatch(
        &running,
        &token,
        "proj-dispatch",
        task_id,
        dispatch_body(
            "implementer",
            &missing_brief,
            &worktree,
            &last,
            &stdout,
            Some(worker_id),
        ),
    )
    .await;
    let status = resp.status();
    let body = resp.text().await.unwrap();
    assert_eq!(status, reqwest::StatusCode::BAD_REQUEST);
    assert!(
        body.contains("unresolved slot skills.missing-skill"),
        "skill error should win over missing brief, got: {body}"
    );
    assert!(
        !body.contains("dispatch brief not found"),
        "missing brief must not take precedence over missing skill, got: {body}"
    );

    let _ = running.shutdown.send(());
    let _ = running.join.await;
}

/// TASK-072 item 1: system-only session chunks without worker finalize must
/// not appear in dispatch stdout artifacts.
#[cfg(unix)]
#[tokio::test]
async fn dispatch_system_only_session_without_finalize_orphans_not_scrapes() {
    let _environment = test_environment_lock().lock().await;
    let tmp = tempfile::tempdir().unwrap();
    let home = Home::at(tmp.path().join("home"));
    home.ensure().unwrap();
    let project_root = tmp.path().join("proj");
    let worker_id = "implementer-cursor-agent-subprocess-stream-json";
    let task_id = "TASK-SYSTEM-ONLY-STDOUT";
    seed_project(&home, &project_root, "proj-dispatch", worker_id, task_id);
    let brief = tmp.path().join("brief-system-only.md");
    let worktree = tmp.path().join("worktree-system-only");
    let last = tmp.path().join("last-system-only.txt");
    let stdout = tmp.path().join("stdout-system-only.log");
    write(&brief, "system-only dispatch brief\n");
    std::fs::create_dir_all(&worktree).unwrap();
    init_git_worktree(&worktree);

    let script = fake_cursor_protocol_exit_script("orphan-system");
    let bin = install_fake_cursor_agent(tmp.path(), &script);
    let _path_guard = PrependPathGuard::new(&bin);

    let running = boot(home.clone()).await;
    let token = read_token(&home);
    let response = post_dispatch(
        &running,
        &token,
        "proj-dispatch",
        task_id,
        dispatch_body(
            "implementer",
            &brief,
            &worktree,
            &last,
            &stdout,
            Some(worker_id),
        ),
    )
    .await;
    assert!(
        response.status().is_success(),
        "dispatch: {}",
        response.status()
    );
    let body: serde_json::Value = response.json().await.unwrap();
    let session_path = PathBuf::from(body["session_path"].as_str().unwrap());

    wait_for_session_ready(&session_path).await;
    for (idx, chunk) in ["Implement", "ed TASK-072", " dispatch hardening."]
        .into_iter()
        .enumerate()
    {
        append_session_driver_event(
            &session_path,
            serde_json::json!({
                "type": "text_chunk",
                "stream": "system",
                "chunk": chunk,
                "seq": 1000 + idx,
            }),
        );
    }
    wait_for_session_run_complete_summary(&session_path, Duration::from_secs(30)).await;
    wait_for_session_failed_release(&session_path, "protocol_end_without_finalize").await;
    assert_orphan_without_finalize_artifacts(&project_root, &last, &stdout).await;

    let _ = running.shutdown.send(());
    let _ = running.join.await;
}

/// TASK-072.1 F-1: watcher grace path when supervisor drops a run without a
/// dispatch-terminal session marker (subprocess exit → Interrupted release).
#[tokio::test]
#[ignore = "spawns a real codex run (≤60s + API spend); run with --ignored"]
async fn dispatch_grace_path_writes_artifacts_without_terminal_session_marker() {
    let available = codex_available();
    if skip_test_if_missing(
        "dispatch_grace_path_writes_artifacts_without_terminal_session_marker",
        &[("codex", available)],
    ) {
        assert_required_test_tooling(&[ToolRequirement::new("codex", 1, available)]);
        return;
    }

    let tmp = tempfile::tempdir().unwrap();
    let home = Home::at(tmp.path().join("home"));
    home.ensure().unwrap();
    let project_root = tmp.path().join("proj");
    let worker_id = "implementer-codex-stdio";
    let task_id = "TASK-GRACE-PATH";
    seed_project(&home, &project_root, "proj-dispatch", worker_id, task_id);
    let brief = tmp.path().join("brief-grace-path.md");
    let worktree = tmp.path().join("worktree-grace-path");
    let last = tmp.path().join("last-grace-path.txt");
    let stdout = tmp.path().join("stdout-grace-path.log");
    write(&brief, "grace-path dispatch brief\n");
    std::fs::create_dir_all(&worktree).unwrap();
    init_git_worktree(&worktree);

    let running = boot(home.clone()).await;
    let token = read_token(&home);
    let response = post_dispatch(
        &running,
        &token,
        "proj-dispatch",
        task_id,
        dispatch_body(
            "implementer",
            &brief,
            &worktree,
            &last,
            &stdout,
            Some(worker_id),
        ),
    )
    .await;
    assert!(
        response.status().is_success(),
        "dispatch: {}",
        response.status()
    );
    let body: serde_json::Value = response.json().await.unwrap();
    let pid = body["pid"].as_u64().unwrap();
    assert!(pid > 0, "grace-path test requires a real subprocess pid");
    let session_path = PathBuf::from(body["session_path"].as_str().unwrap());

    wait_for_session_nonempty(&session_path).await;
    for (idx, chunk) in ["grace-path system chunk", " from watcher integration test"]
        .into_iter()
        .enumerate()
    {
        append_session_driver_event(
            &session_path,
            serde_json::json!({
                "type": "text_chunk",
                "stream": "system",
                "chunk": chunk,
                "seq": 2000 + idx,
            }),
        );
    }
    terminate_pid(pid);

    let last_body = wait_for_nonempty_file(&last, Duration::from_secs(60)).await;
    assert!(
        last_body.contains("grace-path system chunk"),
        "last_path should carry system text after post-exit grace: {last_body}"
    );
    assert!(
        !last_body.contains("dispatch endpoint test release"),
        "grace path must not depend on release-driven terminal marker: {last_body}"
    );

    let stdout_body = wait_for_nonempty_file(&stdout, Duration::from_secs(60)).await;
    assert!(
        stdout_body.contains("[system] grace-path system chunk"),
        "stdout_path should render system-only session chunks: {stdout_body}"
    );

    let _ = running.shutdown.send(());
    let _ = running.join.await;
}

/// TASK-072 item 3: dispatch-started tx entries with empty reason must not
/// introduce trailing whitespace on the :REASON: line.
#[tokio::test]
async fn dispatch_started_tx_empty_reason_has_no_trailing_whitespace() {
    let tmp = tempfile::tempdir().unwrap();
    let home = Home::at(tmp.path().join("home"));
    home.ensure().unwrap();
    let project_root = tmp.path().join("proj");
    let worker_id = "implementer-codex-ws";
    let task_id = "TASK-EMPTY-REASON";
    seed_project(&home, &project_root, "proj-dispatch", worker_id, task_id);
    let brief = tmp.path().join("brief-empty-reason.md");
    let worktree = tmp.path().join("worktree-empty-reason");
    let last = tmp.path().join("last-empty-reason.txt");
    let stdout = tmp.path().join("stdout-empty-reason.log");
    write(&brief, "empty reason dispatch brief\n");
    std::fs::create_dir_all(&worktree).unwrap();

    let running = boot(home.clone()).await;
    let token = read_token(&home);
    let mut body = dispatch_body(
        "implementer",
        &brief,
        &worktree,
        &last,
        &stdout,
        Some(worker_id),
    );
    if let Some(obj) = body.as_object_mut() {
        obj.insert("reason".into(), serde_json::Value::String(String::new()));
    }
    let response = post_dispatch(&running, &token, "proj-dispatch", task_id, body).await;
    assert!(
        response.status().is_success(),
        "dispatch: {}",
        response.status()
    );

    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    let mut reason_line = None;
    while std::time::Instant::now() < deadline {
        let raw = read_project_tx(&project_root);
        if let Some(line) = raw.lines().find(|line| line.starts_with(":REASON:")) {
            reason_line = Some(line.to_string());
            break;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    let reason_line = reason_line.expect("dispatch tx should include a REASON line");
    assert!(
        !reason_line.ends_with(' ') && !reason_line.ends_with('\t'),
        "empty REASON must not pad with trailing whitespace: {reason_line:?}"
    );

    let _ = running.shutdown.send(());
    let _ = running.join.await;
}

// orgasmic:TASK-5HBST
// The fake-cursor tests used to serialize their `PATH` mutation on a lock
// private to this binary. One process has one environment, so a private lock
// is one more thing that has to be remembered rather than a guarantee: it
// excluded these nine tests from each other and nothing else. They now hold
// `test_environment_lock`, the workspace-wide environment lock (TASK-R2HDN),
// which is also what the drivers-side probes and the daemon's own lib tests
// take — so a future environment mutation added anywhere in this binary
// excludes them for free. Being a `tokio` mutex, it is also held across the
// awaits below without `clippy::await_holding_lock` suppressions.

#[cfg(unix)]
fn fake_cursor_protocol_exit_script(session_id: &str) -> String {
    let mut script = format!(
        "#!/bin/sh\nexec 0<&-\ncat >/dev/null\nprintf '%s\\n' '{{\"type\":\"system\",\"subtype\":\"init\",\"model\":\"composer-2.5-fast\",\"session_id\":\"{session_id}\"}}'\n"
    );
    for idx in 1..=15 {
        script.push_str(&format!(
            "printf '%s\\n' '{{\"type\":\"synthetic-chunk-{idx:02}\"}}'\n"
        ));
    }
    script
}

// orgasmic:TASK-Z7VQK
/// The one executable every fake harness in this file is exec'd through.
///
/// macOS evaluates a newly created executable through Gatekeeper on its first
/// exec, and that evaluation is serialized system-wide (`.orgasmic/gotchas.org`,
/// durable invariant as of TASK-STWVB). Every test here used to write its own
/// fake agent, so every test paid a fresh evaluation *on a timed path*. Measured
/// on this machine 2026-07-29: a fresh script's first exec 177ms, a hard link to
/// an already-evaluated one 5ms, a byte-identical copy 157ms — the cache key is
/// the inode, not the path. Idle those numbers are nothing; queued behind a
/// loaded suite they became seconds and spent whole wait budgets, which is what
/// `timed out waiting for session ready` and `timed out waiting for synthetic
/// run_complete` were reporting (TASK-5FEN5, TASK-HQ970, TASK-Z7VQK).
///
/// So there is one file. It is exec'd once at first install, where no deadline
/// is running; every fixture is a hard link to it, which reuses that inode's
/// evaluation; and per-test behavior lives in a `.behaviour` data file beside
/// the link, which is *sourced* rather than exec'd and so is never evaluated at
/// all. Raising a bound would have kept the cost and only moved where it queues.
const FAKE_AGENT_DISPATCHER: &str = r#"#!/bin/sh
# Hard-linked as cursor-agent, worker-server and codex. `$0` is the link the
# caller exec'd, so the behaviour beside it is this test's and no other's.
if [ "$1" = "--orgasmic-fixture-warm" ]; then
  printf 'warm\n'
  exit 0
fi
# Parameter expansion, not `dirname`: one less fork on the path whose whole
# point is that starting this process costs nothing.
FAKE_AGENT_DIR=${0%/*}
export FAKE_AGENT_DIR
. "$0.behaviour"
"#;

/// A path beside this test binary, keyed by the fixture's own contents.
///
/// Not by the binary's name: cargo reuses that name across rebuilds (measured
/// 2026-07-29 — `dispatch_endpoint-1edf7bef84349123` survived an edit to this
/// file), so a name-keyed fixture is a *stale* fixture the moment its text
/// changes. Content-keyed, an edited fixture simply gets a new path, and an
/// older test binary still running keeps the one it warmed.
fn beside_test_binary(suffix: &str, contents: &str) -> PathBuf {
    use std::hash::{Hash as _, Hasher as _};
    let test_binary = std::env::current_exe().expect("resolve test binary");
    let file_name = test_binary
        .file_name()
        .expect("test binary has a file name")
        .to_string_lossy()
        .to_string();
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    contents.hash(&mut hasher);
    let key = hasher.finish();
    test_binary.with_file_name(format!("{file_name}.{key:016x}.{suffix}"))
}

/// Publish an executable at `path` exactly once, tolerating a second copy of
/// this test binary racing for the same path.
fn publish_shared_executable(path: &Path, fill: impl FnOnce(&Path)) {
    if path.exists() {
        return;
    }
    let parent = path.parent().expect("test binary has a parent");
    let pending = tempfile::NamedTempFile::new_in(parent).expect("stage shared fixture");
    fill(pending.path());
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(pending.path(), std::fs::Permissions::from_mode(0o755))
            .expect("make shared fixture executable");
    }
    match pending.persist_noclobber(path) {
        Ok(_) => {}
        Err(error) if error.error.kind() == std::io::ErrorKind::AlreadyExists => {}
        Err(error) => panic!("publish shared fixture: {}", error.error),
    }
}

/// Pay the first-exec cost of the shared dispatcher here, once, where nothing
/// is being timed — and assert it answered, so a fixture that cannot run at all
/// fails as itself rather than as some later mystified timeout (TASK-GEZHQ).
fn shared_fake_agent() -> &'static Path {
    static SHARED: std::sync::OnceLock<PathBuf> = std::sync::OnceLock::new();
    SHARED
        .get_or_init(|| {
            let path = beside_test_binary("fake-agent.sh", FAKE_AGENT_DISPATCHER);
            publish_shared_executable(&path, |pending| {
                std::fs::write(pending, FAKE_AGENT_DISPATCHER).expect("write shared fixture");
            });
            assert_eq!(
                std::fs::read_to_string(&path).expect("read shared fixture"),
                FAKE_AGENT_DISPATCHER,
                "the shared fake agent must match the contents its path is \
                 keyed by — a mismatch means every test in this run would drive \
                 a harness nobody wrote"
            );
            let output = Command::new(&path)
                .arg("--orgasmic-fixture-warm")
                .output()
                .expect("the shared fake agent must be executable");
            let stdout = String::from_utf8_lossy(&output.stdout);
            assert!(
                stdout.contains("warm"),
                "the shared fake agent must answer before any test waits on it: {stdout:?}"
            );
            // The receipt is the answer itself. `fake_harnesses_share_one_pre_warmed_executable`
            // reads it, so "was this exec'd before the tests ran" is a fact on
            // disk rather than a property of code nobody checks.
            std::fs::write(warm_receipt(&path), stdout.as_bytes())
                .expect("record the shared fake agent's warm receipt");
            path
        })
        .as_path()
}

fn warm_receipt(shared: &Path) -> PathBuf {
    PathBuf::from(format!("{}.warmed", shared.display()))
}

/// Put a pre-warmed executable at a production-shaped name without creating a
/// new file, and therefore without a new Gatekeeper evaluation.
fn link_shared_executable(source: &Path, path: &Path) {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).expect("create fixture link parent");
    }
    let _ = std::fs::remove_file(path);
    std::fs::hard_link(source, path)
        .unwrap_or_else(|error| panic!("link shared fixture at {}: {error}", path.display()));
}

/// Install one fake harness: the shared executable under the name the daemon
/// will look up, and this test's script beside it as sourced data.
fn install_fake_agent(path: &Path, script: &str) {
    write(
        &PathBuf::from(format!("{}.behaviour", path.display())),
        script,
    );
    link_shared_executable(shared_fake_agent(), path);
}

#[cfg(unix)]
fn install_fake_cursor_agent(tmp: &Path, script: &str) -> PathBuf {
    let bin = tmp.join("bin");
    std::fs::create_dir_all(&bin).unwrap();
    install_fake_agent(&bin.join("worker-server"), "#!/bin/sh\nsleep \"$@\"\n");
    install_fake_agent(&bin.join("cursor-agent"), script);
    bin
}

/// The identity half of what the rest of this file's timing rests on.
///
/// Checked by inode because that is the key macOS's evaluation cache uses: a
/// byte-identical *copy* pays the full first-exec cost (157ms measured
/// 2026-07-29) while a hard link to the warmed file pays 5ms. Asserted here, in
/// its own test, because every other test in this file would report a violation
/// as a wait expiring somewhere far away — the symptom that got read as "slow
/// machine" for three tasks running (TASK-HQ970, TASK-5FEN5, TASK-Z7VQK).
#[cfg(unix)]
#[test]
fn every_fake_harness_is_the_same_executable() {
    use std::os::unix::fs::MetadataExt as _;
    let first = tempfile::tempdir().unwrap();
    let second = tempfile::tempdir().unwrap();
    let cursor_bin = install_fake_cursor_agent(first.path(), "#!/bin/sh\nexit 0\n");
    let codex_bin = install_fake_codex(second.path(), "#!/bin/sh\nexit 0\n");

    let shared_inode = std::fs::metadata(shared_fake_agent()).unwrap().ino();
    for installed in [
        cursor_bin.join("cursor-agent"),
        cursor_bin.join("worker-server"),
        codex_bin.join("codex"),
    ] {
        assert_eq!(
            std::fs::metadata(&installed).unwrap().ino(),
            shared_inode,
            "{} must be a link to the pre-warmed executable rather than a new \
             one: a new inode is a new Gatekeeper evaluation, and that \
             evaluation is serialized system-wide",
            installed.display()
        );
    }
}

/// The other half: one shared executable is worth nothing if the first exec of
/// it still lands inside somebody's deadline.
///
/// The receipt is written by the warm-up itself, so this asserts the exec
/// happened rather than that the code which would have done it is present.
#[cfg(unix)]
#[test]
fn the_shared_fake_agent_is_exec_d_before_any_test_waits_on_it() {
    let tmp = tempfile::tempdir().unwrap();
    install_fake_cursor_agent(tmp.path(), "#!/bin/sh\nexit 0\n");

    let receipt = warm_receipt(shared_fake_agent());
    assert!(
        receipt.exists(),
        "the shared fake agent must be exec'd at install time, before any \
         test's clock starts; no warm receipt beside it"
    );
    assert!(
        std::fs::read_to_string(&receipt).unwrap().contains("warm"),
        "the warm receipt must hold what the fixture actually answered"
    );
}

/// The `codex`-named half of the same treatment, for the finalize, restart and
/// cleanup tests that hand the daemon a `PATH` in the dispatch body.
fn install_fake_codex(tmp: &Path, script: &str) -> PathBuf {
    let bin = tmp.join("bin");
    std::fs::create_dir_all(&bin).unwrap();
    install_fake_agent(&bin.join("codex"), script);
    bin
}

/// True when a C compiler is available to build the hermetic fake harness;
/// tests that need it skip-with-warning instead of failing the suite on
/// boxes without developer tools.
#[cfg(unix)]
fn cc_available_for_test() -> bool {
    Command::new("cc")
        .arg("--version")
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .map(|status| status.success())
        .unwrap_or(false)
}

/// The hermetic fake harness, compiled once for the whole file.
///
/// Its two per-test knobs are read from the environment rather than argv, and
/// that is load-bearing: `dispatch_response_pid_prefers_worker_server_child`
/// asserts the *generic* sibling can never satisfy the "worker-server"
/// substring, and a fixture argv carrying `--session-id worker-server-pid`
/// would appear in every child's `ps` line and defeat it.
const FAKE_CURSOR_AGENT_SOURCE: &str = r#"#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <unistd.h>

int main(int argc, char **argv) {
    char session_id[128];
    const char *requested = getenv("ORGASMIC_FAKE_SESSION_ID");
    const char *generic = getenv("ORGASMIC_FAKE_GENERIC_SIBLING");
    if (argc > 1 && strcmp(argv[1], "--orgasmic-fixture-warm") == 0) {
        return 0;
    }
    strncpy(session_id, requested ? requested : "fixture", sizeof(session_id) - 1);
    session_id[sizeof(session_id) - 1] = 0;
    if (generic && strcmp(generic, "1") == 0) {
        pid_t generic_pid = fork();
        if (generic_pid == 0) {
            memset(argv[0], 0, strlen(argv[0]));
            strncpy(argv[0], "generic-sleep", 13);
            sleep(300);
            return 0;
        }
    }
    pid_t worker_pid = fork();
    if (worker_pid == 0) {
        memset(argv[0], 0, strlen(argv[0]));
        strncpy(argv[0], "worker-server", 13);
        sleep(300);
        return 0;
    }
    memset(argv[0], 0, strlen(argv[0]));
    strncpy(argv[0], "worker-server", 13);
    printf("{\"type\":\"system\",\"subtype\":\"init\",\"model\":\"composer-2.5-fast\",\"session_id\":\"%s\"}\n",
           session_id);
    fflush(stdout);
    sleep(300);
    return 0;
}
"#;

/// Compile the hermetic harness once, pre-warm it once, and hand every test the
/// same inode — a fresh `cc` output is as new to Gatekeeper as a fresh script.
#[cfg(unix)]
fn shared_fake_cursor_binary() -> &'static Path {
    static SHARED: std::sync::OnceLock<PathBuf> = std::sync::OnceLock::new();
    SHARED
        .get_or_init(|| {
            let path = beside_test_binary("fake-cursor-agent", FAKE_CURSOR_AGENT_SOURCE);
            publish_shared_executable(&path, |pending| {
                let source = beside_test_binary("fake-cursor-agent.c", FAKE_CURSOR_AGENT_SOURCE);
                write(&source, FAKE_CURSOR_AGENT_SOURCE);
                let output = Command::new("cc")
                    .arg(&source)
                    .arg("-o")
                    .arg(pending)
                    .output()
                    .expect("compile fake cursor-agent binary");
                assert!(
                    output.status.success(),
                    "compile fake cursor-agent binary failed: {}{}",
                    String::from_utf8_lossy(&output.stderr),
                    String::from_utf8_lossy(&output.stdout)
                );
            });
            let status = Command::new(&path)
                .arg("--orgasmic-fixture-warm")
                .status()
                .expect("pre-warm fake cursor-agent binary");
            assert!(
                status.success(),
                "the hermetic fake harness must run before any test waits on it"
            );
            path
        })
        .as_path()
}

#[cfg(unix)]
fn install_fake_cursor_agent_binary(
    tmp: &Path,
    spawn_generic_sibling: bool,
    session_id: &str,
) -> PathBuf {
    let bin = install_fake_cursor_agent(tmp, "#!/bin/sh\nexit 127\n");
    link_shared_executable(
        shared_fake_cursor_binary(),
        &bin.join("cursor-agent-worker-server"),
    );
    let generic = if spawn_generic_sibling { "1" } else { "0" };
    install_fake_agent(
        &bin.join("cursor-agent"),
        &format!(
            "#!/bin/sh\nORGASMIC_FAKE_SESSION_ID={session_id} \
             ORGASMIC_FAKE_GENERIC_SIBLING={generic} \
             exec \"$FAKE_AGENT_DIR/cursor-agent-worker-server\" \"$@\"\n"
        ),
    );
    bin
}

#[cfg(unix)]
fn prepend_path(dir: &Path) {
    let current = std::env::var("PATH").unwrap_or_default();
    std::env::set_var("PATH", format!("{}:{current}", dir.display()));
}

#[cfg(unix)]
struct PrependPathGuard {
    previous: Option<std::ffi::OsString>,
}

#[cfg(unix)]
impl PrependPathGuard {
    fn new(dir: &Path) -> Self {
        let previous = std::env::var_os("PATH");
        prepend_path(dir);
        Self { previous }
    }
}

#[cfg(unix)]
impl Drop for PrependPathGuard {
    fn drop(&mut self) {
        match &self.previous {
            Some(path) => std::env::set_var("PATH", path),
            None => std::env::remove_var("PATH"),
        }
    }
}

async fn wait_for_session_run_complete_summary(session_path: &Path, timeout: Duration) -> String {
    let deadline = std::time::Instant::now() + timeout;
    while std::time::Instant::now() < deadline {
        if let Ok(envelopes) = read_session_file(session_path) {
            for envelope in &envelopes {
                if envelope.kind == SessionEventKind::DriverEvent
                    && envelope.event.get("type").and_then(|value| value.as_str())
                        == Some("run_complete")
                {
                    return envelope
                        .event
                        .get("summary")
                        .and_then(|value| value.as_str())
                        .unwrap_or("")
                        .to_string();
                }
            }
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    // What the session did contain decides which failure this is: no driver
    // event at all means the fake harness never spoke, a `ready` with nothing
    // after it means the daemon did not synthesize. Without this the two are
    // indistinguishable and get diagnosed as "slow machine".
    let seen: Vec<String> = read_session_file(session_path)
        .map(|envelopes| {
            envelopes
                .iter()
                .map(|envelope| {
                    format!(
                        "{:?}:{}",
                        envelope.kind,
                        envelope
                            .event
                            .get("type")
                            .and_then(|value| value.as_str())
                            .unwrap_or("-")
                    )
                })
                .collect()
        })
        .unwrap_or_default();
    panic!(
        "timed out waiting for synthetic run_complete in {} after {timeout:?}; session held {seen:?}",
        session_path.display()
    );
}

/// TASK-074 item 3: invalid task ids must 404 immediately without loading bodies.
#[tokio::test]
async fn get_project_invalid_task_returns_404_quickly_without_paths() {
    let tmp = tempfile::tempdir().unwrap();
    let home = Home::at(tmp.path().join("home"));
    home.ensure().unwrap();
    let project_root = tmp.path().join("proj");
    let worker_id = "implementer-codex-ws";
    seed_project(
        &home,
        &project_root,
        "proj-dispatch",
        worker_id,
        "TASK-VALID",
    );
    write(
        &project_root.join(".orgasmic/tasks/TASK-VALID/node.org"),
        format!(
            "#+title: orgasmic task TASK-VALID\n#+orgasmic_version: 2\n\n* BACKLOG TASK-VALID Valid task\n:PROPERTIES:\n:ID:               TASK-VALID\n:END:\n\n** Description\n{}\n",
            "x".repeat(64 * 1024)
        ),
    );

    let running = boot(home.clone()).await;
    let token = read_token(&home);
    let client = reqwest::Client::new();
    let started = std::time::Instant::now();
    let resp = client
        .get(format!(
            "http://{}/api/projects/proj-dispatch/tasks/TASK-INVALID",
            running.addr
        ))
        .bearer_auth(&token)
        .timeout(Duration::from_secs(5))
        .send()
        .await
        .unwrap();
    assert!(
        started.elapsed() < Duration::from_secs(3),
        "invalid task lookup took too long: {:?}",
        started.elapsed()
    );
    let home_root = home.source();
    let reject = [project_root.as_path(), home_root.as_path()];
    let error =
        assert_path_free_error_response(resp, reqwest::StatusCode::NOT_FOUND, "not found", &reject)
            .await;
    assert!(
        error.contains("task not found"),
        "unexpected error: {error}"
    );

    let _ = running.shutdown.send(());
    let _ = running.join.await;
}

/// TASK-075: dispatch response pid should track the inner worker child, not the
/// short-lived cursor-agent CLI wrapper.
#[cfg(unix)]
#[tokio::test]
async fn dispatch_response_pid_is_inner_subprocess_child() {
    let _environment = test_environment_lock().lock().await;
    if skip_test_if_missing(
        "dispatch_response_pid_is_inner_subprocess_child",
        &[("cc", cc_available_for_test())],
    ) {
        return;
    }
    let tmp = tempfile::tempdir().unwrap();
    let bin = install_fake_cursor_agent_binary(tmp.path(), false, "watch-pid");
    let agent = bin.join("cursor-agent");
    let _path_guard = PrependPathGuard::new(&bin);
    let home = Home::at(tmp.path().join("home"));
    home.ensure().unwrap();
    let project_root = tmp.path().join("proj");
    let worker_id = "implementer-cursor-agent-subprocess-stream-json";
    let task_id = "TASK-WATCH-PID";
    seed_project(&home, &project_root, "proj-dispatch", worker_id, task_id);
    let brief = tmp.path().join("brief-watch-pid.md");
    let worktree = tmp.path().join("worktree-watch-pid");
    let last = tmp.path().join("last-watch-pid.txt");
    let stdout = tmp.path().join("stdout-watch-pid.log");
    write(&brief, "watch pid dispatch brief\n");
    std::fs::create_dir_all(&worktree).unwrap();

    let running = boot(home.clone()).await;
    let token = read_token(&home);
    let response = post_dispatch(
        &running,
        &token,
        "proj-dispatch",
        task_id,
        dispatch_body(
            "implementer",
            &brief,
            &worktree,
            &last,
            &stdout,
            Some(worker_id),
        ),
    )
    .await;
    assert!(
        response.status().is_success(),
        "dispatch: {}",
        response.status()
    );
    let body: serde_json::Value = response.json().await.unwrap();
    let pid = body["pid"].as_u64().unwrap();
    assert!(pid > 0, "dispatch should return a watch pid");
    let ps = Command::new("ps")
        .args(["-p", pid.to_string().as_str(), "-o", "command="])
        .output()
        .expect("ps watch pid");
    let command = String::from_utf8_lossy(&ps.stdout);
    // Loose on purpose: macOS ps may report the wrapper path for script-backed
    // execs. The strict wrapper-vs-child guarantee is owned by the supervisor
    // unit test poll_direct_child_pid_prefers_worker_server_over_generic_sibling.
    assert!(
        command.contains(&agent.display().to_string()) || command.contains("worker-server"),
        "dispatch pid should resolve to the hermetic fake harness, got command={command:?}"
    );
    terminate_pid(pid);
    let _ = Command::new("pkill")
        .arg("-f")
        .arg(agent.display().to_string())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status();

    let _ = running.shutdown.send(());
    let _ = running.join.await;
}

/// TASK-075.1 F-2: dispatch watch pid must prefer worker-server over a generic sibling.
#[cfg(unix)]
#[tokio::test]
async fn dispatch_response_pid_prefers_worker_server_child() {
    let _environment = test_environment_lock().lock().await;
    if skip_test_if_missing(
        "dispatch_response_pid_prefers_worker_server_child",
        &[("cc", cc_available_for_test())],
    ) {
        return;
    }
    let tmp = tempfile::tempdir().unwrap();
    let bin = install_fake_cursor_agent_binary(tmp.path(), true, "worker-server-pid");
    let _path_guard = PrependPathGuard::new(&bin);
    let home = Home::at(tmp.path().join("home"));
    home.ensure().unwrap();
    let project_root = tmp.path().join("proj");
    let worker_id = "implementer-cursor-agent-subprocess-stream-json";
    let task_id = "TASK-WORKER-SERVER-PID";
    seed_project(&home, &project_root, "proj-dispatch", worker_id, task_id);
    let brief = tmp.path().join("brief-worker-server-pid.md");
    let worktree = tmp.path().join("worktree-worker-server-pid");
    let last = tmp.path().join("last-worker-server-pid.txt");
    let stdout = tmp.path().join("stdout-worker-server-pid.log");
    write(&brief, "worker-server pid dispatch brief\n");
    std::fs::create_dir_all(&worktree).unwrap();

    let running = boot(home.clone()).await;
    let token = read_token(&home);
    let response = post_dispatch(
        &running,
        &token,
        "proj-dispatch",
        task_id,
        dispatch_body(
            "implementer",
            &brief,
            &worktree,
            &last,
            &stdout,
            Some(worker_id),
        ),
    )
    .await;
    assert!(
        response.status().is_success(),
        "dispatch: {}",
        response.status()
    );
    let body: serde_json::Value = response.json().await.unwrap();
    let pid = body["pid"].as_u64().unwrap();
    assert!(pid > 0, "dispatch should return a watch pid");
    let ps = Command::new("ps")
        .args(["-p", pid.to_string().as_str(), "-o", "command="])
        .output()
        .expect("ps watch pid");
    let command = String::from_utf8_lossy(&ps.stdout);
    // Loose on purpose: the generic sibling renames itself to "generic-sleep"
    // so it can never satisfy either accepted substring. The strict
    // wrapper-vs-child preference is owned by the supervisor unit test
    // poll_direct_child_pid_prefers_worker_server_over_generic_sibling.
    assert!(
        command.contains("worker-server") || command.contains("cursor-agent"),
        "dispatch pid should be the hermetic fake harness, not generic sibling: {command:?}"
    );
    terminate_pid(pid);

    let _ = running.shutdown.send(());
    let _ = running.join.await;
}

/// TASK-074 item 2: acquire + ready then subprocess exit auto-releases the lease.
#[cfg(unix)]
#[tokio::test]
async fn dispatch_early_exit_auto_releases_stuck_lease() {
    let _environment = test_environment_lock().lock().await;
    let tmp = tempfile::tempdir().unwrap();
    let bin = install_fake_cursor_agent(
        tmp.path(),
        "#!/bin/sh\nexec 0<&-\nexec 2>/dev/null\ncat >/dev/null\nprintf '%s\\n' '{\"type\":\"system\",\"subtype\":\"init\",\"model\":\"composer-2.5-fast\",\"session_id\":\"early-exit\"}'\n",
    );
    let _path_guard = PrependPathGuard::new(&bin);
    let home = Home::at(tmp.path().join("home"));
    home.ensure().unwrap();
    let project_root = tmp.path().join("proj");
    let worker_id = "implementer-cursor-agent-subprocess-stream-json";
    let task_id = "TASK-EARLY-EXIT";
    seed_project(&home, &project_root, "proj-dispatch", worker_id, task_id);
    let brief = tmp.path().join("brief-early-exit.md");
    let worktree = tmp.path().join("worktree-early-exit");
    let last = tmp.path().join("last-early-exit.txt");
    let stdout = tmp.path().join("stdout-early-exit.log");
    write(&brief, "early-exit dispatch brief\n");
    std::fs::create_dir_all(&worktree).unwrap();
    init_git_worktree(&worktree);

    let sessions_dir = project_root.join(".orgasmic/tmp/sessions");
    std::fs::create_dir_all(&sessions_dir).unwrap();
    let session_count_before = count_session_jsonl(&sessions_dir);

    let running = boot(home.clone()).await;
    let token = read_token(&home);
    let first = post_dispatch(
        &running,
        &token,
        "proj-dispatch",
        task_id,
        dispatch_body(
            "implementer",
            &brief,
            &worktree,
            &last,
            &stdout,
            Some(worker_id),
        ),
    )
    .await;
    assert!(
        first.status().is_success(),
        "first dispatch: {}",
        first.status()
    );
    let first_body: serde_json::Value = first.json().await.unwrap();
    let session_path = PathBuf::from(first_body["session_path"].as_str().unwrap());
    assert_eq!(
        count_session_jsonl(&sessions_dir),
        session_count_before + 1,
        "dispatch should create exactly one session file"
    );

    let orphan_deadline = std::time::Instant::now() + Duration::from_secs(30);
    let mut orphaned = false;
    while std::time::Instant::now() < orphan_deadline {
        let raw = read_project_tx(&project_root);
        if raw.contains(":TYPE:         manager.dispatch_orphaned") {
            orphaned = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert!(
        orphaned,
        "early-exit without work envelopes must flag manager.dispatch_orphaned"
    );

    let release_deadline = std::time::Instant::now() + Duration::from_secs(30);
    let mut release_count = 0usize;
    while std::time::Instant::now() < release_deadline {
        if let Ok(envelopes) = read_session_file(&session_path) {
            release_count = envelopes
                .iter()
                .filter(|envelope| {
                    envelope.kind == SessionEventKind::Lifecycle
                        && envelope.event.get("phase").and_then(|phase| phase.as_str())
                            == Some("release")
                })
                .count();
            if release_count == 1 {
                break;
            }
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert_eq!(release_count, 1, "early-exit must release exactly once");
    tokio::time::sleep(Duration::from_millis(500)).await;
    let session_raw = std::fs::read_to_string(&session_path).unwrap_or_default();
    assert!(
        session_raw.contains("early-exit subprocess with no work envelopes"),
        "early-exit tombstone must use the literal watcher release reason: {session_raw}"
    );
    assert!(
        session_raw.contains("\"outcome\":\"failed\"")
            || session_raw.contains("\"outcome\": \"failed\""),
        "early-exit tombstone must record Failed outcome: {session_raw}"
    );
    let orphan_tx_count = read_project_tx(&project_root)
        .matches(":TYPE:         manager.dispatch_orphaned")
        .count();
    assert_eq!(
        orphan_tx_count, 1,
        "early-exit orphan must emit exactly one manager.dispatch_orphaned tx"
    );
    tokio::time::sleep(Duration::from_millis(500)).await;
    let session_raw_late = std::fs::read_to_string(&session_path).unwrap_or_default();
    assert!(
        session_raw_late.contains("early-exit subprocess with no work envelopes"),
        "late re-read must preserve literal early-exit reason: {session_raw_late}"
    );
    assert!(
        session_raw_late.matches("\"phase\":\"release\"").count()
            + session_raw_late.matches("\"phase\": \"release\"").count()
            == 1,
        "late re-read must still have exactly one release: {session_raw_late}"
    );
    assert!(
        !last.exists()
            || std::fs::read_to_string(&last)
                .unwrap_or_default()
                .is_empty(),
        "early-exit orphan must not synthesize last.txt"
    );
    assert!(
        !stdout.exists()
            || std::fs::read_to_string(&stdout)
                .unwrap_or_default()
                .is_empty(),
        "early-exit orphan must not synthesize stdout.log"
    );
    let runs = get_runs_json(&running, &token).await;
    let live = runs["live"].as_array().cloned().unwrap_or_default();
    assert!(
        live.is_empty(),
        "early-exit orphan must leave zero live runs: {live:?}"
    );
    assert_eq!(
        count_session_jsonl(&sessions_dir),
        session_count_before + 1,
        "early-exit orphan must not create a replacement session"
    );
    assert!(
        !session_raw.contains("\"phase\":\"continuation\"")
            && !session_raw.contains("\"phase\": \"continuation\""),
        "early-exit orphan must not write Lifecycle::Continuation"
    );

    let deadline = std::time::Instant::now() + Duration::from_secs(30);
    while std::time::Instant::now() < deadline {
        let second = post_dispatch(
            &running,
            &token,
            "proj-dispatch",
            task_id,
            dispatch_body(
                "implementer",
                &brief,
                &worktree,
                &last,
                &stdout,
                Some(worker_id),
            ),
        )
        .await;
        if second.status().is_success() {
            let _ = running.shutdown.send(());
            let _ = running.join.await;
            return;
        }
        assert_eq!(
            second.status(),
            reqwest::StatusCode::CONFLICT,
            "unexpected early-exit follow-up dispatch status"
        );
        tokio::time::sleep(Duration::from_millis(250)).await;
    }
    panic!("timed out waiting for early-exit lease auto-release");
}

fn count_session_jsonl(dir: &Path) -> usize {
    std::fs::read_dir(dir)
        .map(|entries| {
            entries
                .flatten()
                .filter(|entry| {
                    entry.path().extension().and_then(|ext| ext.to_str()) == Some("jsonl")
                })
                .count()
        })
        .unwrap_or(0)
}

/// TASK-074 item 1 + TASK-P4MGK: subprocess-stream-json still synthesizes
/// `run_complete` from the system tail on exit, but that protocol-end is no
/// longer a dispatch success signal — without `orgasmic dispatch finalize`
/// the completion watcher orphans instead of scraping last.txt.
#[cfg(unix)]
#[tokio::test]
async fn dispatch_subprocess_exit_synthesizes_run_complete_from_system_tail() {
    // orgasmic:TASK-P4MGK
    let _environment = test_environment_lock().lock().await;
    let tmp = tempfile::tempdir().unwrap();
    let mut script = String::from(
        "#!/bin/sh\nexec 0<&-\ncat >/dev/null\nprintf '%s\\n' '{\"type\":\"system\",\"subtype\":\"init\",\"model\":\"composer-2.5-fast\",\"session_id\":\"synthetic-exit\"}'\n",
    );
    for idx in 1..=15 {
        script.push_str(&format!(
            "printf '%s\\n' '{{\"type\":\"synthetic-chunk-{idx:02}\"}}'\n"
        ));
    }
    let bin = install_fake_cursor_agent(tmp.path(), &script);
    let _path_guard = PrependPathGuard::new(&bin);
    let home = Home::at(tmp.path().join("home"));
    home.ensure().unwrap();
    let project_root = tmp.path().join("proj");
    let worker_id = "implementer-cursor-agent-subprocess-stream-json";
    let task_id = "TASK-SYNTHETIC-RC";
    seed_project(&home, &project_root, "proj-dispatch", worker_id, task_id);
    let brief = tmp.path().join("brief-synthetic-rc.md");
    let worktree = tmp.path().join("worktree-synthetic-rc");
    let last = tmp.path().join("last-synthetic-rc.txt");
    let stdout = tmp.path().join("stdout-synthetic-rc.log");
    write(&brief, "synthetic run_complete dispatch brief\n");
    std::fs::create_dir_all(&worktree).unwrap();
    init_git_worktree(&worktree);

    let running = boot(home.clone()).await;
    let token = read_token(&home);
    let sessions_dir = project_root.join(".orgasmic/tmp/sessions");
    let session_count_before = count_session_jsonl(&sessions_dir);
    let response = post_dispatch(
        &running,
        &token,
        "proj-dispatch",
        task_id,
        dispatch_body(
            "implementer",
            &brief,
            &worktree,
            &last,
            &stdout,
            Some(worker_id),
        ),
    )
    .await;
    assert!(
        response.status().is_success(),
        "dispatch: {}",
        response.status()
    );
    let body: serde_json::Value = response.json().await.unwrap();
    let session_path = PathBuf::from(body["session_path"].as_str().unwrap());
    let summary =
        wait_for_session_run_complete_summary(&session_path, Duration::from_secs(30)).await;
    assert!(
        !summary.is_empty(),
        "synthetic run_complete summary must be non-empty"
    );
    assert!(
        summary.contains("synthetic-chunk-15"),
        "summary should include trailing system chunks: {summary}"
    );
    assert!(
        !summary.contains("synthetic-chunk-01"),
        "summary must not concatenate the full system stream: {summary}"
    );

    // Protocol-end without finalize → orphan, never a scraped last.txt.
    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    let mut orphaned = false;
    while std::time::Instant::now() < deadline {
        let raw = read_project_tx(&project_root);
        if raw.contains(":TYPE:         manager.dispatch_orphaned") {
            orphaned = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert!(
        orphaned,
        "protocol-end without finalize must flag manager.dispatch_orphaned"
    );
    assert!(
        !last.exists()
            || std::fs::read_to_string(&last)
                .unwrap_or_default()
                .is_empty(),
        "protocol-end without finalize must not scrape a fake last.txt"
    );
    // orgasmic:TASK-QPKCD — orphan path must not auto-spawn a continuation run.
    let runs = get_runs_json(&running, &token).await;
    let live = runs["live"].as_array().cloned().unwrap_or_default();
    assert!(
        live.is_empty(),
        "protocol-end without finalize must leave zero live runs (no auto-continuation): {live:?}"
    );
    let session_raw = std::fs::read_to_string(&session_path).unwrap_or_default();
    assert!(
        !session_raw.contains("\"phase\":\"continuation\"")
            && !session_raw.contains("\"phase\": \"continuation\""),
        "failed dispatch must not write Lifecycle::Continuation"
    );
    assert_eq!(
        count_session_jsonl(&sessions_dir),
        session_count_before + 1,
        "protocol-end orphan must not create a replacement session"
    );

    let _ = running.shutdown.send(());
    let _ = running.join.await;
}

async fn post_dispatch_cleanup(
    running: &RunningDaemon,
    token: &str,
    project_id: &str,
    task_id: &str,
    body: serde_json::Value,
) -> reqwest::Response {
    reqwest::Client::new()
        .post(format!(
            "http://{}/api/projects/{}/tasks/{}/dispatch/cleanup",
            running.addr, project_id, task_id
        ))
        .bearer_auth(token)
        .json(&body)
        .send()
        .await
        .unwrap()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn dispatch_cleanup_releases_worker_and_deletes_worktree_branch() {
    let tmp = tempfile::tempdir().unwrap();
    let home = Home::at(tmp.path().join("home"));
    home.ensure().unwrap();
    let project_root = tmp.path().join("project");
    std::fs::create_dir_all(&project_root).unwrap();
    let project_id = "proj-dispatch";
    let task_id = "TASK-CLEANUP-EP";
    seed_project(
        &home,
        &project_root,
        project_id,
        "implementer-codex-ws",
        task_id,
    );
    write(
        &project_root.join(".orgasmic/project.org"),
        "#+title: orgasmic\n#+orgasmic_version: 1\n\n* PROJECT orgasmic\n:PROPERTIES:\n:ID:                     orgasmic\n:END:\n",
    );
    Command::new("git")
        .args(["init", "-b", "main"])
        .current_dir(&project_root)
        .status()
        .unwrap();
    Command::new("git")
        .args(["config", "user.email", "tester@example.com"])
        .current_dir(&project_root)
        .status()
        .unwrap();
    Command::new("git")
        .args(["config", "user.name", "Test User"])
        .current_dir(&project_root)
        .status()
        .unwrap();
    write(&project_root.join("README.md"), "init\n");
    Command::new("git")
        .args(["add", "."])
        .current_dir(&project_root)
        .status()
        .unwrap();
    Command::new("git")
        .args(["commit", "-m", "init"])
        .current_dir(&project_root)
        .status()
        .unwrap();
    let head = String::from_utf8_lossy(
        &Command::new("git")
            .args(["rev-parse", "HEAD"])
            .current_dir(&project_root)
            .output()
            .unwrap()
            .stdout,
    )
    .trim()
    .to_string();

    // Installed but never handed to the dispatch: unlike its siblings below,
    // this test's dispatch body carries no `PATH`, so the harness it drives is
    // whatever the machine has. Left as found — making it hermetic changes what
    // the test dispatches, which is not this task's (TASK-Z7VQK) to decide.
    let _unreached_codex_stub = install_fake_codex(
        tmp.path(),
        "#!/bin/sh\nlast=\"\"\nwhile [ \"$#\" -gt 0 ]; do\n  if [ \"$1\" = \"--output-last-message\" ]; then\n    shift\n    last=\"$1\"\n  fi\n  shift\ndone\nif [ -n \"$last\" ]; then\n  printf 'stub-done\\n' > \"$last\"\nfi\nsleep 120\n",
    );

    let stem_dir = project_root.join(".orgasmic/tmp/dispatch/task-cleanup-ep");
    std::fs::create_dir_all(&stem_dir).unwrap();
    let worktree = stem_dir.join("worktree");
    Command::new("git")
        .args([
            "worktree",
            "add",
            "-B",
            "task-cleanup-ep-impl",
            worktree.to_str().unwrap(),
            &head,
        ])
        .current_dir(&project_root)
        .status()
        .unwrap();
    let brief = stem_dir.join("task-cleanup-ep-brief.md");
    write(&brief, "cleanup endpoint brief");
    let attempt = "aaaa1111bbbb2222cccc3333dddd4444";
    let last = stem_dir.join(format!("task-cleanup-ep-{attempt}-last.txt"));
    let stdout = stem_dir.join(format!("task-cleanup-ep-{attempt}-stdout.log"));
    std::fs::write(&last, "").unwrap();
    std::fs::write(&stdout, "").unwrap();

    let running = boot(home.clone()).await;
    let token = read_token(&home);
    let mut dispatch = dispatch_body(
        "implementer",
        &brief,
        &worktree,
        &last,
        &stdout,
        Some("implementer-codex-ws"),
    );
    dispatch["dispatch_attempt_token"] = serde_json::json!(attempt);
    dispatch["branch"] = serde_json::json!("task-cleanup-ep-impl");
    let response = post_dispatch(&running, &token, project_id, task_id, dispatch).await;
    assert_eq!(
        response.status(),
        200,
        "dispatch failed: {}",
        response.text().await.unwrap_or_default()
    );

    let cleanup = post_dispatch_cleanup(
        &running,
        &token,
        project_id,
        task_id,
        serde_json::json!({
            "kind": "implementer",
            "worktree_path": worktree,
            "branch": "task-cleanup-ep-impl",
            "dispatch_attempt_token": attempt,
            "last_path": last,
            "stdout_path": stdout,
        }),
    )
    .await;
    assert_eq!(cleanup.status(), 200, "cleanup endpoint failed");
    let body: serde_json::Value = cleanup.json().await.unwrap();
    assert_eq!(body["status"], "ok");
    assert!(body["worktree_removed"].as_bool().unwrap_or(false));
    assert!(body["branch_deleted"].as_bool().unwrap_or(false));
    assert!(
        !worktree.exists(),
        "worktree should be removed by daemon cleanup"
    );
    let branch_exists = Command::new("git")
        .args([
            "show-ref",
            "--verify",
            "--quiet",
            "refs/heads/task-cleanup-ep-impl",
        ])
        .current_dir(&project_root)
        .status()
        .map(|status| status.success())
        .unwrap_or(false);
    assert!(!branch_exists, "branch should be deleted by daemon cleanup");

    let lease = reqwest::Client::new()
        .post(format!(
            "http://{}/api/projects/{}/tasks/{}/lease/release",
            running.addr, project_id, task_id
        ))
        .bearer_auth(&token)
        .json(&serde_json::json!({ "kind": "implementer" }))
        .send()
        .await
        .unwrap();
    let lease_body: serde_json::Value = lease.json().await.unwrap();
    assert_eq!(lease_body["status"], "no_lease");

    let _ = running.shutdown.send(());
    let _ = running.join.await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn dispatch_cleanup_branch_mismatch_returns_conflict() {
    let tmp = tempfile::tempdir().unwrap();
    let home = Home::at(tmp.path().join("home"));
    home.ensure().unwrap();
    let project_root = tmp.path().join("project");
    std::fs::create_dir_all(&project_root).unwrap();
    let project_id = "proj-dispatch";
    let task_id = "TASK-CLEANUP-BRANCH";
    seed_project(
        &home,
        &project_root,
        project_id,
        "implementer-codex-ws",
        task_id,
    );
    write(
        &project_root.join(".orgasmic/project.org"),
        "#+title: orgasmic\n#+orgasmic_version: 1\n\n* PROJECT orgasmic\n:PROPERTIES:\n:ID:                     orgasmic\n:END:\n",
    );
    Command::new("git")
        .args(["init", "-b", "main"])
        .current_dir(&project_root)
        .status()
        .unwrap();
    Command::new("git")
        .args(["config", "user.email", "tester@example.com"])
        .current_dir(&project_root)
        .status()
        .unwrap();
    Command::new("git")
        .args(["config", "user.name", "Test User"])
        .current_dir(&project_root)
        .status()
        .unwrap();
    write(&project_root.join("README.md"), "init\n");
    Command::new("git")
        .args(["add", "."])
        .current_dir(&project_root)
        .status()
        .unwrap();
    Command::new("git")
        .args(["commit", "-m", "init"])
        .current_dir(&project_root)
        .status()
        .unwrap();
    let head = String::from_utf8_lossy(
        &Command::new("git")
            .args(["rev-parse", "HEAD"])
            .current_dir(&project_root)
            .output()
            .unwrap()
            .stdout,
    )
    .trim()
    .to_string();

    let stem_dir = project_root.join(".orgasmic/tmp/dispatch/task-cleanup-branch");
    std::fs::create_dir_all(&stem_dir).unwrap();
    let worktree = stem_dir.join("worktree");
    let branch = "task-cleanup-branch-impl";
    Command::new("git")
        .args([
            "worktree",
            "add",
            "-B",
            branch,
            worktree.to_str().unwrap(),
            &head,
        ])
        .current_dir(&project_root)
        .status()
        .unwrap();
    let brief = stem_dir.join("task-cleanup-branch-brief.md");
    write(&brief, "branch mismatch cleanup brief");
    let attempt = "aaaa1111bbbb2222cccc3333dddd4444";
    let last = stem_dir.join(format!("task-cleanup-branch-{attempt}-last.txt"));
    let stdout = stem_dir.join(format!("task-cleanup-branch-{attempt}-stdout.log"));
    std::fs::write(&last, "").unwrap();
    std::fs::write(&stdout, "").unwrap();

    let running = boot(home.clone()).await;
    let token = read_token(&home);
    let mut dispatch = dispatch_body(
        "implementer",
        &brief,
        &worktree,
        &last,
        &stdout,
        Some("implementer-codex-ws"),
    );
    dispatch["dispatch_attempt_token"] = serde_json::json!(attempt);
    dispatch["branch"] = serde_json::json!(branch);
    let response = post_dispatch(&running, &token, project_id, task_id, dispatch).await;
    assert_eq!(
        response.status(),
        200,
        "dispatch failed: {}",
        response.text().await.unwrap_or_default()
    );

    let cleanup = post_dispatch_cleanup(
        &running,
        &token,
        project_id,
        task_id,
        serde_json::json!({
            "kind": "implementer",
            "worktree_path": worktree,
            "branch": "wrong-branch-impl",
            "dispatch_attempt_token": attempt,
            "last_path": last,
            "stdout_path": stdout,
        }),
    )
    .await;
    assert_eq!(cleanup.status(), 200, "cleanup endpoint transport failed");
    let body: serde_json::Value = cleanup.json().await.unwrap();
    assert_eq!(body["status"], "conflict");
    assert!(!body["worktree_removed"].as_bool().unwrap_or(true));
    assert!(!body["branch_deleted"].as_bool().unwrap_or(true));
    assert!(
        worktree.exists(),
        "worktree must survive branch mismatch cleanup"
    );
    let branch_ref = format!("refs/heads/{branch}");
    let branch_exists = Command::new("git")
        .args(["show-ref", "--verify", "--quiet", branch_ref.as_str()])
        .current_dir(&project_root)
        .status()
        .map(|status| status.success())
        .unwrap_or(false);
    assert!(
        branch_exists,
        "dispatch branch must survive mismatch cleanup"
    );

    let _ = running.shutdown.send(());
    let _ = running.join.await;
}

/// TASK-NW4WV: worker finalize through the dispatch endpoint must preserve
/// worker-authored last/stdout artifacts and must not flag an orphan.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn dispatch_endpoint_worker_finalize_preserves_authoritative_artifacts() {
    let tmp = tempfile::tempdir().unwrap();
    let home = Home::at(tmp.path().join("home"));
    home.ensure().unwrap();
    let project_root = tmp.path().join("project");
    std::fs::create_dir_all(&project_root).unwrap();
    let project_id = "proj-dispatch";
    let task_id = "TASK-FINALIZE-EP";
    seed_project(
        &home,
        &project_root,
        project_id,
        "implementer-codex-ws",
        task_id,
    );
    write(
        &project_root.join(".orgasmic/project.org"),
        "#+title: orgasmic\n#+orgasmic_version: 1\n\n* PROJECT orgasmic\n:PROPERTIES:\n:ID:                     orgasmic\n:END:\n",
    );
    Command::new("git")
        .args(["init", "-b", "main"])
        .current_dir(&project_root)
        .status()
        .unwrap();
    Command::new("git")
        .args(["config", "user.email", "tester@example.com"])
        .current_dir(&project_root)
        .status()
        .unwrap();
    Command::new("git")
        .args(["config", "user.name", "Test User"])
        .current_dir(&project_root)
        .status()
        .unwrap();
    write(&project_root.join("README.md"), "init\n");
    Command::new("git")
        .args(["add", "."])
        .current_dir(&project_root)
        .status()
        .unwrap();
    Command::new("git")
        .args(["commit", "-m", "init"])
        .current_dir(&project_root)
        .status()
        .unwrap();
    let head = String::from_utf8_lossy(
        &Command::new("git")
            .args(["rev-parse", "HEAD"])
            .current_dir(&project_root)
            .output()
            .unwrap()
            .stdout,
    )
    .trim()
    .to_string();

    let bin_dir = install_fake_codex(
        tmp.path(),
        "#!/bin/sh\nlast=\"\"\nwhile [ \"$#\" -gt 0 ]; do\n  if [ \"$1\" = \"--output-last-message\" ]; then\n    shift\n    last=\"$1\"\n  fi\n  shift\ndone\nif [ -n \"$last\" ]; then\n  printf 'stub-done\\n' > \"$last\"\nfi\nsleep 120\n",
    );

    let stem_dir = project_root.join(".orgasmic/tmp/dispatch/task-finalize-ep");
    std::fs::create_dir_all(&stem_dir).unwrap();
    let worktree = stem_dir.join("worktree");
    let branch = "task-finalize-ep-impl";
    Command::new("git")
        .args([
            "worktree",
            "add",
            "-B",
            branch,
            worktree.to_str().unwrap(),
            &head,
        ])
        .current_dir(&project_root)
        .status()
        .unwrap();
    let brief = stem_dir.join("task-finalize-ep-brief.md");
    write(&brief, "finalize endpoint brief");
    let attempt = "aaaa1111bbbb2222cccc3333dddd4444";
    let last = stem_dir.join(format!("task-finalize-ep-{attempt}-last.txt"));
    let stdout = stem_dir.join(format!("task-finalize-ep-{attempt}-stdout.log"));
    std::fs::write(&last, "").unwrap();
    std::fs::write(&stdout, "").unwrap();

    let running = boot(home.clone()).await;
    let token = read_token(&home);
    let mut dispatch = dispatch_body(
        "implementer",
        &brief,
        &worktree,
        &last,
        &stdout,
        Some("implementer-codex-ws"),
    );
    dispatch["dispatch_attempt_token"] = serde_json::json!(attempt);
    dispatch["branch"] = serde_json::json!(branch);
    dispatch["driver_config"] = serde_json::json!({
        "PATH": format!("{}:{}", bin_dir.display(), std::env::var("PATH").unwrap_or_default())
    });
    let response = post_dispatch(&running, &token, project_id, task_id, dispatch).await;
    assert_eq!(
        response.status(),
        200,
        "dispatch failed: {}",
        response.text().await.unwrap_or_default()
    );
    let body: serde_json::Value = response.json().await.unwrap();
    let run_id = body["run_id"]
        .as_str()
        .expect("dispatch response run_id")
        .to_string();

    let worker_last = "## Report\nendpoint finalize authoritative.";
    let worker_stdout = "endpoint stdout authoritative.";
    std::fs::write(&last, worker_last).unwrap();
    std::fs::write(&stdout, worker_stdout).unwrap();

    let release = reqwest::Client::new()
        .post(format!("http://{}/api/runs/{run_id}/release", running.addr))
        .bearer_auth(&token)
        // orgasmic:TASK-37TAF — carries its terminal tx, as every `dispatch
        // finalize` has since TASK-WGXKD. A `finalized_by_worker` release
        // WITHOUT one is now refused outright (old-CLI skew), so sending the
        // old payload here would test the refusal, not the artifact contract
        // this test is about.
        .json(&serde_json::json!({
            "reason": format!("worker finalize for {task_id}"),
            "finalized_by_worker": true,
            "request_id": "dispatch-endpoint-finalize-test",
            "terminal_tx": {
                "request_id": format!("dispatch-finalize-{task_id}"),
                "type": "implementer.reported",
                "actor": "agent.implementer",
                "project": project_id,
                "task": task_id,
                "extra": [["RUN_ID", run_id]],
            },
        }))
        .send()
        .await
        .unwrap();
    assert!(
        release.status().is_success(),
        "worker finalize release failed: {}",
        release.status()
    );

    tokio::time::sleep(Duration::from_millis(500)).await;
    assert_eq!(std::fs::read_to_string(&last).unwrap(), worker_last);
    assert_eq!(std::fs::read_to_string(&stdout).unwrap(), worker_stdout);
    assert!(
        !read_project_tx(&project_root).contains(":TYPE:         manager.dispatch_orphaned"),
        "worker finalize must not flag manager.dispatch_orphaned"
    );

    let _ = running.shutdown.send(());
    let _ = running.join.await;
}

/// TASK-WGXKD: the terminal tx a worker finalize hands to its release must be
/// written even when the caller is gone before the response could reach it.
///
/// That is the normal case, not an edge case: the release tears down the driver,
/// which reaps the harness's whole setsid process group — and the `orgasmic
/// dispatch finalize` process is in that group. Here the caller writes the
/// request and closes the socket without ever reading a byte back, which drops
/// the request future on the daemon side. The tx must still be on record, or
/// the run lands in the state that has no name: work committed, report written,
/// lease released, nothing reported, no orphan flag.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn dispatch_endpoint_worker_finalize_tx_survives_caller_disconnect() {
    let tmp = tempfile::tempdir().unwrap();
    let home = Home::at(tmp.path().join("home"));
    home.ensure().unwrap();
    let project_root = tmp.path().join("project");
    std::fs::create_dir_all(&project_root).unwrap();
    let project_id = "proj-dispatch";
    let task_id = "TASK-FINALIZE-TX";
    seed_project(
        &home,
        &project_root,
        project_id,
        "implementer-codex-ws",
        task_id,
    );
    write(
        &project_root.join(".orgasmic/project.org"),
        "#+title: orgasmic\n#+orgasmic_version: 1\n\n* PROJECT orgasmic\n:PROPERTIES:\n:ID:                     orgasmic\n:END:\n",
    );
    let bin_dir = install_fake_codex(tmp.path(), "#!/bin/sh\nsleep 120\n");
    let stem_dir = project_root.join(".orgasmic/tmp/dispatch/task-finalize-tx");
    std::fs::create_dir_all(&stem_dir).unwrap();
    let worktree = stem_dir.join("worktree");
    std::fs::create_dir_all(&worktree).unwrap();
    let brief = stem_dir.join("brief.md");
    write(&brief, "finalize tx brief");
    let last = stem_dir.join("last.txt");
    let stdout = stem_dir.join("stdout.log");
    write(&last, "worker report");
    write(&stdout, "");

    // `release_terminal_tx_delay` widens the daemon's own release-to-tx gap so
    // the caller can be made to vanish inside it. In production that gap is two
    // consecutive awaits; the invariant under test is that whoever is holding
    // it, it is not the caller.
    let running = Daemon::run(
        home.clone(),
        DaemonOptions {
            release_terminal_tx_delay: Some(Duration::from_secs(3)),
            ..test_options()
        },
    )
    .await
    .expect("boot daemon");
    let token = read_token(&home);
    let mut dispatch = dispatch_body(
        "implementer",
        &brief,
        &worktree,
        &last,
        &stdout,
        Some("implementer-codex-ws"),
    );
    dispatch["driver_config"] = serde_json::json!({
        "PATH": format!("{}:{}", bin_dir.display(), std::env::var("PATH").unwrap_or_default())
    });
    let response = post_dispatch(&running, &token, project_id, task_id, dispatch).await;
    assert_eq!(
        response.status(),
        200,
        "dispatch failed: {}",
        response.text().await.unwrap_or_default()
    );
    let body: serde_json::Value = response.json().await.unwrap();
    let run_id = body["run_id"]
        .as_str()
        .expect("dispatch response run_id")
        .to_string();

    // The finalize call, as `cmd_dispatch_finalize` sends it — then the caller
    // vanishes without reading the response, exactly as the group reap leaves it.
    let payload = serde_json::json!({
        "reason": format!("worker finalize for {task_id}"),
        "request_id": format!("dispatch-release-finalize-tx-{run_id}"),
        "finalized_by_worker": true,
        "terminal_tx": {
            "request_id": format!("dispatch-finalize-{task_id}-{run_id}"),
            "type": "implementer.reported",
            "actor": "agent.implementer",
            "project": project_id,
            "task": task_id,
            "extra": [["RUN_ID", run_id]],
        },
    })
    .to_string();
    {
        use std::io::Write as _;
        let mut socket = std::net::TcpStream::connect(running.addr).unwrap();
        write!(
            socket,
            "POST /api/runs/{run_id}/release HTTP/1.1\r\n\
             Host: {}\r\n\
             Authorization: Bearer {token}\r\n\
             Content-Type: application/json\r\n\
             Content-Length: {}\r\n\
             Connection: close\r\n\r\n{payload}",
            running.addr,
            payload.len()
        )
        .unwrap();
        socket.flush().unwrap();
        // Land inside the release-to-tx gap, then vanish without ever reading
        // the response. This is the real shape of the loss: the request is in
        // flight, the caller is not.
        tokio::time::sleep(Duration::from_millis(500)).await;
        assert!(
            !read_project_tx(&project_root).contains(":TYPE:         implementer.reported"),
            "the tx landed before the caller disconnected — this test would \
             prove nothing; the release is finishing too fast to interrupt"
        );
    }

    let deadline = std::time::Instant::now() + Duration::from_secs(20);
    let reported = loop {
        let raw = read_project_tx(&project_root);
        if raw.contains(":TYPE:         implementer.reported") {
            break raw;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "released run never got its terminal tx — [unreported] again:\n{raw}"
        );
        tokio::time::sleep(Duration::from_millis(100)).await;
    };
    let reported_block = reported
        .split("\n* TX ")
        .find(|block| block.contains(":TYPE:         implementer.reported"))
        .expect("implementer.reported block");
    assert!(
        reported_block
            .lines()
            .any(|line| line.starts_with(":RUN_ID:") && line.contains(&run_id)),
        "the terminal tx must carry this run's RUN_ID so dispatch matching \
         attaches it (TASK-6AYEJ.1):\n{reported_block}"
    );
    assert!(
        !reported.contains(":TYPE:         manager.dispatch_orphaned"),
        "a reported worker finalize must not also flag an orphan"
    );

    let _ = running.shutdown.send(());
    let _ = running.join.await;
}

/// TASK-37TAF: the OTHER version-skew direction — an old `dispatch finalize`
/// (pre-TASK-WGXKD) against a current daemon — must also be fail-closed.
///
/// TASK-WGXKD.1 closed new-CLI/old-daemon with a capability handshake. Nothing
/// closed old-CLI/new-daemon: `terminal_tx` is optional on the payload, so a
/// `finalized_by_worker: true` release carrying none released the run anyway
/// and answered 200 with `terminal_tx_id: null`, expecting the caller to post
/// its own `/tx` afterwards. On stdio there is no afterwards — the release
/// tears down the driver, the driver reaps the harness's setsid process group
/// with a direct `kill(-pgid, …)`, and the finalize CLI is in that group. The
/// run ends released, `Completed`, with no terminal tx AND no orphan flag,
/// because the dispatch completion watcher reads `finalized_by_worker` as
/// "the worker reported this itself" and skips its fallback entirely.
///
/// So the daemon refuses instead, and the lease stays HELD — the same trade
/// `require_daemon_writes_terminal_tx` takes on the CLI side: visible-and-wrong
/// beats invisible-and-wrong. The second half of this test pins the refusal
/// narrow: the same release WITH a `terminal_tx` still succeeds.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn dispatch_endpoint_old_cli_release_without_terminal_tx_is_refused() {
    let tmp = tempfile::tempdir().unwrap();
    let home = Home::at(tmp.path().join("home"));
    home.ensure().unwrap();
    let project_root = tmp.path().join("project");
    std::fs::create_dir_all(&project_root).unwrap();
    let project_id = "proj-dispatch";
    let task_id = "TASK-SKEW-TX";
    // stdio specifically: this is the transport where the loss is total
    // (3/3 in TASK-WGXKD's measurements) because the group reap is a direct
    // kill rather than a round trip through an mux server.
    let worker_id = "implementer-codex-stdio";
    seed_project(&home, &project_root, project_id, worker_id, task_id);
    write(
        &project_root.join(".orgasmic/project.org"),
        "#+title: orgasmic\n#+orgasmic_version: 1\n\n* PROJECT orgasmic\n:PROPERTIES:\n:ID:                     orgasmic\n:END:\n",
    );
    // The harness never has to answer: the app-server handshake blocks on its stdout,
    // which is exactly the live-run state a finalize is issued from.
    let bin_dir = install_fake_codex(tmp.path(), "#!/bin/sh\nsleep 120\n");
    let stem_dir = project_root.join(".orgasmic/tmp/dispatch/task-skew-tx");
    std::fs::create_dir_all(&stem_dir).unwrap();
    let worktree = stem_dir.join("worktree");
    std::fs::create_dir_all(&worktree).unwrap();
    let brief = stem_dir.join("brief.md");
    write(&brief, "old-cli skew brief");
    let last = stem_dir.join("last.txt");
    let stdout = stem_dir.join("stdout.log");
    write(&last, "worker report");
    write(&stdout, "");

    let running = boot(home.clone()).await;
    let token = read_token(&home);
    let mut dispatch = dispatch_body(
        "implementer",
        &brief,
        &worktree,
        &last,
        &stdout,
        Some(worker_id),
    );
    dispatch["driver_config"] = serde_json::json!({
        "PATH": format!("{}:{}", bin_dir.display(), std::env::var("PATH").unwrap_or_default())
    });
    let response = post_dispatch(&running, &token, project_id, task_id, dispatch).await;
    assert_eq!(
        response.status(),
        200,
        "dispatch failed: {}",
        response.text().await.unwrap_or_default()
    );
    let body: serde_json::Value = response.json().await.unwrap();
    let run_id = body["run_id"]
        .as_str()
        .expect("dispatch response run_id")
        .to_string();

    // Guard against a false red: the assertion below reads "the lease is gone"
    // as the defect, so a run that was never live (or that the harness stub
    // already lost) has to fail with its own message instead.
    assert!(
        live_run_ids(&running, &token).await.contains(&run_id),
        "run {run_id} was not live before the finalize; this test would prove \
         nothing about the release"
    );

    // The old CLI's release payload, verbatim: it declares the worker finalize
    // and sends no tx, because that CLI still intends to POST /tx itself.
    let old_cli_release = reqwest::Client::new()
        .post(format!("http://{}/api/runs/{run_id}/release", running.addr))
        .bearer_auth(&token)
        .json(&serde_json::json!({
            "reason": format!("worker finalize for {task_id}"),
            "request_id": format!("dispatch-release-skew-{run_id}"),
            "finalized_by_worker": true,
        }))
        .send()
        .await
        .unwrap();
    let status = old_cli_release.status();
    let refusal = old_cli_release.text().await.unwrap_or_default();

    // THE LOSS, asserted first: the lease must still be held. A released run
    // here is the invisible fourth state — committed, reported to last.txt,
    // lease gone, no tx, no orphan flag.
    let still_live = live_run_ids(&running, &token).await;
    assert!(
        still_live.contains(&run_id),
        "an old-CLI finalize with no terminal_tx must leave the lease HELD, not \
         release a run nothing will ever report: run {run_id} is gone from \
         /runs/live (status {status}, body {refusal})\nlive={still_live:?}"
    );
    assert_eq!(
        status, 400,
        "the skewed release must be refused, not performed: {refusal}"
    );
    // The refusal has to name the skew and the remedy (EP3H1/TZKAC standard):
    // an operator reading only this string must know which side is old and
    // what to do next.
    for needle in [
        "terminal_tx",
        "still held",
        "orgasmic dispatch finalize",
        "orgasmic update",
    ] {
        assert!(
            refusal.contains(needle),
            "the refusal must name `{needle}` so the operator can act on it: {refusal}"
        );
    }
    assert!(
        !read_project_tx(&project_root).contains(":TYPE:         implementer.reported"),
        "a refused release must not leave a terminal tx behind"
    );

    // The guard is narrow: the same release WITH the tx the current CLI sends
    // still succeeds, and still writes it.
    let good = reqwest::Client::new()
        .post(format!("http://{}/api/runs/{run_id}/release", running.addr))
        .bearer_auth(&token)
        .json(&serde_json::json!({
            "reason": format!("worker finalize for {task_id}"),
            "request_id": format!("dispatch-release-skew-ok-{run_id}"),
            "finalized_by_worker": true,
            "terminal_tx": {
                "request_id": format!("dispatch-finalize-{task_id}-{run_id}"),
                "type": "implementer.reported",
                "actor": "agent.implementer",
                "project": project_id,
                "task": task_id,
                "extra": [["RUN_ID", run_id]],
            },
        }))
        .send()
        .await
        .unwrap();
    let good_status = good.status();
    let good_body = good.text().await.unwrap_or_default();
    assert!(
        good_status.is_success(),
        "a release carrying its terminal tx must still be accepted: \
         {good_status} {good_body}"
    );
    let deadline = std::time::Instant::now() + Duration::from_secs(20);
    loop {
        let raw = read_project_tx(&project_root);
        if raw.contains(":TYPE:         implementer.reported") {
            break;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "the accepted release must still write its terminal tx:\n{raw}"
        );
        tokio::time::sleep(Duration::from_millis(100)).await;
    }

    let _ = running.shutdown.send(());
    let _ = running.join.await;
}

/// TASK-WGXKD.1 finding 2: a controlled restart that lands INSIDE the
/// release-to-tx gap must not exit before that tx is durable.
///
/// The detached release task outlives its request by design (the caller is
/// normally dead), so nothing in the request path owns it. Before this fix its
/// only `JoinHandle` lived on the request future: the writer drain barrier —
/// which proves only that work ALREADY ENQUEUED in the writer has landed —
/// could pass while the release was still in teardown, the writer would stop,
/// and the tx would be lost. That is a graceful-restart loss, not the accepted
/// daemon-crash gap; it is every `launchctl kickstart` during a runtime
/// rebuild.
///
/// The existing disconnect test cannot catch it: it waits for the tx and only
/// then shuts down. Here the restart is initiated BEFORE the append, inside the
/// `release_terminal_tx_delay` window, and the tx is asserted the instant the
/// restart call returns — with no polling, because the restart is what must
/// have waited.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn dispatch_endpoint_controlled_restart_waits_for_pending_terminal_tx() {
    let tmp = tempfile::tempdir().unwrap();
    let home = Home::at(tmp.path().join("home"));
    home.ensure().unwrap();
    let project_root = tmp.path().join("project");
    std::fs::create_dir_all(&project_root).unwrap();
    let project_id = "proj-dispatch";
    let task_id = "TASK-RESTART-TX";
    seed_project(
        &home,
        &project_root,
        project_id,
        "implementer-codex-ws",
        task_id,
    );
    write(
        &project_root.join(".orgasmic/project.org"),
        "#+title: orgasmic\n#+orgasmic_version: 1\n\n* PROJECT orgasmic\n:PROPERTIES:\n:ID:                     orgasmic\n:END:\n",
    );
    let bin_dir = install_fake_codex(tmp.path(), "#!/bin/sh\nsleep 120\n");
    let stem_dir = project_root.join(".orgasmic/tmp/dispatch/task-restart-tx");
    std::fs::create_dir_all(&stem_dir).unwrap();
    let worktree = stem_dir.join("worktree");
    std::fs::create_dir_all(&worktree).unwrap();
    let brief = stem_dir.join("brief.md");
    write(&brief, "restart tx brief");
    let last = stem_dir.join("last.txt");
    let stdout = stem_dir.join("stdout.log");
    write(&last, "worker report");
    write(&stdout, "");

    let running = Daemon::run(
        home.clone(),
        DaemonOptions {
            release_terminal_tx_delay: Some(Duration::from_secs(3)),
            ..test_options()
        },
    )
    .await
    .expect("boot daemon");
    let token = read_token(&home);
    let mut dispatch = dispatch_body(
        "implementer",
        &brief,
        &worktree,
        &last,
        &stdout,
        Some("implementer-codex-ws"),
    );
    dispatch["driver_config"] = serde_json::json!({
        "PATH": format!("{}:{}", bin_dir.display(), std::env::var("PATH").unwrap_or_default())
    });
    let response = post_dispatch(&running, &token, project_id, task_id, dispatch).await;
    assert_eq!(
        response.status(),
        200,
        "dispatch failed: {}",
        response.text().await.unwrap_or_default()
    );
    let body: serde_json::Value = response.json().await.unwrap();
    let run_id = body["run_id"]
        .as_str()
        .expect("dispatch response run_id")
        .to_string();

    let payload = serde_json::json!({
        "reason": format!("worker finalize for {task_id}"),
        "request_id": format!("dispatch-release-restart-tx-{run_id}"),
        "finalized_by_worker": true,
        "terminal_tx": {
            "request_id": format!("dispatch-finalize-{task_id}-{run_id}"),
            "type": "implementer.reported",
            "actor": "agent.implementer",
            "project": project_id,
            "task": task_id,
            "extra": [["RUN_ID", run_id]],
        },
    })
    .to_string();
    {
        use std::io::Write as _;
        let mut socket = std::net::TcpStream::connect(running.addr).unwrap();
        write!(
            socket,
            "POST /api/runs/{run_id}/release HTTP/1.1\r\n\
             Host: {}\r\n\
             Authorization: Bearer {token}\r\n\
             Content-Type: application/json\r\n\
             Content-Length: {}\r\n\
             Connection: close\r\n\r\n{payload}",
            running.addr,
            payload.len()
        )
        .unwrap();
        socket.flush().unwrap();
        tokio::time::sleep(Duration::from_millis(500)).await;
        assert!(
            !read_project_tx(&project_root).contains(":TYPE:         implementer.reported"),
            "the tx landed before the restart was even requested — this test \
             would prove nothing"
        );
    }

    // The restart the operator (or `launchctl kickstart`) triggers, landing
    // squarely inside the gap.
    let restart = reqwest::Client::new()
        .post(format!("http://{}/api/daemon/restart", running.addr))
        .bearer_auth(&token)
        .json(&serde_json::json!({
            "reason": "runtime rebuild",
            "request_id": "wgxkd1-restart"
        }))
        .send()
        .await
        .unwrap();
    assert!(
        restart.status().is_success(),
        "controlled restart failed: {}",
        restart.status()
    );
    let restart_body: serde_json::Value = restart.json().await.unwrap();
    let warnings = restart_body["warnings"]
        .as_array()
        .cloned()
        .unwrap_or_default();
    assert!(
        !warnings
            .iter()
            .any(|warning| warning["kind"] == "release_finalization_timeout"),
        "the release should have drained well inside the timeout: {restart_body}"
    );

    // No polling: the restart response is the claim under test. If the drain
    // barrier had been placed before the release finalization was awaited, the
    // restart would have returned with the tx still unwritten.
    let reported = read_project_tx(&project_root);
    assert!(
        reported.contains(":TYPE:         implementer.reported"),
        "controlled restart returned before the pending terminal tx was \
         durable — a graceful restart would lose it:\n{reported}"
    );
    let reported_block = reported
        .split("\n* TX ")
        .find(|block| block.contains(":TYPE:         implementer.reported"))
        .expect("implementer.reported block");
    assert!(
        reported_block
            .lines()
            .any(|line| line.starts_with(":RUN_ID:") && line.contains(&run_id)),
        "the terminal tx must still carry this run's RUN_ID (TASK-6AYEJ.1):\n{reported_block}"
    );

    // A release arriving after the restart was requested is refused rather than
    // half-performed: the lease stays held and is retryable against the next
    // daemon.
    let late = reqwest::Client::new()
        .post(format!("http://{}/api/runs/{run_id}/release", running.addr))
        .bearer_auth(&token)
        .json(&serde_json::json!({
            "reason": "late finalize",
            "finalized_by_worker": true,
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(
        late.status(),
        503,
        "a release arriving after restart was requested must be refused, not \
         performed without its tx"
    );

    let _ = running.shutdown.send(());
    let _ = running.join.await;

    assert!(
        read_project_tx(&project_root).contains(":TYPE:         implementer.reported"),
        "the terminal tx must survive the old daemon actually exiting"
    );
}

/// TASK-WGXKD.2 finding 1: a release that is admitted but NOT YET registered
/// when the restart lands must still be waited for.
///
/// This is the case the existing restart test cannot reach. There, the release
/// is already a registered task by the time the restart is requested, so it
/// passes against the pre-fix code too. Here `release_admission_delay` holds the
/// handler in exactly the window the pre-fix code really had — after the
/// `is_closed()` read, before the `spawn()` that incremented the count — and the
/// restart is requested inside it. Pre-fix, the drain saw an empty tracker,
/// returned immediately, placed the writer barrier, and the terminal tx landed
/// after it. Post-fix, admission IS the registration, so the drain waits.
///
/// The restart response is the claim: the tx is asserted the instant it returns,
/// with no polling.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn dispatch_endpoint_restart_waits_for_a_release_admitted_but_not_yet_registered() {
    let tmp = tempfile::tempdir().unwrap();
    let home = Home::at(tmp.path().join("home"));
    home.ensure().unwrap();
    let project_root = tmp.path().join("project");
    std::fs::create_dir_all(&project_root).unwrap();
    let project_id = "proj-dispatch";
    let task_id = "TASK-ADMIT-RACE";
    seed_project(
        &home,
        &project_root,
        project_id,
        "implementer-codex-ws",
        task_id,
    );
    write(
        &project_root.join(".orgasmic/project.org"),
        "#+title: orgasmic\n#+orgasmic_version: 1\n\n* PROJECT orgasmic\n:PROPERTIES:\n:ID:                     orgasmic\n:END:\n",
    );
    let bin_dir = install_fake_codex(tmp.path(), "#!/bin/sh\nsleep 120\n");
    let stem_dir = project_root.join(".orgasmic/tmp/dispatch/task-admit-race");
    std::fs::create_dir_all(&stem_dir).unwrap();
    let worktree = stem_dir.join("worktree");
    std::fs::create_dir_all(&worktree).unwrap();
    let brief = stem_dir.join("brief.md");
    write(&brief, "admission race brief");
    let last = stem_dir.join("last.txt");
    let stdout = stem_dir.join("stdout.log");
    write(&last, "worker report");
    write(&stdout, "");

    let running = Daemon::run(
        home.clone(),
        DaemonOptions {
            // The whole window under test: the handler is admitted and then
            // parked here, with no task registered yet.
            release_admission_delay: Some(Duration::from_secs(3)),
            ..test_options()
        },
    )
    .await
    .expect("boot daemon");
    let token = read_token(&home);
    let mut dispatch = dispatch_body(
        "implementer",
        &brief,
        &worktree,
        &last,
        &stdout,
        Some("implementer-codex-ws"),
    );
    dispatch["driver_config"] = serde_json::json!({
        "PATH": format!("{}:{}", bin_dir.display(), std::env::var("PATH").unwrap_or_default())
    });
    let response = post_dispatch(&running, &token, project_id, task_id, dispatch).await;
    assert_eq!(
        response.status(),
        200,
        "dispatch failed: {}",
        response.text().await.unwrap_or_default()
    );
    let body: serde_json::Value = response.json().await.unwrap();
    let run_id = body["run_id"]
        .as_str()
        .expect("dispatch response run_id")
        .to_string();

    let payload = serde_json::json!({
        "reason": format!("worker finalize for {task_id}"),
        "request_id": format!("dispatch-release-admit-race-{run_id}"),
        "finalized_by_worker": true,
        "terminal_tx": {
            "request_id": format!("dispatch-finalize-{task_id}-{run_id}"),
            "type": "implementer.reported",
            "actor": "agent.implementer",
            "project": project_id,
            "task": task_id,
            "extra": [["RUN_ID", run_id]],
        },
    })
    .to_string();
    // Raw socket with `Connection: close`, as the real worker finalize does:
    // the caller is killed by this very call, so nothing is left listening.
    let socket = {
        use std::io::Write as _;
        let mut socket = std::net::TcpStream::connect(running.addr).unwrap();
        write!(
            socket,
            "POST /api/runs/{run_id}/release HTTP/1.1\r\n\
             Host: {}\r\n\
             Authorization: Bearer {token}\r\n\
             Content-Type: application/json\r\n\
             Content-Length: {}\r\n\
             Connection: close\r\n\r\n{payload}",
            running.addr,
            payload.len()
        )
        .unwrap();
        socket.flush().unwrap();
        socket
    };
    // Long enough that the handler has certainly been admitted, far short of
    // the 3s admission delay, so the restart lands strictly inside the window.
    tokio::time::sleep(Duration::from_millis(500)).await;
    assert!(
        !read_project_tx(&project_root).contains(":TYPE:         implementer.reported"),
        "the tx landed before the restart was requested — this test would prove nothing"
    );

    let restart = reqwest::Client::new()
        .post(format!("http://{}/api/daemon/restart", running.addr))
        .bearer_auth(&token)
        .json(&serde_json::json!({
            "reason": "runtime rebuild",
            "request_id": "wgxkd2-admission-race"
        }))
        .send()
        .await
        .unwrap();
    assert!(
        restart.status().is_success(),
        "controlled restart failed: {}",
        restart.status()
    );
    let restart_body: serde_json::Value = restart.json().await.unwrap();
    let warnings = restart_body["warnings"]
        .as_array()
        .cloned()
        .unwrap_or_default();
    assert!(
        !warnings
            .iter()
            .any(|warning| warning["kind"] == "release_finalization_timeout"),
        "the release should have drained well inside the timeout: {restart_body}"
    );
    assert!(
        !warnings
            .iter()
            .any(|warning| warning["kind"] == "release_finalization_lost"),
        "nothing was lost on this path: {restart_body}"
    );

    let reported = read_project_tx(&project_root);
    assert!(
        reported.contains(":TYPE:         implementer.reported"),
        "the restart returned while a release admitted-but-unregistered was \
         still running: its terminal tx is the one a graceful restart loses:\n{reported}"
    );
    let reported_block = reported
        .split("\n* TX ")
        .find(|block| block.contains(":TYPE:         implementer.reported"))
        .expect("implementer.reported block");
    assert!(
        reported_block
            .lines()
            .any(|line| line.starts_with(":RUN_ID:") && line.contains(&run_id)),
        "the terminal tx must still carry the client's verbatim RUN_ID \
         (TASK-6AYEJ.1), or the dispatch sticks at [unreported]:\n{reported_block}"
    );

    drop(socket);
    let _ = running.shutdown.send(());
    let _ = running.join.await;
}

/// TASK-NW4WV: mismatched cleanup while a live tokened worker holds the lease
/// must conflict and leave process/worktree/branch/artifacts intact.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn dispatch_cleanup_token_mismatch_while_live_worker_survives() {
    let tmp = tempfile::tempdir().unwrap();
    let home = Home::at(tmp.path().join("home"));
    home.ensure().unwrap();
    let project_root = tmp.path().join("project");
    std::fs::create_dir_all(&project_root).unwrap();
    let project_id = "proj-dispatch";
    let task_id = "TASK-CLEANUP-LIVE";
    seed_project(
        &home,
        &project_root,
        project_id,
        "implementer-codex-ws",
        task_id,
    );
    write(
        &project_root.join(".orgasmic/project.org"),
        "#+title: orgasmic\n#+orgasmic_version: 1\n\n* PROJECT orgasmic\n:PROPERTIES:\n:ID:                     orgasmic\n:END:\n",
    );
    Command::new("git")
        .args(["init", "-b", "main"])
        .current_dir(&project_root)
        .status()
        .unwrap();
    Command::new("git")
        .args(["config", "user.email", "tester@example.com"])
        .current_dir(&project_root)
        .status()
        .unwrap();
    Command::new("git")
        .args(["config", "user.name", "Test User"])
        .current_dir(&project_root)
        .status()
        .unwrap();
    write(&project_root.join("README.md"), "init\n");
    Command::new("git")
        .args(["add", "."])
        .current_dir(&project_root)
        .status()
        .unwrap();
    Command::new("git")
        .args(["commit", "-m", "init"])
        .current_dir(&project_root)
        .status()
        .unwrap();
    let head = String::from_utf8_lossy(
        &Command::new("git")
            .args(["rev-parse", "HEAD"])
            .current_dir(&project_root)
            .output()
            .unwrap()
            .stdout,
    )
    .trim()
    .to_string();

    let bin_dir = install_fake_codex(
        tmp.path(),
        "#!/bin/sh\nlast=\"\"\nwhile [ \"$#\" -gt 0 ]; do\n  if [ \"$1\" = \"--output-last-message\" ]; then\n    shift\n    last=\"$1\"\n  fi\n  shift\ndone\nif [ -n \"$last\" ]; then\n  printf 'stub-done\\n' > \"$last\"\nfi\nsleep 120\n",
    );

    let stem_dir = project_root.join(".orgasmic/tmp/dispatch/task-cleanup-live");
    std::fs::create_dir_all(&stem_dir).unwrap();
    let worktree = stem_dir.join("worktree");
    let branch = "task-cleanup-live-impl";
    Command::new("git")
        .args([
            "worktree",
            "add",
            "-B",
            branch,
            worktree.to_str().unwrap(),
            &head,
        ])
        .current_dir(&project_root)
        .status()
        .unwrap();
    let brief = stem_dir.join("task-cleanup-live-brief.md");
    write(&brief, "live cleanup mismatch brief");
    let attempt_b = "bbbb1111cccc2222dddd3333eeee4444";
    let last = stem_dir.join(format!("task-cleanup-live-{attempt_b}-last.txt"));
    let stdout = stem_dir.join(format!("task-cleanup-live-{attempt_b}-stdout.log"));
    std::fs::write(&last, "live-worker-last").unwrap();
    std::fs::write(&stdout, "live-worker-stdout").unwrap();

    let running = boot(home.clone()).await;
    let token = read_token(&home);
    let mut dispatch = dispatch_body(
        "implementer",
        &brief,
        &worktree,
        &last,
        &stdout,
        Some("implementer-codex-ws"),
    );
    dispatch["dispatch_attempt_token"] = serde_json::json!(attempt_b);
    dispatch["branch"] = serde_json::json!(branch);
    dispatch["driver_config"] = serde_json::json!({
        "PATH": format!("{}:{}", bin_dir.display(), std::env::var("PATH").unwrap_or_default())
    });
    let response = post_dispatch(&running, &token, project_id, task_id, dispatch).await;
    assert_eq!(
        response.status(),
        200,
        "dispatch failed: {}",
        response.text().await.unwrap_or_default()
    );
    let dispatch_body: serde_json::Value = response.json().await.unwrap();
    let pid = dispatch_body["pid"].as_u64().expect("live dispatch pid");

    let cleanup = post_dispatch_cleanup(
        &running,
        &token,
        project_id,
        task_id,
        serde_json::json!({
            "kind": "implementer",
            "worktree_path": worktree,
            "branch": branch,
            "dispatch_attempt_token": "aaaa1111bbbb2222cccc3333dddd4444",
            "last_path": last,
            "stdout_path": stdout,
        }),
    )
    .await;
    assert_eq!(cleanup.status(), 200, "cleanup endpoint transport failed");
    let cleanup_body: serde_json::Value = cleanup.json().await.unwrap();
    assert_eq!(cleanup_body["status"], "conflict");
    assert!(!cleanup_body["worktree_removed"].as_bool().unwrap_or(true));
    assert!(!cleanup_body["branch_deleted"].as_bool().unwrap_or(true));
    assert!(
        worktree.exists(),
        "live worker worktree must survive mismatched cleanup"
    );
    assert_eq!(std::fs::read_to_string(&last).unwrap(), "live-worker-last");
    assert_eq!(
        std::fs::read_to_string(&stdout).unwrap(),
        "live-worker-stdout"
    );
    let branch_ref = format!("refs/heads/{branch}");
    let branch_exists = Command::new("git")
        .args(["show-ref", "--verify", "--quiet", branch_ref.as_str()])
        .current_dir(&project_root)
        .status()
        .map(|status| status.success())
        .unwrap_or(false);
    assert!(
        branch_exists,
        "live worker branch must survive mismatched cleanup"
    );
    let pid_arg = pid.to_string();
    assert!(
        Command::new("kill")
            .args(["-0", pid_arg.as_str()])
            .status()
            .map(|status| status.success())
            .unwrap_or(false),
        "live worker process must survive mismatched cleanup"
    );

    let _ = running.shutdown.send(());
    let _ = running.join.await;
}
