use std::process::Command;

use orgasmic_core::Home;

#[test]
fn daemon_status_reports_adapter_and_persistence_for_external_target() {
    let tmp = tempfile::tempdir().unwrap();
    let home = Home::at(tmp.path().join("home"));
    home.ensure().unwrap();

    let output = Command::new(env!("CARGO_BIN_EXE_orgasmic"))
        .args(["daemon", "status"])
        .env("ORGASMIC_HOME", &home.root)
        .env("ORGASMIC_DAEMON_URL", "http://127.0.0.1:9")
        .output()
        .expect("run orgasmic daemon status");
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);

    assert!(
        output.status.success(),
        "daemon status failed\nstdout={stdout}\nstderr={stderr}"
    );
    assert!(stdout.contains("stopped"));
    assert!(stdout.contains("adapter: external-url"));
    assert!(stdout.contains("persistence: installed=no enabled=no"));
    assert!(stdout.contains("local daemon lifecycle is externally owned"));
}
