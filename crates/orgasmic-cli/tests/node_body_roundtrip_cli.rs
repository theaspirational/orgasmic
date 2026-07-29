// orgasmic:task_ZYWZD
//! Production-path proof for TASK-ZYWZD: the real `orgasmic` binary, against a
//! real daemon, over a scaffolded project.
//!
//! The unit and daemon-integration tests pin the HTTP contract; this pins what
//! an agent at a terminal actually sees:
//!
//! - `node body set --body '<body with ** headings>'` exits non-zero and the
//!   message names how many nested headings were found, quotes the first, and
//!   points at `===` — instead of "org file update failed" or, worse, a
//!   success line over a 92% truncated write.
//! - The same body with `===` sub-headings survives `set` → `task get` with
//!   zero loss.
//! - `node body append --section` refuses a section that has sub-sections,
//!   naming them, rather than appending above them.

use std::path::{Path, PathBuf};
use std::process::Output;
use std::time::Duration;

use orgasmic_core::Home;
use orgasmic_daemon::{Daemon, RunningDaemon};

mod common;

use common::{init_git_repo, orgasmic_command, run_git, test_options};

use orgasmic_drivers::modes::rmux::test_tooling::live_session_guard;

/// The TASK-ATAXN shape: ~300 characters of free prose then three sub-headings
/// carrying the other ~92%.
fn ataxn_shaped_body(marker: &str) -> String {
    let mut body = String::from(
        "Not a duplicate of TASK-870YX, and not TASK-RRVX0 (cancelled as superseded).\n\
         870YX sized the lock retry budget at 125 ms with 10 ms steps, and its reviewer\n\
         confirmed that is correct for the case it was filed about: a transient CLI probe\n\
         that holds the lock for microseconds. This task is about a different holder.\n\n",
    );
    for (title, text) in [
        (
            "The mismatch, made concrete by TASK-Q07Y5",
            "The daemon holds its home instance lock until graceful shutdown returns, and\n\
             that can cost 40 s: connection drain 10, release finalization drain 20, writer\n\
             shutdown 10. So a restart whose predecessor is mid-shutdown fails its\n\
             replacement start with \"instance lock is held\" and leaves no daemon at all.\n",
        ),
        (
            "Ask",
            "Size the acquisition wait for the worst legitimate holder, not the cheapest.\n\
             Derive the budget from the shutdown budget so the two cannot drift apart, keep\n\
             the fast path fast, and report progress while waiting rather than going silent\n\
             for forty seconds — silence is what makes an operator kill the process.\n",
        ),
        (
            "Acceptance and non-goals",
            "Acceptance: a start against a predecessor in graceful shutdown succeeds once the\n\
             predecessor exits, and the retry budget is derived rather than hardcoded, both\n\
             proven by tests. Non-goals: no change to the shutdown budget itself, and no\n\
             force-unlock escape hatch — a stuck lock is a bug to diagnose, not to bypass.\n",
        ),
    ] {
        body.push_str(&format!("{marker} {title}\n{text}\n"));
    }
    body
}

async fn boot(home: Home) -> RunningDaemon {
    home.ensure().unwrap();
    Daemon::run(home, test_options())
        .await
        .expect("boot daemon")
}

fn run_cli_output(
    home: &Home,
    running: &RunningDaemon,
    project_root: &Path,
    args: &[&str],
) -> Output {
    orgasmic_command()
        .args(args)
        .current_dir(project_root)
        .env("ORGASMIC_HOME", &home.root)
        .env("ORGASMIC_DAEMON_URL", format!("http://{}", running.addr))
        .output()
        .expect("run orgasmic")
}

fn run_cli(home: &Home, running: &RunningDaemon, project_root: &Path, args: &[&str]) -> String {
    let output = run_cli_output(home, running, project_root, args);
    assert!(
        output.status.success(),
        "orgasmic {:?} failed\nstdout={}\nstderr={}",
        args,
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8_lossy(&output.stdout).to_string()
}

/// stderr of a command that must fail.
fn run_cli_failure(
    home: &Home,
    running: &RunningDaemon,
    project_root: &Path,
    args: &[&str],
) -> String {
    let output = run_cli_output(home, running, project_root, args);
    assert!(
        !output.status.success(),
        "orgasmic {:?} must fail, not report success\nstdout={}",
        args,
        String::from_utf8_lossy(&output.stdout)
    );
    String::from_utf8_lossy(&output.stderr).to_string()
}

fn repo_root() -> PathBuf {
    let mut here = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    loop {
        if here.join(".orgasmic").is_dir() && here.join("shipped").is_dir() {
            return here;
        }
        if !here.pop() {
            panic!("could not locate orgasmic repo root from CARGO_MANIFEST_DIR");
        }
    }
}

fn seed_source(home: &Home) {
    home.ensure().unwrap();
    if !home.source().exists() {
        std::os::unix::fs::symlink(repo_root(), home.source()).unwrap();
    }
}

fn wait_for_project_loaded(
    home: &Home,
    running: &RunningDaemon,
    project_root: &Path,
    project_id: &str,
) {
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    loop {
        let output = run_cli_output(
            home,
            running,
            project_root,
            &["tasks", "list", "--project", project_id],
        );
        if output.status.success() {
            return;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "project {project_id} never loaded\nstdout={}\nstderr={}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        std::thread::sleep(Duration::from_millis(50));
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn node_body_verbs_round_trip_or_refuse_by_name() {
    let _live_guard = live_session_guard();

    let tmp = tempfile::tempdir().unwrap();
    let home = Home::at(tmp.path().join("home"));
    seed_source(&home);
    let running = boot(home.clone()).await;

    let project_root = tmp.path().join("repo");
    init_git_repo(&project_root);
    let project_id = "node-body-roundtrip";
    run_cli(
        &home,
        &running,
        &project_root,
        &[
            "project",
            "init",
            "--path",
            project_root.to_str().unwrap(),
            "--name",
            project_id,
        ],
    );
    run_git(&project_root, &["add", "."]);
    run_git(&project_root, &["commit", "-m", "scaffold .orgasmic"]);
    wait_for_project_loaded(&home, &running, &project_root, project_id);

    let created = run_cli(
        &home,
        &running,
        &project_root,
        &[
            "task",
            "create",
            "--project",
            project_id,
            "--title",
            "Round-trip subject",
            "--body",
            "** Description\nPlaceholder.\n",
        ],
    );
    let created: serde_json::Value = serde_json::from_str(&created).expect("task create json");
    let task_id = created["id"].as_str().expect("minted id").to_string();

    // --- WRITE: nested `**` headings are refused, by name. ---
    let nested = ataxn_shaped_body("**");
    let stderr = run_cli_failure(
        &home,
        &running,
        &project_root,
        &[
            "node",
            "body",
            "set",
            &task_id,
            "--project",
            project_id,
            "--kind",
            "task",
            "--section",
            "Description",
            "--body",
            &nested,
        ],
    );
    assert!(
        stderr.contains("** The mismatch, made concrete by TASK-Q07Y5"),
        "refusal must quote the first offending heading: {stderr}"
    );
    assert!(
        stderr.contains('3') && stderr.contains("nested"),
        "refusal must say how many nested headings: {stderr}"
    );
    assert!(
        stderr.contains("==="),
        "refusal must name the supported alternative: {stderr}"
    );

    // Nothing landed: the previous body is intact.
    let after_refusal = run_cli(
        &home,
        &running,
        &project_root,
        &["task", "get", &task_id, "--project", project_id],
    );
    let after_refusal: serde_json::Value =
        serde_json::from_str(&after_refusal).expect("task get json");
    assert_eq!(
        after_refusal["body"]["description"].as_str().unwrap(),
        "Placeholder.",
        "a refused write must not have partially landed"
    );

    // --- WRITE: the same shape with `===` sub-headings round-trips whole. ---
    let supported = ataxn_shaped_body("===");
    run_cli(
        &home,
        &running,
        &project_root,
        &[
            "node",
            "body",
            "set",
            &task_id,
            "--project",
            project_id,
            "--kind",
            "task",
            "--section",
            "Description",
            "--body",
            &supported,
        ],
    );
    let detail = run_cli(
        &home,
        &running,
        &project_root,
        &["task", "get", &task_id, "--project", project_id],
    );
    let detail: serde_json::Value = serde_json::from_str(&detail).expect("task get json");
    let description = detail["body"]["description"].as_str().unwrap();
    assert_eq!(
        description.trim(),
        supported.trim(),
        "set → get must be lossless"
    );

    // --- READ + APPEND: a section that carries sub-sections is never
    // presented (or appended to) as if its prose were the whole thing. ---
    let with_nested = run_cli(
        &home,
        &running,
        &project_root,
        &[
            "task",
            "create",
            "--project",
            project_id,
            "--title",
            "Filed with sub-headings",
            "--body",
            &format!("** Description\nLead prose.\n\n{}", ataxn_shaped_body("***")),
        ],
    );
    let with_nested: serde_json::Value =
        serde_json::from_str(&with_nested).expect("task create json");
    let nested_id = with_nested["id"].as_str().expect("minted id").to_string();

    let nested_detail = run_cli(
        &home,
        &running,
        &project_root,
        &["task", "get", &nested_id, "--project", project_id],
    );
    let nested_detail: serde_json::Value =
        serde_json::from_str(&nested_detail).expect("task get json");
    let nested_description = nested_detail["body"]["description"].as_str().unwrap();
    assert!(
        nested_description.contains("*** Ask")
            && nested_description.contains("a stuck lock is a bug to diagnose"),
        "task get hid the sub-sections of Description: {nested_description}"
    );

    let append_stderr = run_cli_failure(
        &home,
        &running,
        &project_root,
        &[
            "node",
            "body",
            "append",
            &nested_id,
            "--project",
            project_id,
            "--kind",
            "task",
            "--section",
            "Description",
            "--body",
            "One more line.",
        ],
    );
    assert!(
        append_stderr.contains("nested sub-section"),
        "append must refuse a section it cannot append to the end of: {append_stderr}"
    );
    assert!(
        append_stderr.contains("Ask"),
        "append refusal must name the sub-sections: {append_stderr}"
    );

    let _ = running.shutdown.send(());
    let _ = running.join.await;
}
