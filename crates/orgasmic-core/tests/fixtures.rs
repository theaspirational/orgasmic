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
    is_supported_worker_pair, org::OrgRewriter, parse_tx_file, ArchEdgeKind, ArchEdgeTarget,
    ArchitectureNode, ArtifactScheme, DecisionNode, GlossaryTerm, LifecycleStage, OrgFile,
    ProjectFile, TaskHeading, Worker,
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
    let path = ".orgasmic/tasks/done.org";
    let f = parse_or_panic(path);
    let task_003 = f
        .find_by_id("TASK-VWBDJ")
        .expect("TASK-VWBDJ present in done.org");
    let view = TaskHeading::from_heading(&f, task_003, path).unwrap();
    assert_eq!(view.id, "TASK-VWBDJ");
    assert_eq!(view.worker, Some("implementer-claude"));
    assert!(view
        .write_scope
        .iter()
        .any(|s| s.starts_with("crates/orgasmic-core/src/")));
    assert!(view.produces.iter().any(|s| s.contains("org.rs")));
    let acceptance = view.acceptance.expect("acceptance section parsed");
    assert!(acceptance.contains("Slot compilation is strict"));
    // Every implementer task in the sprint must parse with a recognized state
    // (this is what the test is actually trying to prove — that the schema
    // accepts the live corpus, regardless of which task happens to be DONE).
    let parsed_tasks = f
        .headings
        .iter()
        .filter_map(|h| TaskHeading::from_heading(&f, h, path).ok())
        .collect::<Vec<_>>();
    assert!(!parsed_tasks.is_empty(), "done.org should contain tasks");
    assert!(
        parsed_tasks
            .iter()
            .any(|t| t.lifecycle_stage == LifecycleStage::Done),
        "done.org should include completed tasks"
    );
}

#[test]
fn live_state_files_parse_without_legacy_worker_warnings() {
    let mut parsed_tasks = 0;
    let warnings = count_warnings(|| {
        for rel in [".orgasmic/tasks/backlog.org", ".orgasmic/tasks/done.org"] {
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
    let f = parse_or_panic(".orgasmic/decisions.org");
    assert!(!f.headings.is_empty());
    let dec_heading = f.find_by_id("dec_R75SW").expect("dec_R75SW present");
    let view = DecisionNode::from_heading(&f, dec_heading, ".orgasmic/decisions.org").unwrap();
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
fn parses_real_architecture() {
    let f = parse_or_panic(".orgasmic/architecture.org");
    let arch_heading = f.find_by_id("arch_BVH7M").expect("arch_BVH7M present");
    let view =
        ArchitectureNode::from_heading(&f, arch_heading, ".orgasmic/architecture.org").unwrap();
    assert_eq!(view.id, "arch_BVH7M");
    assert!(view.motivated_by.contains(&"dec_R75SW"));
    assert!(view.purpose.unwrap().contains("file profile"));
}

#[test]
fn architecture_top_level_only_parses_unchanged() {
    let source = "#+title: architecture\n\n* arch_001 Component\n:PROPERTIES:\n:ID:                 arch_001\n:DEPENDS_ON:         arch_002\n:MOTIVATED_BY:       dec_001\n:END:\n";
    let file = OrgFile::parse(source, "inline.org").unwrap();
    let nodes = ArchitectureNode::from_org(&file, "inline.org").unwrap();
    assert_eq!(nodes.len(), 1);
    assert_eq!(nodes[0].id, "arch_001");
    assert_eq!(nodes[0].label, "Component");
    assert_eq!(nodes[0].parent_id, None);
    assert_eq!(nodes[0].depends_on, vec!["arch_002"]);
}

#[test]
fn architecture_child_heading_parses_parent_label_body_and_source_paths() {
    let source = "#+title: architecture\n\n* arch_006 Daemon API\n:PROPERTIES:\n:ID:                 arch_006\n:END:\n\n** arch_006.3 Materialized index\n:PROPERTIES:\n:ID:                 arch_006.3\n:SOURCE_PATHS:       crates/orgasmic-daemon/src/index.rs\n:TESTS:              cargo test -p orgasmic-core; cargo test -p orgasmic-daemon\n:READS:              file:board\n:WRITES:             projection:materialized-index\n:END:\nReads project board.org to hydrate the in-memory project graph.\n";
    let file = OrgFile::parse(source, "inline.org").unwrap();
    let nodes = ArchitectureNode::from_org(&file, "inline.org").unwrap();
    let child = nodes.iter().find(|node| node.id == "arch_006.3").unwrap();
    assert_eq!(child.label, "Materialized index");
    assert_eq!(child.parent_id.as_deref(), Some("arch_006"));
    assert_eq!(
        child.source_paths,
        vec!["crates/orgasmic-daemon/src/index.rs"]
    );
    // `:TESTS:` is `;`-separated and preserves intra-command spaces.
    assert_eq!(
        child.tests,
        vec![
            "cargo test -p orgasmic-core".to_string(),
            "cargo test -p orgasmic-daemon".to_string(),
        ]
    );
    assert!(child
        .description
        .as_deref()
        .unwrap()
        .contains("hydrate the in-memory project graph"));
}

#[test]
fn architecture_multiple_children_and_nested_typed_edges_parse() {
    let source = "#+title: architecture\n\n* arch_004 Runtime supervisor\n:PROPERTIES:\n:ID:                 arch_004\n:END:\n\n** arch_004.1 Supervisor acquire\n:PROPERTIES:\n:ID:                 arch_004.1\n:CALLS:              arch_004.2 arch_006\n:SPAWNS:             arch_004.3\n:END:\n\n** arch_004.2 Worker trait\n:PROPERTIES:\n:ID:                 arch_004.2\n:READS:              file:sessions\n:END:\n\n** arch_004.3 Driver task\n:PROPERTIES:\n:ID:                 arch_004.3\n:SUBSCRIBES_TO:      socket:events\n:END:\n";
    let file = OrgFile::parse(source, "inline.org").unwrap();
    let nodes = ArchitectureNode::from_org(&file, "inline.org").unwrap();
    assert_eq!(nodes.len(), 4);
    let first = nodes.iter().find(|node| node.id == "arch_004.1").unwrap();
    assert!(first
        .edges
        .iter()
        .any(|edge| edge.kind == ArchEdgeKind::Calls
            && edge.target
                == (ArchEdgeTarget::Node {
                    id: "arch_004.2".into()
                })));
    assert!(first
        .edges
        .iter()
        .any(|edge| edge.kind == ArchEdgeKind::Spawns
            && edge.target
                == (ArchEdgeTarget::Node {
                    id: "arch_004.3".into()
                })));
}

#[test]
fn architecture_artifact_schemes_parse() {
    let source = "#+title: architecture\n\n* arch_006 Daemon API\n:PROPERTIES:\n:ID:                 arch_006\n:END:\n\n** arch_006.1 Router\n:PROPERTIES:\n:ID:                 arch_006.1\n:READS:              file:tx projection:materialized-index socket:events\n:END:\n";
    let file = OrgFile::parse(source, "inline.org").unwrap();
    let nodes = ArchitectureNode::from_org(&file, "inline.org").unwrap();
    let child = nodes.iter().find(|node| node.id == "arch_006.1").unwrap();
    let schemes = child
        .edges
        .iter()
        .filter_map(|edge| match &edge.target {
            ArchEdgeTarget::Artifact(artifact) => Some(artifact.scheme.clone()),
            ArchEdgeTarget::Node { .. } => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(
        schemes,
        vec![
            ArtifactScheme::File,
            ArtifactScheme::Projection,
            ArtifactScheme::Socket
        ]
    );
}

#[test]
fn architecture_invalid_artifact_scheme_is_parse_error() {
    let source = "#+title: architecture\n\n* arch_006 Daemon API\n:PROPERTIES:\n:ID:                 arch_006\n:END:\n\n** arch_006.1 Router\n:PROPERTIES:\n:ID:                 arch_006.1\n:READS:              unknown:foo\n:END:\n";
    let file = OrgFile::parse(source, "inline.org").unwrap();
    let err = ArchitectureNode::from_org(&file, "inline.org").unwrap_err();
    assert!(err.to_string().contains("unknown architecture namespace"));
}

#[test]
fn architecture_edge_property_splits_multiple_values() {
    let source = "#+title: architecture\n\n* arch_006 Daemon API\n:PROPERTIES:\n:ID:                 arch_006\n:END:\n\n** arch_006.2 HTTP router\n:PROPERTIES:\n:ID:                 arch_006.2\n:WRITES:             file:tx projection:materialized-index arch_006.3\n:END:\n";
    let file = OrgFile::parse(source, "inline.org").unwrap();
    let nodes = ArchitectureNode::from_org(&file, "inline.org").unwrap();
    let child = nodes.iter().find(|node| node.id == "arch_006.2").unwrap();
    let targets = child
        .edges
        .iter()
        .map(|edge| edge.target.id())
        .collect::<Vec<_>>();
    assert_eq!(
        targets,
        vec!["file:tx", "projection:materialized-index", "arch_006.3"]
    );
}

#[test]
fn parses_real_glossary() {
    let f = parse_or_panic(".orgasmic/glossary.org");
    let tx_term = f.find_by_id("term_YC32J").expect("term term_YC32J present");
    let view = GlossaryTerm::from_heading(tx_term, ".orgasmic/glossary.org").unwrap();
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
    parse_or_panic("shipped/prompt-studio/slots.org");
    parse_or_panic("shipped/schema/state-machine.org");
    parse_or_panic("shipped/entry/router.org");
}

#[test]
fn parses_shipped_project_scaffold() {
    for name in [
        "shipped/skills/orgasmic/scaffold/tasks/backlog.org",
        "shipped/skills/orgasmic/scaffold/tasks/todo.org",
        "shipped/skills/orgasmic/scaffold/tasks/in_progress.org",
        "shipped/skills/orgasmic/scaffold/tasks/in_review.org",
        "shipped/skills/orgasmic/scaffold/tasks/done.org",
        "shipped/skills/orgasmic/scaffold/tasks/cancelled.org",
    ] {
        parse_or_panic(name);
    }
    parse_or_panic("shipped/skills/orgasmic/scaffold/tasks/goal.org");
    parse_or_panic("shipped/skills/orgasmic/scaffold/tasks/handoff.org");
    // Project scaffold uses {{PROJECT_NAME}} placeholders; the parser must
    // still accept it because slot syntax is not Org syntax.
    parse_or_panic("shipped/skills/orgasmic/scaffold/decisions.org");
    parse_or_panic("shipped/skills/orgasmic/scaffold/project.org");
    parse_or_panic("shipped/skills/orgasmic/scaffold/entry.org");
    parse_or_panic("shipped/skills/orgasmic/scaffold/conventions/contributing.org");
    parse_or_panic("shipped/skills/orgasmic/scaffold/conventions/no-skill-installed.org");
    parse_or_panic("shipped/skills/orgasmic/scaffold/conventions/orgasmic-tooling.org");
}

#[test]
fn shipped_scaffold_state_files_ship_without_seed_headings() {
    // backlog.org is the exception: it ships the bootstrap task tree (see
    // shipped_scaffold_seeds_bootstrap_task_tree). The other five state files
    // must be empty — placeholder seeds trip the TASK-HC7PW phantom lint.
    for name in [
        "todo.org",
        "in_progress.org",
        "in_review.org",
        "done.org",
        "cancelled.org",
    ] {
        let rel = format!("shipped/skills/orgasmic/scaffold/tasks/{name}");
        let f = parse_or_panic(&rel);
        assert!(
            f.headings.is_empty(),
            "{rel} must not ship seed headings (TASK-HC7PW phantom lint)"
        );
    }
}

#[test]
fn shipped_scaffold_seeds_bootstrap_task_tree() {
    // A freshly scaffolded project starts with one bootstrap task (a minted bootstrap id)
    // and three inference subtasks. Every heading must be schema-valid so the
    // daemon can index a just-bootstrapped project, and the parent/subtask
    // structure + ordering must hold (dec_056). Under the file-per-state
    // layout (dec_QQYXM) the tree ships in backlog.org — its stage.
    let rel = "shipped/skills/orgasmic/scaffold/tasks/backlog.org";
    let f = parse_or_panic(rel);
    let tasks: Vec<TaskHeading> = f
        .headings
        .iter()
        .map(|h| TaskHeading::from_heading(&f, h, rel).expect("bootstrap task is schema-valid"))
        .collect();

    let parent = tasks
        .iter()
        .find(|t| t.id == "TASK-C9V29")
        .expect("TASK-C9V29");
    assert_eq!(parent.worker, Some("griller"));
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
    // infer-architecture is the architector; it depends on infer-decisions,
    // which depends on infer-project — so .orgasmic/ fills in a sound order.
    let arch = tasks.iter().find(|t| t.id == "TASK-C9V29.3").unwrap();
    assert_eq!(arch.worker, Some("architector"));
    assert!(arch
        .write_scope
        .iter()
        .any(|s| s.contains("architecture.org")));
    assert!(arch.depends_on.contains(&"TASK-C9V29.2"));
    let decisions = tasks.iter().find(|t| t.id == "TASK-C9V29.2").unwrap();
    assert!(decisions.depends_on.contains(&"TASK-C9V29.1"));
}

#[test]
fn parses_every_shipped_worker() {
    let root = repo_root().join("shipped/workers");
    let mut count = 0;
    for entry in std::fs::read_dir(&root).unwrap() {
        let path = entry.unwrap().path();
        if path.extension().and_then(|ext| ext.to_str()) != Some("org") {
            continue;
        }
        let rel = path.strip_prefix(repo_root()).unwrap().to_string_lossy();
        let file = parse_or_panic(&rel);
        let worker = Worker::from_org(&file, &rel).unwrap();
        assert!(
            is_supported_worker_pair(worker.driver, worker.harness),
            "{} declares unsupported pair {}/{}",
            rel,
            worker.driver,
            worker.harness
        );
        count += 1;
    }
    assert_eq!(count, 20);
}

#[test]
fn round_trip_rewrite_is_byte_stable_outside_touched_heading() {
    let path = ".orgasmic/tasks/done.org";
    let original = read(path);
    let parsed = OrgFile::parse(original.clone(), path).unwrap();
    let mut rw = OrgRewriter::new(&parsed, path);
    // Touch only TASK-VWBDJ's PRIORITY property.
    rw.set_property("TASK-VWBDJ", "PRIORITY", "P0").unwrap();
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
        if a.property("ID") == Some("TASK-VWBDJ") {
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
    let path = ".orgasmic/tasks/done.org";
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
    // TASK-V3DWJ's content unchanged.
    let task_004 = reparsed.find_by_id("TASK-V3DWJ").unwrap();
    let original_parsed = OrgFile::parse(&original, path).unwrap();
    let task_004_orig = original_parsed.find_by_id("TASK-V3DWJ").unwrap();
    assert_eq!(
        reparsed.slice(task_004.span.clone()),
        original_parsed.slice(task_004_orig.span.clone()),
    );
}
