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

/// Validated dispatch attempt artifact pair for cleanup. Stores the opened
/// stem directory handle, no-follow artifact file handles, and relative names
/// so unlink targets the validated inode, not a replaced same-name entry
/// (TASK-KE0JW, TASK-1FV1N).
pub struct DispatchAttemptArtifacts {
    pub stem: String,
    pub attempt_id: Option<String>,
    worktree_handle: std::fs::File,
    stem_dir_handle: std::fs::File,
    last_name: String,
    stdout_name: String,
    #[cfg(unix)]
    last_file: std::fs::File,
    #[cfg(unix)]
    stdout_file: std::fs::File,
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
    let last = last_path.ok_or_else(|| "last_path required for dispatch cleanup".to_string())?;
    let stdout =
        stdout_path.ok_or_else(|| "stdout_path required for dispatch cleanup".to_string())?;
    let worktree_handle = validate_dispatch_worktree(worktree_path)?;
    let stem_dir = last
        .parent()
        .ok_or_else(|| "last_path has no parent stem dir".to_string())?;
    let stem_dir = canonicalize_path(stem_dir)?;
    let expected_dispatch = canonicalize_dir(&project_dispatch_dir(project_root))?;
    if stem_dir.parent() != Some(expected_dispatch.as_path()) {
        return Err("artifacts not under project dispatch dir".into());
    }
    let stem = stem_dir
        .file_name()
        .and_then(|s| s.to_str())
        .ok_or_else(|| "dispatch stem dir has no name".to_string())?
        .to_string();
    if stem.contains("..") {
        return Err("invalid dispatch stem".into());
    }
    validate_dispatch_artifact_pair(&stem_dir, &stem, last, stdout, worktree_handle)
}

/// Re-open the worktree without following symlinks and prove that the path
/// still names the directory retained by cleanup validation.
pub fn verify_dispatch_worktree_identity(
    artifacts: &DispatchAttemptArtifacts,
    worktree_path: &Path,
) -> Result<(), String> {
    let current = open_dispatch_dir(worktree_path)?;
    same_file_identity(&artifacts.worktree_handle, &current)
        .then_some(())
        .ok_or_else(|| "worktree identity changed after cleanup validation".to_string())
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
    #[cfg(unix)]
    {
        unlink_validated_artifact(
            &artifacts.stem_dir_handle,
            &artifacts.last_name,
            &artifacts.last_file,
        )?;
        unlink_validated_artifact(
            &artifacts.stem_dir_handle,
            &artifacts.stdout_name,
            &artifacts.stdout_file,
        )?;
    }
    #[cfg(not(unix))]
    {
        let _ = artifacts;
        return Err("no-follow dispatch artifact deletion requires unix".into());
    }
    Ok(())
}

fn validate_dispatch_worktree(worktree_path: &Path) -> Result<std::fs::File, String> {
    if worktree_path
        .components()
        .any(|c| matches!(c, std::path::Component::ParentDir))
    {
        return Err(format!("path contains ..: {}", worktree_path.display()));
    }
    let wt_meta = std::fs::symlink_metadata(worktree_path).map_err(|err| err.to_string())?;
    if wt_meta.file_type().is_symlink() {
        return Err(format!(
            "worktree path is a symlink: {}",
            worktree_path.display()
        ));
    }
    if !wt_meta.is_dir() {
        return Err(format!(
            "worktree path is not a directory: {}",
            worktree_path.display()
        ));
    }
    open_dispatch_dir(worktree_path)
}

fn validate_dispatch_artifact_pair(
    stem_dir: &Path,
    stem: &str,
    last_path: &Path,
    stdout_path: &Path,
    worktree_handle: std::fs::File,
) -> Result<DispatchAttemptArtifacts, String> {
    let stem_dir_handle = open_dispatch_dir(stem_dir)?;
    let last_name = validate_dispatch_artifact_file(stem_dir, stem, last_path)?;
    let stdout_name = validate_dispatch_artifact_file(stem_dir, stem, stdout_path)?;
    let (last_attempt, last_kind) = parse_dispatch_artifact_name(stem, &last_name)?;
    let (stdout_attempt, stdout_kind) = parse_dispatch_artifact_name(stem, &stdout_name)?;
    if last_kind != "last" || stdout_kind != "stdout" {
        return Err("artifact pair must be last.txt and stdout.log".into());
    }
    if last_attempt != stdout_attempt {
        return Err("artifact pair attempt id mismatch".into());
    }
    if last_name.ends_with("-brief.md") || stdout_name.ends_with("-brief.md") {
        return Err("brief path cannot be a cleanup artifact".into());
    }
    #[cfg(unix)]
    let (last_file, stdout_file) = {
        let last_file = open_artifact_in_stem_dir(&stem_dir_handle, &last_name)?;
        let stdout_file = open_artifact_in_stem_dir(&stem_dir_handle, &stdout_name)?;
        (last_file, stdout_file)
    };
    Ok(DispatchAttemptArtifacts {
        stem: stem.to_string(),
        attempt_id: last_attempt,
        worktree_handle,
        stem_dir_handle,
        last_name,
        stdout_name,
        #[cfg(unix)]
        last_file,
        #[cfg(unix)]
        stdout_file,
    })
}

#[cfg(unix)]
fn open_dispatch_dir(stem_dir: &Path) -> Result<std::fs::File, String> {
    use std::os::unix::fs::OpenOptionsExt;
    std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC)
        .open(stem_dir)
        .map_err(|err| err.to_string())
}

#[cfg(not(unix))]
fn open_dispatch_dir(_stem_dir: &Path) -> Result<std::fs::File, String> {
    Err("no-follow dispatch cleanup requires unix".into())
}

#[cfg(unix)]
fn same_file_identity(left: &std::fs::File, right: &std::fs::File) -> bool {
    use std::os::unix::fs::MetadataExt;
    match (left.metadata(), right.metadata()) {
        (Ok(left), Ok(right)) => left.dev() == right.dev() && left.ino() == right.ino(),
        _ => false,
    }
}

#[cfg(not(unix))]
fn same_file_identity(_left: &std::fs::File, _right: &std::fs::File) -> bool {
    false
}

#[cfg(unix)]
fn open_artifact_in_stem_dir(
    stem_dir: &std::fs::File,
    name: &str,
) -> Result<std::fs::File, String> {
    use std::ffi::CString;
    use std::os::unix::io::AsRawFd;

    if name.contains('/') || name.contains('\0') {
        return Err(format!("invalid artifact name {name}"));
    }
    let dir_fd = stem_dir.as_raw_fd();
    let name_c = CString::new(name).map_err(|_| format!("invalid artifact name {name}"))?;
    let file_fd = unsafe {
        libc::openat(
            dir_fd,
            name_c.as_ptr(),
            libc::O_RDONLY | libc::O_NOFOLLOW | libc::O_CLOEXEC,
        )
    };
    if file_fd < 0 {
        return Err(std::io::Error::last_os_error().to_string());
    }
    Ok(unsafe { std::fs::File::from_raw_fd(file_fd) })
}

fn validate_dispatch_artifact_file(
    stem_dir: &Path,
    stem: &str,
    artifact: &Path,
) -> Result<String, String> {
    if artifact
        .components()
        .any(|c| matches!(c, std::path::Component::ParentDir))
    {
        return Err(format!("path contains ..: {}", artifact.display()));
    }
    let meta = std::fs::symlink_metadata(artifact).map_err(|err| err.to_string())?;
    if meta.file_type().is_symlink() {
        return Err(format!("{} is a symlink", artifact.display()));
    }
    if !meta.is_file() {
        return Err(format!("{} is not a regular file", artifact.display()));
    }
    let file_name = artifact
        .file_name()
        .and_then(|s| s.to_str())
        .ok_or_else(|| "artifact has no filename".to_string())?;
    if file_name == format!("{stem}-brief.md") {
        return Err("brief path cannot be deleted as dispatch artifact".into());
    }
    let parent = artifact
        .parent()
        .ok_or_else(|| format!("artifact {} has no parent", artifact.display()))?;
    let canonical_parent = std::fs::canonicalize(parent).map_err(|err| err.to_string())?;
    if canonical_parent != stem_dir {
        return Err(format!(
            "artifact {} not directly under expected stem dir",
            artifact.display()
        ));
    }
    parse_dispatch_artifact_name(stem, file_name)?;
    Ok(file_name.to_string())
}

#[cfg(unix)]
fn unlink_validated_artifact(
    stem_dir: &std::fs::File,
    name: &str,
    validated_file: &std::fs::File,
) -> Result<(), String> {
    use std::ffi::CString;
    use std::os::unix::io::AsRawFd;

    if name.contains('/') || name.contains('\0') {
        return Err(format!("invalid artifact name {name}"));
    }
    let dir_fd = stem_dir.as_raw_fd();
    let file_fd = validated_file.as_raw_fd();
    let name_c = CString::new(name).map_err(|_| format!("invalid artifact name {name}"))?;
    let mut stat_file = std::mem::MaybeUninit::<libc::stat>::uninit();
    let mut stat_name = std::mem::MaybeUninit::<libc::stat>::uninit();
    unsafe {
        if libc::fstat(file_fd, stat_file.as_mut_ptr()) != 0 {
            return Err(std::io::Error::last_os_error().to_string());
        }
        if libc::fstatat(
            dir_fd,
            name_c.as_ptr(),
            stat_name.as_mut_ptr(),
            libc::AT_SYMLINK_NOFOLLOW,
        ) != 0
        {
            let err = std::io::Error::last_os_error();
            if err.kind() == std::io::ErrorKind::NotFound {
                return Ok(());
            }
            return Err(err.to_string());
        }
        let stat_file = stat_file.assume_init();
        let stat_name = stat_name.assume_init();
        if stat_file.st_ino != stat_name.st_ino || stat_file.st_dev != stat_name.st_dev {
            return Err(format!(
                "artifact identity mismatch before unlink for {name}"
            ));
        }
        let rc = libc::unlinkat(dir_fd, name_c.as_ptr(), 0);
        if rc == 0 {
            Ok(())
        } else {
            let err = std::io::Error::last_os_error();
            if err.kind() == std::io::ErrorKind::NotFound {
                Ok(())
            } else {
                Err(err.to_string())
            }
        }
    }
}

#[cfg(unix)]
use std::os::unix::io::FromRawFd;

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

    #[test]
    fn validate_dispatch_cleanup_accepts_registered_layout_with_external_worktree() {
        let tmp = tempfile::tempdir().unwrap();
        let project_root = tmp.path().join("repo");
        let stem_dir = project_root.join(".orgasmic/tmp/dispatch/task-dispatch");
        let worktree = tmp.path().join("custom-worktrees/task-dispatch");
        std::fs::create_dir_all(&stem_dir).unwrap();
        std::fs::create_dir_all(&worktree).unwrap();
        let last = stem_dir.join("task-dispatch-aaaa1111bbbb2222cccc3333dddd4444-last.txt");
        let stdout = stem_dir.join("task-dispatch-aaaa1111bbbb2222cccc3333dddd4444-stdout.log");
        std::fs::write(&last, "last").unwrap();
        std::fs::write(&stdout, "stdout").unwrap();

        validate_dispatch_cleanup_targets(&project_root, &worktree, Some(&last), Some(&stdout))
            .unwrap();
    }

    #[test]
    fn validate_dispatch_cleanup_rejects_symlink_artifacts() {
        let tmp = tempfile::tempdir().unwrap();
        let project_root = tmp.path().join("repo");
        let stem_dir = project_root.join(".orgasmic/tmp/dispatch/task-dispatch");
        std::fs::create_dir_all(stem_dir.join("worktree")).unwrap();
        let worktree = stem_dir.join("worktree");
        let victim_last = stem_dir.join("task-dispatch-bbbb1111cccc2222dddd3333eeee4444-last.txt");
        let victim_stdout =
            stem_dir.join("task-dispatch-bbbb1111cccc2222dddd3333eeee4444-stdout.log");
        std::fs::write(&victim_last, "victim").unwrap();
        std::fs::write(&victim_stdout, "victim").unwrap();
        let attempt_id = "aaaa1111bbbb2222cccc3333dddd4444";
        let link_last = stem_dir.join(format!("task-dispatch-{attempt_id}-last.txt"));
        let link_stdout = stem_dir.join(format!("task-dispatch-{attempt_id}-stdout.log"));
        #[cfg(unix)]
        {
            std::os::unix::fs::symlink(&victim_last, &link_last).unwrap();
            std::os::unix::fs::symlink(&victim_stdout, &link_stdout).unwrap();
        }
        #[cfg(not(unix))]
        {
            return;
        }
        assert!(validate_dispatch_cleanup_targets(
            &project_root,
            &worktree,
            Some(&link_last),
            Some(&link_stdout)
        )
        .is_err());
        assert!(victim_last.exists());
        assert!(victim_stdout.exists());
    }

    #[test]
    fn prune_validated_dispatch_attempt_survives_stem_dir_swap() {
        let tmp = tempfile::tempdir().unwrap();
        let project_root = tmp.path().join("repo");
        let stem_dir = project_root.join(".orgasmic/tmp/dispatch/task-dispatch");
        std::fs::create_dir_all(stem_dir.join("worktree")).unwrap();
        let worktree = stem_dir.join("worktree");
        let attempt_id = "aaaa1111bbbb2222cccc3333dddd4444";
        let last_name = format!("task-dispatch-{attempt_id}-last.txt");
        let stdout_name = format!("task-dispatch-{attempt_id}-stdout.log");
        std::fs::write(stem_dir.join(&last_name), "last").unwrap();
        std::fs::write(stem_dir.join(&stdout_name), "stdout").unwrap();
        let artifacts = validate_dispatch_cleanup_targets(
            &project_root,
            &worktree,
            Some(&stem_dir.join(&last_name)),
            Some(&stem_dir.join(&stdout_name)),
        )
        .unwrap();
        // Simulate TOCTOU: rename stem dir and replace with symlink elsewhere.
        let renamed = project_root.join(".orgasmic/tmp/dispatch/task-dispatch-old");
        std::fs::rename(&stem_dir, &renamed).unwrap();
        let bait = tmp.path().join("bait");
        std::fs::create_dir_all(&bait).unwrap();
        std::fs::write(bait.join(&last_name), "bait-last").unwrap();
        std::fs::write(bait.join(&stdout_name), "bait-stdout").unwrap();
        #[cfg(unix)]
        std::os::unix::fs::symlink(&bait, &stem_dir).unwrap();
        #[cfg(not(unix))]
        return;

        prune_validated_dispatch_attempt(&artifacts).unwrap();
        assert!(!renamed.join(&last_name).exists());
        assert!(!renamed.join(&stdout_name).exists());
        assert!(bait.join(&last_name).exists());
        assert!(bait.join(&stdout_name).exists());
    }

    #[test]
    fn retained_worktree_identity_rejects_path_swap() {
        let tmp = tempfile::tempdir().unwrap();
        let project_root = tmp.path().join("repo");
        let stem_dir = project_root.join(".orgasmic/tmp/dispatch/task-dispatch");
        let worktree = stem_dir.join("worktree");
        std::fs::create_dir_all(&worktree).unwrap();
        let last = stem_dir.join("task-dispatch-aaaa1111bbbb2222cccc3333dddd4444-last.txt");
        let stdout = stem_dir.join("task-dispatch-aaaa1111bbbb2222cccc3333dddd4444-stdout.log");
        std::fs::write(&last, "last").unwrap();
        std::fs::write(&stdout, "stdout").unwrap();
        let artifacts =
            validate_dispatch_cleanup_targets(&project_root, &worktree, Some(&last), Some(&stdout))
                .unwrap();

        std::fs::rename(&worktree, stem_dir.join("original-worktree")).unwrap();
        std::fs::create_dir(&worktree).unwrap();
        assert!(verify_dispatch_worktree_identity(&artifacts, &worktree).is_err());
    }
}
