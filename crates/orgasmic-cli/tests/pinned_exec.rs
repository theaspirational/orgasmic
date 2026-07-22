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
fn retained_wrapper_alias_runs_real_verifier_after_public_replacement() {
    let tmp = tempfile::tempdir().unwrap();
    let public_wrapper = tmp.path().join("orgasmic");
    std::fs::copy(orgasmic(), &public_wrapper).unwrap();
    std::fs::set_permissions(&public_wrapper, std::fs::Permissions::from_mode(0o755)).unwrap();
    let aliases = tmp.path().join("trusted-exec-wrappers");
    std::fs::create_dir(&aliases).unwrap();
    std::fs::set_permissions(&aliases, std::fs::Permissions::from_mode(0o700)).unwrap();
    let retained_wrapper = aliases.join("orgasmic-retained");
    std::fs::hard_link(&public_wrapper, &retained_wrapper).unwrap();

    let target = tmp.path().join("2.1.217");
    let trusted_log = tmp.path().join("trusted.log");
    let malicious_log = tmp.path().join("malicious.log");
    executable(
        &target,
        "#!/bin/sh\necho \"trusted $*\" > \"$TRUSTED_LOG\"\n",
    );
    let target_meta = std::fs::metadata(&target).unwrap();
    std::fs::rename(&public_wrapper, public_wrapper.with_extension("opened")).unwrap();
    executable(
        &public_wrapper,
        "#!/bin/sh\necho malicious > \"$MALICIOUS_LOG\"\n",
    );

    let status = Command::new(&retained_wrapper)
        .args([
            "__exec-pinned",
            target.to_str().unwrap(),
            &target_meta.dev().to_string(),
            &target_meta.ino().to_string(),
            "--",
            "--resume",
            "session-wrapper-pin",
        ])
        .env("TRUSTED_LOG", &trusted_log)
        .env("MALICIOUS_LOG", &malicious_log)
        .status()
        .unwrap();
    assert!(status.success());
    assert_eq!(
        std::fs::read_to_string(&trusted_log).unwrap().trim(),
        "trusted --resume session-wrapper-pin"
    );
    assert!(
        !malicious_log.exists(),
        "public wrapper replacement must not cross retained wrapper identity"
    );
}

#[test]
fn pinned_exec_scavenges_private_alias_after_parent_sigkill() {
    let tmp = tempfile::tempdir().unwrap();
    let target = tmp.path().join("2.1.217");
    let cleaner = tmp.path().join("2.1.218");
    let child_pid = tmp.path().join("child.pid");
    let gate = tmp.path().join("release-child");
    executable(
        &target,
        "#!/bin/sh\necho $$ > \"$CHILD_PID\"\nwhile [ ! -f \"$GATE\" ]; do sleep 0.01; done\n",
    );
    executable(&cleaner, "#!/bin/sh\nexit 0\n");
    let metadata = std::fs::metadata(&target).unwrap();
    let mut parent = Command::new(orgasmic())
        .args([
            "__exec-pinned",
            target.to_str().unwrap(),
            &metadata.dev().to_string(),
            &metadata.ino().to_string(),
            "--",
        ])
        .env("CHILD_PID", &child_pid)
        .env("GATE", &gate)
        .spawn()
        .unwrap();
    wait_for(&child_pid);
    let alias = std::fs::read_dir(tmp.path())
        .unwrap()
        .flatten()
        .map(|entry| entry.path())
        .find(|path| {
            path.file_name()
                .and_then(|name| name.to_str())
                .is_some_and(|name| name.starts_with(".orgasmic-exec-"))
        })
        .expect("private alias exists while wrapper is live");

    unsafe { libc::kill(parent.id() as libc::pid_t, libc::SIGKILL) };
    let _ = parent.wait();
    let cleaner_meta = std::fs::metadata(&cleaner).unwrap();
    let live_cleanup = Command::new(orgasmic())
        .args([
            "__exec-pinned",
            cleaner.to_str().unwrap(),
            &cleaner_meta.dev().to_string(),
            &cleaner_meta.ino().to_string(),
            "--",
        ])
        .status()
        .unwrap();
    assert!(live_cleanup.success());
    assert!(alias.exists(), "live orphan child lease must protect alias");

    std::fs::write(&gate, "go").unwrap();
    let pid: libc::pid_t = std::fs::read_to_string(&child_pid)
        .unwrap()
        .trim()
        .parse()
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(10);
    while unsafe { libc::kill(pid, 0) } == 0 {
        assert!(Instant::now() < deadline, "orphan child did not exit");
        std::thread::sleep(Duration::from_millis(10));
    }

    let stale_cleanup = Command::new(orgasmic())
        .args([
            "__exec-pinned",
            cleaner.to_str().unwrap(),
            &cleaner_meta.dev().to_string(),
            &cleaner_meta.ino().to_string(),
            "--",
        ])
        .status()
        .unwrap();
    assert!(stale_cleanup.success());
    assert!(!alias.exists(), "stale SIGKILL alias must be scavenged");
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
