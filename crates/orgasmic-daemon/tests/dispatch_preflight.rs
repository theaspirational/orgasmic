//! A dispatch whose harness cannot authenticate must leave nothing behind.
//!
//! TASK-TJKFC's end-to-end acceptance: the preflight's whole value is that the
//! rejection happens *before* anything is committed, and the only way to show
//! that is to drive a real dispatch through the daemon and then look for the
//! absence of a lease, a session file and a run record.
//!
//! Why this lives in its own test binary rather than in `dispatch_endpoint.rs`:
//! it must control `ANTHROPIC_API_KEY`, which is process-global state shared by
//! every test in a binary (`.orgasmic/gotchas.org`). A cargo test binary is a
//! process, so keeping this file to a single test is what makes the mutation
//! safe — there is no concurrent test to observe it.

use std::path::{Path, PathBuf};

mod common;

use common::assert_path_free_error;
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

fn seed_worker(home: &Home, id: &str, driver: &str, harness: &str, provider: &str) {
    write(
        &home.user().join(format!("workers/{id}.org")),
        format!(
            "* WORKER {id}\n:PROPERTIES:\n:ID:                          {id}\n:KIND:                        implementer\n:DRIVER:                      {driver}\n:HARNESS:                     {harness}\n:PROVIDERS:                   {provider}\n:DEFAULT_PROVIDER:            {provider}\n:LINKED_SKILLS:\n:APPLICABLE_STATES:           working, done, blocked, cancelled\n:MAX_ITERATIONS:              1\n:CONTEXT_BUDGET:              4000\n:VERSION:                     1\n:END:\n\n** Persona\nPreflight test worker.\n\n** Operating Rules\n- Keep the test run minimal.\n"
        ),
    );
}

fn seed_project(home: &Home, project_root: &Path, project_id: &str, task_id: &str) {
    symlink_repo_source(home);
    write(
        &project_root.join(".orgasmic/tasks/backlog.org"),
        format!(
            "#+title: sprint\n#+orgasmic_version: 1\n\n* BACKLOG {task_id} Preflight task\n:PROPERTIES:\n:ID:               {task_id}\n:END:\n"
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

/// Every session file the daemon could have written for this project.
fn session_files(project_root: &Path) -> Vec<PathBuf> {
    let sessions_dir = project_root.join(".orgasmic/tmp/sessions");
    let Ok(entries) = std::fs::read_dir(&sessions_dir) else {
        return Vec::new();
    };
    entries.flatten().map(|entry| entry.path()).collect()
}

fn project_tx(project_root: &Path) -> String {
    let mut raw = String::new();
    if let Ok(entries) = std::fs::read_dir(project_root.join(".orgasmic/tx")) {
        for entry in entries.flatten() {
            if entry.path().extension().and_then(|ext| ext.to_str()) == Some("org") {
                raw.push_str(&std::fs::read_to_string(entry.path()).unwrap_or_default());
            }
        }
    }
    raw
}

async fn runs_json(running: &RunningDaemon, token: &str) -> serde_json::Value {
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

/// An unusable credential must be caught before the dispatch owns anything.
///
/// The failure this reproduces is not hypothetical: on 2026-07-25 a dispatch
/// whose harness could not authenticate still acquired a lease, wrote a session
/// file, and left an open dispatch record, a worktree, a branch and a live
/// process behind. Rejecting late is what made all of that litter; rejecting
/// early is the fix, and "early" only means anything if nothing was created.
///
/// The credential is made unusable in the one way that is both deterministic
/// and honest — an *empty* `ANTHROPIC_API_KEY`. That is a certain failure
/// knowable without asking the provider, so the verdict costs no network call
/// and no money (a probe that submits a real turn was measured at $0.0994 per
/// dispatch and rejected on that ground). It also exercises the real
/// `resolve_credentials` path: an inherited key selects `--bare`, whose whole
/// contract is that no other credential source is read, so there is nothing
/// left to fall back on.
///
/// The assertion on the error text is load-bearing, not decoration. Without it
/// the absence checks below would pass just as well if the dispatch had been
/// refused for some unrelated reason — a bad worktree, an unsupported pair —
/// and the test would prove nothing about the preflight.
#[tokio::test]
async fn an_unusable_credential_leaves_no_lease_no_session_and_no_run() {
    // Process-global, and safe here only because this binary holds one test.
    std::env::set_var("ANTHROPIC_API_KEY", "");
    // A forced simulation would skip the probe entirely and defeat the test.
    std::env::remove_var("ORGASMIC_DRIVER_SIMULATE");

    let tmp = tempfile::tempdir().unwrap();
    let home = Home::at(tmp.path().join("home"));
    home.ensure().unwrap();
    let project_root = tmp.path().join("proj");
    let worker_id = "implementer-claude-acp-stdio";
    let task_id = "TASK-PREFLIGHT-ABSENT";
    seed_worker(&home, worker_id, "acp-stdio", "claude", "anthropic");
    seed_project(&home, &project_root, "proj-preflight", task_id);
    let brief = tmp.path().join("brief.md");
    let worktree = tmp.path().join("worktree");
    let last = tmp.path().join("last.txt");
    let stdout = tmp.path().join("stdout.log");
    write(&brief, "preflight dispatch brief\n");
    std::fs::create_dir_all(&worktree).unwrap();

    let running = Daemon::run(home.clone(), test_options())
        .await
        .expect("boot daemon");
    let token = read_token(&home);
    let response = reqwest::Client::new()
        .post(format!(
            "http://{}/api/projects/proj-preflight/tasks/{task_id}/dispatch",
            running.addr
        ))
        .bearer_auth(&token)
        .json(&serde_json::json!({
            "kind": "implementer",
            "mode": "acp-stdio",
            "harness": "claude",
            "brief_path": brief,
            "worktree_path": worktree,
            "last_path": last,
            "stdout_path": stdout,
            "worker_id": worker_id,
            "branch": "task-preflight",
            "liveness": "deadbeef",
            "reason": "preflight absence test",
        }))
        .send()
        .await
        .unwrap();

    let status = response.status();
    let body = response.text().await.unwrap();
    assert_eq!(
        status,
        reqwest::StatusCode::BAD_REQUEST,
        "preflight rejection must be a 400 so the CLI treats it as an \
         unambiguous refusal and rolls back the worktree it created: {body}"
    );
    // Named reason, and no secret or path in it — this text reaches tx records
    // and durable task evidence.
    let error = assert_path_free_error(
        &body,
        "acp-stdio/claude cannot start a worker",
        &[
            project_root.as_path(),
            home.root.as_path(),
            brief.as_path(),
            worktree.as_path(),
        ],
    );
    assert!(
        error.contains("ANTHROPIC_API_KEY"),
        "the reason must name the credential the operator has to fix: {error}"
    );

    // Nothing may have been committed. Each of these is one of the artifacts
    // the 2026-07-25 incident actually left behind.
    assert_eq!(
        session_files(&project_root),
        Vec::<PathBuf>::new(),
        "a rejected dispatch must not write a session file"
    );
    let runs = runs_json(&running, &token).await;
    let live = runs["live"].as_array().cloned().unwrap_or_default();
    assert!(
        !live
            .iter()
            .any(|run| run["task_id"].as_str() == Some(task_id)),
        "a rejected dispatch must not hold a lease: {runs}"
    );
    let tx = project_tx(&project_root);
    assert!(
        !tx.contains("run.created"),
        "a rejected dispatch must not record a run: {tx}"
    );

    // The lease must be genuinely free rather than merely unreported: a second
    // dispatch is refused with 409 when one is held, so the same 400 proves the
    // task is still dispatchable once the operator fixes the credential.
    let retry = reqwest::Client::new()
        .post(format!(
            "http://{}/api/projects/proj-preflight/tasks/{task_id}/dispatch",
            running.addr
        ))
        .bearer_auth(&token)
        .json(&serde_json::json!({
            "kind": "implementer",
            "mode": "acp-stdio",
            "harness": "claude",
            "brief_path": brief,
            "worktree_path": worktree,
            "last_path": last,
            "stdout_path": stdout,
            "worker_id": worker_id,
            "branch": "task-preflight",
            "liveness": "deadbeef",
            "reason": "preflight absence test retry",
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(
        retry.status(),
        reqwest::StatusCode::BAD_REQUEST,
        "the first rejection must not have left a lease behind"
    );

    let _ = running.shutdown.send(());
    let _ = running.join.await;
}
