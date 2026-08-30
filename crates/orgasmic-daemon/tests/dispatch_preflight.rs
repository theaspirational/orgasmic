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
        if here.join("shipped/entry/router.org").is_file() {
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
        &project_root.join(format!(".orgasmic/tasks/{task_id}/node.org")),
        format!(
            "#+title: orgasmic task {task_id}\n#+orgasmic_version: 2\n\n* BACKLOG {task_id} Preflight task\n:PROPERTIES:\n:ID:               {task_id}\n:END:\n"
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
/// The credential is made unusable without asking the provider anything, so the
/// verdict costs no network call and no money (a probe that submits a real turn
/// was measured at $0.0994 per dispatch and rejected on that ground): no login,
/// no key, no `apiKeyHelper`.
///
/// It used to rely on an *empty* `ANTHROPIC_API_KEY` alone, and that stopped
/// working when TASK-S0QRM established that an empty key is not a credential
/// (measured: `ANTHROPIC_API_KEY=""` reports `apiKeySource: "none"`, exactly as
/// an unset one does). With no key to prefer, the mode fell through to whatever
/// the *developer's own machine* answered — so this test asserted a rejection
/// while passing only on a logged-out machine, and failed on main on a
/// logged-in one. Nothing about the credential under test may come from the
/// machine running the suite, so all three sources are now pinned: an empty
/// key, a `claude` stub that reports itself logged out, and an empty
/// `CLAUDE_CONFIG_DIR` so the operator's real `apiKeyHelper` cannot decide it.
///
/// Pinning the three *sources* turned out not to be enough, because the machine
/// had a fourth way in: it can make the stub too slow to be heard. See
/// [`warm_up_stub`] — TASK-GEZHQ.
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
    let harness_dir = tempfile::tempdir().unwrap();
    write_logged_out_claude_stub(harness_dir.path());
    warm_up_stub(harness_dir.path());
    let saved_path = std::env::var("PATH").unwrap_or_default();
    std::env::set_var(
        "PATH",
        format!("{}:{saved_path}", harness_dir.path().display()),
    );
    let settings_dir = tempfile::tempdir().unwrap();
    std::env::set_var("CLAUDE_CONFIG_DIR", settings_dir.path());

    let tmp = tempfile::tempdir().unwrap();
    let home = Home::at(tmp.path().join("home"));
    home.ensure().unwrap();
    let project_root = tmp.path().join("proj");
    let worker_id = "implementer-claude-stdio";
    let task_id = "TASK-PREFLIGHT-ABSENT";
    seed_worker(&home, worker_id, "stdio", "claude", "anthropic");
    seed_project(&home, &project_root, "proj-preflight", task_id);
    let brief = tmp.path().join("brief.md");
    let worktree = tmp.path().join("worktree");
    let last = tmp.path().join("task-last.txt");
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
            "mode": "stdio",
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
        "stdio/claude cannot start a worker",
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
            "mode": "stdio",
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

    std::env::set_var("PATH", &saved_path);
    std::env::remove_var("CLAUDE_CONFIG_DIR");
    let _ = running.shutdown.send(());
    let _ = running.join.await;
}

/// Pay the first-exec cost of a file written a millisecond ago, here, where
/// there is no deadline to blow.
///
/// The preflight gives a harness a bounded few seconds to answer and treats
/// silence as "could not ask", which is not a rejection — so any
/// second this stub spends being started is a second of the test's own premise
/// draining away. Measured 2026-07-29 under a loaded workspace test run
/// (TASK-GEZHQ): the stub's first `auth status` never reached the first line of
/// its own script inside the bound — a child still waiting to exec — while the
/// identical file exec'd normally moments later in the same process. The
/// dispatch was admitted, a run was created, and the test read 200 where it
/// requires 400.
///
/// So the first invocation is made here, synchronously and unbounded, and its
/// answer is asserted. That does two things: it moves macOS's one-time
/// evaluation of a brand-new executable off the probe's budget, and it turns a
/// stub that cannot answer at all into a failure that says so, instead of one
/// that surfaces as a mystified 200 much further down.
fn warm_up_stub(dir: &Path) {
    let output = std::process::Command::new(dir.join("claude"))
        .args(["auth", "status"])
        .output()
        .expect("the stub must be executable");
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains("\"loggedIn\":false"),
        "the stub must answer logged-out before the preflight is asked to \
         believe it: {stdout:?}"
    );
}

/// A `claude` that answers `auth status` with the logged-out payload the real
/// harness emits, so the credential this test rejects is the test's own and not
/// the developer's.
///
/// `claude auth status` exits 1 when logged out (measured 2026-07-25 on 2.1.220)
/// and the adapter deliberately reads the payload rather than the exit status;
/// the stub reproduces both so it cannot pass for the wrong reason.
fn write_logged_out_claude_stub(dir: &Path) {
    let stub = dir.join("claude");
    std::fs::write(
        &stub,
        r#"#!/bin/sh
if [ "$1" = "auth" ] && [ "$2" = "status" ]; then
  printf '%s\n' '{"loggedIn":false,"authMethod":"none","apiProvider":"firstParty"}'
  exit 1
fi
if [ "$1" = "--version" ]; then
  exit 0
fi
echo "unexpected stub invocation: $*" >&2
exit 3
"#,
    )
    .unwrap();
    use std::os::unix::fs::PermissionsExt;
    let mut perms = std::fs::metadata(&stub).unwrap().permissions();
    perms.set_mode(0o755);
    std::fs::set_permissions(&stub, perms).unwrap();
}
