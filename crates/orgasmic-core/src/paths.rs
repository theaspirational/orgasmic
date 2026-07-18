//! Canonical paths for task and goal files under `.orgasmic/tasks/`.

use std::path::{Path, PathBuf};

use crate::schema::LifecycleStage;

pub const TASKS_DIR: &str = "tasks";
/// One org file per kanban lifecycle state (dec_QQYXM). Goal/handoff are
/// manager surfaces and are not in this list — see `GOAL_FILE` / handoff path.
pub const TASK_FILE_NAMES: &[&str] = &[
    "backlog.org",
    "todo.org",
    "in_progress.org",
    "in_review.org",
    "done.org",
    "cancelled.org",
];
pub const GOAL_FILE: &str = "goal.org";
pub const HANDOFF_FILE: &str = "handoff.org";

/// Default task file for new top-level tasks and tx targets.
pub const DEFAULT_TASK_FILE: &str = "backlog.org";

/// Relative path to the default task file for tx targets and stage specs.
pub const DEFAULT_TASK_FILE_REL: &str = ".orgasmic/tasks/backlog.org";

const DOTORG: &str = ".orgasmic";

pub fn lifecycle_stage_file_name(stage: LifecycleStage) -> &'static str {
    match stage {
        LifecycleStage::Backlog => "backlog.org",
        LifecycleStage::Todo => "todo.org",
        LifecycleStage::InProgress => "in_progress.org",
        LifecycleStage::InReview => "in_review.org",
        LifecycleStage::Done => "done.org",
        LifecycleStage::Cancelled => "cancelled.org",
    }
}

pub fn dotorg_tasks_dir(project_root: &Path) -> PathBuf {
    project_root.join(DOTORG).join(TASKS_DIR)
}

pub fn task_file_path(project_root: &Path, name: &str) -> PathBuf {
    dotorg_tasks_dir(project_root).join(name)
}

pub fn task_file_rel(name: &str) -> String {
    format!("{DOTORG}/{TASKS_DIR}/{name}")
}

pub fn goal_file_path(project_root: &Path) -> PathBuf {
    dotorg_tasks_dir(project_root).join(GOAL_FILE)
}

pub fn goal_file_rel() -> &'static str {
    concat!(".orgasmic/tasks/", "goal.org")
}

pub fn handoff_file_path(project_root: &Path) -> PathBuf {
    dotorg_tasks_dir(project_root).join(HANDOFF_FILE)
}

pub fn iter_task_file_paths(project_root: &Path) -> impl Iterator<Item = PathBuf> + '_ {
    TASK_FILE_NAMES
        .iter()
        .map(move |name| task_file_path(project_root, name))
}

/// Project-local directory for transient workflow files (git-ignored via
/// `.orgasmic/.gitignore`). Everything orgasmic creates during a session that
/// is not durable project state lives under here.
pub fn project_tmp_dir(project_root: &Path) -> PathBuf {
    project_root.join(DOTORG).join("tmp")
}

/// Per-project session transcript directory (`.orgasmic/tmp/sessions/`). The
/// source of truth for per-run JSONL; the daemon writes here and boot recovery
/// enumerates these per project.
pub fn project_sessions_dir(project_root: &Path) -> PathBuf {
    project_tmp_dir(project_root).join("sessions")
}

/// Per-project dispatch artifact base (`.orgasmic/tmp/dispatch/`). Briefs,
/// last-message files, and stdout logs live in a per-task subfolder under here.
pub fn project_dispatch_dir(project_root: &Path) -> PathBuf {
    project_tmp_dir(project_root).join("dispatch")
}

/// Validated dispatch attempt artifact pair for cleanup.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DispatchAttemptArtifacts {
    pub stem: String,
    pub attempt_id: Option<String>,
    pub last_path: PathBuf,
    pub stdout_path: PathBuf,
}

/// Validate that `worktree_path` and the artifact pair belong to the selected
/// project's dispatch surface before any deletion (TASK-ZGT1X).
// orgasmic:TASK-ZHRRH,TASK-AFE5Q,TASK-ZGT1X
pub fn validate_dispatch_cleanup_targets(
    project_root: &Path,
    worktree_path: &Path,
    last_path: Option<&Path>,
    stdout_path: Option<&Path>,
) -> Result<DispatchAttemptArtifacts, String> {
    let (stem_dir, stem) = validate_dispatch_worktree_layout(project_root, worktree_path)?;
    let last = last_path.ok_or_else(|| "last_path required for dispatch cleanup".to_string())?;
    let stdout =
        stdout_path.ok_or_else(|| "stdout_path required for dispatch cleanup".to_string())?;
    validate_dispatch_artifact_pair(&stem_dir, &stem, last, stdout)
}

/// After a dispatch worktree is removed, drop only the validated attempt's
/// transient artifacts while retaining the brief and sibling attempts.
// orgasmic:TASK-ZHRRH,TASK-AFE5Q,TASK-ZGT1X
pub fn prune_dispatch_stem_after_worktree(
    project_root: &Path,
    worktree_path: &Path,
    last_path: Option<&Path>,
    stdout_path: Option<&Path>,
) -> Result<(), String> {
    let artifacts =
        validate_dispatch_cleanup_targets(project_root, worktree_path, last_path, stdout_path)?;
    prune_validated_dispatch_attempt(&artifacts)
}

pub fn prune_validated_dispatch_attempt(
    artifacts: &DispatchAttemptArtifacts,
) -> Result<(), String> {
    for path in [&artifacts.last_path, &artifacts.stdout_path] {
        if path.is_file() {
            std::fs::remove_file(path).map_err(|err| err.to_string())?;
        }
    }
    Ok(())
}

fn validate_dispatch_worktree_layout(
    project_root: &Path,
    worktree_path: &Path,
) -> Result<(PathBuf, String), String> {
    if worktree_path.file_name().and_then(|s| s.to_str()) != Some("worktree") {
        return Err("worktree path must end with /worktree".into());
    }
    let canonical_root = canonicalize_dir(project_root)?;
    let canonical_worktree = canonicalize_path(worktree_path)?;
    if !path_within(&canonical_worktree, &canonical_root) {
        return Err("worktree outside project root".into());
    }
    let stem_dir = canonical_worktree
        .parent()
        .ok_or_else(|| "worktree has no parent stem dir".to_string())?
        .to_path_buf();
    let expected_dispatch = canonicalize_dir(&project_dispatch_dir(project_root))?;
    if stem_dir.parent() != Some(expected_dispatch.as_path()) {
        return Err("worktree not under project dispatch dir".into());
    }
    let stem = stem_dir
        .file_name()
        .and_then(|s| s.to_str())
        .ok_or_else(|| "dispatch stem dir has no name".to_string())?
        .to_string();
    if stem.contains("..") {
        return Err("invalid dispatch stem".into());
    }
    Ok((stem_dir, stem))
}

fn validate_dispatch_artifact_pair(
    stem_dir: &Path,
    stem: &str,
    last_path: &Path,
    stdout_path: &Path,
) -> Result<DispatchAttemptArtifacts, String> {
    let canonical_last = validate_dispatch_artifact_file(stem_dir, stem, last_path)?;
    let canonical_stdout = validate_dispatch_artifact_file(stem_dir, stem, stdout_path)?;
    let last_name = canonical_last
        .file_name()
        .and_then(|s| s.to_str())
        .ok_or_else(|| "last artifact has no filename".to_string())?;
    let stdout_name = canonical_stdout
        .file_name()
        .and_then(|s| s.to_str())
        .ok_or_else(|| "stdout artifact has no filename".to_string())?;
    let (last_attempt, last_kind) = parse_dispatch_artifact_name(stem, last_name)?;
    let (stdout_attempt, stdout_kind) = parse_dispatch_artifact_name(stem, stdout_name)?;
    if last_kind != "last" || stdout_kind != "stdout" {
        return Err("artifact pair must be last.txt and stdout.log".into());
    }
    if last_attempt != stdout_attempt {
        return Err("artifact pair attempt id mismatch".into());
    }
    if last_name.ends_with("-brief.md") || stdout_name.ends_with("-brief.md") {
        return Err("brief path cannot be a cleanup artifact".into());
    }
    Ok(DispatchAttemptArtifacts {
        stem: stem.to_string(),
        attempt_id: last_attempt,
        last_path: canonical_last,
        stdout_path: canonical_stdout,
    })
}

fn validate_dispatch_artifact_file(
    stem_dir: &Path,
    stem: &str,
    artifact: &Path,
) -> Result<PathBuf, String> {
    let canonical = canonicalize_path(artifact)?;
    if canonical.parent() != Some(stem_dir) {
        return Err(format!(
            "artifact {} not directly under expected stem dir",
            artifact.display()
        ));
    }
    let meta = std::fs::metadata(&canonical).map_err(|err| err.to_string())?;
    if !meta.is_file() {
        return Err(format!("{} is not a regular file", artifact.display()));
    }
    if meta.file_type().is_symlink() {
        return Err(format!("{} is a symlink", artifact.display()));
    }
    let file_name = canonical
        .file_name()
        .and_then(|s| s.to_str())
        .ok_or_else(|| "artifact has no filename".to_string())?;
    if file_name == format!("{stem}-brief.md") {
        return Err("brief path cannot be deleted as dispatch artifact".into());
    }
    parse_dispatch_artifact_name(stem, file_name)?;
    Ok(canonical)
}

fn parse_dispatch_artifact_name(
    stem: &str,
    file_name: &str,
) -> Result<(Option<String>, &'static str), String> {
    let prefix = format!("{stem}-");
    if !file_name.starts_with(&prefix) {
        return Err(format!("artifact filename must start with {prefix}"));
    }
    let rest = &file_name[prefix.len()..];
    if rest == "last.txt" {
        return Ok((None, "last"));
    }
    if rest == "stdout.log" {
        return Ok((None, "stdout"));
    }
    if let Some(id) = rest.strip_suffix("-last.txt") {
        if is_full_uuid(id) {
            return Ok((Some(id.to_string()), "last"));
        }
    }
    if let Some(id) = rest.strip_suffix("-stdout.log") {
        if is_full_uuid(id) {
            return Ok((Some(id.to_string()), "stdout"));
        }
    }
    Err(format!("unrecognized dispatch artifact name {file_name}"))
}

fn is_full_uuid(value: &str) -> bool {
    value.len() == 32 && value.chars().all(|c| c.is_ascii_hexdigit())
}

fn canonicalize_dir(path: &Path) -> Result<PathBuf, String> {
    std::fs::create_dir_all(path).map_err(|err| err.to_string())?;
    std::fs::canonicalize(path).map_err(|err| err.to_string())
}

fn canonicalize_path(path: &Path) -> Result<PathBuf, String> {
    if path
        .components()
        .any(|c| matches!(c, std::path::Component::ParentDir))
    {
        return Err(format!("path contains ..: {}", path.display()));
    }
    std::fs::canonicalize(path).map_err(|err| err.to_string())
}

fn path_within(path: &Path, root: &Path) -> bool {
    path.starts_with(root)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn task_file_rel_matches_helpers() {
        let root = Path::new("/repo");
        assert_eq!(
            task_file_path(root, "backlog.org"),
            PathBuf::from("/repo/.orgasmic/tasks/backlog.org")
        );
        assert_eq!(task_file_rel("backlog.org"), ".orgasmic/tasks/backlog.org");
        assert_eq!(goal_file_rel(), ".orgasmic/tasks/goal.org");
        assert_eq!(
            goal_file_path(root),
            PathBuf::from("/repo/.orgasmic/tasks/goal.org")
        );
        let paths: Vec<_> = iter_task_file_paths(root).collect();
        assert_eq!(paths.len(), 6);
        assert!(paths[0].ends_with("backlog.org"));
        assert_eq!(DEFAULT_TASK_FILE, "backlog.org");
        assert_eq!(DEFAULT_TASK_FILE_REL, ".orgasmic/tasks/backlog.org");
    }

    #[test]
    fn lifecycle_stage_file_names_cover_all_states() {
        assert_eq!(
            lifecycle_stage_file_name(LifecycleStage::Backlog),
            "backlog.org"
        );
        assert_eq!(
            lifecycle_stage_file_name(LifecycleStage::InProgress),
            "in_progress.org"
        );
        assert_eq!(lifecycle_stage_file_name(LifecycleStage::Done), "done.org");
        for &name in TASK_FILE_NAMES {
            assert!(name.ends_with(".org"));
        }
    }

    #[test]
    fn prune_dispatch_stem_removes_only_selected_attempt_artifacts() {
        let tmp = tempfile::tempdir().unwrap();
        let project_root = tmp.path().join("repo");
        let stem_dir = project_root.join(".orgasmic/tmp/dispatch/task-dispatch");
        std::fs::create_dir_all(stem_dir.join("worktree")).unwrap();
        let worktree = stem_dir.join("worktree");
        let attempt_a_last =
            stem_dir.join("task-dispatch-aaaa1111bbbb2222cccc3333dddd4444-last.txt");
        let attempt_a_stdout =
            stem_dir.join("task-dispatch-aaaa1111bbbb2222cccc3333dddd4444-stdout.log");
        let attempt_b_last =
            stem_dir.join("task-dispatch-bbbb1111cccc2222dddd3333eeee4444-last.txt");
        let attempt_b_stdout =
            stem_dir.join("task-dispatch-bbbb1111cccc2222dddd3333eeee4444-stdout.log");
        let legacy_last = stem_dir.join("task-dispatch-last.txt");
        for path in [
            &attempt_a_last,
            &attempt_a_stdout,
            &attempt_b_last,
            &attempt_b_stdout,
            &legacy_last,
        ] {
            std::fs::write(path, "artifact").unwrap();
        }

        prune_dispatch_stem_after_worktree(
            &project_root,
            &worktree,
            Some(&attempt_a_last),
            Some(&attempt_a_stdout),
        )
        .unwrap();

        assert!(!attempt_a_last.exists());
        assert!(!attempt_a_stdout.exists());
        assert!(attempt_b_last.exists());
        assert!(attempt_b_stdout.exists());
        assert!(legacy_last.exists());
    }

    #[test]
    fn validate_dispatch_cleanup_rejects_brief_and_mismatched_pair() {
        let tmp = tempfile::tempdir().unwrap();
        let project_root = tmp.path().join("repo");
        let stem_dir = project_root.join(".orgasmic/tmp/dispatch/task-dispatch");
        std::fs::create_dir_all(stem_dir.join("worktree")).unwrap();
        let worktree = stem_dir.join("worktree");
        let brief = stem_dir.join("task-dispatch-brief.md");
        let last_a = stem_dir.join("task-dispatch-aaaa1111bbbb2222cccc3333dddd4444-last.txt");
        let stdout_b = stem_dir.join("task-dispatch-bbbb1111cccc2222dddd3333eeee4444-stdout.log");
        for path in [&brief, &last_a, &stdout_b] {
            std::fs::write(path, "x").unwrap();
        }
        assert!(validate_dispatch_cleanup_targets(
            &project_root,
            &worktree,
            Some(&brief),
            Some(&stdout_b)
        )
        .is_err());
        assert!(validate_dispatch_cleanup_targets(
            &project_root,
            &worktree,
            Some(&last_a),
            Some(&stdout_b)
        )
        .is_err());
    }

    #[test]
    fn validate_dispatch_cleanup_rejects_external_suffix_lookalike() {
        let tmp = tempfile::tempdir().unwrap();
        let project_root = tmp.path().join("repo");
        std::fs::create_dir_all(&project_root).unwrap();
        let external = tmp
            .path()
            .join("fake/.orgasmic/tmp/dispatch/task-dispatch/worktree");
        std::fs::create_dir_all(&external).unwrap();
        let last = external
            .parent()
            .unwrap()
            .join("task-dispatch-aaaa1111bbbb2222cccc3333dddd4444-last.txt");
        let stdout = external
            .parent()
            .unwrap()
            .join("task-dispatch-aaaa1111bbbb2222cccc3333dddd4444-stdout.log");
        for path in [&last, &stdout] {
            std::fs::write(path, "x").unwrap();
        }
        assert!(validate_dispatch_cleanup_targets(
            &project_root,
            &external,
            Some(&last),
            Some(&stdout)
        )
        .is_err());
    }
}
