use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use orgasmic_core::Home;

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

#[test]
fn second_serve_exits_zero_when_healthy_incumbent_owns_home_lock() {
    let tmp = tempfile::tempdir().unwrap();
    let home = Home::at(tmp.path().join("home"));
    home.ensure().unwrap();
    let reservation = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let port = reservation.local_addr().unwrap().port();
    drop(reservation);
    std::fs::write(
        home.config(),
        format!("bind_host: 127.0.0.1\nbind_port: {port}\n"),
    )
    .unwrap();
    let mut first_command = Command::new(env!("CARGO_BIN_EXE_orgasmic"));
    first_command
        .arg("serve")
        .env("ORGASMIC_HOME", &home.root)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        first_command.process_group(0);
    }
    let first = first_command.spawn().expect("spawn incumbent serve");
    let _first = ChildGuard(first);
    let deadline = Instant::now() + Duration::from_secs(5);
    while std::net::TcpStream::connect(("127.0.0.1", port)).is_err() {
        assert!(
            Instant::now() < deadline,
            "incumbent daemon did not bind within 5s"
        );
        std::thread::sleep(Duration::from_millis(25));
    }

    let output = Command::new(env!("CARGO_BIN_EXE_orgasmic"))
        .arg("serve")
        .env("ORGASMIC_HOME", &home.root)
        .output()
        .expect("run second serve");
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);

    assert!(
        output.status.success(),
        "second serve failed\nstdout={stdout}\nstderr={stderr}"
    );
    assert!(
        stdout.contains("daemon already running"),
        "missing already-running confirmation: {stdout}"
    );
}
