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

/// After a dispatch worktree at `.orgasmic/tmp/dispatch/<stem>/worktree` is
/// removed, drop only the selected attempt's transient artifacts while
/// retaining the brief and sibling attempts. No-op for worktrees outside the
/// dispatch layout.
// orgasmic:TASK-ZHRRH,TASK-AFE5Q
pub fn prune_dispatch_stem_after_worktree(
    worktree_path: &Path,
    last_path: Option<&Path>,
    stdout_path: Option<&Path>,
) {
    if worktree_path.file_name().and_then(|s| s.to_str()) != Some("worktree") {
        return;
    }
    let Some(stem_dir) = worktree_path.parent() else {
        return;
    };
    if !is_dispatch_stem_dir(stem_dir) {
        return;
    };
    for artifact in [last_path, stdout_path].into_iter().flatten() {
        if dispatch_artifact_under_stem(stem_dir, artifact) && artifact.is_file() {
            let _ = std::fs::remove_file(artifact);
        }
    }
    let leftover_worktree = stem_dir.join("worktree");
    if leftover_worktree.is_dir() {
        let _ = std::fs::remove_dir(&leftover_worktree);
    }
}

fn dispatch_artifact_under_stem(stem_dir: &Path, artifact: &Path) -> bool {
    artifact.parent() == Some(stem_dir)
}

fn is_dispatch_stem_dir(stem_dir: &Path) -> bool {
    let Some(dispatch_dir) = stem_dir.parent() else {
        return false;
    };
    if dispatch_dir.file_name().and_then(|s| s.to_str()) != Some("dispatch") {
        return false;
    }
    let Some(tmp_dir) = dispatch_dir.parent() else {
        return false;
    };
    tmp_dir.file_name().and_then(|s| s.to_str()) == Some("tmp")
        && tmp_dir
            .parent()
            .and_then(|p| p.file_name())
            .and_then(|s| s.to_str())
            == Some(DOTORG)
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
    fn prune_dispatch_stem_removes_only_selected_attempt_artifacts() {
        let tmp = tempfile::tempdir().unwrap();
        let stem_dir = tmp.path().join(".orgasmic/tmp/dispatch/task-dispatch");
        std::fs::create_dir_all(stem_dir.join("worktree")).unwrap();
        let worktree = stem_dir.join("worktree");
        let attempt_a_last = stem_dir.join("task-dispatch-aaaa-last.txt");
        let attempt_a_stdout = stem_dir.join("task-dispatch-aaaa-stdout.log");
        let attempt_b_last = stem_dir.join("task-dispatch-bbbb-last.txt");
        let attempt_b_stdout = stem_dir.join("task-dispatch-bbbb-stdout.log");
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
            &worktree,
            Some(&attempt_a_last),
            Some(&attempt_a_stdout),
        );

        assert!(!attempt_a_last.exists());
        assert!(!attempt_a_stdout.exists());
        assert!(attempt_b_last.exists());
        assert!(attempt_b_stdout.exists());
        assert!(legacy_last.exists());
    }
}
