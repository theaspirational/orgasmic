use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{bail, Context, Result};
use orgasmic_core::node_kernel::{journal_header, node_org_header};
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
    old_files: Vec<PathBuf>,
    headings: BTreeMap<&'static str, usize>,
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
    println!("  nodes {}", migration.nodes.len());
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
    if present == 0 && already_v2 {
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
    Ok(migration)
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

        let dry = plan(tmp.path()).unwrap();
        assert_eq!(dry.nodes.len(), 3);
        assert!(!dotorg.join("tasks/TASK-A").exists());
        apply(tmp.path(), &dry).unwrap();
        assert!(
            std::fs::read_to_string(dotorg.join("tasks/TASK-A/node.org"))
                .unwrap()
                .ends_with("** Description\nexact\n")
        );
        assert!(plan(tmp.path()).unwrap().old_files.is_empty());
    }
}
