use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::time::{Duration, Instant};

use orgasmic_core::Home;
use orgasmic_daemon::{Daemon, DaemonOptions, RunningDaemon};
use reqwest::header::AUTHORIZATION;

#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;

/// Serialize heavy real-subprocess tests across ALL test binaries via an
/// exclusive advisory flock on a shared temp path (the same path the live
/// tmux/rmux tests use, TASK-X0ZVE). Dispatch tests boot a real daemon and run
/// real `git` worktree add/remove; under `cargo test --workspace` peak load the
/// worktree teardown races the post-close pruning assertions. Held for the whole
/// test via the returned guard (TASK-SJQ9V residual). Defined locally because an
/// integration binary cannot reach the lib's `#[cfg(test)]` test_support module.
fn live_session_guard() -> LiveSessionGuard {
    let path = std::env::temp_dir().join("orgasmic-live-session-tests.lock");
    let file = std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(&path)
        .expect("open live-session lock file");
    // MSRV 1.87: call fs2 explicitly — std's File::lock_exclusive (1.89) shadows it.
    fs2::FileExt::lock_exclusive(&file).expect("flock live-session lock");
    LiveSessionGuard(file)
}

struct LiveSessionGuard(std::fs::File);
impl Drop for LiveSessionGuard {
    fn drop(&mut self) {
        let _ = fs2::FileExt::unlock(&self.0);
    }
}

fn dispatch_artifact_has_content(path: &Path) -> bool {
    std::fs::metadata(path)
        .map(|meta| meta.len() > 0)
        .unwrap_or(false)
}

fn test_options() -> DaemonOptions {
    DaemonOptions {
        bind_override: Some("127.0.0.1".parse().unwrap()),
        port_override: Some(0),
        ..DaemonOptions::default()
    }
}

async fn boot(home: Home) -> RunningDaemon {
    boot_with_options(home, test_options()).await
}

async fn boot_with_options(home: Home, options: DaemonOptions) -> RunningDaemon {
    // Ensure the home config never defaults to port 4848 to avoid port
    // contention with a real daemon from the main checkout during
    // parallel test execution. Tests pass ORGASMIC_DAEMON_URL to CLI
    // subprocesses as the primary daemon address; if the env var is
    // lost due to subprocess environment leakage in parallel mode, we
    // want the fallback to use an unlikely port (65533) rather than
    // 4848, so the CLI fails obviously rather than silently talking to
    // the wrong daemon.
    home.ensure().unwrap();
    std::fs::write(home.config(), "bind_host: 127.0.0.1\nbind_port: 65533\n").unwrap();
    Daemon::run(home, options).await.expect("boot daemon")
}

fn write(path: &Path, contents: impl AsRef<str>) {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).unwrap();
    }
    std::fs::write(path, contents.as_ref()).unwrap();
}

fn repo_root() -> PathBuf {
    let mut here = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    loop {
        if here.join(".orgasmic").is_dir() && here.join("shipped").is_dir() {
            return here;
        }
        if !here.pop() {
            panic!("could not locate orgasmic repo root from CARGO_MANIFEST_DIR");
        }
    }
}

fn symlink_repo_source(home: &Home) {
    if home.source().exists() {
        return;
    }
    std::os::unix::fs::symlink(repo_root(), home.source()).unwrap();
}

fn run_git(project_root: &Path, args: &[&str]) -> String {
    let output = Command::new("git")
        .args(args)
        .current_dir(project_root)
        .output()
        .expect("run git");
    assert!(
        output.status.success(),
        "git {:?} failed\nstdout={}\nstderr={}",
        args,
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8_lossy(&output.stdout).trim().to_string()
}

fn seed_project(home: &Home, project_root: &Path) {
    symlink_repo_source(home);
    write(
        &home.user().join("workers/implementer-codex-appserver.org"),
        "* WORKER implementer-codex-appserver\n:PROPERTIES:\n:ID:                          implementer-codex-appserver\n:KIND:             implementer\n:DRIVER:                      acp-ws\n:HARNESS:                     codex\n:PROVIDERS:                   openai\n:MODELS:                      gpt-5.5\n:REASONING_EFFORTS:           high xhigh\n:DEFAULT_PROVIDER:            openai\n:DEFAULT_MODEL:               gpt-5.5\n:DEFAULT_EFFORT:              high\n:LINKED_SKILLS:\n:APPLICABLE_STATES:           claimed, analyzing, implementing, testing, fixing\n:MAX_ITERATIONS:              1\n:CONTEXT_BUDGET:              4000\n:VERSION:                     1\n:END:\n\n** Persona\nTest implementer.\n\n** Operating Rules\n- Keep test runs simulated.\n",
    );
    write(
        &home.user().join("workers/reviewer-codex-acp.org"),
        "* WORKER reviewer-codex-acp\n:PROPERTIES:\n:ID:                          reviewer-codex-acp\n:KIND:             reviewer\n:DRIVER:                      acp-stdio\n:HARNESS:                     claude\n:PROVIDERS:                   anthropic\n:MODELS:                      claude-sonnet-4-6\n:REASONING_EFFORTS:           high\n:DEFAULT_PROVIDER:            anthropic\n:DEFAULT_MODEL:               claude-sonnet-4-6\n:DEFAULT_EFFORT:              high\n:LINKED_SKILLS:\n:APPLICABLE_STATES:           reviewing\n:MAX_ITERATIONS:              1\n:CONTEXT_BUDGET:              4000\n:VERSION:                     1\n:END:\n\n** Persona\nTest reviewer.\n\n** Operating Rules\n- Keep test runs simulated.\n",
    );
    write(
        &project_root.join(".orgasmic/project.org"),
        "#+title: orgasmic\n#+orgasmic_version: 1\n\n* PROJECT orgasmic\n:PROPERTIES:\n:ID:                     orgasmic\n:END:\n",
    );
    write(
        &project_root.join(".orgasmic/tasks/backlog.org"),
        "#+title: backlog\n#+orgasmic_version: 1\n\n* BACKLOG TASK-DISPATCH Dispatch CLI smoke :cli:\n:PROPERTIES:\n:ID:               TASK-DISPATCH\n:END:\n\n* BACKLOG TASK-ABORT Dispatch abort smoke :cli:\n:PROPERTIES:\n:ID:               TASK-ABORT\n:END:\n\n* BACKLOG TASK-FIX Fix subtask smoke :cli:\n:PROPERTIES:\n:ID:               TASK-FIX\n:END:\n\n* BACKLOG TASK-FIX-DECL Declarative fix subtask smoke :cli:\n:PROPERTIES:\n:ID:               TASK-FIX-DECL\n:FIX_SUBTASK:      t\n:END:\n\n* BACKLOG TASK-NO-MERGE Missing merge smoke :cli:\n:PROPERTIES:\n:ID:               TASK-NO-MERGE\n:END:\n\n* BACKLOG TASK-BUNDLE-A Bundle smoke A :cli:\n:PROPERTIES:\n:ID:               TASK-BUNDLE-A\n:END:\n\n* BACKLOG TASK-BUNDLE-B Bundle smoke B :cli:\n:PROPERTIES:\n:ID:               TASK-BUNDLE-B\n:END:\n\n* BACKLOG TASK-CLEANUP Cleanup smoke :cli:\n:PROPERTIES:\n:ID:               TASK-CLEANUP\n:END:\n",
    );
    write(
        &project_root.join(".orgasmic/tasks/in_review.org"),
        "#+title: in review\n#+orgasmic_version: 1\n\n* IN_REVIEW TASK-REVIEW Reviewer dispatch smoke :cli:\n:PROPERTIES:\n:ID:               TASK-REVIEW\n:END:\n\n* IN_REVIEW TASK-REVIEW-ISSUES Reviewer issue smoke :cli:\n:PROPERTIES:\n:ID:               TASK-REVIEW-ISSUES\n:END:\n\n* IN_REVIEW TASK-SHIP-CLEAN Ship verdict smoke :cli:\n:PROPERTIES:\n:ID:               TASK-SHIP-CLEAN\n:END:\n\n* IN_REVIEW TASK-HAS-ISSUES Has-issues verdict smoke :cli:\n:PROPERTIES:\n:ID:               TASK-HAS-ISSUES\n:END:\n",
    );
    for (name, title) in [
        ("todo.org", "todo"),
        ("in_progress.org", "in progress"),
        ("done.org", "done"),
        ("cancelled.org", "cancelled"),
    ] {
        write(
            &project_root.join(".orgasmic/tasks").join(name),
            format!("#+title: {title}\n#+orgasmic_version: 1\n\n"),
        );
    }
    write(
        &project_root.join(".orgasmic/tasks/goal.org"),
        "#+title: goal\n#+orgasmic_version: 1\n\n* GOAL Test goal\n:PROPERTIES:\n:ID:               goal-test\n:STATUS:           active\n:END:\n",
    );
    write(
        &home.board(),
        format!(
            "#+title: orgasmic board\n#+orgasmic_version: 1\n\n* PROJECT orgasmic\n:PROPERTIES:\n:ID:               orgasmic\n:PATH:             {}\n:BRANCH:           main\n:STATUS:           active\n:END:\n",
            project_root.display()
        ),
    );
}

fn init_git_project(project_root: &Path) -> String {
    run_git(project_root, &["init", "-b", "main"]);
    run_git(
        project_root,
        &["config", "user.email", "tester@example.com"],
    );
    run_git(project_root, &["config", "user.name", "Test User"]);
    run_git(project_root, &["add", "."]);
    run_git(project_root, &["commit", "-m", "init"]);
    run_git(project_root, &["rev-parse", "HEAD"])
}

fn write_stub_codex(bin_dir: &Path) -> PathBuf {
    write_stub_codex_with_sleep(bin_dir, None)
}

fn write_sleeping_stub_codex(bin_dir: &Path) -> PathBuf {
    write_stub_codex_with_sleep(bin_dir, Some(60))
}

fn write_stub_codex_with_sleep(bin_dir: &Path, sleep_seconds: Option<u64>) -> PathBuf {
    let path = bin_dir.join("codex");
    let sleep_line = sleep_seconds
        .map(|seconds| format!("sleep {seconds}\n"))
        .unwrap_or_default();
    write(
        &path,
        format!(
            "#!/bin/sh\nlast=\"\"\nwhile [ \"$#\" -gt 0 ]; do\n  if [ \"$1\" = \"--output-last-message\" ]; then\n    shift\n    last=\"$1\"\n  fi\n  shift\ndone\nif [ -n \"$last\" ]; then\n  printf 'stub-done\\n' > \"$last\"\nfi\n{}exit 0\n",
            sleep_line
        ),
    );
    #[cfg(unix)]
    {
        let mut perms = std::fs::metadata(&path).unwrap().permissions();
        perms.set_mode(0o755);
        std::fs::set_permissions(&path, perms).unwrap();
    }
    path
}

fn path_with_stub(bin_dir: &Path) -> std::ffi::OsString {
    let mut paths = vec![bin_dir.to_path_buf()];
    if let Some(existing) = std::env::var_os("PATH") {
        paths.extend(std::env::split_paths(&existing));
    }
    std::env::join_paths(paths).unwrap()
}

fn path_only(bin_dir: &Path) -> std::ffi::OsString {
    std::env::join_paths([bin_dir.to_path_buf()]).unwrap()
}

fn write_git_proxy(bin_dir: &Path) {
    let output = Command::new("sh")
        .args(["-c", "command -v git"])
        .output()
        .expect("locate git");
    assert!(
        output.status.success(),
        "command -v git failed\nstderr={}",
        String::from_utf8_lossy(&output.stderr)
    );
    let git = String::from_utf8_lossy(&output.stdout).trim().to_string();
    let path = bin_dir.join("git");
    write(&path, format!("#!/bin/sh\nexec {} \"$@\"\n", git));
    #[cfg(unix)]
    {
        let mut perms = std::fs::metadata(&path).unwrap().permissions();
        perms.set_mode(0o755);
        std::fs::set_permissions(&path, perms).unwrap();
    }
}

fn write_nonspawning_codex(bin_dir: &Path) {
    let path = bin_dir.join("codex");
    write(&path, "#!/nonexistent/orgasmic-codex-test\n");
    #[cfg(unix)]
    {
        let mut perms = std::fs::metadata(&path).unwrap().permissions();
        perms.set_mode(0o755);
        std::fs::set_permissions(&path, perms).unwrap();
    }
}

fn run_orgasmic(
    home: &Home,
    running: &RunningDaemon,
    project_root: &Path,
    path_env: &std::ffi::OsString,
    args: &[&str],
) -> String {
    let output = run_orgasmic_output(home, running, project_root, path_env, args);
    assert!(
        output.status.success(),
        "orgasmic {:?} failed\nstdout={}\nstderr={}",
        args,
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8_lossy(&output.stdout).to_string()
}

fn run_orgasmic_output(
    home: &Home,
    running: &RunningDaemon,
    project_root: &Path,
    path_env: &std::ffi::OsString,
    args: &[&str],
) -> Output {
    run_orgasmic_output_with_env(home, running, project_root, path_env, args, &[])
}

fn run_orgasmic_output_with_env(
    home: &Home,
    running: &RunningDaemon,
    project_root: &Path,
    path_env: &std::ffi::OsString,
    args: &[&str],
    extra_env: &[(&str, &str)],
) -> Output {
    run_orgasmic_output_with_daemon_url(
        home,
        &format!("http://{}", running.addr),
        project_root,
        path_env,
        args,
        extra_env,
    )
}

fn run_orgasmic_output_with_daemon_url(
    home: &Home,
    daemon_url: &str,
    project_root: &Path,
    path_env: &std::ffi::OsString,
    args: &[&str],
    extra_env: &[(&str, &str)],
) -> Output {
    let exe = orgasmic_exe();
    let mut command = Command::new(exe);
    command
        .args(args)
        .current_dir(project_root)
        .env("ORGASMIC_HOME", &home.root)
        .env("ORGASMIC_DAEMON_URL", daemon_url)
        .env("PATH", path_env);
    for (key, value) in extra_env {
        command.env(key, value);
    }
    command.output().expect("run orgasmic")
}

fn orgasmic_exe() -> PathBuf {
    let exe = PathBuf::from(env!("CARGO_BIN_EXE_orgasmic"));
    if exe.is_absolute() {
        exe
    } else {
        std::env::current_dir().unwrap().join(exe)
    }
}

fn branch_exists(project_root: &Path, branch: &str) -> bool {
    Command::new("git")
        .args([
            "show-ref",
            "--verify",
            "--quiet",
            &format!("refs/heads/{branch}"),
        ])
        .current_dir(project_root)
        .status()
        .map(|status| status.success())
        .unwrap_or(false)
}

fn run_orgasmic_failure(
    home: &Home,
    running: &RunningDaemon,
    project_root: &Path,
    path_env: &std::ffi::OsString,
    args: &[&str],
) -> String {
    let output = run_orgasmic_output(home, running, project_root, path_env, args);
    assert!(
        !output.status.success(),
        "orgasmic {:?} unexpectedly succeeded\nstdout={}\nstderr={}",
        args,
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8_lossy(&output.stderr).to_string()
}

fn wait_for_file(path: &Path) {
    let deadline = Instant::now() + Duration::from_secs(2);
    while Instant::now() < deadline {
        if path.exists() {
            return;
        }
        std::thread::sleep(Duration::from_millis(25));
    }
    panic!("timed out waiting for {}", path.display());
}

fn sprint_source(project_root: &Path) -> String {
    [
        "backlog.org",
        "todo.org",
        "in_progress.org",
        "in_review.org",
        "done.org",
        "cancelled.org",
    ]
    .into_iter()
    .map(|name| project_root.join(".orgasmic/tasks").join(name))
    .filter(|path| path.is_file())
    .map(|path| std::fs::read_to_string(path).unwrap())
    .collect::<Vec<_>>()
    .join("\n")
}

fn assert_task_stage(project_root: &Path, task: &str, keyword: &str, state: &str) {
    let _ = state;
    let sprint = sprint_source(project_root);
    let heading = format!("* {keyword} {task}");
    sprint
        .find(&heading)
        .unwrap_or_else(|| panic!("expected {task} heading keyword {keyword}\n{sprint}"));
}

fn count_occurrences(haystack: &str, needle: &str) -> usize {
    haystack.match_indices(needle).count()
}

/// The daemon names project tx files by the current calendar month
/// (`Utc::now().format("%Y-%m")`, see `daemon::api::resolve_tx_destination`).
/// Tests must derive the same name rather than hardcode a month, or they break
/// at every month rollover.
fn tx_file_name() -> String {
    format!("{}.org", chrono::Utc::now().format("%Y-%m"))
}

fn tx_file_path(project_root: &Path) -> PathBuf {
    project_root.join(".orgasmic/tx").join(tx_file_name())
}

fn tx_log(project_root: &Path) -> String {
    std::fs::read_to_string(tx_file_path(project_root)).unwrap()
}

fn tx_id_for(raw: &str, ty: &str, task: &str) -> String {
    for block in raw.split("\n\n* TX ") {
        if block.contains(&format!(":TYPE:         {ty}"))
            && block.contains(&format!(":TASK:         {task}"))
        {
            for line in block.lines() {
                if let Some(value) = line.trim_start().strip_prefix(":TX_ID:") {
                    return value.trim().to_string();
                }
            }
        }
    }
    panic!("missing tx id for type={ty} task={task}\n{raw}");
}

fn append_partial_close_tx(
    project_root: &Path,
    closed_tx: &str,
    task: &str,
    head: &str,
    branch: &str,
) {
    let path = tx_file_path(project_root);
    let mut raw = std::fs::read_to_string(&path).unwrap();
    raw.push_str(&format!(
        "\n\n* TX 2026-05-23 Sat 10:01:00 implementer.done {task}\n:PROPERTIES:\n:TX_ID:        tx-partial-close-{task}\n:TIME:         [2026-05-23 Sat 10:01:00]\n:TYPE:         implementer.done\n:ACTOR:        agent.implementer\n:MACHINE:      test\n:PROJECT:      orgasmic\n:TASK:         {task}\n:MERGE_SHA:    {head}\n:BRANCH:       {branch}\n:CLOSED_TX:    {closed_tx}\n:CLEANUP_STATUS: ok\n:END:\n"
    ));
    write(&path, raw);
}

fn tx_property_for(raw: &str, ty: &str, task: &str, key: &str) -> String {
    for block in raw.split("\n\n* TX ") {
        if block.contains(&format!(":TYPE:         {ty}"))
            && block.contains(&format!(":TASK:         {task}"))
        {
            let prefix = format!(":{key}:");
            for line in block.lines() {
                if let Some(value) = line.trim_start().strip_prefix(prefix.as_str()) {
                    return value.trim().to_string();
                }
            }
        }
    }
    panic!("missing {key} for type={ty} task={task}\n{raw}");
}

fn resolve_project_path(project_root: &Path, path: &str) -> PathBuf {
    let path = PathBuf::from(path);
    if path.is_relative() {
        project_root.join(path)
    } else {
        path
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn manager_dispatch_status_close_done_with_stub_codex() {
    let tmp = tempfile::tempdir().unwrap();
    let home = Home::at(tmp.path().join("home"));
    home.ensure().unwrap();
    let project_root = tmp.path().join("project");
    std::fs::create_dir_all(&project_root).unwrap();
    seed_project(&home, &project_root);
    let head = init_git_project(&project_root);
    let bin_dir = tmp.path().join("bin");
    std::fs::create_dir_all(&bin_dir).unwrap();
    write_stub_codex(&bin_dir);
    let path_env = path_with_stub(&bin_dir);
    let codex_dir = tmp.path().join("codex");
    std::fs::create_dir_all(&codex_dir).unwrap();
    let brief = codex_dir.join("task-dispatch-brief.md");
    write(&brief, "stub implementer brief");
    let last = codex_dir.join("task-dispatch-last.txt");
    let worktree = tmp.path().join("worktrees/task-dispatch");

    let running = boot(home.clone()).await;
    let dry_worktree = tmp.path().join("worktrees/task-dispatch-dry");
    let dry_stdout = run_orgasmic(
        &home,
        &running,
        &project_root,
        &path_env,
        &[
            "manager",
            "dispatch",
            "--task",
            "TASK-DISPATCH",
            "--kind",
            "implementer",
            "--worker",
            "implementer-codex-appserver",
            "--brief",
            brief.to_str().unwrap(),
            "--from",
            &head,
            "--worktree",
            dry_worktree.to_str().unwrap(),
            "--branch",
            "task-dispatch-dry-run",
            "--dry-run",
        ],
    );
    assert!(dry_stdout.contains("dispatch plan:"));
    assert!(!dry_worktree.exists(), "dry-run must not create worktree");
    assert!(
        !project_root.join(".orgasmic/tx").exists(),
        "dry-run must not append tx"
    );

    let dispatch_stdout = run_orgasmic(
        &home,
        &running,
        &project_root,
        &path_env,
        &[
            "manager",
            "dispatch",
            "--task",
            "TASK-DISPATCH",
            "--kind",
            "implementer",
            "--worker",
            "implementer-codex-appserver",
            "--brief",
            brief.to_str().unwrap(),
            "--from",
            &head,
            "--worktree",
            worktree.to_str().unwrap(),
            "--branch",
            "task-dispatch-test-impl",
            "--reason",
            "integration smoke",
        ],
    );
    assert!(dispatch_stdout.contains("dispatched: TASK-DISPATCH implementer pid="));
    assert!(
        dispatch_stdout.contains("watch: orgasmic run show "),
        "non-subprocess dispatch modes must suggest `orgasmic run show` for watching: {dispatch_stdout}"
    );
    assert!(worktree.is_dir(), "worktree should exist");
    assert_task_stage(&project_root, "TASK-DISPATCH", "IN_PROGRESS", "in_progress");
    let _ = last;

    let tx_path = project_root.join(".orgasmic/tx");
    let tx_raw = std::fs::read_to_string(tx_path.join(tx_file_name())).unwrap();
    assert!(tx_raw.contains(":TYPE:         manager.dispatch_started"));
    assert!(tx_raw.contains(":TASK:         TASK-DISPATCH"));
    assert!(
        !tx_raw.contains(":WORKER_PID:") && !tx_raw.contains(":CODEX_PID:"),
        "dispatch_started is appended after acquire, so it must not include the worker pid"
    );

    let status_stdout = run_orgasmic(
        &home,
        &running,
        &project_root,
        &path_env,
        &["manager", "dispatch-status", "--task", "TASK-DISPATCH"],
    );
    assert!(status_stdout.contains("TASK=TASK-DISPATCH"));
    assert!(status_stdout.contains("[exists]"));

    let close_stdout = run_orgasmic(
        &home,
        &running,
        &project_root,
        &path_env,
        &[
            "manager",
            "dispatch-close",
            "--task",
            "TASK-DISPATCH",
            "--status",
            "done",
            "--merge-sha",
            &head,
            "--codex-commit",
            &head,
            "--tokens",
            "42",
            "--wall",
            "1s",
            "--reason",
            "stub landed",
        ],
    );
    assert!(close_stdout.contains("closed: TASK-DISPATCH implementer.done tx="));
    assert!(!worktree.exists(), "worktree should be removed on close");
    assert_task_stage(&project_root, "TASK-DISPATCH", "IN_REVIEW", "in_review");
    let tx_raw = std::fs::read_to_string(tx_path.join(tx_file_name())).unwrap();
    assert!(tx_raw.contains(":TYPE:         implementer.done"));
    assert!(tx_raw.contains(":MERGE_SHA:    "));
    assert!(tx_raw.contains(":CLOSED_TX:    "));
    assert!(tx_raw.contains(":CLEANUP_STATUS: ok"));

    let status_stdout = run_orgasmic(
        &home,
        &running,
        &project_root,
        &path_env,
        &["manager", "dispatch-status", "--task", "TASK-DISPATCH"],
    );
    assert!(
        status_stdout.trim().is_empty(),
        "closed dispatch should not appear in status: {status_stdout}"
    );

    let review_brief = codex_dir.join("task-review-brief.md");
    write(&review_brief, "stub reviewer brief");
    let review_last = codex_dir.join("task-review-last.txt");
    let review_worktree = tmp.path().join("worktrees/task-review");
    let review_dry_worktree = tmp.path().join("worktrees/task-review-dry");
    let review_dry_stdout = run_orgasmic(
        &home,
        &running,
        &project_root,
        &path_env,
        &[
            "manager",
            "dispatch",
            "--task",
            "TASK-REVIEW",
            "--kind",
            "reviewer",
            "--worker",
            "reviewer-codex-acp",
            "--brief",
            review_brief.to_str().unwrap(),
            "--from",
            &head,
            "--worktree",
            review_dry_worktree.to_str().unwrap(),
            "--branch",
            "task-review-dry-run",
            "--dry-run",
        ],
    );
    assert!(review_dry_stdout.contains("dispatch plan:"));
    assert!(
        !review_dry_worktree.exists(),
        "reviewer dry-run must not create worktree"
    );

    let review_dispatch_stdout = run_orgasmic(
        &home,
        &running,
        &project_root,
        &path_env,
        &[
            "manager",
            "dispatch",
            "--task",
            "TASK-REVIEW",
            "--kind",
            "reviewer",
            "--worker",
            "reviewer-codex-acp",
            "--brief",
            review_brief.to_str().unwrap(),
            "--from",
            &head,
            "--worktree",
            review_worktree.to_str().unwrap(),
            "--branch",
            "task-review-test",
            "--reason",
            "reviewer smoke",
        ],
    );
    assert!(review_dispatch_stdout.contains("dispatched: TASK-REVIEW reviewer pid="));
    assert_task_stage(&project_root, "TASK-REVIEW", "IN_REVIEW", "in_review");
    let _ = review_last;

    let review_close_stdout = run_orgasmic(
        &home,
        &running,
        &project_root,
        &path_env,
        &[
            "manager",
            "dispatch-close",
            "--task",
            "TASK-REVIEW",
            "--status",
            "done",
            "--property",
            "VERDICT=clean",
            "--property",
            "FINDINGS_TOTAL=0",
            "--property",
            "REPORT_PATH=/tmp/task-review-report.md",
            "--reviewed-diff",
            "abc123..def456",
            "--reason",
            "review clean",
        ],
    );
    assert!(review_close_stdout.contains("closed: TASK-REVIEW reviewer.done tx="));
    assert_task_stage(&project_root, "TASK-REVIEW", "DONE", "done");
    let tx_raw = std::fs::read_to_string(tx_path.join(tx_file_name())).unwrap();
    assert!(tx_raw.contains(":TYPE:         reviewer.done"));
    assert!(tx_raw.contains(":VERDICT:      clean"));
    assert!(tx_raw.contains(":FINDINGS_TOTAL: 0"));
    assert!(tx_raw.contains(":REPORT_PATH:  /tmp/task-review-report.md"));
    assert!(tx_raw.contains(":REVIEWED_DIFF: abc123..def456"));

    let second_brief = codex_dir.join("task-dispatch-second-brief.md");
    write(&second_brief, "second implementer brief");
    let second_last = codex_dir.join("task-dispatch-second-last.txt");
    let second_worktree = tmp.path().join("worktrees/task-dispatch-second");
    let second_dispatch_stdout = run_orgasmic(
        &home,
        &running,
        &project_root,
        &path_env,
        &[
            "manager",
            "dispatch",
            "--task",
            "TASK-NO-MERGE",
            "--kind",
            "implementer",
            "--worker",
            "implementer-codex-appserver",
            "--brief",
            second_brief.to_str().unwrap(),
            "--from",
            &head,
            "--worktree",
            second_worktree.to_str().unwrap(),
            "--branch",
            "task-dispatch-no-merge",
        ],
    );
    assert!(second_dispatch_stdout.contains("dispatched: TASK-NO-MERGE implementer pid="));
    let _ = second_last;
    let close_stderr = run_orgasmic_failure(
        &home,
        &running,
        &project_root,
        &path_env,
        &[
            "manager",
            "dispatch-close",
            "--task",
            "TASK-NO-MERGE",
            "--status",
            "done",
        ],
    );
    assert!(
        second_worktree.exists(),
        "validation failure must not clean up the worktree"
    );
    assert!(
        close_stderr.contains(
            "--merge-sha is required when closing an implementer dispatch as implementer.done"
        ),
        "unexpected close error: {close_stderr}"
    );

    let _ = running.shutdown.send(());
    let _ = running.join.await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn dispatch_close_uses_fix_subtask_property_and_abort_backlog() {
    let tmp = tempfile::tempdir().unwrap();
    let home = Home::at(tmp.path().join("home"));
    home.ensure().unwrap();
    let project_root = tmp.path().join("project");
    std::fs::create_dir_all(&project_root).unwrap();
    seed_project(&home, &project_root);
    let head = init_git_project(&project_root);
    let bin_dir = tmp.path().join("bin");
    std::fs::create_dir_all(&bin_dir).unwrap();
    write_stub_codex(&bin_dir);
    let path_env = path_with_stub(&bin_dir);
    let codex_dir = tmp.path().join("codex");
    std::fs::create_dir_all(&codex_dir).unwrap();

    let running = boot(home.clone()).await;

    let fix_brief = codex_dir.join("task-fix-brief.md");
    write(&fix_brief, "fix subtask brief");
    let fix_last = codex_dir.join("task-fix-last.txt");
    let fix_worktree = tmp.path().join("worktrees/task-fix");
    let fix_stdout = run_orgasmic(
        &home,
        &running,
        &project_root,
        &path_env,
        &[
            "manager",
            "dispatch",
            "--task",
            "TASK-FIX",
            "--kind",
            "implementer",
            "--worker",
            "implementer-codex-appserver",
            "--brief",
            fix_brief.to_str().unwrap(),
            "--from",
            &head,
            "--worktree",
            fix_worktree.to_str().unwrap(),
            "--branch",
            "task-fix-impl",
        ],
    );
    assert!(fix_stdout.contains("dispatched: TASK-FIX implementer pid="));
    let _ = fix_last;
    run_orgasmic(
        &home,
        &running,
        &project_root,
        &path_env,
        &[
            "manager",
            "dispatch-close",
            "--task",
            "TASK-FIX",
            "--status",
            "done",
            "--merge-sha",
            &head,
        ],
    );
    assert_task_stage(&project_root, "TASK-FIX", "IN_REVIEW", "in_review");

    let fix_decl_brief = codex_dir.join("task-fix-decl-brief.md");
    write(&fix_decl_brief, "declarative fix subtask brief");
    let fix_decl_last = codex_dir.join("task-fix-decl-last.txt");
    let fix_decl_worktree = tmp.path().join("worktrees/task-fix-decl");
    let fix_decl_stdout = run_orgasmic(
        &home,
        &running,
        &project_root,
        &path_env,
        &[
            "manager",
            "dispatch",
            "--task",
            "TASK-FIX-DECL",
            "--kind",
            "implementer",
            "--worker",
            "implementer-codex-appserver",
            "--brief",
            fix_decl_brief.to_str().unwrap(),
            "--from",
            &head,
            "--worktree",
            fix_decl_worktree.to_str().unwrap(),
            "--branch",
            "task-fix-decl-impl",
        ],
    );
    assert!(fix_decl_stdout.contains("dispatched: TASK-FIX-DECL implementer pid="));
    let _ = fix_decl_last;
    run_orgasmic(
        &home,
        &running,
        &project_root,
        &path_env,
        &[
            "manager",
            "dispatch-close",
            "--task",
            "TASK-FIX-DECL",
            "--status",
            "done",
            "--merge-sha",
            &head,
        ],
    );
    assert_task_stage(&project_root, "TASK-FIX-DECL", "DONE", "done");

    let abort_brief = codex_dir.join("task-abort-brief.md");
    write(&abort_brief, "abort brief");
    let abort_last = codex_dir.join("task-abort-last.txt");
    let abort_worktree = tmp.path().join("worktrees/task-abort");
    run_orgasmic(
        &home,
        &running,
        &project_root,
        &path_env,
        &[
            "manager",
            "dispatch",
            "--task",
            "TASK-ABORT",
            "--kind",
            "implementer",
            "--worker",
            "implementer-codex-appserver",
            "--brief",
            abort_brief.to_str().unwrap(),
            "--from",
            &head,
            "--worktree",
            abort_worktree.to_str().unwrap(),
            "--branch",
            "task-abort-impl",
        ],
    );
    assert_task_stage(&project_root, "TASK-ABORT", "IN_PROGRESS", "in_progress");
    let _ = abort_last;
    run_orgasmic(
        &home,
        &running,
        &project_root,
        &path_env,
        &[
            "manager",
            "dispatch-close",
            "--task",
            "TASK-ABORT",
            "--status",
            "aborted",
            "--reason",
            "stub abort",
        ],
    );
    assert_task_stage(&project_root, "TASK-ABORT", "TODO", "todo");
    let tx_raw = std::fs::read_to_string(tx_file_path(&project_root)).unwrap();
    assert!(tx_raw.contains(":TYPE:         manager.dispatch_aborted"));
    assert!(tx_raw.contains(":CLEANUP_STATUS: ok"));

    let _ = running.shutdown.send(());
    let _ = running.join.await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn dispatch_rejects_any_overlapping_open_task() {
    let tmp = tempfile::tempdir().unwrap();
    let home = Home::at(tmp.path().join("home"));
    home.ensure().unwrap();
    let project_root = tmp.path().join("project");
    std::fs::create_dir_all(&project_root).unwrap();
    seed_project(&home, &project_root);
    let head = init_git_project(&project_root);
    let bin_dir = tmp.path().join("bin");
    std::fs::create_dir_all(&bin_dir).unwrap();
    write_stub_codex(&bin_dir);
    let path_env = path_with_stub(&bin_dir);
    let codex_dir = tmp.path().join("codex");
    std::fs::create_dir_all(&codex_dir).unwrap();
    let first_brief = codex_dir.join("task-overlap-first-brief.md");
    write(&first_brief, "first overlap brief");
    let first_worktree = tmp.path().join("worktrees/task-overlap-first");

    let running = boot(home.clone()).await;
    run_orgasmic(
        &home,
        &running,
        &project_root,
        &path_env,
        &[
            "manager",
            "dispatch",
            "--task",
            "TASK-BUNDLE-A",
            "--kind",
            "implementer",
            "--worker",
            "implementer-codex-appserver",
            "--brief",
            first_brief.to_str().unwrap(),
            "--from",
            &head,
            "--worktree",
            first_worktree.to_str().unwrap(),
            "--branch",
            "task-overlap-first",
        ],
    );

    let second_brief = codex_dir.join("task-overlap-second-brief.md");
    write(&second_brief, "second overlap brief");
    let second_worktree = tmp.path().join("worktrees/task-overlap-second");
    let stderr = run_orgasmic_failure(
        &home,
        &running,
        &project_root,
        &path_env,
        &[
            "manager",
            "dispatch",
            "--task",
            "TASK-BUNDLE-A",
            "--task",
            "TASK-BUNDLE-B",
            "--kind",
            "implementer",
            "--worker",
            "implementer-codex-appserver",
            "--brief",
            second_brief.to_str().unwrap(),
            "--from",
            &head,
            "--worktree",
            second_worktree.to_str().unwrap(),
            "--branch",
            "task-overlap-second",
        ],
    );
    assert!(stderr.contains("overlapping task(s) TASK-BUNDLE-A"));
    assert!(stderr.contains("TASK-BUNDLE-A"));
    assert!(
        !second_worktree.exists(),
        "overlap validation should fail before creating the second worktree"
    );

    let _ = running.shutdown.send(());
    let _ = running.join.await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn dispatch_endpoint_failure_restores_bundled_lifecycle() {
    let tmp = tempfile::tempdir().unwrap();
    let home = Home::at(tmp.path().join("home"));
    home.ensure().unwrap();
    let project_root = tmp.path().join("project");
    std::fs::create_dir_all(&project_root).unwrap();
    seed_project(&home, &project_root);
    let head = init_git_project(&project_root);
    let bin_dir = tmp.path().join("bin");
    std::fs::create_dir_all(&bin_dir).unwrap();
    write_git_proxy(&bin_dir);
    write_nonspawning_codex(&bin_dir);
    let path_env = path_only(&bin_dir);
    let codex_dir = tmp.path().join("codex");
    std::fs::create_dir_all(&codex_dir).unwrap();
    let brief = codex_dir.join("task-spawn-fail-brief.md");
    write(&brief, "spawn failure brief");
    let worktree = tmp.path().join("worktrees/task-spawn-fail");

    let running = boot(home.clone()).await;
    let stderr = run_orgasmic_failure(
        &home,
        &running,
        &project_root,
        &path_env,
        &[
            "manager",
            "dispatch",
            "--task",
            "TASK-BUNDLE-A",
            "--task",
            "TASK-BUNDLE-B",
            "--kind",
            "implementer",
            "--worker",
            "missing-worker",
            "--brief",
            brief.to_str().unwrap(),
            "--from",
            &head,
            "--worktree",
            worktree.to_str().unwrap(),
            "--branch",
            "task-spawn-fail",
        ],
    );
    assert!(
        stderr.contains("daemon dispatch failed"),
        "unexpected stderr: {stderr}"
    );
    assert_task_stage(&project_root, "TASK-BUNDLE-A", "BACKLOG", "backlog");
    assert_task_stage(&project_root, "TASK-BUNDLE-B", "BACKLOG", "backlog");
    assert!(
        !worktree.exists(),
        "dispatch-fail rollback should remove worktree"
    );
    assert!(
        !branch_exists(&project_root, "task-spawn-fail"),
        "dispatch-fail rollback should remove branch"
    );

    let tx_path = tx_file_path(&project_root);
    let tx_raw = std::fs::read_to_string(&tx_path).unwrap_or_default();
    assert!(
        !tx_raw.contains(":TYPE:         manager.dispatch_started"),
        "daemon dispatch failure must not leave dispatch_started: {tx_raw}"
    );
    assert!(
        !tx_raw.contains(":TYPE:         manager.dispatch_aborted"),
        "daemon dispatch failure must not append dispatch_aborted without dispatch_started: {tx_raw}"
    );

    let _ = running.shutdown.send(());
    let _ = running.join.await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn reviewer_close_with_recommended_subtasks_stays_in_review() {
    let tmp = tempfile::tempdir().unwrap();
    let home = Home::at(tmp.path().join("home"));
    home.ensure().unwrap();
    let project_root = tmp.path().join("project");
    std::fs::create_dir_all(&project_root).unwrap();
    seed_project(&home, &project_root);
    let head = init_git_project(&project_root);
    let bin_dir = tmp.path().join("bin");
    std::fs::create_dir_all(&bin_dir).unwrap();
    write_stub_codex(&bin_dir);
    let path_env = path_with_stub(&bin_dir);
    let codex_dir = tmp.path().join("codex");
    std::fs::create_dir_all(&codex_dir).unwrap();
    let brief = codex_dir.join("task-review-issues-brief.md");
    write(&brief, "reviewer issue brief");
    let last = codex_dir.join("task-review-issues-last.txt");
    let worktree = tmp.path().join("worktrees/task-review-issues");

    let running = boot(home.clone()).await;
    run_orgasmic(
        &home,
        &running,
        &project_root,
        &path_env,
        &[
            "manager",
            "dispatch",
            "--task",
            "TASK-REVIEW-ISSUES",
            "--kind",
            "reviewer",
            "--worker",
            "reviewer-codex-acp",
            "--brief",
            brief.to_str().unwrap(),
            "--from",
            &head,
            "--worktree",
            worktree.to_str().unwrap(),
            "--branch",
            "task-review-issues",
        ],
    );
    let _ = last;
    run_orgasmic(
        &home,
        &running,
        &project_root,
        &path_env,
        &[
            "manager",
            "dispatch-close",
            "--task",
            "TASK-REVIEW-ISSUES",
            "--status",
            "done",
            "--property",
            "VERDICT=has-issues",
            "--property",
            "REPORT_PATH=/tmp/task-review-issues.md",
            "--property",
            "RECOMMENDED_SUBTASKS=TASK-REVIEW-ISSUES.1",
        ],
    );
    assert_task_stage(
        &project_root,
        "TASK-REVIEW-ISSUES",
        "IN_PROGRESS",
        "in_progress",
    );

    let _ = running.shutdown.send(());
    let _ = running.join.await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn reviewer_close_verdict_ship_closes_done() {
    let tmp = tempfile::tempdir().unwrap();
    let home = Home::at(tmp.path().join("home"));
    home.ensure().unwrap();
    let project_root = tmp.path().join("project");
    std::fs::create_dir_all(&project_root).unwrap();
    seed_project(&home, &project_root);
    let head = init_git_project(&project_root);
    let bin_dir = tmp.path().join("bin");
    std::fs::create_dir_all(&bin_dir).unwrap();
    write_stub_codex(&bin_dir);
    let path_env = path_with_stub(&bin_dir);
    let codex_dir = tmp.path().join("codex");
    std::fs::create_dir_all(&codex_dir).unwrap();
    let brief = codex_dir.join("task-ship-clean-brief.md");
    write(&brief, "reviewer ship brief");
    let last = codex_dir.join("task-ship-clean-last.txt");
    let worktree = tmp.path().join("worktrees/task-ship-clean");

    let running = boot(home.clone()).await;
    run_orgasmic(
        &home,
        &running,
        &project_root,
        &path_env,
        &[
            "manager",
            "dispatch",
            "--task",
            "TASK-SHIP-CLEAN",
            "--kind",
            "reviewer",
            "--worker",
            "reviewer-codex-acp",
            "--brief",
            brief.to_str().unwrap(),
            "--from",
            &head,
            "--worktree",
            worktree.to_str().unwrap(),
            "--branch",
            "task-ship-clean",
        ],
    );
    let _ = last;
    run_orgasmic(
        &home,
        &running,
        &project_root,
        &path_env,
        &[
            "manager",
            "dispatch-close",
            "--task",
            "TASK-SHIP-CLEAN",
            "--status",
            "done",
            "--property",
            "VERDICT=ship",
            "--property",
            "REPORT_PATH=/tmp/task-ship-clean.md",
            "--property",
            "RECOMMENDED_SUBTASKS=-",
        ],
    );
    assert_task_stage(&project_root, "TASK-SHIP-CLEAN", "DONE", "done");

    let _ = running.shutdown.send(());
    let _ = running.join.await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn reviewer_close_verdict_has_issues_stays_in_progress() {
    let tmp = tempfile::tempdir().unwrap();
    let home = Home::at(tmp.path().join("home"));
    home.ensure().unwrap();
    let project_root = tmp.path().join("project");
    std::fs::create_dir_all(&project_root).unwrap();
    seed_project(&home, &project_root);
    let head = init_git_project(&project_root);
    let bin_dir = tmp.path().join("bin");
    std::fs::create_dir_all(&bin_dir).unwrap();
    write_stub_codex(&bin_dir);
    let path_env = path_with_stub(&bin_dir);
    let codex_dir = tmp.path().join("codex");
    std::fs::create_dir_all(&codex_dir).unwrap();
    let brief = codex_dir.join("task-has-issues-brief.md");
    write(&brief, "reviewer has-issues brief");
    let last = codex_dir.join("task-has-issues-last.txt");
    let worktree = tmp.path().join("worktrees/task-has-issues");

    let running = boot(home.clone()).await;
    run_orgasmic(
        &home,
        &running,
        &project_root,
        &path_env,
        &[
            "manager",
            "dispatch",
            "--task",
            "TASK-HAS-ISSUES",
            "--kind",
            "reviewer",
            "--worker",
            "reviewer-codex-acp",
            "--brief",
            brief.to_str().unwrap(),
            "--from",
            &head,
            "--worktree",
            worktree.to_str().unwrap(),
            "--branch",
            "task-has-issues",
        ],
    );
    let _ = last;
    run_orgasmic(
        &home,
        &running,
        &project_root,
        &path_env,
        &[
            "manager",
            "dispatch-close",
            "--task",
            "TASK-HAS-ISSUES",
            "--status",
            "done",
            "--property",
            "VERDICT=has-issues",
            "--property",
            "REPORT_PATH=/tmp/task-has-issues.md",
        ],
    );
    assert_task_stage(
        &project_root,
        "TASK-HAS-ISSUES",
        "IN_PROGRESS",
        "in_progress",
    );

    let _ = running.shutdown.send(());
    let _ = running.join.await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn multi_task_dispatch_writes_one_start_and_per_task_closes() {
    let tmp = tempfile::tempdir().unwrap();
    let home = Home::at(tmp.path().join("home"));
    home.ensure().unwrap();
    let project_root = tmp.path().join("project");
    std::fs::create_dir_all(&project_root).unwrap();
    seed_project(&home, &project_root);
    let head = init_git_project(&project_root);
    let bin_dir = tmp.path().join("bin");
    std::fs::create_dir_all(&bin_dir).unwrap();
    write_stub_codex(&bin_dir);
    let path_env = path_with_stub(&bin_dir);
    let codex_dir = tmp.path().join("codex");
    std::fs::create_dir_all(&codex_dir).unwrap();
    let brief = codex_dir.join("task-bundle-brief.md");
    write(&brief, "bundle brief");
    let last = codex_dir.join("task-bundle-last.txt");
    let worktree = tmp.path().join("worktrees/task-bundle");

    let running = boot(home.clone()).await;
    let dispatch_stdout = run_orgasmic(
        &home,
        &running,
        &project_root,
        &path_env,
        &[
            "manager",
            "dispatch",
            "--task",
            "TASK-BUNDLE-A",
            "--task",
            "TASK-BUNDLE-B",
            "--kind",
            "implementer",
            "--worker",
            "implementer-codex-appserver",
            "--brief",
            brief.to_str().unwrap(),
            "--from",
            &head,
            "--worktree",
            worktree.to_str().unwrap(),
            "--branch",
            "task-bundle-impl",
        ],
    );
    assert!(dispatch_stdout.contains("dispatched: TASK-BUNDLE-A TASK-BUNDLE-B implementer pid="));
    let _ = last;
    assert_task_stage(&project_root, "TASK-BUNDLE-A", "IN_PROGRESS", "in_progress");
    assert_task_stage(&project_root, "TASK-BUNDLE-B", "IN_PROGRESS", "in_progress");

    let tx_path = tx_file_path(&project_root);
    let tx_raw = std::fs::read_to_string(&tx_path).unwrap();
    assert_eq!(
        count_occurrences(&tx_raw, ":TYPE:         manager.dispatch_started"),
        1
    );
    assert!(tx_raw.contains(":TASK:         TASK-BUNDLE-A TASK-BUNDLE-B"));

    let status_stdout = run_orgasmic(
        &home,
        &running,
        &project_root,
        &path_env,
        &["manager", "dispatch-status", "--task", "TASK-BUNDLE-A"],
    );
    assert!(status_stdout.contains("TASK=TASK-BUNDLE-A TASK-BUNDLE-B"));

    run_orgasmic(
        &home,
        &running,
        &project_root,
        &path_env,
        &[
            "manager",
            "dispatch-close",
            "--task",
            "TASK-BUNDLE-A",
            "--task",
            "TASK-BUNDLE-B",
            "--status",
            "done",
            "--merge-sha",
            &head,
            "--codex-commit",
            &head,
            "--codex-session",
            "session-bundle",
            "--tokens",
            "123",
            "--wall",
            "2s",
        ],
    );
    assert_task_stage(&project_root, "TASK-BUNDLE-A", "IN_REVIEW", "in_review");
    assert_task_stage(&project_root, "TASK-BUNDLE-B", "IN_REVIEW", "in_review");
    let tx_raw = std::fs::read_to_string(&tx_path).unwrap();
    assert_eq!(
        count_occurrences(&tx_raw, ":TYPE:         implementer.done"),
        2
    );
    assert!(tx_raw.contains(":TASK:         TASK-BUNDLE-A"));
    assert!(tx_raw.contains(":TASK:         TASK-BUNDLE-B"));
    assert_eq!(count_occurrences(&tx_raw, ":CLOSED_TX:    "), 2);
    assert_eq!(
        count_occurrences(&tx_raw, ":WORKER_SESSION: session-bundle"),
        2
    );

    let status_stdout = run_orgasmic(
        &home,
        &running,
        &project_root,
        &path_env,
        &["manager", "dispatch-status", "--task", "TASK-BUNDLE-A"],
    );
    assert!(
        status_stdout.trim().is_empty(),
        "bundled close should close the dispatch: {status_stdout}"
    );

    let _ = running.shutdown.send(());
    let _ = running.join.await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn bundled_partial_close_retry_is_idempotent_and_visible() {
    let tmp = tempfile::tempdir().unwrap();
    let home = Home::at(tmp.path().join("home"));
    home.ensure().unwrap();
    let project_root = tmp.path().join("project");
    std::fs::create_dir_all(&project_root).unwrap();
    seed_project(&home, &project_root);
    let head = init_git_project(&project_root);
    let bin_dir = tmp.path().join("bin");
    std::fs::create_dir_all(&bin_dir).unwrap();
    write_stub_codex(&bin_dir);
    let path_env = path_with_stub(&bin_dir);
    let codex_dir = tmp.path().join("codex");
    std::fs::create_dir_all(&codex_dir).unwrap();
    let brief = codex_dir.join("task-bundle-partial-brief.md");
    write(&brief, "bundle partial brief");
    let last = codex_dir.join("task-bundle-partial-last.txt");
    let worktree = tmp.path().join("worktrees/task-bundle-partial");

    let running = boot(home.clone()).await;
    run_orgasmic(
        &home,
        &running,
        &project_root,
        &path_env,
        &[
            "manager",
            "dispatch",
            "--task",
            "TASK-BUNDLE-A",
            "--task",
            "TASK-BUNDLE-B",
            "--kind",
            "implementer",
            "--worker",
            "implementer-codex-appserver",
            "--brief",
            brief.to_str().unwrap(),
            "--from",
            &head,
            "--worktree",
            worktree.to_str().unwrap(),
            "--branch",
            "task-bundle-partial-impl",
        ],
    );
    let _ = last;
    assert_task_stage(&project_root, "TASK-BUNDLE-A", "IN_PROGRESS", "in_progress");
    assert_task_stage(&project_root, "TASK-BUNDLE-B", "IN_PROGRESS", "in_progress");

    let start_tx = tx_id_for(
        &tx_log(&project_root),
        "manager.dispatch_started",
        "TASK-BUNDLE-A TASK-BUNDLE-B",
    );
    run_git(
        &project_root,
        &["worktree", "remove", "--force", worktree.to_str().unwrap()],
    );
    run_git(&project_root, &["branch", "-D", "task-bundle-partial-impl"]);
    append_partial_close_tx(
        &project_root,
        &start_tx,
        "TASK-BUNDLE-A",
        &head,
        "task-bundle-partial-impl",
    );

    let partial_status = run_orgasmic(
        &home,
        &running,
        &project_root,
        &path_env,
        &["manager", "dispatch-status", "--task", "TASK-BUNDLE-A"],
    );
    assert!(partial_status.contains("PARTIAL_CLOSED=1/2 missing=[TASK-BUNDLE-B]"));
    let filtered_status = run_orgasmic(
        &home,
        &running,
        &project_root,
        &path_env,
        &["manager", "dispatch-status", "--partial-closed"],
    );
    assert!(filtered_status.contains("TASK=TASK-BUNDLE-A TASK-BUNDLE-B"));
    assert!(filtered_status.contains("PARTIAL_CLOSED=1/2 missing=[TASK-BUNDLE-B]"));

    let close_stdout = run_orgasmic(
        &home,
        &running,
        &project_root,
        &path_env,
        &[
            "manager",
            "dispatch-close",
            "--task",
            "TASK-BUNDLE-A",
            "--task",
            "TASK-BUNDLE-B",
            "--status",
            "done",
            "--merge-sha",
            &head,
            "--codex-commit",
            &head,
            "--codex-session",
            "session-bundle-partial",
            "--tokens",
            "456",
            "--wall",
            "3s",
            "--branch-delete",
        ],
    );
    assert!(close_stdout.contains("closed: TASK-BUNDLE-A TASK-BUNDLE-B implementer.done tx="));
    assert_task_stage(&project_root, "TASK-BUNDLE-A", "IN_REVIEW", "in_review");
    assert_task_stage(&project_root, "TASK-BUNDLE-B", "IN_REVIEW", "in_review");

    let tx_raw = tx_log(&project_root);
    assert_eq!(
        count_occurrences(&tx_raw, ":TYPE:         implementer.done"),
        2
    );
    assert!(tx_raw.contains(":TASK:         TASK-BUNDLE-A"));
    assert!(tx_raw.contains(":TASK:         TASK-BUNDLE-B"));
    assert!(tx_raw.contains(":CLEANUP_STATUS: cleanup_already_run"));
    assert!(!tx_raw.contains(":CLEANUP_STATUS: worktree_failed"));
    assert!(!tx_raw.contains(":CLEANUP_STATUS: branch_failed"));

    let status_after = run_orgasmic(
        &home,
        &running,
        &project_root,
        &path_env,
        &["manager", "dispatch-status", "--task", "TASK-BUNDLE-A"],
    );
    assert!(
        status_after.trim().is_empty(),
        "completed retry should close dispatch: {status_after}"
    );
    let cleanup_failed = run_orgasmic(
        &home,
        &running,
        &project_root,
        &path_env,
        &["manager", "dispatch-status", "--cleanup-failed"],
    );
    assert!(
        cleanup_failed.trim().is_empty(),
        "cleanup_already_run should not be reported as a failure: {cleanup_failed}"
    );

    let _ = running.shutdown.send(());
    let _ = running.join.await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn dispatch_close_records_cleanup_failure_and_status_filter_lists_it() {
    let tmp = tempfile::tempdir().unwrap();
    let home = Home::at(tmp.path().join("home"));
    home.ensure().unwrap();
    let project_root = tmp.path().join("project");
    std::fs::create_dir_all(&project_root).unwrap();
    seed_project(&home, &project_root);
    let head = init_git_project(&project_root);
    let bin_dir = tmp.path().join("bin");
    std::fs::create_dir_all(&bin_dir).unwrap();
    write_stub_codex(&bin_dir);
    let path_env = path_with_stub(&bin_dir);
    let codex_dir = tmp.path().join("codex");
    std::fs::create_dir_all(&codex_dir).unwrap();
    let brief = codex_dir.join("task-cleanup-brief.md");
    write(&brief, "cleanup brief");
    let last = codex_dir.join("task-cleanup-last.txt");
    let worktree = tmp.path().join("worktrees/task-cleanup");

    let running = boot(home.clone()).await;
    run_orgasmic(
        &home,
        &running,
        &project_root,
        &path_env,
        &[
            "manager",
            "dispatch",
            "--task",
            "TASK-CLEANUP",
            "--kind",
            "implementer",
            "--worker",
            "implementer-codex-appserver",
            "--brief",
            brief.to_str().unwrap(),
            "--from",
            &head,
            "--worktree",
            worktree.to_str().unwrap(),
            "--branch",
            "task-cleanup-impl",
        ],
    );
    let _ = last;
    std::fs::remove_dir_all(&worktree).unwrap();
    let output = run_orgasmic_output(
        &home,
        &running,
        &project_root,
        &path_env,
        &[
            "manager",
            "dispatch-close",
            "--task",
            "TASK-CLEANUP",
            "--status",
            "done",
            "--merge-sha",
            &head,
        ],
    );
    assert!(
        output.status.success(),
        "cleanup failure close should still append tx\nstdout={}\nstderr={}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("warning: dispatch cleanup status=worktree_failed"));
    assert_task_stage(&project_root, "TASK-CLEANUP", "IN_REVIEW", "in_review");

    let tx_raw = std::fs::read_to_string(tx_file_path(&project_root)).unwrap();
    assert!(tx_raw.contains(":CLEANUP_STATUS: worktree_failed"));
    assert!(tx_raw.contains(":CLEANUP_ERROR:"));
    let cleanup_status = run_orgasmic(
        &home,
        &running,
        &project_root,
        &path_env,
        &["manager", "dispatch-status", "--cleanup-failed"],
    );
    assert!(cleanup_status.contains("TASK=TASK-CLEANUP"));
    assert!(cleanup_status.contains("CLEANUP_STATUS=worktree_failed"));

    let _ = running.shutdown.send(());
    let _ = running.join.await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn dispatch_status_matches_pid_by_last_message_path() {
    let tmp = tempfile::tempdir().unwrap();
    let home = Home::at(tmp.path().join("home"));
    home.ensure().unwrap();
    let project_root = tmp.path().join("project");
    std::fs::create_dir_all(&project_root).unwrap();
    seed_project(&home, &project_root);
    init_git_project(&project_root);
    let bin_dir = tmp.path().join("bin");
    std::fs::create_dir_all(&bin_dir).unwrap();
    let stub = write_sleeping_stub_codex(&bin_dir);
    let path_env = path_with_stub(&bin_dir);
    let codex_dir = tmp.path().join("codex");
    std::fs::create_dir_all(&codex_dir).unwrap();
    let implementer_brief = codex_dir.join("task-dispatch-brief.md");
    let reviewer_brief = codex_dir.join("task-dispatch-review-brief.md");
    write(&implementer_brief, "implementer brief");
    write(&reviewer_brief, "reviewer brief");
    let implementer_last = codex_dir.join("task-dispatch-last.txt");
    let reviewer_last = codex_dir.join("task-dispatch-review-last.txt");
    let implementer_worktree = tmp.path().join("worktrees/task-dispatch");
    let reviewer_worktree = tmp.path().join("worktrees/task-dispatch-review");
    std::fs::create_dir_all(&implementer_worktree).unwrap();
    std::fs::create_dir_all(&reviewer_worktree).unwrap();
    write(
        &tx_file_path(&project_root),
        format!(
            "#+title: tx\n#+orgasmic_version: 1\n\n* TX 2026-05-23 Sat 10:00:00 manager.dispatch_started TASK-DISPATCH\n:PROPERTIES:\n:TX_ID:        tx-start-impl\n:TIME:         [2026-05-23 Sat 10:00:00]\n:TYPE:         manager.dispatch_started\n:ACTOR:        a@example.com\n:MACHINE:      host\n:PROJECT:      orgasmic\n:TASK:         TASK-DISPATCH\n:KIND:         implementer\n:WORKTREE:     {}\n:BRANCH:       task-dispatch-impl\n:CODEX_BRIEF_PATH: {}\n:CODEX_MODEL:  gpt-5.5\n:CODEX_EFFORT: high\n:STARTED_AT:   [2026-05-23 Sat 10:00:00]\n:END:\n\n* TX 2026-05-23 Sat 10:05:00 manager.dispatch_started TASK-DISPATCH\n:PROPERTIES:\n:TX_ID:        tx-start-review\n:TIME:         [2026-05-23 Sat 10:05:00]\n:TYPE:         manager.dispatch_started\n:ACTOR:        a@example.com\n:MACHINE:      host\n:PROJECT:      orgasmic\n:TASK:         TASK-DISPATCH\n:KIND:         reviewer\n:WORKTREE:     {}\n:BRANCH:       task-dispatch-review\n:CODEX_BRIEF_PATH: {}\n:CODEX_MODEL:  gpt-5.5\n:CODEX_EFFORT: high\n:STARTED_AT:   [2026-05-23 Sat 10:05:00]\n:END:\n",
            implementer_worktree.display(),
            implementer_brief.display(),
            reviewer_worktree.display(),
            reviewer_brief.display()
        ),
    );

    let mut implementer_child = Command::new(&stub)
        .arg("exec")
        .arg("--output-last-message")
        .arg(&implementer_last)
        .arg("TASK-DISPATCH implementer")
        .spawn()
        .expect("spawn implementer stub");
    let mut reviewer_child = Command::new(&stub)
        .arg("exec")
        .arg("--output-last-message")
        .arg(&reviewer_last)
        .arg("TASK-DISPATCH reviewer")
        .spawn()
        .expect("spawn reviewer stub");
    wait_for_file(&implementer_last);
    wait_for_file(&reviewer_last);

    let running = boot(home.clone()).await;
    let output = run_orgasmic_output(
        &home,
        &running,
        &project_root,
        &path_env,
        &["manager", "dispatch-status", "--task", "TASK-DISPATCH"],
    );
    let _ = implementer_child.kill();
    let _ = reviewer_child.kill();
    let _ = implementer_child.wait();
    let _ = reviewer_child.wait();
    let _ = running.shutdown.send(());
    let _ = running.join.await;
    assert!(
        output.status.success(),
        "dispatch-status failed\nstdout={}\nstderr={}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    let status_stdout = String::from_utf8_lossy(&output.stdout);
    let implementer_line = status_stdout
        .lines()
        .find(|line| line.contains("KIND=implementer"))
        .expect("implementer status line");
    let reviewer_line = status_stdout
        .lines()
        .find(|line| line.contains("KIND=reviewer"))
        .expect("reviewer status line");
    assert!(
        implementer_line.contains(&format!("WORKER_PID={} (derived)", implementer_child.id())),
        "implementer line has wrong pid: {implementer_line}"
    );
    assert!(
        reviewer_line.contains(&format!("WORKER_PID={} (derived)", reviewer_child.id())),
        "reviewer line has wrong pid: {reviewer_line}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn dispatch_worker_flag_shows_in_dry_run_plan() {
    let tmp = tempfile::tempdir().unwrap();
    let home = Home::at(tmp.path().join("home"));
    home.ensure().unwrap();
    let project_root = tmp.path().join("project");
    std::fs::create_dir_all(&project_root).unwrap();
    seed_project(&home, &project_root);
    init_git_project(&project_root);
    let bin_dir = tmp.path().join("bin");
    std::fs::create_dir_all(&bin_dir).unwrap();
    write_stub_codex(&bin_dir);
    let path_env = path_with_stub(&bin_dir);
    let codex_dir = tmp.path().join("codex");
    std::fs::create_dir_all(&codex_dir).unwrap();
    let brief = codex_dir.join("task-worker-flag-brief.md");
    write(&brief, "worker flag brief");

    let running = boot(home.clone()).await;
    let stdout = run_orgasmic(
        &home,
        &running,
        &project_root,
        &path_env,
        &[
            "manager",
            "dispatch",
            "--task",
            "TASK-DISPATCH",
            "--kind",
            "implementer",
            "--brief",
            brief.to_str().unwrap(),
            "--worker",
            "implementer-codex-stdio",
            "--dry-run",
        ],
    );
    assert!(stdout.contains("dispatch plan:"));
    assert!(stdout.contains("worker:   implementer-codex-stdio"));

    let _ = running.shutdown.send(());
    let _ = running.join.await;
}

/// Regression for TASK-096.1: dispatching one kind with a `--worktree` that
/// equals another kind's *default* worktree path for the same task must bail.
/// The collision check lives in `cmd_dispatch` (manager.rs) and fires before the
/// `--dry-run` early return, so a dry-run dispatch is sufficient to exercise the
/// bail without spawning a worker or touching the worktree on disk.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn dispatch_rejects_cross_kind_default_worktree_reuse() {
    let tmp = tempfile::tempdir().unwrap();
    let home = Home::at(tmp.path().join("home"));
    home.ensure().unwrap();
    let project_root = tmp.path().join("project");
    std::fs::create_dir_all(&project_root).unwrap();
    seed_project(&home, &project_root);
    init_git_project(&project_root);
    let codex_dir = tmp.path().join("codex");
    std::fs::create_dir_all(&codex_dir).unwrap();
    let brief = codex_dir.join("task-collision-brief.md");
    write(&brief, "collision regression brief");
    // Preserve the system PATH so `git` (used by build_dispatch_plan to resolve
    // --from/HEAD) is found; no stub codex is needed because the dry-run bails
    // before any worker spawn.
    let bin_dir = tmp.path().join("bin");
    std::fs::create_dir_all(&bin_dir).unwrap();
    let path_env = path_with_stub(&bin_dir);

    let running = boot(home.clone()).await;

    // Default worktree suffixes mirror `default_worktree` in manager.rs.
    // `TASK-DISPATCH` is BACKLOG (dispatchable as implementer/architector);
    // `TASK-REVIEW-ISSUES` is IN_REVIEW (dispatchable as reviewer).
    let dispatch_impl_default = project_root
        .join(".orgasmic/tmp/dispatch/task-dispatch/worktree")
        .display()
        .to_string();
    let dispatch_review_default = project_root
        .join(".orgasmic/tmp/dispatch/task-dispatch-review/worktree")
        .display()
        .to_string();
    let review_issues_impl_default = project_root
        .join(".orgasmic/tmp/dispatch/task-review-issues/worktree")
        .display()
        .to_string();

    // (task, kind, colliding --worktree, expected substring) — exhausts the
    // cross-kind matrix the collision loop guards.
    let cases: &[(&str, &str, &str, &str)] = &[
        (
            "TASK-DISPATCH",
            "architector",
            dispatch_impl_default.as_str(),
            "architector worktree must not reuse implementer default path:",
        ),
        (
            "TASK-DISPATCH",
            "architector",
            dispatch_review_default.as_str(),
            "architector worktree must not reuse reviewer default path:",
        ),
        (
            "TASK-REVIEW-ISSUES",
            "reviewer",
            review_issues_impl_default.as_str(),
            "reviewer worktree must not reuse implementer default path:",
        ),
    ];

    for (task, kind, worktree, expected) in cases {
        let worker = match *kind {
            "reviewer" => "reviewer-codex-acp",
            "architector" => "architector-test",
            _ => "implementer-codex-appserver",
        };
        let stderr = run_orgasmic_failure(
            &home,
            &running,
            &project_root,
            &path_env,
            &[
                "manager",
                "dispatch",
                "--task",
                task,
                "--kind",
                kind,
                "--worker",
                worker,
                "--brief",
                brief.to_str().unwrap(),
                "--worktree",
                worktree,
                "--dry-run",
            ],
        );
        assert!(
            stderr.contains(expected),
            "kind={kind} task={task} worktree={worktree}: expected error containing {expected:?}\nstderr={stderr}"
        );
    }

    // The colliding dispatches must not have created any worktree (dry-run +
    // bail before worktree creation).
    for worktree in [
        &dispatch_impl_default,
        &dispatch_review_default,
        &review_issues_impl_default,
    ] {
        assert!(
            !Path::new(worktree.as_str()).exists(),
            "collision bail must not create worktree {worktree}"
        );
    }

    let _ = running.shutdown.send(());
    let _ = running.join.await;
}

/// Regression for TASK-WTJ5V: when the CLI dispatch HTTP client times out after
/// the daemon has already spawned the worker, rollback must be daemon-executed
/// (release worker, then delete worktree + branch) — never CLI-local worktree
/// deletion racing a live worker.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn dispatch_timeout_requests_daemon_cleanup() {
    let tmp = tempfile::tempdir().unwrap();
    let home = Home::at(tmp.path().join("home"));
    home.ensure().unwrap();
    let project_root = tmp.path().join("project");
    std::fs::create_dir_all(&project_root).unwrap();
    seed_project(&home, &project_root);
    let head = init_git_project(&project_root);
    let bin_dir = tmp.path().join("bin");
    std::fs::create_dir_all(&bin_dir).unwrap();
    write_git_proxy(&bin_dir);
    write_sleeping_stub_codex(&bin_dir);
    let path_env = path_with_stub(&bin_dir);
    let stem_dir = project_root.join(".orgasmic/tmp/dispatch/task-dispatch");
    std::fs::create_dir_all(&stem_dir).unwrap();
    let brief = stem_dir.join("task-dispatch-brief.md");
    write(&brief, "timeout regression brief");
    let branch = "task-dispatch-impl";
    let worktree = stem_dir.join("worktree");

    let running = boot_with_options(
        home.clone(),
        DaemonOptions {
            dispatch_response_delay: Some(Duration::from_secs(3)),
            ..test_options()
        },
    )
    .await;

    let output = run_orgasmic_output_with_env(
        &home,
        &running,
        &project_root,
        &path_env,
        &[
            "manager",
            "dispatch",
            "--task",
            "TASK-DISPATCH",
            "--kind",
            "implementer",
            "--worker",
            "implementer-codex-appserver",
            "--brief",
            brief.to_str().unwrap(),
            "--from",
            &head,
        ],
        &[("ORGASMIC_DISPATCH_HTTP_TIMEOUT_SECS", "1")],
    );
    assert!(
        !output.status.success(),
        "dispatch should fail on timeout\nstdout={}\nstderr={}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("daemon dispatch failed"),
        "unexpected stderr: {stderr}"
    );

    assert_task_stage(&project_root, "TASK-DISPATCH", "BACKLOG", "backlog");
    assert!(
        !worktree.exists(),
        "daemon cleanup should remove worktree after CLI timeout"
    );
    assert!(
        !branch_exists(&project_root, branch),
        "daemon cleanup should remove branch after CLI timeout"
    );

    let lease_output = run_orgasmic_output(
        &home,
        &running,
        &project_root,
        &path_env,
        &[
            "manager",
            "lease-release",
            "--task",
            "TASK-DISPATCH",
            "--kind",
            "implementer",
        ],
    );
    assert!(
        lease_output.status.success(),
        "lease-release should succeed after daemon cleanup\nstdout={}\nstderr={}",
        String::from_utf8_lossy(&lease_output.stdout),
        String::from_utf8_lossy(&lease_output.stderr)
    );
    let lease_stdout = String::from_utf8_lossy(&lease_output.stdout);
    assert!(
        lease_stdout.contains("no lease held"),
        "cleanup should clear the supervisor lease: {lease_stdout}"
    );

    let _ = running.shutdown.send(());
    let _ = running.join.await;
}

/// Default dispatch worktrees live under `<project>/.orgasmic/tmp/dispatch/<stem>/worktree`.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn dispatch_default_worktree_lives_under_project_dispatch_dir() {
    let tmp = tempfile::tempdir().unwrap();
    let home = Home::at(tmp.path().join("home"));
    home.ensure().unwrap();
    let project_root = tmp.path().join("project");
    std::fs::create_dir_all(&project_root).unwrap();
    seed_project(&home, &project_root);
    let head = init_git_project(&project_root);
    let bin_dir = tmp.path().join("bin");
    std::fs::create_dir_all(&bin_dir).unwrap();
    let path_env = path_with_stub(&bin_dir);
    let brief = project_root.join(".orgasmic/tmp/dispatch/task-dispatch/task-dispatch-brief.md");
    write(&brief, "default worktree layout brief");

    let running = boot(home.clone()).await;
    let stdout = run_orgasmic(
        &home,
        &running,
        &project_root,
        &path_env,
        &[
            "manager",
            "dispatch",
            "--task",
            "TASK-DISPATCH",
            "--kind",
            "implementer",
            "--worker",
            "implementer-codex-appserver",
            "--brief",
            brief.to_str().unwrap(),
            "--from",
            &head,
            "--dry-run",
        ],
    );
    let expected = project_root
        .join(".orgasmic/tmp/dispatch/task-dispatch/worktree")
        .display()
        .to_string();
    assert!(
        stdout.contains(&expected),
        "dry-run should show project-local default worktree, got:\n{stdout}"
    );

    let _ = running.shutdown.send(());
    let _ = running.join.await;
}

/// A live default-path worktree must not dirty `git status` in the parent repo.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn dispatch_default_worktree_keeps_parent_git_status_clean() {
    let tmp = tempfile::tempdir().unwrap();
    let home = Home::at(tmp.path().join("home"));
    home.ensure().unwrap();
    let project_root = tmp.path().join("project");
    std::fs::create_dir_all(&project_root).unwrap();
    seed_project(&home, &project_root);
    write(&project_root.join(".orgasmic/.gitignore"), "tmp/\n");
    let head = init_git_project(&project_root);
    let bin_dir = tmp.path().join("bin");
    std::fs::create_dir_all(&bin_dir).unwrap();
    write_stub_codex(&bin_dir);
    let path_env = path_with_stub(&bin_dir);
    let brief = project_root.join(".orgasmic/tmp/dispatch/task-dispatch/task-dispatch-brief.md");
    write(&brief, "git status regression brief");

    let running = boot(home.clone()).await;
    let _ = run_orgasmic(
        &home,
        &running,
        &project_root,
        &path_env,
        &[
            "manager",
            "dispatch",
            "--task",
            "TASK-DISPATCH",
            "--kind",
            "implementer",
            "--worker",
            "implementer-codex-appserver",
            "--brief",
            brief.to_str().unwrap(),
            "--from",
            &head,
            "--reason",
            "layout regression",
        ],
    );
    let worktree = project_root.join(".orgasmic/tmp/dispatch/task-dispatch/worktree");
    assert!(worktree.is_dir(), "default worktree should exist");

    let status = Command::new("git")
        .args(["status", "--porcelain"])
        .current_dir(&project_root)
        .output()
        .expect("git status");
    assert!(
        status.status.success(),
        "git status failed: {}",
        String::from_utf8_lossy(&status.stderr)
    );
    let porcelain = String::from_utf8_lossy(&status.stdout);
    assert!(
        !porcelain.contains(".orgasmic/tmp/dispatch"),
        "dispatch worktree under .orgasmic/tmp must stay gitignored; git status:\n{porcelain}"
    );

    let _ = running.shutdown.send(());
    let _ = running.join.await;
}

/// Closing a dispatch removes transient stem artifacts but retains the brief.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn dispatch_close_prunes_stem_dir_leaving_brief() {
    let _live_guard = live_session_guard();
    let tmp = tempfile::tempdir().unwrap();
    let home = Home::at(tmp.path().join("home"));
    home.ensure().unwrap();
    let project_root = tmp.path().join("project");
    std::fs::create_dir_all(&project_root).unwrap();
    seed_project(&home, &project_root);
    let head = init_git_project(&project_root);
    let bin_dir = tmp.path().join("bin");
    std::fs::create_dir_all(&bin_dir).unwrap();
    write_stub_codex(&bin_dir);
    let path_env = path_with_stub(&bin_dir);
    let stem_dir = project_root.join(".orgasmic/tmp/dispatch/task-dispatch");
    let brief = stem_dir.join("task-dispatch-brief.md");
    write(&brief, "stem cleanup brief");

    let running = boot(home.clone()).await;
    let _ = run_orgasmic(
        &home,
        &running,
        &project_root,
        &path_env,
        &[
            "manager",
            "dispatch",
            "--task",
            "TASK-DISPATCH",
            "--kind",
            "implementer",
            "--worker",
            "implementer-codex-appserver",
            "--brief",
            brief.to_str().unwrap(),
            "--from",
            &head,
            "--reason",
            "stem cleanup regression",
        ],
    );
    let worktree = stem_dir.join("worktree");
    assert!(worktree.is_dir());
    let tx_raw = tx_log(&project_root);
    let attempt_last = resolve_project_path(
        &project_root,
        &tx_property_for(&tx_raw, "run.created", "TASK-DISPATCH", "LAST_PATH"),
    );
    let attempt_stdout = resolve_project_path(
        &project_root,
        &tx_property_for(&tx_raw, "run.created", "TASK-DISPATCH", "STDOUT_PATH"),
    );
    write(&attempt_last, "worker summary");
    write(&attempt_stdout, "worker stdout");
    let sibling_last = stem_dir.join("task-dispatch-attempt2-last.txt");
    let sibling_stdout = stem_dir.join("task-dispatch-attempt2-stdout.log");
    let legacy_last = stem_dir.join("task-dispatch-last.txt");
    let legacy_stdout = stem_dir.join("task-dispatch-stdout.log");
    write(&sibling_last, "sibling attempt report");
    write(&sibling_stdout, "sibling attempt stdout");
    write(&legacy_last, "legacy summary");
    write(&legacy_stdout, "legacy stdout");

    let _ = run_orgasmic(
        &home,
        &running,
        &project_root,
        &path_env,
        &[
            "manager",
            "dispatch-close",
            "--task",
            "TASK-DISPATCH",
            "--status",
            "done",
            "--merge-sha",
            &head,
            "--codex-commit",
            &head,
            "--reason",
            "stub landed",
        ],
    );

    assert!(!worktree.exists(), "worktree should be removed on close");
    assert!(
        !attempt_last.exists(),
        "selected attempt last.txt should be pruned on close"
    );
    assert!(
        !attempt_stdout.exists(),
        "selected attempt stdout.log should be pruned on close"
    );
    assert!(
        sibling_last.exists(),
        "sibling attempt last.txt must survive close"
    );
    assert!(
        sibling_stdout.exists(),
        "sibling attempt stdout.log must survive close"
    );
    assert!(
        legacy_last.exists(),
        "legacy last.txt must survive when not selected"
    );
    assert!(
        legacy_stdout.exists(),
        "legacy stdout.log must survive when not selected"
    );
    assert!(brief.is_file(), "brief should be retained after close");

    let _ = running.shutdown.send(());
    let _ = running.join.await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn dispatch_close_fails_when_liveness_probe_unreachable() {
    // Review finding (ZD72S/BRXGG reviewer pass): a failed /runs liveness
    // probe must fail the close, not read as "no live runs" — otherwise the
    // close prunes worktree artifacts under a possibly still-live worker.
    let tmp = tempfile::tempdir().unwrap();
    let home = Home::at(tmp.path().join("home"));
    home.ensure().unwrap();
    let project_root = tmp.path().join("project");
    std::fs::create_dir_all(&project_root).unwrap();
    seed_project(&home, &project_root);
    init_git_project(&project_root);
    let bin_dir = tmp.path().join("bin");
    std::fs::create_dir_all(&bin_dir).unwrap();
    write_stub_codex(&bin_dir);
    let path_env = path_with_stub(&bin_dir);
    let worktree = tmp.path().join("worktrees/task-cleanup");
    std::fs::create_dir_all(&worktree).unwrap();
    write(&worktree.join("marker.txt"), "worker artifacts live here");
    let brief = tmp.path().join("codex/task-cleanup-brief.md");
    write(&brief, "cleanup brief");
    write(
        &tx_file_path(&project_root),
        format!(
            "#+title: tx\n#+orgasmic_version: 1\n\n* TX 2026-05-23 Sat 10:00:00 manager.dispatch_started TASK-CLEANUP\n:PROPERTIES:\n:TX_ID:        tx-start-cleanup\n:TIME:         [2026-05-23 Sat 10:00:00]\n:TYPE:         manager.dispatch_started\n:ACTOR:        a@example.com\n:MACHINE:      host\n:PROJECT:      orgasmic\n:TASK:         TASK-CLEANUP\n:KIND:         implementer\n:WORKTREE:     {}\n:BRANCH:       task-cleanup-impl\n:CODEX_BRIEF_PATH: {}\n:STARTED_AT:   [2026-05-23 Sat 10:00:00]\n:END:\n",
            worktree.display(),
            brief.display()
        ),
    );

    let running = boot(home.clone()).await;
    // Point the close at a dead daemon so the /runs liveness probe errors.
    let output = run_orgasmic_output_with_env(
        &home,
        &running,
        &project_root,
        &path_env,
        &[
            "manager",
            "dispatch-close",
            "--task",
            "TASK-CLEANUP",
            "--status",
            "done",
            "--merge-sha",
            "deadbeef",
        ],
        &[("ORGASMIC_DAEMON_URL", "http://127.0.0.1:1")],
    );
    assert!(
        !output.status.success(),
        "dispatch-close must fail when the liveness probe is unreachable\nstdout={}\nstderr={}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("liveness check before dispatch-close cleanup"),
        "close failure should name the liveness probe, got stderr={stderr}"
    );
    assert!(
        worktree.join("marker.txt").is_file(),
        "worktree artifacts must survive a failed liveness probe"
    );

    let _ = running.shutdown.send(());
    let _ = running.join.await;
}

/// Extract the `last=<path>` suffix `orgasmic dispatch finalize` prints on
/// success (see `cmd_dispatch_finalize`'s `println!`).
fn finalized_last_path(stdout: &str) -> PathBuf {
    let line = stdout
        .lines()
        .find(|line| line.starts_with("finalized:"))
        .unwrap_or_else(|| panic!("no `finalized:` line in stdout: {stdout}"));
    let marker = "last=";
    let idx = line
        .rfind(marker)
        .unwrap_or_else(|| panic!("no `last=` in finalize output: {line}"));
    PathBuf::from(line[idx + marker.len()..].trim())
}

/// Dispatch a sleeping-stub implementer run for TASK-DISPATCH and return its
/// worktree path. The stub sleeps 60s so the run stays live in the
/// supervisor while the test drives `orgasmic dispatch finalize` against it
/// from inside the worktree, mirroring a real worker's terminal call.
async fn dispatch_sleeping_implementer(
    home: &Home,
    running: &RunningDaemon,
    project_root: &Path,
    path_env: &std::ffi::OsString,
    head: &str,
    worktree: &Path,
    brief: &Path,
) {
    write(brief, "stub implementer brief");
    let dispatch_stdout = run_orgasmic(
        home,
        running,
        project_root,
        path_env,
        &[
            "manager",
            "dispatch",
            "--task",
            "TASK-DISPATCH",
            "--kind",
            "implementer",
            "--worker",
            "implementer-codex-appserver",
            "--brief",
            brief.to_str().unwrap(),
            "--from",
            head,
            "--worktree",
            worktree.to_str().unwrap(),
            "--branch",
            "task-dispatch-test-impl",
            "--reason",
            "finalize smoke",
        ],
    );
    assert!(dispatch_stdout.contains("dispatched: TASK-DISPATCH implementer pid="));
    assert!(worktree.is_dir(), "worktree should exist");
}

/// Acceptance #1 (TASK-WFW1N): `orgasmic dispatch finalize` writes last.txt
/// byte-verbatim from `--summary-file`, never scraped scrollback. The
/// summary content deliberately looks nothing like driver output (mixed
/// line endings, trailing whitespace, no trailing newline, a unicode
/// marker) so any transformation or scrape contamination would be visible.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn dispatch_finalize_writes_last_txt_verbatim_no_scrollback_contamination() {
    let _live_guard = live_session_guard();
    let tmp = tempfile::tempdir().unwrap();
    let home = Home::at(tmp.path().join("home"));
    home.ensure().unwrap();
    let project_root = tmp.path().join("project");
    std::fs::create_dir_all(&project_root).unwrap();
    seed_project(&home, &project_root);
    let head = init_git_project(&project_root);
    let bin_dir = tmp.path().join("bin");
    std::fs::create_dir_all(&bin_dir).unwrap();
    write_sleeping_stub_codex(&bin_dir);
    let path_env = path_with_stub(&bin_dir);
    let brief = tmp.path().join("codex/task-dispatch-brief.md");
    let worktree = tmp.path().join("worktrees/task-dispatch");

    let running = boot(home.clone()).await;
    dispatch_sleeping_implementer(
        &home,
        &running,
        &project_root,
        &path_env,
        &head,
        &worktree,
        &brief,
    )
    .await;

    let summary_path = tmp.path().join("summary.md");
    let summary_content =
        "## Report\r\nline one\nline two with trailing spaces   \n\nVERBATIM-MARKER-\u{1f525}: DONE (no trailing newline)";
    write(&summary_path, summary_content);

    let finalize_stdout = run_orgasmic(
        &home,
        &running,
        &worktree,
        &path_env,
        &[
            "dispatch",
            "finalize",
            "--task",
            "TASK-DISPATCH",
            "--summary-file",
            summary_path.to_str().unwrap(),
        ],
    );
    assert!(
        finalize_stdout.contains("finalized: TASK-DISPATCH implementer.done tx="),
        "unexpected finalize output: {finalize_stdout}"
    );
    let last_path = finalized_last_path(&finalize_stdout);
    let last_bytes =
        std::fs::read(&last_path).unwrap_or_else(|e| panic!("read {}: {e}", last_path.display()));
    assert_eq!(
        last_bytes,
        summary_content.as_bytes(),
        "last.txt must be byte-verbatim from --summary-file, no scrollback contamination"
    );

    let _ = running.shutdown.send(());
    let _ = running.join.await;
}

/// Acceptance #2 (TASK-WFW1N): `--commit` on a dirty worktree produces the
/// worktree commit as part of finalize, so a finalize with uncommitted
/// changes leaves a clean, committed worktree — commit-stall is
/// structurally impossible.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn dispatch_finalize_commit_flag_leaves_clean_committed_worktree() {
    let _live_guard = live_session_guard();
    let tmp = tempfile::tempdir().unwrap();
    let home = Home::at(tmp.path().join("home"));
    home.ensure().unwrap();
    let project_root = tmp.path().join("project");
    std::fs::create_dir_all(&project_root).unwrap();
    seed_project(&home, &project_root);
    let head = init_git_project(&project_root);
    let bin_dir = tmp.path().join("bin");
    std::fs::create_dir_all(&bin_dir).unwrap();
    write_sleeping_stub_codex(&bin_dir);
    let path_env = path_with_stub(&bin_dir);
    let brief = tmp.path().join("codex/task-dispatch-brief.md");
    let worktree = tmp.path().join("worktrees/task-dispatch");

    let running = boot(home.clone()).await;
    dispatch_sleeping_implementer(
        &home,
        &running,
        &project_root,
        &path_env,
        &head,
        &worktree,
        &brief,
    )
    .await;

    // Simulate uncommitted worker changes left in the worktree.
    write(&worktree.join("NOTES.md"), "uncommitted worker output\n");
    let dirty_status = run_git(&worktree, &["status", "--porcelain"]);
    assert!(
        !dirty_status.is_empty(),
        "worktree should be dirty before finalize"
    );
    let head_before = run_git(&worktree, &["rev-parse", "HEAD"]);

    let summary_path = tmp.path().join("summary.md");
    write(&summary_path, "commit-stall regression check");

    let finalize_stdout = run_orgasmic(
        &home,
        &running,
        &worktree,
        &path_env,
        &[
            "dispatch",
            "finalize",
            "--task",
            "TASK-DISPATCH",
            "--summary-file",
            summary_path.to_str().unwrap(),
            "--commit",
        ],
    );
    assert!(
        finalize_stdout.contains("finalized: TASK-DISPATCH implementer.done tx="),
        "unexpected finalize output: {finalize_stdout}"
    );

    let clean_status = run_git(&worktree, &["status", "--porcelain"]);
    assert!(
        clean_status.is_empty(),
        "worktree must be clean after --commit: {clean_status}"
    );
    let head_after = run_git(&worktree, &["rev-parse", "HEAD"]);
    assert_ne!(
        head_before, head_after,
        "--commit must produce a new commit when the worktree was dirty"
    );

    let tx_raw = tx_log(&project_root);
    assert!(
        tx_raw
            .lines()
            .any(|line| line.trim_start().starts_with(":SHA:") && line.contains(&head_after)),
        "tx should capture the sha --commit produced: {tx_raw}"
    );
    assert!(
        tx_raw
            .lines()
            .any(|line| line.trim_start().starts_with(":MERGE_SHA:") && line.contains(&head_after)),
        "implementer.done tx should carry MERGE_SHA: {tx_raw}"
    );

    let _ = running.shutdown.send(());
    let _ = running.join.await;
}

/// Acceptance #3 (TASK-WFW1N): finalize emits the correct terminal tx
/// (`implementer.done`/`reviewer.done`), releases the lease, and
/// `manager dispatch-status` shows the run closed. Also proves the lease
/// itself (not just the tx record) is released: a second dispatch against
/// the same task+kind after finalize is accepted rather than rejected as
/// overlapping.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn dispatch_finalize_emits_terminal_tx_and_releases_lease() {
    let _live_guard = live_session_guard();
    let tmp = tempfile::tempdir().unwrap();
    let home = Home::at(tmp.path().join("home"));
    home.ensure().unwrap();
    let project_root = tmp.path().join("project");
    std::fs::create_dir_all(&project_root).unwrap();
    seed_project(&home, &project_root);
    let head = init_git_project(&project_root);
    let bin_dir = tmp.path().join("bin");
    std::fs::create_dir_all(&bin_dir).unwrap();
    write_sleeping_stub_codex(&bin_dir);
    let path_env = path_with_stub(&bin_dir);
    let brief = tmp.path().join("codex/task-dispatch-brief.md");
    let worktree = tmp.path().join("worktrees/task-dispatch");

    let running = boot(home.clone()).await;
    dispatch_sleeping_implementer(
        &home,
        &running,
        &project_root,
        &path_env,
        &head,
        &worktree,
        &brief,
    )
    .await;

    let summary_path = tmp.path().join("summary.md");
    write(&summary_path, "implementer finalize smoke");
    let finalize_stdout = run_orgasmic(
        &home,
        &running,
        &worktree,
        &path_env,
        &[
            "dispatch",
            "finalize",
            "--task",
            "TASK-DISPATCH",
            "--summary-file",
            summary_path.to_str().unwrap(),
        ],
    );
    assert!(
        finalize_stdout.contains("finalized: TASK-DISPATCH implementer.done tx="),
        "unexpected finalize output: {finalize_stdout}"
    );

    let tx_raw = tx_log(&project_root);
    assert!(tx_raw.contains(":TYPE:         implementer.done"));
    assert!(tx_raw.contains(":TASK:         TASK-DISPATCH"));

    let status_stdout = run_orgasmic(
        &home,
        &running,
        &project_root,
        &path_env,
        &["manager", "dispatch-status", "--task", "TASK-DISPATCH"],
    );
    assert!(
        status_stdout.trim().is_empty(),
        "finalized dispatch should not appear as open in dispatch-status: {status_stdout}"
    );

    // The supervisor lease itself (not just the tx record) must be
    // released: `manager lease-release` talks directly to the daemon's
    // (task_id, kind) lease map, independent of the tx-scan dispatch-status
    // view, and must report nothing left to clear.
    let lease_stdout = run_orgasmic(
        &home,
        &running,
        &project_root,
        &path_env,
        &[
            "manager",
            "lease-release",
            "--task",
            "TASK-DISPATCH",
            "--kind",
            "implementer",
        ],
    );
    assert!(
        lease_stdout.contains("no lease held"),
        "finalize must release the supervisor lease: {lease_stdout}"
    );

    // Finalize the reviewer kind too, proving the terminal-tx type follows
    // the run's own kind (`reviewer.done`), not a hardcoded implementer path.
    let review_worktree = tmp.path().join("worktrees/task-review");
    let review_brief = tmp.path().join("codex/task-review-brief.md");
    write(&review_brief, "stub reviewer brief");
    let review_dispatch_stdout = run_orgasmic(
        &home,
        &running,
        &project_root,
        &path_env,
        &[
            "manager",
            "dispatch",
            "--task",
            "TASK-REVIEW",
            "--kind",
            "reviewer",
            "--worker",
            "reviewer-codex-acp",
            "--brief",
            review_brief.to_str().unwrap(),
            "--from",
            &head,
            "--worktree",
            review_worktree.to_str().unwrap(),
            "--branch",
            "task-review-test-review",
            "--reason",
            "reviewer finalize smoke",
        ],
    );
    assert!(review_dispatch_stdout.contains("dispatched: TASK-REVIEW reviewer pid="));
    let review_summary_path = tmp.path().join("review-summary.md");
    write(&review_summary_path, "reviewer finalize smoke");
    let review_finalize_stdout = run_orgasmic(
        &home,
        &running,
        &review_worktree,
        &path_env,
        &[
            "dispatch",
            "finalize",
            "--task",
            "TASK-REVIEW",
            "--summary-file",
            review_summary_path.to_str().unwrap(),
        ],
    );
    assert!(
        review_finalize_stdout.contains("finalized: TASK-REVIEW reviewer.done tx="),
        "unexpected reviewer finalize output: {review_finalize_stdout}"
    );
    let review_status_stdout = run_orgasmic(
        &home,
        &running,
        &project_root,
        &path_env,
        &["manager", "dispatch-status", "--task", "TASK-REVIEW"],
    );
    assert!(
        review_status_stdout.trim().is_empty(),
        "finalized reviewer dispatch should not appear as open: {review_status_stdout}"
    );

    let _ = running.shutdown.send(());
    let _ = running.join.await;
}

/// Item C (TASK-DWJVH): the previously-untested `--status blocked` finalize
/// path, beside the WFW1N `--status done` coverage above. Asserts both
/// halves of `cmd_dispatch_finalize`'s blocked branch: `--reason` is
/// required (bails before ever touching the live run), and, when given,
/// finalize writes last.txt verbatim, releases the lease, and emits
/// `manager.dispatch_aborted` — never a `*.done` tx.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn dispatch_finalize_blocked_status_emits_dispatch_aborted_and_requires_reason() {
    let _live_guard = live_session_guard();
    let tmp = tempfile::tempdir().unwrap();
    let home = Home::at(tmp.path().join("home"));
    home.ensure().unwrap();
    let project_root = tmp.path().join("project");
    std::fs::create_dir_all(&project_root).unwrap();
    seed_project(&home, &project_root);
    let head = init_git_project(&project_root);
    let bin_dir = tmp.path().join("bin");
    std::fs::create_dir_all(&bin_dir).unwrap();
    write_sleeping_stub_codex(&bin_dir);
    let path_env = path_with_stub(&bin_dir);
    let brief = tmp.path().join("codex/task-dispatch-brief.md");
    let worktree = tmp.path().join("worktrees/task-dispatch");

    let running = boot(home.clone()).await;
    dispatch_sleeping_implementer(
        &home,
        &running,
        &project_root,
        &path_env,
        &head,
        &worktree,
        &brief,
    )
    .await;

    let summary_path = tmp.path().join("summary.md");
    write(&summary_path, "brief impossible as written");

    // Without --reason: bails fast, before touching the live run or writing
    // any artifacts.
    let failure_stderr = run_orgasmic_failure(
        &home,
        &running,
        &worktree,
        &path_env,
        &[
            "dispatch",
            "finalize",
            "--task",
            "TASK-DISPATCH",
            "--summary-file",
            summary_path.to_str().unwrap(),
            "--status",
            "blocked",
        ],
    );
    assert!(
        failure_stderr.contains("--reason is required when --status blocked"),
        "unexpected failure output: {failure_stderr}"
    );

    // With --reason: succeeds, writes last.txt verbatim, releases the lease,
    // and emits manager.dispatch_aborted (not a done tx).
    let finalize_stdout = run_orgasmic(
        &home,
        &running,
        &worktree,
        &path_env,
        &[
            "dispatch",
            "finalize",
            "--task",
            "TASK-DISPATCH",
            "--summary-file",
            summary_path.to_str().unwrap(),
            "--status",
            "blocked",
            "--reason",
            "brief impossible as written",
        ],
    );
    assert!(
        finalize_stdout.contains("finalized: TASK-DISPATCH manager.dispatch_aborted tx="),
        "unexpected finalize output: {finalize_stdout}"
    );

    let last_path = finalized_last_path(&finalize_stdout);
    let last_bytes =
        std::fs::read(&last_path).unwrap_or_else(|e| panic!("read {}: {e}", last_path.display()));
    assert_eq!(
        last_bytes,
        "brief impossible as written".as_bytes(),
        "last.txt must be written verbatim on the blocked path too"
    );

    let tx_raw = tx_log(&project_root);
    assert!(tx_raw.contains(":TYPE:         manager.dispatch_aborted"));
    assert!(
        !tx_raw.contains(":TYPE:         implementer.done"),
        "blocked finalize must not emit a done tx: {tx_raw}"
    );

    let lease_stdout = run_orgasmic(
        &home,
        &running,
        &project_root,
        &path_env,
        &[
            "manager",
            "lease-release",
            "--task",
            "TASK-DISPATCH",
            "--kind",
            "implementer",
        ],
    );
    assert!(
        lease_stdout.contains("no lease held"),
        "blocked finalize must release the supervisor lease: {lease_stdout}"
    );

    let _ = running.shutdown.send(());
    let _ = running.join.await;
}

/// Item B (TASK-DWJVH, WFW1N review #5 residual): the stall sweep and the
/// worker's own finalize can race — the sweep releases the run in the
/// window between the worker resolving it (`resolve_finalize_run`) and the
/// worker's own release call landing, after the commit + last.txt write
/// already made the work durable. Finalize must not hard-error on that:
/// the terminal `*.done` tx must still land from the intact report, instead
/// of leaving the run a done-less orphan.
///
/// `ORGASMIC_TEST_FINALIZE_RELEASE_DELAY_MS` (test-only knob added beside
/// this fix in manager.rs) opens a deterministic window between the
/// last.txt write and finalize's own release call; a background task races
/// a raw release call — mirroring exactly what the stall sweep does
/// (no caller identity, `finalized_by_worker: false`, a timeout reason) —
/// into that window as soon as last.txt exists.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn dispatch_finalize_survives_stall_sweep_race_and_still_records_done() {
    let _live_guard = live_session_guard();
    let tmp = tempfile::tempdir().unwrap();
    let home = Home::at(tmp.path().join("home"));
    home.ensure().unwrap();
    let project_root = tmp.path().join("project");
    std::fs::create_dir_all(&project_root).unwrap();
    seed_project(&home, &project_root);
    let head = init_git_project(&project_root);
    let bin_dir = tmp.path().join("bin");
    std::fs::create_dir_all(&bin_dir).unwrap();
    write_sleeping_stub_codex(&bin_dir);
    let path_env = path_with_stub(&bin_dir);
    let brief = tmp.path().join("codex/task-dispatch-brief.md");
    let worktree = tmp.path().join("worktrees/task-dispatch");

    let running = boot(home.clone()).await;
    dispatch_sleeping_implementer(
        &home,
        &running,
        &project_root,
        &path_env,
        &head,
        &worktree,
        &brief,
    )
    .await;

    let token = std::fs::read_to_string(home.auth_token())
        .unwrap()
        .trim()
        .to_string();
    let http = reqwest::Client::new();
    let runs: serde_json::Value = http
        .get(format!("http://{}/api/runs", running.addr))
        .header(AUTHORIZATION, format!("Bearer {token}"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let live = runs["live"].as_array().expect("live runs array");
    let run = live
        .iter()
        .find(|run| run["task_id"] == "TASK-DISPATCH")
        .expect("live run for TASK-DISPATCH");
    let run_id = run["run_id"].as_str().unwrap().to_string();
    let last_path = PathBuf::from(run["last_path"].as_str().expect("last_path"));

    let summary_path = tmp.path().join("summary.md");
    let summary_content = "race smoke: durable despite stall-sweep race";
    write(&summary_path, summary_content);

    let racer_http = http.clone();
    let racer_addr = running.addr;
    let racer_token = token.clone();
    let racer_run_id = run_id.clone();
    let racer_last_path = last_path.clone();
    let racer = tokio::spawn(async move {
        let deadline = tokio::time::Instant::now() + Duration::from_secs(2);
        while !dispatch_artifact_has_content(&racer_last_path) {
            assert!(
                tokio::time::Instant::now() < deadline,
                "timed out waiting for {} before racing the release",
                racer_last_path.display()
            );
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        racer_http
            .post(format!(
                "http://{racer_addr}/api/runs/{racer_run_id}/release"
            ))
            .header(AUTHORIZATION, format!("Bearer {racer_token}"))
            .json(&serde_json::json!({
                "reason": "stall_timeout_exceeded",
                "finalized_by_worker": false,
            }))
            .send()
            .await
            .expect("racer release request")
    });

    let finalize_output = run_orgasmic_output_with_env(
        &home,
        &running,
        &worktree,
        &path_env,
        &[
            "dispatch",
            "finalize",
            "--task",
            "TASK-DISPATCH",
            "--summary-file",
            summary_path.to_str().unwrap(),
        ],
        &[("ORGASMIC_TEST_FINALIZE_RELEASE_DELAY_MS", "300")],
    );

    let racer_response = racer.await.expect("racer task panicked");
    assert!(
        racer_response.status().is_success(),
        "racer release against the still-live run should have succeeded: {}",
        racer_response.status()
    );

    let stdout = String::from_utf8_lossy(&finalize_output.stdout).to_string();
    let stderr = String::from_utf8_lossy(&finalize_output.stderr).to_string();
    assert!(
        finalize_output.status.success(),
        "finalize must not hard-error on the stall-sweep race\nstdout={stdout}\nstderr={stderr}"
    );
    assert!(
        stdout.contains("finalized: TASK-DISPATCH implementer.done tx="),
        "expected a done tx despite the race: stdout={stdout} stderr={stderr}"
    );
    assert!(
        stderr.contains("already released"),
        "expected the already-released resilience warning on stderr: {stderr}"
    );

    let last_bytes = std::fs::read(&last_path).unwrap();
    assert_eq!(
        last_bytes,
        summary_content.as_bytes(),
        "the worker's report must survive the race intact, not be clobbered by orphan handling"
    );

    let tx_raw = tx_log(&project_root);
    assert!(
        tx_raw.contains(":TYPE:         implementer.done"),
        "a run whose worker committed + wrote its report must be recorded done, \
         never left a bare orphan: {tx_raw}"
    );

    let _ = running.shutdown.send(());
    let _ = running.join.await;
}

/// Regression for TASK-QKQ3R: a dispatch worktree whose project has not yet
/// committed `.orgasmic/` (the greenfield window between `orgasmic project
/// init` and its first commit) must not let `dispatch finalize --commit`
/// escape the worktree via the `.orgasmic/project.org` marker walk and
/// commit the manager's live repo root instead. `.orgasmic/project.org`
/// exists on disk at the project root but is gitignored, so the linked
/// worktree checkout (nested at the real default
/// `.orgasmic/tmp/dispatch/<task>/worktree` layout) carries no `.orgasmic`
/// at all.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn dispatch_finalize_commit_binds_to_worktree_when_orgasmic_is_uncommitted() {
    let _live_guard = live_session_guard();
    let tmp = tempfile::tempdir().unwrap();
    let home = Home::at(tmp.path().join("home"));
    home.ensure().unwrap();
    let project_root = tmp.path().join("project");
    std::fs::create_dir_all(&project_root).unwrap();
    seed_project(&home, &project_root);

    write(&project_root.join(".gitignore"), ".orgasmic/\n");
    run_git(&project_root, &["init", "-b", "main"]);
    run_git(
        &project_root,
        &["config", "user.email", "tester@example.com"],
    );
    run_git(&project_root, &["config", "user.name", "Test User"]);
    run_git(&project_root, &["add", "."]);
    run_git(&project_root, &["commit", "-m", "init"]);
    let head = run_git(&project_root, &["rev-parse", "HEAD"]);
    let manager_head_before = head.clone();

    let bin_dir = tmp.path().join("bin");
    std::fs::create_dir_all(&bin_dir).unwrap();
    write_sleeping_stub_codex(&bin_dir);
    let path_env = path_with_stub(&bin_dir);
    let brief = tmp.path().join("codex/task-dispatch-brief.md");
    write(&brief, "stub implementer brief");

    // Default worktree layout: nested under the project's own
    // `.orgasmic/tmp/dispatch/<task>/worktree` — exactly the layout that
    // escaped to the manager root pre-fix.
    let worktree = project_root.join(".orgasmic/tmp/dispatch/task-dispatch/worktree");

    let running = boot(home.clone()).await;
    let dispatch_stdout = run_orgasmic(
        &home,
        &running,
        &project_root,
        &path_env,
        &[
            "manager",
            "dispatch",
            "--task",
            "TASK-DISPATCH",
            "--kind",
            "implementer",
            "--worker",
            "implementer-codex-appserver",
            "--brief",
            brief.to_str().unwrap(),
            "--from",
            &head,
            "--reason",
            "finalize wrong-root regression",
        ],
    );
    assert!(
        dispatch_stdout.contains("dispatched: TASK-DISPATCH implementer pid="),
        "unexpected dispatch output: {dispatch_stdout}"
    );
    assert!(
        worktree.is_dir(),
        "worktree should exist at the default nested layout: {}",
        worktree.display()
    );
    assert!(
        !worktree.join(".orgasmic/project.org").exists(),
        "worktree checkout must NOT carry .orgasmic (it was never committed)"
    );

    // Simulate the implementer's uncommitted work in the worktree.
    write(&worktree.join("scripts/greet.sh"), "#!/bin/sh\necho hi\n");
    let worktree_head_before = run_git(&worktree, &["rev-parse", "HEAD"]);

    // The manager repo has its own uncommitted scratch state at the moment
    // finalize runs — this must be left completely untouched.
    write(
        &project_root.join("scratch.txt"),
        "unrelated manager work\n",
    );

    let summary_path = tmp.path().join("summary.md");
    write(
        &summary_path,
        "TASK-QKQ3R regression: commit only the worktree",
    );

    let finalize_stdout = run_orgasmic(
        &home,
        &running,
        &worktree,
        &path_env,
        &[
            "dispatch",
            "finalize",
            "--task",
            "TASK-DISPATCH",
            "--summary-file",
            summary_path.to_str().unwrap(),
            "--commit",
        ],
    );
    assert!(
        finalize_stdout.contains("finalized: TASK-DISPATCH implementer.done tx="),
        "unexpected finalize output: {finalize_stdout}"
    );

    // The manager repo root must be untouched: HEAD unchanged, and the
    // untracked scratch file is still untracked (not swept into a commit).
    let manager_head_after = run_git(&project_root, &["rev-parse", "HEAD"]);
    assert_eq!(
        manager_head_before, manager_head_after,
        "finalize --commit must never advance the manager repo's HEAD"
    );
    assert!(
        project_root.join("scratch.txt").exists(),
        "unrelated manager scratch file must survive untouched"
    );
    let manager_status = run_git(&project_root, &["status", "--porcelain", "--ignored"]);
    assert!(
        manager_status.contains("scratch.txt"),
        "scratch.txt must remain untracked/uncommitted in the manager repo: {manager_status}"
    );

    // The worktree itself must have advanced and be clean.
    let worktree_head_after = run_git(&worktree, &["rev-parse", "HEAD"]);
    assert_ne!(
        worktree_head_before, worktree_head_after,
        "finalize --commit must commit the worktree's own dirty state"
    );
    let worktree_status = run_git(&worktree, &["status", "--porcelain"]);
    assert!(
        worktree_status.is_empty(),
        "worktree must be clean after --commit: {worktree_status}"
    );
    assert!(
        run_git(&worktree, &["show", "HEAD:scripts/greet.sh"]).contains("echo hi"),
        "the worker's file must be committed onto the worktree branch"
    );

    let _ = running.shutdown.send(());
    let _ = running.join.await;
}

/// Worktree-mismatch refusal (TASK-QKQ3R part B): the daemon's live run
/// record advertises the dispatched worktree; running `dispatch finalize
/// --commit` from an unrelated repo must hard-error and commit nothing
/// anywhere, rather than silently committing whatever `git` resolves from
/// the wrong cwd.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn dispatch_finalize_refuses_commit_when_git_root_does_not_match_dispatched_worktree() {
    let _live_guard = live_session_guard();
    let tmp = tempfile::tempdir().unwrap();
    let home = Home::at(tmp.path().join("home"));
    home.ensure().unwrap();
    let project_root = tmp.path().join("project");
    std::fs::create_dir_all(&project_root).unwrap();
    seed_project(&home, &project_root);
    let head = init_git_project(&project_root);
    let bin_dir = tmp.path().join("bin");
    std::fs::create_dir_all(&bin_dir).unwrap();
    write_sleeping_stub_codex(&bin_dir);
    let path_env = path_with_stub(&bin_dir);
    let brief = tmp.path().join("codex/task-dispatch-brief.md");
    let worktree = tmp.path().join("worktrees/task-dispatch");

    let running = boot(home.clone()).await;
    dispatch_sleeping_implementer(
        &home,
        &running,
        &project_root,
        &path_env,
        &head,
        &worktree,
        &brief,
    )
    .await;

    // Leave uncommitted work in the real dispatched worktree — it must
    // survive the refused finalize untouched.
    write(&worktree.join("NOTES.md"), "uncommitted worker output\n");
    let worktree_head_before = run_git(&worktree, &["rev-parse", "HEAD"]);
    let worktree_status_before = run_git(&worktree, &["status", "--porcelain"]);

    // A completely unrelated git repo, standing in for "an unexpected cwd".
    let other_repo = tmp.path().join("other-repo");
    std::fs::create_dir_all(&other_repo).unwrap();
    run_git(&other_repo, &["init", "-b", "main"]);
    run_git(&other_repo, &["config", "user.email", "tester@example.com"]);
    run_git(&other_repo, &["config", "user.name", "Test User"]);
    write(&other_repo.join("README.md"), "unrelated repo\n");
    run_git(&other_repo, &["add", "."]);
    run_git(&other_repo, &["commit", "-m", "init"]);
    let other_head_before = run_git(&other_repo, &["rev-parse", "HEAD"]);

    let summary_path = tmp.path().join("summary.md");
    write(&summary_path, "should never be committed anywhere");

    let stderr = run_orgasmic_failure(
        &home,
        &running,
        &other_repo,
        &path_env,
        &[
            "dispatch",
            "finalize",
            "--task",
            "TASK-DISPATCH",
            "--summary-file",
            summary_path.to_str().unwrap(),
            "--commit",
        ],
    );
    assert!(
        stderr.contains("refusing --commit"),
        "expected a loud worktree-mismatch refusal: {stderr}"
    );

    // Nothing committed anywhere: neither the unrelated cwd repo...
    let other_head_after = run_git(&other_repo, &["rev-parse", "HEAD"]);
    assert_eq!(
        other_head_before, other_head_after,
        "the unrelated repo used as cwd must never be committed to"
    );
    // ...nor the real dispatched worktree (finalize must bail before it
    // ever runs `git add`/`git commit`).
    let worktree_head_after = run_git(&worktree, &["rev-parse", "HEAD"]);
    assert_eq!(
        worktree_head_before, worktree_head_after,
        "the dispatched worktree must not be committed to either"
    );
    let worktree_status_after = run_git(&worktree, &["status", "--porcelain"]);
    assert_eq!(
        worktree_status_before, worktree_status_after,
        "the dispatched worktree's uncommitted state must be untouched"
    );

    let _ = running.shutdown.send(());
    let _ = running.join.await;
}

/// TASK-P4MGK: `orgasmic dispatch finalize` is accepted from acp-stdio, not
/// only rmux/acp-ws. PATH has no `codex` so the driver stays Simulated and
/// the lease stays live until finalize (protocol-end is not the success
/// signal).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn dispatch_finalize_from_acp_stdio_mode() {
    // orgasmic:TASK-P4MGK
    let _live_guard = live_session_guard();
    let tmp = tempfile::tempdir().unwrap();
    let home = Home::at(tmp.path().join("home"));
    home.ensure().unwrap();
    let project_root = tmp.path().join("project");
    std::fs::create_dir_all(&project_root).unwrap();
    seed_project(&home, &project_root);
    write(
        &home.user().join("workers/implementer-codex-acp.org"),
        "* WORKER implementer-codex-acp\n:PROPERTIES:\n:ID:                          implementer-codex-acp\n:KIND:             implementer\n:DRIVER:                      acp-stdio\n:HARNESS:                     codex\n:PROVIDERS:                   openai\n:MODELS:                      gpt-5.5\n:REASONING_EFFORTS:           high\n:DEFAULT_PROVIDER:            openai\n:DEFAULT_MODEL:               gpt-5.5\n:DEFAULT_EFFORT:              high\n:LINKED_SKILLS:\n:APPLICABLE_STATES:           claimed, analyzing, implementing, testing, fixing\n:MAX_ITERATIONS:              1\n:CONTEXT_BUDGET:              4000\n:VERSION:                     1\n:END:\n\n** Persona\nTest acp-stdio implementer.\n\n** Operating Rules\n- Keep test runs simulated.\n",
    );
    let head = init_git_project(&project_root);
    let bin_dir = tmp.path().join("bin");
    std::fs::create_dir_all(&bin_dir).unwrap();
    write_git_proxy(&bin_dir);
    // No `codex` on PATH → acp-stdio stays Simulated (Ready only, lease live).
    let path_env = path_only(&bin_dir);
    let brief = tmp.path().join("brief.md");
    let worktree = tmp.path().join("worktrees/task-dispatch-acp-stdio");
    write(&brief, "acp-stdio finalize smoke");

    let running = boot(home.clone()).await;
    let dispatch_stdout = run_orgasmic(
        &home,
        &running,
        &project_root,
        &path_env,
        &[
            "manager",
            "dispatch",
            "--task",
            "TASK-DISPATCH",
            "--kind",
            "implementer",
            "--worker",
            "implementer-codex-acp",
            "--brief",
            brief.to_str().unwrap(),
            "--from",
            &head,
            "--worktree",
            worktree.to_str().unwrap(),
            "--branch",
            "task-dispatch-acp-stdio-impl",
            "--reason",
            "acp-stdio finalize smoke",
        ],
    );
    assert!(
        dispatch_stdout.contains("dispatched: TASK-DISPATCH implementer pid="),
        "unexpected dispatch output: {dispatch_stdout}"
    );

    let summary_path = tmp.path().join("summary.md");
    write(&summary_path, "acp-stdio finalize report");
    let finalize_stdout = run_orgasmic(
        &home,
        &running,
        &worktree,
        &path_env,
        &[
            "dispatch",
            "finalize",
            "--task",
            "TASK-DISPATCH",
            "--summary-file",
            summary_path.to_str().unwrap(),
        ],
    );
    assert!(
        finalize_stdout.contains("finalized: TASK-DISPATCH implementer.done tx="),
        "unexpected finalize output: {finalize_stdout}"
    );
    let last_path = finalized_last_path(&finalize_stdout);
    assert_eq!(
        std::fs::read_to_string(&last_path).unwrap(),
        "acp-stdio finalize report"
    );
    let tx_raw = tx_log(&project_root);
    assert!(
        tx_raw.contains(":TYPE:         implementer.done"),
        "acp-stdio finalize must emit implementer.done: {tx_raw}"
    );

    let _ = running.shutdown.send(());
    let _ = running.join.await;
}

/// TASK-P4MGK: finalize accepted from subprocess-stream-json (cursor-agent).
/// No `cursor-agent` on PATH → Simulated mode; control keeps the event
/// channel open so protocol RunComplete at acquire does not release the
/// lease before finalize.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn dispatch_finalize_from_subprocess_stream_json_mode() {
    // orgasmic:TASK-P4MGK
    let _live_guard = live_session_guard();
    let tmp = tempfile::tempdir().unwrap();
    let home = Home::at(tmp.path().join("home"));
    home.ensure().unwrap();
    let project_root = tmp.path().join("project");
    std::fs::create_dir_all(&project_root).unwrap();
    seed_project(&home, &project_root);
    write(
        &home.user().join("workers/implementer-cursor.org"),
        "* WORKER implementer-cursor\n:PROPERTIES:\n:ID:                          implementer-cursor\n:KIND:             implementer\n:DRIVER:                      subprocess-stream-json\n:HARNESS:                     cursor-agent\n:PROVIDERS:                   cursor\n:MODELS:                      composer-2.5-fast\n:REASONING_EFFORTS:           high\n:DEFAULT_PROVIDER:            cursor\n:DEFAULT_MODEL:               composer-2.5-fast\n:DEFAULT_EFFORT:              high\n:LINKED_SKILLS:\n:APPLICABLE_STATES:           claimed, analyzing, implementing, testing, fixing\n:MAX_ITERATIONS:              1\n:CONTEXT_BUDGET:              4000\n:VERSION:                     1\n:END:\n\n** Persona\nTest subprocess-stream-json implementer.\n\n** Operating Rules\n- Keep test runs simulated.\n",
    );
    let head = init_git_project(&project_root);
    let bin_dir = tmp.path().join("bin");
    std::fs::create_dir_all(&bin_dir).unwrap();
    write_git_proxy(&bin_dir);
    let path_env = path_only(&bin_dir);
    let brief = tmp.path().join("brief.md");
    let worktree = tmp.path().join("worktrees/task-dispatch-stream-json");
    write(&brief, "subprocess-stream-json finalize smoke");

    let running = boot(home.clone()).await;
    let dispatch_stdout = run_orgasmic(
        &home,
        &running,
        &project_root,
        &path_env,
        &[
            "manager",
            "dispatch",
            "--task",
            "TASK-DISPATCH",
            "--kind",
            "implementer",
            "--worker",
            "implementer-cursor",
            "--brief",
            brief.to_str().unwrap(),
            "--from",
            &head,
            "--worktree",
            worktree.to_str().unwrap(),
            "--branch",
            "task-dispatch-stream-json-impl",
            "--reason",
            "subprocess-stream-json finalize smoke",
        ],
    );
    assert!(
        dispatch_stdout.contains("dispatched: TASK-DISPATCH implementer pid="),
        "unexpected dispatch output: {dispatch_stdout}"
    );

    let summary_path = tmp.path().join("summary.md");
    write(&summary_path, "subprocess-stream-json finalize report");
    let finalize_stdout = run_orgasmic(
        &home,
        &running,
        &worktree,
        &path_env,
        &[
            "dispatch",
            "finalize",
            "--task",
            "TASK-DISPATCH",
            "--summary-file",
            summary_path.to_str().unwrap(),
        ],
    );
    assert!(
        finalize_stdout.contains("finalized: TASK-DISPATCH implementer.done tx="),
        "unexpected finalize output: {finalize_stdout}"
    );
    let last_path = finalized_last_path(&finalize_stdout);
    assert_eq!(
        std::fs::read_to_string(&last_path).unwrap(),
        "subprocess-stream-json finalize report"
    );
    let tx_raw = tx_log(&project_root);
    assert!(
        tx_raw.contains(":TYPE:         implementer.done"),
        "subprocess-stream-json finalize must emit implementer.done: {tx_raw}"
    );

    let _ = running.shutdown.send(());
    let _ = running.join.await;
}

/// TASK-8PXDP / HIGH1: when protocol-end wins the finalize race, finalize must
/// not mask the 404 and emit `implementer.done` (which would orphan AND done).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn dispatch_finalize_protocol_end_during_release_refuses_done_tx() {
    // orgasmic:TASK-8PXDP
    let _live_guard = live_session_guard();
    let tmp = tempfile::tempdir().unwrap();
    let home = Home::at(tmp.path().join("home"));
    home.ensure().unwrap();
    let project_root = tmp.path().join("project");
    std::fs::create_dir_all(&project_root).unwrap();
    seed_project(&home, &project_root);
    write(
        &home.user().join("workers/implementer-codex-acp.org"),
        "* WORKER implementer-codex-acp\n:PROPERTIES:\n:ID:                          implementer-codex-acp\n:KIND:             implementer\n:DRIVER:                      acp-stdio\n:HARNESS:                     codex\n:PROVIDERS:                   openai\n:MODELS:                      gpt-5.5\n:REASONING_EFFORTS:           high\n:DEFAULT_PROVIDER:            openai\n:DEFAULT_MODEL:               gpt-5.5\n:DEFAULT_EFFORT:              high\n:LINKED_SKILLS:\n:APPLICABLE_STATES:           claimed, analyzing, implementing, testing, fixing\n:MAX_ITERATIONS:              1\n:CONTEXT_BUDGET:              4000\n:VERSION:                     1\n:END:\n\n** Persona\nTest acp-stdio implementer.\n\n** Operating Rules\n- Keep test runs simulated.\n",
    );
    let head = init_git_project(&project_root);
    let bin_dir = tmp.path().join("bin");
    std::fs::create_dir_all(&bin_dir).unwrap();
    write_sleeping_stub_codex(&bin_dir);
    write_git_proxy(&bin_dir);
    let path_env = path_with_stub(&bin_dir);
    let brief = tmp.path().join("brief.md");
    let worktree = tmp.path().join("worktrees/task-dispatch-protocol-race");
    write(&brief, "protocol-end race brief");

    let running = boot(home.clone()).await;
    run_orgasmic(
        &home,
        &running,
        &project_root,
        &path_env,
        &[
            "manager",
            "dispatch",
            "--task",
            "TASK-DISPATCH",
            "--kind",
            "implementer",
            "--worker",
            "implementer-codex-acp",
            "--brief",
            brief.to_str().unwrap(),
            "--from",
            &head,
            "--worktree",
            worktree.to_str().unwrap(),
            "--branch",
            "task-dispatch-protocol-race-impl",
            "--reason",
            "protocol-end race",
        ],
    );

    let token = std::fs::read_to_string(home.auth_token())
        .unwrap()
        .trim()
        .to_string();
    let http = reqwest::Client::new();
    let runs: serde_json::Value = http
        .get(format!("http://{}/api/runs", running.addr))
        .header(AUTHORIZATION, format!("Bearer {token}"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let live = runs["live"].as_array().expect("live runs array");
    let run = live
        .iter()
        .find(|run| run["task_id"] == "TASK-DISPATCH")
        .expect("live run for TASK-DISPATCH");
    let run_id = run["run_id"].as_str().unwrap().to_string();
    let last_path = PathBuf::from(run["last_path"].as_str().expect("last_path"));

    let summary_path = tmp.path().join("summary.md");
    write(&summary_path, "would-be finalize report");

    let racer_http = http.clone();
    let racer_addr = running.addr;
    let racer_token = token.clone();
    let racer_run_id = run_id.clone();
    let racer_last_path = last_path.clone();
    let racer = tokio::spawn(async move {
        let deadline = tokio::time::Instant::now() + Duration::from_secs(2);
        while !dispatch_artifact_has_content(&racer_last_path) {
            assert!(
                tokio::time::Instant::now() < deadline,
                "timed out waiting for {} before racing protocol-end",
                racer_last_path.display()
            );
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        racer_http
            .post(format!(
                "http://{racer_addr}/api/runs/{racer_run_id}/release"
            ))
            .header(AUTHORIZATION, format!("Bearer {racer_token}"))
            .json(&serde_json::json!({
                "reason": "protocol_end_without_finalize",
                "finalized_by_worker": false,
            }))
            .send()
            .await
            .expect("racer release request")
    });

    let finalize_output = run_orgasmic_output_with_env(
        &home,
        &running,
        &worktree,
        &path_env,
        &[
            "dispatch",
            "finalize",
            "--task",
            "TASK-DISPATCH",
            "--summary-file",
            summary_path.to_str().unwrap(),
        ],
        &[("ORGASMIC_TEST_FINALIZE_RELEASE_DELAY_MS", "300")],
    );

    let _ = racer.await.expect("racer task panicked");

    assert!(
        !finalize_output.status.success(),
        "finalize must refuse done tx when protocol-end tombstone is present\nstdout={}\nstderr={}",
        String::from_utf8_lossy(&finalize_output.stdout),
        String::from_utf8_lossy(&finalize_output.stderr)
    );
    let stderr = String::from_utf8_lossy(&finalize_output.stderr);
    assert!(
        stderr.contains("protocol before finalize")
            || stderr.contains("no worker-finalize tombstone"),
        "expected protocol-end refusal on stderr: {stderr}"
    );

    let tx_raw = tx_log(&project_root);
    assert!(
        !tx_raw.contains(":TYPE:         implementer.done"),
        "protocol-end race must never emit implementer.done: {tx_raw}"
    );

    let _ = running.shutdown.send(());
    let _ = running.join.await;
}

/// TASK-8PXDP / HIGH1: two concurrent finalizers must emit at most one
/// terminal `*.done` tx (deterministic request_id + writer dedupe).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn dispatch_finalize_concurrent_double_finalize_emits_single_done_tx() {
    // orgasmic:TASK-8PXDP
    let _live_guard = live_session_guard();
    let tmp = tempfile::tempdir().unwrap();
    let home = Home::at(tmp.path().join("home"));
    home.ensure().unwrap();
    let project_root = tmp.path().join("project");
    std::fs::create_dir_all(&project_root).unwrap();
    seed_project(&home, &project_root);
    let head = init_git_project(&project_root);
    let bin_dir = tmp.path().join("bin");
    std::fs::create_dir_all(&bin_dir).unwrap();
    write_sleeping_stub_codex(&bin_dir);
    let path_env = path_with_stub(&bin_dir);
    let brief = tmp.path().join("codex/task-dispatch-brief.md");
    let worktree = tmp.path().join("worktrees/task-dispatch-double-finalize");

    let running = boot(home.clone()).await;
    dispatch_sleeping_implementer(
        &home,
        &running,
        &project_root,
        &path_env,
        &head,
        &worktree,
        &brief,
    )
    .await;

    let summary_path = tmp.path().join("summary.md");
    write(&summary_path, "concurrent finalize smoke");

    let daemon_url = format!("http://{}", running.addr);
    let home_a = home.clone();
    let daemon_url_a = daemon_url.clone();
    let worktree_a = worktree.clone();
    let path_env_a = path_env.clone();
    let summary_a = summary_path.clone();
    let first = std::thread::spawn(move || {
        run_orgasmic_output_with_daemon_url(
            &home_a,
            &daemon_url_a,
            &worktree_a,
            &path_env_a,
            &[
                "dispatch",
                "finalize",
                "--task",
                "TASK-DISPATCH",
                "--summary-file",
                summary_a.to_str().unwrap(),
            ],
            &[("ORGASMIC_TEST_FINALIZE_RELEASE_DELAY_MS", "200")],
        )
    });
    let home_b = home.clone();
    let daemon_url_b = daemon_url.clone();
    let worktree_b = worktree.clone();
    let path_env_b = path_env.clone();
    let summary_b = summary_path.clone();
    let second = std::thread::spawn(move || {
        run_orgasmic_output_with_daemon_url(
            &home_b,
            &daemon_url_b,
            &worktree_b,
            &path_env_b,
            &[
                "dispatch",
                "finalize",
                "--task",
                "TASK-DISPATCH",
                "--summary-file",
                summary_b.to_str().unwrap(),
            ],
            &[("ORGASMIC_TEST_FINALIZE_RELEASE_DELAY_MS", "200")],
        )
    });

    let out_a = first.join().expect("first finalize thread panicked");
    let out_b = second.join().expect("second finalize thread panicked");
    assert!(
        out_a.status.success() || out_b.status.success(),
        "at least one concurrent finalize must succeed\na={:?}\nb={:?}",
        out_a.status,
        out_b.status
    );

    let tx_raw = tx_log(&project_root);
    let done_count = tx_raw.matches(":TYPE:         implementer.done").count();
    assert_eq!(
        done_count, 1,
        "concurrent double-finalize must emit exactly one implementer.done: {tx_raw}"
    );

    let _ = running.shutdown.send(());
    let _ = running.join.await;
}

fn seed_stage_workers(home: &Home) {
    for (id, kind) in [
        ("griller", "griller"),
        ("planner", "planner"),
        ("architector", "architector"),
    ] {
        write(
            &home.user().join(format!("workers/{id}.org")),
            format!(
                "* WORKER {id}\n:PROPERTIES:\n:ID:                          {id}\n:KIND:             {kind}\n:DRIVER:                      acp-stdio\n:HARNESS:                     codex\n:PROVIDERS:                   openai\n:MODELS:                      gpt-5.5\n:REASONING_EFFORTS:           high\n:DEFAULT_PROVIDER:            openai\n:DEFAULT_MODEL:               gpt-5.5\n:DEFAULT_EFFORT:              high\n:LINKED_SKILLS:\n:APPLICABLE_STATES:           working\n:MAX_ITERATIONS:              1\n:CONTEXT_BUDGET:              4000\n:VERSION:                     1\n:END:\n\n** Persona\nTest {kind}.\n"
            ),
        );
    }
}

async fn live_run_for_id(
    http: &reqwest::Client,
    addr: std::net::SocketAddr,
    token: &str,
    run_id: &str,
) -> serde_json::Value {
    let runs: serde_json::Value = http
        .get(format!("http://{addr}/api/runs"))
        .header(AUTHORIZATION, format!("Bearer {token}"))
        .send()
        .await
        .expect("fetch live runs")
        .json()
        .await
        .expect("decode live runs");
    runs["live"]
        .as_array()
        .expect("live runs array")
        .iter()
        .find(|run| run["run_id"].as_str() == Some(run_id))
        .cloned()
        .unwrap_or_else(|| panic!("live run {run_id} not found"))
}

async fn start_stage_on_main(
    http: &reqwest::Client,
    addr: std::net::SocketAddr,
    token: &str,
    stage: &str,
    task_id: &str,
) -> (String, PathBuf) {
    let resp: serde_json::Value = http
        .post(format!("http://{addr}/api/{stage}"))
        .header(AUTHORIZATION, format!("Bearer {token}"))
        .json(&serde_json::json!({
            "project": "orgasmic",
            "task_id": task_id,
            "reason": "stage finalize smoke",
        }))
        .send()
        .await
        .expect("start stage")
        .json()
        .await
        .expect("decode stage response");
    assert_eq!(resp["status"], "acquired");
    let run_id = resp["run_id"].as_str().expect("run_id").to_string();
    let live = live_run_for_id(http, addr, token, &run_id).await;
    let last_path = PathBuf::from(live["last_path"].as_str().expect("last_path"));
    (run_id, last_path)
}

/// TASK-TZJFF: stage workers on `main` finalize via exported `ORGASMIC_RUN_ID`,
/// not branch-derived task identity.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn stage_grill_finalize_from_orgasmic_run_id_on_main() {
    let _live_guard = live_session_guard();
    let tmp = tempfile::tempdir().unwrap();
    let home = Home::at(tmp.path().join("home"));
    home.ensure().unwrap();
    let project_root = tmp.path().join("project");
    std::fs::create_dir_all(&project_root).unwrap();
    seed_project(&home, &project_root);
    seed_stage_workers(&home);
    init_git_project(&project_root);
    let bin_dir = tmp.path().join("bin");
    std::fs::create_dir_all(&bin_dir).unwrap();
    write_sleeping_stub_codex(&bin_dir);
    let path_env = path_with_stub(&bin_dir);

    let running = boot(home.clone()).await;
    let token = std::fs::read_to_string(home.auth_token())
        .unwrap()
        .trim()
        .to_string();
    let http = reqwest::Client::new();
    let (run_id, last_path) =
        start_stage_on_main(&http, running.addr, &token, "grill", "TASK-STAGE-GRILL").await;

    let summary_path = tmp.path().join("grill-summary.md");
    write(
        &summary_path,
        "grill finalize from main via ORGASMIC_RUN_ID",
    );

    let stdout = run_orgasmic_output_with_env(
        &home,
        &running,
        &project_root,
        &path_env,
        &[
            "dispatch",
            "finalize",
            "--summary-file",
            summary_path.to_str().unwrap(),
        ],
        &[("ORGASMIC_RUN_ID", run_id.as_str())],
    );
    assert!(
        stdout.status.success(),
        "grill finalize from main failed\nstdout={}\nstderr={}",
        String::from_utf8_lossy(&stdout.stdout),
        String::from_utf8_lossy(&stdout.stderr)
    );
    let out = String::from_utf8_lossy(&stdout.stdout);
    assert!(
        out.contains("griller.done"),
        "expected griller.done in finalize output: {out}"
    );
    assert_eq!(
        std::fs::read_to_string(&last_path).unwrap(),
        "grill finalize from main via ORGASMIC_RUN_ID"
    );

    let _ = running.shutdown.send(());
    let _ = running.join.await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn stage_plan_finalize_from_orgasmic_run_id_on_main() {
    let _live_guard = live_session_guard();
    let tmp = tempfile::tempdir().unwrap();
    let home = Home::at(tmp.path().join("home"));
    home.ensure().unwrap();
    let project_root = tmp.path().join("project");
    std::fs::create_dir_all(&project_root).unwrap();
    seed_project(&home, &project_root);
    seed_stage_workers(&home);
    init_git_project(&project_root);
    let bin_dir = tmp.path().join("bin");
    std::fs::create_dir_all(&bin_dir).unwrap();
    write_sleeping_stub_codex(&bin_dir);
    let path_env = path_with_stub(&bin_dir);

    let running = boot(home.clone()).await;
    let token = std::fs::read_to_string(home.auth_token())
        .unwrap()
        .trim()
        .to_string();
    let http = reqwest::Client::new();
    let (run_id, last_path) =
        start_stage_on_main(&http, running.addr, &token, "plan", "TASK-STAGE-PLAN").await;

    let summary_path = tmp.path().join("plan-summary.md");
    write(&summary_path, "plan finalize from main via ORGASMIC_RUN_ID");

    let stdout = run_orgasmic_output_with_env(
        &home,
        &running,
        &project_root,
        &path_env,
        &[
            "dispatch",
            "finalize",
            "--summary-file",
            summary_path.to_str().unwrap(),
        ],
        &[("ORGASMIC_RUN_ID", run_id.as_str())],
    );
    assert!(
        stdout.status.success(),
        "plan finalize from main failed\nstdout={}\nstderr={}",
        String::from_utf8_lossy(&stdout.stdout),
        String::from_utf8_lossy(&stdout.stderr)
    );
    let out = String::from_utf8_lossy(&stdout.stdout);
    assert!(
        out.contains("planner.done"),
        "expected planner.done in finalize output: {out}"
    );
    assert_eq!(
        std::fs::read_to_string(&last_path).unwrap(),
        "plan finalize from main via ORGASMIC_RUN_ID"
    );

    let _ = running.shutdown.send(());
    let _ = running.join.await;
}

/// TASK-99W9C: architect stage on `main` finalizes via exported `ORGASMIC_RUN_ID`.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn stage_architect_finalize_from_orgasmic_run_id_on_main() {
    let _live_guard = live_session_guard();
    let tmp = tempfile::tempdir().unwrap();
    let home = Home::at(tmp.path().join("home"));
    home.ensure().unwrap();
    let project_root = tmp.path().join("project");
    std::fs::create_dir_all(&project_root).unwrap();
    seed_project(&home, &project_root);
    seed_stage_workers(&home);
    init_git_project(&project_root);
    let bin_dir = tmp.path().join("bin");
    std::fs::create_dir_all(&bin_dir).unwrap();
    write_sleeping_stub_codex(&bin_dir);
    let path_env = path_with_stub(&bin_dir);

    let running = boot(home.clone()).await;
    let token = std::fs::read_to_string(home.auth_token())
        .unwrap()
        .trim()
        .to_string();
    let http = reqwest::Client::new();
    let (run_id, last_path) =
        start_stage_on_main(&http, running.addr, &token, "architect", "TASK-STAGE-ARCH").await;

    let summary_path = tmp.path().join("architect-summary.md");
    write(
        &summary_path,
        "architect finalize from main via ORGASMIC_RUN_ID",
    );

    let stdout = run_orgasmic_output_with_env(
        &home,
        &running,
        &project_root,
        &path_env,
        &[
            "dispatch",
            "finalize",
            "--summary-file",
            summary_path.to_str().unwrap(),
        ],
        &[("ORGASMIC_RUN_ID", run_id.as_str())],
    );
    assert!(
        stdout.status.success(),
        "architect finalize from main failed\nstdout={}\nstderr={}",
        String::from_utf8_lossy(&stdout.stdout),
        String::from_utf8_lossy(&stdout.stderr)
    );
    let out = String::from_utf8_lossy(&stdout.stdout);
    assert!(
        out.contains("architector.done"),
        "expected architector.done in finalize output: {out}"
    );
    assert_eq!(
        std::fs::read_to_string(&last_path).unwrap(),
        "architect finalize from main via ORGASMIC_RUN_ID"
    );

    let _ = running.shutdown.send(());
    let _ = running.join.await;
}

/// TASK-99W9C: app manager release via real CLI + HTTP with `ORGASMIC_RUN_ID`.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn app_manager_release_via_cli_orgasmic_run_id() {
    let _live_guard = live_session_guard();
    if !Command::new("tmux")
        .arg("-V")
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
    {
        eprintln!("skipping app_manager_release_via_cli_orgasmic_run_id: tmux not on PATH");
        return;
    }
    let tmp = tempfile::tempdir().unwrap();
    let home = Home::at(tmp.path().join("home"));
    home.ensure().unwrap();
    let project_root = tmp.path().join("project");
    std::fs::create_dir_all(&project_root).unwrap();
    seed_project(&home, &project_root);
    init_git_project(&project_root);
    let bin_dir = tmp.path().join("bin");
    std::fs::create_dir_all(&bin_dir).unwrap();
    write_sleeping_stub_codex(&bin_dir);
    let path_env = path_with_stub(&bin_dir);

    let running = boot(home.clone()).await;
    let token = std::fs::read_to_string(home.auth_token())
        .unwrap()
        .trim()
        .to_string();
    let http = reqwest::Client::new();
    let resp: serde_json::Value = http
        .post(format!("http://{}/api/manager/launch", running.addr))
        .header(AUTHORIZATION, format!("Bearer {token}"))
        .json(&serde_json::json!({
            "project_id": "orgasmic",
            "mode": "tmux",
            "harness": "codex",
        }))
        .send()
        .await
        .expect("manager launch")
        .json()
        .await
        .expect("decode manager launch");
    let run_id = resp["run_id"].as_str().expect("run_id").to_string();
    let live = live_run_for_id(&http, running.addr, &token, &run_id).await;
    let session_path = PathBuf::from(live["session_path"].as_str().expect("session_path"));

    let stdout = run_orgasmic_output_with_env(
        &home,
        &running,
        &project_root,
        &path_env,
        &["manager", "release", "--project", "orgasmic"],
        &[("ORGASMIC_RUN_ID", run_id.as_str())],
    );
    assert!(
        stdout.status.success(),
        "manager release via CLI failed\nstdout={}\nstderr={}",
        String::from_utf8_lossy(&stdout.stdout),
        String::from_utf8_lossy(&stdout.stderr)
    );
    let out = String::from_utf8_lossy(&stdout.stdout);
    assert!(
        out.contains("released manager registration"),
        "expected release confirmation: {out}"
    );

    let runs: serde_json::Value = http
        .get(format!("http://{}/api/runs", running.addr))
        .header(AUTHORIZATION, format!("Bearer {token}"))
        .send()
        .await
        .expect("fetch live runs")
        .json()
        .await
        .expect("decode live runs");
    assert!(
        !state_has_live_run(&runs, &run_id),
        "released manager run must leave the live set"
    );
    let body = std::fs::read_to_string(&session_path).unwrap_or_default();
    assert!(
        body.contains("manager_released"),
        "CLI release must write manager_released tombstone: {body}"
    );

    let _ = running.shutdown.send(());
    let _ = running.join.await;
}

fn state_has_live_run(runs: &serde_json::Value, run_id: &str) -> bool {
    runs["live"]
        .as_array()
        .map(|live| {
            live.iter()
                .any(|run| run["run_id"].as_str() == Some(run_id))
        })
        .unwrap_or(false)
}
