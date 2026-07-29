// orgasmic:TASK-2YZDJ
//! Pre-ready daemon boot heartbeat published next to `daemon.lock`.
//!
//! The CLI reads this record to distinguish a live progressing boot from a
//! stalled or dead process, instead of treating every 20s wall-clock wait as
//! "daemon process exited".

use std::fs::{self, File};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use orgasmic_core::Home;
use serde::{Deserialize, Serialize};
use tokio::task::JoinHandle;

/// Filename next to `$ORGASMIC_HOME/daemon.lock`.
pub const BOOT_STATE_FILE: &str = "daemon.boot";

/// Cadence for refreshing an in-progress phase without changing its name.
const DEFAULT_REFRESH_INTERVAL: Duration = Duration::from_secs(2);

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DaemonBootState {
    pub pid: u32,
    pub phase: String,
    pub started_at: DateTime<Utc>,
    /// Monotonic progress identity: advances on every publish/refresh.
    pub seq: u64,
    pub refreshed_at: DateTime<Utc>,
}

impl DaemonBootState {
    pub fn progress_key(&self) -> (u64, i64) {
        (self.seq, self.refreshed_at.timestamp_millis())
    }
}

/// How long a published heartbeat may go unrefreshed before a reader stops
/// calling the owner "starting".
///
/// orgasmic:TASK-5P60H — derived from the cadence the owner itself publishes at:
/// four consecutive missed refreshes. One missed refresh is scheduling noise on
/// a machine loaded enough to make a boot slow in the first place; four in a row
/// is the signature TASK-KKGKM produced, where a synchronous read on the runtime
/// thread starved the refresher and the phase readout froze. Anything shorter
/// re-creates the defect this task audits — calling a live, progressing boot
/// unhealthy — and anything unbounded means a wedged owner is never named.
pub fn heartbeat_stale_after() -> Duration {
    default_refresh_interval() * 4
}

pub fn boot_state_path(home: &Home) -> PathBuf {
    home.root.join(BOOT_STATE_FILE)
}

/// What the boot record plus pid liveness say about whoever holds this home.
///
/// orgasmic:TASK-5P60H — `daemon.lock`, the boot record, and pid liveness were
/// three independent reads, and a refusal printed them side by side without ever
/// answering the one question an operator has: is this thing starting, or is it
/// dead? "A lock-held pre-bind owner with a fresh heartbeat is `starting`, not
/// unhealthy" is a *classification*, so it lives in one place and is decided
/// once, from the records rather than from an HTTP probe that cannot see a
/// pre-bind daemon at all.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BootOwnerVerdict {
    /// Live pid, heartbeat refreshed within [`heartbeat_stale_after`]. The
    /// owner is booting and must not be replaced or killed.
    Starting {
        pid: u32,
        phase: String,
        seq: u64,
        boot_age: Duration,
        heartbeat_age: Duration,
    },
    /// The record's owner is gone. Its lock, if still held, is held by
    /// something else — or by the OS, until it is actually released.
    StaleDeadOwner {
        pid: u32,
        phase: String,
        boot_age: Duration,
    },
    /// The owner is alive but has stopped refreshing: a wedged boot, not a slow
    /// one.
    StaleFrozenHeartbeat {
        pid: u32,
        phase: String,
        heartbeat_age: Duration,
    },
}

impl BootOwnerVerdict {
    /// True only for a live, progressing boot.
    pub fn is_starting(&self) -> bool {
        matches!(self, Self::Starting { .. })
    }

    pub fn pid(&self) -> u32 {
        match self {
            Self::Starting { pid, .. }
            | Self::StaleDeadOwner { pid, .. }
            | Self::StaleFrozenHeartbeat { pid, .. } => *pid,
        }
    }
}

impl std::fmt::Display for BootOwnerVerdict {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Starting {
                pid,
                phase,
                seq,
                boot_age,
                heartbeat_age,
            } => write!(
                f,
                "a boot record names pid {pid}, which is alive and starting: phase {phase:?} (seq {seq}), booting for {}ms, heartbeat {}ms old",
                boot_age.as_millis(),
                heartbeat_age.as_millis()
            ),
            Self::StaleDeadOwner {
                pid,
                phase,
                boot_age,
            } => write!(
                f,
                "a boot record names pid {pid}, which is no longer running: stale, last phase {phase:?} after {}ms",
                boot_age.as_millis()
            ),
            Self::StaleFrozenHeartbeat {
                pid,
                phase,
                heartbeat_age,
            } => write!(
                f,
                "a boot record names pid {pid}, which is alive but stalled: phase {phase:?} has not refreshed for {}ms",
                heartbeat_age.as_millis()
            ),
        }
    }
}

/// Classify a boot record against pid liveness and the clock.
///
/// `alive` is supplied by the caller so the decision itself stays pure and
/// testable; `now` likewise. A negative age (a record written by a process whose
/// clock ran ahead) saturates to zero rather than wrapping, which keeps a
/// clock-skewed record *fresh* — the failure that costs nothing.
pub fn classify_boot_owner(
    state: &DaemonBootState,
    alive: bool,
    now: DateTime<Utc>,
    stale_after: Duration,
) -> BootOwnerVerdict {
    let age = |since: DateTime<Utc>| {
        now.signed_duration_since(since)
            .to_std()
            .unwrap_or(Duration::ZERO)
    };
    let boot_age = age(state.started_at);
    let heartbeat_age = age(state.refreshed_at);
    if !alive {
        return BootOwnerVerdict::StaleDeadOwner {
            pid: state.pid,
            phase: state.phase.clone(),
            boot_age,
        };
    }
    if heartbeat_age > stale_after {
        return BootOwnerVerdict::StaleFrozenHeartbeat {
            pid: state.pid,
            phase: state.phase.clone(),
            heartbeat_age,
        };
    }
    BootOwnerVerdict::Starting {
        pid: state.pid,
        phase: state.phase.clone(),
        seq: state.seq,
        boot_age,
        heartbeat_age,
    }
}

/// One completed (or in-flight) pre-ready boot phase.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PhaseTiming {
    pub phase: String,
    pub millis: u64,
}

/// Where a boot spent its wall clock, plus what it had to read to spend it.
///
/// orgasmic:TASK-5P60H — the startup budget question ("is 8.45s the scan, the
/// session catalog, or the bind?") was unanswerable from the outside: the boot
/// published a *phase name*, never a phase *duration*. This is that measurement,
/// carried on the started daemon so a test can assert on it and logged as one
/// line so an operator can read it off a real boot.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct BootPhaseReport {
    pub phases: Vec<PhaseTiming>,
    pub total_millis: u64,
    /// Projects the boot scan indexed.
    pub projects: usize,
    /// Tx entries the boot scan loaded.
    pub tx_entries: usize,
}

impl BootPhaseReport {
    pub fn phase_millis(&self, phase: &str) -> Option<u64> {
        self.phases
            .iter()
            .find(|timing| timing.phase == phase)
            .map(|timing| timing.millis)
    }

    /// `loading config=3ms, scanning projects=812ms, …` — the log line's body.
    pub fn summary(&self) -> String {
        self.phases
            .iter()
            .map(|timing| format!("{}={}ms", timing.phase, timing.millis))
            .collect::<Vec<_>>()
            .join(", ")
    }
}

/// Read boot state; `None` when missing or unparseable (CLI degrades safely).
pub fn read_boot_state(home: &Home) -> Option<DaemonBootState> {
    read_boot_state_at(&boot_state_path(home))
}

pub fn read_boot_state_at(path: &Path) -> Option<DaemonBootState> {
    let raw = fs::read_to_string(path).ok()?;
    serde_json::from_str(raw.trim()).ok()
}

/// Remove the boot record only when it still names this process.
pub fn clear_boot_state_if_owner(home: &Home, pid: u32) {
    clear_boot_state_if_owner_at(&boot_state_path(home), pid);
}

pub fn clear_boot_state_if_owner_at(path: &Path, pid: u32) {
    match read_boot_state_at(path) {
        Some(state) if state.pid == pid => {
            let _ = fs::remove_file(path);
        }
        _ => {}
    }
}

fn write_boot_state_atomic(path: &Path, state: &DaemonBootState) -> Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).with_context(|| format!("create {}", parent.display()))?;
    }
    let payload = serde_json::to_vec_pretty(state).context("serialize daemon boot state")?;
    // A refresher and `set_phase` may publish concurrently. The monotonic
    // sequence is allocated before this write, so it gives every publication
    // its own staging path instead of allowing one writer to rename another
    // writer's file (or receive ENOENT).
    let tmp = path.with_extension(format!("boot.{}.{}.tmp", state.pid, state.seq));
    {
        let mut file = File::create(&tmp).with_context(|| format!("create {}", tmp.display()))?;
        file.write_all(&payload)
            .with_context(|| format!("write {}", tmp.display()))?;
        file.sync_all()
            .with_context(|| format!("sync {}", tmp.display()))?;
    }
    fs::rename(&tmp, path)
        .with_context(|| format!("rename {} -> {}", tmp.display(), path.display()))?;
    Ok(())
}

struct SharedBootProgress {
    path: PathBuf,
    pid: u32,
    started_at: DateTime<Utc>,
    seq: AtomicU64,
    phase: Mutex<String>,
    /// Serializes publication and retirement so an aborted refresh cannot
    /// recreate the record after its owner has retired it.
    publication: Mutex<()>,
    /// Monotonic boot start, for durations that a wall-clock adjustment cannot
    /// distort. Phases closed so far, plus when the open one began.
    started_instant: Instant,
    timings: Mutex<Vec<PhaseTiming>>,
    phase_started: Mutex<Instant>,
}

impl SharedBootProgress {
    fn publish(&self) -> Result<()> {
        let _publication = self
            .publication
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        self.publish_locked()
    }

    fn publish_locked(&self) -> Result<()> {
        let phase = self
            .phase
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone();
        let seq = self.seq.fetch_add(1, Ordering::AcqRel) + 1;
        let state = DaemonBootState {
            pid: self.pid,
            phase,
            started_at: self.started_at,
            seq,
            refreshed_at: Utc::now(),
        };
        write_boot_state_atomic(&self.path, &state)
    }

    fn set_phase(&self, phase: String) -> Result<()> {
        let _publication = self
            .publication
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        {
            let mut guard = self
                .phase
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            // The phase that is ending is the one still named here, and it ends
            // now — under the same lock that publishes, so a concurrent refresh
            // cannot land between closing it and naming its successor.
            let mut started = self
                .phase_started
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            let ended = std::mem::replace(&mut *started, Instant::now());
            self.timings
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .push(PhaseTiming {
                    phase: guard.clone(),
                    millis: ended.elapsed().as_millis() as u64,
                });
            *guard = phase;
        }
        self.publish_locked()
    }

    /// Every phase closed so far plus the open one, measured to now.
    fn report(&self) -> BootPhaseReport {
        let _publication = self
            .publication
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let mut phases = self
            .timings
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone();
        let open = self
            .phase
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone();
        let open_started = *self
            .phase_started
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        phases.push(PhaseTiming {
            phase: open,
            millis: open_started.elapsed().as_millis() as u64,
        });
        BootPhaseReport {
            phases,
            total_millis: self.started_instant.elapsed().as_millis() as u64,
            projects: 0,
            tx_entries: 0,
        }
    }

    fn retire(&self) {
        let _publication = self
            .publication
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        clear_boot_state_if_owner_at(&self.path, self.pid);
    }
}

/// Publisher owned by the lock-holding daemon process for the pre-ready window.
pub struct BootProgress {
    shared: Arc<SharedBootProgress>,
    stop_refresh: Arc<AtomicBool>,
    refresh_handle: Option<JoinHandle<()>>,
}

impl BootProgress {
    /// Begin publishing boot state immediately after lock ownership is taken.
    pub fn start(home: &Home, phase: impl Into<String>) -> Result<Self> {
        let now = Instant::now();
        let shared = Arc::new(SharedBootProgress {
            path: boot_state_path(home),
            pid: std::process::id(),
            started_at: Utc::now(),
            seq: AtomicU64::new(0),
            phase: Mutex::new(phase.into()),
            publication: Mutex::new(()),
            started_instant: now,
            timings: Mutex::new(Vec::new()),
            phase_started: Mutex::new(now),
        });
        shared.publish()?;
        Ok(Self {
            shared,
            stop_refresh: Arc::new(AtomicBool::new(false)),
            refresh_handle: None,
        })
    }

    pub fn set_phase(&self, phase: impl Into<String>) -> Result<()> {
        self.shared.set_phase(phase.into())
    }

    /// Keep `refreshed_at`/`seq` advancing during a long single phase.
    pub fn start_refresh_loop(&mut self, interval: Duration) {
        self.stop_refresh_loop();
        self.stop_refresh.store(false, Ordering::Release);
        let shared = self.shared.clone();
        let stop = self.stop_refresh.clone();
        self.refresh_handle = Some(tokio::spawn(async move {
            loop {
                tokio::time::sleep(interval).await;
                if stop.load(Ordering::Acquire) {
                    break;
                }
                let _ = shared.publish();
            }
        }));
    }

    pub fn stop_refresh_loop(&mut self) {
        self.stop_refresh.store(true, Ordering::Release);
        if let Some(handle) = self.refresh_handle.take() {
            handle.abort();
        }
    }

    pub fn pid(&self) -> u32 {
        self.shared.pid
    }

    /// Phase durations so far, including the phase currently open. Read at the
    /// moment the listener binds, before [`BootProgress::retire`] takes the
    /// record away.
    pub fn report(&self) -> BootPhaseReport {
        self.shared.report()
    }

    /// Retire this process's boot record once the daemon is ready (or aborting).
    pub fn retire(mut self) {
        self.stop_refresh_loop();
        self.shared.retire();
    }
}

impl Drop for BootProgress {
    fn drop(&mut self) {
        self.stop_refresh_loop();
        self.shared.retire();
    }
}

pub fn default_refresh_interval() -> Duration {
    std::env::var("ORGASMIC_TEST_BOOT_REFRESH_MS")
        .ok()
        .and_then(|raw| raw.parse::<u64>().ok())
        .filter(|millis| *millis > 0)
        .map(Duration::from_millis)
        .unwrap_or(DEFAULT_REFRESH_INTERVAL)
}

/// Test/prod hook: hold pre-bind boot while heartbeats continue.
pub fn prebind_hold_for_tests() -> Option<Duration> {
    std::env::var("ORGASMIC_TEST_BOOT_HOLD_MS")
        .ok()
        .and_then(|raw| raw.parse::<u64>().ok())
        .filter(|millis| *millis > 0)
        .map(Duration::from_millis)
}

/// Test hook: make the project scan itself take a chosen amount of wall clock,
/// inside the `scanning projects` phase and with its refresh loop running.
///
/// orgasmic:TASK-5P60H — [`prebind_hold_for_tests`] holds *after* the scan and
/// publishes its own phase name, so it cannot reproduce the case this task is
/// about: a board whose scan alone outlasts the readiness timeout, while every
/// other boot phase behaves normally. A real board of that size takes minutes to
/// seed and measures the host rather than the protocol.
pub fn scan_hold_for_tests() -> Option<Duration> {
    std::env::var("ORGASMIC_TEST_SCAN_HOLD_MS")
        .ok()
        .and_then(|raw| raw.parse::<u64>().ok())
        .filter(|millis| *millis > 0)
        .map(Duration::from_millis)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn write_is_atomic_and_parseable() {
        let tmp = tempfile::tempdir().unwrap();
        let home = Home::at(tmp.path().join("home"));
        std::fs::create_dir_all(&home.root).unwrap();
        let progress = BootProgress::start(&home, "scanning projects").unwrap();
        let state = read_boot_state(&home).expect("boot state");
        assert_eq!(state.pid, std::process::id());
        assert_eq!(state.phase, "scanning projects");
        assert_eq!(state.seq, 1);

        progress.set_phase("binding listener").unwrap();
        let state = read_boot_state(&home).expect("boot state after phase");
        assert_eq!(state.phase, "binding listener");
        assert_eq!(state.seq, 2);
        assert!(serde_json::from_str::<DaemonBootState>(
            &std::fs::read_to_string(boot_state_path(&home)).unwrap()
        )
        .is_ok());
        progress.retire();
        assert!(!boot_state_path(&home).exists());
    }

    #[test]
    fn clear_is_ownership_safe() {
        let tmp = tempfile::tempdir().unwrap();
        let home = Home::at(tmp.path().join("home"));
        std::fs::create_dir_all(&home.root).unwrap();
        let path = boot_state_path(&home);
        let foreign_pid = std::process::id().wrapping_add(999).max(1);
        let foreign = DaemonBootState {
            pid: foreign_pid,
            phase: "scanning projects".into(),
            started_at: Utc::now(),
            seq: 1,
            refreshed_at: Utc::now(),
        };
        write_boot_state_atomic(&path, &foreign).unwrap();
        clear_boot_state_if_owner(&home, std::process::id());
        assert!(path.exists(), "foreign boot state must survive");

        let own = BootProgress::start(&home, "loading config").unwrap();
        let pid = own.pid();
        own.retire();
        assert!(!boot_state_path(&home).exists());
        assert_ne!(pid, foreign_pid);
    }

    #[test]
    fn malformed_state_is_ignored() {
        let tmp = tempfile::tempdir().unwrap();
        let home = Home::at(tmp.path().join("home"));
        std::fs::create_dir_all(&home.root).unwrap();
        let path = boot_state_path(&home);
        std::fs::write(&path, "{partial").unwrap();
        assert!(read_boot_state(&home).is_none());
        std::fs::write(&path, "").unwrap();
        assert!(read_boot_state(&home).is_none());
    }

    #[test]
    fn slow_prebind_phase_is_visible_before_listener_bind() {
        let tmp = tempfile::tempdir().unwrap();
        let home = Home::at(tmp.path().join("home"));
        std::fs::create_dir_all(&home.root).unwrap();
        let progress = BootProgress::start(&home, "reattaching runs").unwrap();
        progress.set_phase("waiting to bind listener").unwrap();

        let state = read_boot_state(&home).expect("pre-bind state");
        assert_eq!(state.phase, "waiting to bind listener");
        progress.retire();
    }

    #[test]
    fn concurrent_publication_and_retirement_leave_no_partial_or_owned_state() {
        let tmp = tempfile::tempdir().unwrap();
        let home = Home::at(tmp.path().join("home"));
        std::fs::create_dir_all(&home.root).unwrap();
        let progress = Arc::new(BootProgress::start(&home, "loading config").unwrap());
        let mut workers = Vec::new();
        for worker in 0..8 {
            let progress = progress.clone();
            workers.push(std::thread::spawn(move || {
                for step in 0..20 {
                    progress
                        .set_phase(format!("phase-{worker}-{step}"))
                        .expect("concurrent publish");
                }
            }));
        }
        for worker in workers {
            worker.join().unwrap();
        }

        let state = read_boot_state(&home).expect("final state remains parseable");
        assert_eq!(state.pid, std::process::id());
        let staged = std::fs::read_dir(&home.root)
            .unwrap()
            .filter_map(Result::ok)
            .filter(|entry| entry.file_name().to_string_lossy().contains(".tmp"))
            .count();
        assert_eq!(staged, 0, "atomic staging files must be consumed");

        let progress = match Arc::try_unwrap(progress) {
            Ok(progress) => progress,
            Err(_) => panic!("all concurrent publishers should be joined"),
        };
        progress.retire();
        assert!(!boot_state_path(&home).exists());
    }

    fn state_at(
        pid: u32,
        phase: &str,
        started_at: DateTime<Utc>,
        refreshed_at: DateTime<Utc>,
    ) -> DaemonBootState {
        DaemonBootState {
            pid,
            phase: phase.to_string(),
            started_at,
            seq: 7,
            refreshed_at,
        }
    }

    /// orgasmic:TASK-5P60H — the classification this task exists to make
    /// possible: a lock-held pre-bind owner with a fresh heartbeat is
    /// `starting`, and nothing may replace or kill it.
    #[test]
    fn a_live_owner_with_a_fresh_heartbeat_is_starting_however_long_it_has_been_booting() {
        let now = Utc::now();
        // Far past every readiness timeout the CLI applies, which is the whole
        // point: age alone is not evidence of death.
        let started = now - chrono::Duration::seconds(600);
        let state = state_at(4242, "scanning projects", started, now);

        let verdict = classify_boot_owner(&state, true, now, heartbeat_stale_after());
        assert!(
            verdict.is_starting(),
            "a ten-minute boot with a current heartbeat must still be starting: {verdict}"
        );
        assert_eq!(verdict.pid(), 4242);
        let rendered = verdict.to_string();
        assert!(
            rendered.contains("alive and starting") && rendered.contains("scanning projects"),
            "the verdict must name the live phase: {rendered}"
        );
    }

    #[test]
    fn a_dead_owner_is_stale_even_with_a_freshly_written_record() {
        let now = Utc::now();
        let state = state_at(4243, "binding listener", now, now);
        let verdict = classify_boot_owner(&state, false, now, heartbeat_stale_after());
        assert!(matches!(verdict, BootOwnerVerdict::StaleDeadOwner { .. }));
        assert!(
            verdict.to_string().contains("no longer running"),
            "{verdict}"
        );
    }

    #[test]
    fn a_live_owner_whose_heartbeat_froze_is_stale_not_starting() {
        let now = Utc::now();
        let stale_after = heartbeat_stale_after();
        let frozen_at = now - chrono::Duration::from_std(stale_after * 2).unwrap();
        let state = state_at(4244, "scanning projects", frozen_at, frozen_at);

        let verdict = classify_boot_owner(&state, true, now, stale_after);
        assert!(matches!(
            verdict,
            BootOwnerVerdict::StaleFrozenHeartbeat { .. }
        ));
        assert!(
            verdict.to_string().contains("alive but stalled"),
            "{verdict}"
        );

        // One missed refresh is not a stall: the boundary belongs to the owner's
        // own publish cadence, not to a reader's patience.
        let one_missed = now - chrono::Duration::from_std(default_refresh_interval()).unwrap();
        let state = state_at(4244, "scanning projects", frozen_at, one_missed);
        assert!(classify_boot_owner(&state, true, now, stale_after).is_starting());
    }

    /// A record written by a process whose clock ran ahead must read as fresh,
    /// not wrap into a decade-old heartbeat and get its live owner declared
    /// stale.
    #[test]
    fn a_record_from_the_future_saturates_to_fresh() {
        let now = Utc::now();
        let ahead = now + chrono::Duration::seconds(30);
        let state = state_at(4245, "loading config", ahead, ahead);
        assert!(classify_boot_owner(&state, true, now, heartbeat_stale_after()).is_starting());
    }

    #[test]
    fn heartbeat_staleness_is_derived_from_the_publish_cadence() {
        assert_eq!(heartbeat_stale_after(), default_refresh_interval() * 4);
        assert!(
            heartbeat_stale_after() > default_refresh_interval(),
            "a single missed refresh must never read as a stall"
        );
    }

    /// orgasmic:TASK-5P60H — phase durations, which is what makes the startup
    /// budget question answerable at all.
    #[test]
    fn every_phase_a_boot_passes_through_is_measured_including_the_open_one() {
        let tmp = tempfile::tempdir().unwrap();
        let home = Home::at(tmp.path().join("home"));
        std::fs::create_dir_all(&home.root).unwrap();
        let progress = BootProgress::start(&home, "loading config").unwrap();
        std::thread::sleep(Duration::from_millis(15));
        progress.set_phase("scanning projects").unwrap();
        std::thread::sleep(Duration::from_millis(15));
        progress.set_phase("binding listener").unwrap();

        let report = progress.report();
        let names: Vec<&str> = report
            .phases
            .iter()
            .map(|timing| timing.phase.as_str())
            .collect();
        assert_eq!(
            names,
            vec!["loading config", "scanning projects", "binding listener"],
            "the open phase must be reported too, in order"
        );
        assert!(
            report.phase_millis("loading config").unwrap() >= 10,
            "a 15ms phase must not measure as instant: {}",
            report.summary()
        );
        let sum: u64 = report.phases.iter().map(|timing| timing.millis).sum();
        assert!(
            report.total_millis + 5 >= sum,
            "the total ({}ms) must account for the phases ({sum}ms)",
            report.total_millis
        );
        assert!(report.summary().contains("scanning projects="));
        progress.retire();
    }

    #[tokio::test]
    async fn retirement_wins_against_an_in_flight_refresh() {
        let tmp = tempfile::tempdir().unwrap();
        let home = Home::at(tmp.path().join("home"));
        std::fs::create_dir_all(&home.root).unwrap();
        let mut progress = BootProgress::start(&home, "binding listener").unwrap();
        progress.start_refresh_loop(Duration::from_millis(1));
        tokio::time::sleep(Duration::from_millis(10)).await;

        progress.retire();
        tokio::time::sleep(Duration::from_millis(10)).await;
        assert!(
            !boot_state_path(&home).exists(),
            "an aborted refresh must not recreate the retired owner record"
        );
    }
}
