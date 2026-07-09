//! Greenfield bootstrap end-to-end smoke (TASK-DHYXQ).
//!
//! Reproduces what the 2026-07-06 orsl battle-test did on the greenfield
//! path, CLI-only, against a running isolated test daemon: `project init` in
//! a temp git repo, then drive the shipped bootstrap the way an agent would
//! — decisions (daemon-minted ids), glossary refs, architecture edges, a
//! post-init task create/update, and an artifact submit. Every write goes
//! through the CLI; the only direct `.org` write is the initial scaffold from
//! `project init` itself.
//!
//! Pins three battle-test regressions:
//! - F1 (TASK-GERBB): a project registered after daemon boot must be usable
//!   with no restart (exercised by the post-init task create/update).
//! - F2 (TASK-4T80X): reference-valued properties must be daemon-minted ids,
//!   validated at write time, not free prose or invented sequential ids
//!   (exercised by the decision/glossary steps, plus a rejection probe).

use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::time::Duration;

use orgasmic_core::Home;
use orgasmic_daemon::{Daemon, RunningDaemon};

mod common;

use common::{init_git_repo, orgasmic_exe, run_git, test_options, write};

/// Serialize with the other daemon-booting cli tests (dispatch.rs) via the
/// same shared flock path, so this suite doesn't contend under `cargo test
/// --workspace`. Defined locally: an integration test binary cannot import
/// another integration test binary's private helpers.
fn live_session_guard() -> LiveSessionGuard {
    let path = std::env::temp_dir().join("orgasmic-live-session-tests.lock");
    let file = std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(&path)
        .expect("open live-session lock file");
    // MSRV 1.87: call fs2 explicitly — std's File::lock_exclusive (1.89) shadows it.
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
    // Home config port is a decoy (65533): every CLI call below sets
    // ORGASMIC_DAEMON_URL explicitly, so if that env var were ever dropped
    // the CLI fails obviously instead of silently hitting the real 4848
    // daemon (port-isolation gotcha).
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

/// Extract `"id"` from a JSON object printed on stdout (decision / glossary /
/// architecture create responses all share this shape).
fn extract_id(stdout: &str) -> String {
    let value: serde_json::Value =
        serde_json::from_str(stdout).unwrap_or_else(|e| panic!("stdout not JSON: {e}\n{stdout}"));
    value
        .get("id")
        .and_then(serde_json::Value::as_str)
        .unwrap_or_else(|| panic!("response has no id: {stdout}"))
        .to_string()
}

/// Poll a cheap, project-scoped read until it succeeds or the deadline
/// passes. The daemon loads a freshly board-registered project via its fs
/// watcher (debounced ~200ms, TASK-GERBB), not synchronously with
/// `project init`'s file writes, so the first post-init call can race it.
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

/// Locate the orgasmic repo root from `CARGO_MANIFEST_DIR` so the test can
/// point `home.source()` at the *real* shipped scaffold — this test drives
/// the scaffold's own instructions (dec_K1DR7-style daemon-minted decision
/// ids, `conventions/contributing.org`'s CLI-only guidance), so a generic
/// placeholder fixture would defeat the point.
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

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn greenfield_bootstrap_e2e_through_first_artifact() {
    let _live_guard = live_session_guard();

    let tmp = tempfile::tempdir().unwrap();
    let home = Home::at(tmp.path().join("home"));
    seed_source(&home);
    let running = boot(home.clone()).await;

    let project_root = tmp.path().join("repo");
    init_git_repo(&project_root);

    // --- project init: the one lawful direct-write step (the CLI scaffolds
    // .orgasmic/ itself); everything after this is a daemon-mediated write. ---
    let project_id = "bootstrap-smoke";
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
    assert!(
        project_root.join(".orgasmic/decisions.org").is_file(),
        "project init should scaffold decisions.org directly"
    );
    assert!(
        !project_root.join(".orgasmic/config.org").exists(),
        "project init must not scaffold config.org"
    );
    run_git(&project_root, &["add", "."]);
    run_git(&project_root, &["commit", "-m", "scaffold .orgasmic"]);

    // TASK-GERBB (F1): the project was registered on the board after the
    // daemon was already running; it must become usable with no restart.
    wait_for_project_loaded(&home, &running, &project_root, project_id);

    // --- decisions: daemon-minted ids, per the corrected scaffold text
    // (conventions/contributing.org: "Use `orgasmic decision create` ... for
    // genuinely new rationale") — never invent sequential ids. ---
    let dec_one_stdout = run_cli(
        &home,
        &running,
        &project_root,
        &[
            "decision",
            "create",
            "--project",
            project_id,
            "--title",
            "Adopt CLI-only bootstrap smoke",
            "--body",
            "Every scaffolded file must stay writable through the CLI.",
        ],
    );
    let dec_one = extract_id(&dec_one_stdout);
    assert!(
        dec_one.starts_with("dec_"),
        "decision create should mint a dec_* id, got {dec_one}"
    );

    let dec_two_stdout = run_cli(
        &home,
        &running,
        &project_root,
        &[
            "decision",
            "create",
            "--project",
            project_id,
            "--title",
            "Reference the first decision from later nodes",
            "--body",
            "Second decision so glossary/architecture refs below resolve.",
        ],
    );
    let dec_two = extract_id(&dec_two_stdout);
    assert!(dec_two.starts_with("dec_"));
    assert_ne!(
        dec_one, dec_two,
        "each decision create should mint a fresh id"
    );

    // --- glossary: --relates-to must take real minted ids; prose is
    // rejected at write time (TASK-4T80X, the F2 class). ---
    let bad_glossary = run_cli_output(
        &home,
        &running,
        &project_root,
        &[
            "glossary",
            "create",
            "--project",
            project_id,
            "--title",
            "bootstrap term (should be rejected)",
            "--definition",
            "A term whose --relates-to is free prose instead of an id.",
            "--relates-to",
            "some related concept",
        ],
    );
    assert!(
        !bad_glossary.status.success(),
        "glossary create with prose --relates-to should be rejected"
    );
    let bad_glossary_stderr = String::from_utf8_lossy(&bad_glossary.stderr);
    assert!(
        bad_glossary_stderr.contains("unresolvable id"),
        "rejection should name the unresolvable token: {bad_glossary_stderr}"
    );

    let term_stdout = run_cli(
        &home,
        &running,
        &project_root,
        &[
            "glossary",
            "create",
            "--project",
            project_id,
            "--title",
            "bootstrap term",
            "--definition",
            "A glossary term created through the CLI, relating to a real decision.",
            "--relates-to",
            &dec_one,
        ],
    );
    let term_id = extract_id(&term_stdout);
    assert!(term_id.starts_with("term_"), "got {term_id}");

    // --- architecture: typed edges + MOTIVATED_BY, refs must resolve. ---
    let arch_stdout = run_cli(
        &home,
        &running,
        &project_root,
        &[
            "architecture",
            "create",
            "--project",
            project_id,
            "--title",
            "bootstrap smoke test module",
            "--body",
            "Owns the greenfield bootstrap e2e smoke test.",
            "--property",
            &format!("MOTIVATED_BY={dec_two}"),
            "--property",
            &format!("RELATES_TO={term_id}"),
        ],
    );
    let arch_id = extract_id(&arch_stdout);
    assert!(arch_id.starts_with("arch_"), "got {arch_id}");

    // --- task: create + update, proving the post-boot write path TASK-GERBB
    // made work without a restart (F1). ---
    let task_stdout = run_cli(
        &home,
        &running,
        &project_root,
        &[
            "task",
            "create",
            "--project",
            project_id,
            "--title",
            "Bootstrap smoke follow-up",
        ],
    );
    let task_id = extract_id(&task_stdout);
    assert!(task_id.starts_with("TASK-"), "got {task_id}");
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
            "todo",
        ],
    );

    // --- artifact: mint a lawful id, then submit minimal MDX. ---
    let art_id_stdout = run_cli(
        &home,
        &running,
        &project_root,
        &["id", "mint", "--class", "artifact"],
    );
    let art_id = art_id_stdout.trim().to_string();
    assert!(art_id.starts_with("ART-"), "got {art_id}");
    let mdx_path = tmp.path().join("artifact.mdx");
    write(
        &mdx_path,
        "<RichText>Bootstrap smoke artifact.</RichText>\n",
    );
    run_cli(
        &home,
        &running,
        &project_root,
        &[
            "artifact",
            "submit",
            &art_id,
            "--project",
            project_id,
            "--file",
            mdx_path.to_str().unwrap(),
            "--title",
            "Bootstrap smoke artifact",
            "--subject-nodes",
            &arch_id,
        ],
    );

    // --- no silent-empty listings: every record just created must show up. ---
    let decision_list = run_cli(
        &home,
        &running,
        &project_root,
        &["decision", "list", "--project", project_id],
    );
    assert!(decision_list.contains(&dec_one));
    assert!(decision_list.contains(&dec_two));

    let task_list = run_cli(
        &home,
        &running,
        &project_root,
        &["tasks", "list", "--project", project_id],
    );
    assert!(
        task_list.contains(&task_id),
        "task list was silent-empty: {task_list}"
    );

    // --- final gate: the whole index must be clean, no dangling refs, no
    // parse errors — every scaffolded file stayed lawfully writable. ---
    let status_stdout = run_cli(&home, &running, &project_root, &["status"]);
    let status_value: serde_json::Value = serde_json::from_str(&status_stdout)
        .unwrap_or_else(|e| panic!("status stdout not JSON: {e}\n{status_stdout}"));
    assert_eq!(
        status_value
            .get("parse_errors")
            .and_then(serde_json::Value::as_u64),
        Some(0),
        "parse_errors should be 0, got: {status_stdout}"
    );

    let _ = running.shutdown.send(());
    let _ = running.join.await;
}
