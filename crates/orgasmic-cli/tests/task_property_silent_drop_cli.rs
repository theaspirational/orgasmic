// orgasmic:task_HXSW0
//! Production-path proof for TASK-HXSW0's P1 core: no write verb silently
//! ignores an argument, in EITHER direction.
//!
//! The real `orgasmic` binary against a real daemon over a scaffolded project,
//! because every instance in the task body was measured at the terminal and
//! every one of them returned a normal success object. A daemon-level unit test
//! would have been green throughout — the defect never looked like breakage.
//!
//! What is pinned here, one test per measured instance:
//!
//! - `task create --property priority=P1` (the miscased key that filed seven P1
//!   tasks as unprioritised) is REFUSED and names `PRIORITY`.
//! - `task create --priority P1` exists at all, and lands `:PRIORITY:`.
//! - `task create --property PARENT_TASK=…` is refused and names the id grammar
//!   that actually carries parentage.
//! - `task create --property FIX_SUBTASK=…` — a key `update` writes and
//!   `create` used to swallow — is refused and names `task update`.
//! - `task update --state … --property …` is refused instead of returning
//!   `{"changed":{"STATE":…}}` with the properties dropped.
//! - `tx record` without `--project` lands on the PROJECT ledger, not the
//!   global home file.

use std::path::{Path, PathBuf};
use std::process::Output;
use std::time::Duration;

use orgasmic_core::Home;
use orgasmic_daemon::{Daemon, RunningDaemon};

mod common;

use common::{init_git_repo, orgasmic_command, run_git, test_options};

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

fn run_cli_json(
    home: &Home,
    running: &RunningDaemon,
    project_root: &Path,
    args: &[&str],
) -> serde_json::Value {
    let stdout = run_cli(home, running, project_root, args);
    serde_json::from_str(&stdout)
        .unwrap_or_else(|e| panic!("orgasmic {args:?} json: {e}\n{stdout}"))
}

/// stderr of a command that MUST fail. The assertion is the point: a silent
/// drop is exactly a command that succeeds while discarding an argument, so a
/// test that only inspected the message would pass on the broken tree.
fn run_cli_refusal(
    home: &Home,
    running: &RunningDaemon,
    project_root: &Path,
    args: &[&str],
) -> String {
    let output = run_cli_output(home, running, project_root, args);
    assert!(
        !output.status.success(),
        "orgasmic {:?} must be refused, not silently accepted\nstdout={}",
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

/// A booted daemon and a scaffolded, registered project inside a tempdir.
struct Fixture {
    _tmp: tempfile::TempDir,
    home: Home,
    running: RunningDaemon,
    project_root: PathBuf,
    project_id: String,
}

impl Fixture {
    async fn new(project_id: &str) -> Self {
        let tmp = tempfile::tempdir().unwrap();
        let home = Home::at(tmp.path().join("home"));
        seed_source(&home);
        let running = boot(home.clone()).await;
        let project_root = tmp.path().join("repo");
        init_git_repo(&project_root);
        let out = orgasmic_command()
            .args([
                "project",
                "init",
                "--path",
                project_root.to_str().unwrap(),
                "--name",
                project_id,
            ])
            .current_dir(&project_root)
            .env("ORGASMIC_HOME", &home.root)
            .env("ORGASMIC_DAEMON_URL", format!("http://{}", running.addr))
            .output()
            .expect("run orgasmic project init");
        assert!(
            out.status.success(),
            "project init failed: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        run_git(&project_root, &["add", "."]);
        run_git(&project_root, &["commit", "-m", "scaffold .orgasmic"]);
        wait_for_project_loaded(&home, &running, &project_root, project_id);
        Self {
            _tmp: tmp,
            home,
            running,
            project_root,
            project_id: project_id.to_string(),
        }
    }

    fn run(&self, args: &[&str]) -> String {
        run_cli(&self.home, &self.running, &self.project_root, args)
    }

    fn json(&self, args: &[&str]) -> serde_json::Value {
        run_cli_json(&self.home, &self.running, &self.project_root, args)
    }

    fn refusal(&self, args: &[&str]) -> String {
        run_cli_refusal(&self.home, &self.running, &self.project_root, args)
    }

    fn create_task(&self, title: &str, extra: &[&str]) -> String {
        let mut args = vec![
            "task",
            "create",
            "--project",
            self.project_id.as_str(),
            "--title",
            title,
        ];
        args.extend_from_slice(extra);
        self.json(&args)["id"]
            .as_str()
            .expect("minted id")
            .to_string()
    }

    /// The task's drawer, read straight off disk — the daemon's own view of a
    /// property it never wrote is not evidence.
    fn drawer(&self, task_id: &str) -> String {
        let dir = self.project_root.join(".orgasmic").join("tasks");
        for entry in std::fs::read_dir(&dir).expect("read tasks dir") {
            let path = entry.expect("dir entry").path();
            let Ok(text) = std::fs::read_to_string(&path) else {
                continue;
            };
            let mut out = String::new();
            let mut inside = false;
            for line in text.lines() {
                if line.starts_with("* ") {
                    if inside {
                        break;
                    }
                    inside = line.contains(task_id);
                }
                if inside {
                    out.push_str(line);
                    out.push('\n');
                }
            }
            if !out.is_empty() {
                return out;
            }
        }
        panic!("no heading for {task_id} under {}", dir.display());
    }
}

/// INSTANCE 1 (the confirmed one): `--property priority=P1` on create.
///
/// `Heading::property` compares keys byte for byte, so a lowercase key lands in
/// the drawer as `:priority:` and is read by nothing. Seven P1 tasks were filed
/// unprioritised this way on 2026-07-26 and repaired afterwards by hand.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn task_create_refuses_a_miscased_property_key_and_names_the_canonical_one() {
    let fx = Fixture::new("prop-miscased").await;

    let stderr = fx.refusal(&[
        "task",
        "create",
        "--project",
        "prop-miscased",
        "--title",
        "filed with a lowercase priority key",
        "--property",
        "priority=P1",
    ]);
    assert!(
        stderr.contains("PRIORITY"),
        "the refusal must name the canonical key: {stderr}"
    );
    assert!(
        stderr.contains("priority"),
        "the refusal must quote the key that was passed: {stderr}"
    );

    // Nothing was filed: a refusal that still wrote the heading would be a
    // worse bug than the drop it replaced. (The scaffold ships seed tasks, so
    // this asks whether THIS heading landed, not whether the project is empty.)
    let listed = fx.json(&["tasks", "list", "--project", "prop-miscased"]);
    let body = serde_json::to_string(&listed).unwrap();
    assert!(
        !body.contains("filed with a lowercase priority key"),
        "the refused create must not have filed a task: {body}"
    );
}

/// The other half of instance 1: `--priority` has to EXIST on create, or the
/// refusal above just moves the pain. `update --priority` always worked.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn task_create_takes_priority_as_a_first_class_flag() {
    let fx = Fixture::new("prop-priority").await;

    let task_id = fx.create_task("filed with a real priority flag", &["--priority", "P1"]);
    let drawer = fx.drawer(&task_id);
    assert!(
        drawer.contains(":PRIORITY: P1"),
        "`--priority P1` must write the canonical drawer key:\n{drawer}"
    );

    let detail = fx.json(&["task", "get", "--project", "prop-priority", &task_id]);
    assert_eq!(
        detail["priority"].as_str(),
        Some("P1"),
        "the priority must be READ back, not merely stored: {detail}"
    );

    // Spelling it both ways in one call is a contradiction, not a merge.
    let stderr = fx.refusal(&[
        "task",
        "create",
        "--project",
        "prop-priority",
        "--title",
        "two spellings of one field",
        "--priority",
        "P1",
        "--property",
        "PRIORITY=P2",
    ]);
    assert!(
        stderr.contains("--priority") && stderr.contains("PRIORITY"),
        "the refusal must name both spellings: {stderr}"
    );
}

/// INSTANCE 5: `:PARENT_TASK:` is accepted, stored, echoed as changed — and
/// read by nothing. `parent_task` comes from the id grammar alone.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn parent_task_property_is_refused_on_both_verbs_and_names_the_id_grammar() {
    let fx = Fixture::new("prop-parent").await;

    let parent = fx.create_task("the parent", &[]);

    let create_stderr = fx.refusal(&[
        "task",
        "create",
        "--project",
        "prop-parent",
        "--title",
        "a fix subtask filed with a back-edge property",
        "--property",
        &format!("PARENT_TASK={parent}"),
    ]);
    assert!(
        create_stderr.contains("TASK-<parent>"),
        "the refusal must name the id grammar that carries parentage: {create_stderr}"
    );

    let child = fx.create_task("filed the documented way", &[]);
    let update_stderr = fx.refusal(&[
        "task",
        "update",
        "--project",
        "prop-parent",
        &child,
        "--property",
        &format!("PARENT_TASK={parent}"),
    ]);
    assert!(
        update_stderr.contains("TASK-<parent>"),
        "update accepted and stored this key while nothing read it: {update_stderr}"
    );
    assert!(
        !fx.drawer(&child).contains("PARENT_TASK"),
        "the refused update must not have written the drawer:\n{}",
        fx.drawer(&child)
    );

    // The mechanism that DOES work, so the refusal is not a dead end.
    let minted = fx.create_task("the real subtask", &["--id", &format!("{parent}.1")]);
    let detail = fx.json(&["task", "get", "--project", "prop-parent", &minted]);
    assert_eq!(
        detail["parent_task"].as_str(),
        Some(parent.as_str()),
        "id-grammar parentage must actually produce the edge: {detail}"
    );
}

/// INSTANCE 4: a key `update` persists and `create` used to filter out, so the
/// same `--property` produced different state depending on the verb reached for.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn task_create_refuses_keys_only_update_writes_and_names_update() {
    let fx = Fixture::new("prop-divergence").await;

    let stderr = fx.refusal(&[
        "task",
        "create",
        "--project",
        "prop-divergence",
        "--title",
        "filed with an update-only key",
        "--property",
        "FIX_SUBTASK=t",
    ]);
    assert!(
        stderr.contains("task update"),
        "the refusal must name the verb that does write it: {stderr}"
    );

    // …and `update` really does write it, so the divergence the message states
    // is the divergence that exists.
    let task_id = fx.create_task("filed without it", &[]);
    let changed = fx.json(&[
        "task",
        "update",
        "--project",
        "prop-divergence",
        &task_id,
        "--property",
        "FIX_SUBTASK=t",
    ]);
    assert_eq!(changed["changed"]["FIX_SUBTASK"].as_str(), Some("t"));
    assert!(fx.drawer(&task_id).contains(":FIX_SUBTASK: t"));
}

/// INSTANCE 5 in the task body, and the worst class in it: `--state` short-
/// circuited the update handler and returned `{"changed":{"STATE":…}}` with
/// every drawer field on the same call thrown away.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn task_update_refuses_state_and_drawer_fields_in_one_call() {
    let fx = Fixture::new("prop-state").await;

    let task_id = fx.create_task("a task about to be moved and annotated", &[]);
    let stderr = fx.refusal(&[
        "task",
        "update",
        "--project",
        "prop-state",
        &task_id,
        "--state",
        "in_progress",
        "--property",
        "WRITE_SCOPE=crates/**",
        "--property",
        "TEST_CMD=cargo test",
    ]);
    assert!(
        stderr.contains("--state"),
        "the refusal must name the field that used to win: {stderr}"
    );
    assert!(
        stderr.contains("WRITE_SCOPE") && stderr.contains("TEST_CMD"),
        "the refusal must name the fields that used to be dropped: {stderr}"
    );

    // Neither half landed, so the caller is not left guessing which one did.
    let drawer = fx.drawer(&task_id);
    assert!(
        !drawer.contains("WRITE_SCOPE") && !drawer.contains("TEST_CMD"),
        "the refused call must not have written the drawer:\n{drawer}"
    );
    assert!(
        drawer.contains("BACKLOG"),
        "the refused call must not have moved the task either:\n{drawer}"
    );

    // Run as two calls, both land.
    fx.run(&[
        "task",
        "update",
        "--project",
        "prop-state",
        &task_id,
        "--state",
        "in_progress",
    ]);
    fx.run(&[
        "task",
        "update",
        "--project",
        "prop-state",
        &task_id,
        "--property",
        "WRITE_SCOPE=crates/**",
    ]);
    let drawer = fx.drawer(&task_id);
    assert!(drawer.contains(":WRITE_SCOPE: crates/**"), "{drawer}");
}

/// INSTANCE 6: `tx record` was the only mutation verb that was not project-
/// scoped by default. Without `--project` it appended to the global home
/// ledger and reported success with a home-shaped tx id, so a manager
/// following the obvious invocation lost the entry and could not tell.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn tx_record_defaults_to_the_cwd_project_like_its_siblings() {
    let fx = Fixture::new("tx-scope").await;

    let recorded = fx.json(&[
        "tx",
        "record",
        "--type",
        "manager.action",
        "--reason",
        "recorded from inside the project with no --project",
    ]);
    let tx_path = recorded["tx_path"]
        .as_str()
        .unwrap_or_else(|| panic!("tx_path in {recorded}"));
    // macOS hands back `/private/var/...` for a `/var/...` tempdir, so compare
    // canonical paths rather than the strings the daemon happened to print.
    let tx_path = std::fs::canonicalize(tx_path).expect("canonicalize tx path");
    let project_root = std::fs::canonicalize(&fx.project_root).expect("canonicalize project root");
    let home_root = std::fs::canonicalize(&fx.home.root).expect("canonicalize home root");
    assert!(
        tx_path.starts_with(&project_root),
        "tx record without --project must land on the PROJECT ledger, not {}",
        tx_path.display()
    );
    assert!(
        !tx_path.starts_with(&home_root),
        "tx record without --project must not land in $ORGASMIC_HOME: {}",
        tx_path.display()
    );

    let listed = fx.json(&["tx", "list", "--project", "tx-scope", "--limit", "5"]);
    let body = serde_json::to_string(&listed).unwrap();
    assert!(
        body.contains("recorded from inside the project with no --project"),
        "the entry must be on the ledger `tx list --project` reads: {body}"
    );

    // `--tx-path` stays the explicit way out, and still bypasses the project.
    let explicit = fx.project_root.join("explicit-tx.org");
    let recorded = fx.json(&[
        "tx",
        "record",
        "--type",
        "manager.action",
        "--reason",
        "explicitly pathed",
        "--tx-path",
        explicit.to_str().unwrap(),
    ]);
    assert_eq!(
        std::fs::canonicalize(recorded["tx_path"].as_str().expect("tx_path")).ok(),
        std::fs::canonicalize(&explicit).ok(),
        "--tx-path must still win: {recorded}"
    );
}
