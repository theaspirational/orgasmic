use std::path::Path;
use std::process::{Command, Output};

use orgasmic_core::Home;
use orgasmic_daemon::Daemon;

mod common;

use common::{orgasmic_exe, test_options, write};

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
    Command::new(orgasmic_exe())
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
