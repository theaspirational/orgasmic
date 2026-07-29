//! Wire-level regression for the bounded `GET /api/runs` inventory.
//!
//! The reported failure was an authenticated run-list request that never
//! produced a complete response. Two properties have to hold on the wire, not
//! just in unit tests:
//!
//! 1. A permanently unresponsive driver attach cannot stop the response from
//!    reaching EOF within the CLI's request budget.
//! 2. Classifying a board of records must not read transcript bytes, so a
//!    single multi-megabyte TUI session (or two hundred of them) cannot push
//!    the endpoint past that budget, on the first request or any later one.
//!
//! Everything here talks raw HTTP/1.1 over TCP so "headers arrived" and "body
//! reached EOF" are observable events rather than client-library conveniences.

use std::io::{Read, Write};
use std::net::TcpStream;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use orgasmic_core::{
    Home, Lifecycle, ReleaseOutcome, SessionEnvelope, SessionEventKind, SessionScanBudget,
};
use orgasmic_daemon::{Daemon, DaemonOptions};
use orgasmic_drivers::modes::rmux::test_tooling::test_environment_lock;
use serde_json::{json, Value};

/// The CLI's own run-list budget. Every assertion here is well inside it.
const HARD_DEADLINE: Duration = Duration::from_secs(5);

/// Big enough that reading it whole would dominate every other cost in the
/// pass, so a bounded-bytes assertion is unambiguous.
const HUGE_TRANSCRIPT_BYTES: usize = 32 * 1024 * 1024;

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

fn seed_board(home: &Home, project_root: &Path, project_id: &str) {
    write(
        &project_root.join(".orgasmic/project.org"),
        format!(
            "#+title: {project_id}\n#+orgasmic_version: 1\n\n* PROJECT {project_id}\n:PROPERTIES:\n:ID:               {project_id}\n:END:\n"
        ),
    );
    write(
        &home.board(),
        format!(
            "#+title: orgasmic board\n#+orgasmic_version: 1\n\n* PROJECT {project_id}\n:PROPERTIES:\n:ID:               {project_id}\n:PATH:             {}\n:BRANCH:           main\n:STATUS:           active\n:END:\n",
            project_root.display()
        ),
    );
}

fn read_token(home: &Home) -> String {
    std::fs::read_to_string(home.auth_token())
        .expect("token file")
        .trim()
        .to_string()
}

/// One production-shaped session record: acquire + run_meta head, a
/// `text_chunk` transcript body, and an optional terminal tail.
struct SessionFixture {
    run_id: String,
    transport: String,
    harness: Option<String>,
    /// `None` for a still-open (non-terminal) record.
    outcome: Option<ReleaseOutcome>,
    /// `None` reuses the project root; `Some` records a worktree that no
    /// longer exists on disk.
    worktree: Option<PathBuf>,
    transcript_bytes: usize,
}

impl SessionFixture {
    fn new(run_id: &str, transport: &str, harness: Option<&str>) -> Self {
        Self {
            run_id: run_id.to_string(),
            transport: transport.to_string(),
            harness: harness.map(str::to_string),
            outcome: None,
            worktree: None,
            transcript_bytes: 8 * 1024,
        }
    }

    fn released(mut self, outcome: ReleaseOutcome) -> Self {
        self.outcome = Some(outcome);
        self
    }

    fn worktree(mut self, worktree: PathBuf) -> Self {
        self.worktree = Some(worktree);
        self
    }

    fn transcript_bytes(mut self, bytes: usize) -> Self {
        self.transcript_bytes = bytes;
        self
    }

    fn write_to(&self, project_root: &Path, project_id: &str) -> PathBuf {
        let mut seq = 0_u64;
        let mut out = String::with_capacity(self.transcript_bytes + 8192);
        let mut push = |kind: SessionEventKind, event: Value, out: &mut String| {
            let envelope = SessionEnvelope {
                seq,
                time: chrono::Utc::now(),
                run_id: self.run_id.clone(),
                runtime_id: format!("runtime-{}", self.run_id),
                boot_id: "boot-before-restart".to_string(),
                kind,
                event,
            };
            out.push_str(&serde_json::to_string(&envelope).unwrap());
            out.push('\n');
            seq += 1;
        };

        push(
            SessionEventKind::Lifecycle,
            serde_json::to_value(Lifecycle::Acquire {
                task_id: "TASK-INVENTORY".into(),
                kind: "worker".into(),
                worker_id: "implementer-claude-tmux".into(),
            })
            .unwrap(),
            &mut out,
        );
        push(
            SessionEventKind::Lifecycle,
            serde_json::to_value(Lifecycle::RunMeta {
                transport: self.transport.clone(),
                harness: self.harness.clone(),
                project_id: Some(project_id.to_string()),
                worktree: Some(
                    self.worktree
                        .clone()
                        .unwrap_or_else(|| project_root.to_path_buf()),
                ),
                last_path: None,
                stdout_path: None,
                dispatch_attempt_token: None,
                role: Some("implementer".into()),
                requires_worker_finalize: Some(false),
                credential_mode: None,
                driver_config: json!({"force_inert": false}),
            })
            .unwrap(),
            &mut out,
        );
        push(
            SessionEventKind::DriverEvent,
            json!({"type": "ready", "protocol_version": "tmux-tui/1", "capabilities": {}}),
            &mut out,
        );

        let chunk = "y".repeat(4000);
        let mut written = 0;
        while written < self.transcript_bytes {
            push(
                SessionEventKind::DriverEvent,
                json!({"type": "text_chunk", "stream": "stdout", "text": chunk}),
                &mut out,
            );
            written += chunk.len();
        }

        if let Some(outcome) = self.outcome {
            push(
                SessionEventKind::DriverEvent,
                json!({"type": "run_complete", "ok": true}),
                &mut out,
            );
            push(
                SessionEventKind::Lifecycle,
                serde_json::to_value(Lifecycle::Release {
                    reason: "inventory fixture".into(),
                    outcome,
                    finalized_by_worker: false,
                })
                .unwrap(),
                &mut out,
            );
        }

        let path = project_root
            .join(".orgasmic/tmp/sessions")
            .join(format!("{}.jsonl", self.run_id));
        write(&path, out);
        path
    }
}

// orgasmic:TASK-5HBST
/// Restores `PATH` when the test that installed a stub drops it — on the panic
/// path too, which a trailing restore statement misses.
struct PathGuard(Option<std::ffi::OsString>);

impl Drop for PathGuard {
    fn drop(&mut self) {
        match self.0.take() {
            Some(previous) => std::env::set_var("PATH", previous),
            None => std::env::remove_var("PATH"),
        }
    }
}

/// A `tmux` that never answers. The inventory attach probe must abandon it at
/// its own deadline instead of holding the response open.
///
/// `PATH` is process-global, so this stub is visible to every test in this
/// binary until the returned guard drops — the other test boots its own daemon,
/// whose recovery probes would otherwise resolve `tmux` to a `sleep 600`. Both
/// tests hold `test_environment_lock` for that reason; the guard bounds the
/// window and the lock keeps anyone from being inside it.
#[must_use]
fn install_hanging_tmux(bin_dir: &Path) -> PathGuard {
    let stub = bin_dir.join("tmux");
    write(
        &stub,
        "#!/bin/sh\n# Recovery attach probes call `tmux has-session`; hang forever.\nsleep 600\n",
    );
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&stub, std::fs::Permissions::from_mode(0o755)).unwrap();
    }
    let previous = std::env::var_os("PATH");
    let path = std::env::var("PATH").unwrap_or_default();
    std::env::set_var("PATH", format!("{}:{}", bin_dir.display(), path));
    PathGuard(previous)
}

struct WireResponse {
    status_line: String,
    headers: String,
    time_to_headers: Duration,
    time_to_eof: Duration,
    body: Vec<u8>,
}

/// Issue an authenticated `GET /api/runs` over a raw socket with a hard
/// deadline, recording when headers arrived and when the body reached EOF.
/// `Connection: close` makes EOF the unambiguous end of the body.
fn get_runs_over_wire(addr: std::net::SocketAddr, token: &str) -> WireResponse {
    get_runs_over_wire_query(addr, token, "")
}

/// One authenticated GET over a raw socket, for any path.
fn get_over_wire(addr: std::net::SocketAddr, token: &str, path: &str) -> WireResponse {
    let started = Instant::now();
    let mut stream = TcpStream::connect(addr).expect("connect daemon");
    stream.set_read_timeout(Some(HARD_DEADLINE)).unwrap();
    stream.set_write_timeout(Some(HARD_DEADLINE)).unwrap();
    let request = format!(
        "GET {path} HTTP/1.1\r\nHost: {addr}\r\nAuthorization: Bearer {token}\r\nConnection: close\r\nAccept: application/json\r\n\r\n"
    );
    stream.write_all(request.as_bytes()).expect("send request");
    stream.flush().unwrap();
    let mut raw = Vec::new();
    let mut chunk = [0_u8; 16 * 1024];
    loop {
        assert!(
            started.elapsed() < HARD_DEADLINE,
            "GET {path} exceeded the {HARD_DEADLINE:?} hard deadline after {} bytes",
            raw.len()
        );
        let read = stream.read(&mut chunk).expect("read response");
        if read == 0 {
            break;
        }
        raw.extend_from_slice(&chunk[..read]);
    }
    let elapsed = started.elapsed();
    let header_end = raw
        .windows(4)
        .position(|window| window == b"\r\n\r\n")
        .expect("response headers must be complete");
    let head = String::from_utf8_lossy(&raw[..header_end]).to_string();
    let (status_line, headers) = head.split_once("\r\n").unwrap_or((head.as_str(), ""));
    WireResponse {
        status_line: status_line.to_string(),
        headers: headers.to_string(),
        time_to_headers: elapsed,
        time_to_eof: elapsed,
        body: raw[header_end + 4..].to_vec(),
    }
}

/// orgasmic:TASK-FZB6T — `GET /api/runs` serves a BOUNDED recent-terminal
/// window by default; `?terminal=all` is the explicit query for the whole
/// history. Actionable buckets are never bounded either way.
fn get_runs_over_wire_query(addr: std::net::SocketAddr, token: &str, query: &str) -> WireResponse {
    let started = Instant::now();
    let mut stream = TcpStream::connect(addr).expect("connect daemon");
    stream.set_read_timeout(Some(HARD_DEADLINE)).unwrap();
    stream.set_write_timeout(Some(HARD_DEADLINE)).unwrap();
    let request = format!(
        "GET /api/runs{query} HTTP/1.1\r\nHost: {addr}\r\nAuthorization: Bearer {token}\r\nConnection: close\r\nAccept: application/json\r\n\r\n"
    );
    stream.write_all(request.as_bytes()).expect("send request");
    stream.flush().unwrap();

    let mut raw = Vec::new();
    let mut chunk = [0_u8; 16 * 1024];
    let mut time_to_headers = None;
    loop {
        assert!(
            started.elapsed() < HARD_DEADLINE,
            "GET /api/runs exceeded the {HARD_DEADLINE:?} hard deadline after {} bytes",
            raw.len()
        );
        let read = stream.read(&mut chunk).expect("read response");
        if read == 0 {
            break; // EOF
        }
        raw.extend_from_slice(&chunk[..read]);
        if time_to_headers.is_none() && raw.windows(4).any(|window| window == b"\r\n\r\n") {
            time_to_headers = Some(started.elapsed());
        }
    }
    let time_to_eof = started.elapsed();
    let header_end = raw
        .windows(4)
        .position(|window| window == b"\r\n\r\n")
        .expect("response headers must be complete");
    let head = String::from_utf8_lossy(&raw[..header_end]).to_string();
    let (status_line, headers) = head.split_once("\r\n").unwrap_or((head.as_str(), ""));

    WireResponse {
        status_line: status_line.to_string(),
        headers: headers.to_string(),
        time_to_headers: time_to_headers.expect("headers must arrive"),
        time_to_eof,
        body: raw[header_end + 4..].to_vec(),
    }
}

fn classification_ids(runs: &Value, bucket: &str) -> Vec<String> {
    runs[bucket]
        .as_array()
        .unwrap_or_else(|| panic!("{bucket} must be an array"))
        .iter()
        .map(|run| run["run_id"].as_str().unwrap().to_string())
        .collect()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn runs_endpoint_completes_over_the_wire_with_a_hanging_worker_and_huge_transcripts() {
    let _environment = test_environment_lock().lock().await;
    let tmp = tempfile::tempdir().unwrap();
    let home = Home::at(tmp.path().join("home"));
    home.ensure().unwrap();
    let project_root = tmp.path().join("project");
    seed_board(&home, &project_root, "orgasmic");

    // Boot first, then seed. Boot auto-reattach is a separate path — since
    // TASK-KKGKM it runs after bind, on a blocking thread, and it is still
    // unbounded (TASK-7QM8M). This regression is about what a *request* costs.
    let running = Daemon::run(home.clone(), test_options())
        .await
        .expect("boot daemon");
    let token = read_token(&home);

    let bin_dir = tmp.path().join("bin");
    std::fs::create_dir_all(&bin_dir).unwrap();
    let _tmux_path = install_hanging_tmux(&bin_dir);

    let missing_worktree = tmp.path().join("pruned-worktree");
    let fixtures = [
        // Terminal tombstone carrying the huge transcript.
        SessionFixture::new("run-terminal-huge", "acp-stdio", Some("claude"))
            .released(ReleaseOutcome::Completed)
            .transcript_bytes(HUGE_TRANSCRIPT_BYTES),
        // Failed tombstone: immutable, recoverable, never attach-probed.
        SessionFixture::new("run-failed", "acp-stdio", Some("claude"))
            .released(ReleaseOutcome::Failed),
        // Non-terminal on a transport with no reattachable handle.
        SessionFixture::new("run-interrupted", "acp-stdio", Some("claude")),
        // Non-terminal whose recorded worktree is gone.
        SessionFixture::new("run-missing-worktree", "acp-stdio", Some("claude"))
            .worktree(missing_worktree),
        // Non-terminal whose driver attach never answers.
        SessionFixture::new("run-hanging-attach", "tmux", Some("claude"))
            .transcript_bytes(4 * 1024 * 1024),
    ];
    for fixture in &fixtures {
        fixture.write_to(&project_root, "orgasmic");
    }

    let first = tokio::task::spawn_blocking({
        let token = token.clone();
        let addr = running.addr;
        move || get_runs_over_wire(addr, &token)
    })
    .await
    .unwrap();

    assert!(
        first.status_line.starts_with("HTTP/1.1 200"),
        "unexpected status line: {}",
        first.status_line
    );
    assert!(
        first
            .headers
            .to_ascii_lowercase()
            .contains("content-type: application/json"),
        "run list must announce JSON framing: {}",
        first.headers
    );
    assert!(
        !first.body.is_empty(),
        "response body reached EOF with zero bytes"
    );
    let body: Value =
        serde_json::from_slice(&first.body).expect("response body must parse as JSON");
    assert!(
        first.time_to_eof < HARD_DEADLINE,
        "body took {:?} to reach EOF",
        first.time_to_eof
    );
    assert!(first.time_to_headers <= first.time_to_eof);

    assert_eq!(
        classification_ids(&body, "terminal_noop"),
        vec!["run-terminal-huge"]
    );
    assert_eq!(
        classification_ids(&body, "failed_recoverable"),
        vec!["run-failed"]
    );
    assert_eq!(
        classification_ids(&body, "ambiguous"),
        vec!["run-missing-worktree"],
        "a record whose worktree is gone must not stay an attach candidate"
    );
    let mut interrupted = classification_ids(&body, "interrupted");
    interrupted.sort();
    assert_eq!(interrupted, vec!["run-hanging-attach", "run-interrupted"]);
    let hanging = body["interrupted"]
        .as_array()
        .unwrap()
        .iter()
        .find(|run| run["run_id"] == "run-hanging-attach")
        .unwrap();
    assert!(
        hanging["reason"]
            .as_str()
            .unwrap()
            .contains("exceeded inventory deadline"),
        "the hung probe must be recorded as abandoned: {}",
        hanging["reason"]
    );

    let inventory = &body["inventory"];
    assert_eq!(inventory["session_files"], 5);
    assert_eq!(inventory["attach_probes_timed_out"], 1);
    let file_bytes = inventory["session_file_bytes"].as_u64().unwrap();
    let inspected = inventory["bytes_inspected"].as_u64().unwrap();
    assert!(
        file_bytes > HUGE_TRANSCRIPT_BYTES as u64,
        "fixtures must actually be transcript-heavy: {file_bytes}"
    );
    let budget = SessionScanBudget::DEFAULT;
    let ceiling = 5 * (budget.prefix_bytes + budget.tail_bytes);
    assert!(
        inspected <= ceiling,
        "inventory read {inspected} bytes of {file_bytes}; the bounded ceiling is {ceiling}"
    );

    // Second request: the same bounded cost. Transcript bytes are never
    // rescanned, so repeated polling cannot degrade with transcript size.
    let second = tokio::task::spawn_blocking({
        let token = token.clone();
        let addr = running.addr;
        move || get_runs_over_wire(addr, &token)
    })
    .await
    .unwrap();
    let second_body: Value = serde_json::from_slice(&second.body).expect("second body parses");
    assert!(second.status_line.starts_with("HTTP/1.1 200"));
    let second_inspected = second_body["inventory"]["bytes_inspected"]
        .as_u64()
        .unwrap();
    let second_file_bytes = second_body["inventory"]["session_file_bytes"]
        .as_u64()
        .unwrap();
    // orgasmic:TASK-FZB6T — this assertion used to be `second_inspected ==
    // inspected`: repeated polling cost the same bounded window every time.
    // The catalog makes the contract STRICTLY STRONGER — a record whose session
    // file has not been written since it was indexed is answered from the
    // catalog for zero bytes — so equality is now the wrong shape and would
    // pass against a build that had lost the cache. The bound is stated as the
    // ceiling it always was, plus what the second request must NOT do.
    assert!(
        second_inspected <= inspected,
        "a second request must never inspect more than the first: \
         {second_inspected} vs {inspected}"
    );
    assert!(
        second_inspected <= ceiling && second_inspected * 10 < second_file_bytes,
        "inspected bytes must stay at the fixed per-record ceiling ({ceiling}), \
         far below the {second_file_bytes} transcript bytes on disk: {second_inspected}"
    );
    // The live/hanging records are still being written, so a poll may re-read
    // those; the released ones never are. Either way the catalog answers most
    // of the board from memory.
    assert!(
        second_body["inventory"]["catalog_cache_hits"]
            .as_u64()
            .unwrap()
            > 0,
        "a second request must answer at least some records from the catalog"
    );

    let _ = running.shutdown.send(());
    let _ = running.join.await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn runs_endpoint_stays_bounded_across_two_hundred_records() {
    // This test mutates nothing, but it boots a daemon whose recovery probes
    // resolve `tmux` through `PATH`. Holding the lock is what keeps the other
    // test's hanging stub out of this one's timing assertions.
    let _environment = test_environment_lock().lock().await;
    let tmp = tempfile::tempdir().unwrap();
    let home = Home::at(tmp.path().join("home"));
    home.ensure().unwrap();
    let project_root = tmp.path().join("project");
    seed_board(&home, &project_root, "orgasmic");

    let running = Daemon::run(home.clone(), test_options())
        .await
        .expect("boot daemon");
    let token = read_token(&home);

    // Board shape taken from the reported machine: ~200 terminal records with
    // a handful of very large transcripts.
    for index in 0..200 {
        let bytes = if index % 40 == 0 {
            4 * 1024 * 1024
        } else {
            32 * 1024
        };
        SessionFixture::new(
            &format!("run-scale-{index:03}"),
            "acp-stdio",
            Some("claude"),
        )
        .released(ReleaseOutcome::Completed)
        .transcript_bytes(bytes)
        .write_to(&project_root, "orgasmic");
    }

    // Warm the page cache and the pass itself, then measure.
    let warm = tokio::task::spawn_blocking({
        let token = token.clone();
        let addr = running.addr;
        move || get_runs_over_wire(addr, &token)
    })
    .await
    .unwrap();
    let warm_body: Value = serde_json::from_slice(&warm.body).unwrap();
    assert_eq!(warm_body["inventory"]["session_files"], 200);
    // orgasmic:TASK-FZB6T — every seeded record still CLASSIFIES terminal, but
    // the default response no longer carries all 200: terminal history only
    // grows and is never re-decided, so it is served as a bounded recent window
    // with the full count alongside it. This assertion used to read the array
    // length as the classification count; that conflated "how many records the
    // daemon classified" with "how many it chose to send", which is exactly the
    // response-size problem the window fixes.
    assert_eq!(
        warm_body["inventory"]["terminal_total"], 200,
        "every seeded record classifies terminal"
    );
    let default_window = classification_ids(&warm_body, "terminal_noop").len();
    assert_eq!(
        default_window, 50,
        "the default response carries a bounded recent-terminal window"
    );
    assert_eq!(warm_body["inventory"]["terminal_returned"], 50);

    // ...and the whole history is reachable by an explicit query, without
    // reclassifying anything.
    let all = tokio::task::spawn_blocking({
        let token = token.clone();
        let addr = running.addr;
        move || get_runs_over_wire_query(addr, &token, "?terminal=all")
    })
    .await
    .unwrap();
    let all_body: Value = serde_json::from_slice(&all.body).unwrap();
    assert_eq!(
        classification_ids(&all_body, "terminal_noop").len(),
        200,
        "?terminal=all serves the whole terminal history"
    );
    // Paging reaches the same records in windows.
    let page = tokio::task::spawn_blocking({
        let token = token.clone();
        let addr = running.addr;
        move || get_runs_over_wire_query(addr, &token, "?terminal_limit=25&terminal_offset=50")
    })
    .await
    .unwrap();
    let page_body: Value = serde_json::from_slice(&page.body).unwrap();
    let paged = classification_ids(&page_body, "terminal_noop");
    assert_eq!(paged.len(), 25);
    assert_eq!(page_body["inventory"]["terminal_total"], 200);
    assert!(
        paged
            .iter()
            .all(|id| !classification_ids(&warm_body, "terminal_noop").contains(id)),
        "an offset page must not repeat the default window"
    );

    let measured = tokio::task::spawn_blocking({
        let token = token.clone();
        let addr = running.addr;
        move || get_runs_over_wire(addr, &token)
    })
    .await
    .unwrap();
    let measured_body: Value = serde_json::from_slice(&measured.body).unwrap();
    let inventory = &measured_body["inventory"];
    let inspected = inventory["bytes_inspected"].as_u64().unwrap();
    let file_bytes = inventory["session_file_bytes"].as_u64().unwrap();
    let budget = SessionScanBudget::DEFAULT;
    assert!(
        inspected <= 200 * (budget.prefix_bytes + budget.tail_bytes),
        "inspected {inspected} bytes across 200 records"
    );
    assert!(file_bytes > 20 * 1024 * 1024);
    // Generous versus the 250 ms target so a loaded CI box does not flake,
    // but far below the CLI request budget this regression is about.
    assert!(
        measured.time_to_eof < Duration::from_secs(2),
        "warm 200-record inventory took {:?} (reported inventory duration {} ms)",
        measured.time_to_eof,
        inventory["duration_ms"]
    );

    // orgasmic:TASK-FZB6T item 4 — the maintenance accounting, over the same
    // wire and against the same 200-record board. It is a separate route from
    // the inventory precisely because it visits every byte of every session
    // file, which `GET /api/runs` must never do; this is the production path
    // proving it answers, reports by driver+harness and event class, and says
    // it wrote nothing.
    let history = tokio::task::spawn_blocking({
        let token = token.clone();
        let addr = running.addr;
        move || get_over_wire(addr, &token, "/api/runs/history")
    })
    .await
    .unwrap();
    assert!(history.status_line.starts_with("HTTP/1.1 200"));
    let history_body: Value = serde_json::from_slice(&history.body).expect("history body parses");
    let report = &history_body["report"];
    assert_eq!(report["dry_run"], true);
    assert_eq!(report["session_files"], 200);
    assert_eq!(report["unreadable_files"], 0);
    let accounted = report["bytes_accounted"].as_u64().unwrap();
    assert_eq!(
        accounted, file_bytes,
        "every byte on disk must land in exactly one event class"
    );
    let reclaimable = report["reclaimable_bytes"].as_u64().unwrap();
    assert!(
        reclaimable > 0 && reclaimable < accounted,
        "reclaimable {reclaimable} of {accounted}: authority bytes are never reclaimable"
    );
    assert!(
        report["reclaimable_by_driver"]["acp-stdio/claude"]
            .as_u64()
            .unwrap()
            > 0,
        "accounting is reported by driver+harness: {}",
        report["reclaimable_by_driver"]
    );
    let buckets = report["buckets"].as_array().unwrap();
    assert!(buckets
        .iter()
        .any(|b| b["event_class"] == "lifecycle" && b["reclaimable"] == false));
    assert!(buckets
        .iter()
        .any(|b| b["event_class"] == "rendered_tui" && b["reclaimable"] == true));
    assert!(report["retention"]
        .as_array()
        .unwrap()
        .iter()
        .any(
            |tier| tier["tier"] == "harness_native_history" && tier["authority"] == "vendor-owned"
        ));

    // ...and the board is byte-identical afterwards: the dry run changes nothing.
    let after: u64 = std::fs::read_dir(project_root.join(".orgasmic/tmp/sessions"))
        .unwrap()
        .flatten()
        .map(|entry| entry.metadata().unwrap().len())
        .sum();
    assert_eq!(after, file_bytes, "the dry run must change nothing on disk");

    let _ = running.shutdown.send(());
    let _ = running.join.await;
}
