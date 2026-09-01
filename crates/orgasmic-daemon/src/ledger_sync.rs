//! Git transport for the hidden ledger worktree.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::{bail, Context, Result};
use chrono::{DateTime, Utc};
use serde::Serialize;

use crate::index::Index;
use crate::writer::{TxAppend, TxIdPolicy, WriterHandle};

const SYNC_INTERVAL: Duration = Duration::from_secs(2);
const MAX_BACKOFF: Duration = Duration::from_secs(5 * 60);
const PUSH_ATTEMPTS: usize = 5;

#[derive(Debug, Clone, Serialize)]
pub(crate) struct LedgerSyncStatus {
    pub outcome: &'static str,
    pub error: Option<String>,
    pub consecutive_failures: u32,
    pub last_attempt_at: Option<DateTime<Utc>>,
    pub last_success_at: Option<DateTime<Utc>>,
    pub next_attempt_at: Option<DateTime<Utc>>,
}

impl Default for LedgerSyncStatus {
    fn default() -> Self {
        Self {
            outcome: "idle",
            error: None,
            consecutive_failures: 0,
            last_attempt_at: None,
            last_success_at: None,
            next_attempt_at: None,
        }
    }
}

pub(crate) type LedgerSyncStatuses = Arc<Mutex<BTreeMap<PathBuf, LedgerSyncStatus>>>;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum SyncOutcome {
    Idle,
    Synced {
        push_retries: usize,
    },
    Conflict {
        parked_ref: String,
        paths: Vec<String>,
        local_head: String,
        remote_head: String,
    },
}

/// Commit, rebase, and push one hidden `orgasmic` ledger worktree.
///
/// `Idle` includes the normal single-machine cases: an ordinary project
/// checkout or no `origin`.
pub(crate) fn sync_once(ledger: &Path, machine_id: &str) -> Result<SyncOutcome> {
    sync_once_inner(ledger, machine_id, |_| {})
}

fn sync_once_inner(
    ledger: &Path,
    machine_id: &str,
    mut before_push: impl FnMut(usize),
) -> Result<SyncOutcome> {
    uuid::Uuid::parse_str(machine_id).context("machine-id is not a UUID")?;
    if git_optional(ledger, &["symbolic-ref", "--short", "HEAD"])?.as_deref() != Some("orgasmic")
        || git_optional(ledger, &["remote", "get-url", "origin"])?.is_none()
    {
        return Ok(SyncOutcome::Idle);
    }

    let dotorg = ledger.join(".orgasmic");
    if dotorg.exists() {
        let ignore = dotorg.join(".gitignore");
        let mut ignored = match std::fs::read(&ignore) {
            Ok(ignored) => ignored,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Vec::new(),
            Err(error) => return Err(error).context("read .orgasmic/.gitignore"),
        };
        if !ignored
            .split(|byte| *byte == b'\n')
            .any(|line| line.strip_suffix(b"\r").unwrap_or(line) == b"views/")
        {
            if !ignored.is_empty() && !ignored.ends_with(b"\n") {
                ignored.push(b'\n');
            }
            ignored.extend_from_slice(b"views/\n");
            std::fs::write(&ignore, ignored).context("write .orgasmic/.gitignore")?;
        }
        git(
            ledger,
            &[
                "rm",
                "-r",
                "-q",
                "--cached",
                "--ignore-unmatch",
                "--",
                ".orgasmic/views",
            ],
        )?;

        // Stage everything this machine changed inside the ledger.
        //
        // Staging only the node dirs it holds a claim on *right now* dropped every
        // edit whose claim had already been released: a dispatch releases its claim
        // milliseconds after writing the node, and the next tick is seconds later,
        // so the node write was never committed and never reached another machine.
        // It also never staged the files that are not claim-gated at all — the
        // singleton `project.org`, `tasks/goal.org`, `tasks/handoff.org`, and
        // `gotchas.org` — which were left as permanent uncommitted changes for
        // `--autostash` to churn on every tick.
        //
        // Writes are free between dispatch claims. If two machines edit the same
        // path, the pull conflict parks this machine's side before following remote.
        //
        // ponytail: excluding writer sidecars still lets a tick commit node rewrites
        // before their close tx lands; a peer can see that torn state for one sync
        // interval. Add a writer-published quiescence barrier or ledger-wide lease if
        // that bounded window becomes unacceptable.
    }
    stage_ledger(ledger, machine_id)?;
    commit_staged(ledger, &format!("ledger: sync {machine_id}"))?;

    let has_remote_branch = git_success(
        ledger,
        &["ls-remote", "--exit-code", "--heads", "origin", "orgasmic"],
    )?;
    for retry in 0..PUSH_ATTEMPTS {
        if has_remote_branch || retry > 0 {
            let pull = git_output(
                ledger,
                &["pull", "--rebase", "--autostash", "origin", "orgasmic"],
            )?;
            if !pull.status.success() {
                let paths = conflict_paths(&pull);
                if !paths.is_empty() {
                    git(ledger, &["rebase", "--abort"])?;
                    return park_conflict(ledger, machine_id, paths);
                }
                let _ = git_output(ledger, &["rebase", "--abort"]);
                bail!("git pull --rebase failed: {}", output_message(&pull));
            }
        }
        before_push(retry);
        let push = git_output(ledger, &["push", "origin", "HEAD:orgasmic"])?;
        if push.status.success() {
            return Ok(SyncOutcome::Synced {
                push_retries: retry,
            });
        }
        if retry + 1 == PUSH_ATTEMPTS {
            bail!(
                "git push still failed after {PUSH_ATTEMPTS} attempts: {}",
                output_message(&push)
            );
        }
    }
    unreachable!()
}

fn stage_ledger(ledger: &Path, machine_id: &str) -> Result<()> {
    if ledger.join(".orgasmic").exists() {
        git(
            ledger,
            &[
                "add",
                "--all",
                "--",
                ".orgasmic",
                ":(exclude).orgasmic/machines",
                ":(exclude,glob).orgasmic/**/*.tmp",
                ":(exclude,glob).orgasmic/**/*.tmp.*",
                ":(exclude,glob).orgasmic/**/*.bak.*",
            ],
        )?;
    }
    let machine_rel = PathBuf::from(".orgasmic/machines").join(machine_id);
    if ledger.join(&machine_rel).exists() {
        let machine_rel = path_arg(&machine_rel)?;
        let tmp = format!(":(exclude,glob){machine_rel}/**/*.tmp");
        let tmp_request = format!(":(exclude,glob){machine_rel}/**/*.tmp.*");
        let backup = format!(":(exclude,glob){machine_rel}/**/*.bak.*");
        git(
            ledger,
            &[
                "add",
                "--all",
                "--",
                machine_rel,
                &tmp,
                &tmp_request,
                &backup,
            ],
        )?;
    }
    Ok(())
}

fn commit_staged(ledger: &Path, message: &str) -> Result<()> {
    if !git_success(ledger, &["diff", "--cached", "--quiet"])? {
        git(
            ledger,
            &[
                "-c",
                "user.name=orgasmic daemon",
                "-c",
                "user.email=daemon@orgasmic.local",
                "commit",
                "-m",
                message,
            ],
        )?;
    }
    Ok(())
}

fn conflict_paths(output: &Output) -> Vec<String> {
    let text = format!(
        "{}\n{}",
        String::from_utf8_lossy(&output.stderr),
        String::from_utf8_lossy(&output.stdout)
    );
    let mut paths = Vec::new();
    for line in text.lines().map(str::trim) {
        if line.starts_with("CONFLICT (") {
            if let Some((_, path)) = line.rsplit_once(" in ") {
                if !paths.iter().any(|known| known == path) {
                    paths.push(path.to_string());
                }
            }
        }
    }
    paths
}

fn park_conflict(ledger: &Path, machine_id: &str, paths: Vec<String>) -> Result<SyncOutcome> {
    stage_ledger(ledger, machine_id)?;
    commit_staged(ledger, &format!("ledger: conflict salvage {machine_id}"))?;
    let local_head = git_optional(ledger, &["rev-parse", "HEAD"])?
        .context("ledger conflict has no local HEAD")?;
    let base = format!(
        "refs/orgasmic/conflicts/{machine_id}/{}",
        Utc::now().format("%Y%m%dT%H%M%SZ")
    );
    let mut parked_ref = base.clone();
    for suffix in 2.. {
        if !git_success(ledger, &["show-ref", "--verify", "--quiet", &parked_ref])? {
            break;
        }
        parked_ref = format!("{base}-{suffix}");
    }
    git(ledger, &["update-ref", &parked_ref, "HEAD"])?;
    match git_output(
        ledger,
        &["push", "origin", &format!("{parked_ref}:{parked_ref}")],
    ) {
        Ok(push) if push.status.success() => {}
        Ok(push) => tracing::warn!(
            parked_ref,
            error = %output_message(&push),
            "push parked ledger conflict ref failed"
        ),
        Err(error) => tracing::warn!(parked_ref, %error, "push parked ledger conflict ref failed"),
    }
    git(ledger, &["fetch", "origin", "orgasmic"])?;
    let remote_head = git_optional(ledger, &["rev-parse", "origin/orgasmic"])?
        .context("fetched ledger conflict has no origin/orgasmic")?;
    git(ledger, &["reset", "--hard", "origin/orgasmic"])?;
    Ok(SyncOutcome::Conflict {
        parked_ref,
        paths,
        local_head,
        remote_head,
    })
}

fn sync_ledger_at(
    ledger: &Path,
    machine_id: &str,
    statuses: &LedgerSyncStatuses,
    now: DateTime<Utc>,
) -> Option<SyncOutcome> {
    let previous = {
        let mut statuses = statuses.lock().expect("ledger sync status lock");
        let status = statuses.entry(ledger.to_path_buf()).or_default();
        if status
            .next_attempt_at
            .as_ref()
            .is_some_and(|next| now < *next)
        {
            status.outcome = "backed_off";
            return None;
        }
        status.clone()
    };

    match sync_once(ledger, machine_id) {
        Ok(outcome) => {
            let recovered = previous.consecutive_failures > 0;
            let (status_outcome, error) = match &outcome {
                SyncOutcome::Idle => ("idle", None),
                SyncOutcome::Synced { .. } => ("synced", None),
                SyncOutcome::Conflict {
                    parked_ref, paths, ..
                } => (
                    "conflict",
                    Some(format!(
                        "{} conflicting paths parked at {parked_ref}: {}",
                        paths.len(),
                        paths.join(" ")
                    )),
                ),
            };
            let mut statuses = statuses.lock().expect("ledger sync status lock");
            statuses.insert(
                ledger.to_path_buf(),
                LedgerSyncStatus {
                    outcome: status_outcome,
                    error: error.clone(),
                    consecutive_failures: 0,
                    last_attempt_at: Some(now),
                    last_success_at: Some(now),
                    next_attempt_at: None,
                },
            );
            drop(statuses);
            if recovered {
                tracing::info!(ledger = %ledger.display(), "ledger sync recovered");
            }
            if let Some(error) = error {
                tracing::warn!(ledger = %ledger.display(), %error, "ledger sync conflict parked");
            }
            matches!(outcome, SyncOutcome::Conflict { .. }).then_some(outcome)
        }
        Err(error) => {
            let error = format!("{error:#}");
            let consecutive_failures = previous.consecutive_failures.saturating_add(1);
            let multiplier = 1_u32 << consecutive_failures.min(8);
            let backoff = SYNC_INTERVAL.saturating_mul(multiplier).min(MAX_BACKOFF);
            let changed = previous.consecutive_failures == 0
                || previous.error.as_deref() != Some(error.as_str());
            statuses.lock().expect("ledger sync status lock").insert(
                ledger.to_path_buf(),
                LedgerSyncStatus {
                    outcome: "failed",
                    error: Some(error.clone()),
                    consecutive_failures,
                    last_attempt_at: Some(now),
                    last_success_at: previous.last_success_at,
                    next_attempt_at: Some(now + backoff),
                },
            );
            if changed {
                tracing::warn!(ledger = %ledger.display(), %error, "ledger sync failed; backing off");
            }
            None
        }
    }
}

async fn record_sync_conflict(
    writer: &WriterHandle,
    ledger: &Path,
    project_id: &str,
    machine_id: &str,
    now: DateTime<Utc>,
    outcome: &SyncOutcome,
) -> Result<()> {
    let SyncOutcome::Conflict {
        parked_ref,
        paths,
        local_head,
        remote_head,
    } = outcome
    else {
        return Ok(());
    };
    let mut entry = orgasmic_core::tx::TxEntry::new(
        "pending-project-sequence",
        "ledger.sync_conflict",
        now.format("[%Y-%m-%d %a %H:%M:%S]").to_string(),
        "agent.daemon",
        machine_id,
    );
    entry.project = Some(project_id.to_string());
    entry.extra = vec![
        ("EVENT_ID".into(), uuid::Uuid::new_v4().to_string()),
        ("PARKED_REF".into(), parked_ref.clone()),
        ("PATHS".into(), paths.join(" ")),
        ("LOCAL_HEAD".into(), local_head.clone()),
        ("REMOTE_HEAD".into(), remote_head.clone()),
    ];
    let request_id = format!("ledger-sync-conflict:{parked_ref}");
    writer
        .append_tx(
            TxAppend {
                tx_path: ledger
                    .join(".orgasmic/machines")
                    .join(machine_id)
                    .join(format!("{}.org", now.format("%Y-%m"))),
                entry,
                project_id: Some(project_id.to_string()),
                tx_id_policy: TxIdPolicy::ProjectSequence {
                    project_id: project_id.to_string(),
                    date: now.format("%Y%m%d").to_string(),
                },
                request_id: Some(request_id.clone()),
            },
            Some(request_id),
        )
        .await?;
    Ok(())
}

/// The daemon's coalescing loop. A missing remote is deliberately silent.
pub(crate) fn spawn(
    index: Index,
    machine_id: String,
    statuses: LedgerSyncStatuses,
    writer: WriterHandle,
    mut shutdown: tokio::sync::watch::Receiver<bool>,
) {
    tokio::spawn(async move {
        let mut interval = tokio::time::interval(SYNC_INTERVAL);
        interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        loop {
            tokio::select! {
                _ = interval.tick() => {}
                changed = shutdown.changed() => {
                    if changed.is_err() || *shutdown.borrow() {
                        return;
                    }
                    continue;
                }
            }
            let ledgers: std::collections::BTreeSet<_> = index
                .snapshot()
                .await
                .board
                .into_iter()
                .map(|entry| (entry.path, entry.id))
                .collect();
            for (ledger, project_id) in ledgers {
                let sync_machine_id = machine_id.clone();
                let statuses = statuses.clone();
                let sync_ledger = ledger.clone();
                let result = tokio::task::spawn_blocking(move || {
                    sync_ledger_at(&sync_ledger, &sync_machine_id, &statuses, Utc::now())
                })
                .await;
                match result {
                    Ok(Some(conflict)) => {
                        if let Err(error) = record_sync_conflict(
                            &writer,
                            &ledger,
                            &project_id,
                            &machine_id,
                            Utc::now(),
                            &conflict,
                        )
                        .await
                        {
                            tracing::warn!(
                                ledger = %ledger.display(),
                                %error,
                                "record ledger sync conflict event failed"
                            );
                        }
                    }
                    Ok(None) => {}
                    Err(error) => {
                        tracing::warn!(%error, "ledger sync task failed; will retry");
                    }
                }
            }
        }
    });
}

fn path_arg(path: &Path) -> Result<&str> {
    path.to_str().context("ledger path is not UTF-8")
}

fn git_optional(cwd: &Path, args: &[&str]) -> Result<Option<String>> {
    let output = git_output(cwd, args)?;
    Ok(output
        .status
        .success()
        .then(|| String::from_utf8_lossy(&output.stdout).trim().to_string()))
}

fn git_success(cwd: &Path, args: &[&str]) -> Result<bool> {
    Ok(git_output(cwd, args)?.status.success())
}

fn git(cwd: &Path, args: &[&str]) -> Result<()> {
    let output = git_output(cwd, args)?;
    if output.status.success() {
        Ok(())
    } else {
        bail!("git {} failed: {}", args.join(" "), output_message(&output))
    }
}

fn git_output(cwd: &Path, args: &[&str]) -> Result<Output> {
    Command::new("git")
        .args(args)
        .env("LC_ALL", "C")
        .current_dir(cwd)
        .output()
        .with_context(|| format!("run git {} in {}", args.join(" "), cwd.display()))
}

fn output_message(output: &Output) -> String {
    let stderr = String::from_utf8_lossy(&output.stderr);
    let stdout = String::from_utf8_lossy(&output.stdout);
    stderr.trim().to_string() + stdout.trim()
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Barrier};

    use super::*;

    fn run(cwd: &Path, args: &[&str]) {
        let output = git_output(cwd, args).unwrap();
        assert!(output.status.success(), "{}", output_message(&output));
    }

    fn seed_remote(tmp: &tempfile::TempDir) -> (PathBuf, PathBuf, PathBuf) {
        let remote = tmp.path().join("remote.git");
        run(tmp.path(), &["init", "--bare", path_arg(&remote).unwrap()]);
        let seed = tmp.path().join("seed");
        run(
            tmp.path(),
            &["init", "-b", "orgasmic", path_arg(&seed).unwrap()],
        );
        std::fs::create_dir_all(seed.join(".orgasmic")).unwrap();
        std::fs::write(seed.join(".orgasmic/.keep"), "ledger\n").unwrap();
        run(&seed, &["add", ".orgasmic/.keep"]);
        run(
            &seed,
            &[
                "-c",
                "user.name=test",
                "-c",
                "user.email=test@example.com",
                "commit",
                "-m",
                "seed",
            ],
        );
        run(
            &seed,
            &["remote", "add", "origin", path_arg(&remote).unwrap()],
        );
        run(&seed, &["push", "-u", "origin", "orgasmic"]);
        let a = tmp.path().join("a");
        let b = tmp.path().join("b");
        for clone in [&a, &b] {
            run(
                tmp.path(),
                &[
                    "clone",
                    "--branch",
                    "orgasmic",
                    path_arg(&remote).unwrap(),
                    path_arg(clone).unwrap(),
                ],
            );
        }
        (remote, a, b)
    }

    fn local_commit(repo: &Path, machine_id: &str) {
        let path = repo
            .join(".orgasmic/machines")
            .join(machine_id)
            .join("tx/2026-08.org");
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, format!("event from {machine_id}\n")).unwrap();
        run(repo, &["add", ".orgasmic"]);
        run(
            repo,
            &[
                "-c",
                "user.name=test",
                "-c",
                "user.email=test@example.com",
                "commit",
                "-m",
                machine_id,
            ],
        );
    }

    fn daemon_home(root: &Path, ledger: &Path, project_id: &str) -> orgasmic_core::Home {
        let home = orgasmic_core::Home::at(root);
        home.ensure().unwrap();
        std::fs::write(
            home.board(),
            format!(
                "#+title: board\n#+orgasmic_version: 1\n\n* PROJECT {project_id}\n:PROPERTIES:\n:ID: {project_id}\n:PATH: {}\n:BRANCH: orgasmic\n:STATUS: active\n:END:\n",
                ledger.display()
            ),
        )
        .unwrap();
        home
    }

    #[test]
    fn concurrent_machine_writes_rebase_and_retry_the_push_race() {
        let tmp = tempfile::tempdir().unwrap();
        let (_remote, a, b) = seed_remote(&tmp);
        let a_id = uuid::Uuid::new_v4().to_string();
        let b_id = uuid::Uuid::new_v4().to_string();
        local_commit(&a, &a_id);
        local_commit(&b, &b_id);

        let barrier = Arc::new(Barrier::new(2));
        let workers: Vec<_> = [(a.clone(), a_id.clone()), (b.clone(), b_id.clone())]
            .into_iter()
            .map(|(repo, id)| {
                let barrier = barrier.clone();
                std::thread::spawn(move || {
                    sync_once_inner(&repo, &id, |attempt| {
                        if attempt == 0 {
                            barrier.wait();
                        }
                    })
                    .unwrap()
                })
            })
            .collect();
        let outcomes: Vec<_> = workers
            .into_iter()
            .map(|worker| worker.join().unwrap())
            .collect();
        assert!(outcomes.iter().any(|outcome| matches!(
            outcome,
            SyncOutcome::Synced { push_retries } if *push_retries > 0
        )));

        sync_once(&a, &a_id).unwrap();
        assert!(a.join(".orgasmic/machines").join(&a_id).is_dir());
        assert!(a.join(".orgasmic/machines").join(&b_id).is_dir());
        assert!(!a.join(".git/rebase-merge").exists());
    }

    #[test]
    fn no_remote_is_idle() {
        let tmp = tempfile::tempdir().unwrap();
        run(tmp.path(), &["init", "-b", "orgasmic"]);
        assert_eq!(
            sync_once(tmp.path(), &uuid::Uuid::new_v4().to_string()).unwrap(),
            SyncOutcome::Idle
        );
    }

    #[test]
    fn writer_sidecars_are_never_staged() {
        let tmp = tempfile::tempdir().unwrap();
        let (_remote, a, _b) = seed_remote(&tmp);
        let machine_id = uuid::Uuid::new_v4().to_string();
        let node = a.join(".orgasmic/tasks/T1/node.org");
        std::fs::create_dir_all(node.parent().unwrap()).unwrap();
        std::fs::write(&node, "task\n").unwrap();
        for suffix in [".tmp", ".tmp.req-rollback-x", ".bak.abc"] {
            std::fs::write(format!("{}{suffix}", node.display()), "sidecar\n").unwrap();
        }
        let tx = a
            .join(".orgasmic/machines")
            .join(&machine_id)
            .join("tx/2026-09.org");
        std::fs::create_dir_all(tx.parent().unwrap()).unwrap();
        std::fs::write(&tx, "tx\n").unwrap();
        std::fs::write(format!("{}.bak.zzz", tx.display()), "sidecar\n").unwrap();

        sync_once(&a, &machine_id).unwrap();

        let tracked = git_optional(&a, &["ls-files"]).unwrap().unwrap();
        assert!(tracked
            .lines()
            .any(|path| path == ".orgasmic/tasks/T1/node.org"));
        assert!(
            tracked
                .lines()
                .any(|path| { path == format!(".orgasmic/machines/{machine_id}/tx/2026-09.org") }),
            "tracked files: {tracked}"
        );
        assert!(!tracked.lines().any(|path| {
            path.ends_with(".tmp") || path.contains(".tmp.") || path.contains(".bak.")
        }));
    }

    #[tokio::test]
    async fn conflicting_two_writer_tick_parks_recovers_and_records_event() {
        let tmp = tempfile::tempdir().unwrap();
        let (remote, a, b) = seed_remote(&tmp);
        let machine_id = uuid::Uuid::new_v4().to_string();
        let relative = ".orgasmic/tasks/T1/node.org";
        for (repo, contents) in [(&a, "a\n"), (&b, "b\n")] {
            let path = repo.join(relative);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(path, contents).unwrap();
        }
        run(&b, &["add", relative]);
        run(
            &b,
            &[
                "-c",
                "user.name=test",
                "-c",
                "user.email=test@example.com",
                "commit",
                "-m",
                "remote conflict",
            ],
        );
        run(&b, &["push", "origin", "orgasmic"]);
        let statuses = LedgerSyncStatuses::default();
        let now = Utc::now();

        let conflict = sync_ledger_at(&a, &machine_id, &statuses, now)
            .expect("conflicting pull must be parked");
        let SyncOutcome::Conflict {
            parked_ref,
            paths,
            local_head,
            remote_head,
        } = &conflict
        else {
            panic!("unexpected outcome: {conflict:?}");
        };

        let status = statuses.lock().unwrap()[&a].clone();
        assert_eq!(status.outcome, "conflict");
        assert_eq!(status.consecutive_failures, 0);
        assert!(status.next_attempt_at.is_none());
        assert!(status.error.as_deref().unwrap().contains(parked_ref));
        assert_eq!(paths, &[relative]);
        assert_eq!(
            git_optional(&a, &["show", &format!("{parked_ref}:{relative}")])
                .unwrap()
                .as_deref(),
            Some("a")
        );
        assert_eq!(
            git_optional(&a, &["rev-parse", parked_ref])
                .unwrap()
                .as_deref(),
            Some(local_head.as_str())
        );
        assert_eq!(
            git_optional(&a, &["rev-parse", "HEAD"]).unwrap().as_deref(),
            Some(remote_head.as_str())
        );
        assert_eq!(
            git_optional(&a, &["rev-parse", "origin/orgasmic"])
                .unwrap()
                .as_deref(),
            Some(remote_head.as_str())
        );
        assert_eq!(std::fs::read_to_string(a.join(relative)).unwrap(), "b\n");

        let writer = crate::writer::spawn(crate::events::EventBus::new());
        record_sync_conflict(&writer, &a, "project-a", &machine_id, now, &conflict)
            .await
            .unwrap();
        let tx_path = a
            .join(".orgasmic/machines")
            .join(&machine_id)
            .join(format!("{}.org", now.format("%Y-%m")));
        let tx = orgasmic_core::tx::parse_tx_file(
            &std::fs::read_to_string(&tx_path).unwrap(),
            "machine conflict tx",
        )
        .unwrap();
        let events: Vec<_> = tx
            .iter()
            .filter(|entry| entry.ty == "ledger.sync_conflict")
            .collect();
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].machine, machine_id);
        assert!(events[0]
            .extra
            .iter()
            .any(|(key, value)| key == "PARKED_REF" && value == parked_ref));

        let fresh = ".orgasmic/tasks/T2/node.org";
        std::fs::create_dir_all(a.join(fresh).parent().unwrap()).unwrap();
        std::fs::write(a.join(fresh), "fresh a write\n").unwrap();
        assert!(sync_ledger_at(&a, &machine_id, &statuses, now + Duration::from_secs(1)).is_none());
        assert_eq!(statuses.lock().unwrap()[&a].outcome, "synced");
        assert_eq!(
            git_optional(&remote, &["show", &format!("orgasmic:{fresh}")])
                .unwrap()
                .as_deref(),
            Some("fresh a write")
        );
    }

    #[test]
    fn non_conflict_failure_is_reported_and_backed_off() {
        let tmp = tempfile::tempdir().unwrap();
        let (_remote, a, _b) = seed_remote(&tmp);
        let machine_id = uuid::Uuid::new_v4().to_string();
        run(
            &a,
            &[
                "remote",
                "set-url",
                "origin",
                path_arg(&tmp.path().join("missing.git")).unwrap(),
            ],
        );
        std::fs::write(a.join(".orgasmic/local.org"), "local\n").unwrap();
        let statuses = LedgerSyncStatuses::default();
        let now = Utc::now();

        assert!(sync_ledger_at(&a, &machine_id, &statuses, now).is_none());

        let failed = statuses.lock().unwrap()[&a].clone();
        assert_eq!(failed.outcome, "failed");
        assert_eq!(failed.consecutive_failures, 1);
        assert!(failed
            .error
            .as_deref()
            .unwrap()
            .contains("git pull --rebase failed"));
        assert!(!failed.error.as_deref().unwrap().contains("parked at"));
        let reflog = git_optional(&a, &["reflog", "--format=%H"]).unwrap();

        assert!(sync_ledger_at(&a, &machine_id, &statuses, now + Duration::from_secs(1)).is_none());

        let backed_off = statuses.lock().unwrap()[&a].clone();
        assert_eq!(backed_off.outcome, "backed_off");
        assert_eq!(backed_off.consecutive_failures, 1);
        assert_eq!(
            git_optional(&a, &["reflog", "--format=%H"]).unwrap(),
            reflog,
            "a backed-off tick must not invoke git"
        );
    }

    #[test]
    fn synced_repo_without_dotorg_does_not_fabricate_it() {
        let tmp = tempfile::tempdir().unwrap();
        let remote = tmp.path().join("remote.git");
        let repo = tmp.path().join("repo");
        run(tmp.path(), &["init", "--bare", path_arg(&remote).unwrap()]);
        run(
            tmp.path(),
            &["init", "-b", "orgasmic", path_arg(&repo).unwrap()],
        );
        std::fs::write(repo.join("README.md"), "ledger\n").unwrap();
        run(&repo, &["add", "README.md"]);
        run(
            &repo,
            &[
                "-c",
                "user.name=test",
                "-c",
                "user.email=test@example.com",
                "commit",
                "-m",
                "seed",
            ],
        );
        run(
            &repo,
            &["remote", "add", "origin", path_arg(&remote).unwrap()],
        );
        run(&repo, &["push", "-u", "origin", "orgasmic"]);

        sync_once(&repo, &uuid::Uuid::new_v4().to_string()).unwrap();
        assert!(!repo.join(".orgasmic").exists());
    }

    #[test]
    fn existing_ledger_views_are_ignored_untracked_and_idempotent() {
        let tmp = tempfile::tempdir().unwrap();
        let (_remote, a, _b) = seed_remote(&tmp);
        let machine_id = uuid::Uuid::new_v4().to_string();
        std::fs::create_dir_all(a.join(".orgasmic/views")).unwrap();
        std::fs::write(a.join(".orgasmic/.gitignore"), "tmp/\n").unwrap();
        std::fs::write(a.join(".orgasmic/views/board.org"), "derived\n").unwrap();
        run(&a, &["add", ".orgasmic"]);
        run(
            &a,
            &[
                "-c",
                "user.name=test",
                "-c",
                "user.email=test@example.com",
                "commit",
                "-m",
                "seed tracked views",
            ],
        );
        run(&a, &["push", "origin", "orgasmic"]);

        sync_once(&a, &machine_id).unwrap();
        assert_eq!(
            git_optional(&a, &["ls-files", ".orgasmic/views"])
                .unwrap()
                .as_deref(),
            Some("")
        );
        assert_eq!(
            std::fs::read(a.join(".orgasmic/.gitignore")).unwrap(),
            b"tmp/\nviews/\n"
        );
        assert!(a.join(".orgasmic/views/board.org").is_file());

        let first_commit = git_optional(&a, &["rev-parse", "HEAD"]).unwrap();
        sync_once(&a, &machine_id).unwrap();
        assert_eq!(
            git_optional(&a, &["rev-parse", "HEAD"]).unwrap(),
            first_commit
        );
    }

    /// A node this machine wrote must reach the remote even though the claim
    /// that authorised the write is already released. A dispatch releases its
    /// claim milliseconds after writing the node and the sync tick is seconds
    /// later, so "stage what is claimed right now" saw nothing to stage and the
    /// edit was silently dropped from the ledger.
    #[test]
    fn a_node_written_under_a_released_claim_still_reaches_the_remote() {
        let tmp = tempfile::tempdir().unwrap();
        let (_remote, a, b) = seed_remote(&tmp);
        let a_id = uuid::Uuid::new_v4().to_string();
        let b_id = uuid::Uuid::new_v4().to_string();

        let node = a.join(".orgasmic/tasks/TASK-DONE/node.org");
        std::fs::create_dir_all(node.parent().unwrap()).unwrap();
        std::fs::write(
            &node,
            "#+title: orgasmic task TASK-DONE\n#+orgasmic_version: 2\n\n\
             * DONE TASK-DONE Written under a claim :work:\n\
             :PROPERTIES:\n:ID:               TASK-DONE\n:END:\n",
        )
        .unwrap();

        // Claimed, then released — the state a closed dispatch leaves behind.
        let claims = a.join(".orgasmic/machines").join(&a_id).join("claims.org");
        std::fs::create_dir_all(claims.parent().unwrap()).unwrap();
        let mut writer = orgasmic_core::tx::TxWriter::open(claims).unwrap();
        for (tx_id, ty, time) in [
            (
                "claim-a",
                orgasmic_core::claims::CLAIMED,
                "[2026-08-26 Wed 10:00:00]",
            ),
            (
                "release-a",
                orgasmic_core::claims::RELEASED,
                "[2026-08-26 Wed 10:00:01]",
            ),
        ] {
            let mut event = orgasmic_core::tx::TxEntry::new(tx_id, ty, time, "test", &a_id);
            event.task = Some("TASK-DONE".into());
            writer.append(&event).unwrap();
        }
        drop(writer);

        assert!(matches!(
            sync_once(&a, &a_id).unwrap(),
            SyncOutcome::Synced { .. }
        ));
        sync_once(&b, &b_id).unwrap();

        let landed = b.join(".orgasmic/tasks/TASK-DONE/node.org");
        assert!(
            landed.is_file(),
            "the node written under the released claim never reached the other machine"
        );
        assert_eq!(
            std::fs::read_to_string(landed).unwrap(),
            std::fs::read_to_string(&node).unwrap()
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn two_daemon_loops_converge_through_the_bare_remote() {
        let tmp = tempfile::tempdir().unwrap();
        let (_remote, a, b) = seed_remote(&tmp);
        let a_home = daemon_home(&tmp.path().join("home-a"), &a, "project-a");
        let b_home = daemon_home(&tmp.path().join("home-b"), &b, "project-b");
        let a_id = a_home.load_or_mint_machine_id().unwrap();
        let b_id = b_home.load_or_mint_machine_id().unwrap();
        let options = crate::DaemonOptions {
            bind_override: Some("127.0.0.1".parse().unwrap()),
            port_override: Some(0),
            fs_watcher_enabled: false,
            ..crate::DaemonOptions::default()
        };
        let a_daemon = crate::Daemon::run(a_home, options.clone()).await.unwrap();
        let b_daemon = crate::Daemon::run(b_home, options).await.unwrap();
        for (ledger, id) in [(&a, &a_id), (&b, &b_id)] {
            let path = ledger
                .join(".orgasmic/machines")
                .join(id)
                .join("tx/2026-08.org");
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            let mut event = orgasmic_core::tx::TxEntry::new(
                format!("tx-{}", uuid::Uuid::new_v4()),
                "manager.action",
                "[2026-08-26 Wed 10:00:00]",
                "test",
                id,
            );
            event
                .extra
                .push(("EVENT_ID".into(), uuid::Uuid::new_v4().to_string()));
            std::fs::write(
                path,
                format!(
                    "#+title: machine tx\n#+orgasmic_version: 1\n\n{}",
                    event.render()
                ),
            )
            .unwrap();
            let claims_path = ledger
                .join(".orgasmic/machines")
                .join(id)
                .join("claims.org");
            let mut claim = orgasmic_core::tx::TxEntry::new(
                format!("claim-{id}"),
                orgasmic_core::claims::CLAIMED,
                "[2026-08-26 Wed 10:00:00]",
                "test",
                id,
            );
            claim.task = Some("TASK-RACE".into());
            claim
                .extra
                .push(("EVENT_ID".into(), uuid::Uuid::new_v4().to_string()));
            let mut writer = orgasmic_core::tx::TxWriter::open(claims_path).unwrap();
            writer.append(&claim).unwrap();
        }

        let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
        loop {
            if [&a, &b].iter().all(|ledger| {
                ledger.join(".orgasmic/machines").join(&a_id).is_dir()
                    && ledger.join(".orgasmic/machines").join(&b_id).is_dir()
            }) {
                break;
            }
            assert!(
                tokio::time::Instant::now() < deadline,
                "daemon ledgers did not converge"
            );
            tokio::time::sleep(Duration::from_millis(100)).await;
        }

        let expected = std::cmp::min(a_id.as_str(), b_id.as_str());
        for ledger in [&a, &b] {
            let claim = &orgasmic_core::read_claims(ledger).unwrap()["TASK-RACE"];
            assert_eq!(claim.holder, expected);
            assert_eq!(claim.contenders.len(), 2);
        }

        let _ = a_daemon.shutdown.send(());
        let _ = b_daemon.shutdown.send(());
        a_daemon.join.await.unwrap();
        b_daemon.join.await.unwrap();
    }
}
