//! F13/F14 (TASK-MTB56): list verbs never produce empty-and-silent output,
//! and `--ids` emits a stable one-id-per-line porcelain stream.
//!
//! Drives a freshly scaffolded project (via `project init`) against a real
//! daemon: `glossary`/`architecture` start with no `.org` file at all (a
//! genuinely empty list), while `decision`/`tasks` start pre-seeded by the
//! shipped scaffold (`dec_K1DR7`, `TASK-C9V29`) — covering both the empty
//! and non-empty paths for each list verb.

use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::time::Duration;

use orgasmic_core::Home;
use orgasmic_daemon::{Daemon, RunningDaemon};

mod common;

use common::{init_git_repo, orgasmic_exe, run_git, test_options};

fn live_session_guard() -> LiveSessionGuard {
    let path = std::env::temp_dir().join("orgasmic-live-session-tests.lock");
    let file = std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(&path)
        .expect("open live-session lock file");
    fs2::FileExt::lock_exclusive(&file).expect("flock live-session lock");
    LiveSessionGuard(file)
}

struct LiveSessionGuard(std::fs::File);
impl Drop for LiveSessionGuard {
    fn drop(&mut self) {
        let _ = fs2::FileExt::unlock(&self.0);
    }
}

async fn boot(home: Home) -> RunningDaemon {
    home.ensure().unwrap();
    std::fs::write(home.config(), "bind_host: 127.0.0.1\nbind_port: 65533\n").unwrap();
    Daemon::run(home, test_options())
        .await
        .expect("boot daemon")
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
        if std::time::Instant::now() >= deadline {
            panic!(
                "project {project_id} never loaded into the running daemon\nstdout={}\nstderr={}",
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr)
            );
        }
        std::thread::sleep(Duration::from_millis(50));
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn list_verbs_never_empty_and_silent_and_ids_mode_is_porcelain() {
    let _live_guard = live_session_guard();

    let tmp = tempfile::tempdir().unwrap();
    let home = Home::at(tmp.path().join("home"));
    seed_source(&home);
    let running = boot(home.clone()).await;

    let project_root = tmp.path().join("repo");
    init_git_repo(&project_root);
    let project_id = "list-output-smoke";
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

    // --- F13: genuinely empty lists (no glossary.org/architecture.org file
    // at all in a freshly scaffolded project) print a "0 <noun>" message,
    // never nothing. ---
    let empty_glossary = run_cli(
        &home,
        &running,
        &project_root,
        &["glossary", "list", "--project", project_id],
    );
    assert!(
        !empty_glossary.trim().is_empty(),
        "empty glossary list must not print nothing"
    );
    assert!(
        empty_glossary.contains("0 glossary terms"),
        "got: {empty_glossary}"
    );

    let empty_architecture = run_cli(
        &home,
        &running,
        &project_root,
        &["architecture", "list", "--project", project_id],
    );
    assert!(
        !empty_architecture.trim().is_empty(),
        "empty architecture list must not print nothing"
    );
    assert!(
        empty_architecture.contains("0 architecture nodes"),
        "got: {empty_architecture}"
    );

    // Same guarantee for a genuinely empty --ids stream: zero lines, but the
    // command still succeeds (exit 0) rather than erroring or hanging.
    let empty_glossary_ids = run_cli(
        &home,
        &running,
        &project_root,
        &["glossary", "list", "--project", project_id, "--ids"],
    );
    assert_eq!(empty_glossary_ids.trim(), "");

    // --- F13: non-empty lists (scaffold-seeded decisions/tasks) still show
    // every record — the battle-test's "9+ tasks, zero stdout" class. ---
    let decision_list = run_cli(
        &home,
        &running,
        &project_root,
        &["decision", "list", "--project", project_id],
    );
    assert!(
        decision_list.contains("dec_K1DR7"),
        "decision list dropped the scaffold-seeded decision: {decision_list}"
    );

    let task_list = run_cli(
        &home,
        &running,
        &project_root,
        &["tasks", "list", "--project", project_id],
    );
    assert!(
        task_list.contains("TASK-C9V29"),
        "task list was silent/incomplete: {task_list}"
    );

    // --- F14: --ids is porcelain — one id per line, nothing else. ---
    let decision_ids = run_cli(
        &home,
        &running,
        &project_root,
        &["decision", "list", "--project", project_id, "--ids"],
    );
    let decision_id_lines: Vec<&str> = decision_ids.lines().collect();
    assert_eq!(
        decision_id_lines,
        vec!["dec_K1DR7"],
        "decision --ids should emit exactly the node ids, one per line: {decision_ids:?}"
    );

    let task_ids = run_cli(
        &home,
        &running,
        &project_root,
        &["tasks", "list", "--project", project_id, "--ids"],
    );
    assert!(
        task_ids.lines().any(|line| line == "TASK-C9V29"),
        "task --ids should include TASK-C9V29: {task_ids:?}"
    );
    assert!(
        task_ids.lines().all(|line| !line.contains(' ')),
        "task --ids lines must be bare ids, not prose: {task_ids:?}"
    );

    // --- glossary --ids: only top-level node ids, never interleaved
    // RELATES_TO/related-term ids that would break naive grep counting. ---
    let glossary_stdout = run_cli(
        &home,
        &running,
        &project_root,
        &[
            "glossary",
            "create",
            "--project",
            project_id,
            "--title",
            "porcelain term",
            "--definition",
            "A term used to prove --ids porcelain output.",
            "--relates-to",
            "dec_K1DR7",
        ],
    );
    let glossary_value: serde_json::Value = serde_json::from_str(&glossary_stdout)
        .unwrap_or_else(|e| panic!("glossary create stdout not JSON: {e}\n{glossary_stdout}"));
    let glossary_id = glossary_value
        .get("id")
        .and_then(serde_json::Value::as_str)
        .expect("glossary create response has an id")
        .to_string();

    let glossary_ids = run_cli(
        &home,
        &running,
        &project_root,
        &["glossary", "list", "--project", project_id, "--ids"],
    );
    let glossary_id_lines: Vec<&str> = glossary_ids.lines().collect();
    assert_eq!(
        glossary_id_lines,
        vec![glossary_id.as_str()],
        "glossary --ids must emit only the term id, not its RELATES_TO ids: {glossary_ids:?}"
    );

    let _ = running.shutdown.send(());
    let _ = running.join.await;
}
