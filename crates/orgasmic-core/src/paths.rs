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

/// Directory for one dispatch generation's promoted record.
pub fn dispatch_record_dir(
    project_root: &Path,
    task_id: &str,
    started_tx: &str,
) -> Result<PathBuf, String> {
    let task_id = sanitize_started_tx(task_id)?;
    Ok(task_node_file_path(project_root, task_id)
        .with_file_name("dispatches")
        .join(sanitize_started_tx(started_tx)?))
}

/// Repo-relative path recorded as `:REPORT_PATH:` on the close (and optionally
/// `*.reported`) tx. Always names `report.md` — the worker summary — not the
/// typed `evidence.json` or optional harness `stdout.log` beside it.
pub fn dispatch_record_report_rel(task_id: &str, started_tx: &str) -> Result<String, String> {
    let task_id = sanitize_started_tx(task_id)?;
    let started_tx = sanitize_started_tx(started_tx)?;
    Ok(format!(
        "{DOTORG}/tasks/{task_id}/dispatches/{started_tx}/report.md"
    ))
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
/// (TASK-QGWK7.1). Truncation is named inside the promoted excerpt.
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
/// in the file itself (TASK-QGWK7.1.1 M-3).
#[cfg(unix)]
const STDOUT_TRUNCATION_BANNER: &str = "[orgasmic] stdout.log truncated by dispatch-close";

/// Outcome of promoting a validated attempt's artifacts (TASK-QGWK7.1).
///
/// `report_path` is set whenever `last.txt` landed at the canonical location,
/// even if evidence or stdout promotion failed afterward — a half-succeeded
/// promote must still name what it kept. `error` carries non-fatal promote
/// problems for the close tx's `CLEANUP_ERROR` channel; it never means the
/// report was destroyed (unlink runs only after every intended copy succeeds).
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
    #[cfg(unix)]
    brief_name: Option<String>,
    #[cfg(unix)]
    compiled_prompt_name: Option<String>,
    #[cfg(unix)]
    brief_file: Option<std::fs::File>,
    #[cfg(unix)]
    compiled_prompt_file: Option<std::fs::File>,
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
    let mut artifacts =
        validate_dispatch_artifact_pair(&stem_dir, &stem, last, stdout, Some(worktree_handle))?;
    validate_dispatch_compiled_prompt(&mut artifacts, &stem_dir, last)?;
    Ok(artifacts)
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

/// Validate the complete tmp record copied by manager close. The brief path
/// comes from `manager.dispatch_started`; the compiled prompt follows the
/// dispatch stem grammar beside `last.txt` and `stdout.log`.
// orgasmic:TASK-W97C8.1
pub fn validate_dispatch_record_targets(
    project_root: &Path,
    worktree_path: Option<&Path>,
    brief_path: Option<&Path>,
    last_path: Option<&Path>,
    stdout_path: Option<&Path>,
) -> Result<DispatchAttemptArtifacts, String> {
    let mut artifacts = match worktree_path {
        Some(worktree) => {
            validate_dispatch_cleanup_targets(project_root, worktree, last_path, stdout_path)?
        }
        None => validate_dispatch_promote_targets(project_root, last_path, stdout_path)?,
    };
    let last = last_path.ok_or_else(|| "last_path required for dispatch promote".to_string())?;
    let stem_dir = canonicalize_path(
        last.parent()
            .ok_or_else(|| "last_path has no parent stem dir".to_string())?,
    )?;
    let brief_name = match brief_path {
        Some(brief) => match std::fs::symlink_metadata(brief) {
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => None,
            Err(err) => return Err(err.to_string()),
            Ok(_) => {
                let expected = dispatch_brief_name(&artifacts.stem, brief)?;
                validate_dispatch_sidecar_file(&stem_dir, brief, &expected)?
            }
        },
        None => None,
    };
    #[cfg(unix)]
    {
        if let Some(brief_name) = brief_name {
            match open_artifact_in_stem_dir(&artifacts.stem_dir_handle, &brief_name) {
                Ok(file) => {
                    artifacts.brief_name = Some(brief_name);
                    artifacts.brief_file = Some(file);
                }
                Err(err) if err.kind() == std::io::ErrorKind::NotFound => {}
                Err(err) => return Err(err.to_string()),
            }
        }
    }
    validate_dispatch_compiled_prompt(&mut artifacts, &stem_dir, last)?;
    Ok(artifacts)
}

/// Canonical tmp path for the compiled dispatch bundle beside an attempt.
// orgasmic:TASK-W97C8.1
pub fn dispatch_compiled_prompt_path(last_path: &Path) -> Result<PathBuf, String> {
    let parent = last_path
        .parent()
        .ok_or_else(|| "last_path has no parent stem dir".to_string())?;
    let file = last_path
        .file_name()
        .and_then(|name| name.to_str())
        .ok_or_else(|| "last_path has no filename".to_string())?;
    let prefix = file
        .strip_suffix("-last.txt")
        .ok_or_else(|| "last_path filename must end with -last.txt".to_string())?;
    Ok(parent.join(format!("{prefix}-compiled-prompt.md")))
}

fn dispatch_brief_name(stem: &str, brief: &Path) -> Result<String, String> {
    let name = brief
        .file_name()
        .and_then(|name| name.to_str())
        .ok_or_else(|| "brief path has no filename".to_string())?;
    let derived_stem = name
        .strip_suffix("-brief.md")
        .or_else(|| Path::new(name).file_stem().and_then(|value| value.to_str()))
        .ok_or_else(|| "brief path has no stem".to_string())?;
    if derived_stem != stem {
        return Err(format!(
            "brief {} does not match dispatch stem {stem}",
            brief.display()
        ));
    }
    Ok(name.to_string())
}

fn validate_dispatch_compiled_prompt(
    artifacts: &mut DispatchAttemptArtifacts,
    stem_dir: &Path,
    last: &Path,
) -> Result<(), String> {
    let compiled_prompt = dispatch_compiled_prompt_path(last)?;
    let expected = compiled_prompt
        .file_name()
        .and_then(|name| name.to_str())
        .ok_or_else(|| "compiled prompt path has no filename".to_string())?;
    let Some(name) = validate_dispatch_sidecar_file(stem_dir, &compiled_prompt, expected)? else {
        return Ok(());
    };
    #[cfg(unix)]
    match open_artifact_in_stem_dir(&artifacts.stem_dir_handle, &name) {
        Ok(file) => {
            artifacts.compiled_prompt_name = Some(name);
            artifacts.compiled_prompt_file = Some(file);
        }
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {}
        Err(err) => return Err(err.to_string()),
    }
    Ok(())
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
    unlink_validated_attempt_artifacts(artifacts)
}

/// Move the validated attempt's brief, compiled prompt, `last.txt`, typed
/// session evidence, and any non-empty bounded `stdout.log` out of gitignored `tmp/` into
/// `.orgasmic/tasks/TASK-X/dispatches/<started_tx>/`, then unlink the tmp copies
/// when every intended copy succeeded.
///
/// `last.txt` and `evidence.json` are always promoted in full. `stdout.log`
/// remains crash insurance without unbounded git growth (TASK-QGWK7.1): empty
/// files promote nothing; larger files promote a [`STDOUT_PROMOTE_MAX_BYTES`]
/// excerpt (head + tail, with a banner naming the original size and
/// truncation). Retention numbers live in the manager-dispatch convention.
// orgasmic:TASK-QGWK7,TASK-QGWK7.1,TASK-W97C8
pub fn promote_validated_dispatch_attempt(
    artifacts: &DispatchAttemptArtifacts,
    project_root: &Path,
    task_id: &str,
    started_tx: &str,
    evidence_json: &[u8],
) -> Result<PromoteOutcome, String> {
    let report_rel = dispatch_record_report_rel(task_id, started_tx)?;
    let dest_dir = dispatch_record_dir(project_root, task_id, started_tx)?;
    std::fs::create_dir_all(&dest_dir).map_err(|err| err.to_string())?;

    #[cfg(unix)]
    {
        let mut errors = Vec::new();
        if let Some(brief_file) = artifacts.brief_file.as_ref() {
            if let Err(err) = copy_validated_artifact_to(&dest_dir.join("brief.md"), brief_file) {
                return Ok(PromoteOutcome {
                    report_path: None,
                    error: Some(format!("promote brief.md: {err}")),
                });
            }
        } else {
            errors.push("brief.md missing from tmp".to_string());
        }
        if let Some(compiled_prompt_file) = artifacts.compiled_prompt_file.as_ref() {
            if let Err(err) = copy_validated_artifact_to(
                &dest_dir.join("compiled-prompt.md"),
                compiled_prompt_file,
            ) {
                return Ok(PromoteOutcome {
                    report_path: None,
                    error: Some(format!("promote compiled-prompt.md: {err}")),
                });
            }
        } else {
            errors.push("compiled-prompt.md missing from tmp".to_string());
        }

        let last_dest = dest_dir.join("report.md");
        if let Err(err) = copy_validated_artifact_to(&last_dest, &artifacts.last_file) {
            return Ok(PromoteOutcome {
                report_path: None,
                error: Some(format!("promote last.txt: {err}")),
            });
        }

        let evidence_dest = dest_dir.join("evidence.json");
        if let Err(err) = write_promoted_bytes_to(&evidence_dest, evidence_json) {
            errors.push(format!("promote evidence.json: {err}"));
            return Ok(PromoteOutcome {
                report_path: Some(report_rel),
                error: Some(errors.join("; ")),
            });
        }

        let stdout_dest = dest_dir.join("stdout.log");
        match copy_validated_stdout_excerpt_to(
            &stdout_dest,
            &artifacts.stdout_file,
            STDOUT_PROMOTE_MAX_BYTES,
        ) {
            Ok(_) => {
                // Unlink tmp only after every intended copy succeeded so a
                // partial failure duplicates rather than loses.
                unlink_validated_attempt_artifacts(artifacts)?;
                Ok(PromoteOutcome {
                    report_path: Some(report_rel),
                    error: (!errors.is_empty()).then(|| errors.join("; ")),
                })
            }
            Err(err) => {
                // last.txt is at the canonical path; name it. Leave tmp intact
                // (no unlink) and scrub any mid-flight .promoting residue.
                errors.push(format!("promote stdout.log: {err}"));
                Ok(PromoteOutcome {
                    report_path: Some(report_rel),
                    error: Some(errors.join("; ")),
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

fn unlink_validated_attempt_artifacts(artifacts: &DispatchAttemptArtifacts) -> Result<(), String> {
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
        if let (Some(name), Some(file)) = (&artifacts.brief_name, &artifacts.brief_file) {
            unlink_validated_artifact(&artifacts.stem_dir_handle, name, file)?;
        }
        if let (Some(name), Some(file)) = (
            &artifacts.compiled_prompt_name,
            &artifacts.compiled_prompt_file,
        ) {
            unlink_validated_artifact(&artifacts.stem_dir_handle, name, file)?;
        }
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

/// Atomically write generated evidence beside promoted artifacts.
#[cfg(unix)]
fn write_promoted_bytes_to(dest: &Path, bytes: &[u8]) -> Result<(), String> {
    use std::io::Write;

    let evidence: serde_json::Value = serde_json::from_slice(bytes)
        .map_err(|err| format!("dispatch evidence is not valid JSON: {err}"))?;
    let count = |field: &str| {
        evidence
            .get(field)
            .and_then(serde_json::Value::as_u64)
            .unwrap_or(0)
    };
    let has_failure_reason = count("unparsed_events") > 0
        || evidence
            .pointer("/session/reason")
            .and_then(serde_json::Value::as_str)
            .is_some_and(|reason| !reason.trim().is_empty());
    if count("event_count") == 0 && count("tool_call_count") == 0 && !has_failure_reason {
        return Err("refusing semantically empty dispatch evidence".into());
    }
    let tmp = dest.with_extension("promoting");
    let result = (|| {
        let mut out = std::fs::File::create(&tmp).map_err(|err| err.to_string())?;
        out.write_all(bytes).map_err(|err| err.to_string())?;
        out.sync_all().map_err(|err| err.to_string())?;
        drop(out);
        std::fs::rename(&tmp, dest).map_err(|err| err.to_string())
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(&tmp);
    }
    result
}

/// Promote a bounded excerpt of `stdout.log`. Returns the original byte count.
///
/// A 0-byte source promotes no `stdout.log`. Over the cap, the excerpt is
/// `banner + first half + elision marker + last half` (TASK-QGWK7.1.1 M-4):
/// the banner carries the original size and makes truncation visible in the
/// file itself, while keeping the head preserves evidence of a harness that
/// died early and then printed retry noise.
#[cfg(unix)]
fn copy_validated_stdout_excerpt_to(
    dest: &Path,
    source: &std::fs::File,
    max_bytes: u64,
) -> Result<u64, String> {
    use std::io::Write;

    let original_len = source.metadata().map_err(|err| err.to_string())?.len();
    let tmp = dest.with_extension("promoting");
    let result = (|| {
        if original_len == 0 {
            // Empty harness stdout is the common case; evidence.json says the
            // attempt was promoted, so retain no empty marker or stale excerpt.
            match std::fs::remove_file(dest) {
                Ok(()) => {}
                Err(err) if err.kind() == std::io::ErrorKind::NotFound => {}
                Err(err) => return Err(err.to_string()),
            }
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
        std::fs::rename(&tmp, dest).map_err(|err| err.to_string())?;
        Ok(original_len)
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(&tmp);
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
        let last_file = open_artifact_in_stem_dir(&stem_dir_handle, &last_name)
            .map_err(|err| err.to_string())?;
        let stdout_file = open_artifact_in_stem_dir(&stem_dir_handle, &stdout_name)
            .map_err(|err| err.to_string())?;
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
        #[cfg(unix)]
        brief_name: None,
        #[cfg(unix)]
        compiled_prompt_name: None,
        #[cfg(unix)]
        brief_file: None,
        #[cfg(unix)]
        compiled_prompt_file: None,
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
) -> std::io::Result<std::fs::File> {
    use std::ffi::CString;
    use std::os::unix::io::AsRawFd;

    if name.contains('/') || name.contains('\0') {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            format!("invalid artifact name {name}"),
        ));
    }
    let dir_fd = stem_dir.as_raw_fd();
    let name_c = CString::new(name).map_err(|_| {
        std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            format!("invalid artifact name {name}"),
        )
    })?;
    let file_fd = unsafe {
        libc::openat(
            dir_fd,
            name_c.as_ptr(),
            libc::O_RDONLY | libc::O_NOFOLLOW | libc::O_CLOEXEC,
        )
    };
    if file_fd < 0 {
        return Err(std::io::Error::last_os_error());
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

fn validate_dispatch_sidecar_file(
    stem_dir: &Path,
    artifact: &Path,
    expected_name: &str,
) -> Result<Option<String>, String> {
    let meta = match std::fs::symlink_metadata(artifact) {
        Ok(meta) => meta,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(err) => return Err(err.to_string()),
    };
    if artifact
        .components()
        .any(|component| matches!(component, std::path::Component::ParentDir))
    {
        return Err(format!("path contains ..: {}", artifact.display()));
    }
    let file_name = artifact
        .file_name()
        .and_then(|name| name.to_str())
        .ok_or_else(|| "artifact has no filename".to_string())?;
    if file_name != expected_name {
        return Err(format!(
            "artifact {} does not have expected sidecar name {expected_name}",
            artifact.display()
        ));
    }
    if meta.file_type().is_symlink() {
        return Err(format!("{} is a symlink", artifact.display()));
    }
    if !meta.is_file() {
        return Err(format!("{} is not a regular file", artifact.display()));
    }
    let parent = artifact
        .parent()
        .ok_or_else(|| format!("artifact {} has no parent", artifact.display()))?;
    if canonicalize_path(parent)? != stem_dir {
        return Err(format!(
            "artifact {} not directly under expected stem dir",
            artifact.display()
        ));
    }
    Ok(Some(file_name.to_string()))
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

    const EVIDENCE_JSON: &[u8] = br#"{"event_count":1,"tool_call_count":0}"#;

    fn write_dispatch_record_sidecars(
        stem_dir: &Path,
        stem: &str,
        last: &Path,
    ) -> (PathBuf, PathBuf) {
        let brief = stem_dir.join(format!("{stem}-brief.md"));
        let compiled_prompt = dispatch_compiled_prompt_path(last).unwrap();
        std::fs::write(&brief, "manager brief").unwrap();
        std::fs::write(&compiled_prompt, "compiled prompt").unwrap();
        (brief, compiled_prompt)
    }

    #[cfg(unix)]
    #[test]
    fn promoted_evidence_requires_work_or_a_named_failure() {
        let tmp = tempfile::tempdir().unwrap();
        let dest = tmp.path().join("evidence.json");

        let empty = br#"{"event_count":0,"tool_call_count":0,"unparsed_events":0,"session":{"status":"found"}}"#;
        assert!(write_promoted_bytes_to(&dest, empty)
            .unwrap_err()
            .contains("semantically empty"));
        let missing = br#"{"event_count":0,"tool_call_count":0,"unparsed_events":0,"session":{"status":"missing","reason":"session JSONL is missing"}}"#;
        write_promoted_bytes_to(&dest, missing).unwrap();
        let unparsed = br#"{"event_count":0,"tool_call_count":0,"unparsed_events":1,"session":{"status":"found"}}"#;
        write_promoted_bytes_to(&dest, unparsed).unwrap();
    }

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
        let attempt_a_compiled = dispatch_compiled_prompt_path(&attempt_a_last).unwrap();
        let attempt_b_last =
            stem_dir.join("task-dispatch-bbbb1111cccc2222dddd3333eeee4444-last.txt");
        let attempt_b_stdout =
            stem_dir.join("task-dispatch-bbbb1111cccc2222dddd3333eeee4444-stdout.log");
        let attempt_b_compiled = dispatch_compiled_prompt_path(&attempt_b_last).unwrap();
        let legacy_last = stem_dir.join("task-dispatch-last.txt");
        for path in [
            &attempt_a_last,
            &attempt_a_stdout,
            &attempt_a_compiled,
            &attempt_b_last,
            &attempt_b_stdout,
            &attempt_b_compiled,
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
        assert!(!attempt_a_compiled.exists());
        assert!(attempt_b_last.exists());
        assert!(attempt_b_stdout.exists());
        assert!(attempt_b_compiled.exists());
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
        let (brief, compiled_prompt) =
            write_dispatch_record_sidecars(&stem_dir, "task-dispatch", &last);
        std::fs::write(&last, "worker report survives close").unwrap();
        std::fs::write(&stdout, "harness stdout").unwrap();

        let artifacts = validate_dispatch_record_targets(
            &project_root,
            Some(&worktree),
            Some(&brief),
            Some(&last),
            Some(&stdout),
        )
        .unwrap();
        let started_tx = "tx-20260806-orgasmic-4916";
        let outcome = promote_validated_dispatch_attempt(
            &artifacts,
            &project_root,
            "TASK-X",
            started_tx,
            EVIDENCE_JSON,
        )
        .unwrap();

        assert_eq!(
            outcome.report_path.as_deref(),
            Some(".orgasmic/tasks/TASK-X/dispatches/tx-20260806-orgasmic-4916/report.md")
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
        let record_dir =
            project_root.join(".orgasmic/tasks/TASK-X/dispatches/tx-20260806-orgasmic-4916");
        assert_eq!(
            std::fs::read_to_string(record_dir.join("stdout.log")).unwrap(),
            "harness stdout"
        );
        assert_eq!(
            std::fs::read(record_dir.join("evidence.json")).unwrap(),
            EVIDENCE_JSON
        );
        assert_eq!(
            std::fs::read_to_string(record_dir.join("brief.md")).unwrap(),
            "manager brief"
        );
        assert_eq!(
            std::fs::read_to_string(record_dir.join("compiled-prompt.md")).unwrap(),
            "compiled prompt"
        );
        assert!(!record_dir.join("stdout.log.bytes").exists());
        assert!(!brief.exists(), "tmp brief must be moved, not copied");
        assert!(
            !compiled_prompt.exists(),
            "tmp compiled prompt must be moved, not copied"
        );
        assert!(!last.exists(), "tmp last.txt must be moved, not copied");
        assert!(!stdout.exists(), "tmp stdout.log must be moved, not copied");
    }

    // orgasmic:TASK-QGWK7.1,TASK-QGWK7.1.1
    #[test]
    fn promote_skips_empty_stdout_and_bounds_tail_with_visible_banner() {
        let tmp = tempfile::tempdir().unwrap();
        let project_root = tmp.path().join("repo");
        let stem_dir = project_root.join(".orgasmic/tmp/dispatch/task-dispatch");
        std::fs::create_dir_all(stem_dir.join("worktree")).unwrap();
        let worktree = stem_dir.join("worktree");
        let last = stem_dir.join("task-dispatch-aaaa1111bbbb2222cccc3333dddd4444-last.txt");
        let stdout = stem_dir.join("task-dispatch-aaaa1111bbbb2222cccc3333dddd4444-stdout.log");
        let (brief, _) = write_dispatch_record_sidecars(&stem_dir, "task-dispatch", &last);
        std::fs::write(&last, "summary").unwrap();
        std::fs::write(&stdout, "").unwrap();

        let artifacts = validate_dispatch_record_targets(
            &project_root,
            Some(&worktree),
            Some(&brief),
            Some(&last),
            Some(&stdout),
        )
        .unwrap();
        let outcome = promote_validated_dispatch_attempt(
            &artifacts,
            &project_root,
            "TASK-X",
            "tx-empty-stdout",
            EVIDENCE_JSON,
        )
        .unwrap();
        assert_eq!(outcome.error, None);
        let record_dir = project_root.join(".orgasmic/tasks/TASK-X/dispatches/tx-empty-stdout");
        assert!(record_dir.join("report.md").exists());
        assert!(
            !record_dir.join("stdout.log").exists(),
            "0-byte stdout.log must not be promoted"
        );
        assert!(record_dir.join("evidence.json").exists());
        assert!(!record_dir.join("stdout.log.bytes").exists());

        // Fresh attempt for the bounded-tail case.
        let stem_dir = project_root.join(".orgasmic/tmp/dispatch/task-tail");
        std::fs::create_dir_all(stem_dir.join("worktree")).unwrap();
        let worktree = stem_dir.join("worktree");
        let last = stem_dir.join("task-tail-aaaa1111bbbb2222cccc3333dddd4444-last.txt");
        let stdout = stem_dir.join("task-tail-aaaa1111bbbb2222cccc3333dddd4444-stdout.log");
        let (brief, _) = write_dispatch_record_sidecars(&stem_dir, "task-tail", &last);
        std::fs::write(&last, "summary").unwrap();
        let original_len = STDOUT_PROMOTE_MAX_BYTES + 100;
        let mut body = vec![b'a'; original_len as usize];
        body[..4].copy_from_slice(b"HEAD");
        body[original_len as usize - 4..].copy_from_slice(b"TAIL");
        std::fs::write(&stdout, &body).unwrap();
        let artifacts = validate_dispatch_record_targets(
            &project_root,
            Some(&worktree),
            Some(&brief),
            Some(&last),
            Some(&stdout),
        )
        .unwrap();
        let outcome = promote_validated_dispatch_attempt(
            &artifacts,
            &project_root,
            "TASK-X",
            "tx-tail-stdout",
            EVIDENCE_JSON,
        )
        .unwrap();
        assert_eq!(outcome.error, None);
        let record_dir = project_root.join(".orgasmic/tasks/TASK-X/dispatches/tx-tail-stdout");
        let promoted = std::fs::read(record_dir.join("stdout.log")).unwrap();
        let text = String::from_utf8_lossy(&promoted);
        // TASK-QGWK7.1.1 M-3: truncation is stated in the excerpt itself.
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
        assert!(!record_dir.join("stdout.log.bytes").exists());
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
        let (brief, compiled_prompt) =
            write_dispatch_record_sidecars(&stem_dir, "task-dispatch", &last);
        std::fs::write(&last, "kept report").unwrap();
        std::fs::write(&stdout, "harness").unwrap();

        let dest_dir = project_root.join(".orgasmic/tasks/TASK-X/dispatches/tx-half");
        std::fs::create_dir_all(&dest_dir).unwrap();
        // Block the evidence rename so report.md lands and evidence fails.
        std::fs::create_dir(dest_dir.join("evidence.json")).unwrap();

        let artifacts = validate_dispatch_record_targets(
            &project_root,
            Some(&worktree),
            Some(&brief),
            Some(&last),
            Some(&stdout),
        )
        .unwrap();
        let outcome = promote_validated_dispatch_attempt(
            &artifacts,
            &project_root,
            "TASK-X",
            "tx-half",
            EVIDENCE_JSON,
        )
        .unwrap();
        assert_eq!(
            outcome.report_path.as_deref(),
            Some(".orgasmic/tasks/TASK-X/dispatches/tx-half/report.md")
        );
        assert!(
            outcome
                .error
                .as_deref()
                .unwrap_or("")
                .contains("evidence.json"),
            "evidence failure must be reported: {:?}",
            outcome.error
        );
        assert!(dest_dir.join("report.md").exists());
        assert!(
            brief.exists(),
            "tmp brief must remain when promote is partial"
        );
        assert!(
            compiled_prompt.exists(),
            "tmp compiled prompt must remain when promote is partial"
        );
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
        let (brief, _) = write_dispatch_record_sidecars(&stem_dir, "task-dispatch", &last);
        std::fs::write(&last, "summary").unwrap();
        std::fs::write(&stdout, "out").unwrap();

        let artifacts = validate_dispatch_record_targets(
            &project_root,
            None,
            Some(&brief),
            Some(&last),
            Some(&stdout),
        )
        .unwrap();
        let outcome = promote_validated_dispatch_attempt(
            &artifacts,
            &project_root,
            "TASK-X",
            "tx-no-wt",
            EVIDENCE_JSON,
        )
        .unwrap();
        assert_eq!(outcome.error, None);
        assert_eq!(
            outcome.report_path.as_deref(),
            Some(".orgasmic/tasks/TASK-X/dispatches/tx-no-wt/report.md")
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

    #[cfg(unix)]
    #[test]
    fn validate_dispatch_record_rejects_wrong_or_symlinked_brief_sidecar() {
        let tmp = tempfile::tempdir().unwrap();
        let project_root = tmp.path().join("repo");
        let stem_dir = project_root.join(".orgasmic/tmp/dispatch/task-dispatch");
        let worktree = stem_dir.join("worktree");
        std::fs::create_dir_all(&worktree).unwrap();
        let last = stem_dir.join("task-dispatch-aaaa1111bbbb2222cccc3333dddd4444-last.txt");
        let stdout =
            stem_dir.join("task-dispatch-aaaa1111bbbb2222cccc3333dddd4444-stdout.log");
        let sibling_last =
            stem_dir.join("task-dispatch-bbbb1111cccc2222dddd3333eeee4444-last.txt");
        for path in [&last, &stdout, &sibling_last] {
            std::fs::write(path, "artifact").unwrap();
        }
        let compiled_prompt = dispatch_compiled_prompt_path(&last).unwrap();
        std::fs::write(&compiled_prompt, "bundle").unwrap();

        assert!(
            validate_dispatch_record_targets(
                &project_root,
                Some(&worktree),
                Some(&sibling_last),
                Some(&last),
                Some(&stdout),
            )
            .err()
            .unwrap()
            .contains("does not match dispatch stem")
        );

        let brief = stem_dir.join("task-dispatch-brief.md");
        let victim = stem_dir.join("victim.md");
        std::fs::write(&victim, "victim").unwrap();
        std::os::unix::fs::symlink(&victim, &brief).unwrap();
        assert!(
            validate_dispatch_record_targets(
                &project_root,
                Some(&worktree),
                Some(&brief),
                Some(&last),
                Some(&stdout),
            )
            .err()
            .unwrap()
            .contains("is a symlink")
        );
        assert_eq!(std::fs::read_to_string(&victim).unwrap(), "victim");
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
