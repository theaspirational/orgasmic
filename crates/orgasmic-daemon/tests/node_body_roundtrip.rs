// orgasmic:task_ZYWZD
//! Round-trip contract for the node body write/read surface (TASK-ZYWZD).
//!
//! A writer must not commit what it cannot read back. `node body set` /
//! `node body append` replace the free prose between a drawer and the first
//! nested heading, so a submitted body carrying `**` sub-headings can never
//! round-trip through that span. The contract these tests pin:
//!
//! - WRITE: such a body is REFUSED, and the refusal names how many nested
//!   headings were found and quotes the first one (never a silent partial
//!   write, never a bare "org file update failed").
//! - WRITE: the same body written with `===` sub-headings round-trips with
//!   zero loss (the ~4000-char TASK-ATAXN shape is the fixture).
//! - READ: `task get` exposes nested `**` sections in `description` instead of
//!   silently presenting the free prose as the whole body.
//!
//! Shares its principle with TASK-HQ970 (re-parse before reporting success on
//! `tx record`).

mod common;

use std::path::Path;

use orgasmic_core::Home;
use orgasmic_daemon::{Daemon, DaemonOptions};

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

/// The TASK-ATAXN shape: free prose, then three sub-headings with substantial
/// bodies. ~4000 characters, of which the free prose before the first
/// sub-heading is ~300 — the 92% that went missing.
fn ataxn_shaped_body(marker: &str) -> String {
    let mut body = String::new();
    body.push_str(
        "Not a duplicate of TASK-870YX, and not TASK-RRVX0 (cancelled as superseded).\n\
         870YX sized `DAEMON_LOCK_RETRY_BUDGET` at 125 ms with 10 ms steps, and its\n\
         reviewer confirmed that is correct for the case it was filed about: a\n\
         transient CLI lock probe that holds the lock for microseconds. This task is\n\
         about a different holder with a budget three orders of magnitude larger.\n\n",
    );
    let sections = [
        (
            "The mismatch, made concrete by TASK-Q07Y5",
            "`orgasmic-daemon` holds its home instance lock until `graceful_shutdown` returns.\n\
             Q07Y5 established what that can cost: connection drain 10 s plus release\n\
             finalization drain 20 s plus writer shutdown 10 s is a 40 s total, and a writer\n\
             blocked in a syscall can push the process to the far end of it.\n\n\
             So the same lock has two classes of holder: a transient CLI probe that holds it\n\
             for microseconds, for which a 125 ms retry budget is correct today, and a daemon\n\
             in graceful shutdown that can hold it for up to 40 s, for which 125 ms fails\n\
             immediately. A restart whose predecessor is mid-shutdown therefore fails its\n\
             replacement start with \"instance lock is held\", even though the predecessor is\n\
             shutting down exactly as designed and the lock will be free shortly. The restart\n\
             is reported as a failure and the machine is left with no daemon at all.\n\n\
             The two holders are not distinguishable from the acquirer's side: the lock file\n\
             carries a pid and a boot id, and both are as live for a shutting-down daemon as\n\
             for a serving one. Only the wait tells them apart, which is why the wait, not\n\
             the inspection, is where this has to be fixed.\n",
        ),
        (
            "Ask",
            "Size the acquisition wait for the worst legitimate holder, not the cheapest one.\n\
             Derive the budget from `ShutdownBudgets::default().total()` so the two numbers\n\
             cannot drift apart: if the shutdown budget grows, the retry budget grows with it,\n\
             and the derivation is asserted by a test rather than restated as a constant.\n\n\
             Keep the fast path fast. A lock free on the first probe must still return in\n\
             microseconds; only a genuinely held lock pays the long wait, and it must report\n\
             progress rather than blocking silently for forty seconds. Distinguish the two\n\
             failure shapes in the message the operator sees: a lock held by a live daemon is\n\
             a different diagnosis from a lock held by a shutting-down one, and conflating\n\
             them is what made the original outage take an hour to read correctly.\n\n\
             Report progress while waiting. Forty seconds of silence on a foreground `daemon\n\
             start` is indistinguishable from a hang, and the operator's next move — another\n\
             start, or a kill — is the one move that makes the situation worse. One line per\n\
             retry window naming the holder pid and the remaining budget is enough, and it\n\
             costs nothing on the fast path because the fast path never reaches it.\n\n\
             Finally, make the budget observable. Whatever number the derivation produces\n\
             should appear in `daemon status` and in the start log, so a future outage can be\n\
             read from the artifacts rather than from the source.\n",
        ),
        (
            "Acceptance and non-goals",
            "Acceptance: a start against a predecessor in graceful shutdown succeeds once the\n\
             predecessor exits, proven by a test that holds the lock for longer than the old\n\
             125 ms budget; the retry budget is derived from the shutdown budget rather than\n\
             hardcoded, proven by a test that moves the shutdown budget and observes the\n\
             retry budget follow; and the operator-facing message names which holder class\n\
             was observed.\n\n\
             Non-goals: do not change `ShutdownBudgets` itself here, do not touch the\n\
             LaunchAgent `ExitTimeOut` derivation from R74E8 (this task consumes that\n\
             derivation, it does not change it), and do not add a force-unlock escape hatch —\n\
             a stuck lock is a bug to diagnose, not a flag to bypass.\n\n\
             Out of scope for the same reason: the supervisor's own restart backoff, the\n\
             LaunchAgent `KeepAlive` semantics, and the CLI autostart probe. Each of them\n\
             observes the lock, none of them owns its sizing, and widening the change to\n\
             cover them would put four surfaces in one review for a one-constant defect.\n\
             File follow-ups instead if the derived budget turns out to be wrong for them.\n",
        ),
    ];
    for (title, text) in sections {
        body.push_str(marker);
        body.push(' ');
        body.push_str(title);
        body.push('\n');
        body.push_str(text);
        body.push('\n');
    }
    body
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
        &project_root.join(".orgasmic/tasks/backlog.org"),
        "#+title: sprint\n#+orgasmic_version: 1\n\n\
         * BACKLOG TASK-R01 Round-trip test task :work:\n\
         :PROPERTIES:\n\
         :ID:               TASK-R01\n\
         :END:\n\n\
         ** Description\nOriginal description.\n\n\
         ** Acceptance Criteria\n- [ ] Item.\n",
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

async fn base_version(client: &reqwest::Client, base: &str, token: &str, project: &str) -> String {
    let doc: serde_json::Value = client
        .get(format!("{base}/api/org/node"))
        .bearer_auth(token)
        .query(&[("project", project), ("id", "TASK-R01"), ("kind", "task")])
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    doc["source"]["base_version"].as_str().unwrap().to_string()
}

async fn post_edit(
    client: &reqwest::Client,
    base: &str,
    token: &str,
    project: &str,
    request_id: &str,
    ops: serde_json::Value,
) -> reqwest::Response {
    let base_version = base_version(client, base, token, project).await;
    client
        .post(format!("{base}/api/org/node/TASK-R01/edit"))
        .bearer_auth(token)
        .json(&serde_json::json!({
            "project": project,
            "kind": "task",
            "base_version": base_version,
            "request_id": request_id,
            "ops": ops,
        }))
        .send()
        .await
        .unwrap()
}

/// The refusal must name the loss: how many nested headings, and the first one
/// verbatim. "org file update failed" is exactly the silence this task exists
/// to remove.
fn assert_names_the_loss(message: &str, first_heading: &str, count: usize) {
    assert!(
        message.contains(first_heading),
        "refusal must quote the first offending heading {first_heading:?}: {message}"
    );
    assert!(
        message.contains(&count.to_string()),
        "refusal must name how many nested headings ({count}) would be lost: {message}"
    );
    assert!(
        message.contains("==="),
        "refusal must point at the supported alternative (`===` sub-headings): {message}"
    );
}

// ---------------------------------------------------------------------------
// orgasmic:TASK-CB6GQ
// WRITE: a section write edits; only `--create` creates; `unset` removes
// ---------------------------------------------------------------------------

/// A write to a section that does not exist is refused by name, listing the
/// sections the node does have.
///
/// It used to append one instead — `set_section_body` and `add_section` shared
/// a code path through `upsert_section_text` — so a mistyped title minted a
/// permanent heading and said nothing. The listing is the point: the typo is
/// obvious the moment the real titles are next to it.
#[tokio::test]
async fn section_write_to_an_unknown_title_is_refused_and_names_what_exists() {
    let tmp = tempfile::tempdir().unwrap();
    let home = Home::at(tmp.path().join("home"));
    home.ensure().unwrap();
    let project_root = tmp.path().join("proj");
    seed_project(&home, &project_root, "proj-cb6gq");
    let running = Daemon::run(home.clone(), test_options()).await.unwrap();
    let token = read_token(&home);
    let client = reqwest::Client::new();
    let base = format!("http://{}", running.addr);

    let response = post_edit(
        &client,
        &base,
        &token,
        "proj-cb6gq",
        "cb6gq-typo",
        serde_json::json!([{
            "op": "set_section_body",
            "title": "Descrption",
            "body": "typo'd title.\n",
        }]),
    )
    .await;

    assert_eq!(response.status(), reqwest::StatusCode::BAD_REQUEST);
    let message = response.text().await.unwrap();
    assert!(
        message.contains("Descrption"),
        "the refusal must quote the title that was asked for: {message}"
    );
    assert!(
        message.contains("Description") && message.contains("Acceptance Criteria"),
        "the refusal must list the sections the node does have: {message}"
    );
    assert!(
        message.contains("--create"),
        "the refusal must name the flag that makes creation deliberate: {message}"
    );

    let on_disk =
        std::fs::read_to_string(project_root.join(".orgasmic/tasks/backlog.org")).unwrap();
    assert!(
        !on_disk.contains("Descrption"),
        "a refused section write must not have appended the heading: {on_disk}"
    );
}

/// `--create` adds a section and `unset` removes it, leaving the file byte
/// identical to before either ran.
#[tokio::test]
async fn create_then_unset_a_section_round_trips_byte_exactly() {
    let tmp = tempfile::tempdir().unwrap();
    let home = Home::at(tmp.path().join("home"));
    home.ensure().unwrap();
    let project_root = tmp.path().join("proj");
    seed_project(&home, &project_root, "proj-cb6gq");
    let backlog = project_root.join(".orgasmic/tasks/backlog.org");
    let before = std::fs::read_to_string(&backlog).unwrap();

    let running = Daemon::run(home.clone(), test_options()).await.unwrap();
    let token = read_token(&home);
    let client = reqwest::Client::new();
    let base = format!("http://{}", running.addr);

    let created = post_edit(
        &client,
        &base,
        &token,
        "proj-cb6gq",
        "cb6gq-create",
        serde_json::json!([{
            "op": "add_section",
            "title": "Evidence",
            "body": "Deliberately created.\n",
        }]),
    )
    .await;
    assert_eq!(created.status(), reqwest::StatusCode::OK);
    let with_section = std::fs::read_to_string(&backlog).unwrap();
    assert!(
        with_section.contains("** Evidence"),
        "--create must add the section: {with_section}"
    );

    let removed = post_edit(
        &client,
        &base,
        &token,
        "proj-cb6gq",
        "cb6gq-remove",
        serde_json::json!([{ "op": "remove_section", "title": "Evidence" }]),
    )
    .await;
    assert_eq!(removed.status(), reqwest::StatusCode::OK);

    let after = std::fs::read_to_string(&backlog).unwrap();
    assert_eq!(
        after, before,
        "create-then-remove must leave the file byte identical"
    );
}

/// Removing a section that is not there is refused, never a silent no-op — the
/// caller asked for a state change and has to learn it did not happen.
#[tokio::test]
async fn removing_an_absent_section_is_refused() {
    let tmp = tempfile::tempdir().unwrap();
    let home = Home::at(tmp.path().join("home"));
    home.ensure().unwrap();
    let project_root = tmp.path().join("proj");
    seed_project(&home, &project_root, "proj-cb6gq");
    let running = Daemon::run(home.clone(), test_options()).await.unwrap();
    let token = read_token(&home);
    let client = reqwest::Client::new();
    let base = format!("http://{}", running.addr);

    let response = post_edit(
        &client,
        &base,
        &token,
        "proj-cb6gq",
        "cb6gq-absent",
        serde_json::json!([{ "op": "remove_section", "title": "Nonexistent" }]),
    )
    .await;

    assert!(
        response.status().is_client_error() || response.status().is_server_error(),
        "removing an absent section must not report success"
    );
    let message = response.text().await.unwrap();
    assert!(
        message.contains("Nonexistent"),
        "the refusal must name the section asked for: {message}"
    );
}

// ---------------------------------------------------------------------------
// WRITE: nested `**` headings are refused, naming what would be lost
// ---------------------------------------------------------------------------

#[tokio::test]
async fn node_body_set_refuses_nested_headings_naming_the_first_one() {
    let tmp = tempfile::tempdir().unwrap();
    let home = Home::at(tmp.path().join("home"));
    home.ensure().unwrap();
    let project_root = tmp.path().join("proj");
    seed_project(&home, &project_root, "rtbodytest");

    let running = Daemon::run(home.clone(), test_options())
        .await
        .expect("boot daemon");
    let token = read_token(&home);
    let client = reqwest::Client::new();
    let base = format!("http://{}", running.addr);

    let body = ataxn_shaped_body("**");
    let resp = post_edit(
        &client,
        &base,
        &token,
        "rtbodytest",
        "rt-node-body-refuse",
        serde_json::json!([{ "op": "set_body", "body": body }]),
    )
    .await;

    let status = resp.status();
    let text = resp.text().await.unwrap();
    assert_eq!(
        status,
        reqwest::StatusCode::BAD_REQUEST,
        "a body with nested headings must be refused, never partially written: {text}"
    );
    assert_names_the_loss(&text, "** The mismatch, made concrete by TASK-Q07Y5", 3);
    common::assert_body_rejects_paths(&text, &[&project_root]);

    let on_disk =
        std::fs::read_to_string(project_root.join(".orgasmic/tasks/backlog.org")).unwrap();
    assert!(
        on_disk.contains("** Description\nOriginal description.\n"),
        "refused write must leave the file untouched: {on_disk}"
    );
    assert!(
        !on_disk.contains("Not a duplicate of TASK-870YX"),
        "no partial body may land after a refusal: {on_disk}"
    );

    let _ = running.shutdown.send(());
    let _ = running.join.await;
}

#[tokio::test]
async fn section_body_set_refuses_nested_headings_naming_the_first_one() {
    let tmp = tempfile::tempdir().unwrap();
    let home = Home::at(tmp.path().join("home"));
    home.ensure().unwrap();
    let project_root = tmp.path().join("proj");
    seed_project(&home, &project_root, "rtsectiontest");

    let running = Daemon::run(home.clone(), test_options())
        .await
        .expect("boot daemon");
    let token = read_token(&home);
    let client = reqwest::Client::new();
    let base = format!("http://{}", running.addr);

    let body = ataxn_shaped_body("**");
    let resp = post_edit(
        &client,
        &base,
        &token,
        "rtsectiontest",
        "rt-section-body-refuse",
        serde_json::json!([
            { "op": "set_section_body", "title": "Description", "body": body }
        ]),
    )
    .await;

    let status = resp.status();
    let text = resp.text().await.unwrap();
    assert_eq!(
        status,
        reqwest::StatusCode::BAD_REQUEST,
        "a section body with nested headings must be refused: {text}"
    );
    assert_names_the_loss(&text, "** The mismatch, made concrete by TASK-Q07Y5", 3);
    common::assert_body_rejects_paths(&text, &[&project_root]);

    let _ = running.shutdown.send(());
    let _ = running.join.await;
}

// ---------------------------------------------------------------------------
// WRITE: the same ~4000-char shape with `===` sub-headings round-trips whole
// ---------------------------------------------------------------------------

#[tokio::test]
async fn ataxn_shape_round_trips_through_set_and_get_with_zero_loss() {
    let tmp = tempfile::tempdir().unwrap();
    let home = Home::at(tmp.path().join("home"));
    home.ensure().unwrap();
    let project_root = tmp.path().join("proj");
    seed_project(&home, &project_root, "rtzerolosstest");

    let running = Daemon::run(home.clone(), test_options())
        .await
        .expect("boot daemon");
    let token = read_token(&home);
    let client = reqwest::Client::new();
    let base = format!("http://{}", running.addr);

    let body = ataxn_shaped_body("===");
    assert!(
        body.len() > 3500,
        "fixture must be the ~4000-char ATAXN shape, got {}",
        body.len()
    );

    let resp = post_edit(
        &client,
        &base,
        &token,
        "rtzerolosstest",
        "rt-zero-loss",
        serde_json::json!([
            { "op": "set_section_body", "title": "Description", "body": body }
        ]),
    )
    .await;
    let status = resp.status();
    let text = resp.text().await.unwrap();
    assert!(status.is_success(), "zero-loss write must succeed: {text}");

    // 1. Raw org node keeps every byte.
    let on_disk =
        std::fs::read_to_string(project_root.join(".orgasmic/tasks/backlog.org")).unwrap();
    assert!(
        on_disk.contains(body.trim_end()),
        "raw org node lost content: {on_disk}"
    );

    // 2. The node read surface returns it whole.
    let doc: serde_json::Value = client
        .get(format!("{base}/api/org/node"))
        .bearer_auth(&token)
        .query(&[
            ("project", "rtzerolosstest"),
            ("id", "TASK-R01"),
            ("kind", "task"),
        ])
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let section_body = doc["sections"]
        .as_array()
        .unwrap()
        .iter()
        .find(|s| s["title"] == "Description")
        .map(|s| s["body"].as_str().unwrap().to_string())
        .expect("Description section");
    assert_eq!(
        section_body.trim(),
        body.trim(),
        "node read surface truncated the body it just accepted"
    );

    // 3. `task get` returns it whole.
    let detail: serde_json::Value = client
        .get(format!("{base}/api/projects/rtzerolosstest/tasks/TASK-R01"))
        .bearer_auth(&token)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let description = detail["body"]["description"].as_str().unwrap();
    assert!(
        description.contains("=== Acceptance and non-goals"),
        "task get truncated the description it just accepted: {description}"
    );
    assert!(
        description.len() >= body.trim().len(),
        "task get lost {} of {} characters",
        body.trim().len().saturating_sub(description.len()),
        body.trim().len()
    );

    let _ = running.shutdown.send(());
    let _ = running.join.await;
}

// ---------------------------------------------------------------------------
// READ: nested `**` sections must not be hidden from `task get`
// ---------------------------------------------------------------------------

/// The ATAXN state as it actually exists on disk: a task heading whose body was
/// split into nested `**` sections by an earlier compose path. Reading it back
/// must not present the 300-char free prose as the whole description.
#[tokio::test]
async fn task_get_exposes_nested_sections_in_description() {
    let tmp = tempfile::tempdir().unwrap();
    let home = Home::at(tmp.path().join("home"));
    home.ensure().unwrap();
    let project_root = tmp.path().join("proj");
    seed_project(&home, &project_root, "rtreadtest");

    // Rewrite the task with the split-on-disk shape.
    let body = ataxn_shaped_body("**");
    write(
        &project_root.join(".orgasmic/tasks/backlog.org"),
        format!(
            "#+title: sprint\n#+orgasmic_version: 1\n\n\
             * BACKLOG TASK-R01 Round-trip test task :work:\n\
             :PROPERTIES:\n:ID:               TASK-R01\n:END:\n\n\
             {body}\n\
             ** Acceptance Criteria\n- [ ] Item.\n"
        ),
    );

    let running = Daemon::run(home.clone(), test_options())
        .await
        .expect("boot daemon");
    let token = read_token(&home);
    let client = reqwest::Client::new();
    let base = format!("http://{}", running.addr);

    let detail: serde_json::Value = client
        .get(format!("{base}/api/projects/rtreadtest/tasks/TASK-R01"))
        .bearer_auth(&token)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let description = detail["body"]["description"].as_str().unwrap();

    for heading in [
        "The mismatch, made concrete by TASK-Q07Y5",
        "Ask",
        "Acceptance and non-goals",
    ] {
        assert!(
            description.contains(heading),
            "description hid nested section {heading:?}: {description}"
        );
    }
    assert!(
        description.contains("a stuck lock is a bug to diagnose"),
        "description hid the body of the last nested section: {description}"
    );

    let _ = running.shutdown.send(());
    let _ = running.join.await;
}

/// How TASK-ATAXN actually reached that state: filed through `task create`,
/// whose `--body` is a whole org body, so its `**` sub-headings became real
/// sections. Nothing was lost on disk — `task get` hid it. File and read back
/// must agree.
#[tokio::test]
async fn task_created_with_nested_headings_reads_back_whole() {
    let tmp = tempfile::tempdir().unwrap();
    let home = Home::at(tmp.path().join("home"));
    home.ensure().unwrap();
    let project_root = tmp.path().join("proj");
    seed_project(&home, &project_root, "rtcreatetest");

    let running = Daemon::run(home.clone(), test_options())
        .await
        .expect("boot daemon");
    let token = read_token(&home);
    let client = reqwest::Client::new();
    let base = format!("http://{}", running.addr);

    let submitted = format!("** Description\n{}", ataxn_shaped_body("**"));
    let created: serde_json::Value = client
        .post(format!("{base}/api/projects/rtcreatetest/tasks"))
        .bearer_auth(&token)
        .json(&serde_json::json!({
            "title": "Filed with nested sub-headings",
            "body": submitted,
            "request_id": "rt-create-nested",
        }))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let task_id = created["id"].as_str().expect("minted task id");

    let detail: serde_json::Value = client
        .get(format!("{base}/api/projects/rtcreatetest/tasks/{task_id}"))
        .bearer_auth(&token)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let description = detail["body"]["description"].as_str().unwrap();

    for probe in [
        "Not a duplicate of TASK-870YX",
        "** The mismatch, made concrete by TASK-Q07Y5",
        "** Ask",
        "** Acceptance and non-goals",
        "a stuck lock is a bug to diagnose",
    ] {
        assert!(
            description.contains(probe),
            "task get lost {probe:?} from the filed body: {description}"
        );
    }

    let _ = running.shutdown.send(());
    let _ = running.join.await;
}
