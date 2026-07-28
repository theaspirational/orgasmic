//! The credential decision a dispatch is admitted on must reach the launch.
//!
//! TASK-KKBTP's acceptance, at the layer where the boundary actually is. The
//! driver-level regressions in `orgasmic-drivers` prove the adapter pins a plan
//! and that composition applies it; they cannot prove the *daemon* carries the
//! plan across the step in between, because that step —
//! `spawn_worker_run`'s `preflight.pin_into(&driver_config)` — only exists on
//! the dispatch endpoint. Without this test, "the daemon pins it" is a claim
//! supported by reading the code.
//!
//! Why this lives in its own test binary rather than in `dispatch_endpoint.rs`:
//! it must control `PATH`, `ANTHROPIC_API_KEY` and `CLAUDE_CONFIG_DIR`, all of
//! which are process-global state shared by every test in a binary
//! (`.orgasmic/gotchas.org`). A cargo test binary is a process, so keeping this
//! file to a single test is what makes the mutation safe.

use std::path::{Path, PathBuf};

use orgasmic_core::Home;
use orgasmic_daemon::{Daemon, DaemonOptions};

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
        if here.join(".orgasmic").is_dir() && here.join("shipped").is_dir() {
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
            "* WORKER {id}\n:PROPERTIES:\n:ID:                          {id}\n:KIND:                        implementer\n:DRIVER:                      acp-stdio\n:HARNESS:                     claude\n:PROVIDERS:                   anthropic\n:DEFAULT_PROVIDER:            anthropic\n:LINKED_SKILLS:\n:APPLICABLE_STATES:           working, done, blocked, cancelled\n:MAX_ITERATIONS:              1\n:CONTEXT_BUDGET:              4000\n:VERSION:                     1\n:END:\n\n** Persona\nCredential-plan test worker.\n\n** Operating Rules\n- Keep the test run minimal.\n"
        ),
    );
}

fn seed_project(home: &Home, project_root: &Path, project_id: &str, task_id: &str) {
    if !home.source().exists() {
        std::os::unix::fs::symlink(repo_root(), home.source()).unwrap();
    }
    write(
        &project_root.join(".orgasmic/tasks/backlog.org"),
        format!(
            "#+title: sprint\n#+orgasmic_version: 1\n\n* BACKLOG {task_id} Credential plan task\n:PROPERTIES:\n:ID:               {task_id}\n:END:\n"
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

/// A `claude` that reports a live login and then behaves like the harness.
///
/// It records every invocation, because the count is half the property: a
/// dispatch must ask `claude auth status` once, before it owns anything, and
/// never again.
fn write_recording_claude_stub(dir: &Path) -> PathBuf {
    let log = dir.join("invocations.log");
    let stub = dir.join("claude");
    std::fs::write(
        &stub,
        format!(
            r#"#!/bin/sh
printf '%s\n' "$*" >> "{log}"
if [ "$1" = "auth" ] && [ "$2" = "status" ]; then
  printf '%s\n' '{{"loggedIn":true,"authMethod":"claude.ai","apiProvider":"firstParty"}}'
  exit 0
fi
if [ "$1" = "--version" ]; then
  exit 0
fi
printf '%s\n' '{{"type":"system","subtype":"init","session_id":"stub-session","model":"stub-model","claude_code_version":"stub"}}'
printf '%s\n' '{{"type":"result","subtype":"success","result":"stub complete"}}'
"#,
            log = log.display()
        ),
    )
    .unwrap();
    use std::os::unix::fs::PermissionsExt;
    let mut perms = std::fs::metadata(&stub).unwrap().permissions();
    perms.set_mode(0o755);
    std::fs::set_permissions(&stub, perms).unwrap();
    log
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

/// The daemon must carry the admitted credential decision into the launch.
///
/// The stub reports a live login and no key is present, so the only mode a
/// preflight can admit is `native_login`. Three things then have to agree, and
/// each is a different layer: the plan the daemon pinned onto the driver config,
/// the mode the supervisor recorded in `RunMeta`, and the argv the harness was
/// actually launched with. A dispatch that re-detected anywhere below admission
/// could disagree with all three, which is precisely what TASK-Z8WEJ's reviewer
/// found and what the count assertion pins shut.
#[tokio::test]
async fn the_daemon_pins_the_admitted_credential_plan_into_the_launch() {
    // Process-global, and safe here only because this binary holds one test.
    std::env::remove_var("ORGASMIC_DRIVER_SIMULATE");
    std::env::remove_var("ANTHROPIC_API_KEY");
    let harness_dir = tempfile::tempdir().unwrap();
    let log = write_recording_claude_stub(harness_dir.path());
    let saved_path = std::env::var("PATH").unwrap_or_default();
    std::env::set_var(
        "PATH",
        format!("{}:{saved_path}", harness_dir.path().display()),
    );
    // An empty settings dir, so the operator's real `apiKeyHelper` cannot
    // decide this test's outcome.
    let settings_dir = tempfile::tempdir().unwrap();
    std::env::set_var("CLAUDE_CONFIG_DIR", settings_dir.path());

    let tmp = tempfile::tempdir().unwrap();
    let home = Home::at(tmp.path().join("home"));
    home.ensure().unwrap();
    let project_root = tmp.path().join("proj");
    let worker_id = "implementer-claude-acp-stdio";
    let task_id = "TASK-CREDENTIAL-PLAN";
    seed_worker(&home, worker_id);
    seed_project(&home, &project_root, "proj-credential-plan", task_id);
    let brief = tmp.path().join("brief.md");
    let worktree = tmp.path().join("worktree");
    write(&brief, "credential plan dispatch brief\n");
    std::fs::create_dir_all(&worktree).unwrap();

    let running = Daemon::run(home.clone(), test_options())
        .await
        .expect("boot daemon");
    let token = read_token(&home);
    let response = reqwest::Client::new()
        .post(format!(
            "http://{}/api/projects/proj-credential-plan/tasks/{task_id}/dispatch",
            running.addr
        ))
        .bearer_auth(&token)
        .json(&serde_json::json!({
            "kind": "implementer",
            "mode": "acp-stdio",
            "harness": "claude",
            "brief_path": brief,
            "worktree_path": worktree,
            "last_path": tmp.path().join("last.txt"),
            "stdout_path": tmp.path().join("stdout.log"),
            "worker_id": worker_id,
            "branch": "task-credential-plan",
            "liveness": "deadbeef",
            "reason": "credential plan test",
        }))
        .send()
        .await
        .unwrap();
    let status = response.status();
    let body = response.text().await.unwrap();
    assert_eq!(
        status,
        reqwest::StatusCode::OK,
        "a logged-in claude must be dispatchable: {body}"
    );

    // The stub writes its log before it prints, so waiting for the harness's
    // own invocation to land is what makes the count below deterministic.
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(20);
    let run_meta = loop {
        let found = session_lines(&project_root).into_iter().find(|line| {
            line["event"]["phase"] == "run_meta" && line["event"]["harness"] == "claude"
        });
        if let Some(found) = found {
            break found;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "no RunMeta lifecycle event was written for the dispatch"
        );
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    };

    std::env::set_var("PATH", &saved_path);
    std::env::remove_var("CLAUDE_CONFIG_DIR");

    // 1. The plan the daemon pinned between admitting the dispatch and
    //    acquiring it, persisted with the rest of the driver config.
    let plan = &run_meta["event"]["driver_config"]["credential_plan"];
    assert_eq!(
        plan["mode"], "native_login",
        "the daemon must pin the mode its preflight admitted: {run_meta}"
    );
    assert_eq!(
        plan["native_login"], "present",
        "and record what detection saw, so a failed run says why: {run_meta}"
    );

    // 2. The mode the supervisor recorded for the run it actually spawned.
    assert_eq!(
        run_meta["event"]["credential_mode"], "native_login",
        "RunMeta must record the admitted mode: {run_meta}"
    );

    // 3. The count, named. One dispatch asks the harness about its credential
    //    exactly once — before ownership.
    let invocations: Vec<String> = std::fs::read_to_string(&log)
        .unwrap_or_default()
        .lines()
        .map(str::to_string)
        .collect();
    let asked: Vec<&String> = invocations
        .iter()
        .filter(|line| line.starts_with("auth status") || line.starts_with("--version"))
        .collect();
    assert_eq!(
        asked.len(),
        1,
        "one `auth status` at preflight and nothing else; observed {invocations:?}"
    );

    let _ = running.shutdown.send(());
    let _ = running.join.await;
}
