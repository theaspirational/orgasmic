//! TASK-FZF2D: closed stdout/mirror must not fail the request that logs to it.
//!
//! Dedicated integration binary so `try_init` installs our sink first.

#![cfg(unix)]

use std::fs::File;
use std::os::unix::io::FromRawFd;

use axum::http::StatusCode;
use axum::routing::get;
use axum::Router;
use orgasmic_daemon::{dropped_log_writes, init_tracing_to, LogMirror, DAEMON_OUT_LOG};

fn closed_pipe_writer() -> File {
    let mut fds = [0; 2];
    assert_eq!(unsafe { libc::pipe(fds.as_mut_ptr()) }, 0);
    unsafe {
        libc::close(fds[0]);
        File::from_raw_fd(fds[1])
    }
}

async fn logged_request() -> (StatusCode, &'static str) {
    // This is deliberately emitted by the successful request handler, rather
    // than by a pre-request probe: it proves a closed log mirror cannot poison
    // the request path that produced a trace line.
    tracing::error!(target: "orgasmic_daemon::logging_sink_test", "closed-mirror request");
    (StatusCode::OK, "request completed")
}

#[tokio::test]
async fn closed_stdout_mirror_drops_request_logs_without_failing_the_request() {
    let tmp = tempfile::tempdir().unwrap();
    let log_path = tmp.path().join(DAEMON_OUT_LOG);
    // Install the sink before starting the test-local request router. The
    // mirror is a pipe whose read end is already closed (EPIPE on every write).
    assert!(
        init_tracing_to(
            "info",
            Some(&log_path),
            LogMirror::Writer(closed_pipe_writer()),
        ),
        "expected to install the best-effort tracing subscriber"
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind test request listener");
    let address = listener.local_addr().unwrap();
    let server = tokio::spawn(async move {
        axum::serve(
            listener,
            Router::new().route("/request", get(logged_request)),
        )
        .await
        .expect("serve test request router");
    });

    let before_request = dropped_log_writes();
    let resp = reqwest::get(format!("http://{address}/request"))
        .await
        .expect("request through closed log mirror");
    assert!(
        resp.status().is_success(),
        "request failed with closed log mirror: {}",
        resp.status()
    );
    let body = resp.text().await.unwrap();
    assert_eq!(body, "request completed");

    let after_request = dropped_log_writes();
    assert!(
        after_request > before_request,
        "the successful request must account for a closed-mirror drop ({before_request} -> {after_request})"
    );

    let log = std::fs::read_to_string(&log_path).unwrap_or_default();
    assert!(
        log.contains("closed-mirror request"),
        "durable log sink empty or unusable at {}: {log:?}",
        log_path.display()
    );

    server.abort();
    let _ = server.await;
}
