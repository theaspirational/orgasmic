use std::process::Command;
use std::time::Duration;

use orgasmic_core::Home;
use orgasmic_daemon::Daemon;

mod common;

use common::{init_git_repo, orgasmic_exe, run_git, seed_required_shipped, test_options, write};

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[cfg(unix)]
async fn doctor_warns_when_running_daemon_predates_binary_mtime() {
    let tmp = tempfile::tempdir().unwrap();
    let home = Home::at(tmp.path().join("home"));
    let running = Daemon::run(home.clone(), test_options())
        .await
        .expect("boot daemon");

    tokio::time::sleep(Duration::from_millis(1_100)).await;
    let stub = tmp.path().join("orgasmic-stub");
    write(&stub, "#!/bin/sh\nexit 0\n");
    std::os::unix::fs::symlink(&stub, home.bin_orgasmic()).unwrap();

    let output = Command::new(orgasmic_exe())
        .arg("doctor")
        .env("ORGASMIC_HOME", &home.root)
        .env("ORGASMIC_DAEMON_URL", format!("http://{}", running.addr))
        .output()
        .expect("run orgasmic doctor");
    assert!(
        output.status.success(),
        "doctor failed\nstdout={}\nstderr={}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8_lossy(&output.stdout);

    let _ = running.shutdown.send(());
    let _ = running.join.await;

    assert!(stdout.contains("[warn] running daemon predates"));
    assert!(stdout.contains("daemon uptime:"));
    assert!(stdout.contains("binary built:"));
    assert!(stdout.contains("0 daemon-code commits since boot"));
    assert!(stdout.contains("restart recommended (orgasmic restart)"));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[cfg(unix)]
async fn doctor_warns_for_git_commits_since_daemon_boot() {
    let tmp = tempfile::tempdir().unwrap();
    let home = Home::at(tmp.path().join("home"));
    home.ensure().unwrap();

    let source = home.source();
    init_git_repo(&source);
    seed_required_shipped(&source);
    run_git(&source, &["add", "."]);
    run_git(&source, &["commit", "-m", "seed shipped fixtures"]);

    let stub = tmp.path().join("orgasmic-stub");
    write(&stub, "#!/bin/sh\nexit 0\n");
    std::os::unix::fs::symlink(&stub, home.bin_orgasmic()).unwrap();

    let running = Daemon::run(home.clone(), test_options())
        .await
        .expect("boot daemon");

    tokio::time::sleep(Duration::from_millis(1_100)).await;
    write(
        &source.join("crates/orgasmic-daemon/foo.rs"),
        "pub fn git_path_route() {}\n",
    );
    run_git(&source, &["add", "."]);
    run_git(&source, &["commit", "-m", "TASK-052 git path route"]);
    let sha = run_git(&source, &["rev-parse", "--short", "HEAD"]);

    let output = Command::new(orgasmic_exe())
        .arg("doctor")
        .env("ORGASMIC_HOME", &home.root)
        .env("ORGASMIC_DAEMON_URL", format!("http://{}", running.addr))
        .output()
        .expect("run orgasmic doctor");
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);

    let _ = running.shutdown.send(());
    let _ = running.join.await;

    assert!(
        output.status.success(),
        "doctor failed\nstdout={stdout}\nstderr={stderr}"
    );
    assert!(stdout.contains("[warn] running daemon predates"));
    assert!(stdout.contains("1 daemon-code commit since boot"));
    assert!(stdout.contains(&sha));
    assert!(stdout.contains("TASK-052 git path route"));
    assert!(!stdout.contains("0 daemon-code commits since boot"));
}
