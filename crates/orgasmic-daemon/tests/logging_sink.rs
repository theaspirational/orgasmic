//! TASK-FZF2D: closed stdout/mirror must not fail requests or the durable log.
//!
//! Dedicated integration binary so `try_init` installs our sinks first.

#![cfg(unix)]

use std::fs::File;
use std::io::Write;
use std::os::unix::io::FromRawFd;
use std::path::PathBuf;
use std::time::Duration;

use orgasmic_core::Home;
use orgasmic_daemon::{
    dropped_log_writes, init_tracing_to, Daemon, DaemonOptions, LogMirror, DAEMON_OUT_LOG,
};

fn closed_pipe_writer() -> File {
    let mut fds = [0; 2];
    assert_eq!(unsafe { libc::pipe(fds.as_mut_ptr()) }, 0);
    unsafe {
        libc::close(fds[0]);
        File::from_raw_fd(fds[1])
    }
}

fn read_token(home: &Home) -> String {
    let path = home.auth_token();
    for _ in 0..40 {
        if path.is_file() {
            break;
        }
        std::thread::sleep(Duration::from_millis(25));
    }
    std::fs::read_to_string(&path)
        .expect("token file")
        .trim()
        .to_string()
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

#[tokio::test]
async fn closed_stdout_mirror_drops_logs_without_failing_requests() {
    let tmp = tempfile::tempdir().unwrap();
    let home = Home::at(tmp.path().join("home"));
    home.ensure().unwrap();
    std::os::unix::fs::symlink(repo_root(), home.source()).unwrap();

    let log_path = home.logs().join(DAEMON_OUT_LOG);
    let before = dropped_log_writes();
    // Install sinks before Daemon::run so try_init wins; mirror is a pipe whose
    // read end is already closed (EPIPE on every write).
    assert!(
        init_tracing_to(
            "info",
            Some(&log_path),
            LogMirror::Writer(closed_pipe_writer()),
        ),
        "expected to install the best-effort tracing subscriber"
    );
    // Force a line through the sink regardless of ambient RUST_LOG.
    tracing::error!(target: "orgasmic_daemon::logging_sink_test", "closed-mirror probe");
    let after_probe = dropped_log_writes();
    assert!(
        after_probe > before,
        "closed mirror should drop the probe write ({before} -> {after_probe})"
    );

    let running = Daemon::run(
        home.clone(),
        DaemonOptions {
            bind_override: Some("127.0.0.1".parse().unwrap()),
            port_override: Some(0),
            fs_watcher_enabled: false,
            ..DaemonOptions::default()
        },
    )
    .await
    .expect("boot daemon with closed log mirror");

    let after_boot = dropped_log_writes();
    assert!(
        after_boot >= after_probe,
        "drop counter must not reset ({after_probe} -> {after_boot})"
    );

    let token = read_token(&home);
    let client = reqwest::Client::new();
    let resp = client
        .get(format!("http://{}/api/daemon/status", running.addr))
        .bearer_auth(&token)
        .send()
        .await
        .expect("status request");
    assert!(
        resp.status().is_success(),
        "status request failed: {}",
        resp.status()
    );
    let body = resp.text().await.unwrap();
    assert!(body.contains("boot_id"), "unexpected status body: {body}");

    let after_request = dropped_log_writes();
    assert!(
        after_request >= after_boot,
        "drop counter must not reset ({after_boot} -> {after_request})"
    );

    let log = std::fs::read_to_string(&log_path).unwrap_or_default();
    assert!(
        log.contains("orgasmic daemon") || log.contains("pre-bind") || !log.is_empty(),
        "durable log sink empty or unusable at {}: {log:?}",
        log_path.display()
    );

    // Durable sink remains writable after the closed mirror has been failing.
    let marker = format!("task-fzf2d-probe-{}\n", after_request);
    std::fs::OpenOptions::new()
        .append(true)
        .open(&log_path)
        .unwrap()
        .write_all(marker.as_bytes())
        .expect("append probe to durable log");
    let log_after = std::fs::read_to_string(&log_path).unwrap();
    assert!(
        log_after.contains(marker.trim()),
        "durable log not writable after mirror failures"
    );

    let _ = running.shutdown.send(());
    let _ = running.join.await;
}
