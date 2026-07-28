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
///
/// TASK-Z3093 collapsed the nine copies of this guard into one definition in
/// `orgasmic_drivers::modes::rmux::test_tooling`, which also reaps the rmux
/// sessions a test registers on it. Re-exported here so CLI callers keep their
/// existing import.
pub(crate) use orgasmic_drivers::modes::rmux::test_tooling::live_session_guard;

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

/// A minimal HTTP daemon stand-in that records every path it is asked for.
///
/// Exists so a control-path fence can be tested for *which question it asks*,
/// not merely for the answer it reaches: a fence that reads liveness from the
/// recovery inventory and one that reads it from the live source are
/// indistinguishable by return value on a healthy board, and differ only on a
/// board where the durable history cannot be read.
pub(crate) struct RecordingDaemon {
    port: u16,
    paths: std::sync::Arc<Mutex<Vec<String>>>,
    join: Option<std::thread::JoinHandle<()>>,
}

impl RecordingDaemon {
    /// Bind an ephemeral port and answer each request through `respond`, which
    /// maps a request path to `(status, json_body)`. An unmapped path is a 404.
    pub(crate) fn start(respond: fn(&str) -> Option<(u16, String)>) -> Self {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind recording daemon");
        let port = listener.local_addr().unwrap().port();
        let paths = std::sync::Arc::new(Mutex::new(Vec::new()));
        let recorded = paths.clone();
        let join = std::thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(mut stream) = stream else { break };
                let mut request = Vec::new();
                let mut byte = [0_u8; 1];
                // Read just the request line + headers; these are GETs.
                loop {
                    match std::io::Read::read(&mut stream, &mut byte) {
                        Ok(0) | Err(_) => break,
                        Ok(_) => {
                            request.push(byte[0]);
                            if request.ends_with(b"\r\n\r\n") {
                                break;
                            }
                        }
                    }
                }
                let head = String::from_utf8_lossy(&request).to_string();
                let Some(path) = head
                    .lines()
                    .next()
                    .and_then(|line| line.split_whitespace().nth(1))
                else {
                    continue;
                };
                if path == SHUTDOWN_PATH {
                    break;
                }
                recorded.lock().unwrap_or_else(|p| p.into_inner()).push(path.to_string());
                let (status, body) = respond(path)
                    .unwrap_or_else(|| (404, "{\"error\":\"not found\"}".to_string()));
                let reason = match status {
                    200 => "OK",
                    404 => "Not Found",
                    500 => "Internal Server Error",
                    _ => "Status",
                };
                let response = format!(
                    "HTTP/1.1 {status} {reason}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                );
                let _ = std::io::Write::write_all(&mut stream, response.as_bytes());
                let _ = std::io::Write::flush(&mut stream);
                let _ = stream.shutdown(std::net::Shutdown::Both);
            }
        });
        Self {
            port,
            paths,
            join: Some(join),
        }
    }

    pub(crate) fn port(&self) -> u16 {
        self.port
    }

    /// Every path requested so far, in order.
    pub(crate) fn paths(&self) -> Vec<String> {
        self.paths
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .clone()
    }
}

/// Path the accept loop treats as "stop"; never recorded, never routed.
const SHUTDOWN_PATH: &str = "/__recording_daemon_shutdown";

impl Drop for RecordingDaemon {
    fn drop(&mut self) {
        // Unblock the accept loop rather than leaving a thread parked on it.
        if let Ok(mut stream) = std::net::TcpStream::connect(("127.0.0.1", self.port)) {
            let _ = std::io::Write::write_all(
                &mut stream,
                format!("GET {SHUTDOWN_PATH} HTTP/1.1\r\nHost: localhost\r\n\r\n").as_bytes(),
            );
        }
        if let Some(join) = self.join.take() {
            let _ = join.join();
        }
    }
}
