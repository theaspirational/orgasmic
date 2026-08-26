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

/// Sorted `node.org` paths for one dir-backed collection.
pub fn collection_node_file_paths(
    project_root: &Path,
    collection: &str,
) -> std::io::Result<Vec<PathBuf>> {
    let dir = project_root.join(DOTORG).join(collection);
    let entries = match std::fs::read_dir(&dir) {
        Ok(entries) => entries,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(err) => return Err(err),
    };
    let mut paths = Vec::new();
    for entry in entries {
        let path = entry?.path().join(crate::node_kernel::NODE_FILE);
        if path.is_file() {
            paths.push(path);
        }
    }
    paths.sort();
    Ok(paths)
}

pub fn task_node_file_path(project_root: &Path, id: &str) -> PathBuf {
    project_root
        .join(DOTORG)
        .join(TASKS_DIR)
        .join(id)
        .join(crate::node_kernel::NODE_FILE)
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

/// Durable, git-tracked home for a closed dispatch's worker report
/// (TASK-QGWK7). Lives under `.orgasmic/` but outside gitignored `tmp/`, so a
/// fresh clone can still read it. Keyed by the dispatch generation
/// (`started_tx`), not the task — a chain of six dispatches across three tasks
/// keeps six distinct records.
pub fn project_dispatch_records_dir(project_root: &Path) -> PathBuf {
    project_root.join(DOTORG).join("dispatch-records")
}

/// Directory for one dispatch generation's promoted record.
pub fn dispatch_record_dir(project_root: &Path, started_tx: &str) -> Result<PathBuf, String> {
    Ok(project_dispatch_records_dir(project_root).join(sanitize_started_tx(started_tx)?))
}

/// Repo-relative path recorded as `:REPORT_PATH:` on the close (and optionally
/// `*.reported`) tx. Always names `last.txt` — the worker summary — not the
/// harness `stdout.log` that sits beside it.
pub fn dispatch_record_report_rel(started_tx: &str) -> Result<String, String> {
    let started_tx = sanitize_started_tx(started_tx)?;
    Ok(format!("{DOTORG}/dispatch-records/{started_tx}/last.txt"))
}

fn sanitize_started_tx(started_tx: &str) -> Result<&str, String> {
    let trimmed = started_tx.trim();
    if trimmed.is_empty()
        || trimmed.contains('/')
        || trimmed.contains('\\')
        || trimmed.contains("..")
        || trimmed.contains('\0')
    {
        return Err(format!(
            "invalid started_tx for dispatch record: {started_tx}"
        ));
    }
    Ok(trimmed)
}

/// Max bytes of harness `stdout.log` promoted into permanent git history
/// (TASK-QGWK7.1). The original byte count is recorded beside the promoted
/// excerpt so truncation is visible.
pub const STDOUT_PROMOTE_MAX_BYTES: u64 = 64 * 1024;

/// A truncated excerpt keeps this many bytes from the HEAD of the log and
/// spends the rest of [`STDOUT_PROMOTE_MAX_BYTES`] on the tail
/// (TASK-QGWK7.1.1 M-4). A tail-only excerpt keeps the wrong end for the case
/// that matters most — a harness that dies early and then emits retry noise
/// puts its evidence at the head. Year-one arithmetic is unchanged: the cap is
/// the same, only its split moved.
#[cfg(unix)]
const STDOUT_PROMOTE_HEAD_BYTES: u64 = STDOUT_PROMOTE_MAX_BYTES / 2;

/// First line of a truncated promoted `stdout.log`, so truncation is visible
/// IN the file rather than only by comparing it against `stdout.log.bytes`
/// (TASK-QGWK7.1.1 M-3).
#[cfg(unix)]
const STDOUT_TRUNCATION_BANNER: &str = "[orgasmic] stdout.log truncated by dispatch-close";

/// Outcome of promoting a validated attempt's artifacts (TASK-QGWK7.1).
///
/// `report_path` is set whenever `last.txt` landed at the canonical location,
/// even if `stdout.log` promotion failed afterward — a half-succeeded promote
/// must still name what it kept. `error` carries non-fatal promote problems
/// for the close tx's `CLEANUP_ERROR` channel; it never means the report was
/// destroyed (unlink runs only after every intended copy succeeds).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PromoteOutcome {
    pub report_path: Option<String>,
    pub error: Option<String>,
}

/// Validated dispatch attempt artifact pair for cleanup. Stores the opened
/// stem directory handle, no-follow artifact file handles, and relative names
/// so unlink targets the validated inode, not a replaced same-name entry
/// (TASK-KE0JW, TASK-1FV1N).
pub struct DispatchAttemptArtifacts {
    pub stem: String,
    pub attempt_id: Option<String>,
    /// Present when the worktree was validated. Promote-only paths (worktree
    /// already reclaimed) leave this `None` — identity checks require `Some`.
    worktree_handle: Option<std::fs::File>,
    #[cfg(unix)]
    stem_dir_handle: std::fs::File,
    #[cfg(unix)]
    last_name: String,
    #[cfg(unix)]
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
    let (stem_dir, stem) = validate_dispatch_stem_dir(project_root, last)?;
    validate_dispatch_artifact_pair(&stem_dir, &stem, last, stdout, Some(worktree_handle))
}

/// Validate the artifact pair for promotion when the worktree may already be
/// gone (TASK-QGWK7.1 F-4). Same stem/artifact fences as cleanup validation;
/// skips the worktree existence check so a close after reclaim can still
/// promote.
// orgasmic:TASK-QGWK7.1
pub fn validate_dispatch_promote_targets(
    project_root: &Path,
    last_path: Option<&Path>,
    stdout_path: Option<&Path>,
) -> Result<DispatchAttemptArtifacts, String> {
    let last = last_path.ok_or_else(|| "last_path required for dispatch promote".to_string())?;
    let stdout =
        stdout_path.ok_or_else(|| "stdout_path required for dispatch promote".to_string())?;
    let (stem_dir, stem) = validate_dispatch_stem_dir(project_root, last)?;
    validate_dispatch_artifact_pair(&stem_dir, &stem, last, stdout, None)
}

fn validate_dispatch_stem_dir(
    project_root: &Path,
    last: &Path,
) -> Result<(PathBuf, String), String> {
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
    Ok((stem_dir, stem))
}

/// Re-open the worktree without following symlinks and prove that the path
/// still names the directory retained by cleanup validation.
pub fn verify_dispatch_worktree_identity(
    artifacts: &DispatchAttemptArtifacts,
    worktree_path: &Path,
) -> Result<(), String> {
    let expected = artifacts.worktree_handle.as_ref().ok_or_else(|| {
        "worktree identity check requires a worktree-validated artifact set".to_string()
    })?;
    let current = open_dispatch_dir(worktree_path)?;
    same_file_identity(expected, &current)
        .then_some(())
        .ok_or_else(|| "worktree identity changed after cleanup validation".to_string())
}

/// After a dispatch worktree is removed, drop only the validated attempt's
/// transient artifacts while retaining the brief and sibling attempts.
///
/// Prefer [`promote_validated_dispatch_attempt`] on the manager close path
/// (TASK-QGWK7): a close must keep the report. This delete-only helper remains
/// for failed-dispatch rollback, where there is no durable report to keep.
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

/// Delete the validated tmp artifacts with no durable copy. Used by failed-
/// dispatch rollback only — see [`promote_validated_dispatch_attempt`].
pub fn prune_validated_dispatch_attempt(
    artifacts: &DispatchAttemptArtifacts,
) -> Result<(), String> {
    unlink_validated_attempt_pair(artifacts)
}

/// Move the validated attempt's `last.txt` and (bounded) `stdout.log` out of
/// gitignored `tmp/` into `.orgasmic/dispatch-records/<started_tx>/`, then
/// unlink the tmp copies when every intended copy succeeded.
///
/// `last.txt` is always promoted in full. `stdout.log` keeps falsifiability
/// without unbounded git growth (TASK-QGWK7.1): empty files promote no
/// `stdout.log`; larger files promote a [`STDOUT_PROMOTE_MAX_BYTES`] excerpt
/// (head + tail, with a banner naming the truncation). `stdout.log.bytes` is
/// written for EVERY promoted attempt, including the empty one, so "the
/// harness printed nothing" is distinguishable from "stdout was never
/// promoted" (TASK-QGWK7.1.1 M-3). Retention numbers live in the
/// manager-dispatch convention.
// orgasmic:TASK-QGWK7,TASK-QGWK7.1
pub fn promote_validated_dispatch_attempt(
    artifacts: &DispatchAttemptArtifacts,
    project_root: &Path,
    started_tx: &str,
) -> Result<PromoteOutcome, String> {
    let report_rel = dispatch_record_report_rel(started_tx)?;
    let dest_dir = dispatch_record_dir(project_root, started_tx)?;
    std::fs::create_dir_all(&dest_dir).map_err(|err| err.to_string())?;

    #[cfg(unix)]
    {
        let last_dest = dest_dir.join("last.txt");
        if let Err(err) = copy_validated_artifact_to(&last_dest, &artifacts.last_file) {
            return Ok(PromoteOutcome {
                report_path: None,
                error: Some(format!("promote last.txt: {err}")),
            });
        }

        let stdout_dest = dest_dir.join("stdout.log");
        let stdout_bytes_dest = dest_dir.join("stdout.log.bytes");
        match copy_validated_stdout_excerpt_to(
            &stdout_dest,
            &stdout_bytes_dest,
            &artifacts.stdout_file,
            STDOUT_PROMOTE_MAX_BYTES,
        ) {
            Ok(_) => {
                // Unlink tmp only after every intended copy succeeded so a
                // partial failure duplicates rather than loses.
                unlink_validated_attempt_pair(artifacts)?;
                Ok(PromoteOutcome {
                    report_path: Some(report_rel),
                    error: None,
                })
            }
            Err(err) => {
                // last.txt is at the canonical path; name it. Leave tmp intact
                // (no unlink) and scrub any mid-flight .promoting residue.
                Ok(PromoteOutcome {
                    report_path: Some(report_rel),
                    error: Some(format!("promote stdout.log: {err}")),
                })
            }
        }
    }
    #[cfg(not(unix))]
    {
        let _ = (artifacts, project_root, report_rel, dest_dir);
        Err("no-follow dispatch artifact promotion requires unix".into())
    }
}

fn unlink_validated_attempt_pair(artifacts: &DispatchAttemptArtifacts) -> Result<(), String> {
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
        Ok(())
    }
    #[cfg(not(unix))]
    {
        let _ = artifacts;
        Err("no-follow dispatch artifact deletion requires unix".into())
    }
}

/// Stream `source` to `dest` via a `.promoting` temp, without buffering the
/// whole artifact in RSS (TASK-QGWK7.1 F-7). Scrubs the temp on failure so a
/// half-flight promote leaves no tracked residue (F-8).
#[cfg(unix)]
fn copy_validated_artifact_to(dest: &Path, source: &std::fs::File) -> Result<(), String> {
    use std::io::Write;
    use std::os::unix::fs::FileExt;

    let tmp = dest.with_extension("promoting");
    let result = (|| {
        let mut out = std::fs::File::create(&tmp).map_err(|err| err.to_string())?;
        let mut offset = 0u64;
        loop {
            let mut chunk = [0u8; 64 * 1024];
            let n = source
                .read_at(&mut chunk, offset)
                .map_err(|err| err.to_string())?;
            if n == 0 {
                break;
            }
            out.write_all(&chunk[..n]).map_err(|err| err.to_string())?;
            offset += n as u64;
        }
        out.sync_all().map_err(|err| err.to_string())?;
        drop(out);
        std::fs::rename(&tmp, dest).map_err(|err| err.to_string())?;
        Ok(())
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(&tmp);
    }
    result
}

/// Promote a bounded excerpt of `stdout.log`. Returns the original byte count.
///
/// A 0-byte source promotes no `stdout.log` — but it still writes
/// `stdout.log.bytes` (`0`), so a reader can tell "the harness printed nothing"
/// from "stdout was never promoted" (TASK-QGWK7.1.1 M-3). Over the cap, the
/// excerpt is `banner + first half + elision marker + last half`
/// (TASK-QGWK7.1.1 M-4): the banner makes truncation visible in the file
/// itself, and keeping the head as well keeps the evidence of a harness that
/// died early and then printed retry noise.
///
/// The sidecar is renamed into place BEFORE `stdout.log`, so a failure between
/// the two renames can leave a byte count with no excerpt but never a
/// truncated excerpt with no byte count.
#[cfg(unix)]
fn copy_validated_stdout_excerpt_to(
    dest: &Path,
    bytes_sidecar: &Path,
    source: &std::fs::File,
    max_bytes: u64,
) -> Result<u64, String> {
    use std::io::Write;

    let original_len = source.metadata().map_err(|err| err.to_string())?.len();
    let tmp = dest.with_extension("promoting");
    let bytes_tmp = bytes_sidecar.with_extension("promoting");
    let result = (|| {
        {
            let mut bytes_out = std::fs::File::create(&bytes_tmp).map_err(|err| err.to_string())?;
            write!(bytes_out, "{original_len}").map_err(|err| err.to_string())?;
            bytes_out.sync_all().map_err(|err| err.to_string())?;
        }
        if original_len == 0 {
            // Empty tmux panes are the common case; do not track a useless
            // blob. The sidecar alone says the attempt was promoted.
            std::fs::rename(&bytes_tmp, bytes_sidecar).map_err(|err| err.to_string())?;
            return Ok(0);
        }

        let mut out = std::fs::File::create(&tmp).map_err(|err| err.to_string())?;
        if original_len > max_bytes {
            let head_len = STDOUT_PROMOTE_HEAD_BYTES.min(max_bytes);
            let tail_len = max_bytes - head_len;
            let elided = original_len - max_bytes;
            writeln!(
                out,
                "{STDOUT_TRUNCATION_BANNER}: {original_len} bytes total, kept the first \
                 {head_len} and the last {tail_len}."
            )
            .map_err(|err| err.to_string())?;
            copy_range(source, &mut out, 0, head_len)?;
            writeln!(
                out,
                "\n{STDOUT_TRUNCATION_BANNER}: {elided} bytes elided here."
            )
            .map_err(|err| err.to_string())?;
            copy_range(source, &mut out, original_len - tail_len, tail_len)?;
        } else {
            copy_range(source, &mut out, 0, original_len)?;
        }
        out.sync_all().map_err(|err| err.to_string())?;
        drop(out);
        std::fs::rename(&bytes_tmp, bytes_sidecar).map_err(|err| err.to_string())?;
        std::fs::rename(&tmp, dest).map_err(|err| err.to_string())?;
        Ok(original_len)
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(&tmp);
        let _ = std::fs::remove_file(&bytes_tmp);
    }
    result
}

/// Copy `len` bytes of `source` starting at `offset` into `out`, without
/// buffering the whole range in RSS (TASK-QGWK7.1 F-7).
#[cfg(unix)]
fn copy_range(
    source: &std::fs::File,
    out: &mut std::fs::File,
    offset: u64,
    len: u64,
) -> Result<(), String> {
    use std::io::Write;
    use std::os::unix::fs::FileExt;

    let mut offset = offset;
    let mut remaining = len;
    while remaining > 0 {
        let mut chunk = [0u8; 64 * 1024];
        let want = remaining.min(chunk.len() as u64) as usize;
        let n = source
            .read_at(&mut chunk[..want], offset)
            .map_err(|err| err.to_string())?;
        if n == 0 {
            break;
        }
        out.write_all(&chunk[..n]).map_err(|err| err.to_string())?;
        offset += n as u64;
        remaining -= n as u64;
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
    worktree_handle: Option<std::fs::File>,
) -> Result<DispatchAttemptArtifacts, String> {
    #[cfg(unix)]
    let stem_dir_handle = open_dispatch_dir(stem_dir)?;
    #[cfg(not(unix))]
    open_dispatch_dir(stem_dir)?;
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
        #[cfg(unix)]
        stem_dir_handle,
        #[cfg(unix)]
        last_name,
        #[cfg(unix)]
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
        assert!(collection_node_file_paths(root, "tasks")
            .unwrap()
            .is_empty());
        assert_eq!(
            task_node_file_path(root, "TASK-ABC"),
            PathBuf::from("/repo/.orgasmic/tasks/TASK-ABC/node.org")
        );
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

    // orgasmic:TASK-QGWK7,TASK-QGWK7.1
    #[test]
    fn promote_dispatch_attempt_keeps_report_under_started_tx() {
        let tmp = tempfile::tempdir().unwrap();
        let project_root = tmp.path().join("repo");
        let stem_dir = project_root.join(".orgasmic/tmp/dispatch/task-dispatch");
        std::fs::create_dir_all(stem_dir.join("worktree")).unwrap();
        let worktree = stem_dir.join("worktree");
        let last = stem_dir.join("task-dispatch-aaaa1111bbbb2222cccc3333dddd4444-last.txt");
        let stdout = stem_dir.join("task-dispatch-aaaa1111bbbb2222cccc3333dddd4444-stdout.log");
        std::fs::write(&last, "worker report survives close").unwrap();
        std::fs::write(&stdout, "harness stdout").unwrap();

        let artifacts =
            validate_dispatch_cleanup_targets(&project_root, &worktree, Some(&last), Some(&stdout))
                .unwrap();
        let started_tx = "tx-20260806-orgasmic-4916";
        let outcome =
            promote_validated_dispatch_attempt(&artifacts, &project_root, started_tx).unwrap();

        assert_eq!(
            outcome.report_path.as_deref(),
            Some(".orgasmic/dispatch-records/tx-20260806-orgasmic-4916/last.txt")
        );
        assert_eq!(outcome.error, None);
        let report_path = outcome.report_path.unwrap();
        let promoted = project_root.join(&report_path);
        assert!(
            promoted.exists(),
            "after close the report must still be readable from the path the tx names"
        );
        assert_eq!(
            std::fs::read_to_string(&promoted).unwrap(),
            "worker report survives close"
        );
        let record_dir = project_root.join(".orgasmic/dispatch-records/tx-20260806-orgasmic-4916");
        assert_eq!(
            std::fs::read_to_string(record_dir.join("stdout.log")).unwrap(),
            "harness stdout"
        );
        assert_eq!(
            std::fs::read_to_string(record_dir.join("stdout.log.bytes")).unwrap(),
            "14"
        );
        assert!(!last.exists(), "tmp last.txt must be moved, not copied");
        assert!(!stdout.exists(), "tmp stdout.log must be moved, not copied");
    }

    // orgasmic:TASK-QGWK7.1,TASK-QGWK7.1.1
    #[test]
    fn promote_skips_empty_stdout_and_bounds_tail_with_visible_byte_count() {
        let tmp = tempfile::tempdir().unwrap();
        let project_root = tmp.path().join("repo");
        let stem_dir = project_root.join(".orgasmic/tmp/dispatch/task-dispatch");
        std::fs::create_dir_all(stem_dir.join("worktree")).unwrap();
        let worktree = stem_dir.join("worktree");
        let last = stem_dir.join("task-dispatch-aaaa1111bbbb2222cccc3333dddd4444-last.txt");
        let stdout = stem_dir.join("task-dispatch-aaaa1111bbbb2222cccc3333dddd4444-stdout.log");
        std::fs::write(&last, "summary").unwrap();
        std::fs::write(&stdout, "").unwrap();

        let artifacts =
            validate_dispatch_cleanup_targets(&project_root, &worktree, Some(&last), Some(&stdout))
                .unwrap();
        let outcome =
            promote_validated_dispatch_attempt(&artifacts, &project_root, "tx-empty-stdout")
                .unwrap();
        assert_eq!(outcome.error, None);
        let record_dir = project_root.join(".orgasmic/dispatch-records/tx-empty-stdout");
        assert!(record_dir.join("last.txt").exists());
        assert!(
            !record_dir.join("stdout.log").exists(),
            "0-byte stdout.log must not be promoted"
        );
        // TASK-QGWK7.1.1 M-3: "the harness printed nothing" must be readable
        // off the record, not inferred from an absent file that also means
        // "stdout was never promoted".
        assert_eq!(
            std::fs::read_to_string(record_dir.join("stdout.log.bytes")).unwrap(),
            "0",
            "an empty promoted stdout must still be distinguishable from an unpromoted one"
        );

        // Fresh attempt for the bounded-tail case.
        let stem_dir = project_root.join(".orgasmic/tmp/dispatch/task-tail");
        std::fs::create_dir_all(stem_dir.join("worktree")).unwrap();
        let worktree = stem_dir.join("worktree");
        let last = stem_dir.join("task-tail-aaaa1111bbbb2222cccc3333dddd4444-last.txt");
        let stdout = stem_dir.join("task-tail-aaaa1111bbbb2222cccc3333dddd4444-stdout.log");
        std::fs::write(&last, "summary").unwrap();
        let original_len = STDOUT_PROMOTE_MAX_BYTES + 100;
        let mut body = vec![b'a'; original_len as usize];
        body[..4].copy_from_slice(b"HEAD");
        body[original_len as usize - 4..].copy_from_slice(b"TAIL");
        std::fs::write(&stdout, &body).unwrap();
        let artifacts =
            validate_dispatch_cleanup_targets(&project_root, &worktree, Some(&last), Some(&stdout))
                .unwrap();
        let outcome =
            promote_validated_dispatch_attempt(&artifacts, &project_root, "tx-tail-stdout")
                .unwrap();
        assert_eq!(outcome.error, None);
        let record_dir = project_root.join(".orgasmic/dispatch-records/tx-tail-stdout");
        let promoted = std::fs::read(record_dir.join("stdout.log")).unwrap();
        let text = String::from_utf8_lossy(&promoted);
        // TASK-QGWK7.1.1 M-3: truncation is stated IN the excerpt, not only
        // recoverable by comparing its length against the sidecar.
        assert!(
            text.starts_with(STDOUT_TRUNCATION_BANNER),
            "a truncated excerpt must say so on its first line: {:?}",
            &text[..text.len().min(120)]
        );
        assert!(
            text.contains("100 bytes elided here."),
            "the elision marker must name how much was dropped"
        );
        // TASK-QGWK7.1.1 M-4: both ends survive, so an early death keeps its
        // evidence instead of only the retry noise that followed it.
        assert!(promoted.starts_with(STDOUT_TRUNCATION_BANNER.as_bytes()));
        assert!(text.contains("HEAD"), "the head of the log must be kept");
        assert!(promoted.ends_with(b"TAIL"), "the tail must still be kept");
        // Excerpt content stays within the cap; only the two banner lines are
        // added on top, so the retention arithmetic is unchanged.
        let banner_bytes = promoted.len() as u64 - STDOUT_PROMOTE_MAX_BYTES;
        assert!(
            banner_bytes < 200,
            "banners must not meaningfully widen the 64 KB cap: {banner_bytes}"
        );
        assert_eq!(
            std::fs::read_to_string(record_dir.join("stdout.log.bytes")).unwrap(),
            original_len.to_string(),
            "truncation must be visible via the original byte count sidecar"
        );
    }

    // orgasmic:TASK-QGWK7.1
    #[test]
    fn half_succeeded_promote_names_last_txt_and_leaves_no_promoting_residue() {
        let tmp = tempfile::tempdir().unwrap();
        let project_root = tmp.path().join("repo");
        let stem_dir = project_root.join(".orgasmic/tmp/dispatch/task-dispatch");
        std::fs::create_dir_all(stem_dir.join("worktree")).unwrap();
        let worktree = stem_dir.join("worktree");
        let last = stem_dir.join("task-dispatch-aaaa1111bbbb2222cccc3333dddd4444-last.txt");
        let stdout = stem_dir.join("task-dispatch-aaaa1111bbbb2222cccc3333dddd4444-stdout.log");
        std::fs::write(&last, "kept report").unwrap();
        std::fs::write(&stdout, "harness").unwrap();

        let dest_dir = project_root.join(".orgasmic/dispatch-records/tx-half");
        std::fs::create_dir_all(&dest_dir).unwrap();
        // Block the stdout rename so last.txt lands and stdout fails.
        std::fs::create_dir(dest_dir.join("stdout.log")).unwrap();

        let artifacts =
            validate_dispatch_cleanup_targets(&project_root, &worktree, Some(&last), Some(&stdout))
                .unwrap();
        let outcome =
            promote_validated_dispatch_attempt(&artifacts, &project_root, "tx-half").unwrap();
        assert_eq!(
            outcome.report_path.as_deref(),
            Some(".orgasmic/dispatch-records/tx-half/last.txt")
        );
        assert!(
            outcome.error.as_deref().unwrap_or("").contains("stdout"),
            "stdout failure must be reported: {:?}",
            outcome.error
        );
        assert!(dest_dir.join("last.txt").exists());
        assert!(last.exists(), "tmp must remain when promote is partial");
        assert!(stdout.exists(), "tmp must remain when promote is partial");
        let residue: Vec<_> = std::fs::read_dir(&dest_dir)
            .unwrap()
            .filter_map(|e| e.ok())
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .filter(|name| name.contains("promoting"))
            .collect();
        assert!(
            residue.is_empty(),
            "half-succeeded promote must leave no .promoting files: {residue:?}"
        );
    }

    // orgasmic:TASK-QGWK7.1
    #[test]
    fn promote_targets_validate_without_worktree() {
        let tmp = tempfile::tempdir().unwrap();
        let project_root = tmp.path().join("repo");
        let stem_dir = project_root.join(".orgasmic/tmp/dispatch/task-dispatch");
        std::fs::create_dir_all(&stem_dir).unwrap();
        let last = stem_dir.join("task-dispatch-aaaa1111bbbb2222cccc3333dddd4444-last.txt");
        let stdout = stem_dir.join("task-dispatch-aaaa1111bbbb2222cccc3333dddd4444-stdout.log");
        std::fs::write(&last, "summary").unwrap();
        std::fs::write(&stdout, "out").unwrap();

        let artifacts =
            validate_dispatch_promote_targets(&project_root, Some(&last), Some(&stdout)).unwrap();
        let outcome =
            promote_validated_dispatch_attempt(&artifacts, &project_root, "tx-no-wt").unwrap();
        assert_eq!(outcome.error, None);
        assert_eq!(
            outcome.report_path.as_deref(),
            Some(".orgasmic/dispatch-records/tx-no-wt/last.txt")
        );
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
