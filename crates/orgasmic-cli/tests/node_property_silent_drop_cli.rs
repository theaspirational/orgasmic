// orgasmic:task_ZKZBF
//! Production-path proof for TASK-ZKZBF: the remaining `--property` surfaces
//! refuse a miscased key BY NAME instead of storing it (or dropping it)
//! silently — the TASK-HXSW0 shape audited across every other write verb.
//!
//! The real `orgasmic` binary against a real daemon over a scaffolded project,
//! with the drawer read back off disk, because the whole defect class is
//! "returned a normal success object while the argument went nowhere".
//!
//! One test per audited surface:
//!
//! - `node prop set <id> priority P1` is refused and names `PRIORITY` (used to
//!   store `:priority:` and return `{"changed":{"priority":"P1"}}`).
//! - `node prop unset <id> priority` is refused and names `PRIORITY` (used to
//!   fail with a mute "org file update failed" while `:PRIORITY:` sat there).
//! - `glossary create --property canonical=…` is refused and names `CANONICAL`;
//!   spelling one field both ways (`--definition` + `--property DEFINITION=`)
//!   is a contradiction, not a silent overwrite by the typed flag.
//! - `decision create --property parent=…` is refused and names `PARENT`.
//! - `manager dispatch-close --property verdict=…` is refused at parse time
//!   and names `VERDICT`, and `--status aborted` refuses `--property` keys by
//!   name instead of accepting and silently discarding them.

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
/// drop is exactly a command that succeeds while discarding an argument.
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

    fn create_task(&self, title: &str) -> String {
        self.json(&[
            "task",
            "create",
            "--project",
            self.project_id.as_str(),
            "--title",
            title,
        ])["id"]
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

    /// A whole graph file off disk: `.orgasmic/decisions.org` etc. Empty when
    /// the file does not exist yet (a refused create must not create it).
    fn graph_file(&self, name: &str) -> String {
        let path = self.project_root.join(".orgasmic").join(name);
        std::fs::read_to_string(&path).unwrap_or_default()
    }
}

/// SURFACE 1: `node prop set`. The generic org-node editor wrote any key
/// verbatim, so a lowercase key landed as `:priority:` — stored, echoed in
/// `changed`, and read by nothing (the byte comparison in `Heading::property`).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn node_prop_set_refuses_a_miscased_task_key_and_names_the_canonical_one() {
    let fx = Fixture::new("nodeprop-set").await;
    let task_id = fx.create_task("a task about to be annotated through the node editor");

    let stderr = fx.refusal(&["node", "prop", "set", &task_id, "priority", "P1"]);
    assert!(
        stderr.contains("PRIORITY"),
        "the refusal must name the canonical key: {stderr}"
    );
    assert!(
        stderr.contains("priority"),
        "the refusal must quote the key that was passed: {stderr}"
    );

    // Nothing was written: neither spelling is in the drawer.
    let drawer = fx.drawer(&task_id);
    assert!(
        !drawer.contains("priority") && !drawer.contains("PRIORITY"),
        "the refused set must not have written the drawer:\n{drawer}"
    );

    // The mechanism that DOES work, so the refusal is not a dead end — and
    // the canonical spelling is genuinely read back, not merely stored.
    let changed = fx.json(&["node", "prop", "set", &task_id, "PRIORITY", "P1"]);
    assert_eq!(
        changed["changed"]["PRIORITY"].as_str(),
        Some("P1"),
        "the canonical spelling must write and echo the field: {changed}"
    );
    assert!(
        fx.drawer(&task_id).contains(":PRIORITY: P1"),
        "the drawer must carry the canonical key"
    );
    let detail = fx.json(&["task", "get", "--project", "nodeprop-set", &task_id]);
    assert_eq!(
        detail["priority"].as_str(),
        Some("P1"),
        "the priority must be READ back, not merely stored: {detail}"
    );
}

/// SURFACE 2: `node prop unset`. A miscased key used to die inside the
/// rewriter as `PropertyNotFound` and surface as a mute "org file update
/// failed" — no spelling of the key anywhere in the message.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn node_prop_unset_refuses_a_miscased_task_key_and_names_the_canonical_one() {
    let fx = Fixture::new("nodeprop-unset").await;
    let task_id = fx.create_task("a task carrying a real priority");
    fx.run(&["node", "prop", "set", &task_id, "PRIORITY", "P2"]);

    let stderr = fx.refusal(&["node", "prop", "unset", &task_id, "priority"]);
    assert!(
        stderr.contains("PRIORITY"),
        "the refusal must name the canonical key: {stderr}"
    );
    assert!(
        stderr.contains("priority"),
        "the refusal must quote the key that was passed: {stderr}"
    );

    // The real property survived the refused call.
    assert!(
        fx.drawer(&task_id).contains(":PRIORITY: P2"),
        "the refused unset must not have removed the canonical key"
    );

    // The mechanism that DOES work.
    fx.run(&["node", "prop", "unset", &task_id, "PRIORITY"]);
    let drawer = fx.drawer(&task_id);
    assert!(
        !drawer.contains("PRIORITY"),
        "the canonical unset must remove the key:\n{drawer}"
    );
}

/// SURFACE 3: `glossary create --property`. `render_graph_heading` wrote every
/// property verbatim, so `--property canonical=…` stored `:canonical:` and no
/// reader consumed it. And a typed flag (`--definition`) silently OVERWROTE a
/// `--property DEFINITION=` passed in the same call — the HXSW0 both-spellings
/// drop wearing a second hat.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn glossary_create_refuses_miscased_and_double_spelled_keys_by_name() {
    let fx = Fixture::new("glossary-prop").await;

    let stderr = fx.refusal(&[
        "glossary",
        "create",
        "--project",
        "glossary-prop",
        "--title",
        "vertical-slice-zkzbf",
        "--property",
        "canonical=Vertical Slice",
    ]);
    assert!(
        stderr.contains("CANONICAL"),
        "the refusal must name the canonical key: {stderr}"
    );
    assert!(
        stderr.contains("canonical"),
        "the refusal must quote the key that was passed: {stderr}"
    );
    assert!(
        !fx.graph_file("glossary.org")
            .contains("vertical-slice-zkzbf"),
        "the refused create must not have filed a term"
    );

    // Both spellings of one field in one call is a contradiction, not a merge.
    let stderr = fx.refusal(&[
        "glossary",
        "create",
        "--project",
        "glossary-prop",
        "--title",
        "vertical-slice-zkzbf",
        "--definition",
        "one deployable slice of behavior",
        "--property",
        "DEFINITION=the other value",
    ]);
    assert!(
        stderr.contains("--definition") && stderr.contains("DEFINITION"),
        "the refusal must name both spellings: {stderr}"
    );

    // The mechanism that DOES work: typed flag lands the canonical key and the
    // term is read back through the glossary read model.
    let created = fx.json(&[
        "glossary",
        "create",
        "--project",
        "glossary-prop",
        "--title",
        "vertical-slice-zkzbf",
        "--definition",
        "one deployable slice of behavior",
    ]);
    let term_id = created["id"].as_str().expect("minted term id").to_string();
    assert!(
        fx.graph_file("glossary.org")
            .contains(":DEFINITION: one deployable slice of behavior"),
        "the typed flag must write the canonical drawer key"
    );
    let listed = fx.json(&["glossary", "list", "--project", "glossary-prop"]);
    let body = serde_json::to_string(&listed).unwrap();
    assert!(
        body.contains(&term_id),
        "the created term must be in the read model: {body}"
    );
}

/// SURFACE 4: `decision create --property`. Same verbatim-write shape: a
/// lowercase `parent` was stored as `:parent:` while `DecisionNode` reads
/// `PARENT` byte for byte — the edge silently did not exist.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn decision_create_refuses_a_miscased_property_key_and_names_the_canonical_one() {
    let fx = Fixture::new("decision-prop").await;

    let parent = fx.json(&[
        "decision",
        "create",
        "--project",
        "decision-prop",
        "--title",
        "the parent decision",
    ])["id"]
        .as_str()
        .expect("minted decision id")
        .to_string();

    let stderr = fx.refusal(&[
        "decision",
        "create",
        "--project",
        "decision-prop",
        "--title",
        "the child filed with a lowercase parent key",
        "--property",
        &format!("parent={parent}"),
    ]);
    assert!(
        stderr.contains("PARENT"),
        "the refusal must name the canonical key: {stderr}"
    );
    assert!(
        stderr.contains("parent"),
        "the refusal must quote the key that was passed: {stderr}"
    );
    assert!(
        !fx.graph_file("decisions.org")
            .contains("lowercase parent key"),
        "the refused create must not have filed a decision"
    );

    // The mechanism that DOES work: canonical spelling lands the edge and the
    // read model reports the parentage.
    let child = fx.json(&[
        "decision",
        "create",
        "--project",
        "decision-prop",
        "--title",
        "the child decision",
        "--property",
        &format!("PARENT={parent}"),
    ]);
    let child_id = child["id"].as_str().expect("minted child id").to_string();
    assert!(
        fx.graph_file("decisions.org")
            .contains(&format!(":PARENT: {parent}")),
        "the canonical spelling must write the drawer key"
    );
    // `decision get` prints a human outline; the parent edge must be READ back
    // through it, not merely stored in the drawer.
    let detail = fx.run(&["decision", "get", "--project", "decision-prop", &child_id]);
    assert!(
        detail.contains(&format!("parent: {parent}")),
        "the parent edge must be READ back, not merely stored: {detail}"
    );
}

/// SURFACE 5a: `manager dispatch-close --property` refuses a miscased key at
/// parse time and names the canonical spelling — close-tx readers (`extra()`)
/// match keys byte for byte, so `:verdict:` would be recorded and never read.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn dispatch_close_refuses_a_miscased_property_key_by_name() {
    let fx = Fixture::new("close-miscased").await;

    let stderr = fx.refusal(&[
        "manager",
        "dispatch-close",
        "--task",
        "TASK-NOSUCH",
        "--status",
        "done",
        "--property",
        "verdict=clean",
    ]);
    assert!(
        stderr.contains("VERDICT"),
        "the refusal must name the canonical spelling: {stderr}"
    );
    assert!(
        stderr.contains("verdict"),
        "the refusal must quote the key that was passed: {stderr}"
    );
}

/// SURFACE 5b: `--status aborted` used to accept `--property` and silently
/// discard every value — `close_aborted_request` has no generic property
/// channel. Refused by name instead, before anything is cleaned up.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn dispatch_close_aborted_refuses_properties_it_would_silently_drop() {
    let fx = Fixture::new("close-aborted").await;
    let task_id = fx.create_task("a task whose dispatch will be aborted");

    // Stage one open dispatch generation the close can resolve to.
    let started = fx.json(&[
        "tx",
        "record",
        "--project",
        "close-aborted",
        "--type",
        "manager.dispatch_started",
        "--task",
        &task_id,
        "--extra",
        "KIND=implementer",
    ]);
    let started_tx = started["tx_id"]
        .as_str()
        .expect("tx record returns tx_id")
        .to_string();

    let stderr = fx.refusal(&[
        "manager",
        "dispatch-close",
        "--task",
        &task_id,
        "--started-tx",
        &started_tx,
        "--status",
        "aborted",
        "--reason",
        "worker went sideways",
        "--property",
        "SALVAGE_NOTE=kept the branch around",
    ]);
    assert!(
        stderr.contains("SALVAGE_NOTE"),
        "the refusal must name the dropped key: {stderr}"
    );
    assert!(
        stderr.contains("aborted"),
        "the refusal must say which status does not record properties: {stderr}"
    );

    // The abort tx channel that DOES exist — the same close without
    // --property records the abort off its structured fields.
    let out = fx.run(&[
        "manager",
        "dispatch-close",
        "--task",
        &task_id,
        "--started-tx",
        &started_tx,
        "--status",
        "aborted",
        "--reason",
        "worker went sideways",
    ]);
    assert!(
        out.contains("manager.dispatch_aborted"),
        "the abort close must still record its tx: {out}"
    );
}
