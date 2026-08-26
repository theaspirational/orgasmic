//! Integration tests against the real `.orgasmic/*.org` and
//! `shipped/**/*.org` fixtures committed to this repo. These tests prove the
//! orgasmic profile parser handles the live corpus and that rewriting one
//! heading does not perturb unrelated bytes.

use std::path::PathBuf;
use std::sync::{
    atomic::{AtomicUsize, Ordering},
    Arc,
};

use orgasmic_core::{
    collection_node_file_paths, org::OrgRewriter, parse_tx_file, DecisionNode, GlossaryTerm,
    LifecycleStage, OrgFile, ProjectFile, TaskHeading,
};
use tracing::span::{Attributes, Id, Record};
use tracing::{Event, Metadata, Subscriber};

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

fn read(rel: &str) -> String {
    let path = repo_root().join(rel);
    std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("read {rel}: {e}"))
}

fn parse_or_panic(rel: &str) -> OrgFile {
    let src = read(rel);
    OrgFile::parse(src, rel).unwrap_or_else(|e| panic!("parse {rel}: {e}"))
}

fn count_warnings(run: impl FnOnce()) -> usize {
    let warnings = Arc::new(AtomicUsize::new(0));
    let subscriber = WarningCounter {
        warnings: warnings.clone(),
    };
    tracing::dispatcher::with_default(&tracing::Dispatch::new(subscriber), run);
    warnings.load(Ordering::SeqCst)
}

#[test]
fn parses_real_done_tasks() {
    let path = ".orgasmic/tasks/TASK-VWBDJ/node.org";
    let f = parse_or_panic(path);
    let task_003 = f
        .find_by_id("TASK-VWBDJ")
        .expect("TASK-VWBDJ present in its node.org");
    let view = TaskHeading::from_heading(&f, task_003, path).unwrap();
    assert_eq!(view.id, "TASK-VWBDJ");
    assert!(view
        .write_scope
        .iter()
        .any(|s| s.starts_with("crates/orgasmic-core/src/")));
    assert!(view.produces.iter().any(|s| s.contains("org.rs")));
    let acceptance = view.acceptance.expect("acceptance section parsed");
    assert!(acceptance.contains("Slot compilation is strict"));
    // Every task node in the live corpus must parse with a recognized state
    // (this is what the test is actually trying to prove — that the schema
    // accepts the live corpus, regardless of which task happens to be DONE).
    let mut parsed_tasks = 0usize;
    let mut any_done = false;
    for node_path in collection_node_file_paths(&repo_root(), "tasks").unwrap() {
        let rel = node_path
            .strip_prefix(repo_root())
            .unwrap()
            .to_string_lossy()
            .into_owned();
        let f = parse_or_panic(&rel);
        for h in &f.headings {
            if let Ok(t) = TaskHeading::from_heading(&f, h, &rel) {
                parsed_tasks += 1;
                any_done |= t.lifecycle_stage == LifecycleStage::Done;
            }
        }
    }
    assert!(parsed_tasks > 0, "task corpus should contain tasks");
    assert!(any_done, "task corpus should include completed tasks");
}

#[test]
fn live_state_files_parse_without_retired_property_warnings() {
    let mut parsed_tasks = 0;
    let node_rels: Vec<String> = collection_node_file_paths(&repo_root(), "tasks")
        .unwrap()
        .into_iter()
        .map(|p| {
            p.strip_prefix(repo_root())
                .unwrap()
                .to_string_lossy()
                .into_owned()
        })
        .collect();
    let warnings = count_warnings(|| {
        for rel in &node_rels {
            let f = parse_or_panic(rel);
            for heading in &f.headings {
                let looks_like_task = heading
                    .property("ID")
                    .map(|id| id.starts_with("TASK-"))
                    .unwrap_or(false)
                    && heading.todo.is_some();
                if !looks_like_task {
                    continue;
                }
                TaskHeading::from_heading(&f, heading, rel).unwrap();
                parsed_tasks += 1;
            }
        }
    });

    assert!(parsed_tasks > 0, "live task corpus should contain tasks");
    assert_eq!(warnings, 0);
}

#[test]
fn lifecycle_stage_parses_from_heading_todo_keyword() {
    let source = "#+title: sprint\n\n* IN_PROGRESS TASK-042 Do it\n:PROPERTIES:\n:ID:               TASK-042\n:END:\n";
    let file = OrgFile::parse(source, "inline.org").unwrap();
    let heading = file.find_by_id("TASK-042").unwrap();
    let view = TaskHeading::from_heading(&file, heading, "inline.org").unwrap();
    assert_eq!(view.lifecycle_stage, LifecycleStage::InProgress);
}

struct WarningCounter {
    warnings: Arc<AtomicUsize>,
}

impl Subscriber for WarningCounter {
    fn enabled(&self, metadata: &Metadata<'_>) -> bool {
        *metadata.level() <= tracing::Level::WARN
    }

    fn new_span(&self, _span: &Attributes<'_>) -> Id {
        Id::from_u64(1)
    }

    fn record(&self, _span: &Id, _values: &Record<'_>) {}

    fn record_follows_from(&self, _span: &Id, _follows: &Id) {}

    fn event(&self, event: &Event<'_>) {
        if *event.metadata().level() == tracing::Level::WARN {
            self.warnings.fetch_add(1, Ordering::SeqCst);
        }
    }

    fn enter(&self, _span: &Id) {}

    fn exit(&self, _span: &Id) {}
}

#[test]
fn old_ready_keyword_is_not_a_task_todo_keyword() {
    let source = "#+title: sprint\n\n* READY TASK-999 Old state\n:PROPERTIES:\n:ID:               TASK-999\n:END:\n";
    let file = OrgFile::parse(source, "inline.org").unwrap();
    let heading = file.find_by_id("TASK-999").unwrap();
    assert!(heading.todo.is_none());
    assert_eq!(heading.title, "READY TASK-999 Old state");
    assert!(TaskHeading::from_heading(&file, heading, "inline.org").is_err());
}

#[test]
fn lifecycle_stage_round_trips() {
    let stage: LifecycleStage = "backlog".parse().unwrap();
    assert_eq!(stage, LifecycleStage::Backlog);
    assert_eq!(stage.as_str(), "backlog");
    let json = serde_json::to_string(&stage).unwrap();
    assert_eq!(json, "\"backlog\"");
    let back: LifecycleStage = serde_json::from_str(&json).unwrap();
    assert_eq!(back, stage);
}

#[test]
fn task_heading_from_heading_tolerates_heading_id_token_mismatch() {
    let source = "#+title: sprint\n\n\
        * BACKLOG TASK-WRONG Display copy drift\n\
        :PROPERTIES:\n\
        :ID:               TASK-RIGHT\n\
        \
        :END:\n";
    let file = OrgFile::parse(source, "inline.org").unwrap();
    let heading = file.find_by_id("TASK-RIGHT").unwrap();
    let view = TaskHeading::from_heading(&file, heading, "inline.org").unwrap();
    assert_eq!(view.id, "TASK-RIGHT");
    assert_eq!(view.title, "TASK-WRONG Display copy drift");
}

#[test]
fn task_heading_parent_task_is_derived_from_id() {
    let source = "#+title: sprint\n\n* BACKLOG TASK-038.1 Child\n:PROPERTIES:\n:ID:               TASK-038.1\n:END:\n";
    let file = OrgFile::parse(source, "inline.org").unwrap();
    let heading = file.find_by_id("TASK-038.1").unwrap();
    let view = TaskHeading::from_heading(&file, heading, "inline.org").unwrap();
    assert_eq!(view.parent_task.as_deref(), Some("TASK-038"));
}

#[test]
fn task_heading_provider_model_effort_properties_are_parsed() {
    let source = "#+title: sprint\n\n* IN_PROGRESS TASK-059 Match workers\n:PROPERTIES:\n:ID:               TASK-059\n:PROVIDER:          OpenAI \n:MODEL:             gpt-5.5\n:REASONING_EFFORT:  xhigh\n:END:\n";
    let file = OrgFile::parse(source, "inline.org").unwrap();
    let heading = file.find_by_id("TASK-059").unwrap();
    let view = TaskHeading::from_heading(&file, heading, "inline.org").unwrap();
    assert_eq!(view.provider, Some("OpenAI"));
    assert_eq!(view.model, Some("gpt-5.5"));
    assert_eq!(view.reasoning_effort, Some("xhigh"));
}

#[test]
fn task_heading_empty_provider_model_effort_properties_are_dropped() {
    let source = "#+title: sprint\n\n* IN_PROGRESS TASK-059 Match workers\n:PROPERTIES:\n:ID:               TASK-059\n:PROVIDER:          \n:MODEL:             \t\n:REASONING_EFFORT:  \n:END:\n";
    let file = OrgFile::parse(source, "inline.org").unwrap();
    let heading = file.find_by_id("TASK-059").unwrap();
    let view = TaskHeading::from_heading(&file, heading, "inline.org").unwrap();
    assert_eq!(view.provider, None);
    assert_eq!(view.model, None);
    assert_eq!(view.reasoning_effort, None);
}

#[test]
fn parses_real_decisions() {
    let f = parse_or_panic(".orgasmic/decisions/dec_R75SW/node.org");
    assert!(!f.headings.is_empty());
    let dec_heading = f.find_by_id("dec_R75SW").expect("dec_R75SW present");
    let view =
        DecisionNode::from_heading(&f, dec_heading, ".orgasmic/decisions/dec_R75SW/node.org")
            .unwrap();
    assert_eq!(view.id, "dec_R75SW");
    assert!(!view.tags.is_empty(), "decision carries topic tag(s)");
    assert!(
        !view
            .context
            .as_deref()
            .map(str::trim)
            .unwrap_or("")
            .is_empty(),
        "ADR record has a Context section"
    );
    assert!(
        !view
            .decision
            .as_deref()
            .map(str::trim)
            .unwrap_or("")
            .is_empty(),
        "ADR record has a Decision section"
    );
}

#[test]
fn parses_real_glossary() {
    let f = parse_or_panic(".orgasmic/glossary/term_YC32J/node.org");
    let tx_term = f.find_by_id("term_YC32J").expect("term term_YC32J present");
    let view =
        GlossaryTerm::from_heading(tx_term, ".orgasmic/glossary/term_YC32J/node.org").unwrap();
    assert_eq!(view.canonical, Some("tx file"));
    assert!(view.definition.unwrap().contains("append-only audit"));
}

#[test]
fn parses_real_project() {
    let f = parse_or_panic(".orgasmic/project.org");
    let view = ProjectFile::from_org(&f, ".orgasmic/project.org").unwrap();
    assert_eq!(view.id, "orgasmic");
    assert!(view.mission.unwrap().contains("orgasmic coordinates"));
}

#[test]
fn parses_real_tx_file() {
    let tx_path = std::fs::read_dir(repo_root().join(".orgasmic/tx"))
        .unwrap()
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .find(|path| path.extension().and_then(|ext| ext.to_str()) == Some("org"))
        .expect("at least one live tx fixture");
    let rel = tx_path.strip_prefix(repo_root()).unwrap().to_string_lossy();
    let src = std::fs::read_to_string(&tx_path).unwrap_or_else(|e| panic!("read {rel}: {e}"));
    let entries = parse_tx_file(&src, &rel).unwrap();
    assert!(!entries.is_empty());
    assert!(entries.iter().all(|e| !e.tx_id.is_empty()));
    let last = entries.last().unwrap();
    assert!(!last.actor.is_empty());
}

#[test]
fn parses_shipped_schema_files() {
    parse_or_panic("shipped/schema/tx.org");
    parse_or_panic("shipped/schema/journal.org");
    parse_or_panic("shipped/prompt-studio/slots.org");
    parse_or_panic("shipped/schema/state-machine.org");
    parse_or_panic("shipped/schema/node-types/task.org");
    parse_or_panic("shipped/schema/node-types/decision.org");
    parse_or_panic("shipped/schema/node-types/glossary.org");
    parse_or_panic("shipped/schema/node-types/artifact.org");
    parse_or_panic("shipped/entry/router.org");
    parse_or_panic("shipped/workflows/default.org");
}

#[test]
fn parses_shipped_project_scaffold() {
    for name in [
        "shipped/project-scaffold/tasks/TASK-C9V29/node.org",
        "shipped/project-scaffold/tasks/TASK-C9V29.1/node.org",
        "shipped/project-scaffold/tasks/TASK-C9V29.2/node.org",
        "shipped/project-scaffold/tasks/TASK-C9V29.3/node.org",
    ] {
        parse_or_panic(name);
    }
    parse_or_panic("shipped/project-scaffold/tasks/goal.org");
    parse_or_panic("shipped/project-scaffold/tasks/handoff.org");
    // Project scaffold uses {{PROJECT_NAME}} placeholders; the parser must
    // still accept it because slot syntax is not Org syntax.
    parse_or_panic("shipped/project-scaffold/project.org");
    parse_or_panic("shipped/project-scaffold/entry.org");
}

#[test]
fn shipped_scaffold_ships_no_aggregate_task_state_files() {
    // Node-dir layout (dec_E01MC): the scaffold ships the bootstrap tree as
    // node dirs plus the goal.org/handoff.org singletons. An aggregate
    // per-state file reappearing would resurrect the retired layout.
    let dir = repo_root().join("shipped/project-scaffold/tasks");
    for entry in std::fs::read_dir(&dir).unwrap() {
        let path = entry.unwrap().path();
        if path.is_file() {
            let name = path.file_name().unwrap().to_string_lossy().into_owned();
            assert!(
                name == "goal.org" || name == "handoff.org",
                "unexpected aggregate task file in scaffold: {name}"
            );
        }
    }
}

#[test]
fn shipped_scaffold_seeds_bootstrap_task_tree() {
    // A freshly scaffolded project starts with one bootstrap task (a minted
    // bootstrap id) and three subtasks: infer-project, infer-decisions, then
    // migrate-instructions. Every heading must be schema-valid so the daemon
    // can index a just-bootstrapped project, and the parent/subtask structure +
    // ordering must hold (dec_056). Under the node-dir layout (dec_E01MC)
    // the tree ships as one node dir per task.
    // orgasmic:TASK-RQ270.5 — infer-architecture was removed with the
    // architecture layer (dec_HBK6A); migrate-instructions took the .3 slot.
    let rels = [
        "shipped/project-scaffold/tasks/TASK-C9V29/node.org",
        "shipped/project-scaffold/tasks/TASK-C9V29.1/node.org",
        "shipped/project-scaffold/tasks/TASK-C9V29.2/node.org",
        "shipped/project-scaffold/tasks/TASK-C9V29.3/node.org",
    ];
    let files: Vec<(&str, OrgFile)> = rels.iter().map(|rel| (*rel, parse_or_panic(rel))).collect();
    let tasks: Vec<TaskHeading> = files
        .iter()
        .flat_map(|(rel, f)| {
            f.headings.iter().map(move |h| {
                TaskHeading::from_heading(f, h, rel).expect("bootstrap task is schema-valid")
            })
        })
        .collect();

    let parent = tasks
        .iter()
        .find(|t| t.id == "TASK-C9V29")
        .expect("TASK-C9V29");
    assert_eq!(parent.lifecycle_stage, LifecycleStage::Backlog);
    assert!(parent.parent_task.is_none());

    for id in ["TASK-C9V29.1", "TASK-C9V29.2", "TASK-C9V29.3"] {
        let sub = tasks
            .iter()
            .find(|t| t.id == id)
            .unwrap_or_else(|| panic!("{id}"));
        assert_eq!(sub.parent_task.as_deref(), Some("TASK-C9V29"));
        assert_eq!(sub.lifecycle_stage, LifecycleStage::Backlog);
    }
    assert!(
        !tasks.iter().any(|t| t.id == "TASK-C9V29.4"),
        "bootstrap tree must not keep a fourth subtask after architecture excision"
    );
    // migrate-instructions depends on infer-decisions, which depends on
    // infer-project — so .orgasmic/ fills in a sound order, with no
    // architecture.org write scope left in the seed.
    let migrate = tasks.iter().find(|t| t.id == "TASK-C9V29.3").unwrap();
    assert!(
        migrate.title.contains("migrate-instructions"),
        "TASK-C9V29.3 must be migrate-instructions, not infer-architecture"
    );
    assert!(migrate
        .write_scope
        .iter()
        .all(|s| !s.contains("architecture.org")));
    assert!(migrate.depends_on.contains(&"TASK-C9V29.2"));
    let decisions = tasks.iter().find(|t| t.id == "TASK-C9V29.2").unwrap();
    assert!(decisions.depends_on.contains(&"TASK-C9V29.1"));
}

#[test]
fn round_trip_rewrite_is_byte_stable_outside_touched_heading() {
    // The live task corpus and the scaffold are one heading per node file
    // now; the multi-heading byte-stability property is proven on a synthetic
    // multi-heading source instead.
    let path = "inline-multi.org";
    let original = String::from(
        "#+title: sprint\n\n\
         * BACKLOG TASK-AAA First\n:PROPERTIES:\n:ID:               TASK-AAA\n:END:\n** Description\nAlpha.\n\n\
         * BACKLOG TASK-BBB Second\n:PROPERTIES:\n:ID:               TASK-BBB\n:PRIORITY:         P2\n:END:\n** Description\nBeta.\n\n\
         * BACKLOG TASK-CCC Third\n:PROPERTIES:\n:ID:               TASK-CCC\n:END:\n** Description\nGamma.\n",
    );
    let parsed = OrgFile::parse(original.clone(), path).unwrap();
    let mut rw = OrgRewriter::new(&parsed, path);
    // Touch only TASK-BBB's PRIORITY property.
    rw.set_property("TASK-BBB", "PRIORITY", "P0").unwrap();
    let rewritten = rw.finish();
    assert_ne!(rewritten, original, "rewrite must change the file");
    // Every other top-level heading should still appear at the same offset.
    let original_parsed = OrgFile::parse(&original, path).unwrap();
    let rewritten_parsed = OrgFile::parse(&rewritten, path).unwrap();
    assert_eq!(
        original_parsed.headings.len(),
        rewritten_parsed.headings.len()
    );
    for (a, b) in original_parsed
        .headings
        .iter()
        .zip(rewritten_parsed.headings.iter())
    {
        if a.property("ID") == Some("TASK-BBB") {
            continue;
        }
        assert_eq!(
            original_parsed.slice(a.span.clone()),
            rewritten_parsed.slice(b.span.clone()),
            "heading {} must be byte-identical after touching a different heading",
            a.title
        );
    }
}

#[test]
fn round_trip_through_section_body_rewrite() {
    let path = ".orgasmic/tasks/TASK-VWBDJ/node.org";
    let original = read(path);
    let parsed = OrgFile::parse(original.clone(), path).unwrap();
    let mut rw = OrgRewriter::new(&parsed, path);
    rw.set_section_body(
        "TASK-VWBDJ",
        "Worklog",
        "- [2026-05-21 Thu 21:00] Implemented orgasmic-core.\n",
    )
    .unwrap();
    let rewritten = rw.finish();
    let reparsed = OrgFile::parse(&rewritten, path).unwrap();
    let updated = reparsed.find_by_id("TASK-VWBDJ").unwrap();
    let worklog = updated.section("Worklog").unwrap();
    assert_eq!(
        reparsed.slice(worklog.body.clone()),
        "- [2026-05-21 Thu 21:00] Implemented orgasmic-core.\n"
    );
    // The untouched Description section's bytes are unchanged.
    let original_parsed = OrgFile::parse(&original, path).unwrap();
    let desc_orig = original_parsed
        .find_by_id("TASK-VWBDJ")
        .unwrap()
        .section("Description")
        .unwrap();
    let desc_new = updated.section("Description").unwrap();
    assert_eq!(
        reparsed.slice(desc_new.body.clone()),
        original_parsed.slice(desc_orig.body.clone()),
    );
}
