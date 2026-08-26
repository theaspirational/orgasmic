use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{bail, Context, Result};
use orgasmic_core::node_kernel::{append_entry, journal_header, node_org_header, JournalEntry};
use orgasmic_core::{projects, Home, OrgFile, ProjectFile};

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

struct ScratchDir(PathBuf);

impl ScratchDir {
    fn new(label: &str) -> Result<Self> {
        let path = std::env::temp_dir().join(format!("orgasmic-{label}-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir(&path).with_context(|| format!("create {}", path.display()))?;
        Ok(Self(path))
    }

    fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for ScratchDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
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

pub(crate) fn run(home: &Home, dry_run: bool, to_branch: bool) -> Result<()> {
    let root = crate::manager::find_project_root()?;
    run_at(home, &root, dry_run, to_branch)
}

fn run_at(home: &Home, root: &Path, dry_run: bool, to_branch: bool) -> Result<()> {
    let migration = plan(root)?;
    if to_branch
        && migration.old_files.is_empty()
        && is_ledger_root(home, root, &migration.project_source)?
    {
        println!("already migrated");
        return Ok(());
    }
    refuse_dirty_tree(root)?;
    if to_branch && !dry_run && !is_ledger_root(home, root, &migration.project_source)? {
        migrate_to_branch(home, root, &migration)?;
    } else if migration.old_files.is_empty() {
        println!("already migrated");
        return Ok(());
    } else if !dry_run {
        apply(root, &migration)?;
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
    if to_branch {
        println!("  target orphan branch orgasmic");
        if !dry_run {
            // The cutover removes `.orgasmic/` from the worked tree but leaves
            // those files tracked on the current branch, so the removal sits
            // there as an uncommitted deletion. Committing it is the operator's
            // call — it is a commit on their branch — but leaving it unsaid
            // strands them: any later checkout or stash restores `.orgasmic/`
            // beside the real ledger, and this command then refuses to run
            // again because the tree is dirty.
            println!(
                "  ledger {}",
                home.project_ledger(&project_id(&migration.project_source)?)
                    .display()
            );
            println!();
            println!("Next: the ledger now lives on the orphan `orgasmic` branch, but its");
            println!("removal from this branch is still uncommitted. Commit it:");
            println!();
            println!("  git add -A .orgasmic && git commit -m \"chore: move the orgasmic ledger to its own branch\"");
        }
    }
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
        return Ok(Migration {
            project_source,
            ..Migration::default()
        });
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

fn project_id(source: &str) -> Result<String> {
    let file = OrgFile::parse(source.to_string(), "project.org")?;
    Ok(ProjectFile::from_org(&file, "project.org")?.id.to_string())
}

fn is_ledger_root(home: &Home, root: &Path, project_source: &str) -> Result<bool> {
    Ok(std::fs::canonicalize(root).ok()
        == std::fs::canonicalize(home.project_ledger(&project_id(project_source)?)).ok())
}

fn migrate_to_branch(home: &Home, root: &Path, migration: &Migration) -> Result<()> {
    let id = project_id(&migration.project_source)?;
    let registered = projects::read_board(home)?
        .into_iter()
        .any(|entry| entry.id == id);
    if !registered {
        bail!("project {id} is not registered; run `orgasmic project add` first");
    }

    let target = home.project_ledger(&id);
    if !target.join(".orgasmic/project.org").is_file() {
        let stage = ScratchDir::new("ledger-stage")?;
        copy_tree(&root.join(".orgasmic"), &stage.path().join(".orgasmic"))?;
        let staged = plan(stage.path())?;
        if !staged.old_files.is_empty() {
            apply(stage.path(), &staged)?;
        }
        create_orphan_branch(root, stage.path())?;
        if target.exists() {
            bail!(
                "ledger target already exists but is incomplete: {}",
                target.display()
            );
        }
        std::fs::create_dir_all(target.parent().expect("ledger has project parent"))?;
        git(
            root,
            &[
                "worktree",
                "add",
                target.to_str().context("ledger path is not UTF-8")?,
                "orgasmic",
            ],
        )?;
    }

    let target_source = std::fs::read_to_string(target.join(".orgasmic/project.org"))?;
    if project_id(&target_source)? != id {
        bail!("orgasmic branch belongs to another project");
    }
    if !target_source
        .lines()
        .any(|line| line.trim() == "#+orgasmic_version: 2")
    {
        bail!("orgasmic branch ledger is not migrated to version 2");
    }
    ensure_ledger_worktree(root, &target)?;
    ensure_orphan(root)?;
    let source_tmp = root.join(".orgasmic/tmp");
    if source_tmp.is_dir() {
        copy_tree(&source_tmp, &target.join(".orgasmic/tmp"))?;
    }
    std::fs::remove_dir_all(root.join(".orgasmic")).context("remove .orgasmic from main tree")?;
    Ok(())
}

fn create_orphan_branch(repo: &Path, work_tree: &Path) -> Result<()> {
    if git_ok(repo, &["rev-parse", "--verify", "refs/heads/orgasmic"]) {
        ensure_orphan(repo)?;
        return Ok(());
    }
    let index_dir = ScratchDir::new("ledger-index")?;
    let index = index_dir.path().join("index");
    git_env(repo, work_tree, &index, &["read-tree", "--empty"])?;
    git_env(
        repo,
        work_tree,
        &index,
        &["add", "--all", "--", ".orgasmic"],
    )?;
    let tree = git_capture_env(repo, work_tree, &index, &["write-tree"])?;
    let commit = git_capture(
        repo,
        &[
            "-c",
            "user.name=orgasmic",
            "-c",
            "user.email=orgasmic@localhost",
            "commit-tree",
            &tree,
            "-m",
            "Initialize orgasmic ledger",
        ],
    )?;
    git(repo, &["update-ref", "refs/heads/orgasmic", &commit])
}

fn ensure_orphan(repo: &Path) -> Result<()> {
    let line = git_capture(repo, &["rev-list", "--parents", "-n", "1", "orgasmic"])?;
    if line.split_whitespace().count() != 1 {
        bail!("refusing non-orphan branch named orgasmic");
    }
    Ok(())
}

fn ensure_ledger_worktree(repo: &Path, ledger: &Path) -> Result<()> {
    if git_capture(ledger, &["rev-parse", "--abbrev-ref", "HEAD"])? != "orgasmic" {
        bail!("ledger target is not the orgasmic branch worktree");
    }
    if git_common_dir(repo)? != git_common_dir(ledger)? {
        bail!("ledger target belongs to another git repository");
    }
    Ok(())
}

fn git_common_dir(repo: &Path) -> Result<PathBuf> {
    let path = PathBuf::from(git_capture(repo, &["rev-parse", "--git-common-dir"])?);
    std::fs::canonicalize(if path.is_absolute() {
        path
    } else {
        repo.join(path)
    })
    .context("canonicalize git common dir")
}

fn copy_tree(source: &Path, target: &Path) -> Result<()> {
    std::fs::create_dir_all(target).with_context(|| format!("create {}", target.display()))?;
    for entry in std::fs::read_dir(source).with_context(|| format!("read {}", source.display()))? {
        let entry = entry?;
        let ty = entry.file_type()?;
        let dest = target.join(entry.file_name());
        if ty.is_symlink() {
            bail!("refusing symlink in ledger: {}", entry.path().display());
        } else if ty.is_dir() {
            copy_tree(&entry.path(), &dest)?;
        } else if ty.is_file() {
            std::fs::copy(entry.path(), &dest)
                .with_context(|| format!("copy {}", entry.path().display()))?;
        }
    }
    Ok(())
}

fn git_ok(repo: &Path, args: &[&str]) -> bool {
    Command::new("git")
        .args(args)
        .current_dir(repo)
        .output()
        .is_ok_and(|output| output.status.success())
}

fn git(repo: &Path, args: &[&str]) -> Result<()> {
    let output = Command::new("git").args(args).current_dir(repo).output()?;
    if !output.status.success() {
        bail!(
            "git {} failed: {}",
            args.join(" "),
            String::from_utf8_lossy(&output.stderr)
        );
    }
    Ok(())
}

fn git_capture(repo: &Path, args: &[&str]) -> Result<String> {
    let output = Command::new("git").args(args).current_dir(repo).output()?;
    if !output.status.success() {
        bail!(
            "git {} failed: {}",
            args.join(" "),
            String::from_utf8_lossy(&output.stderr)
        );
    }
    Ok(String::from_utf8(output.stdout)?.trim().to_string())
}

fn git_env(repo: &Path, work_tree: &Path, index: &Path, args: &[&str]) -> Result<()> {
    git_capture_env(repo, work_tree, index, args).map(drop)
}

fn git_capture_env(repo: &Path, work_tree: &Path, index: &Path, args: &[&str]) -> Result<String> {
    let output = Command::new("git")
        .args(args)
        .current_dir(repo)
        .env("GIT_WORK_TREE", work_tree)
        .env("GIT_INDEX_FILE", index)
        .output()?;
    if !output.status.success() {
        bail!(
            "git {} failed: {}",
            args.join(" "),
            String::from_utf8_lossy(&output.stderr)
        );
    }
    Ok(String::from_utf8(output.stdout)?.trim().to_string())
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

    #[test]
    fn branch_cutover_is_orphan_dry_run_idempotent_and_worker_discoverable() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = tmp.path().join("repo");
        let home = Home::at(tmp.path().join("home"));
        std::fs::create_dir(&repo).unwrap();
        git(&repo, &["init", "-b", "main"]).unwrap();
        git(&repo, &["config", "user.name", "test"]).unwrap();
        git(&repo, &["config", "user.email", "test@example.com"]).unwrap();

        let dotorg = repo.join(".orgasmic");
        std::fs::create_dir_all(dotorg.join("tasks")).unwrap();
        std::fs::write(dotorg.join(".gitignore"), "tmp/\n").unwrap();
        std::fs::write(
            dotorg.join("project.org"),
            "#+orgasmic_version: 1\n\n* PROJECT demo\n:PROPERTIES:\n:ID: demo\n:END:\n",
        )
        .unwrap();
        for state in TASK_STATES {
            let body = if state == "todo" {
                "* TODO TASK-A title\n:PROPERTIES:\n:ID: TASK-A\n:END:\n"
            } else {
                "#+title: empty\n"
            };
            std::fs::write(dotorg.join("tasks").join(format!("{state}.org")), body).unwrap();
        }
        std::fs::write(dotorg.join("decisions.org"), "#+title: empty\n").unwrap();
        std::fs::write(dotorg.join("glossary.org"), "#+title: empty\n").unwrap();
        std::fs::create_dir_all(dotorg.join("tmp")).unwrap();
        std::fs::write(dotorg.join("tmp/session"), "kept").unwrap();
        projects::register_project(&home, &repo, "demo", "main").unwrap();
        git(&repo, &["add", ".orgasmic"]).unwrap();
        git(&repo, &["commit", "-m", "old ledger"]).unwrap();

        run_at(&home, &repo, true, true).unwrap();
        assert!(repo.join(".orgasmic").is_dir());
        assert!(!git_ok(&repo, &["rev-parse", "--verify", "orgasmic"]));

        run_at(&home, &repo, false, true).unwrap();
        let ledger = home.project_ledger("demo");
        assert!(!repo.join(".orgasmic").exists());
        assert!(ledger.join(".orgasmic/tasks/TASK-A/node.org").is_file());
        assert_eq!(
            std::fs::read_to_string(ledger.join(".orgasmic/tmp/session")).unwrap(),
            "kept"
        );
        assert_eq!(
            git_capture(&repo, &["rev-list", "--parents", "-n", "1", "orgasmic"])
                .unwrap()
                .split_whitespace()
                .count(),
            1
        );
        assert!(
            git_capture(&repo, &["ls-tree", "-r", "--name-only", "orgasmic"])
                .unwrap()
                .lines()
                .all(|path| path.starts_with(".orgasmic/"))
        );
        assert_eq!(projects::read_board(&home).unwrap()[0].path, ledger);

        git(&repo, &["add", "-A"]).unwrap();
        git(&repo, &["commit", "-m", "remove ledger from main"]).unwrap();
        let worker = tmp.path().join("worker");
        git(
            &repo,
            &[
                "worktree",
                "add",
                "-b",
                "worker",
                worker.to_str().unwrap(),
                "main",
            ],
        )
        .unwrap();
        assert!(!worker.join(".orgasmic").exists());
        assert_eq!(
            crate::manager::find_project_root_optional_from(&home, &worker)
                .unwrap()
                .unwrap(),
            ledger
        );
        run_at(&home, &ledger, false, true).unwrap();
    }
}
