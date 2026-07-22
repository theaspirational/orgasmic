#![cfg(unix)]

use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::Path;
use std::process::Command;
use std::time::{Duration, Instant};

fn orgasmic() -> &'static str {
    env!("CARGO_BIN_EXE_orgasmic")
}

fn executable(path: &Path, body: &str) {
    std::fs::write(path, body).unwrap();
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755)).unwrap();
}

fn wait_for(path: &Path) {
    let deadline = Instant::now() + Duration::from_secs(10);
    while !path.exists() {
        assert!(
            Instant::now() < deadline,
            "timed out waiting for {}",
            path.display()
        );
        std::thread::sleep(Duration::from_millis(10));
    }
}

#[test]
fn pinned_exec_binds_execution_to_open_file_identity() {
    let tmp = tempfile::tempdir().unwrap();
    let target = tmp.path().join("2.1.217");
    let displaced = tmp.path().join("2.1.217.opened");
    let trusted_log = tmp.path().join("trusted.log");
    let malicious_log = tmp.path().join("malicious.log");
    let gate = tmp.path().join("continue");
    executable(
        &target,
        "#!/bin/sh\necho trusted-start > \"$TRUSTED_LOG\"\nwhile [ ! -f \"$GATE\" ]; do sleep 0.01; done\necho \"trusted-complete $*\" >> \"$TRUSTED_LOG\"\n",
    );
    let metadata = std::fs::metadata(&target).unwrap();
    let mut child = Command::new(orgasmic())
        .args([
            "__exec-pinned",
            target.to_str().unwrap(),
            &metadata.dev().to_string(),
            &metadata.ino().to_string(),
            "--",
            "--resume",
            "session-1",
        ])
        .env("TRUSTED_LOG", &trusted_log)
        .env("MALICIOUS_LOG", &malicious_log)
        .env("GATE", &gate)
        .spawn()
        .unwrap();

    wait_for(&trusted_log);
    std::fs::rename(&target, &displaced).unwrap();
    executable(&target, "#!/bin/sh\necho malicious > \"$MALICIOUS_LOG\"\n");
    std::fs::write(&gate, "go").unwrap();
    let status = child.wait().unwrap();
    assert!(status.success());
    let trusted = std::fs::read_to_string(&trusted_log).unwrap();
    assert!(
        trusted.contains("trusted-complete --resume session-1"),
        "{trusted}"
    );
    assert!(
        !malicious_log.exists(),
        "replacement path won after retained-handle exec"
    );
}

#[test]
fn pinned_exec_rejects_identity_changed_before_open() {
    let tmp = tempfile::tempdir().unwrap();
    let target = tmp.path().join("2.1.217");
    let malicious_log = tmp.path().join("malicious.log");
    executable(&target, "#!/bin/sh\nexit 0\n");
    let metadata = std::fs::metadata(&target).unwrap();
    std::fs::remove_file(&target).unwrap();
    executable(&target, "#!/bin/sh\necho malicious > \"$MALICIOUS_LOG\"\n");

    let output = Command::new(orgasmic())
        .args([
            "__exec-pinned",
            target.to_str().unwrap(),
            &metadata.dev().to_string(),
            &metadata.ino().to_string(),
            "--",
        ])
        .env("MALICIOUS_LOG", &malicious_log)
        .output()
        .unwrap();
    assert!(!output.status.success());
    assert!(
        String::from_utf8_lossy(&output.stderr).contains("identity mismatch"),
        "stderr={}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(!malicious_log.exists());
}

#[test]
fn run_recover_requires_project_at_cli_boundary() {
    let output = Command::new(orgasmic())
        .args(["run", "recover", "run-origin"])
        .output()
        .unwrap();
    assert!(!output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("--project <PROJECT>"), "{stderr}");
    assert!(stderr.contains("required"), "{stderr}");
}
