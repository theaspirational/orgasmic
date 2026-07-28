// orgasmic:task_XPYRR
//! Production-path proof for TASK-XPYRR (a task title must be correctable):
//! the real `orgasmic` binary, against a real daemon, over a scaffolded
//! project whose task is shaped like the incident — TASK-0RCRY, filed with a
//! diagnosis that was retracted the same day and a heading that could not be
//! rewritten.
//!
//! What is pinned here:
//!
//! - `task update <id> --title "…"` rewrites the heading and records a tx.
//! - Everything else the heading line carries survives byte-exact: the
//!   lifecycle TODO keyword, the id token, and the org tags. The drawer
//!   (PRIORITY, WRITE_SCOPE) and the body sections survive too.
//! - A title that Org cannot store verbatim is refused with a reason and
//!   nothing lands — a silently corrupted heading is worse than a refusal.
//!   The refusal is positional, not lexical: `…:retracted:` at the end of an
//!   *untagged* heading would be re-read as a tag, while the same text on a
//!   tagged heading round-trips, because the tag run is anchored to the end
//!   of the line.

use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::time::Duration;

use orgasmic_core::Home;
use orgasmic_daemon::{Daemon, RunningDaemon};

mod common;

use common::{init_git_repo, orgasmic_exe, run_git, test_options};

/// The heading TASK-0RCRY is stuck with, and the correction it needs.
const RETRACTED_TITLE: &str =
    "Workers inherit the daemon's $TMUX, so tmux tests talk to the rmux server and six gated tests never run";
const CORRECTED_TITLE: &str =
    "No tmux call site passes -L/-S, so every probe and test session shares one server — on a dev box, the operator's own";

const TAGS: [&str; 4] = ["daemon", "drivers", "tmux", "testing"];

/// A body with `===` sub-headings, the shape the CLI can write (see
/// TASK-ZYWZD): if a title edit went through the body writer it would land
/// here, so it is the loudest place for loss to show up.
const BODY: &str = "** Description\n\
     **This task was filed on 2026-07-28 with a wrong diagnosis and rewritten the same\n\
     day.** The original claim is false and is recorded as such in\n\
     tx-20260728-orgasmic-3226. What survives is a narrower, measured problem.\n\n\
     === What was measured\n\n\
     No tmux call site passes `-L` or `-S`, so every session lands on the default\n\
     server for the invoking environment.\n\n\
     === Ask\n\n\
     Pass an explicit socket name at every call site.\n";

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
    Command::new(orgasmic_exe())
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

fn run_cli_json(
    home: &Home,
    running: &RunningDaemon,
    project_root: &Path,
    args: &[&str],
) -> serde_json::Value {
    let stdout = run_cli(home, running, project_root, args);
    serde_json::from_str(&stdout).unwrap_or_else(|e| panic!("orgasmic {args:?} json: {e}\n{stdout}"))
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

/// The one heading line carrying `task_id`, read straight off disk. The
/// assertions below are about the *line*, not the daemon's view of it.
fn heading_line(project_root: &Path, stage_file: &str, task_id: &str) -> String {
    let path = project_root.join(".orgasmic").join("tasks").join(stage_file);
    let text = std::fs::read_to_string(&path)
        .unwrap_or_else(|e| panic!("read {}: {e}", path.display()));
    text.lines()
        .find(|line| line.starts_with('*') && line.contains(task_id))
        .unwrap_or_else(|| panic!("no heading for {task_id} in {}\n{text}", path.display()))
        .to_string()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn task_update_title_rewrites_the_heading_and_keeps_everything_else_on_it() {
    let tmp = tempfile::tempdir().unwrap();
    let home = Home::at(tmp.path().join("home"));
    seed_source(&home);
    let running = boot(home.clone()).await;

    let project_root = tmp.path().join("repo");
    init_git_repo(&project_root);
    let project_id = "task-title-edit";
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

    // A fixture shaped like TASK-0RCRY: four tags, a P1 drawer, a WRITE_SCOPE
    // property, a sectioned body — and a title that has since been retracted.
    let created = run_cli_json(
        &home,
        &running,
        &project_root,
        &[
            "task",
            "create",
            "--project",
            project_id,
            "--title",
            RETRACTED_TITLE,
            "--tag",
            TAGS[0],
            "--tag",
            TAGS[1],
            "--tag",
            TAGS[2],
            "--tag",
            TAGS[3],
            "--property",
            "PRIORITY=P1",
            "--property",
            "WRITE_SCOPE=crates/orgasmic-drivers/**",
            "--body",
            BODY,
        ],
    );
    let task_id = created["id"].as_str().expect("minted id").to_string();

    // Move it off the default keyword so the lifecycle state has something to
    // lose: a title write that touched the TODO keyword would corrupt the
    // board far worse than a stale title.
    run_cli(
        &home,
        &running,
        &project_root,
        &[
            "task",
            "update",
            &task_id,
            "--project",
            project_id,
            "--state",
            "in_progress",
        ],
    );

    let before = run_cli_json(
        &home,
        &running,
        &project_root,
        &["task", "get", &task_id, "--project", project_id],
    );
    assert_eq!(before["title"].as_str().unwrap(), RETRACTED_TITLE);
    let before_body = before["body"].clone();
    let before_line = heading_line(&project_root, "in_progress.org", &task_id);
    assert!(
        before_line.contains(":daemon:drivers:tmux:testing:"),
        "fixture must carry tags: {before_line}"
    );

    // --- THE FIX: the title is correctable through the task surface. ---
    let updated = run_cli_json(
        &home,
        &running,
        &project_root,
        &[
            "task",
            "update",
            &task_id,
            "--project",
            project_id,
            "--title",
            CORRECTED_TITLE,
        ],
    );
    assert!(
        updated["tx_id"].as_str().is_some_and(|id| !id.is_empty()),
        "a title edit must record a tx: {updated}"
    );

    // The heading line, byte-exact. Every component it encodes is named here,
    // so a write that dropped any one of them cannot pass.
    let line = heading_line(&project_root, "in_progress.org", &task_id);
    assert_eq!(
        line,
        format!("* IN_PROGRESS {task_id} {CORRECTED_TITLE}    :daemon:drivers:tmux:testing:"),
        "heading must keep its level, TODO keyword, id token and tags"
    );

    let after = run_cli_json(
        &home,
        &running,
        &project_root,
        &["task", "get", &task_id, "--project", project_id],
    );
    assert_eq!(
        after["title"].as_str().unwrap(),
        CORRECTED_TITLE,
        "title must read back corrected"
    );
    let tags: Vec<&str> = after["tags"]
        .as_array()
        .expect("tags array")
        .iter()
        .map(|t| t.as_str().unwrap())
        .collect();
    assert_eq!(tags, TAGS, "tags must survive a title edit");
    assert_eq!(
        after["priority"].as_str().unwrap(),
        "P1",
        "drawer PRIORITY must survive a title edit"
    );
    assert_eq!(
        after["lifecycle_stage"].as_str().unwrap(),
        "in_progress",
        "a title edit must not touch the lifecycle state"
    );
    assert_eq!(
        after["write_scope"], before["write_scope"],
        "drawer WRITE_SCOPE must survive a title edit"
    );
    assert_eq!(
        after["body"], before_body,
        "the body and every named section must survive a title edit"
    );

    // --- REFUSAL: a title Org cannot store verbatim never lands. ---
    // On this tagged heading the trailing `:retracted:` is not at the end of
    // the line, so it round-trips; the refusal below is on an untagged one.
    let untagged = run_cli_json(
        &home,
        &running,
        &project_root,
        &[
            "task",
            "create",
            "--project",
            project_id,
            "--title",
            "Untagged subject",
            "--body",
            "** Description\nPlaceholder.\n",
        ],
    );
    let untagged_id = untagged["id"].as_str().expect("minted id").to_string();
    let untagged_before = heading_line(&project_root, "backlog.org", &untagged_id);

    // Each refusal must name its own cause: an operator who cannot tell which
    // character was the problem cannot rephrase the title.
    for (bad_title, why) in [
        (
            "correct the diagnosis :retracted:",
            "does not read back as written",
        ),
        (
            "correct the diagnosis\n* and inject a heading",
            "must be a single line",
        ),
        ("   ", "must not be empty"),
    ] {
        let stderr = run_cli_failure(
            &home,
            &running,
            &project_root,
            &[
                "task",
                "update",
                &untagged_id,
                "--project",
                project_id,
                "--title",
                bad_title,
            ],
        );
        assert!(
            stderr.contains("title") && stderr.contains(why),
            "refusal must name the title and why it was refused ({why}): {stderr}"
        );
        assert_eq!(
            heading_line(&project_root, "backlog.org", &untagged_id),
            untagged_before,
            "a refused title edit must not have partially landed ({why})"
        );
    }

    // The same trailing-colon text is accepted where Org can store it: the
    // tagged heading's tag run is what anchors the end of the line.
    run_cli(
        &home,
        &running,
        &project_root,
        &[
            "task",
            "update",
            &task_id,
            "--project",
            project_id,
            "--title",
            "correct the diagnosis :retracted:",
        ],
    );
    assert_eq!(
        heading_line(&project_root, "in_progress.org", &task_id),
        format!(
            "* IN_PROGRESS {task_id} correct the diagnosis :retracted:    :daemon:drivers:tmux:testing:"
        ),
        "a trailing `:tag:` shape round-trips when the tag run still anchors the line end"
    );

    let _ = running.shutdown.send(());
    let _ = running.join.await;
}
