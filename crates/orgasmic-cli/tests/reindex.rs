use std::path::Path;
use std::process::Output;

use orgasmic_core::Home;
use orgasmic_daemon::Daemon;

mod common;

use common::{orgasmic_command, test_options, write};

fn seed_project(home: &Home, project_root: &Path, project_id: &str) {
    write(
        &project_root.join(".orgasmic/project.org"),
        &format!(
            "#+title: {project_id}\n#+orgasmic_version: 1\n\n* PROJECT {project_id}\n:PROPERTIES:\n:ID:               {project_id}\n:END:\n"
        ),
    );
    write(
        &project_root.join(".orgasmic/tasks/backlog.org"),
        "#+title: backlog\n#+orgasmic_version: 1\n\n* BACKLOG TASK-PRE Pre-boot task :work:\n:PROPERTIES:\n:ID:               TASK-PRE\n:END:\n",
    );
    write(
        &home.board(),
        &format!(
            "#+title: orgasmic board\n#+orgasmic_version: 1\n\n* PROJECT {project_id}\n:PROPERTIES:\n:ID:               {project_id}\n:PATH:             {}\n:BRANCH:           main\n:STATUS:           active\n:END:\n",
            project_root.display()
        ),
    );
}

fn run_orgasmic(home: &Home, daemon_url: &str, args: &[&str]) -> Output {
    orgasmic_command()
        .args(args)
        .env("ORGASMIC_HOME", &home.root)
        .env("ORGASMIC_DAEMON_URL", daemon_url)
        .output()
        .unwrap_or_else(|e| panic!("run orgasmic {args:?}: {e}"))
}

/// Battle-test F5: `status --errors` must give project/file/node/property/
/// reason attribution for a dangling reference from the CLI alone — no
/// daemon-log grep — and `reindex --project` must clear the count after a
/// fix without a daemon restart.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn status_errors_attributes_dangling_ref_and_reindex_clears_it_after_fix() {
    let tmp = tempfile::tempdir().unwrap();
    let home = Home::at(tmp.path().join("home"));
    home.ensure().unwrap();
    let project_root = tmp.path().join("proj");
    seed_project(&home, &project_root, "orgasmic");
    let glossary_path = project_root.join(".orgasmic/glossary.org");
    write(
        &glossary_path,
        "#+title: glossary\n#+orgasmic_version: 1\n\n* term_A A term\n:PROPERTIES:\n:ID:               term_A\n:RELATES_TO:       missing-slug\n:END:\n",
    );

    let running = Daemon::run(home.clone(), test_options())
        .await
        .expect("boot daemon");
    let daemon_url = format!("http://{}", running.addr);

    let output = run_orgasmic(&home, &daemon_url, &["status", "--errors"]);
    let stdout = String::from_utf8_lossy(&output.stdout).to_string();
    assert!(
        output.status.success(),
        "status --errors failed\nstdout={stdout}\nstderr={}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(stdout.contains("orgasmic"), "project id missing: {stdout}");
    assert!(
        stdout.contains("glossary.org"),
        "file attribution missing: {stdout}"
    );
    assert!(
        stdout.contains("term_A"),
        "node attribution missing: {stdout}"
    );
    assert!(
        stdout.contains("RELATES_TO"),
        "property attribution missing: {stdout}"
    );
    assert!(stdout.contains("missing-slug"), "reason missing: {stdout}");

    // Fix the dangling reference on disk, then reindex just this project —
    // no daemon restart — and confirm the count drops to zero.
    write(
        &glossary_path,
        "#+title: glossary\n#+orgasmic_version: 1\n\n* term_A A term\n:PROPERTIES:\n:ID:               term_A\n:END:\n",
    );
    let output = run_orgasmic(&home, &daemon_url, &["reindex", "--project", "orgasmic"]);
    let stdout = String::from_utf8_lossy(&output.stdout).to_string();
    assert!(
        output.status.success(),
        "reindex failed\nstdout={stdout}\nstderr={}",
        String::from_utf8_lossy(&output.stderr)
    );
    let value: serde_json::Value = serde_json::from_str(&stdout).expect("reindex stdout is JSON");
    assert_eq!(value["projects"]["orgasmic"], 0, "{value}");

    let output = run_orgasmic(&home, &daemon_url, &["status", "--errors"]);
    let stdout = String::from_utf8_lossy(&output.stdout).to_string();
    assert!(
        stdout.contains("0 parse errors"),
        "expected a clean slate after reindex: {stdout}"
    );

    let _ = running.shutdown.send(());
    let _ = running.join.await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn partial_full_board_commands_keep_json_and_errors_but_exit_honestly() {
    let tmp = tempfile::tempdir().unwrap();
    let home = Home::at(tmp.path().join("home"));
    home.ensure().unwrap();
    let unreadable_root = tmp.path().join("unreadable");
    std::fs::create_dir_all(&unreadable_root).unwrap();
    let healthy_root = tmp.path().join("healthy");
    seed_project(&home, &healthy_root, "healthy");
    write(
        &healthy_root.join(".orgasmic/glossary.org"),
        "#+title: glossary\n#+orgasmic_version: 1\n\n* term_A A term\n:PROPERTIES:\n:ID: term_A\n:RELATES_TO: missing-slug\n:END:\n",
    );
    write(
        &home.board(),
        &format!(
            "#+title: board\n#+orgasmic_version: 1\n\n* PROJECT unreadable\n:PROPERTIES:\n:ID: unreadable\n:PATH: {}\n:BRANCH: main\n:STATUS: active\n:END:\n\n* PROJECT healthy\n:PROPERTIES:\n:ID: healthy\n:PATH: {}\n:BRANCH: main\n:STATUS: active\n:END:\n",
            unreadable_root.display(),
            healthy_root.display(),
        ),
    );

    let running = Daemon::run(home.clone(), test_options())
        .await
        .expect("boot daemon");
    let daemon_url = format!("http://{}", running.addr);

    let status = run_orgasmic(&home, &daemon_url, &["status", "--errors"]);
    let status_stdout = String::from_utf8_lossy(&status.stdout);
    let status_stderr = String::from_utf8_lossy(&status.stderr);
    assert!(status.status.success(), "{status_stdout}\n{status_stderr}");
    assert!(status_stdout.contains("missing-slug"), "{status_stdout}");
    assert!(
        status_stderr.contains("failed=[unreadable]"),
        "partial full-board status must name the unloaded project: {status_stderr}"
    );

    let reindex = run_orgasmic(&home, &daemon_url, &["reindex"]);
    let reindex_stdout = String::from_utf8_lossy(&reindex.stdout).to_string();
    let reindex_stderr = String::from_utf8_lossy(&reindex.stderr);
    assert!(
        !reindex.status.success(),
        "{reindex_stdout}\n{reindex_stderr}"
    );
    let value: serde_json::Value =
        serde_json::from_str(&reindex_stdout).expect("partial reindex stdout remains JSON");
    assert!(value["failures"]["unreadable"].is_string(), "{value}");
    assert_eq!(value["projects"]["healthy"], 1, "{value}");
    assert!(
        reindex_stderr.contains("whole-board reindex completed with project failures"),
        "{reindex_stderr}"
    );

    let _ = running.shutdown.send(());
    let _ = running.join.await;
}
