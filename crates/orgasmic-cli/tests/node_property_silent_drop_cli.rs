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
//! - TASK-ZKZBF.1: `node prop set/unset <id> ID …` is refused as
//!   identity-immutable on EVERY layer (a set used to re-key the node and
//!   dangle every reference to the old id); graph revise refuses `ID` exactly
//!   like create; `glossary create --property id=…` is ONE refusal; `tx
//!   record --extra kind=…` is refused naming `KIND` (a miscased extra used
//!   to make a dispatch invisible to `dispatch-close`); a
//!   `PropertyNotFound` on unset names the drawer's real keys without leaking
//!   a filesystem path; unset refusals no longer describe a `--property`
//!   flag the command does not have.
//! - TASK-ZKZBF.2: the refusal table is split by DIRECTION — legacy dead-key
//!   drawer lines (`:PARENT_TASK:`, `:LAST_UPDATED:`) are removable through
//!   `node prop unset` while writing them stays refused, and STATE (owned by
//!   the lifecycle door) still refuses both directions; a self-uppercase but
//!   wrong-SHAPE key (`FOO-BAR`) is refused by the shared drawer check as a
//!   400 naming the flag instead of dying inside the tx writer as a 500.

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

    /// Writes drawer lines into a task's drawer the way the pre-refusal
    /// daemon left them: a direct file write standing in for an old write no
    /// supported verb can produce anymore (every current door refuses these
    /// keys). The next node op reads the artifact fresh off disk, so no
    /// reindex is needed to make the seeded lines visible to `node prop`.
    fn seed_drawer_lines(&self, task_id: &str, lines: &[String]) {
        let dir = self.project_root.join(".orgasmic").join("tasks");
        for entry in std::fs::read_dir(&dir).expect("read tasks dir") {
            let path = entry.expect("dir entry").path();
            let Ok(text) = std::fs::read_to_string(&path) else {
                continue;
            };
            let mut file_lines: Vec<String> = text.split('\n').map(str::to_string).collect();
            let mut in_target = false;
            let mut insert_at = None;
            for (idx, line) in file_lines.iter().enumerate() {
                if line.starts_with("* ") {
                    in_target = line.contains(task_id);
                    continue;
                }
                if in_target && line.trim() == ":PROPERTIES:" {
                    insert_at = Some(idx + 1);
                    break;
                }
            }
            if let Some(at) = insert_at {
                for (offset, line) in lines.iter().enumerate() {
                    file_lines.insert(at + offset, line.clone());
                }
                std::fs::write(&path, file_lines.join("\n")).unwrap();
                return;
            }
        }
        panic!("no drawer for {task_id} under {}", dir.display());
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

// orgasmic:task_ZKZBF.1
// ---------------------------------------------------------------------------
// The round-1 review findings: the ID-immutability guard on every drawer
// write, the tx-record case rule, and the message polish.
// ---------------------------------------------------------------------------

/// MEDIUM-1: `node prop set <id> ID <anything>` used to be a 200 on every
/// non-task layer — the upsert re-keyed the node (`find_by_id` matches
/// `:ID:` byte for byte), dangling every reference to the old id, no --force
/// required. Refused by name as identity-immutable on ALL layers, and the
/// file must stay byte-identical: a refusal that still wrote would be a worse
/// bug than the re-key it replaced.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn node_prop_set_refuses_id_as_identity_on_every_layer_and_never_rekeys() {
    let fx = Fixture::new("nodeprop-id").await;

    let term = fx.json(&[
        "glossary",
        "create",
        "--project",
        "nodeprop-id",
        "--title",
        "a term whose identity must not be rewritable",
        "--definition",
        "the id is derived, not stored from the caller",
    ])["id"]
        .as_str()
        .expect("minted term id")
        .to_string();
    let dec = fx.json(&[
        "decision",
        "create",
        "--project",
        "nodeprop-id",
        "--title",
        "a decision whose identity must not be rewritable",
    ])["id"]
        .as_str()
        .expect("minted decision id")
        .to_string();

    // The scaffold's project heading id, straight off disk — the layer the
    // `--kind project` path addresses.
    let project_node_id = fx
        .graph_file("project.org")
        .lines()
        .find_map(|line| line.trim_start_matches(':').strip_prefix("ID:"))
        .map(str::trim)
        .expect("project.org carries :ID:")
        .to_string();

    // Glossary and Decision resolve from the id prefix; Project needs the
    // explicit kind because a bare project name would infer Glossary.
    for (id, kind, file) in [
        (term.as_str(), None, "glossary.org"),
        (dec.as_str(), None, "decisions.org"),
        (project_node_id.as_str(), Some("project"), "project.org"),
    ] {
        let before = fx.graph_file(file);
        let mut args = vec!["node", "prop", "set", id, "ID", "term_ZZZZZZZZ"];
        if let Some(kind) = kind {
            args.push("--kind");
            args.push(kind);
        }
        let stderr = fx.refusal(&args);
        assert!(
            stderr.contains("immutable") && stderr.contains("ID"),
            "the refusal must name ID and its immutability: {stderr}"
        );
        assert_eq!(
            fx.graph_file(file),
            before,
            "the refused set must not have touched {file}"
        );
    }

    // The term still answers to its original id — the read model never saw a
    // re-key because none happened.
    let got = fx.run(&["glossary", "get", "--project", "nodeprop-id", &term]);
    assert!(
        got.contains(&term),
        "the term must still be reachable under its original id: {got}"
    );
}

/// MEDIUM-1, unset half: removing `:ID:` is the same identity write with the
/// sign flipped. Refused by name on the task layer (which already refused it)
/// and on the layers round 1 left open.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn node_prop_unset_refuses_id_as_identity_on_task_and_term_layers() {
    let fx = Fixture::new("nodeprop-unset-id").await;
    let task_id = fx.create_task("a task whose id is not removable");
    let term = fx.json(&[
        "glossary",
        "create",
        "--project",
        "nodeprop-unset-id",
        "--title",
        "a term whose id is not removable",
        "--definition",
        "identity is derived",
    ])["id"]
        .as_str()
        .expect("minted term id")
        .to_string();

    for (id, file) in [(&task_id, None), (&term, Some("glossary.org"))] {
        let before = file.map(|name| fx.graph_file(name));
        let stderr = fx.refusal(&["node", "prop", "unset", id, "ID"]);
        assert!(
            stderr.contains("immutable") && stderr.contains("ID"),
            "the refusal must name ID and its immutability: {stderr}"
        );
        if let (Some(file), Some(before)) = (file, before) {
            assert_eq!(
                fx.graph_file(file),
                before,
                "the refused unset must not have touched {file}"
            );
        }
    }

    // The task's drawer still carries its :ID: line.
    assert!(
        fx.drawer(&task_id).contains(":ID:"),
        "the refused unset must not have removed the identity line"
    );
}

/// MEDIUM-1, the create/revise divergence: `create_graph_heading` refused ID
/// by name but `mutate_graph_heading` (graph revise, HTTP surface — no CLI
/// verb reaches it) got only the case check, and `ID` is already uppercase.
/// Both now refuse it through the same shared guard. Handoff joins through
/// the org-node editor with an explicit kind: its validation also runs before
/// any file is read.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn graph_revise_and_handoff_refuse_id_like_create() {
    let fx = Fixture::new("graph-id").await;

    let term = fx.json(&[
        "glossary",
        "create",
        "--project",
        "graph-id",
        "--title",
        "a term revise must not re-key",
        "--definition",
        "identity is derived",
    ])["id"]
        .as_str()
        .expect("minted term id")
        .to_string();
    let dec = fx.json(&[
        "decision",
        "create",
        "--project",
        "graph-id",
        "--title",
        "a decision revise must not re-key",
    ])["id"]
        .as_str()
        .expect("minted decision id")
        .to_string();

    let token = std::fs::read_to_string(fx.home.auth_token())
        .expect("daemon bearer token")
        .trim()
        .to_string();
    let client = reqwest::Client::new();
    let base = format!("http://{}", fx.running.addr);

    // Graph revise (`POST /glossary/:id`, `POST /decisions/:id`).
    for (path, file) in [
        (format!("/api/glossary/{term}"), "glossary.org"),
        (format!("/api/decisions/{dec}"), "decisions.org"),
    ] {
        let before = fx.graph_file(file);
        let resp = client
            .post(format!("{base}{path}"))
            .bearer_auth(&token)
            .json(&serde_json::json!({
                "project": fx.project_id,
                "action": "revise",
                "properties": { "ID": "term_ZZZZZZZZ" },
            }))
            .send()
            .await
            .expect("POST graph revise");
        assert_eq!(
            resp.status(),
            400,
            "revise with an ID property must be refused, not re-key"
        );
        let body: serde_json::Value = resp.json().await.expect("error body");
        let error = body["error"].as_str().unwrap_or_default();
        assert!(
            error.contains("immutable") && error.contains("ID"),
            "the refusal must name ID and its immutability: {error}"
        );
        assert_eq!(
            fx.graph_file(file),
            before,
            "the refused revise must not have touched {file}"
        );
    }

    // Handoff layer through the org-node editor (`--kind handoff`); the key
    // validation runs before the node or its file is read.
    let resp = client
        .post(format!("{base}/api/org/node/handoff-current/edit"))
        .bearer_auth(&token)
        .json(&serde_json::json!({
            "project": fx.project_id,
            "kind": "handoff",
            "base_version": "0",
            "ops": [ { "op": "set_property", "key": "ID", "value": "term_ZZZZZZZZ" } ],
        }))
        .send()
        .await
        .expect("POST org node edit");
    assert_eq!(resp.status(), 400, "handoff ID write must be refused");
    let body: serde_json::Value = resp.json().await.expect("error body");
    let error = body["error"].as_str().unwrap_or_default();
    assert!(
        error.contains("immutable") && error.contains("ID"),
        "the refusal must name ID and its immutability: {error}"
    );
}

/// LOW-3: `glossary create --property id=x` used to take two round trips —
/// the case check said "use `ID`", and `ID` was then hard-refused. The guard
/// is matched case-insensitively now, so any casing of the identity key gets
/// ONE refusal naming the immutability rule.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn glossary_create_with_a_miscased_id_is_one_refusal_naming_immutability() {
    let fx = Fixture::new("glossary-id").await;

    for key in ["id", "Id", "ID"] {
        let stderr = fx.refusal(&[
            "glossary",
            "create",
            "--project",
            "glossary-id",
            "--title",
            "a term filed with the identity key as a property",
            "--property",
            &format!("{key}=term_ZZZZZZZZ"),
        ]);
        assert!(
            stderr.contains("immutable") && stderr.contains("ID"),
            "the refusal must name ID immutability: {stderr}"
        );
        assert!(
            !stderr.contains("canonical drawer spelling"),
            "one refusal, not a case correction into a hard refusal: {stderr}"
        );
    }
    assert!(
        !fx.graph_file("glossary.org")
            .contains("filed with the identity key"),
        "the refused creates must not have filed a term"
    );
}

/// MEDIUM-2: `tx record --extra kind=implementer` used to write `:kind:` —
/// byte-exact readers like `extra()` never matched `KIND`, so the staged
/// dispatch was INVISIBLE to `dispatch-close` (not closed wrong: unfindable).
/// The ledger's own key parse now enforces the decided rule (open vocabulary,
/// closed case), while the canonical form still stages and still resolves.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn tx_record_extra_refuses_a_miscased_key_and_the_canonical_form_stages_a_visible_dispatch() {
    let fx = Fixture::new("tx-extra-case").await;
    let task_id = fx.create_task("a task whose dispatch stages the tx case rule");

    // The miscased extra is refused at the ledger's parse, naming the
    // canonical spelling like `dispatch-close --property` already does.
    let stderr = fx.refusal(&[
        "tx",
        "record",
        "--project",
        "tx-extra-case",
        "--type",
        "manager.dispatch_started",
        "--task",
        &task_id,
        "--extra",
        "kind=implementer",
    ]);
    assert!(
        stderr.contains("kind") && stderr.contains("KIND"),
        "the refusal must quote the passed key and name the canonical one: {stderr}"
    );

    // The canonical form still stages — and the close machinery still FINDS
    // it: the aborted close resolves the started tx to a dispatch record,
    // which is only possible when `:KIND:` is where `extra()` looks.
    let started = fx.json(&[
        "tx",
        "record",
        "--project",
        "tx-extra-case",
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
        "staged to prove the uppercase extra stays visible",
    ]);
    assert!(
        out.contains("manager.dispatch_aborted"),
        "the uppercase-staged dispatch must still be findable by the close: {out}"
    );
}

/// LOW-1: a `PropertyNotFound` on unset used to surface the daemon-side
/// Display string, which carries the ABSOLUTE on-disk path — a disclosure no
/// sibling makes (org_parse_bad_request warns the path and returns a
/// path-free body; tx.rs: "Never a path") — while omitting the one fact that
/// explains the miss: the keys the drawer actually has. Mirrors
/// `unknown_section_error` now: node named, real keys listed, no path.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn node_prop_unset_for_an_absent_key_names_the_drawers_real_keys_without_a_path() {
    let fx = Fixture::new("unset-absent").await;
    let task_id = fx.create_task("a task carrying one real property");
    fx.run(&["node", "prop", "set", &task_id, "PRIORITY", "P2"]);

    // PRODUCES is canonically spelled and genuinely absent, so it sails past
    // the case check and dies inside the rewriter: the PropertyNotFound path.
    let stderr = fx.refusal(&["node", "prop", "unset", &task_id, "PRODUCES"]);
    assert!(
        stderr.contains(task_id.as_str()),
        "the refusal must name the node: {stderr}"
    );
    assert!(
        stderr.contains("PRODUCES"),
        "the refusal must name the key that is not there: {stderr}"
    );
    assert!(
        stderr.contains("PRIORITY"),
        "the refusal must list the keys the drawer DOES have: {stderr}"
    );
    assert!(
        !stderr.contains('/') && !stderr.contains(".org"),
        "the refusal must not leak a filesystem path: {stderr}"
    );

    // The real property survived the refused call.
    assert!(fx.drawer(&task_id).contains(":PRIORITY: P2"));
}

/// LOW-2: unset refusals used to reuse the set-verb's phrasing —
/// "`--property KEY=…` is refused" — describing a flag `node prop unset` does
/// not have. The advice is unset-shaped now.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn node_prop_unset_refusal_advice_is_unset_shaped() {
    let fx = Fixture::new("unset-shape").await;
    let task_id = fx.create_task("a task whose STATE is not unsettable");

    let stderr = fx.refusal(&["node", "prop", "unset", &task_id, "STATE"]);
    assert!(
        stderr.contains("STATE"),
        "the refusal must name the refused key: {stderr}"
    );
    assert!(
        stderr.contains("lifecycle"),
        "the refusal must still name the door that owns the key: {stderr}"
    );
    assert!(
        !stderr.contains("--property"),
        "unset advice must not describe a flag the command does not have: {stderr}"
    );
}

// orgasmic:task_ZKZBF.2
// ---------------------------------------------------------------------------
// The round-2 review findings: the unset-split contract (dead keys removable,
// STATE still door-owned both directions) and the drawer-shape 400.
// ---------------------------------------------------------------------------

/// MEDIUM: the refusal table used to be write-only but was consulted for
/// removals too, so legacy `:PARENT_TASK:`/`:LAST_UPDATED:` drawer lines
/// (the real ones in `done.org` were written before any door refused them)
/// had NO supported removal verb: `node prop unset` is the only drawer-key
/// removal door and it refused them by inheritance. The table is split by
/// direction now — dead keys are removable, STATE (whose removal would
/// desynchronise the lifecycle state machine) still refuses BOTH ways.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn dead_drawer_keys_are_removable_while_state_stays_door_owned() {
    let fx = Fixture::new("dead-key-unset").await;
    let parent = fx.create_task("the id-grammar parent");
    let task_id = fx.create_task("a task carrying pre-refusal dead keys");
    fx.seed_drawer_lines(
        &task_id,
        &[
            format!(":PARENT_TASK: {parent}"),
            ":LAST_UPDATED: [2026-01-01 Thu 00:00:00]".to_string(),
        ],
    );
    assert!(
        fx.drawer(&task_id).contains(":PARENT_TASK:"),
        "seeding must land the legacy dead-key lines"
    );

    // The removal door that did not exist before the split.
    fx.run(&["node", "prop", "unset", &task_id, "PARENT_TASK"]);
    fx.run(&["node", "prop", "unset", &task_id, "LAST_UPDATED"]);
    let drawer = fx.drawer(&task_id);
    assert!(
        !drawer.contains("PARENT_TASK") && !drawer.contains("LAST_UPDATED"),
        "the dead keys must be gone after the unsets:\n{drawer}"
    );
    assert!(
        drawer.contains(":ID:"),
        "the removals must not have touched the identity line:\n{drawer}"
    );

    // Writing them is still refused BY NAME, and the refusal names the
    // removal door for the legacy lines instead of a dead end.
    let stderr = fx.refusal(&["node", "prop", "set", &task_id, "PARENT_TASK", &parent]);
    assert!(
        stderr.contains("PARENT_TASK"),
        "the set refusal must name the dead key: {stderr}"
    );
    assert!(
        stderr.contains("node prop unset"),
        "the set refusal must name the removal door for legacy lines: {stderr}"
    );

    // STATE is owned by another door, not merely dead: BOTH directions refuse.
    let stderr = fx.refusal(&["node", "prop", "set", &task_id, "STATE", "DONE"]);
    assert!(
        stderr.contains("lifecycle"),
        "the STATE set refusal must name the owning door: {stderr}"
    );
    let stderr = fx.refusal(&["node", "prop", "unset", &task_id, "STATE"]);
    assert!(
        stderr.contains("unsetting `STATE` is refused"),
        "the STATE unset refusal must stay: {stderr}"
    );
}

/// LOW-2: `FOO-BAR` is its own uppercase, so the canonical-spelling check
/// passed it — and the tx ledger (where every changed key is also recorded)
/// refused the shape only INSIDE the writer, surfacing as a 500 "failed to
/// apply changes" with the real reason confined to the daemon log. The shared
/// drawer check enforces the same `[A-Z][A-Z0-9_]*` now: a 400 naming the
/// flag, the passed key, and the underscored spelling — before any file is
/// read.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn wrong_shaped_drawer_keys_are_refused_as_400s_naming_the_flag() {
    let fx = Fixture::new("drawer-shape").await;
    let task_id = fx.create_task("a task offered a hyphenated key");

    let stderr = fx.refusal(&[
        "task",
        "update",
        "--project",
        "drawer-shape",
        &task_id,
        "--property",
        "FOO-BAR=1",
    ]);
    assert!(
        stderr.contains("--property") && stderr.contains("FOO-BAR"),
        "the refusal must name the flag and the passed key: {stderr}"
    );
    assert!(
        stderr.contains("[A-Z][A-Z0-9_]*") && stderr.contains("FOO_BAR"),
        "the refusal must state the shape rule and the underscored spelling: {stderr}"
    );
    assert!(
        !stderr.contains("failed to apply changes"),
        "the drawer 400 must arrive before the writer 500 could: {stderr}"
    );
    assert!(
        !fx.drawer(&task_id).contains("FOO"),
        "the refused write must not have touched the drawer:\n{}",
        fx.drawer(&task_id)
    );

    // The shared guard covers the node-editor verbs too, set and unset.
    let stderr = fx.refusal(&["node", "prop", "set", &task_id, "FOO-BAR", "1"]);
    assert!(
        stderr.contains("FOO_BAR"),
        "node prop set must refuse the shape with the spelling to use: {stderr}"
    );
    let stderr = fx.refusal(&["node", "prop", "unset", &task_id, "FOO-BAR"]);
    assert!(
        stderr.contains("FOO_BAR"),
        "node prop unset must name the spelling it would mean: {stderr}"
    );

    // A MISCASED compound gets ONE refusal pointing at the shape-correct
    // spelling — not a case correction into the shape refusal (the two-round-
    // trips-to-learn-one-rule shape this chain keeps closing).
    let stderr = fx.refusal(&[
        "task",
        "update",
        "--project",
        "drawer-shape",
        &task_id,
        "--property",
        "foo-bar=1",
    ]);
    assert!(
        stderr.contains("FOO_BAR"),
        "the miscased compound must be corrected all the way: {stderr}"
    );
    assert!(
        !stderr.contains("Use `FOO-BAR`"),
        "correcting to a shape the ledger refuses is not a correction: {stderr}"
    );

    // The underscored shape is open vocabulary and still writes — the refusal
    // is about the shape, not about the name.
    let changed = fx.json(&[
        "task",
        "update",
        "--project",
        "drawer-shape",
        &task_id,
        "--property",
        "FOO_BAR=1",
    ]);
    assert_eq!(
        changed["changed"]["FOO_BAR"].as_str(),
        Some("1"),
        "the underscored shape must write and echo: {changed}"
    );
    assert!(
        fx.drawer(&task_id).contains(":FOO_BAR: 1"),
        "the underscored shape must land in the drawer"
    );
}
