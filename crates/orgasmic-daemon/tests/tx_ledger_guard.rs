// orgasmic:task_HQ970
//! The append-only tx ledger must survive every write its own writer accepts
//! (TASK-HQ970).
//!
//! A `tx record --reason "<multi-line text>"` used to be accepted, written
//! verbatim into the property drawer, and reported back with a `tx_id` — and
//! the next read of that ledger failed with "blank line inside property
//! drawer". Nothing could then dispatch, close, or update a task, because
//! every verb reads the ledger first. Both ledgers were hit in one session:
//! the project ledger and the `$ORGASMIC_HOME/state/tx/` home file.
//!
//! The contract these tests pin:
//!
//! - a multi-line `reason` is REFUSED at the API boundary, with a message
//!   naming the offending property and the single-line constraint;
//! - nothing is written — the ledger file is byte-identical afterwards and
//!   still parses;
//! - the same holds on both destinations (project ledger and home ledger) and
//!   for `extra` values, not just `reason`;
//! - the supported single-line write still lands and the ledger still parses.
//!
//! Shares its principle with TASK-ZYWZD (a writer must not commit what it
//! cannot read back), applied here to a drawer-only record.

mod common;

use std::path::Path;

use orgasmic_core::Home;
use orgasmic_daemon::{Daemon, DaemonOptions};

/// Where this project's tx files land: the legacy `.orgasmic/tx/` unless a
/// per-machine `.orgasmic/machines/<id>/tx/` exists (TASK-MSYN4).
fn resolve_tx_dir(project_root: &std::path::Path) -> std::path::PathBuf {
    let dotorg = project_root.join(".orgasmic");
    if let Ok(machines) = std::fs::read_dir(dotorg.join("machines")) {
        for machine in machines.flatten() {
            let candidate = machine.path().join("tx");
            if candidate.is_dir() {
                return candidate;
            }
        }
    }
    dotorg.join("tx")
}

fn test_options() -> DaemonOptions {
    DaemonOptions {
        bind_override: Some("127.0.0.1".parse().unwrap()),
        port_override: Some(0),
        fs_watcher_enabled: false,
        tmux_input_ready_timeout_secs: Some(1),
        ..DaemonOptions::default()
    }
}

fn write(path: &Path, contents: impl AsRef<str>) {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).unwrap();
    }
    std::fs::write(path, contents.as_ref()).unwrap();
}

fn read_token(home: &Home) -> String {
    let path = home.auth_token();
    for _ in 0..20 {
        if path.exists() {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
    std::fs::read_to_string(&path)
        .unwrap_or_else(|_| {
            std::fs::read_to_string(home.user().join("auth/token")).expect("token file")
        })
        .trim()
        .to_string()
}

fn seed_project(home: &Home, project_root: &Path, project_id: &str) {
    write(
        &project_root.join(".orgasmic/project.org"),
        format!(
            "#+title: {project_id}\n#+orgasmic_version: 1\n\n\
             * PROJECT {project_id}\n:PROPERTIES:\n:ID:               {project_id}\n:END:\n"
        ),
    );
    write(
        &project_root.join(".orgasmic/tasks/TASK-T01/node.org"),
        "#+title: orgasmic task TASK-T01\n#+orgasmic_version: 2\n\n\
         * BACKLOG TASK-T01 Ledger guard task :work:\n\
         :PROPERTIES:\n\
         :ID:               TASK-T01\n\
         :END:\n\n\
         ** Description\nOriginal description.\n",
    );
    write(
        &home.board(),
        format!(
            "#+title: orgasmic board\n#+orgasmic_version: 1\n\n\
             * PROJECT {project_id}\n:PROPERTIES:\n:ID:               {project_id}\n\
             :PATH:             {}\n:BRANCH:           main\n:END:\n",
            project_root.display()
        ),
    );
}

/// The shape that bricked both real ledgers on 2026-07-26: a manager reason
/// pasted from a file, blank line and all.
const MULTI_LINE_REASON: &str = "Dispatched implementer for TASK-D0GA3.\n\n\
     The brief carries the reproduced brick, the three defects, and the\n\
     acceptance. Prior art is TASK-ZYWZD.";

async fn post_tx(
    client: &reqwest::Client,
    base: &str,
    token: &str,
    body: serde_json::Value,
) -> reqwest::Response {
    client
        .post(format!("{base}/api/tx"))
        .bearer_auth(token)
        .json(&body)
        .send()
        .await
        .unwrap()
}

/// Read every tx file under `dir` and parse it the way the daemon does. Panics
/// with the parse error, which is exactly the failure a bricked ledger shows.
fn assert_ledger_parses(dir: &Path, what: &str) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("org") {
            continue;
        }
        let source = std::fs::read_to_string(&path).unwrap();
        if let Err(e) = orgasmic_core::parse_tx_file(&source, &path.to_string_lossy()) {
            panic!("{what} ledger no longer parses after the write: {e}\n---\n{source}");
        }
    }
}

/// Every `.org` file directly under `dir`, sorted. Missing dir is empty, not a
/// panic: a ledger shape only exists once something has been written into it.
/// Node journals get the node-kernel parser, not `parse_tx_file`: an entry
/// there may legitimately carry a body after the drawer (that is where a
/// comment's prose lives), which the strict project-ledger parser refuses.
fn assert_journal_parses(path: &Path, what: &str) -> Vec<orgasmic_core::node_kernel::JournalEntry> {
    let Ok(source) = std::fs::read_to_string(path) else {
        return Vec::new();
    };
    match orgasmic_core::node_kernel::parse_journal(&source, &path.to_string_lossy()) {
        Ok(entries) => entries,
        Err(e) => panic!("{what} no longer parses after the write: {e}\n---\n{source}"),
    }
}

fn org_files(dir: &Path) -> Vec<std::path::PathBuf> {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut out: Vec<_> = entries
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.extension().and_then(|e| e.to_str()) == Some("org"))
        .collect();
    out.sort();
    out
}

fn snapshot(dir: &Path) -> Vec<(String, String)> {
    let mut out = Vec::new();
    let Ok(entries) = std::fs::read_dir(dir) else {
        return out;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("org") {
            continue;
        }
        out.push((
            path.file_name().unwrap().to_string_lossy().to_string(),
            std::fs::read_to_string(&path).unwrap(),
        ));
    }
    out.sort();
    out
}

/// The refusal has to be readable by the operator who typed the command: it
/// must name the property that carries the newline and say what is allowed
/// instead. "tx append failed" is the silence this task exists to remove.
fn assert_names_the_constraint(message: &str, property: &str) {
    assert!(
        message.contains(property),
        "refusal must name the offending property {property:?}: {message}"
    );
    assert!(
        message.to_lowercase().contains("single line")
            || message.to_lowercase().contains("single-line"),
        "refusal must state the single-line constraint: {message}"
    );
}

// ---------------------------------------------------------------------------
// The project ledger
// ---------------------------------------------------------------------------

#[tokio::test]
async fn multi_line_reason_is_refused_and_project_ledger_still_parses() {
    let tmp = tempfile::tempdir().unwrap();
    let home = Home::at(tmp.path().join("home"));
    home.ensure().unwrap();
    let project_root = tmp.path().join("proj");
    seed_project(&home, &project_root, "txguard");

    let running = Daemon::run(home.clone(), test_options())
        .await
        .expect("boot daemon");
    let token = read_token(&home);
    let client = reqwest::Client::new();
    let base = format!("http://{}", running.addr);
    let project_tx_dir = || resolve_tx_dir(&project_root);

    // A supported single-line write first, so the ledger is non-empty and the
    // refusal below has something it could corrupt.
    let ok = post_tx(
        &client,
        &base,
        &token,
        serde_json::json!({
            "type": "manager.action",
            "project": "txguard",
            "task": "TASK-T01",
            "reason": "Dispatched implementer for TASK-T01.",
        }),
    )
    .await;
    assert_eq!(
        ok.status(),
        reqwest::StatusCode::OK,
        "single-line reason must still be accepted: {}",
        ok.text().await.unwrap()
    );
    assert_ledger_parses(&project_tx_dir(), "project");
    let before = snapshot(&project_tx_dir());
    assert!(!before.is_empty(), "the project ledger should exist by now");

    let resp = post_tx(
        &client,
        &base,
        &token,
        serde_json::json!({
            "type": "manager.action",
            "project": "txguard",
            "task": "TASK-T01",
            "reason": MULTI_LINE_REASON,
            "extra": [["ARTIFACTS", "report.md"]],
        }),
    )
    .await;

    let status = resp.status();
    let text = resp.text().await.unwrap();
    assert_eq!(
        status,
        reqwest::StatusCode::BAD_REQUEST,
        "a multi-line reason must be refused, never appended: {text}"
    );
    assert_names_the_constraint(&text, "REASON");
    common::assert_body_rejects_paths(&text, &[&project_root]);

    assert_eq!(
        snapshot(&project_tx_dir()),
        before,
        "a refused tx write must leave the ledger byte-identical"
    );
    assert_ledger_parses(&project_tx_dir(), "project");

    let _ = running.shutdown.send(());
    let _ = running.join.await;
}

// ---------------------------------------------------------------------------
// The home ledger ($ORGASMIC_HOME/state/tx) — hit by the same incident
// ---------------------------------------------------------------------------

#[tokio::test]
async fn multi_line_reason_is_refused_and_home_ledger_still_parses() {
    let tmp = tempfile::tempdir().unwrap();
    let home = Home::at(tmp.path().join("home"));
    home.ensure().unwrap();
    let project_root = tmp.path().join("proj");
    seed_project(&home, &project_root, "txguardhome");

    let running = Daemon::run(home.clone(), test_options())
        .await
        .expect("boot daemon");
    let token = read_token(&home);
    let client = reqwest::Client::new();
    let base = format!("http://{}", running.addr);
    // No `project` field: the write lands in the home ledger.
    let home_tx_dir = home.tx();

    let ok = post_tx(
        &client,
        &base,
        &token,
        serde_json::json!({
            "type": "manager.action",
            "reason": "Home ledger single-line reason.",
        }),
    )
    .await;
    assert_eq!(
        ok.status(),
        reqwest::StatusCode::OK,
        "single-line reason must still be accepted: {}",
        ok.text().await.unwrap()
    );
    assert_ledger_parses(&home_tx_dir, "home");
    let before = snapshot(&home_tx_dir);
    assert!(!before.is_empty(), "the home ledger should exist by now");

    let resp = post_tx(
        &client,
        &base,
        &token,
        serde_json::json!({
            "type": "manager.action",
            "reason": MULTI_LINE_REASON,
        }),
    )
    .await;

    let status = resp.status();
    let text = resp.text().await.unwrap();
    assert_eq!(
        status,
        reqwest::StatusCode::BAD_REQUEST,
        "a multi-line reason must be refused on the home ledger too: {text}"
    );
    assert_names_the_constraint(&text, "REASON");
    common::assert_body_rejects_paths(&text, &[&project_root, &home.root]);

    assert_eq!(
        snapshot(&home_tx_dir),
        before,
        "a refused tx write must leave the home ledger byte-identical"
    );
    assert_ledger_parses(&home_tx_dir, "home");

    let _ = running.shutdown.send(());
    let _ = running.join.await;
}

// ---------------------------------------------------------------------------
// Every supported write leaves a ledger that still parses
// ---------------------------------------------------------------------------

#[tokio::test]
async fn ledger_still_parses_after_every_supported_write() {
    let tmp = tempfile::tempdir().unwrap();
    let home = Home::at(tmp.path().join("home"));
    home.ensure().unwrap();
    let project_root = tmp.path().join("proj");
    seed_project(&home, &project_root, "txguardshapes");

    let running = Daemon::run(home.clone(), test_options())
        .await
        .expect("boot daemon");
    let token = read_token(&home);
    let client = reqwest::Client::new();
    let base = format!("http://{}", running.addr);
    let project_tx_dir = || resolve_tx_dir(&project_root);
    let node_journal = project_root.join(".orgasmic/tasks/TASK-T01/journal.org");

    // The value shapes a tx entry is expected to carry, one write each. The
    // ledger is re-parsed after every one of them, not just at the end.
    let writes = [
        serde_json::json!({
            "type": "manager.action",
            "project": "txguardshapes",
            "reason": "A plain single-line reason.",
        }),
        serde_json::json!({
            "type": "task.state_transitioned",
            "project": "txguardshapes",
            "task": "TASK-T01",
            "extra": [["FROM_STATE", "ready"], ["TO_STATE", "in_progress"]],
        }),
        serde_json::json!({
            "type": "manager.action",
            "project": "txguardshapes",
            "reason": "",
        }),
        serde_json::json!({
            "type": "manager.action",
            "project": "txguardshapes",
            "reason": "Ünïcödé, em-dashes — colons: brackets [2026-07-28 Tue], and `code`.",
        }),
        serde_json::json!({
            "type": "manager.action",
            "project": "txguardshapes",
            "reason": "before\tafter (a tab survives a drawer line)",
        }),
        serde_json::json!({
            "type": "manager.action",
            "project": "txguardshapes",
            "target": ".orgasmic/tasks/backlog.org",
            "reason": "A".repeat(400),
        }),
        serde_json::json!({
            "type": "comment",
            "project": "txguardshapes",
            "task": "TASK-T01",
            // How multi-paragraph prose is carried today: escaped into a
            // property by the comment surface, never as raw newlines.
            "extra": [["BODY", "first paragraph\\n\\nsecond paragraph"]],
        }),
    ];

    for (i, body) in writes.iter().enumerate() {
        let resp = post_tx(&client, &base, &token, body.clone()).await;
        let status = resp.status();
        let text = resp.text().await.unwrap();
        assert_eq!(
            status,
            reqwest::StatusCode::OK,
            "supported write #{i} must be accepted: {text}"
        );
        assert_ledger_parses(&project_tx_dir(), "project");
        assert_journal_parses(&node_journal, "node journal");
    }

    // And the whole ledger reads back as the number of entries written. It is
    // spread over two shapes now: dec_E01MC routes a task-scoped write to that
    // node's `journal.org`, and TASK-MSYN4 splits the rest into per-machine
    // month files. The read-back is the union of both, not one file.
    let mut count = assert_journal_parses(&node_journal, "node journal").len();
    let mut read_back = Vec::new();
    for file in org_files(&project_tx_dir()) {
        let source = std::fs::read_to_string(&file).unwrap();
        let entries = orgasmic_core::parse_tx_file(&source, "ledger").unwrap();
        count += entries.len();
        read_back.push(source);
    }
    assert_eq!(
        count,
        writes.len(),
        "every supported write must be readable back: {}",
        read_back.concat()
    );

    let _ = running.shutdown.send(());
    let _ = running.join.await;
}

// ---------------------------------------------------------------------------
// `--extra KEY=VALUE` carries the same hazard
// ---------------------------------------------------------------------------

#[tokio::test]
async fn multi_line_extra_value_is_refused_and_nothing_is_written() {
    let tmp = tempfile::tempdir().unwrap();
    let home = Home::at(tmp.path().join("home"));
    home.ensure().unwrap();
    let project_root = tmp.path().join("proj");
    seed_project(&home, &project_root, "txguardextra");

    let running = Daemon::run(home.clone(), test_options())
        .await
        .expect("boot daemon");
    let token = read_token(&home);
    let client = reqwest::Client::new();
    let base = format!("http://{}", running.addr);
    let project_tx_dir = || resolve_tx_dir(&project_root);

    let ok = post_tx(
        &client,
        &base,
        &token,
        serde_json::json!({
            "type": "manager.action",
            "project": "txguardextra",
            "reason": "Seed entry.",
        }),
    )
    .await;
    assert_eq!(ok.status(), reqwest::StatusCode::OK);
    let before = snapshot(&project_tx_dir());

    let resp = post_tx(
        &client,
        &base,
        &token,
        serde_json::json!({
            "type": "manager.action",
            "project": "txguardextra",
            "reason": "Single line.",
            "extra": [["NOTE", "first line\nsecond line"]],
        }),
    )
    .await;

    let status = resp.status();
    let text = resp.text().await.unwrap();
    assert_eq!(
        status,
        reqwest::StatusCode::BAD_REQUEST,
        "a multi-line extra value must be refused: {text}"
    );
    assert_names_the_constraint(&text, "NOTE");
    common::assert_body_rejects_paths(&text, &[&project_root]);

    assert_eq!(
        snapshot(&project_tx_dir()),
        before,
        "a refused tx write must leave the ledger byte-identical"
    );
    assert_ledger_parses(&project_tx_dir(), "project");

    let _ = running.shutdown.send(());
    let _ = running.join.await;
}
