use std::process::Command;

use orgasmic_core::Home;
use orgasmic_daemon::{Daemon, DaemonOptions};

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn ui_print_url_creates_daemon_launch_url() {
    let tmp = tempfile::tempdir().unwrap();
    let home = Home::at(tmp.path().join("home"));
    let running = Daemon::run(home.clone(), test_options())
        .await
        .expect("boot daemon");
    let addr = running.addr;

    let output = Command::new(orgasmic_exe())
        .args(["ui", "--print-url"])
        .env("ORGASMIC_HOME", &home.root)
        .env("ORGASMIC_DAEMON_URL", format!("http://{addr}/"))
        .output()
        .expect("run orgasmic ui --print-url");
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);

    let _ = running.shutdown.send(());
    let _ = running.join.await;

    assert!(
        output.status.success(),
        "ui --print-url failed\nstdout={stdout}\nstderr={stderr}"
    );
    assert!(
        stdout
            .trim()
            .starts_with(&format!("http://{addr}/api/auth/ui-session?ticket=")),
        "unexpected launch URL: {stdout}"
    );
}

fn test_options() -> DaemonOptions {
    DaemonOptions {
        bind_override: Some("127.0.0.1".parse().unwrap()),
        port_override: Some(0),
        ..DaemonOptions::default()
    }
}

fn orgasmic_exe() -> std::path::PathBuf {
    let exe = std::path::PathBuf::from(env!("CARGO_BIN_EXE_orgasmic"));
    if exe.is_absolute() {
        exe
    } else {
        std::env::current_dir().unwrap().join(exe)
    }
}
