// orgasmic:TASK-3CM0Q
//! `orgasmic manager tier` — production-path probe for the CLI ↔ daemon tier
//! declaration (TASK-3CM0Q).
//!
//! The daemon-side contract is pinned in
//! `crates/orgasmic-daemon/tests/manager_tier_endpoint.rs`. What this file adds
//! is the half that failure actually travels through: the real binary, spawned
//! as a process, against a real daemon. The obligation this verb creates is a
//! writing one, and a writing obligation nobody can execute from a terminal is
//! no obligation at all.
//!
//! Two properties are load-bearing here and are not visible from the endpoint
//! tests:
//!
//! - declaring `trivial` is **one command** with no flags beyond the task and
//!   the tier, and no round trip. A discipline that costs more than that is a
//!   discipline that gets skipped, which is the whole failure this task exists
//!   to close;
//! - reading an undeclared task **exits non-zero**, so the omission is
//!   scriptable rather than merely legible.

use std::path::{Path, PathBuf};
use std::process::Output;

use orgasmic_core::Home;
use orgasmic_daemon::{Daemon, DaemonOptions, RunningDaemon};

// orgasmic:task_K5NDR
#[path = "common/env_isolation.rs"]
mod env_isolation;
use env_isolation::orgasmic_command;

fn test_options() -> DaemonOptions {
    DaemonOptions {
        bind_override: Some("127.0.0.1".parse().unwrap()),
        port_override: Some(0),
        ..DaemonOptions::default()
    }
}

async fn boot(home: Home) -> RunningDaemon {
    home.ensure().unwrap();
    std::fs::write(home.config(), "bind_host: 127.0.0.1\nbind_port: 65531\n").unwrap();
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
        if here.join("shipped/entry/router.org").is_file() {
            return here;
        }
        if !here.pop() {
            panic!("could not locate orgasmic repo root from CARGO_MANIFEST_DIR");
        }
    }
}

fn seed_project(home: &Home, project_root: &Path, project_id: &str) {
    if !home.source().exists() {
        std::os::unix::fs::symlink(repo_root(), home.source()).unwrap();
    }
    write(
        &project_root.join(".orgasmic/project.org"),
        format!(
            "#+title: {project_id}\n#+orgasmic_version: 1\n\n* PROJECT {project_id}\n:PROPERTIES:\n:ID:               {project_id}\n:END:\n"
        ),
    );
    write(
        &project_root.join(".orgasmic/tasks/todo.org"),
        "#+title: todo\n#+orgasmic_version: 1\n\n* TODO TASK-TIER Declare a tier :cli:\n:PROPERTIES:\n:ID:               TASK-TIER\n:END:\n",
    );
    write(
        &home.board(),
        format!(
            "#+title: orgasmic board\n#+orgasmic_version: 1\n\n* PROJECT {project_id}\n:PROPERTIES:\n:ID:               {project_id}\n:PATH:             {}\n:BRANCH:           main\n:STATUS:           active\n:END:\n",
            project_root.display()
        ),
    );
}

/// `ORGASMIC_DAEMON_URL` is what keeps this a probe and not a hazard: with it
/// set, `ensure_running` short-circuits before any adapter start, so the child
/// talks to the fixture daemon and never touches the operator's.
fn run_orgasmic(home: &Home, running: &RunningDaemon, cwd: &Path, args: &[&str]) -> Output {
    orgasmic_command()
        .args(args)
        .current_dir(cwd)
        .env("ORGASMIC_HOME", &home.root)
        .env("ORGASMIC_DAEMON_URL", format!("http://{}", running.addr))
        .output()
        .expect("run orgasmic")
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn tier_declaration_round_trips_through_the_real_binary() {
    let tmp = tempfile::tempdir().unwrap();
    let home = Home::at(tmp.path().join("home"));
    home.ensure().unwrap();
    let project_root = tmp.path().join("proj");
    seed_project(&home, &project_root, "cli-tier-proj");
    let running = boot(home.clone()).await;

    // Undeclared: the read fails loudly. This is the state a manager is in when
    // it opens a source file having never classified the work, and the exit
    // code is what makes it checkable rather than merely readable.
    let undeclared = run_orgasmic(
        &home,
        &running,
        &project_root,
        &["manager", "tier", "--task", "TASK-TIER"],
    );
    let stderr = String::from_utf8_lossy(&undeclared.stderr).to_string();
    assert!(
        !undeclared.status.success(),
        "an undeclared task must not read as fine\nstdout={}\nstderr={stderr}",
        String::from_utf8_lossy(&undeclared.stdout)
    );
    assert!(
        stderr.contains("no tier declared for TASK-TIER") && stderr.contains("out of policy"),
        "stderr={stderr}"
    );

    // The cheap path, in full: task, tier, done.
    let declared = run_orgasmic(
        &home,
        &running,
        &project_root,
        &[
            "manager",
            "tier",
            "--task",
            "TASK-TIER",
            "--tier",
            "trivial",
        ],
    );
    let stdout = String::from_utf8_lossy(&declared.stdout).to_string();
    assert!(
        declared.status.success(),
        "declare trivial failed\nstdout={stdout}\nstderr={}",
        String::from_utf8_lossy(&declared.stderr)
    );
    assert!(
        stdout.contains("declared TASK-TIER trivial") && stdout.contains("triggers: none"),
        "stdout={stdout}"
    );
    assert!(
        stdout.contains("tx-"),
        "the declaration reports the tx it landed as: {stdout}"
    );

    let read_back = run_orgasmic(
        &home,
        &running,
        &project_root,
        &["manager", "tier", "--task", "TASK-TIER"],
    );
    let stdout = String::from_utf8_lossy(&read_back.stdout).to_string();
    assert!(
        read_back.status.success(),
        "read back failed\nstdout={stdout}\nstderr={}",
        String::from_utf8_lossy(&read_back.stderr)
    );
    assert!(
        stdout.contains("TASK-TIER is declared trivial"),
        "stdout={stdout}"
    );

    // A raised tier with nothing to justify it is refused at the CLI boundary
    // too, so the arithmetic a reader checks is never absent from the record.
    let unjustified = run_orgasmic(
        &home,
        &running,
        &project_root,
        &["manager", "tier", "--task", "TASK-TIER", "--tier", "risky"],
    );
    assert!(
        !unjustified.status.success(),
        "risky with no trigger should fail\nstdout={}",
        String::from_utf8_lossy(&unjustified.stdout)
    );

    // Scope that grew: one command, no flag, no permission asked.
    let raised = run_orgasmic(
        &home,
        &running,
        &project_root,
        &[
            "manager",
            "tier",
            "--task",
            "TASK-TIER",
            "--tier",
            "ordinary",
            "--triggers",
            "breadth,coupling",
        ],
    );
    let stdout = String::from_utf8_lossy(&raised.stdout).to_string();
    assert!(
        raised.status.success(),
        "re-declare upward failed\nstdout={stdout}\nstderr={}",
        String::from_utf8_lossy(&raised.stderr)
    );
    assert!(
        stdout.contains("re-declared TASK-TIER trivial → ordinary")
            && stdout.contains("triggers: breadth, coupling"),
        "stdout={stdout}"
    );
    assert!(
        stdout.contains("is not manager-direct"),
        "an above-floor tier says so, since manager-direct is the thing it \
         withdraws: {stdout}"
    );

    let _ = running.shutdown.send(());
    let _ = running.join.await;
}
