//! A project the daemon cannot read must not stop it from binding.
//!
//! Reproduced on 2026-07-25 after a runtime binary swap invalidated the macOS
//! TCC grant for `~/Documents`: boot logged its pre-bind message, then nothing.
//! The process sat at 0% CPU with no project files open, never reached
//! `listening`, and launchd respawned replacements that each refused on the
//! instance lock. Every CLI verb, the UI and the app were dead, and the only
//! signal was a WARN in a log file nobody was watching.
//!
//! A stack sample placed it in `reattach_live_runs_on_boot` ->
//! `read_session_file` -> `read_to_string`, on the main thread inside
//! `block_on` — which is also why the boot-progress heartbeat froze and
//! `orgasmic status` simply hung.
//!
//! The permission that failed there is not reproducible in a test, but the
//! *shape* is: an `open()` that blocks before a descriptor exists. A FIFO does
//! exactly that, deterministically and without touching TCC. TASK-KKGKM.

use std::net::SocketAddr;
use std::path::Path;
use std::time::Duration;

use orgasmic_core::Home;
use orgasmic_daemon::{Daemon, DaemonOptions};
use orgasmic_drivers::test_tooling::{
    assert_required_test_tooling, skip_test_if_missing, ToolRequirement,
};

/// Generous next to a healthy boot (~50 ms measured) and still decisive: the
/// pre-fix daemon did not bind in minutes, so this only fails on a real hang.
const BOOT_DEADLINE: Duration = Duration::from_secs(20);

fn unix_permission_denial_available_for_test() -> bool {
    use std::os::unix::fs::PermissionsExt;

    let tmp = tempfile::tempdir().unwrap();
    let locked = tmp.path().join("locked");
    std::fs::create_dir(&locked).unwrap();
    std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o000)).unwrap();
    let denied = std::fs::read_dir(&locked).is_err();
    std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o755)).unwrap();
    denied
}

#[test]
fn required_test_tooling_is_present() {
    assert_required_test_tooling(&[ToolRequirement::new(
        "unix-permissions",
        1,
        unix_permission_denial_available_for_test(),
    )]);
}

fn test_options() -> DaemonOptions {
    DaemonOptions {
        bind_override: Some("127.0.0.1".parse().unwrap()),
        port_override: Some(0),
        fs_watcher_enabled: false,
        ..DaemonOptions::default()
    }
}

fn write(path: &Path, contents: impl AsRef<str>) {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).unwrap();
    }
    std::fs::write(path, contents.as_ref()).unwrap();
}

fn seed_project(_home: &Home, project_root: &Path, project_id: &str, board: &mut String) {
    write(
        &project_root.join(".orgasmic/project.org"),
        format!(
            "#+title: {project_id}\n#+orgasmic_version: 1\n\n* PROJECT {project_id}\n:PROPERTIES:\n:ID:               {project_id}\n:END:\n"
        ),
    );
    board.push_str(&format!(
        "* PROJECT {project_id}\n:PROPERTIES:\n:ID:               {project_id}\n:PATH:             {}\n:BRANCH:           main\n:STATUS:           active\n:END:\n\n",
        project_root.display()
    ));
}

/// Boot a daemon on its own thread and runtime, returning its address once it
/// is listening, or `None` if it did not get there within [`BOOT_DEADLINE`].
///
/// Deliberately not `tokio::time::timeout`. The failure being guarded against
/// is a *synchronous* block inside an async fn, which stalls the task mid-poll:
/// the timer fires but the task can never be woken to observe it, so the
/// timeout never returns and the test hangs instead of failing. Verified by
/// reverting the fix — the timeout-based version ran until it was killed. An
/// owned thread keeps the wedge on that thread, so the deadline is real and a
/// regression reports itself.
fn boot_within_deadline(home: Home) -> Option<(SocketAddr, std::thread::JoinHandle<()>)> {
    let (ready_tx, ready_rx) = std::sync::mpsc::channel();
    let (stop_tx, stop_rx) = std::sync::mpsc::channel::<()>();
    let handle = std::thread::spawn(move || {
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(4)
            .enable_all()
            .build()
            .expect("build runtime");
        runtime.block_on(async move {
            let running = match Daemon::run(home, test_options()).await {
                Ok(running) => running,
                Err(error) => {
                    let _ = ready_tx.send(Err(error.to_string()));
                    return;
                }
            };
            let addr = running.addr;
            if ready_tx.send(Ok(addr)).is_err() {
                return;
            }
            let _ = tokio::task::spawn_blocking(move || stop_rx.recv()).await;
            let _ = running.shutdown.send(());
        });
    });
    match ready_rx.recv_timeout(BOOT_DEADLINE) {
        Ok(Ok(addr)) => {
            // Leak the stop channel into the caller's handle so the daemon
            // stays up until the test drops it.
            std::mem::forget(stop_tx);
            Some((addr, handle))
        }
        Ok(Err(error)) => panic!("daemon start failed: {error}"),
        Err(_) => None,
    }
}

/// Ask the listener for anything at all. We only care that a response comes
/// back — that is the whole point of binding before per-project work.
fn responds(addr: SocketAddr) -> bool {
    use std::io::{Read, Write};
    let Ok(mut stream) = std::net::TcpStream::connect_timeout(&addr, Duration::from_secs(2)) else {
        return false;
    };
    stream.set_read_timeout(Some(Duration::from_secs(5))).ok();
    if stream
        .write_all(
            b"GET /api/daemon/status HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n",
        )
        .is_err()
    {
        return false;
    }
    let mut raw = Vec::new();
    let _ = stream.read_to_end(&mut raw);
    // Unauthenticated, so 401 is the expected answer. Any status line proves
    // the daemon is serving.
    raw.starts_with(b"HTTP/1.1 ")
}

#[test]
fn a_session_file_that_blocks_on_open_cannot_stop_the_daemon_binding() {
    let tmp = tempfile::tempdir().unwrap();
    let home = Home::at(tmp.path().join("home"));
    home.ensure().unwrap();

    let healthy = tmp.path().join("healthy");
    let poisoned = tmp.path().join("poisoned");
    let mut board = "#+title: orgasmic board\n#+orgasmic_version: 1\n\n".to_string();
    seed_project(&home, &healthy, "healthy", &mut board);
    seed_project(&home, &poisoned, "poisoned", &mut board);
    write(&home.board(), &board);

    // A named pipe with no writer: `open()` blocks forever, before any file
    // descriptor exists. That is what the operator's revoked TCC grant did.
    let sessions = poisoned.join(".orgasmic/tmp/sessions");
    std::fs::create_dir_all(&sessions).unwrap();
    let blocking = sessions.join("run-blocks-on-open.jsonl");
    assert_eq!(
        unsafe {
            let path = std::ffi::CString::new(blocking.to_str().unwrap()).unwrap();
            libc::mkfifo(path.as_ptr(), 0o644)
        },
        0,
        "create fifo at {}",
        blocking.display()
    );

    let (addr, _daemon) = boot_within_deadline(home).unwrap_or_else(|| {
        panic!(
            "daemon did not bind within {BOOT_DEADLINE:?}: a single unreadable project must never \
             hold the whole runtime — an operator has to be able to reach status and the UI to \
             learn why it is unhealthy"
        )
    });

    assert!(responds(addr), "the daemon bound but does not serve");
    // Still serving once the post-bind reattach scan has had time to run over
    // the poisoned project: a wedge there must not take the listener with it.
    std::thread::sleep(Duration::from_millis(500));
    assert!(responds(addr), "daemon stopped serving after boot");
}

/// The same guarantee for the coarser failure: a project directory the process
/// cannot enter at all.
#[test]
fn an_unreadable_project_directory_cannot_stop_the_daemon_binding() {
    use std::os::unix::fs::PermissionsExt;

    let tmp = tempfile::tempdir().unwrap();
    let home = Home::at(tmp.path().join("home"));
    home.ensure().unwrap();

    let healthy = tmp.path().join("healthy");
    let locked = tmp.path().join("locked");
    let mut board = "#+title: orgasmic board\n#+orgasmic_version: 1\n\n".to_string();
    seed_project(&home, &healthy, "healthy", &mut board);
    seed_project(&home, &locked, "locked", &mut board);
    write(&home.board(), &board);

    let locked_orgasmic = locked.join(".orgasmic");
    std::fs::set_permissions(&locked_orgasmic, std::fs::Permissions::from_mode(0o000)).unwrap();
    // root ignores the mode bits, so there would be nothing to assert.
    let permission_denial_available = std::fs::read_dir(&locked_orgasmic).is_err();
    if skip_test_if_missing(
        "an_unreadable_project_directory_cannot_stop_the_daemon_binding",
        &[("unix-permissions", permission_denial_available)],
    ) {
        std::fs::set_permissions(&locked_orgasmic, std::fs::Permissions::from_mode(0o755)).unwrap();
        return;
    }

    let bound = boot_within_deadline(home).map(|(addr, handle)| {
        assert!(responds(addr), "the daemon bound but does not serve");
        handle
    });
    let bound_ok = bound.is_some();
    drop(bound);

    // Restore before asserting so the tempdir can always be cleaned up.
    std::fs::set_permissions(&locked_orgasmic, std::fs::Permissions::from_mode(0o755)).unwrap();
    assert!(
        bound_ok,
        "daemon did not bind with an unreadable project registered"
    );
}
