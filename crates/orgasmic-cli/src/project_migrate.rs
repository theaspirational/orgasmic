use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{bail, Context, Result};
use orgasmic_core::node_kernel::{append_entry, journal_header, node_org_header, JournalEntry};
use orgasmic_core::OrgFile;

const TASK_STATES: [&str; 6] = [
    "backlog",
    "todo",
    "in_progress",
    "in_review",
    "done",
    "cancelled",
];

struct Collection {
    name: &'static str,
    prefix: &'static str,
    label: &'static str,
    sources: Vec<PathBuf>,
}

#[derive(Default)]
struct Migration {
    nodes: BTreeMap<PathBuf, (String, String)>,
    rewrites: BTreeMap<PathBuf, String>,
    old_files: Vec<PathBuf>,
    headings: BTreeMap<&'static str, usize>,
    in_place_nodes: usize,
    bytes: usize,
    project_source: String,
}

pub(crate) fn run(dry_run: bool) -> Result<()> {
    let root = std::env::current_dir().context("current directory")?;
    refuse_dirty_tree(&root)?;
    let migration = plan(&root)?;
    if migration.old_files.is_empty() {
        println!("already migrated");
        return Ok(());
    }
    if !dry_run {
        apply(&root, &migration)?;
    }
    println!("{}", if dry_run { "DRY RUN" } else { "MIGRATED" });
    for (collection, count) in &migration.headings {
        println!("  {collection}.nodes {count}");
    }
    println!(
        "  nodes {}",
        migration.nodes.len() + migration.in_place_nodes
    );
    println!("  bytes {}", migration.bytes);
    println!("  anomalies 0");
    println!("  heading_round_trip byte-for-byte");
    Ok(())
}

fn refuse_dirty_tree(root: &Path) -> Result<()> {
    let output = Command::new("git")
        .args(["status", "--porcelain=v1", "-z", "--untracked-files=all"])
        .current_dir(root)
        .output()
        .context("git status")?;
    if !output.status.success() {
        bail!(
            "git status failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }
    if !output.stdout.is_empty() {
        bail!("refusing to migrate a dirty git tree");
    }
    Ok(())
}

fn plan(root: &Path) -> Result<Migration> {
    let dotorg = root.join(".orgasmic");
    let project_path = dotorg.join("project.org");
    let project_source = std::fs::read_to_string(&project_path)
        .with_context(|| format!("read {}", project_path.display()))?;
    let already_v2 = project_source
        .lines()
        .any(|line| line.trim() == "#+orgasmic_version: 2");
    let collections = [
        Collection {
            name: "tasks",
            prefix: "TASK-",
            label: "task",
            sources: TASK_STATES
                .iter()
                .map(|state| dotorg.join("tasks").join(format!("{state}.org")))
                .collect(),
        },
        Collection {
            name: "decisions",
            prefix: "dec_",
            label: "decision",
            sources: vec![dotorg.join("decisions.org")],
        },
        Collection {
            name: "glossary",
            prefix: "term_",
            label: "glossary term",
            sources: vec![dotorg.join("glossary.org")],
        },
    ];
    let present = collections
        .iter()
        .flat_map(|collection| &collection.sources)
        .filter(|path| path.exists())
        .count();
    let artifacts_dir = dotorg.join("artifacts");
    let has_legacy_artifacts = std::fs::read_dir(&artifacts_dir)
        .into_iter()
        .flatten()
        .flatten()
        .any(|entry| entry.path().join("artifact.org").is_file());
    if present == 0 && already_v2 && !has_legacy_artifacts {
        return Ok(Migration::default());
    }

    let mut migration = Migration {
        project_source: bump_project_version(&project_source)?,
        ..Migration::default()
    };
    let mut ids = BTreeSet::new();
    for collection in collections {
        for source_path in collection.sources {
            if !source_path.is_file() {
                bail!("migration source is missing: {}", source_path.display());
            }
            let source = std::fs::read_to_string(&source_path)
                .with_context(|| format!("read {}", source_path.display()))?;
            let file = OrgFile::parse(source, source_path.to_string_lossy())
                .with_context(|| format!("parse {}", source_path.display()))?;
            let reassembled = file.slice(file.prelude.clone()).to_string()
                + &file
                    .headings
                    .iter()
                    .map(|heading| file.slice(heading.span.clone()))
                    .collect::<String>();
            if reassembled != file.source() {
                bail!(
                    "byte-for-byte heading round trip failed: {}",
                    source_path.display()
                );
            }
            for heading in &file.headings {
                let id = heading.property("ID").with_context(|| {
                    format!(
                        "{}: heading '{}' has no :ID:",
                        source_path.display(),
                        heading.title
                    )
                })?;
                if !id.starts_with(collection.prefix) {
                    bail!(
                        "{}: id {id} does not start with {}",
                        source_path.display(),
                        collection.prefix
                    );
                }
                if id == "." || id == ".." || id.contains('/') || id.contains('\\') {
                    bail!("unsafe node id {id}");
                }
                if !ids.insert(id.to_string()) {
                    bail!("duplicate node id {id}");
                }
                let block = file.slice(heading.span.clone());
                let node = node_org_header(collection.label, id) + block;
                let journal = journal_header(id);
                let dir = dotorg.join(collection.name).join(id);
                if dir.exists() {
                    bail!("migration target already exists: {}", dir.display());
                }
                migration.nodes.insert(dir, (node, journal));
                *migration.headings.entry(collection.name).or_default() += 1;
                migration.bytes += block.len();
            }
            migration.old_files.push(source_path);
        }
    }
    plan_artifacts(&artifacts_dir, &mut migration)?;
    Ok(migration)
}

fn plan_artifacts(artifacts_dir: &Path, migration: &mut Migration) -> Result<()> {
    let Ok(entries) = std::fs::read_dir(artifacts_dir) else {
        return Ok(());
    };
    for entry in entries {
        let dir = entry?.path();
        let id = dir.file_name().and_then(|name| name.to_str()).unwrap_or("");
        if !dir.is_dir() || !id.starts_with("ART-") {
            continue;
        }
        let old_node = dir.join("artifact.org");
        if !old_node.is_file() {
            continue;
        }
        let old_journal = dir.join("reviews.org");
        if !old_journal.is_file() {
            bail!("migration source is missing: {}", old_journal.display());
        }

        let node_source = std::fs::read_to_string(&old_node)
            .with_context(|| format!("read {}", old_node.display()))?;
        let node_file = OrgFile::parse(&node_source, old_node.to_string_lossy())?;
        let [heading] = node_file.headings.as_slice() else {
            bail!("{} must hold exactly one heading", old_node.display());
        };
        if heading.property("ID") != Some(id) {
            bail!(
                "{} heading id does not match directory {id}",
                old_node.display()
            );
        }
        let node = node_org_header("artifact", id) + node_file.slice(heading.span.clone());

        let reviews_source = std::fs::read_to_string(&old_journal)
            .with_context(|| format!("read {}", old_journal.display()))?;
        let reviews = OrgFile::parse(&reviews_source, old_journal.to_string_lossy())?;
        let mut journal = journal_header(id);
        for heading in &reviews.headings {
            let cid = heading
                .property("CID")
                .with_context(|| format!("{} comment has no :CID:", old_journal.display()))?;
            let mut extras = Vec::new();
            for key in [
                "VERSION",
                "ANCHOR",
                "RESOLUTION_TARGET",
                "REPLY_TO",
                "CONSUMED",
                "RESOLVED",
            ] {
                if let Some(value) = heading.property(key) {
                    extras.push((key.to_string(), value.to_string()));
                }
            }
            let comment = JournalEntry {
                entry_id: cid.to_string(),
                // ponytail: legacy reviews have no timestamp; use file metadata if ordering matters.
                time: heading
                    .property("TIME")
                    .unwrap_or("[1970-01-01 Thu 00:00:00]")
                    .to_string(),
                ty: "comment".into(),
                actor: heading.property("AUTHOR").unwrap_or("legacy").to_string(),
                machine: heading.property("MACHINE").unwrap_or("legacy").to_string(),
                extras,
                body: reviews_source[heading.body.start..heading.span.end]
                    .trim()
                    .to_string(),
            };
            comment.validate()?;
            journal = append_entry(&journal, id, &comment);
        }

        migration.rewrites.insert(dir.join("node.org"), node);
        migration.rewrites.insert(dir.join("journal.org"), journal);
        migration.old_files.extend([old_node, old_journal]);
        *migration.headings.entry("artifacts").or_default() += 1;
        migration.in_place_nodes += 1;
        migration.bytes += node_source.len() + reviews_source.len();
    }
    Ok(())
}

fn bump_project_version(source: &str) -> Result<String> {
    let mut found = false;
    let mut out = String::with_capacity(source.len());
    for line in source.split_inclusive('\n') {
        if line.trim_start().starts_with("#+orgasmic_version:") {
            if found {
                bail!("project.org has duplicate #+orgasmic_version labels");
            }
            found = true;
            out.push_str("#+orgasmic_version: 2");
            if line.ends_with('\n') {
                out.push('\n');
            }
        } else {
            out.push_str(line);
        }
    }
    if !found {
        bail!("project.org has no #+orgasmic_version label");
    }
    Ok(out)
}

fn apply(root: &Path, migration: &Migration) -> Result<()> {
    for (dir, (node, journal)) in &migration.nodes {
        std::fs::create_dir_all(dir.parent().expect("node dir has collection parent"))
            .with_context(|| format!("create collection for {}", dir.display()))?;
        std::fs::create_dir(dir).with_context(|| format!("create {}", dir.display()))?;
        std::fs::write(dir.join("node.org"), node)
            .with_context(|| format!("write {}/node.org", dir.display()))?;
        std::fs::write(dir.join("journal.org"), journal)
            .with_context(|| format!("write {}/journal.org", dir.display()))?;
    }
    for (path, contents) in &migration.rewrites {
        std::fs::write(path, contents).with_context(|| format!("write {}", path.display()))?;
    }
    for path in &migration.old_files {
        std::fs::remove_file(path).with_context(|| format!("delete {}", path.display()))?;
    }
    std::fs::write(
        root.join(".orgasmic/project.org"),
        &migration.project_source,
    )
    .context("write .orgasmic/project.org")?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn migration_is_verbatim_dry_run_and_idempotent() {
        let tmp = tempfile::tempdir().unwrap();
        let dotorg = tmp.path().join(".orgasmic");
        std::fs::create_dir_all(dotorg.join("tasks")).unwrap();
        std::fs::write(dotorg.join("project.org"), "#+orgasmic_version: 1\n").unwrap();
        for state in TASK_STATES {
            let body = if state == "done" {
                "* DONE TASK-A title\n:PROPERTIES:\n:ID: TASK-A\n:END:\n\n** Description\nexact\n"
            } else {
                "#+title: empty\n"
            };
            std::fs::write(dotorg.join("tasks").join(format!("{state}.org")), body).unwrap();
        }
        std::fs::write(
            dotorg.join("decisions.org"),
            "* dec_A choice\n:PROPERTIES:\n:ID: dec_A\n:END:\n",
        )
        .unwrap();
        std::fs::write(
            dotorg.join("glossary.org"),
            "* term_A Word\n:PROPERTIES:\n:ID: term_A\n:END:\n",
        )
        .unwrap();
        let artifact = dotorg.join("artifacts/ART-ABCDE");
        std::fs::create_dir_all(&artifact).unwrap();
        std::fs::write(
            artifact.join("artifact.org"),
            "#+orgasmic_version: 1\n\n* ART-ABCDE title\n:PROPERTIES:\n:ID: ART-ABCDE\n:TITLE: title\n:VERSION: 1\n:STATE: submitted\n:END:\n",
        )
        .unwrap();
        std::fs::write(
            artifact.join("reviews.org"),
            "* CID-old\n:PROPERTIES:\n:CID: CID-old\n:AUTHOR: owner\n:VERSION: 1\n:ANCHOR: {}\n:RESOLUTION_TARGET:\n:REPLY_TO:\n:RESOLVED: false\n:CONSUMED: false\n:END:\n\nkeep me\n",
        )
        .unwrap();

        let dry = plan(tmp.path()).unwrap();
        assert_eq!(dry.nodes.len(), 3);
        assert!(!dotorg.join("tasks/TASK-A").exists());
        apply(tmp.path(), &dry).unwrap();
        assert!(
            std::fs::read_to_string(dotorg.join("tasks/TASK-A/node.org"))
                .unwrap()
                .ends_with("** Description\nexact\n")
        );
        assert!(!artifact.join("artifact.org").exists());
        assert_eq!(
            orgasmic_core::node_kernel::parse_journal(
                &std::fs::read_to_string(artifact.join("journal.org")).unwrap(),
                "journal.org",
            )
            .unwrap()[0]
                .body,
            "keep me"
        );
        assert!(plan(tmp.path()).unwrap().old_files.is_empty());
    }
}
