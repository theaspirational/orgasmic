//! Git transport for the hidden ledger worktree.

use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::time::Duration;

use anyhow::{bail, Context, Result};

use crate::index::Index;

const SYNC_INTERVAL: Duration = Duration::from_secs(2);
const PUSH_ATTEMPTS: usize = 5;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum SyncOutcome {
    Idle,
    Synced { push_retries: usize },
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
    std::fs::create_dir_all(&dotorg).context("create .orgasmic")?;
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

    // Stage everything this machine changed inside the ledger, minus other
    // machines' pens.
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
    // A foreign node dir can only appear modified here if something wrote
    // outside its pen, which the claim gate refuses. Staging it makes the next
    // rebase conflict loudly instead of losing the edit silently.
    if ledger.join(".orgasmic").exists() {
        git(
            ledger,
            &[
                "add",
                "--all",
                "--",
                ".orgasmic",
                ":(exclude).orgasmic/machines",
            ],
        )?;
    }
    let machine_rel = PathBuf::from(".orgasmic/machines").join(machine_id);
    if ledger.join(&machine_rel).exists() {
        git(ledger, &["add", "--all", "--", path_arg(&machine_rel)?])?;
    }
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
                &format!("ledger: sync {machine_id}"),
            ],
        )?;
    }

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

/// The daemon's coalescing loop. A missing remote is deliberately silent.
pub(crate) fn spawn(
    index: Index,
    machine_id: String,
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
                .map(|entry| entry.path)
                .collect();
            for ledger in ledgers {
                let machine_id = machine_id.clone();
                let result =
                    tokio::task::spawn_blocking(move || sync_once(&ledger, &machine_id)).await;
                match result {
                    Ok(Ok(_)) => {}
                    Ok(Err(error)) => tracing::warn!(%error, "ledger sync failed; will retry"),
                    Err(error) => tracing::warn!(%error, "ledger sync task failed; will retry"),
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
