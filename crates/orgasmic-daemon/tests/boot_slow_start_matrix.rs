//! One slow boot, several starts racing it: exactly one owner, one listener.
//!
//! orgasmic:TASK-5P60H — the crash-loop report this task audits is a log of
//! alternating `Address already in use` and instance-lock failures while one
//! daemon was doing a project scan that outlasted every readiness timeout
//! pointed at it. The lock-first boot protocol (TASK-ATAXN, TASK-TZKAC,
//! TASK-2YZDJ) is supposed to make that impossible. Nothing exercised it against
//! a scan slow enough to reproduce the window, so "impossible" was an argument
//! rather than a result.
//!
//! This is that window, deterministically:
//!
//! - the winner holds the lock and spends [`SCAN_HOLD`] inside `scanning
//!   projects`, publishing heartbeats the whole time;
//! - one competitor is given a lock-wait budget an order of magnitude *shorter*
//!   than the scan — the compressed analogue of a readiness timeout that expires
//!   mid-boot — and must refuse, naming the live owner it refused for;
//! - one competitor is given a budget that outlasts the scan and must conclude
//!   with the incumbent's own identity, not a second listener;
//! - an autostart-shaped observer reads only the published records throughout
//!   and must see one owner, advancing, classified `starting`.
//!
//! Compression rather than production numbers, for the reason TASK-G7E4R gives:
//! the ratio between the slow phase and the budget is what is under test, and
//! waiting out `ShutdownBudgets::default()` here would be testing that constant.
//!
//! Process identity cannot vary in-process — every actor here shares one pid —
//! so "one owner" is asserted where it is actually recorded: the lock file, the
//! boot record, and the incumbent status all name the same owner, the OS lock
//! stays held throughout, and exactly one socket ends up bound.

use std::path::Path;
use std::sync::mpsc;
use std::time::{Duration, Instant};

use orgasmic_core::Home;
use orgasmic_daemon::{
    classify_boot_owner, heartbeat_stale_after, read_boot_state, BootPhaseReport, Daemon,
    DaemonAlreadyRunning, DaemonInstanceLockHeld, DaemonOptions, LockHolder, ShutdownBudgets,
};

/// How long the winner's project scan takes. Long enough that every other actor
/// in this test is provably still inside it, short enough to keep the suite fast.
const SCAN_HOLD: Duration = Duration::from_millis(3000);

/// A lock wait that expires *inside* the scan: one tenth of it. This is the
/// readiness timeout that used to fire mid-boot, compressed.
const IMPATIENT_BUDGET: Duration = Duration::from_millis(300);

/// A lock wait that outlasts the scan, so the competitor is still asking when
/// the winner finally answers.
const PATIENT_BUDGET: Duration = Duration::from_millis(12_000);

/// Ceiling on the whole race. Generous next to `SCAN_HOLD` and still decisive.
const BOOT_DEADLINE: Duration = Duration::from_secs(40);

/// Heartbeat cadence: fast enough that a 3s scan publishes many refreshes, so
/// "advancing" is an observation rather than a coin flip.
const REFRESH_MS: &str = "100";

fn write(path: &Path, contents: impl AsRef<str>) {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).unwrap();
    }
    std::fs::write(path, contents.as_ref()).unwrap();
}

fn seed_project(project_root: &Path, project_id: &str, board: &mut String) {
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

/// A port nothing holds right now. The daemons in this test must reach each
/// other's health probe, so port 0 is not an option: `classify_lock_holder`
/// cannot probe an address it does not know, and every verdict would be
/// inconclusive for a reason that has nothing to do with the race.
fn free_port() -> u16 {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("reserve a port");
    let port = listener.local_addr().expect("local_addr").port();
    drop(listener);
    port
}

fn options(budgets: Option<ShutdownBudgets>) -> DaemonOptions {
    DaemonOptions {
        fs_watcher_enabled: false,
        shutdown_budgets: budgets,
        ..DaemonOptions::default()
    }
}

/// Budgets whose `total()` is `wait`: the number `undeclared_holder_budget`
/// derives a competing start's lock wait from.
fn lock_wait(wait: Duration) -> ShutdownBudgets {
    ShutdownBudgets {
        connection_drain: wait,
        release_drain: Duration::ZERO,
        writer_shutdown: Duration::ZERO,
    }
}

/// How a competing start ended.
struct Refusal {
    message: String,
    /// Set when the start concluded that a healthy daemon already owns this
    /// home — the typed outcome, not a phrase in a string.
    incumbent: Option<DaemonAlreadyRunning>,
    /// Set when the start refused on the instance lock.
    lock_held: Option<LockHolder>,
}

/// Run one competing `Daemon::run` to completion on its own runtime and return
/// how it refused. A competitor that *succeeds* is the defect, so success is
/// reported as such rather than unwrapped away.
fn competing_start(home: Home, wait: Duration) -> Result<Refusal, String> {
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .expect("build competitor runtime");
    runtime.block_on(async move {
        match Daemon::run(home, options(Some(lock_wait(wait)))).await {
            Ok(running) => {
                let addr = running.addr;
                let _ = running.shutdown.send(());
                let _ = running.join.await;
                Err(format!("a competing start bound a second listener at {addr}"))
            }
            Err(error) => Ok(Refusal {
                message: format!("{error:#}"),
                incumbent: error.downcast_ref::<DaemonAlreadyRunning>().cloned(),
                lock_held: error
                    .downcast_ref::<DaemonInstanceLockHeld>()
                    .map(|held| held.holder.clone()),
            }),
        }
    })
}

struct Winner {
    addr: std::net::SocketAddr,
    boot_id: String,
    pid: u32,
    report: BootPhaseReport,
}

#[test]
fn a_scan_that_outlasts_the_readiness_budget_still_leaves_one_owner_and_one_listener() {
    let tmp = tempfile::tempdir().unwrap();
    let home = Home::at(tmp.path().join("home"));
    home.ensure().unwrap();
    let port = free_port();
    write(
        &home.config(),
        format!("bind_host: 127.0.0.1\nbind_port: {port}\n"),
    );

    let mut board = "#+title: orgasmic board\n#+orgasmic_version: 1\n\n".to_string();
    for id in ["alpha", "beta", "gamma"] {
        seed_project(&tmp.path().join(id), id, &mut board);
    }
    write(&home.board(), &board);

    // Process-global by construction (the daemon reads them where it uses
    // them), which is why this file holds exactly one test.
    std::env::set_var("ORGASMIC_TEST_SCAN_HOLD_MS", SCAN_HOLD.as_millis().to_string());
    std::env::set_var("ORGASMIC_TEST_BOOT_REFRESH_MS", REFRESH_MS);

    // ---- the winner: holds the lock and stays inside the scan --------------
    let (ready_tx, ready_rx) = mpsc::channel::<Result<Winner, String>>();
    let (stop_tx, stop_rx) = mpsc::channel::<()>();
    let winner_home = home.clone();
    let winner = std::thread::spawn(move || {
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(4)
            .enable_all()
            .build()
            .expect("build winner runtime");
        runtime.block_on(async move {
            let running = match Daemon::run(winner_home, options(None)).await {
                Ok(running) => running,
                Err(error) => {
                    let _ = ready_tx.send(Err(format!("{error:#}")));
                    return;
                }
            };
            let announced = Winner {
                addr: running.addr,
                boot_id: running.boot_id.clone(),
                pid: std::process::id(),
                report: running.boot_report.clone(),
            };
            if ready_tx.send(Ok(announced)).is_err() {
                return;
            }
            let _ = tokio::task::spawn_blocking(move || stop_rx.recv()).await;
            let _ = running.shutdown.send(());
            let _ = running.join.await;
        });
    });

    // ---- the boot is under way: one owner, published and progressing -------
    let started = Instant::now();
    let first = loop {
        if let Some(state) = read_boot_state(&home) {
            break state;
        }
        assert!(
            started.elapsed() < BOOT_DEADLINE,
            "the winner never published a boot record"
        );
        std::thread::sleep(Duration::from_millis(10));
    };
    assert_eq!(first.pid, std::process::id());

    // ---- the impatient competitor, refusing inside the slow scan -----------
    let refusal = competing_start(home.clone(), IMPATIENT_BUDGET)
        .unwrap_or_else(|bound| panic!("{bound}"));
    let impatient = refusal.message.clone();
    match refusal.lock_held {
        Some(LockHolder::NotDeparting { waited }) => assert!(
            waited >= IMPATIENT_BUDGET,
            "the impatient start gave up after {waited:?}, inside its own budget"
        ),
        other => panic!(
            "an impatient start inside a healthy boot must refuse on the instance \
             lock, got {other:?}: {impatient}"
        ),
    }
    assert!(
        refusal.incumbent.is_none(),
        "nothing was listening yet, so no incumbent could have been established"
    );
    // The whole point of the audit: never an address race. The competitor never
    // reached bind, so `Address already in use` cannot appear.
    assert!(
        !impatient.to_lowercase().contains("address")
            && !impatient.to_lowercase().contains("in use"),
        "a refused start must never have reached bind: {impatient}"
    );
    // orgasmic:TASK-5P60H — and the refusal correlates all four records, so an
    // operator reads `starting` rather than inferring it: the lock's pid is
    // alive, the boot record's owner is classified, and the HTTP probe failure
    // is reported as an observation about the observer, not a verdict.
    assert!(
        impatient.contains("alive and starting"),
        "the refusal must classify a live pre-bind owner as starting: {impatient}"
    );
    assert!(
        impatient.contains("scanning projects"),
        "the refusal must name the phase the owner is in: {impatient}"
    );
    assert!(
        impatient.contains("which is alive") && impatient.contains("health probe"),
        "the refusal must correlate lock pid liveness with the probe it could \
         not complete: {impatient}"
    );

    // A refusal must leave the boot exactly as it found it. "Never break a live
    // file lock based only on HTTP failure" is asserted against the lock itself.
    let during = read_boot_state(&home).expect("the boot record survived a competing start");
    assert_eq!(during.pid, first.pid, "the owner changed under a refusal");
    assert!(
        during.seq > first.seq,
        "the heartbeat did not advance across the competing start ({} -> {})",
        first.seq,
        during.seq
    );
    assert!(
        classify_boot_owner(&during, true, chrono::Utc::now(), heartbeat_stale_after())
            .is_starting(),
        "a live owner mid-scan must classify as starting, not stale"
    );
    let lock = home.root.join("daemon.lock");
    assert_eq!(
        std::fs::read_to_string(&lock)
            .unwrap()
            .trim()
            .parse::<u32>()
            .ok(),
        Some(first.pid),
        "the lock file stopped naming the boot's owner"
    );
    let probe = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(&lock)
        .expect("open the lock file");
    assert!(
        fs2::FileExt::try_lock_exclusive(&probe).is_err(),
        "the OS lock was released while its owner was still booting"
    );
    drop(probe);

    // ---- the patient competitor, overlapping the bind ----------------------
    let patient_home = home.clone();
    let patient = std::thread::spawn(move || competing_start(patient_home, PATIENT_BUDGET));

    // ---- the winner finishes -----------------------------------------------
    let winner_result = ready_rx
        .recv_timeout(BOOT_DEADLINE)
        .expect("the winner never became ready");
    let ready = winner_result.unwrap_or_else(|error| panic!("the winner failed to boot: {error}"));
    assert_eq!(ready.addr.port(), port);
    assert_eq!(ready.pid, first.pid, "a different process finished the boot");

    let patient = patient
        .join()
        .expect("patient competitor thread")
        .unwrap_or_else(|bound| panic!("{bound}"));
    let incumbent = patient.incumbent.as_ref().unwrap_or_else(|| {
        panic!(
            "a start that waited out the slow boot must resolve to the incumbent \
             it waited for, got: {}",
            patient.message
        )
    });
    assert_eq!(
        incumbent.boot_id, ready.boot_id,
        "the waiting start concluded with a different daemon's identity"
    );
    assert_eq!(incumbent.pid, ready.pid, "two owners answered for one home");
    assert_eq!(incumbent.addr.port(), port);
    assert!(
        !patient.message.to_lowercase().contains("in use"),
        "a waiting start must never race the port: {}",
        patient.message
    );

    // ---- exactly one listener, and the index it serves is complete ---------
    assert!(
        std::net::TcpListener::bind(("127.0.0.1", port)).is_err(),
        "the daemon's port is free, so nothing is listening on it"
    );
    let projects = get_json(ready.addr, &home, "/api/projects");
    for id in ["alpha", "beta", "gamma"] {
        assert!(
            projects.contains(id),
            "a normal route answered from a partially rebuilt index (missing {id}): {projects}"
        );
    }

    // The boot record is retired once ready, so no reader keeps reporting a
    // phase for a daemon that is serving.
    assert!(
        read_boot_state(&home).is_none(),
        "the boot heartbeat outlived the boot it describes"
    );

    // ---- the measurement ---------------------------------------------------
    let report = &ready.report;
    for phase in [
        "loading config",
        "loading auth",
        "scanning projects",
        "migrating sessions",
        "starting runtime",
        "attaching watchers",
        "waiting to bind listener",
        "binding listener",
    ] {
        assert!(
            report.phase_millis(phase).is_some(),
            "boot phase {phase:?} was not measured: {}",
            report.summary()
        );
    }
    assert_eq!(report.projects, 3, "phase durations without the board size \
         they were spent on are not a measurement");
    assert!(
        report.phase_millis("scanning projects").unwrap() >= SCAN_HOLD.as_millis() as u64,
        "the slow scan did not show up in its own phase: {}",
        report.summary()
    );
    assert!(
        report.total_millis >= SCAN_HOLD.as_millis() as u64,
        "the boot total does not account for the scan: {}ms",
        report.total_millis
    );

    // ---- and nothing killed the healthy boot -------------------------------
    let status = get_json(ready.addr, &home, "/api/daemon/status");
    assert!(
        status.contains(&ready.boot_id),
        "the daemon serving after the race is not the one that booted: {status}"
    );

    let _ = stop_tx.send(());
    let _ = winner.join();
    std::env::remove_var("ORGASMIC_TEST_SCAN_HOLD_MS");
    std::env::remove_var("ORGASMIC_TEST_BOOT_REFRESH_MS");
}

/// Authenticated GET against the running daemon, returning its body.
fn get_json(addr: std::net::SocketAddr, home: &Home, path: &str) -> String {
    let token = std::fs::read_to_string(home.auth_token())
        .expect("auth token")
        .trim()
        .to_string();
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("build client runtime");
    runtime.block_on(async move {
        let client = reqwest::Client::builder()
            .timeout(Duration::from_secs(5))
            .build()
            .expect("client");
        let response = client
            .get(format!("http://{addr}{path}"))
            .bearer_auth(token)
            .send()
            .await
            .unwrap_or_else(|error| panic!("GET {path}: {error}"));
        assert!(response.status().is_success(), "GET {path}: {}", response.status());
        response.text().await.expect("body")
    })
}
