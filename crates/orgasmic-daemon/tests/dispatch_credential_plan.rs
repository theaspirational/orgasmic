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
            "* WORKER {id}\n:PROPERTIES:\n:ID:                          {id}\n:KIND:                        implementer\n:DRIVER:                      stdio\n:HARNESS:                     claude\n:PROVIDERS:                   anthropic\n:DEFAULT_PROVIDER:            anthropic\n:LINKED_SKILLS:\n:APPLICABLE_STATES:           working, done, blocked, cancelled\n:MAX_ITERATIONS:              1\n:CONTEXT_BUDGET:              4000\n:VERSION:                     1\n:END:\n\n** Persona\nCredential-plan test worker.\n\n** Operating Rules\n- Keep the test run minimal.\n"
        ),
    );
}

fn seed_project(home: &Home, project_root: &Path, project_id: &str, task_id: &str) {
    if !home.source().exists() {
        std::os::unix::fs::symlink(repo_root(), home.source()).unwrap();
    }
    write(
        &project_root.join(format!(".orgasmic/tasks/{task_id}/node.org")),
        format!(
            "#+title: orgasmic task {task_id}\n#+orgasmic_version: 2\n\n* BACKLOG {task_id} Credential plan task\n:PROPERTIES:\n:ID:               {task_id}\n:END:\n"
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

/// The argv the stub answers a warm-up on. Not a `claude` subcommand, not a
/// flag the adapter composes, and present in no production string — which is
/// what lets the stub keep warm-ups out of the counted ledger without that
/// exemption ever covering a real probe.
const WARM_UP_ARGV: &str = "__orgasmic_warm_up";

/// What the warm-up arm prints, so a stub that execs but cannot run its own
/// script fails as a stub rather than as silence.
const WARM_UP_ACK: &str = "orgasmic-warm-up ok";

/// A `claude` that reports a live login and then behaves like the harness.
///
/// It records every invocation, because the count is half the property: a
/// dispatch must ask `claude auth status` once, before it owns anything, and
/// never again.
///
/// It is also deliberately late on its very first exec, and warms itself past
/// that lateness before the dispatch is made. See [`warm_up_stub`] — TASK-D1Z87.
fn write_recording_claude_stub(dir: &Path) -> PathBuf {
    let log = dir.join("invocations.log");
    let warmups = dir.join("warmups.log");
    let late = dir.join("late-first-exec");
    let stub = dir.join("claude");
    std::fs::write(&late, "").unwrap();
    std::fs::write(
        &stub,
        format!(
            r#"#!/bin/sh
late=""
if [ -f "{late}" ]; then rm -f "{late}"; late="1"; fi
if [ "$1" = "{warm_up_argv}" ]; then
  if [ -n "$late" ]; then sleep 6; fi
  printf '%s\n' "$*" >> "{warmups}"
  printf '%s\n' '{warm_up_ack}'
  exit 0
fi
printf '%s\n' "$*" >> "{log}"
if [ "$1" = "auth" ] && [ "$2" = "status" ]; then
  if [ -n "$late" ]; then sleep 6; fi
  printf '%s\n' '{{"loggedIn":true,"authMethod":"claude.ai","apiProvider":"firstParty"}}'
  exit 0
fi
if [ "$1" = "--version" ]; then
  exit 0
fi
printf '%s\n' '{{"type":"system","subtype":"init","session_id":"stub-session","model":"stub-model","claude_code_version":"stub"}}'
printf '%s\n' '{{"type":"result","subtype":"success","result":"stub complete"}}'
"#,
            late = late.display(),
            warm_up_argv = WARM_UP_ARGV,
            warm_up_ack = WARM_UP_ACK,
            warmups = warmups.display(),
            log = log.display()
        ),
    )
    .unwrap();
    use std::os::unix::fs::PermissionsExt;
    let mut perms = std::fs::metadata(&stub).unwrap().permissions();
    perms.set_mode(0o755);
    std::fs::set_permissions(&stub, perms).unwrap();
    warm_up_stub(&stub, &log, &warmups);
    log
}

/// Pay the first-exec cost of a file written a millisecond ago, here, where
/// there is no deadline to blow — and do it without being counted.
///
/// The preflight gives a harness five seconds to answer and treats silence as
/// "could not ask", so every second this stub spends being *started* is a
/// second of the test's premise draining away. Measured 2026-07-29 under a
/// loaded workspace run (TASK-GEZHQ): a freshly written stub's first invocation
/// never reached the first line of its own script inside the bound, while the
/// identical file exec'd normally moments later.
///
/// TASK-GEZHQ's own warm-up asks the harness its real question one extra time.
/// That is unavailable here, because this stub *remembers*: `auth status`
/// appends to the ledger the assertion below counts, and asking it twice would
/// turn "one auth status per dispatch" into a number the test manufactured. So
/// the warm-up is a third argv the stub answers above the ledger, and the
/// exemption cannot mask a double probe:
///
/// - It is keyed to an argv production cannot produce — every argv the daemon
///   or adapter can compose still falls through to the `printf … >> log` line.
/// - Nothing is un-recorded: warm-ups get their own ledger, and this function
///   asserts the warm-up landed there and that the counted ledger is still
///   empty before the dispatch is made.
///
/// The stub's answer is asserted, so a warm-up failure fails as a stub failure
/// rather than as a mystified plan assertion two hundred lines below.
fn warm_up_stub(stub: &Path, log: &Path, warmups: &Path) {
    let output = std::process::Command::new(stub)
        .arg(WARM_UP_ARGV)
        .output()
        .expect("the recording stub must be executable");
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        output.status.success() && stdout.contains(WARM_UP_ACK),
        "the stub must run its own script before the preflight is asked to \
         believe it: status {:?}, stdout {stdout:?}",
        output.status.code()
    );
    assert_eq!(
        std::fs::read_to_string(warmups)
            .unwrap_or_default()
            .lines()
            .count(),
        1,
        "the warm-up must be recorded, in its own ledger — an un-recorded exec \
         is one a double-probe regression could hide behind"
    );
    assert_eq!(
        std::fs::read_to_string(log).unwrap_or_default(),
        String::new(),
        "the warm-up must not enter the ledger the one-auth-status-per-dispatch \
         count is read from; if it does, that count is no longer a measurement \
         of production"
    );
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
    let worker_id = "implementer-claude-stdio";
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
            "mode": "stdio",
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
