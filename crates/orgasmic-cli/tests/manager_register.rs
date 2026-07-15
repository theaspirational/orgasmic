// orgasmic:dec_3Y2E1
//! `orgasmic manager register`/`release` (dec_3Y2E1, TASK-VKP5T): production
//! -path probe for the CLI ↔ daemon registration round trip, and the
//! `ORGASMIC_RUN_ID` no-op contract the entry router's unconditional step
//! relies on.

use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use orgasmic_core::Home;
use orgasmic_daemon::{Daemon, DaemonOptions, RunningDaemon};

fn test_options() -> DaemonOptions {
    DaemonOptions {
        bind_override: Some("127.0.0.1".parse().unwrap()),
        port_override: Some(0),
        ..DaemonOptions::default()
    }
}

async fn boot(home: Home) -> RunningDaemon {
    home.ensure().unwrap();
    std::fs::write(home.config(), "bind_host: 127.0.0.1\nbind_port: 65533\n").unwrap();
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

fn seed_project(home: &Home, project_root: &Path, project_id: &str) {
    symlink_repo_source(home);
    write(
        &project_root.join(".orgasmic/project.org"),
        &format!(
            "#+title: {project_id}\n#+orgasmic_version: 1\n\n* PROJECT {project_id}\n:PROPERTIES:\n:ID:               {project_id}\n:END:\n"
        ),
    );
    write(
        &project_root.join(".orgasmic/tasks/backlog.org"),
        "#+title: backlog\n#+orgasmic_version: 1\n\n* BACKLOG TASK-PRE Pre-boot task :cli:\n:PROPERTIES:\n:ID:               TASK-PRE\n:END:\n",
    );
    write(
        &home.board(),
        &format!(
            "#+title: orgasmic board\n#+orgasmic_version: 1\n\n* PROJECT {project_id}\n:PROPERTIES:\n:ID:               {project_id}\n:PATH:             {}\n:BRANCH:           main\n:STATUS:           active\n:END:\n",
            project_root.display()
        ),
    );
}

fn orgasmic_exe() -> PathBuf {
    let exe = PathBuf::from(env!("CARGO_BIN_EXE_orgasmic"));
    if exe.is_absolute() {
        exe
    } else {
        std::env::current_dir().unwrap().join(exe)
    }
}

fn run_orgasmic(
    home: &Home,
    running: &RunningDaemon,
    cwd: &Path,
    args: &[&str],
    extra_env: &[(&str, &str)],
) -> Output {
    let mut command = Command::new(orgasmic_exe());
    command
        .args(args)
        .current_dir(cwd)
        .env("ORGASMIC_HOME", &home.root)
        .env("ORGASMIC_DAEMON_URL", format!("http://{}", running.addr));
    for (key, value) in extra_env {
        command.env(key, value);
    }
    command.output().expect("run orgasmic")
}

/// The entry router (`shipped/entry/router.org`) runs `orgasmic manager
/// register` unconditionally on every manager startup with no conditional
/// prose — the command itself must no-op with exit 0 whenever the daemon
/// already exported `ORGASMIC_RUN_ID` into this session (a PTY it launched).
/// No daemon needs to be reachable: the check happens before any network call.
#[test]
fn orgasmic_run_id_env_set_is_noop_exit_0() {
    let tmp = tempfile::tempdir().unwrap();
    let home = Home::at(tmp.path().join("home"));
    home.ensure().unwrap();

    let mut command = Command::new(orgasmic_exe());
    command
        .args(["manager", "register", "--project", "does-not-matter"])
        .current_dir(tmp.path())
        .env("ORGASMIC_HOME", &home.root)
        .env("ORGASMIC_DAEMON_URL", "http://127.0.0.1:1")
        .env("ORGASMIC_RUN_ID", "run-already-supervised-test");
    let output = command.output().expect("run orgasmic manager register");
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        output.status.success(),
        "expected exit 0 when ORGASMIC_RUN_ID is set\nstdout={stdout}\nstderr={stderr}"
    );
    assert!(
        stdout.contains("already supervised as run-already-supervised-test"),
        "stdout={stdout}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn register_then_release_round_trip_through_real_daemon() {
    let tmp = tempfile::tempdir().unwrap();
    let home = Home::at(tmp.path().join("home"));
    home.ensure().unwrap();
    let project_root = tmp.path().join("proj");
    seed_project(&home, &project_root, "cli-register-proj");
    let running = boot(home.clone()).await;

    let register = run_orgasmic(
        &home,
        &running,
        &project_root,
        &["manager", "register", "--project", "cli-register-proj"],
        &[],
    );
    let register_stdout = String::from_utf8_lossy(&register.stdout).to_string();
    assert!(
        register.status.success(),
        "register failed\nstdout={register_stdout}\nstderr={}",
        String::from_utf8_lossy(&register.stderr)
    );
    assert!(
        register_stdout.contains("registered manager for cli-register-proj as run-"),
        "stdout={register_stdout}"
    );

    // Same test process/session re-registering: the daemon sees the same
    // terminal session-leader pid, so this is an idempotent refresh, not a
    // collision.
    let reregister = run_orgasmic(
        &home,
        &running,
        &project_root,
        &["manager", "register", "--project", "cli-register-proj"],
        &[],
    );
    let reregister_stdout = String::from_utf8_lossy(&reregister.stdout).to_string();
    assert!(
        reregister.status.success(),
        "reregister failed\nstdout={reregister_stdout}\nstderr={}",
        String::from_utf8_lossy(&reregister.stderr)
    );
    assert!(
        reregister_stdout.contains("manager registration for cli-register-proj refreshed"),
        "stdout={reregister_stdout}"
    );

    let release = run_orgasmic(
        &home,
        &running,
        &project_root,
        &["manager", "release", "--project", "cli-register-proj"],
        &[],
    );
    let release_stdout = String::from_utf8_lossy(&release.stdout).to_string();
    assert!(
        release.status.success(),
        "release failed\nstdout={release_stdout}\nstderr={}",
        String::from_utf8_lossy(&release.stderr)
    );
    assert!(
        release_stdout.contains("released manager registration for cli-register-proj"),
        "stdout={release_stdout}"
    );

    let second_release = run_orgasmic(
        &home,
        &running,
        &project_root,
        &["manager", "release", "--project", "cli-register-proj"],
        &[],
    );
    let second_release_stdout = String::from_utf8_lossy(&second_release.stdout).to_string();
    assert!(
        second_release.status.success(),
        "second release should be a no-op, not a failure\nstdout={second_release_stdout}\nstderr={}",
        String::from_utf8_lossy(&second_release.stderr)
    );
    assert!(
        second_release_stdout.contains("no manager registered for cli-register-proj"),
        "stdout={second_release_stdout}"
    );

    let _ = running.shutdown.send(());
    let _ = running.join.await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn register_refusal_exits_nonzero_with_holder_message() {
    let tmp = tempfile::tempdir().unwrap();
    let home = Home::at(tmp.path().join("home"));
    home.ensure().unwrap();
    let project_root = tmp.path().join("proj");
    seed_project(&home, &project_root, "cli-register-refusal");
    let running = boot(home.clone()).await;
    let token = std::fs::read_to_string(home.auth_token()).unwrap();

    let response = reqwest::Client::new()
        .post(format!("http://{}/api/manager/register", running.addr))
        .bearer_auth(token.trim())
        .json(&serde_json::json!({
            "project_id": "cli-register-refusal",
            "pid": u32::MAX,
        }))
        .send()
        .await
        .unwrap();
    assert!(response.status().is_success());

    let refused = run_orgasmic(
        &home,
        &running,
        &project_root,
        &["manager", "register", "--project", "cli-register-refusal"],
        &[],
    );
    let stdout = String::from_utf8_lossy(&refused.stdout);
    let stderr = String::from_utf8_lossy(&refused.stderr);
    assert!(
        !refused.status.success(),
        "refusal must be non-zero\nstdout={stdout}\nstderr={stderr}"
    );
    assert!(
        stderr.contains("already registered externally") && stderr.contains("4294967295"),
        "refusal must name the holder\nstdout={stdout}\nstderr={stderr}"
    );

    let _ = running.shutdown.send(());
    let _ = running.join.await;
}
