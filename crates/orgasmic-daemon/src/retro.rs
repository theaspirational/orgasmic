//! Manual diagnostic scopes. No supervisor, writer, lease, or recovery handles.
use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use anyhow::{ensure, Context, Result};
use orgasmic_core::{fold_dispatches, TxEntry};
use orgasmic_drivers::TranscriptRoots;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use crate::native_evidence::{
    cache_dir, digest, read_bounded, verified_source, CACHE_LIMIT, CONVERTER_VERSION, SOURCE_LIMIT,
};
use crate::run_catalog::RunCatalogEntry;

const MANIFEST_LIMIT: usize = 256 * 1024;
const REPORT_LIMIT: usize = 128 * 1024;

#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Selectors {
    #[serde(default)]
    pub runs: Vec<String>,
    #[serde(default)]
    pub tasks: Vec<String>,
    #[serde(default)]
    pub task_sequence: Vec<String>,
    #[serde(default)]
    pub questions: Vec<String>,
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Scope {
    pub version: u32,
    pub converter_version: u32,
    pub id: String,
    pub project: String,
    pub created_at: String,
    pub selectors: Selectors,
    pub runs: Vec<ScopedRun>,
    pub tasks: Vec<Value>,
    pub task_history: Vec<Value>,
    pub missing_evidence: Vec<String>,
    pub prompt: String,
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ScopedRun {
    pub run_id: String,
    pub summary: Value,
    pub task_ids: Vec<String>,
    pub dispatch_ids: Vec<String>,
    pub session_path: PathBuf,
    pub operational_sha256: Option<String>,
    pub native_sha256: Option<String>,
    pub missing_evidence: Vec<String>,
}

fn scope_dir(root: &Path, id: &str) -> Result<PathBuf> {
    uuid::Uuid::parse_str(id).context("invalid retrospective id")?;
    cache_dir(root, &format!("retro:{id}"))
}

// Publish once; a failed or interrupted worker leaves its immutable scope for inspection.
fn publish(path: &Path, value: &impl Serialize, limit: usize) -> Result<()> {
    use std::io::Write;
    let bytes = serde_json::to_vec_pretty(value)?;
    ensure!(
        bytes.len() <= limit,
        "diagnostic artifact exceeds {limit} bytes"
    );
    let parent = path.parent().context("artifact parent missing")?;
    let tmp = parent.join(format!(".{}.tmp", uuid::Uuid::new_v4()));
    let result = (|| -> Result<()> {
        let mut file = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&tmp)?;
        file.write_all(&bytes)?;
        file.sync_all()?;
        std::fs::hard_link(&tmp, path)
            .context("artifact already submitted or publication failed")?;
        std::fs::File::open(parent)?.sync_all()?;
        Ok(())
    })();
    let _ = std::fs::remove_file(tmp);
    result
}

/// Read the catalog vocabulary without refresh_dir's durable tombstone reconciliation.
pub(crate) fn catalog_entries(root: &Path, project: &str) -> Result<Vec<RunCatalogEntry>> {
    let dir = orgasmic_core::project_sessions_dir(root);
    let paths = match std::fs::read_dir(dir) {
        Ok(paths) => paths,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(e.into()),
    };
    let mut entries = Vec::new();
    for path in paths {
        let path = path?.path();
        if path.extension().and_then(|e| e.to_str()) != Some("jsonl") {
            continue;
        }
        let meta = std::fs::symlink_metadata(&path)?;
        ensure!(meta.is_file(), "catalog session must be a regular file");
        let scan = orgasmic_core::scan_session_lifecycle(
            &path,
            orgasmic_core::SessionScanBudget::DEFAULT,
        )?;
        entries.push(crate::run_catalog::entry_from_scan(
            &scan,
            &path,
            Some(project),
            root,
            crate::run_catalog::SessionFileFingerprint::of(&meta),
        ));
    }
    Ok(entries)
}

// Explicit immutable inputs keep this function independent of daemon mutation handles.
#[allow(clippy::too_many_arguments)]
pub fn prepare(
    root: &Path,
    project: &str,
    selectors: Selectors,
    mut entries: Vec<RunCatalogEntry>,
    tasks: Vec<Value>,
    tx: &[TxEntry],
    prompt: String,
    roots: &TranscriptRoots,
) -> Result<Value> {
    ensure!(
        selectors.runs.len() + selectors.tasks.len() + selectors.task_sequence.len() <= 100,
        "scope allows at most 100 selectors"
    );
    ensure!(
        !selectors.runs.is_empty()
            || !selectors.tasks.is_empty()
            || !selectors.task_sequence.is_empty(),
        "explicit run or task selection required"
    );
    ensure!(
        selectors.task_sequence.is_empty()
            || (selectors.tasks.is_empty() && selectors.runs.is_empty()),
        "task sequence cannot be mixed with other selectors"
    );
    ensure!(
        selectors.questions.len() <= 20
            && selectors
                .questions
                .iter()
                .all(|q| !q.trim().is_empty() && q.len() <= 4096),
        "questions must be nonempty, at most 20 of 4096 bytes"
    );
    let task_ids = if selectors.task_sequence.is_empty() {
        &selectors.tasks
    } else {
        &selectors.task_sequence
    };
    ensure!(
        task_ids.iter().collect::<BTreeSet<_>>().len() == task_ids.len(),
        "duplicate task selector"
    );
    for id in task_ids {
        ensure!(
            tasks.iter().any(|t| t["id"].as_str() == Some(id)),
            "unknown task {id}"
        );
    }
    let dispatches = fold_dispatches(tx);
    let links = |entry: &RunCatalogEntry| {
        let related: Vec<_> = dispatches
            .iter()
            .filter(|d| d.run_ids.contains(&entry.run_id))
            .collect();
        let mut ids: BTreeSet<String> = entry
            .task_id
            .as_deref()
            .unwrap_or_default()
            .split_whitespace()
            .map(str::to_owned)
            .collect();
        for d in &related {
            ids.extend(
                d.started
                    .task
                    .as_deref()
                    .unwrap_or_default()
                    .split_whitespace()
                    .map(str::to_owned),
            );
        }
        (
            ids.into_iter().collect::<Vec<_>>(),
            related
                .iter()
                .map(|d| d.started.tx_id.clone())
                .collect::<Vec<_>>(),
        )
    };
    // Catalog summaries only: never enumerate providers, attach, classify recovery, or convert here.
    entries.retain(|entry| {
        selectors.runs.contains(&entry.run_id)
            || links(entry).0.iter().any(|id| task_ids.contains(id))
    });
    for id in &selectors.runs {
        ensure!(
            entries.iter().any(|e| &e.run_id == id),
            "unknown run {id} in selected project"
        );
    }
    ensure!(
        entries.len() <= 100,
        "scope exceeds 100 runs; select a smaller scope"
    );
    entries.sort_by_key(|e| {
        let task_order = if selectors.task_sequence.is_empty() {
            0
        } else {
            links(e)
                .0
                .iter()
                .filter_map(|id| task_ids.iter().position(|t| t == id))
                .min()
                .unwrap_or(usize::MAX)
        };
        (
            task_order,
            e.lifecycle_envelopes.first().map(|e| e.time),
            e.run_id.clone(),
        )
    });
    ensure!(
        entries
            .iter()
            .map(|e| &e.run_id)
            .collect::<BTreeSet<_>>()
            .len()
            == entries.len(),
        "ambiguous duplicate run identity"
    );
    let mut missing = Vec::new();
    for task in task_ids {
        if !entries.iter().any(|e| links(e).0.contains(task)) {
            missing.push(format!("task {task}: no catalog runs"));
        }
    }
    let canonical_sessions = orgasmic_core::project_sessions_dir(root)
        .canonicalize()
        .ok();
    let mut runs = Vec::new();
    for entry in entries {
        let (task_ids, dispatch_ids) = links(&entry);
        let mut missing_evidence = Vec::new();
        let session_path = entry
            .session_path
            .canonicalize()
            .unwrap_or(entry.session_path.clone());
        ensure!(
            canonical_sessions
                .as_ref()
                .is_some_and(|dir| session_path.starts_with(dir)),
            "run session is outside project sessions"
        );
        let operational_sha256 = match read_bounded(&session_path, SOURCE_LIMIT) {
            Ok(bytes) => Some(digest(&bytes)),
            Err(e) => {
                missing_evidence.push(format!("operational: {e}"));
                None
            }
        };
        let native_sha256 = match verified_source(&entry.run_id, &session_path, roots) {
            Ok((_, bytes)) => Some(digest(&bytes)),
            Err(e) => {
                missing_evidence.push(format!("native: {e}"));
                None
            }
        };
        let summary = json!({"kind":entry.kind,"stage":entry.stage,"transport":entry.transport,"harness":entry.harness,
            "terminal":entry.terminal,"replacement_run_id":entry.replacement_run_id,"scan_truncated":entry.scan_truncated,
            "started_at":entry.lifecycle_envelopes.first().map(|e| e.time),"native":entry.native});
        runs.push(ScopedRun {
            run_id: entry.run_id,
            summary,
            task_ids,
            dispatch_ids,
            session_path,
            operational_sha256,
            native_sha256,
            missing_evidence,
        });
    }
    let all_tasks: BTreeSet<_> = task_ids
        .iter()
        .cloned()
        .chain(runs.iter().flat_map(|r| r.task_ids.iter().cloned()))
        .collect();
    let tasks = tasks
        .into_iter()
        .filter(|t| t["id"].as_str().is_some_and(|id| all_tasks.contains(id)))
        .collect();
    let task_history = tx
        .iter()
        .filter(|t| {
            t.task
                .as_deref()
                .unwrap_or_default()
                .split_whitespace()
                .any(|id| all_tasks.contains(id))
        })
        .map(|t| json!({"id":t.tx_id,"at":t.time,"verb":t.ty,"task":t.task,"extra":t.extra}))
        .collect();
    let id = uuid::Uuid::new_v4().to_string();
    let scope = Scope {
        version: 1,
        converter_version: CONVERTER_VERSION,
        id: id.clone(),
        project: project.into(),
        created_at: chrono::Utc::now().to_rfc3339(),
        selectors,
        runs,
        tasks,
        task_history,
        missing_evidence: missing,
        prompt,
    };
    let dir = scope_dir(root, &id)?;
    let path = dir.join("scope.json");
    publish(&path, &scope, MANIFEST_LIMIT)?;
    Ok(
        json!({"id":id,"scope":path,"scope_sha256":digest(&read_bounded(&path, MANIFEST_LIMIT)?),"directory":dir,"run_count":scope.runs.len()}),
    )
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(tag = "op", rename_all = "snake_case", deny_unknown_fields)]
pub enum Request {
    Catalog,
    Materialize {
        run: String,
    },
    Read {
        run: String,
        offset: usize,
        limit: usize,
    },
    Submit {
        findings: Vec<Finding>,
        coverage: String,
        limitations: String,
    },
}
#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Finding {
    pub category: String,
    pub finding: String,
    pub evidence: Vec<Citation>,
}
#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Citation {
    pub run: String,
    pub source_sha256: String,
    pub source_line: usize,
}

pub async fn request(
    root: &Path,
    id: &str,
    expected_digest: &str,
    request: Request,
    roots: &TranscriptRoots,
) -> Result<Value> {
    let dir = scope_dir(root, id)?;
    let bytes = read_bounded(&dir.join("scope.json"), MANIFEST_LIMIT)?;
    ensure!(
        digest(&bytes) == expected_digest,
        "scope changed since dispatch"
    );
    let scope: Scope = serde_json::from_slice(&bytes)?;
    ensure!(
        scope.version == 1 && scope.id == id && scope.converter_version == CONVERTER_VERSION,
        "unsupported or mismatched scope"
    );
    let get_run = |id: &str| {
        scope
            .runs
            .iter()
            .find(|r| r.run_id == id)
            .context("run outside immutable scope")
    };
    let cache = |run: &ScopedRun| -> Result<Value> {
        let sha = run
            .native_sha256
            .as_deref()
            .context("native evidence missing at scope creation")?;
        let path =
            cache_dir(root, &run.run_id)?.join(format!("claude-v{CONVERTER_VERSION}-{sha}.json"));
        let value: Value = serde_json::from_slice(&read_bounded(&path, CACHE_LIMIT)?)?;
        ensure!(
            value["provenance"] == "derived_native"
                && value["run_id"] == run.run_id
                && value["source_sha256"] == sha
                && value["converter_version"] == CONVERTER_VERSION,
            "cache identity mismatch"
        );
        Ok(value)
    };
    match request {
        Request::Catalog => {
            let mut value = serde_json::to_value(&scope)?;
            value.as_object_mut().unwrap().remove("prompt");
            Ok(value)
        }
        Request::Materialize { run } => {
            let run = get_run(&run)?;
            let expected = run
                .native_sha256
                .as_deref()
                .context("native evidence missing at scope creation")?;
            ensure!(
                run.operational_sha256.as_deref()
                    == Some(digest(&read_bounded(&run.session_path, SOURCE_LIMIT)?).as_str()),
                "operational source changed; create a new scope"
            );
            let (native_id, source) = verified_source(&run.run_id, &run.session_path, roots)?;
            ensure!(
                digest(&source) == expected,
                "native source changed; create a new scope"
            );
            Ok(serde_json::to_value(
                crate::native_evidence::materialize_source(root, &run.run_id, native_id, source)
                    .await?,
            )?)
        }
        Request::Read { run, offset, limit } => {
            ensure!(
                (1..=10).contains(&limit),
                "read limit must be 1..10 records"
            );
            let value = cache(get_run(&run)?)?;
            let records = value["records"]
                .as_array()
                .context("invalid cache records")?;
            ensure!(offset <= records.len(), "offset beyond retained evidence");
            let mut page = Vec::new();
            let mut bytes = 0;
            for record in records.iter().skip(offset).take(limit) {
                let size = serde_json::to_vec(record)?.len();
                if bytes + size > 32 * 1024 {
                    break;
                }
                page.push(record);
                bytes += size;
            }
            // A single large multi-tool row must not make paging stick forever.
            let skipped = page.is_empty() && offset < records.len();
            let next = offset + page.len() + usize::from(skipped);
            Ok(
                json!({"run":run,"source_sha256":value["source_sha256"],"records":page,"skipped_oversized_record":skipped,"next_offset":next,"total_records":records.len()}),
            )
        }
        Request::Submit {
            findings,
            coverage,
            limitations,
        } => {
            ensure!(
                findings.len() <= 50
                    && !coverage.trim().is_empty()
                    && !limitations.trim().is_empty(),
                "report requires coverage and limitations, at most 50 findings"
            );
            for finding in &findings {
                ensure!(
                    !finding.finding.trim().is_empty()
                        && !finding.category.trim().is_empty()
                        && !finding.evidence.is_empty(),
                    "every finding requires a category, statement, and evidence"
                );
                for citation in &finding.evidence {
                    let run = get_run(&citation.run)?;
                    ensure!(
                        run.native_sha256.as_deref() == Some(citation.source_sha256.as_str()),
                        "citation digest outside scope"
                    );
                    let value = cache(run)?;
                    ensure!(value["records"].as_array().context("invalid cache")?.iter().any(|r| r["source_line"].as_u64() == Some(citation.source_line as u64)), "citation line is not retained evidence");
                }
            }
            let report = json!({"provenance":"derived_retrospective","scope_id":id,"scope_sha256":expected_digest,"terminal_declaration":"report_submitted","findings":findings,"coverage":coverage,"limitations":limitations});
            let path = dir.join("report.json");
            publish(&path, &report, REPORT_LIMIT)?;
            Ok(json!({"status":"report_submitted","report":path,"scope":dir.join("scope.json")}))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use orgasmic_core::{RuntimeIdentity, SessionEventKind, SessionWriter};

    fn tx(id: &str, ty: &str, extra: &[(&str, &str)]) -> TxEntry {
        TxEntry {
            tx_id: id.into(),
            time: "[2026-09-07 Mon 12:00:00]".into(),
            ty: ty.into(),
            actor: "fixture".into(),
            machine: "fixture".into(),
            project: Some("proj".into()),
            task: Some("TASK-RETRO".into()),
            target: None,
            reason: None,
            extra: extra
                .iter()
                .map(|(k, v)| (k.to_string(), v.to_string()))
                .collect(),
        }
    }

    #[tokio::test]
    async fn manual_retro_pins_multi_role_recovery_scope_and_accepts_cross_run_evidence_only() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("project");
        let sessions = orgasmic_core::project_sessions_dir(&root);
        std::fs::create_dir_all(&sessions).unwrap();
        let roots = TranscriptRoots::from_home(tmp.path().join("vendor"));
        std::fs::create_dir_all(&roots.claude_projects).unwrap();
        let mut originals = Vec::new();
        for (run, kind, task) in [
            ("run-1", "implementer", Some("TASK-RETRO")),
            ("run-2", "reviewer", Some("TASK-RETRO")),
            ("run-3", "recovery", None),
            ("run-other", "implementer", Some("TASK-OTHER TASK-EXTRA")),
        ] {
            let native = roots.claude_projects.join(format!("{run}.jsonl"));
            let line = json!({"type":"assistant","sessionId":run,"timestamp":"2026-09-07T12:00:00Z","message":{"id":run,"usage":{"input_tokens":100,"output_tokens":20},"content":[{"type":"tool_use","id":"read-1","name":"Read","input":{"path":"src/lib.rs"}}]}});
            let result = json!({"type":"user","sessionId":run,"timestamp":"2026-09-07T12:00:02Z","message":{"content":[{"type":"tool_result","tool_use_id":"read-1","content":"x".repeat(100_000),"is_error":kind == "implementer"}]}});
            std::fs::write(&native, format!("{line}\n{result}\n")).unwrap();
            let session = sessions.join(format!("{run}.jsonl"));
            let mut writer =
                SessionWriter::open(&session, RuntimeIdentity::new(run, "old-boot")).unwrap();
            writer
                .append(
                    SessionEventKind::Lifecycle,
                    json!({"phase":"acquire","task_id":task,"kind":kind,"worker_id":"fixture"}),
                )
                .unwrap();
            // Missing worktree is deliberate: a diagnostic must not mint a tombstone.
            writer.append(SessionEventKind::Lifecycle,json!({"phase":"run_meta","harness":"claude","transport":"stdio","project_id":"proj","worktree":root.join("gone"),"driver_config":{}})).unwrap();
            writer.append(SessionEventKind::Lifecycle,json!({"phase":"native_runtime","provider":"claude","session_id":run,"session_path":native,"launch_argv":[],"resume_argv":[]})).unwrap();
            writer
                .append(
                    SessionEventKind::Lifecycle,
                    json!({"phase":"release","outcome":"failed","finalized_by_worker":false}),
                )
                .unwrap();
            drop(writer);
            originals.push((session.clone(), std::fs::read(session).unwrap()));
            originals.push((native.clone(), std::fs::read(native).unwrap()));
        }
        let ledger = vec![
            tx("tx-backlog", "task.created", &[("TO", "backlog")]),
            tx(
                "tx-progress",
                "task.lifecycle_changed",
                &[("FROM", "backlog"), ("TO", "in_progress")],
            ),
            tx(
                "tx-impl",
                "manager.dispatch_started",
                &[("KIND", "implementer")],
            ),
            tx(
                "tx-run",
                "run.created",
                &[
                    ("ORIGIN", "cli_dispatch"),
                    ("DISPATCH_TX", "tx-impl"),
                    ("RUN_ID", "run-1"),
                ],
            ),
            tx(
                "tx-recovery",
                "run.created",
                &[
                    ("ORIGIN", "recovery"),
                    ("ORIGIN_RUN_ID", "run-1"),
                    ("RUN_ID", "run-3"),
                ],
            ),
            tx(
                "tx-review",
                "manager.dispatch_started",
                &[("KIND", "reviewer")],
            ),
            tx(
                "tx-review-run",
                "run.created",
                &[
                    ("ORIGIN", "cli_dispatch"),
                    ("DISPATCH_TX", "tx-review"),
                    ("RUN_ID", "run-2"),
                ],
            ),
            tx(
                "tx-done",
                "task.lifecycle_changed",
                &[("FROM", "in_review"), ("TO", "done")],
            ),
        ];
        let tasks = vec![
            json!({"id":"TASK-RETRO","lifecycle_stage":"done"}),
            json!({"id":"TASK-OTHER","lifecycle_stage":"backlog"}),
        ];
        let prepare_scope = |selectors| {
            prepare(
                &root,
                "proj",
                selectors,
                catalog_entries(&root, "proj").unwrap(),
                tasks.clone(),
                &ledger,
                "compiled retro prompt".into(),
                &roots,
            )
            .unwrap()
        };
        let prepared = prepare_scope(Selectors {
            runs: vec![],
            tasks: vec!["TASK-RETRO".into()],
            task_sequence: vec![],
            questions: vec!["Where was work repeated?".into()],
        });
        let id = prepared["id"].as_str().unwrap();
        let hash = prepared["scope_sha256"].as_str().unwrap();
        let before_manifest = std::fs::read(prepared["scope"].as_str().unwrap()).unwrap();
        let catalog = request(&root, id, hash, Request::Catalog, &roots)
            .await
            .unwrap();
        assert_eq!(catalog["runs"].as_array().unwrap().len(), 3);
        assert_eq!(catalog["runs"][2]["run_id"], "run-3");
        assert_eq!(catalog["runs"][2]["dispatch_ids"], json!(["tx-impl"]));
        assert_eq!(catalog["task_history"].as_array().unwrap().len(), 8);
        assert!(serde_json::to_vec(&catalog).unwrap().len() < 16 * 1024);
        assert!(!catalog.to_string().contains(&"x".repeat(100)));
        assert!(!root.join(crate::run_catalog::TOMBSTONE_REL_PATH).exists());
        assert!(!cache_dir(&root, "run-1")
            .unwrap()
            .join(format!(
                "claude-v1-{}.json",
                catalog["runs"][0]["native_sha256"].as_str().unwrap()
            ))
            .exists());
        let mut evidence = Vec::new();
        for run in ["run-1", "run-3"] {
            for hit in [false, true] {
                let summary = request(
                    &root,
                    id,
                    hash,
                    Request::Materialize { run: run.into() },
                    &roots,
                )
                .await
                .unwrap();
                assert_eq!(summary["cache_hit"], hit);
                assert!(summary.get("records").is_none());
            }
            let page = request(
                &root,
                id,
                hash,
                Request::Read {
                    run: run.into(),
                    offset: 0,
                    limit: 10,
                },
                &roots,
            )
            .await
            .unwrap();
            assert_eq!(page["records"][0]["events"][0]["name"], "Read");
            assert!(serde_json::to_vec(&page).unwrap().len() < 32 * 1024 + 1024);
            evidence.push(Citation {
                run: run.into(),
                source_sha256: page["source_sha256"].as_str().unwrap().into(),
                source_line: 1,
            });
        }
        assert!(request(
            &root,
            id,
            hash,
            Request::Materialize {
                run: "run-other".into()
            },
            &roots
        )
        .await
        .is_err());
        assert!(
            request(
                &root,
                id,
                hash,
                Request::Read {
                    run: "run-2".into(),
                    offset: 0,
                    limit: 1
                },
                &roots
            )
            .await
            .is_err(),
            "unrequested native evidence was not materialized"
        );
        assert!(serde_json::from_value::<Request>(json!({"op":"release","run":"run-1"})).is_err());
        let bad = Request::Submit {
            findings: vec![Finding {
                category: "test".into(),
                finding: "invalid citation".into(),
                evidence: vec![Citation {
                    run: "run-1".into(),
                    source_sha256: evidence[0].source_sha256.clone(),
                    source_line: 999,
                }],
            }],
            coverage: "two runs".into(),
            limitations: "fixture".into(),
        };
        assert!(request(&root, id, hash, bad, &roots).await.is_err());
        let report = request(&root,id,hash,Request::Submit{findings:vec![Finding{category:"redundant reads".into(),finding:"Implementer and recovery both read src/lib.rs with equivalent arguments.".into(),evidence}],coverage:"Examined implementer and recovery; reviewer not needed for this finding. Token counters and two-second tool intervals observed.".into(),limitations:"Offline fixture; circular reasoning, wait causes and progress per token are unknown.".into()},&roots).await.unwrap();
        assert_eq!(report["status"], "report_submitted");
        let output: Value =
            serde_json::from_slice(&std::fs::read(report["report"].as_str().unwrap()).unwrap())
                .unwrap();
        assert_eq!(
            output["findings"][0]["evidence"].as_array().unwrap().len(),
            2
        );
        assert!(
            request(
                &root,
                id,
                hash,
                Request::Submit {
                    findings: vec![],
                    coverage: "all".into(),
                    limitations: "none".into()
                },
                &roots
            )
            .await
            .is_err(),
            "report is immutable"
        );
        for (path, bytes) in &originals {
            assert_eq!(&std::fs::read(path).unwrap(), bytes);
        }
        let changed = roots.claude_projects.join("run-1.jsonl");
        use std::io::Write;
        std::fs::OpenOptions::new()
            .append(true)
            .open(changed)
            .unwrap()
            .write_all(b"{\"sessionId\":\"run-1\"}\n")
            .unwrap();
        assert!(request(
            &root,
            id,
            hash,
            Request::Materialize {
                run: "run-1".into()
            },
            &roots
        )
        .await
        .unwrap_err()
        .to_string()
        .contains("source changed"));
        assert_eq!(
            std::fs::read(prepared["scope"].as_str().unwrap()).unwrap(),
            before_manifest
        );
        let sequence = prepare_scope(Selectors {
            runs: vec![],
            tasks: vec![],
            task_sequence: vec!["TASK-OTHER".into(), "TASK-RETRO".into()],
            questions: vec![],
        });
        let ordered = request(
            &root,
            sequence["id"].as_str().unwrap(),
            sequence["scope_sha256"].as_str().unwrap(),
            Request::Catalog,
            &roots,
        )
        .await
        .unwrap();
        assert_eq!(ordered["runs"][0]["run_id"], "run-other");
        assert!(request(&root, id, "changed", Request::Catalog, &roots)
            .await
            .is_err());
        assert!(!root.join(crate::run_catalog::TOMBSTONE_REL_PATH).exists());
    }
}
