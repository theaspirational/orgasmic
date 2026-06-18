use std::process::{Command, Output};
use std::time::Duration;

use orgasmic_core::Home;
use orgasmic_daemon::Daemon;

mod common;

use common::{
    init_git_repo, orgasmic_exe, run_git, seed_required_shipped, test_options, unused_port, write,
    write_config_port,
};

fn run_status(home: &Home, daemon_url: Option<String>) -> Output {
    let mut command = Command::new(orgasmic_exe());
    command.arg("status").env("ORGASMIC_HOME", &home.root);
    if let Some(url) = daemon_url {
        command.env("ORGASMIC_DAEMON_URL", url);
    } else {
        command.env_remove("ORGASMIC_DAEMON_URL");
    }
    command.output().expect("run orgasmic status")
}

fn seed_source_repo(home: &Home) {
    let source = home.source();
    init_git_repo(&source);
    seed_required_shipped(&source);
    run_git(&source, &["add", "."]);
    run_git(&source, &["commit", "-m", "seed shipped fixtures"]);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[cfg(unix)]
async fn status_with_stale_binary_emits_warn_on_stderr() {
    let tmp = tempfile::tempdir().unwrap();
    let home = Home::at(tmp.path().join("home"));
    home.ensure().unwrap();
    seed_source_repo(&home);

    let stub = tmp.path().join("orgasmic-stub");
    write(&stub, "#!/bin/sh\nexit 0\n");
    std::os::unix::fs::symlink(&stub, home.bin_orgasmic()).unwrap();

    let running = Daemon::run(home.clone(), test_options())
        .await
        .expect("boot daemon");

    tokio::time::sleep(Duration::from_millis(1_100)).await;
    let source = home.source();
    write(
        &source.join("crates/orgasmic-daemon/status_stale.rs"),
        "pub fn status_stale_route() {}\n",
    );
    run_git(&source, &["add", "."]);
    run_git(&source, &["commit", "-m", "TASK-084 status stale route"]);
    let sha = run_git(&source, &["rev-parse", "--short", "HEAD"]);

    let output = run_status(&home, Some(format!("http://{}", running.addr)));
    let stdout = String::from_utf8_lossy(&output.stdout).to_string();
    let stderr = String::from_utf8_lossy(&output.stderr).to_string();

    let _ = running.shutdown.send(());
    let _ = running.join.await;

    assert!(
        output.status.success(),
        "status failed\nstdout={stdout}\nstderr={stderr}"
    );
    serde_json::from_str::<serde_json::Value>(&stdout).expect("status stdout is JSON");
    assert!(!stdout.contains("[warn] running daemon predates"));
    assert!(stderr.contains("[warn] running daemon predates"));
    assert!(stderr.contains("1 daemon-code commit since boot"));
    assert!(stderr.contains(&sha));
    assert!(stderr.contains("TASK-084 status stale route"));
    assert!(stderr.contains("restart recommended (orgasmic restart)"));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[cfg(unix)]
async fn status_with_fresh_binary_emits_no_warn() {
    let tmp = tempfile::tempdir().unwrap();
    let home = Home::at(tmp.path().join("home"));
    home.ensure().unwrap();
    seed_source_repo(&home);

    let source = home.source();
    write(
        &source.join("crates/orgasmic-daemon/status_fresh.rs"),
        "pub fn status_fresh_route() {}\n",
    );
    run_git(&source, &["add", "."]);
    run_git(&source, &["commit", "-m", "TASK-084 status fresh route"]);

    let stub = tmp.path().join("orgasmic-stub");
    write(&stub, "#!/bin/sh\nexit 0\n");
    std::os::unix::fs::symlink(&stub, home.bin_orgasmic()).unwrap();
    tokio::time::sleep(Duration::from_millis(1_100)).await;

    let running = Daemon::run(home.clone(), test_options())
        .await
        .expect("boot daemon");

    let output = run_status(&home, Some(format!("http://{}", running.addr)));
    let stdout = String::from_utf8_lossy(&output.stdout).to_string();
    let stderr = String::from_utf8_lossy(&output.stderr).to_string();

    let _ = running.shutdown.send(());
    let _ = running.join.await;

    assert!(
        output.status.success(),
        "status failed\nstdout={stdout}\nstderr={stderr}"
    );
    serde_json::from_str::<serde_json::Value>(&stdout).expect("status stdout is JSON");
    assert!(!stdout.contains("running daemon predates"));
    assert!(!stderr.contains("running daemon predates"));
}

#[test]
fn status_with_daemon_down_emits_no_staleness_warn() {
    let tmp = tempfile::tempdir().unwrap();
    let home = Home::at(tmp.path().join("home"));
    home.ensure().unwrap();
    seed_source_repo(&home);
    write_config_port(&home, unused_port());
    write(&home.auth_token(), "test-token\n");

    let output = run_status(&home, Some(format!("http://127.0.0.1:{}", unused_port())));
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);

    assert!(!output.status.success(), "daemon-down status should fail");
    assert!(!stdout.contains("running daemon predates"));
    assert!(!stderr.contains("running daemon predates"));
    assert!(!stdout.contains("[warn]"));
    assert!(!stderr.contains("[warn]"));
}
