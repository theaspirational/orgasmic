//! Shared test-only helpers for serializing access to process-global
//! environment variables.
//!
//! Environment variables are process-global, so any test that mutates them —
//! or runs production code that *reads* them — must serialize against every
//! other such test in the crate, not just the ones in its own module. The
//! daemon token/URL vars (`ORGASMIC_DAEMON_URL`, `ORGASMIC_DAEMON_TOKEN`,
//! `ORGASMIC_DAEMON_TOKEN_FILE`) are set by `daemon_client` tests and read by
//! `doctor` tests' production paths; without ONE shared lock they race under
//! `cargo test --workspace` (TASK-SJQ9V, same class as TASK-BRXGG).

use std::sync::{Mutex, MutexGuard, OnceLock};

/// Serialize heavy real-subprocess tests across ALL test binaries. Some CLI
/// tests spawn real `git` (init/commit/clone/worktree) or boot a real daemon;
/// under `cargo test --workspace` peak concurrency those subprocesses
/// transiently fail or race (a failed `git` spawn even panics `run_git`, whose
/// `assert!(status.success())` treats CPU-pressure failure as a hard error).
/// This is the same contention class as the live tmux/rmux tests (TASK-X0ZVE)
/// and shares their lock PATH, so at most one heavy test runs at a time across
/// every binary. Held for the whole test via the returned guard (TASK-SJQ9V
/// residual: doctor staleness, content-hub install, dispatch-close pruning).
pub(crate) fn live_session_guard() -> LiveSessionGuard {
    let path = std::env::temp_dir().join("orgasmic-live-session-tests.lock");
    let file = std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(&path)
        .expect("open live-session lock file");
    // MSRV 1.87: call fs2 explicitly — std's File::lock_exclusive (1.89) shadows it.
    fs2::FileExt::lock_exclusive(&file).expect("flock live-session lock");
    LiveSessionGuard(file)
}

/// RAII drop-guard releasing the [`live_session_guard`] advisory flock.
pub(crate) struct LiveSessionGuard(std::fs::File);

impl Drop for LiveSessionGuard {
    fn drop(&mut self) {
        let _ = fs2::FileExt::unlock(&self.0);
    }
}

/// Acquire the process-wide environment lock. Hold the returned guard for the
/// duration of any test that sets/clears env or exercises production code that
/// reads the shared daemon env vars.
///
/// Poison-resilient on purpose: a test that panics while holding the guard
/// would otherwise poison the mutex and cascade-fail every later test that
/// locks it (observed as `PoisonError` cascades under workspace concurrency).
/// Recovering the inner guard keeps one failure from masking the rest.
pub(crate) fn env_guard() -> MutexGuard<'static, ()> {
    static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
    LOCK.get_or_init(|| Mutex::new(()))
        .lock()
        .unwrap_or_else(|poison| poison.into_inner())
}

/// RAII environment override: applies the requested changes on construction and
/// restores the prior values (or absence) on drop. Construct while holding
/// [`env_guard`].
pub(crate) struct ScopedEnv {
    keys: Vec<(&'static str, Option<String>)>,
}

impl ScopedEnv {
    /// Set each `(key, value)` pair, remembering the prior value for restore.
    pub(crate) fn set(pairs: &[(&'static str, &str)]) -> Self {
        let keys = pairs
            .iter()
            .map(|(key, value)| {
                let prior = std::env::var(key).ok();
                std::env::set_var(key, value);
                (*key, prior)
            })
            .collect();
        Self { keys }
    }

    /// Remove each key, remembering the prior value for restore.
    pub(crate) fn clear(keys: &[&'static str]) -> Self {
        let keys = keys
            .iter()
            .map(|key| {
                let prior = std::env::var(key).ok();
                std::env::remove_var(key);
                (*key, prior)
            })
            .collect();
        Self { keys }
    }
}

impl Drop for ScopedEnv {
    fn drop(&mut self) {
        for (key, prior) in &self.keys {
            match prior {
                Some(value) => std::env::set_var(key, value),
                None => std::env::remove_var(key),
            }
        }
    }
}
