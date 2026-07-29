use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use orgasmic_core::Home;

// orgasmic:task_K5NDR
#[path = "common/env_isolation.rs"]
mod env_isolation;
use env_isolation::orgasmic_command;

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

/// Block until `orgasmic serve` has printed its last startup line, which it
/// does immediately before awaiting SIGTERM/Ctrl+C, then keep draining its
/// stdout so the daemon's log mirror cannot fill the pipe and block.
#[cfg(unix)]
fn wait_until_serve_awaits_signals(child: &mut Child) {
    use std::io::{BufRead, BufReader};

    let stdout = child.stdout.take().expect("serve stdout is piped");
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let mut reader = BufReader::new(stdout);
        let mut line = String::new();
        let mut announced = false;
        while reader.read_line(&mut line).unwrap_or(0) > 0 {
            if !announced && line.contains("press Ctrl+C to stop") {
                announced = true;
                let _ = tx.send(());
            }
            line.clear();
        }
    });
    rx.recv_timeout(Duration::from_secs(30))
        .expect("serve never reached its signal wait");
}

#[test]
fn daemon_status_reports_adapter_and_persistence_for_external_target() {
    let tmp = tempfile::tempdir().unwrap();
    let home = Home::at(tmp.path().join("home"));
    home.ensure().unwrap();

    let output = orgasmic_command()
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
    let mut first_command = orgasmic_command();
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

    let output = orgasmic_command()
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

/// orgasmic:TASK-QRB8S — a start that overlaps a departing predecessor.
///
/// TASK-ATAXN lets a replacement wait out a predecessor in graceful shutdown for
/// that predecessor's whole shutdown budget (40s in production) before it can
/// take the instance lock. The CLI's own start fuse stayed a 20s literal, so an
/// overlapping start was reported *failed* at 20s while the daemon went on to
/// come up correctly — a healthy machine and an error message.
///
/// The predecessor here is a stand-in that holds the real instance lock and
/// publishes the real departure marker, which is what `graceful_shutdown` does.
/// It is deliberately not a real drain: what is under test is the CLI's ceiling
/// against production budgets, and the only way to make a spawned `serve` drain
/// for 25s is to shrink the very budgets the fix is derived from. The
/// replacement, the wait, the lock and the CLI command are all real.
#[test]
#[cfg(unix)]
fn autostart_survives_a_predecessor_holding_the_lock_past_the_old_start_literal() {
    use std::io::Write as _;

    /// Longer than the retired 20s literal, shorter than the shutdown budget
    /// the replacement is allowed to wait out.
    const HOLD: Duration = Duration::from_secs(25);
    /// The fuse this test exists to prove is gone.
    const OLD_START_LITERAL: Duration = Duration::from_secs(20);

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

    // A predecessor inside its shutdown: instance lock held, nothing answering
    // on the port, departure marker carrying the budget it says it is spending.
    let mut lock = std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(home.root.join("daemon.lock"))
        .unwrap();
    fs2::FileExt::lock_exclusive(&lock).unwrap();
    lock.set_len(0).unwrap();
    writeln!(lock, "{}", std::process::id()).unwrap();
    lock.sync_data().unwrap();
    let budgets = orgasmic_daemon::ShutdownBudgets::default();
    let marker = orgasmic_daemon::DaemonShutdownMarker {
        pid: std::process::id(),
        boot_id: "predecessor-under-test".to_string(),
        started_at: chrono::Utc::now(),
        budget_ms: budgets.total().as_millis() as u64,
    };
    std::fs::write(
        orgasmic_daemon::daemon_shutdown_marker_path(&home),
        serde_json::to_vec(&marker).unwrap(),
    )
    .unwrap();

    // The replacement, started inside that window: it finds the marker and waits
    // the predecessor out instead of refusing on the held lock.
    let mut replacement = orgasmic_command();
    replacement
        .arg("serve")
        .env("ORGASMIC_HOME", &home.root)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    {
        use std::os::unix::process::CommandExt;
        replacement.process_group(0);
    }
    let _replacement = ChildGuard(replacement.spawn().expect("spawn replacement serve"));

    let release = std::thread::spawn(move || {
        std::thread::sleep(HOLD);
        fs2::FileExt::unlock(&lock).unwrap();
    });

    // `orgasmic status` autostarts, so this is the production CLI wait, not a
    // helper called directly.
    let started = Instant::now();
    let output = orgasmic_command()
        .arg("status")
        .env("ORGASMIC_HOME", &home.root)
        // Keep the operator's real LaunchAgent out of it if this ever decides
        // to start a daemon itself.
        .env("ORGASMIC_TEST_SERVICE_ADAPTER", "detached")
        .output()
        .expect("run orgasmic status");
    let elapsed = started.elapsed();
    release.join().unwrap();
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);

    assert!(
        elapsed > OLD_START_LITERAL,
        "the CLI answered in {elapsed:?}, inside the old {OLD_START_LITERAL:?} fuse \
         — the predecessor was not still holding the lock, so this run did not \
         produce the overlap it is about\nstdout={stdout}\nstderr={stderr}"
    );
    assert!(
        output.status.success(),
        "the CLI reported a failed start after {elapsed:?} for a daemon that took \
         the lock and came up correctly\nstdout={stdout}\nstderr={stderr}"
    );
    assert!(
        stdout.contains("boot_id"),
        "status never reached the replacement daemon: {stdout}"
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

    let mut command = orgasmic_command();
    command
        .arg("serve")
        .env("ORGASMIC_HOME", &home.root)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null());
    {
        use std::os::unix::process::CommandExt;
        command.process_group(0);
    }
    let mut child = command.spawn().expect("spawn serve");
    // orgasmic:TASK-Q07Y5 — wait for the daemon to reach its signal wait, not
    // merely for the port to answer. `serve` registers the SIGTERM handler
    // *after* `Daemon::run` returns, so a SIGTERM sent on the strength of a
    // successful connect can still land on the default disposition and fail
    // this test for a reason it is not about. On an idle machine the window is
    // invisible; under a loaded test binary it is not.
    wait_until_serve_awaits_signals(&mut child);

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

/// Everything below drives the *composed* shutdown/restart path (TASK-Q07Y5).
///
/// TASK-WGXKD.2 finding 2: the tests that shipped with that round exercised the
/// HTTP wait and the SIGTERM routing in isolation — a stand-in server for one,
/// an idle daemon for the other — so both stayed green while the real sequence
/// could be cut off during writer shutdown. These start a real daemon, put a
/// write into its writer that outlasts the drain, and drive the real lifecycle
/// over it.
#[cfg(unix)]
mod stalled_writer {
    use super::*;
    use std::io::Write as _;
    use std::net::TcpStream;

    /// Tx type the writer stall injector matches on. Nothing else in a fresh
    /// home writes it, so ordinary daemon startup writes are untouched.
    const STALL_TX_TYPE: &str = "StallProbe";

    /// A detached daemon this test started, killed on the way out whatever the
    /// outcome. It is not a `Child` — the lifecycle CLI spawns it, not us.
    struct DetachedDaemonGuard(std::path::PathBuf);

    impl Drop for DetachedDaemonGuard {
        fn drop(&mut self) {
            let Some(pid) = std::fs::read_to_string(&self.0)
                .ok()
                .and_then(|raw| raw.trim().parse::<i32>().ok())
            else {
                return;
            };
            unsafe {
                libc::kill(pid, libc::SIGKILL);
            }
        }
    }

    fn seed_home(tmp: &std::path::Path, port: u16) -> Home {
        let home = Home::at(tmp.join("home"));
        home.ensure().unwrap();
        std::fs::write(
            home.config(),
            format!("bind_host: 127.0.0.1\nbind_port: {port}\n"),
        )
        .unwrap();
        home
    }

    fn reserved_port() -> u16 {
        let reservation = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = reservation.local_addr().unwrap().port();
        drop(reservation);
        port
    }

    /// Spawn a real daemon whose writer stalls on `STALL_TX_TYPE`, and return
    /// it only once it is bound AND awaiting signals.
    fn spawn_ready_serve(home: &Home, stall: Duration) -> Child {
        let mut command = orgasmic_command();
        command
            .arg("serve")
            .env("ORGASMIC_HOME", &home.root)
            .env(
                "ORGASMIC_TEST_WRITER_STALL_MS",
                stall.as_millis().to_string(),
            )
            .env("ORGASMIC_TEST_WRITER_STALL_TX_TYPE", STALL_TX_TYPE)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::null());
        {
            use std::os::unix::process::CommandExt;
            command.process_group(0);
        }
        let mut child = command.spawn().expect("spawn serve");
        wait_until_serve_awaits_signals(&mut child);
        child
    }

    fn read_token(home: &Home) -> String {
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            if let Ok(raw) = std::fs::read_to_string(home.auth_token()) {
                if !raw.trim().is_empty() {
                    return raw.trim().to_string();
                }
            }
            assert!(Instant::now() < deadline, "daemon published no auth token");
            std::thread::sleep(Duration::from_millis(25));
        }
    }

    /// Put a write into the daemon's writer that will not finish, and hold the
    /// connection open the way a real client waiting on its terminal tx does.
    /// The response is deliberately never read: the writer is stalled, so there
    /// is no response to read until long after the shutdown under test.
    fn post_stalling_tx(port: u16, token: &str) -> TcpStream {
        let mut socket = TcpStream::connect(("127.0.0.1", port)).expect("connect for stalling tx");
        let body = serde_json::json!({
            "type": STALL_TX_TYPE,
            "reason": "TASK-Q07Y5 in-flight write",
        })
        .to_string();
        write!(
            socket,
            "POST /api/tx HTTP/1.1\r\nHost: 127.0.0.1\r\nAuthorization: Bearer {token}\r\n\
             Content-Type: application/json\r\nContent-Length: {}\r\n\r\n{body}",
            body.len()
        )
        .expect("send stalling tx");
        socket.flush().expect("flush stalling tx");
        // Give the request time to reach the writer and become the head-of-line
        // write before the caller signals the daemon.
        std::thread::sleep(Duration::from_secs(1));
        socket
    }

    fn status_boot_id(port: u16, token: &str) -> Option<String> {
        let output = Command::new("curl")
            .args([
                "-s",
                "--max-time",
                "5",
                "-H",
                &format!("Authorization: Bearer {token}"),
                &format!("http://127.0.0.1:{port}/api/daemon/status"),
            ])
            .output()
            .expect("curl daemon status");
        let body: serde_json::Value = serde_json::from_slice(&output.stdout).ok()?;
        body["boot_id"].as_str().map(|id| id.to_string())
    }

    /// TASK-Q07Y5 finding 1: SIGTERM with a terminal-tx-shaped append stuck in
    /// the writer.
    ///
    /// Before this fix `WriterHandle::shutdown` waited on that append with no
    /// budget, so the launchd `ExitTimeOut` could not be proven to cover the
    /// shutdown and the SIGKILL could land while the append was still
    /// non-durable — with nothing written down about it. The append is *still*
    /// not durable here (a write blocked in the kernel cannot be rescued); what
    /// must hold is that the daemon stops waiting on its own budget and leaves a
    /// durable record before the service manager's kill window closes.
    #[test]
    fn sigterm_bounds_a_stuck_write_and_records_the_loss_before_the_kill_window() {
        use std::os::unix::process::ExitStatusExt as _;

        let budgets = orgasmic_daemon::ShutdownBudgets::default();
        // Longer than the connection drain plus the writer budget, so the
        // writer is genuinely still stuck when its budget expires.
        let stall = budgets.total() + Duration::from_secs(10);

        let tmp = tempfile::tempdir().unwrap();
        let port = reserved_port();
        let home = seed_home(tmp.path(), port);
        let mut child = ChildGuard(spawn_ready_serve(&home, stall));
        let token = read_token(&home);
        let _held_connection = post_stalling_tx(port, &token);

        let signalled = Instant::now();
        unsafe {
            assert_eq!(
                libc::kill(child.0.id() as libc::pid_t, libc::SIGTERM),
                0,
                "SIGTERM failed: {}",
                std::io::Error::last_os_error()
            );
        }

        // The acceptance claim: the record is durable BEFORE the watchdog could
        // kill the daemon, i.e. inside the same budget the plist's ExitTimeOut
        // is derived from.
        let loss_dir = orgasmic_daemon::shutdown_loss_dir(&home);
        let record_deadline = signalled + budgets.total() + Duration::from_secs(5);
        let record = loop {
            let found = std::fs::read_dir(&loss_dir)
                .ok()
                .and_then(|entries| entries.flatten().next().map(|entry| entry.path()));
            if let Some(path) = found {
                break path;
            }
            assert!(
                Instant::now() < record_deadline,
                "no shutdown loss record in {} within the shutdown budget {:?}; \
                 the writer shutdown is unbounded again",
                loss_dir.display(),
                budgets.total()
            );
            std::thread::sleep(Duration::from_millis(100));
        };
        let recorded_after = signalled.elapsed();

        let record: serde_json::Value =
            serde_json::from_slice(&std::fs::read(&record).unwrap()).unwrap();
        assert_eq!(
            record["writer_shutdown"]["outcome"], "timed_out",
            "record: {record}"
        );
        assert_eq!(
            record["writer_shutdown"]["in_flight"]["tx_type"], STALL_TX_TYPE,
            "the record must name the write that was not durable: {record}"
        );
        assert!(
            recorded_after >= budgets.writer_shutdown,
            "the daemon gave up before spending its writer budget: {recorded_after:?}"
        );

        // And it still exits through the graceful path rather than being
        // signalled, once the stuck write finally returns.
        let exit_deadline = Instant::now() + stall + Duration::from_secs(20);
        let status = loop {
            match child.0.try_wait().expect("wait on serve") {
                Some(status) => break status,
                None => {
                    assert!(
                        Instant::now() < exit_deadline,
                        "serve never exited after its shutdown budget"
                    );
                    std::thread::sleep(Duration::from_millis(100));
                }
            }
        };
        assert_eq!(status.signal(), None, "serve was signalled, not graceful");
        assert_eq!(status.code(), Some(0), "exit status {status:?}");
    }

    /// TASK-Q07Y5 finding 2: the real `orgasmic daemon restart`, over a write
    /// that outlasts five seconds, including the service stop and the
    /// replacement bind.
    ///
    /// The stand-in-server test this replaces stopped after parsing the drain
    /// response, so it could not see the sequence continue into the stop.
    /// `ORGASMIC_TEST_SERVICE_ADAPTER=detached` keeps the real LaunchAgent (a
    /// fixed label, shared with the operator's own daemon) out of it.
    #[test]
    fn daemon_restart_survives_a_write_that_outlasts_the_drain_barrier() {
        let tmp = tempfile::tempdir().unwrap();
        let port = reserved_port();
        let home = seed_home(tmp.path(), port);
        // Longer than the endpoint's 5s writer drain barrier, shorter than the
        // shutdown budget: the drain must report the pending write and the stop
        // must still complete cleanly.
        let stall = Duration::from_secs(8);
        let mut child = ChildGuard(spawn_ready_serve(&home, stall));
        let token = read_token(&home);
        let original_boot_id = status_boot_id(port, &token).expect("incumbent boot id");
        let _held_connection = post_stalling_tx(port, &token);
        let _replacement = DetachedDaemonGuard(home.state().join("daemon.pid"));

        let started = Instant::now();
        let output = orgasmic_command()
            .args(["daemon", "restart"])
            .env("ORGASMIC_HOME", &home.root)
            .env("ORGASMIC_TEST_SERVICE_ADAPTER", "detached")
            .env(
                "ORGASMIC_TEST_WRITER_STALL_MS",
                stall.as_millis().to_string(),
            )
            .env("ORGASMIC_TEST_WRITER_STALL_TX_TYPE", STALL_TX_TYPE)
            .output()
            .expect("run orgasmic daemon restart");
        let elapsed = started.elapsed();
        let stdout = String::from_utf8_lossy(&output.stdout);
        let stderr = String::from_utf8_lossy(&output.stderr);

        assert!(
            output.status.success(),
            "daemon restart failed\nstdout={stdout}\nstderr={stderr}"
        );
        assert!(
            elapsed >= Duration::from_secs(5),
            "the restart did not wait for the in-flight write: {elapsed:?}"
        );
        assert!(
            stderr.contains("writer drain timed out"),
            "the drain must report the write it could not confirm\nstderr={stderr}"
        );

        // The incumbent really stopped, gracefully, through the signal path.
        let status = child.0.try_wait().expect("wait on incumbent");
        let status = match status {
            Some(status) => status,
            None => {
                let deadline = Instant::now() + Duration::from_secs(20);
                loop {
                    match child.0.try_wait().expect("wait on incumbent") {
                        Some(status) => break status,
                        None => {
                            assert!(Instant::now() < deadline, "incumbent daemon never exited");
                            std::thread::sleep(Duration::from_millis(100));
                        }
                    }
                }
            }
        };
        assert_eq!(status.code(), Some(0), "incumbent exit: {status:?}");

        // And the replacement bound the same port under a new boot.
        let replacement_boot_id = status_boot_id(port, &token).expect("replacement boot id");
        assert_ne!(
            replacement_boot_id, original_boot_id,
            "the port is still served by the old boot; no replacement bind happened"
        );
    }
}
