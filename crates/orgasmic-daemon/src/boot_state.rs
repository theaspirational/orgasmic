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
use std::time::Duration;

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

pub fn boot_state_path(home: &Home) -> PathBuf {
    home.root.join(BOOT_STATE_FILE)
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
            *guard = phase;
        }
        self.publish_locked()
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
        let shared = Arc::new(SharedBootProgress {
            path: boot_state_path(home),
            pid: std::process::id(),
            started_at: Utc::now(),
            seq: AtomicU64::new(0),
            phase: Mutex::new(phase.into()),
            publication: Mutex::new(()),
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
