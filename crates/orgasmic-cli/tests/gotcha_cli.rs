// orgasmic:TASK-WPC6G
//! `orgasmic gotcha add` / `list` against a real daemon over a scaffolded
//! project: add then list shows the title, the file gained exactly one `**`
//! entry, the write left a tx, and a missing gotchas.org is refused by name.

use std::path::{Path, PathBuf};
use std::process::Output;
use std::time::Duration;

use orgasmic_core::Home;
use orgasmic_daemon::{Daemon, RunningDaemon};

mod common;

use common::{init_git_repo, orgasmic_command, run_git, test_options};

use orgasmic_drivers::test_tooling::live_session_guard;

fn repo_root() -> PathBuf {
    let mut here = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    loop {
        if here.join("shipped/entry/router.org").is_file() {
            return here;
        }
        if !here.pop() {
            panic!("could not locate orgasmic repo root from CARGO_MANIFEST_DIR");
        }
    }
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

fn count_entries(path: &Path) -> usize {
    std::fs::read_to_string(path)
        .unwrap()
        .lines()
        .filter(|line| line.starts_with("** "))
        .count()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn gotcha_add_then_list_round_trips_through_the_daemon() {
    let _live_guard = live_session_guard();

    let tmp = tempfile::tempdir().unwrap();
    let home = Home::at(tmp.path().join("home"));
    home.ensure().unwrap();
    if !home.source().exists() {
        std::os::unix::fs::symlink(repo_root(), home.source()).unwrap();
    }
    let running = Daemon::run(home.clone(), test_options())
        .await
        .expect("boot daemon");

    let project_root = tmp.path().join("repo");
    init_git_repo(&project_root);
    let project_id = "gotcha-roundtrip";
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
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    while !run_cli_output(
        &home,
        &running,
        &project_root,
        &["tasks", "list", "--project", project_id],
    )
    .status
    .success()
    {
        assert!(
            std::time::Instant::now() < deadline,
            "project {project_id} never loaded"
        );
        std::thread::sleep(Duration::from_millis(50));
    }

    let gotchas = project_root.join(".orgasmic/gotchas.org");
    let before = count_entries(&gotchas);

    let added = run_cli(
        &home,
        &running,
        &project_root,
        &[
            "gotcha",
            "add",
            "--project",
            project_id,
            "--title",
            "pgrep matches worker prompts",
            "--body",
            "Evidence: false daemon diagnosis.\nFix: match on the binary path.\n",
        ],
    );
    let added: serde_json::Value = serde_json::from_str(&added).expect("gotcha add json");
    let tx_id = added["tx_id"].as_str().expect("tx id").to_string();

    let listed = run_cli(
        &home,
        &running,
        &project_root,
        &["gotcha", "list", "--project", project_id],
    );
    assert!(
        listed
            .lines()
            .any(|line| line == "pgrep matches worker prompts"),
        "list must show the added title: {listed}"
    );
    assert_eq!(
        count_entries(&gotchas),
        before + 1,
        "exactly one new ** entry"
    );
    let text = std::fs::read_to_string(&gotchas).unwrap();
    assert!(
        text.contains(":CREATED: [") && text.contains("Fix: match on the binary path."),
        "entry carries a :CREATED: stamp and the body: {text}"
    );

    let txs = run_cli(
        &home,
        &running,
        &project_root,
        &["tx", "list", "--project", project_id, "--limit", "10"],
    );
    assert!(
        txs.contains(&tx_id) && txs.contains("org.file_rewritten"),
        "the write must leave a tx on the project ledger: {txs}"
    );

    // Same title again is refused by name; nothing lands.
    let stderr = run_cli_failure(
        &home,
        &running,
        &project_root,
        &[
            "gotcha",
            "add",
            "--project",
            project_id,
            "--title",
            "pgrep matches worker prompts",
            "--body",
            "dup",
        ],
    );
    assert!(stderr.contains("already exists"), "{stderr}");
    assert_eq!(count_entries(&gotchas), before + 1);

    // Missing file is refused, naming the scaffold.
    std::fs::remove_file(&gotchas).unwrap();
    let stderr = run_cli_failure(
        &home,
        &running,
        &project_root,
        &["gotcha", "list", "--project", project_id],
    );
    assert!(
        stderr.contains("gotchas.org is missing") && stderr.contains("project-scaffold"),
        "{stderr}"
    );
}
