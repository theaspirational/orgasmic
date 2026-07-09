// arch: arch_QFQTD.1
// orgasmic:arch_QFQTD
//! Project scaffolding and global board registration.
//!
//! This module owns the shared file operations used by both the CLI and the
//! daemon: initialize a repo-local `.orgasmic/` scaffold and register a project
//! on the user board.

use std::fs::OpenOptions;
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{anyhow, bail, Context, Result};

use crate::{Home, OrgFile};

/// Inputs that fill the scaffold placeholders.
#[derive(Debug, Clone)]
pub struct ScaffoldInputs {
    pub project_name: String,
    pub project_id: String,
    pub default_branch: String,
}

impl ScaffoldInputs {
    pub fn derive(project_root: &Path, name: Option<String>) -> Self {
        let project_name = name.unwrap_or_else(|| {
            project_root
                .file_name()
                .and_then(|s| s.to_str())
                .unwrap_or("project")
                .to_string()
        });
        let project_id = slugify(&project_name);
        Self {
            project_name,
            project_id,
            default_branch: git_default_branch(project_root).unwrap_or_default(),
        }
    }
}

fn git_default_branch(project_root: &Path) -> Option<String> {
    let origin_head = git_output(
        project_root,
        &["symbolic-ref", "--short", "refs/remotes/origin/HEAD"],
    )
    .map(|value| value.strip_prefix("origin/").unwrap_or(&value).to_string());
    origin_head.or_else(|| {
        git_output(project_root, &["rev-parse", "--abbrev-ref", "HEAD"])
            .filter(|value| value != "HEAD")
    })
}

fn git_output(project_root: &Path, args: &[&str]) -> Option<String> {
    let output = Command::new("git")
        .args(args)
        .current_dir(project_root)
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    Some(String::from_utf8_lossy(&output.stdout).trim().to_string())
        .filter(|value| !value.is_empty())
}

fn slugify(s: &str) -> String {
    s.chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() {
                c.to_ascii_lowercase()
            } else {
                '-'
            }
        })
        .collect::<String>()
        .trim_matches('-')
        .to_string()
}

/// Shipped scaffold location under `shipped/`. The shipped and user tiers use
/// the same `project-scaffold` key so per-file overrides stay symmetric
/// (dec_N7BM4, dec_GRDFB).
pub const SCAFFOLD_SHIPPED_DIR: &str = "project-scaffold";

/// Logical key for user scaffold overrides: `user/project-scaffold/<rel>`
/// shadows `shipped/project-scaffold/<rel>` per the loader rule (dec_GRDFB).
const SCAFFOLD_USER_DIR: &str = "project-scaffold";

/// Resolve one scaffold template: user override first, then the shipped
/// top-level scaffold tree.
fn resolve_scaffold(home: &Home, rel: &str) -> Option<PathBuf> {
    let user = home.user().join(SCAFFOLD_USER_DIR).join(rel);
    if user.exists() {
        return Some(user);
    }
    let shipped = home
        .source()
        .join("shipped")
        .join(SCAFFOLD_SHIPPED_DIR)
        .join(rel);
    shipped.exists().then_some(shipped)
}

/// Files the scaffold writes inside `<project>/.orgasmic/`, paired with their
/// shipped source under `shipped/project-scaffold/`.
const SCAFFOLD_FILES: &[&str] = &[
    ".gitignore",
    "entry.org",
    "project.org",
    "decisions.org",
    "tasks/backlog.org",
    "tasks/todo.org",
    "tasks/in_progress.org",
    "tasks/in_review.org",
    "tasks/done.org",
    "tasks/cancelled.org",
    "tasks/goal.org",
    "tasks/handoff.org",
    "gotchas.org",
    "conventions/contributing.org",
    "conventions/manager-implementer.org",
    "conventions/no-skill-installed.org",
    "conventions/orgasmic-tooling.org",
];

const SCAFFOLD_DIRS: &[&str] = &["tmp", "tmp/sessions", "tmp/dispatch"];
const LOCAL_INSTRUCTIONS_REL: &str = "tmp/local_instructions.org";
const LOCAL_INSTRUCTIONS_PLACEHOLDER: &str = concat!(
    "#+title: local instructions\n",
    "#+orgasmic_version: 1\n\n",
    "* Local instructions\n\n",
    "This file is intentionally local and ignored. Keep machine-specific setup ",
    "notes here so future agents do not rediscover them.\n",
);

/// Root instruction files harnesses auto-read: `AGENTS.md`
/// (codex/opencode/cursor) and `CLAUDE.md` (Claude). After init both must exist
/// and route to `orgasmic entry`, with `.orgasmic/entry.org` retained as the
/// fallback breadcrumb; the first entry is canonical (carries the real pointer)
/// and the second defers to it — same invariant as the `/orgasmic init` skill.
const ROOT_POINTER_FILES: [&str; 2] = ["AGENTS.md", "CLAUDE.md"];
const ROOT_POINTER_TARGET: &str = ".orgasmic/entry.org";
const ROOT_POINTER_LINE: &str = concat!(
    "This repo is coordinated with **orgasmic**: before you answer, search, ",
    "edit, or run any tool here, you MUST run `orgasmic entry` and follow its ",
    "output; if the CLI is missing, read ",
    "[`.orgasmic/entry.org`](.orgasmic/entry.org); install missing root ",
    "pointers with `orgasmic project init`.\n",
);

/// Initialize `<project_root>/.orgasmic/` from the shipped scaffold templates.
/// Refuses to overwrite an existing `.orgasmic/` directory unless `force`.
pub fn init_project(
    home: &Home,
    project_root: &Path,
    inputs: &ScaffoldInputs,
    force: bool,
) -> Result<Vec<PathBuf>> {
    validate_project_root(project_root)?;
    validate_scaffold_inputs(inputs)?;

    let dotorg = project_root.join(".orgasmic");
    if dotorg.exists() && !force {
        bail!(
            "{} already exists. Use --force to overwrite (will keep existing files; only writes missing scaffold members).",
            dotorg.display()
        );
    }
    std::fs::create_dir_all(&dotorg).with_context(|| format!("create {}", dotorg.display()))?;
    std::fs::create_dir_all(dotorg.join("tx"))
        .with_context(|| format!("create {}", dotorg.join("tx").display()))?;
    for rel in SCAFFOLD_DIRS {
        let dir = dotorg.join(rel);
        std::fs::create_dir_all(&dir).with_context(|| format!("create {}", dir.display()))?;
    }

    let mut written = Vec::new();
    for rel in SCAFFOLD_FILES {
        let template_path = resolve_scaffold(home, rel).ok_or_else(|| {
            anyhow!(
                "scaffold template missing: {}/{}",
                SCAFFOLD_SHIPPED_DIR,
                rel
            )
        })?;
        let template = std::fs::read_to_string(&template_path)
            .with_context(|| format!("read scaffold {}", template_path.display()))?;
        let rendered = render(&template, inputs);
        if rel.ends_with(".org") {
            OrgFile::parse(rendered.clone(), format!(".orgasmic/{}", rel)).with_context(|| {
                format!("scaffold {} did not parse as an app-owned Org file", rel)
            })?;
        }
        let dest = dotorg.join(rel);
        if dest.exists() {
            continue;
        }
        if let Some(parent) = dest.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("create {}", parent.display()))?;
        }
        std::fs::write(&dest, rendered).with_context(|| format!("write {}", dest.display()))?;
        written.push(dest);
    }
    let local_instructions = dotorg.join(LOCAL_INSTRUCTIONS_REL);
    if !local_instructions.exists() {
        if let Some(parent) = local_instructions.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("create {}", parent.display()))?;
        }
        std::fs::write(&local_instructions, LOCAL_INSTRUCTIONS_PLACEHOLDER)
            .with_context(|| format!("write {}", local_instructions.display()))?;
        written.push(local_instructions);
    }
    ensure_root_agent_pointers(project_root)?;
    Ok(written)
}

/// Make both root instruction files route to `.orgasmic/entry.org`. Existing
/// files are only ever appended to; missing ones are created (a file that
/// already references its counterpart counts as routing through it).
fn ensure_root_agent_pointers(project_root: &Path) -> Result<()> {
    let paths = ROOT_POINTER_FILES.map(|name| project_root.join(name));
    let mut contents: Vec<Option<String>> = Vec::with_capacity(2);
    for path in &paths {
        contents.push(if path.exists() {
            Some(
                std::fs::read_to_string(path)
                    .with_context(|| format!("read {}", path.display()))?,
            )
        } else {
            None
        });
    }

    if contents.iter().all(Option::is_none) {
        std::fs::write(
            &paths[0],
            format!("# {}\n\n{ROOT_POINTER_LINE}", ROOT_POINTER_FILES[0]),
        )
        .with_context(|| format!("write {}", paths[0].display()))?;
        std::fs::write(&paths[1], format!("Read `{}`.\n", ROOT_POINTER_FILES[0]))
            .with_context(|| format!("write {}", paths[1].display()))?;
        return Ok(());
    }

    // Append the pointer to existing files that neither route directly nor
    // defer to the counterpart file.
    for i in 0..2 {
        let Some(content) = contents[i].as_ref() else {
            continue;
        };
        if content.contains(ROOT_POINTER_TARGET) || content.contains(ROOT_POINTER_FILES[1 - i]) {
            continue;
        }
        append_pointer(&paths[i], &mut contents[i])?;
    }

    // Two files deferring to each other with no direct route anywhere would
    // loop forever; anchor the first existing file with the real pointer.
    if !contents
        .iter()
        .flatten()
        .any(|c| c.contains(ROOT_POINTER_TARGET))
    {
        let i = contents.iter().position(Option::is_some).unwrap_or(0);
        append_pointer(&paths[i], &mut contents[i])?;
    }

    // Create whichever file is missing as a defer to the routing counterpart.
    for i in 0..2 {
        if contents[i].is_none() {
            std::fs::write(
                &paths[i],
                format!("Read `{}`.\n", ROOT_POINTER_FILES[1 - i]),
            )
            .with_context(|| format!("write {}", paths[i].display()))?;
        }
    }
    Ok(())
}

fn append_pointer(path: &Path, content: &mut Option<String>) -> Result<()> {
    let mut updated = content.clone().unwrap_or_default();
    if !updated.is_empty() && !updated.ends_with('\n') {
        updated.push('\n');
    }
    if !updated.is_empty() {
        updated.push('\n');
    }
    updated.push_str(ROOT_POINTER_LINE);
    std::fs::write(path, &updated).with_context(|| format!("write {}", path.display()))?;
    *content = Some(updated);
    Ok(())
}

fn render(template: &str, inputs: &ScaffoldInputs) -> String {
    template
        .replace("{{PROJECT_NAME}}", &inputs.project_name)
        .replace("{{PROJECT_ID}}", &inputs.project_id)
        .replace("{{DEFAULT_BRANCH}}", &inputs.default_branch)
}

#[derive(Debug, Clone)]
pub struct BoardEntry {
    pub id: String,
    pub path: PathBuf,
    pub branch: String,
    pub status: String,
}

pub fn board_path(home: &Home) -> PathBuf {
    home.board()
}

pub fn read_board(home: &Home) -> Result<Vec<BoardEntry>> {
    let path = board_path(home);
    if !path.exists() {
        return Ok(Vec::new());
    }
    let source =
        std::fs::read_to_string(&path).with_context(|| format!("read {}", path.display()))?;
    parse_board_entries(&source, &path)
}

fn parse_board_entries(source: &str, path: &Path) -> Result<Vec<BoardEntry>> {
    if source.trim().is_empty() {
        return Ok(Vec::new());
    }
    let file = OrgFile::parse(source.to_string(), path.to_string_lossy())
        .with_context(|| format!("parse {}", path.display()))?;
    let mut out = Vec::new();
    for h in &file.headings {
        let Some(id) = h.property("ID") else {
            continue;
        };
        out.push(BoardEntry {
            id: id.to_string(),
            path: PathBuf::from(
                h.property("PATH")
                    .or_else(|| h.property("LOCAL_PATH"))
                    .unwrap_or(""),
            ),
            branch: h
                .property("BRANCH")
                .or_else(|| h.property("DEFAULT_BRANCH"))
                .unwrap_or("")
                .to_string(),
            status: h.property("STATUS").unwrap_or("active").to_string(),
        });
    }
    Ok(out)
}

pub fn register_project(
    home: &Home,
    project_root: &Path,
    project_id: &str,
    branch: &str,
) -> Result<()> {
    validate_project_root(project_root)?;
    validate_project_id(project_id)?;
    validate_single_line(branch, "default_branch", 128)?;
    let project_root = std::fs::canonicalize(project_root)
        .with_context(|| format!("canonicalize {}", project_root.display()))?;

    let path = board_path(home);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).with_context(|| format!("create {}", parent.display()))?;
    }
    let mut file = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(&path)
        .with_context(|| format!("open {}", path.display()))?;
    fs2::FileExt::lock_exclusive(&file).with_context(|| format!("lock {}", path.display()))?;
    let result = register_project_locked(&mut file, &path, &project_root, project_id, branch);
    let unlock = fs2::FileExt::unlock(&file).with_context(|| format!("unlock {}", path.display()));
    result.and(unlock)
}

fn register_project_locked(
    file: &mut std::fs::File,
    path: &Path,
    project_root: &Path,
    project_id: &str,
    branch: &str,
) -> Result<()> {
    file.seek(SeekFrom::Start(0))
        .with_context(|| format!("seek {}", path.display()))?;
    let mut source = String::new();
    file.read_to_string(&mut source)
        .with_context(|| format!("read {}", path.display()))?;
    let mut existing = parse_board_entries(&source, path)?;
    if existing.iter().any(|e| e.id == project_id) {
        bail!("project {} already on the board", project_id);
    }
    existing.push(BoardEntry {
        id: project_id.to_string(),
        path: project_root.to_path_buf(),
        branch: branch.to_string(),
        status: "active".to_string(),
    });
    let rendered = render_board(&existing);
    file.set_len(0)
        .with_context(|| format!("truncate {}", path.display()))?;
    file.seek(SeekFrom::Start(0))
        .with_context(|| format!("seek {}", path.display()))?;
    file.write_all(rendered.as_bytes())
        .with_context(|| format!("write {}", path.display()))?;
    OrgFile::parse(rendered, path.to_string_lossy())
        .context("board.org failed to parse after write")?;
    Ok(())
}

fn validate_project_id(s: &str) -> Result<()> {
    if s.is_empty() || s.len() > 64 {
        bail!("project_id must be 1-64 chars");
    }
    if !s
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '.' | '-'))
    {
        bail!("project_id must match [A-Za-z0-9_.-]");
    }
    Ok(())
}

fn validate_single_line(s: &str, field: &str, max: usize) -> Result<()> {
    if s.len() > max {
        bail!("{field} too long");
    }
    if s.chars()
        .any(|c| c == '\n' || c == '\r' || c == '\0' || c.is_control())
    {
        bail!("{field} must not contain newlines or control characters");
    }
    Ok(())
}

fn validate_project_root(project_root: &Path) -> Result<()> {
    validate_single_line(&project_root.display().to_string(), "project_root", 4096)
}

fn validate_scaffold_inputs(inputs: &ScaffoldInputs) -> Result<()> {
    validate_single_line(&inputs.project_name, "project_name", 256)?;
    validate_project_id(&inputs.project_id)?;
    validate_single_line(&inputs.default_branch, "default_branch", 128)
}

fn render_board(entries: &[BoardEntry]) -> String {
    let mut out = String::from("#+title: orgasmic board\n#+orgasmic_version: 1\n\n");
    for e in entries {
        out.push_str(&format!("* PROJECT {}\n", e.id));
        out.push_str(":PROPERTIES:\n");
        out.push_str(&format!(":ID:               {}\n", e.id));
        out.push_str(&format!(":PATH:             {}\n", e.path.display()));
        out.push_str(&format!(":BRANCH:           {}\n", e.branch));
        out.push_str(&format!(":STATUS:           {}\n", e.status));
        out.push_str(":END:\n\n");
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::DecisionNode;

    fn fake_shipped(home: &Home) {
        let dst = home.source().join("shipped").join(SCAFFOLD_SHIPPED_DIR);
        std::fs::create_dir_all(&dst).unwrap();
        std::fs::write(dst.join(".gitignore"), "tmp/\n").unwrap();
        std::fs::write(
            dst.join("entry.org"),
            "#+title: orgasmic entry\n#+orgasmic_version: 1\n\n* Entry\n\nRun `orgasmic entry` and follow its output.\n\nIf the `orgasmic` CLI is missing, offer to install it with `/orgasmic install`. If the user declines, keep `.orgasmic/` read-only. Edit source only after explicit user confirmation, warn that source edits will drift from orgasmic state, and reconcile once the runtime is available.\n",
        )
        .unwrap();
        let conventions_dst = dst.join("conventions");
        std::fs::create_dir_all(&conventions_dst).unwrap();
        std::fs::write(
            conventions_dst.join("contributing.org"),
            "#+title: contributing\n#+orgasmic_version: 1\n\n* Convention: contributing changes\n\n- Follow the constraints in `project.org`.\n",
        )
        .unwrap();
        std::fs::write(
            conventions_dst.join("manager-implementer.org"),
            "#+title: manager-implementer\n#+orgasmic_version: 1\n\n* Convention: manager-implementer mode\n\nSingle-worker mode; iterate fast, reconcile later.\n",
        )
        .unwrap();
        std::fs::write(
            conventions_dst.join("orgasmic-tooling.org"),
            "#+title: orgasmic tooling\n#+orgasmic_version: 1\n\n* Convention: working with orgasmic installed (optional)\n\nOptional accelerant; not required.\n",
        )
        .unwrap();
        std::fs::write(
            conventions_dst.join("no-skill-installed.org"),
            "#+title: no skill installed\n#+orgasmic_version: 1\n\n* Convention: manager fallback without the `/orgasmic` skill\n\nBrief from plain files.\n",
        )
        .unwrap();
        std::fs::write(
            dst.join("project.org"),
            "#+title: {{PROJECT_NAME}}\n#+orgasmic_version: 1\n\n* PROJECT {{PROJECT_NAME}}\n:PROPERTIES:\n:ID:                  {{PROJECT_ID}}\n:END:\n\n** Mission\n[Describe the project's goal.]\n\n** Operating Constraints\n[Project-specific constraints.]\n",
        )
        .unwrap();
        std::fs::write(
            dst.join("decisions.org"),
            "#+title: {{PROJECT_NAME}} decisions\n#+orgasmic_version: 1\n#+scope: project\n\n* dec_001 Bootstrap orgasmic project state :bootstrap:\n:PROPERTIES:\n:ID:            dec_001\n:GLOSSARY_REFS:\n:DECIDED_AT:\n:SOURCE:        scaffold\n:END:\n** Context\nA freshly initialized project has only scaffold files. Agents need one explicit starting decision so they do not treat empty project memory as authoritative.\n** Decision\nBootstrap `.orgasmic/` from repository evidence and operator answers before relying on project, decision, glossary, or architecture records for downstream work.\n** Consequences\nTASK-001 is the first work item. Later workers should treat scaffold prose as incomplete until TASK-001 and its subtasks replace it with repo-specific state.\n",
        )
        .unwrap();
        let tasks_dst = dst.join("tasks");
        std::fs::create_dir_all(&tasks_dst).unwrap();
        std::fs::write(
            tasks_dst.join("backlog.org"),
            "#+title: Active sprint tasks\n#+orgasmic_version: 1\n\n* BACKLOG TASK-001 Example task title :example:\n:PROPERTIES:\n:ID:               TASK-001\n:PRIORITY:         P2\n:END:\n\n** Description\n[Replace.]\n\n** Acceptance Criteria\n- [ ] Replace with concrete, checkable criteria.\n\n** Evidence\n\n** Worklog\n",
        )
        .unwrap();
        for name in [
            "todo.org",
            "in_progress.org",
            "in_review.org",
            "done.org",
            "cancelled.org",
        ] {
            std::fs::write(
                tasks_dst.join(name),
                format!(
                    "#+title: {}\n#+orgasmic_version: 1\n\n",
                    name.trim_end_matches(".org")
                ),
            )
            .unwrap();
        }
        std::fs::write(
            tasks_dst.join("goal.org"),
            "#+title: Goal\n#+orgasmic_version: 1\n\n* GOAL Bootstrap project state\n:PROPERTIES:\n:ID: goal-bootstrap\n:STATUS: active\n:END:\n\n** Statement\nBootstrap.\n",
        )
        .unwrap();
        std::fs::write(
            tasks_dst.join("handoff.org"),
            "#+title: Handoff\n#+orgasmic_version: 1\n\n* HANDOFF current\n:PROPERTIES:\n:ID: handoff-current\n:GOAL_ID: goal-bootstrap\n:END:\n\n** Next likely actions\n- Start TASK-001.\n",
        )
        .unwrap();
        std::fs::write(
            dst.join("gotchas.org"),
            "#+title: Gotchas\n#+orgasmic_version: 1\n\n* (empty)\n",
        )
        .unwrap();
    }

    #[test]
    fn init_writes_and_validates_scaffold() {
        let tmp = tempfile::tempdir().unwrap();
        let home = Home::at(tmp.path().join("home"));
        home.ensure().unwrap();
        fake_shipped(&home);
        let proj = tmp.path().join("repo");
        std::fs::create_dir_all(&proj).unwrap();
        let inputs = ScaffoldInputs::derive(&proj, None);
        let written = init_project(&home, &proj, &inputs, false).unwrap();
        assert_eq!(written.len(), SCAFFOLD_FILES.len() + 1);
        for p in &written {
            if p.extension().and_then(|ext| ext.to_str()) == Some("org") {
                let src = std::fs::read_to_string(p).unwrap();
                OrgFile::parse(src, p.to_string_lossy()).expect("scaffold output parses");
            }
        }
        // entry.org and per-project conventions are scaffolded too (dec_055).
        let entry = std::fs::read_to_string(proj.join(".orgasmic/entry.org")).unwrap();
        assert!(entry.contains("* Entry"));
        assert!(entry.contains("#+orgasmic_version: 1"));
        assert!(entry.contains("Run `orgasmic entry` and follow its output."));
        assert!(entry.contains("keep `.orgasmic/` read-only"));
        assert!(entry.contains("source edits will drift from orgasmic state"));
        let root_pointer = std::fs::read_to_string(proj.join("AGENTS.md")).unwrap();
        assert!(root_pointer.contains(".orgasmic/entry.org"));
        assert!(root_pointer.contains("orgasmic entry"));
        assert!(root_pointer.contains("orgasmic project init"));
        let claude_pointer = std::fs::read_to_string(proj.join("CLAUDE.md")).unwrap();
        assert_eq!(claude_pointer, "Read `AGENTS.md`.\n");
        assert!(proj
            .join(".orgasmic/conventions/manager-implementer.org")
            .exists());
        let contributing =
            std::fs::read_to_string(proj.join(".orgasmic/conventions/contributing.org")).unwrap();
        assert!(contributing.contains("contributing changes"));
        assert!(proj
            .join(".orgasmic/conventions/orgasmic-tooling.org")
            .exists());
        assert!(proj
            .join(".orgasmic/conventions/no-skill-installed.org")
            .exists());
        assert!(proj.join(".orgasmic/tmp").is_dir());
        assert!(proj.join(".orgasmic/tmp/local_instructions.org").exists());
        let gitignore = std::fs::read_to_string(proj.join(".orgasmic/.gitignore")).unwrap();
        assert!(gitignore.lines().any(|line| line == "tmp/"));
        let project_src = std::fs::read_to_string(proj.join(".orgasmic/project.org")).unwrap();
        assert!(project_src.contains("* PROJECT repo"));
        assert!(project_src.contains(":ID:                  repo"));
        // Machine config moved out of project.org (dec_051).
        assert!(!project_src.contains("STAGE_WORKER"));
        assert!(!project_src.contains("DEFAULT_WORKER"));
        let decisions_src = std::fs::read_to_string(proj.join(".orgasmic/decisions.org")).unwrap();
        assert!(decisions_src.contains("* dec_001 Bootstrap orgasmic project state"));
        let decisions_file =
            OrgFile::parse(decisions_src, ".orgasmic/decisions.org").expect("decisions parses");
        let decision = DecisionNode::from_heading(
            &decisions_file,
            decisions_file
                .headings
                .iter()
                .find(|h| h.property("ID") == Some("dec_001"))
                .expect("dec_001"),
            ".orgasmic/decisions.org",
        )
        .expect("dec_001 schema-valid");
        assert_eq!(decision.title, "Bootstrap orgasmic project state");
        assert!(!proj.join(".orgasmic/config.org").exists());
        let backlog_src =
            std::fs::read_to_string(proj.join(".orgasmic/tasks/backlog.org")).unwrap();
        assert!(backlog_src.contains("* BACKLOG TASK-001"));
        assert!(!backlog_src.contains(":WORKER:"));
        assert!(!backlog_src.contains("* READY TASK-001"));
        assert!(proj.join(".orgasmic/tasks/todo.org").exists());
        assert!(proj.join(".orgasmic/tasks/done.org").exists());
        assert!(proj.join(".orgasmic/tasks/goal.org").exists());
        assert!(proj.join(".orgasmic/tasks/handoff.org").exists());
    }

    #[test]
    fn init_refuses_existing_dotorg_without_force() {
        let tmp = tempfile::tempdir().unwrap();
        let home = Home::at(tmp.path().join("home"));
        home.ensure().unwrap();
        fake_shipped(&home);
        let proj = tmp.path().join("repo");
        std::fs::create_dir_all(proj.join(".orgasmic")).unwrap();
        let inputs = ScaffoldInputs::derive(&proj, None);
        let err = init_project(&home, &proj, &inputs, false).unwrap_err();
        assert!(format!("{err}").contains("already exists"));
    }

    #[test]
    fn init_appends_pointer_to_existing_claude_md() {
        let tmp = tempfile::tempdir().unwrap();
        let home = Home::at(tmp.path().join("home"));
        home.ensure().unwrap();
        fake_shipped(&home);
        let proj = tmp.path().join("repo");
        std::fs::create_dir_all(&proj).unwrap();
        std::fs::write(proj.join("CLAUDE.md"), "custom instructions\n").unwrap();
        let inputs = ScaffoldInputs::derive(&proj, None);
        init_project(&home, &proj, &inputs, false).unwrap();
        let claude = std::fs::read_to_string(proj.join("CLAUDE.md")).unwrap();
        assert!(claude.starts_with("custom instructions\n"));
        assert!(claude.contains(ROOT_POINTER_TARGET));
        let agents = std::fs::read_to_string(proj.join("AGENTS.md")).unwrap();
        assert_eq!(agents, "Read `CLAUDE.md`.\n");
    }

    #[test]
    fn init_leaves_already_routing_claude_md_untouched() {
        let tmp = tempfile::tempdir().unwrap();
        let home = Home::at(tmp.path().join("home"));
        home.ensure().unwrap();
        fake_shipped(&home);
        let proj = tmp.path().join("repo");
        std::fs::create_dir_all(&proj).unwrap();
        let existing = "custom instructions\n\nStart at .orgasmic/entry.org for state.\n";
        std::fs::write(proj.join("CLAUDE.md"), existing).unwrap();
        let inputs = ScaffoldInputs::derive(&proj, None);
        init_project(&home, &proj, &inputs, false).unwrap();
        let claude = std::fs::read_to_string(proj.join("CLAUDE.md")).unwrap();
        assert_eq!(claude, existing);
        let agents = std::fs::read_to_string(proj.join("AGENTS.md")).unwrap();
        assert_eq!(agents, "Read `CLAUDE.md`.\n");
    }

    #[test]
    fn init_routes_agents_md_only_repo() {
        let tmp = tempfile::tempdir().unwrap();
        let home = Home::at(tmp.path().join("home"));
        home.ensure().unwrap();
        fake_shipped(&home);
        let proj = tmp.path().join("repo");
        std::fs::create_dir_all(&proj).unwrap();
        std::fs::write(proj.join("AGENTS.md"), "agents-only instructions\n").unwrap();
        let inputs = ScaffoldInputs::derive(&proj, None);
        init_project(&home, &proj, &inputs, false).unwrap();
        let agents = std::fs::read_to_string(proj.join("AGENTS.md")).unwrap();
        assert!(agents.starts_with("agents-only instructions\n"));
        assert!(agents.contains(ROOT_POINTER_TARGET));
        let claude = std::fs::read_to_string(proj.join("CLAUDE.md")).unwrap();
        assert_eq!(claude, "Read `AGENTS.md`.\n");
    }

    #[test]
    fn init_anchors_mutual_defer_cycle() {
        let tmp = tempfile::tempdir().unwrap();
        let home = Home::at(tmp.path().join("home"));
        home.ensure().unwrap();
        fake_shipped(&home);
        let proj = tmp.path().join("repo");
        std::fs::create_dir_all(&proj).unwrap();
        std::fs::write(proj.join("CLAUDE.md"), "see AGENTS.md\n").unwrap();
        std::fs::write(proj.join("AGENTS.md"), "see CLAUDE.md\n").unwrap();
        let inputs = ScaffoldInputs::derive(&proj, None);
        init_project(&home, &proj, &inputs, false).unwrap();
        let claude = std::fs::read_to_string(proj.join("CLAUDE.md")).unwrap();
        let agents = std::fs::read_to_string(proj.join("AGENTS.md")).unwrap();
        assert!(agents.contains(ROOT_POINTER_TARGET));
        assert!(agents.starts_with("see CLAUDE.md\n"));
        assert_eq!(claude, "see AGENTS.md\n");
    }

    #[test]
    fn scaffold_manifest_matches_shipped_directory() {
        // SCAFFOLD_FILES must track shipped/project-scaffold exactly,
        // or daemon/CLI-inited projects silently diverge from the runtime
        // scaffold (this drifted once: conventions/manager-implementer.org).
        let shipped = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../shipped")
            .join(SCAFFOLD_SHIPPED_DIR);
        fn walk(dir: &Path, base: &Path, out: &mut Vec<String>) {
            for entry in std::fs::read_dir(dir).unwrap() {
                let path = entry.unwrap().path();
                if path.is_dir() {
                    walk(&path, base, out);
                } else {
                    out.push(
                        path.strip_prefix(base)
                            .unwrap()
                            .to_string_lossy()
                            .replace('\\', "/"),
                    );
                }
            }
        }
        let mut on_disk = Vec::new();
        walk(&shipped, &shipped, &mut on_disk);
        on_disk.sort();
        let mut manifest: Vec<String> = SCAFFOLD_FILES.iter().map(|s| s.to_string()).collect();
        manifest.sort();
        assert_eq!(
            on_disk, manifest,
            "shipped project-scaffold/ and SCAFFOLD_FILES disagree"
        );
    }

    #[test]
    fn user_override_supersedes_shipped_template() {
        let tmp = tempfile::tempdir().unwrap();
        let home = Home::at(tmp.path().join("home"));
        home.ensure().unwrap();
        fake_shipped(&home);
        let override_dir = home.user().join("project-scaffold");
        std::fs::create_dir_all(override_dir.join("tasks")).unwrap();
        std::fs::write(
            override_dir.join("tasks/backlog.org"),
            "#+title: Backlog\n#+orgasmic_version: 1\n\n* CUSTOM\n",
        )
        .unwrap();
        let proj = tmp.path().join("repo2");
        std::fs::create_dir_all(&proj).unwrap();
        let inputs = ScaffoldInputs::derive(&proj, None);
        init_project(&home, &proj, &inputs, false).unwrap();
        let backlog = std::fs::read_to_string(proj.join(".orgasmic/tasks/backlog.org")).unwrap();
        assert!(backlog.contains("* CUSTOM"));
    }

    #[test]
    fn register_and_list_roundtrip() {
        let tmp = tempfile::tempdir().unwrap();
        let home = Home::at(tmp.path().join("home"));
        home.ensure().unwrap();
        let proj = tmp.path().join("repo");
        std::fs::create_dir_all(&proj).unwrap();
        register_project(&home, &proj, "proj-x", "main").unwrap();
        register_project(&home, &proj, "proj-y", "develop").unwrap();
        let entries = read_board(&home).unwrap();
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0].id, "proj-x");
        assert_eq!(entries[1].branch, "develop");
        let board = std::fs::read_to_string(board_path(&home)).unwrap();
        OrgFile::parse(board, "board.org").unwrap();
    }

    #[test]
    fn rejects_duplicate_project_id() {
        let tmp = tempfile::tempdir().unwrap();
        let home = Home::at(tmp.path().join("home"));
        home.ensure().unwrap();
        let proj = tmp.path().join("repo");
        std::fs::create_dir_all(&proj).unwrap();
        register_project(&home, &proj, "proj-x", "main").unwrap();
        let err = register_project(&home, &proj, "proj-x", "main").unwrap_err();
        assert!(format!("{err}").contains("already on the board"));
    }

    #[cfg(unix)]
    #[test]
    fn register_project_canonicalizes_symlink_root() {
        let tmp = tempfile::tempdir().unwrap();
        let home = Home::at(tmp.path().join("home"));
        home.ensure().unwrap();
        let real = tmp.path().join("repo");
        let link = tmp.path().join("repo-link");
        std::fs::create_dir_all(&real).unwrap();
        std::os::unix::fs::symlink(&real, &link).unwrap();

        register_project(&home, &link, "proj-x", "main").unwrap();

        let entries = read_board(&home).unwrap();
        assert_eq!(entries[0].path, std::fs::canonicalize(&real).unwrap());
    }
}
