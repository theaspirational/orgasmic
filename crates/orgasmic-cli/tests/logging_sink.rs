//! TASK-FZF2D: `orgasmic serve` with a closed stdout pipe must keep serving
//! and keep writing `$ORGASMIC_HOME/logs/daemon.out.log`.

use std::io::{Read, Write};
use std::net::TcpStream;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use orgasmic_core::Home;
use orgasmic_daemon::DAEMON_OUT_LOG;

struct ChildGuard(Child);

impl Drop for ChildGuard {
    fn drop(&mut self) {
        #[cfg(unix)]
        unsafe {
            libc::kill(-(self.0.id() as libc::pid_t), libc::SIGKILL);
        }
        #[cfg(not(unix))]
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

#[test]
fn serve_with_closed_stdout_still_handles_requests_and_writes_log_file() {
    let tmp = tempfile::tempdir().unwrap();
    let home = Home::at(tmp.path().join("home"));
    home.ensure().unwrap();
    let reservation = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let port = reservation.local_addr().unwrap().port();
    drop(reservation);
    std::fs::write(
        home.config(),
        format!("bind_host: 127.0.0.1\nbind_port: {port}\nlog_level: info\n"),
    )
    .unwrap();

    let mut command = Command::new(env!("CARGO_BIN_EXE_orgasmic"));
    command
        .arg("serve")
        .env("ORGASMIC_HOME", &home.root)
        .env_remove("RUST_LOG")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        command.process_group(0);
    }
    let mut child = command.spawn().expect("spawn orgasmic serve");
    let child_stdout = child.stdout.take().expect("child stdout");
    let mut child_stderr = child.stderr.take().expect("child stderr");
    let _child = ChildGuard(child);

    // Mirror the `serve | head` timeline: allow boot/listen output, then close
    // the pipe read end so subsequent request logging hits EPIPE on stdout.
    let deadline = Instant::now() + Duration::from_secs(15);
    while TcpStream::connect(("127.0.0.1", port)).is_err() {
        assert!(Instant::now() < deadline, "daemon did not bind within 15s");
        std::thread::sleep(Duration::from_millis(25));
    }
    // The successful connect establishes the post-bind point in the timeline;
    // dropping the reader then closes the pipe and makes stdout a dead mirror.
    // Do not attempt a false "drain" with an empty Vec (which reads no bytes).
    drop(child_stdout);

    let token = std::fs::read_to_string(home.auth_token())
        .expect("auth token")
        .trim()
        .to_string();
    let mut stream = TcpStream::connect(("127.0.0.1", port)).expect("connect for status");
    stream
        .set_read_timeout(Some(Duration::from_secs(5)))
        .unwrap();
    let req = format!(
        "GET /api/daemon/status HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nAuthorization: Bearer {token}\r\nConnection: close\r\n\r\n"
    );
    stream.write_all(req.as_bytes()).unwrap();
    let mut buf = Vec::new();
    let read_result = stream.read_to_end(&mut buf);
    let response = String::from_utf8_lossy(&buf);
    if read_result.is_err()
        || !(response.contains("HTTP/1.1 200") || response.contains("HTTP/1.0 200"))
    {
        let mut err = String::new();
        let _ = child_stderr.read_to_string(&mut err);
        panic!(
            "status request failed with closed stdout mirror\nread_err={read_result:?}\nresponse:\n{response}\nstderr:\n{err}"
        );
    }
    assert!(
        response.contains("boot_id"),
        "status body missing boot_id:\n{response}"
    );

    let log_path = home.logs().join(DAEMON_OUT_LOG);
    let log = std::fs::read_to_string(&log_path).unwrap_or_default();
    assert!(
        !log.is_empty(),
        "durable daemon log empty at {} after closed-stdout serve",
        log_path.display()
    );
}
