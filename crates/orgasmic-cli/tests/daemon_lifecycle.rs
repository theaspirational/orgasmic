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

/// TASK-WGXKD.2 finding 2: a service stop must run the graceful shutdown.
///
/// `orgasmic serve` used to await `ctrl_c()` and nothing else, so SIGTERM — what
/// `launchctl kickstart -k`, `launchctl bootout` and systemd all send — killed
/// the daemon on the default disposition. `RunningDaemon.shutdown` never fired,
/// so neither the release-finalization drain nor the writer shutdown ran: the
/// TASK-WGXKD.1 graceful shutdown was unreachable on the only path this machine
/// uses.
///
/// The discriminator is the exit status itself. Default disposition means the
/// process is *signalled*; a handled SIGTERM means it *exits*, which it can only
/// do after `running.join` — i.e. after the drain.
#[test]
#[cfg(unix)]
fn sigterm_exits_through_graceful_shutdown_rather_than_default_disposition() {
    use std::os::unix::process::ExitStatusExt as _;

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

    let mut command = Command::new(env!("CARGO_BIN_EXE_orgasmic"));
    command
        .arg("serve")
        .env("ORGASMIC_HOME", &home.root)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    {
        use std::os::unix::process::CommandExt;
        command.process_group(0);
    }
    let mut child = command.spawn().expect("spawn serve");
    let deadline = Instant::now() + Duration::from_secs(10);
    while std::net::TcpStream::connect(("127.0.0.1", port)).is_err() {
        assert!(Instant::now() < deadline, "daemon did not bind within 10s");
        std::thread::sleep(Duration::from_millis(25));
    }

    unsafe {
        assert_eq!(
            libc::kill(child.id() as libc::pid_t, libc::SIGTERM),
            0,
            "SIGTERM failed: {}",
            std::io::Error::last_os_error()
        );
    }

    // Poll rather than block forever: a daemon that ignored SIGTERM entirely
    // must fail this test, not hang the suite.
    let deadline = Instant::now() + Duration::from_secs(30);
    let status = loop {
        match child.try_wait().expect("wait on serve") {
            Some(status) => break status,
            None => {
                assert!(
                    Instant::now() < deadline,
                    "serve did not exit within 30s of SIGTERM"
                );
                std::thread::sleep(Duration::from_millis(50));
            }
        }
    };

    assert_eq!(
        status.signal(),
        None,
        "serve died on SIGTERM's default disposition, so the graceful shutdown \
         (release-finalization drain, writer shutdown) never ran"
    );
    assert_eq!(
        status.code(),
        Some(0),
        "graceful shutdown must exit cleanly: {status:?}"
    );
}
