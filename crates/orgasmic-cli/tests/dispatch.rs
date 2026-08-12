use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::time::{Duration, Instant};

use orgasmic_core::Home;
use orgasmic_daemon::{Daemon, DaemonOptions, RunningDaemon};
use orgasmic_drivers::modes::rmux::test_tooling::{
    assert_required_test_tooling, live_session_guard, skip_test_if_missing, ToolRequirement,
};
use orgasmic_drivers::modes::tmux::{own_tmux_server_for_tests, real_tmux_on_path};
use reqwest::header::AUTHORIZATION;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;
#[cfg(unix)]
use std::os::unix::process::ExitStatusExt;

#[cfg(unix)]
struct PermissionRestore {
    path: PathBuf,
    mode: u32,
}

#[cfg(unix)]
impl PermissionRestore {
    fn new(path: &Path, mode: u32) -> Self {
        Self {
            path: path.to_path_buf(),
            mode,
        }
    }
}

#[cfg(unix)]
impl Drop for PermissionRestore {
    fn drop(&mut self) {
        let _ = std::fs::set_permissions(&self.path, std::fs::Permissions::from_mode(self.mode));
    }
}

// orgasmic:task_K5NDR
#[path = "common/env_isolation.rs"]
mod env_isolation;
use env_isolation::{orgasmic_command, orgasmic_exe, scrub_ambient_orgasmic_env};

// orgasmic:task_2GS7V
/// Is this host's `tmux` really tmux, and does this process own the server the
/// tmux-gated test below will reach?
///
/// Two questions, one gate, in this order — the shape
/// `crates/orgasmic-daemon/tests/recovery_fault_restart.rs` already uses
/// (TASK-FJCE9), and this binary is the live-tmux test binary that TASK-K4G1D
/// and TASK-0RCRY both missed.
///
/// 1. Strictness (TASK-K4G1D/TASK-JGHNC). This used to ask `tmux -V`, which
///    inside an orgasmic worker is answered by rmux's PATH shim — `tmux 3.4`
///    while the real binary is 3.6a. So the gate said "tmux" and the test ran,
///    driving `new-session` against `/private/tmp/rmux-501/default`: the rmux
///    server hosting live dispatched worker panes.
///
/// 2. Claiming an owned server (TASK-0RCRY), deliberately *after* the
///    strictness check, because claiming starts a keepalive session and
///    claiming through the shim would start it on the rmux server — the thing
///    step 1 exists to prevent.
///
/// Step 2 is what made TASK-2GS7V deterministic rather than merely dangerous.
/// With no `-L`, tmux takes its socket from `$TMUX`, which in any environment
/// that launched this suite from a multiplexer pane names a server this process
/// did not create — an *rmux* socket inside a worker, which does not speak
/// tmux's protocol. The manager launch below then fails at
/// `tmux new-session`, `/api/manager/launch` answers an error carrying no
/// `run_id`, and the test panics unwrapping it. The daemon runs in *this*
/// process, so the in-process claim reaches it.
fn tmux_available_for_test() -> bool {
    if !real_tmux_on_path() {
        return false;
    }
    own_tmux_server_for_tests();
    true
}

#[test]
fn required_test_tooling_is_present() {
    assert_required_test_tooling(&[ToolRequirement::new("tmux", 1, tmux_available_for_test())]);
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

/// A port a replacement daemon can rebind, so a CLI that is mid-command keeps
/// talking to *the* daemon across a restart — which is what launchd does with
/// the real one.
fn reserved_local_port() -> u16 {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("reserve a local port");
    let port = listener.local_addr().unwrap().port();
    drop(listener);
    port
}

async fn boot_on_port(home: Home, port: u16) -> RunningDaemon {
    boot_with_options(
        home,
        DaemonOptions {
            port_override: Some(port),
            ..test_options()
        },
    )
    .await
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

struct InterceptingProxy {
    addr: std::net::SocketAddr,
    paths: std::sync::Arc<std::sync::Mutex<Vec<String>>>,
    shutdown: Option<tokio::sync::oneshot::Sender<()>>,
    join: tokio::task::JoinHandle<()>,
}

impl Drop for InterceptingProxy {
    fn drop(&mut self) {
        if let Some(shutdown) = self.shutdown.take() {
            let _ = shutdown.send(());
        }
        self.join.abort();
    }
}

/// A daemon whose `/api/runs` is unavailable, for the recorded-id close path.
async fn start_runs_rejecting_proxy(backend: std::net::SocketAddr) -> InterceptingProxy {
    start_intercepting_proxy(backend, |path| {
        (path == "/api/runs").then_some((503, "Service Unavailable", "runs list disabled"))
    })
    .await
}

/// A daemon that predates TASK-WGXKD: it has no capability route at all, so the
/// pre-flight handshake 404s exactly as it would against a daemon that has not
/// been restarted onto the current runtime. Everything else is the real daemon,
/// so if the client were to release anyway the release would really happen.
async fn start_pre_wgxkd_daemon_proxy(backend: std::net::SocketAddr) -> InterceptingProxy {
    start_intercepting_proxy(backend, |path| {
        (path == "/api/daemon/capabilities").then_some((
            404,
            "Not Found",
            "{\"error\":\"not found\"}",
        ))
    })
    .await
}

/// TCP proxy in front of the daemon that answers `intercept`ed paths itself and
/// forwards everything else, recording every path it saw.
async fn start_intercepting_proxy(
    backend: std::net::SocketAddr,
    intercept: fn(&str) -> Option<(u16, &'static str, &'static str)>,
) -> InterceptingProxy {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind intercepting proxy");
    let addr = listener.local_addr().unwrap();
    let paths = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
    let recorded_paths = paths.clone();
    let (shutdown, mut shutdown_rx) = tokio::sync::oneshot::channel();
    let join = tokio::spawn(async move {
        loop {
            let accepted = tokio::select! {
                _ = &mut shutdown_rx => break,
                accepted = listener.accept() => accepted,
            };
            let Ok((mut inbound, _)) = accepted else {
                break;
            };
            let request_paths = recorded_paths.clone();
            tokio::spawn(async move {
                let mut request = Vec::new();
                let mut chunk = [0_u8; 4096];
                while !request.windows(4).any(|window| window == b"\r\n\r\n") {
                    let read = inbound.read(&mut chunk).await.expect("read proxy request");
                    if read == 0 {
                        return;
                    }
                    request.extend_from_slice(&chunk[..read]);
                    assert!(
                        request.len() <= 64 * 1024,
                        "proxy request headers too large"
                    );
                }
                let first_line = request
                    .split(|byte| *byte == b'\n')
                    .next()
                    .and_then(|line| std::str::from_utf8(line).ok())
                    .unwrap_or_default();
                let path = first_line
                    .split_whitespace()
                    .nth(1)
                    .unwrap_or_default()
                    .to_string();
                request_paths.lock().unwrap().push(path.clone());
                if let Some((status, reason, body)) = intercept(&path) {
                    let response = format!(
                        "HTTP/1.1 {status} {reason}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                        body.len()
                    );
                    inbound.write_all(response.as_bytes()).await.unwrap();
                    inbound.write_all(body.as_bytes()).await.unwrap();
                    return;
                }

                let mut upstream = tokio::net::TcpStream::connect(backend)
                    .await
                    .expect("connect proxy backend");
                let header_end = request
                    .windows(4)
                    .position(|window| window == b"\r\n\r\n")
                    .expect("complete proxy request headers");
                let mut forwarded = Vec::with_capacity(request.len() + 19);
                forwarded.extend_from_slice(&request[..header_end + 2]);
                forwarded.extend_from_slice(b"Connection: close\r\n");
                forwarded.extend_from_slice(&request[header_end + 2..]);
                upstream
                    .write_all(&forwarded)
                    .await
                    .expect("forward proxy request");
                let _ = tokio::io::copy_bidirectional(&mut inbound, &mut upstream).await;
            });
        }
    });
    InterceptingProxy {
        addr,
        paths,
        shutdown: Some(shutdown),
        join,
    }
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
        &project_root.join(".orgasmic/project.org"),
        "#+title: orgasmic\n#+orgasmic_version: 1\n\n* PROJECT orgasmic\n:PROPERTIES:\n:ID:                     orgasmic\n:END:\n",
    );
    write(
        &project_root.join(".orgasmic/tasks/backlog.org"),
        "#+title: backlog\n#+orgasmic_version: 1\n\n* BACKLOG TASK-DISPATCH Dispatch CLI smoke :cli:\n:PROPERTIES:\n:ID:               TASK-DISPATCH\n:END:\n\n* BACKLOG TASK-ABORT Dispatch abort smoke :cli:\n:PROPERTIES:\n:ID:               TASK-ABORT\n:END:\n\n* BACKLOG TASK-FIX Fix subtask smoke :cli:\n:PROPERTIES:\n:ID:               TASK-FIX\n:END:\n\n* BACKLOG TASK-FIX-DECL Declarative fix subtask smoke :cli:\n:PROPERTIES:\n:ID:               TASK-FIX-DECL\n:FIX_SUBTASK:      t\n:END:\n\n* BACKLOG TASK-FIX-FINAL Final fix round smoke :cli:\n:PROPERTIES:\n:ID:               TASK-FIX-FINAL\n:FIX_SUBTASK:      t\n:END:\n\n* BACKLOG TASK-NO-MERGE Missing merge smoke :cli:\n:PROPERTIES:\n:ID:               TASK-NO-MERGE\n:END:\n\n* BACKLOG TASK-BUNDLE-A Bundle smoke A :cli:\n:PROPERTIES:\n:ID:               TASK-BUNDLE-A\n:END:\n\n* BACKLOG TASK-BUNDLE-B Bundle smoke B :cli:\n:PROPERTIES:\n:ID:               TASK-BUNDLE-B\n:END:\n\n* BACKLOG TASK-CLEANUP Cleanup smoke :cli:\n:PROPERTIES:\n:ID:               TASK-CLEANUP\n:END:\n",
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
    // Most dispatch-close tests exercise integration-branch cleanup rather
    // than the default-branch review gate. Give them a real merge commit on a
    // non-default branch so --merge-sha is honest without adding an unrelated
    // review bypass to every fixture. Default-branch enforcement has its own
    // production-shaped regression below.
    run_git(project_root, &["checkout", "-b", "integration"]);
    run_git(project_root, &["checkout", "-b", "fixture-side"]);
    run_git(
        project_root,
        &["commit", "--allow-empty", "-m", "fixture side"],
    );
    run_git(project_root, &["checkout", "integration"]);
    run_git(
        project_root,
        &["merge", "--no-ff", "-m", "fixture merge", "fixture-side"],
    );
    let merge_sha = run_git(project_root, &["rev-parse", "HEAD"]);
    run_git(project_root, &["checkout", "main"]);
    merge_sha
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
    // orgasmic:task_K5NDR
    // `orgasmic_command` scrubs the ambient `ORGASMIC_*` this shell inherited
    // before the explicit `.env` calls below set what the test actually means.
    // Without it a dispatched worker's `ORGASMIC_RUN_ID` reaches `dispatch
    // finalize` and it resolves the OPERATOR's run: `no live run ...`.
    let mut command = orgasmic_command();
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

// orgasmic:TASK-YN5FJ.1.1
/// Link the binary under test into `bin_dir` under its own name, so a command
/// the CLI *printed* can be executed the way an operator would run it: by name,
/// resolved on `PATH`.
///
/// This exists so no helper silently supplies the executable. The shipped test
/// hand-built its argv and let `run_orgasmic` prefix the binary path, which is
/// exactly why the printed remedy could lose its `orgasmic` token with every
/// assertion still green.
fn link_orgasmic_onto_path(bin_dir: &Path) {
    let exe = orgasmic_exe();
    let link = bin_dir.join(exe.file_name().expect("orgasmic binary file name"));
    if link.exists() {
        std::fs::remove_file(&link).unwrap();
    }
    #[cfg(unix)]
    std::os::unix::fs::symlink(&exe, &link).unwrap();
}

// orgasmic:TASK-YN5FJ.1.1
/// The single backticked command in `message` that contains `must_contain`.
///
/// Refusals quote more than one thing in backticks (a sha, a flag), so the
/// caller names what identifies the command. Two matches is an ambiguous
/// remedy and fails here rather than picking one.
fn backticked_command(message: &str, must_contain: &str) -> String {
    let mut spans = message
        .split('`')
        .skip(1)
        .step_by(2)
        .filter(|span| span.contains(must_contain));
    let command = spans.next().unwrap_or_else(|| {
        panic!("refusal prints no backticked command containing `{must_contain}`: {message}")
    });
    assert!(
        spans.next().is_none(),
        "refusal prints more than one backticked command containing `{must_contain}`, \
         so the remedy an operator should paste is ambiguous: {message}"
    );
    command.to_string()
}

// orgasmic:TASK-YN5FJ.1.1
/// Tokenize a printed remedy into an argv, substituting ONLY the placeholders
/// the refusal is documented to leave open.
///
/// The documented set is deliberately tiny: `<reviewer-tx>`, which does not
/// exist yet at refusal time, and the verdict vocabulary, which is a choice the
/// operator makes. Any other `<…>` token means the message printed a
/// placeholder for something it already knew — the assertion below fails the
/// MESSAGE, which is the point: a test that quietly patched such a token would
/// re-hide the defect this test exists to catch.
fn derive_remedy_argv(command: &str, reviewer_tx: &str, verdict: &str) -> Vec<String> {
    const VERDICT_CHOICE: &str = "<approve|approve-with-follow-ups|reject>";
    command
        .split_whitespace()
        .map(|token| match token {
            "<reviewer-tx>" => reviewer_tx.to_string(),
            VERDICT_CHOICE => verdict.to_string(),
            other => {
                assert!(
                    !other.contains('<') && !other.contains('>'),
                    "printed remedy leaves `{other}` as an undocumented placeholder; the refusal \
                     knows this value and must print it: {command}"
                );
                other.to_string()
            }
        })
        .collect()
}

// orgasmic:TASK-YN5FJ.1.1
/// Run a derived argv by NAME through `path_env`, the way pasting it into a
/// shell would.
///
/// The executable is `argv[0]` and nothing else: if the message stops naming
/// one, this fails to spawn instead of quietly running the right binary anyway.
fn run_derived_argv(
    home: &Home,
    running: &RunningDaemon,
    project_root: &Path,
    path_env: &std::ffi::OsString,
    argv: &[String],
) -> Output {
    let mut command = Command::new(&argv[0]);
    scrub_ambient_orgasmic_env(&mut command);
    command
        .args(&argv[1..])
        .current_dir(project_root)
        .env("ORGASMIC_HOME", &home.root)
        .env("ORGASMIC_DAEMON_URL", format!("http://{}", running.addr))
        .env("PATH", path_env);
    command
        .output()
        .unwrap_or_else(|err| panic!("printed remedy {argv:?} is not runnable as printed: {err}"))
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
            "--mode",
            "ws",
            "--harness",
            "codex",
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
            "--mode",
            "ws",
            "--harness",
            "codex",
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
        dispatch_stdout.contains("watch: orgasmic manager dispatch-wait --started-tx "),
        "dispatch must print the generation-aware blocking watcher: {dispatch_stdout}"
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

    let started_tx = started_tx_from_dispatch_stdout(&dispatch_stdout);
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
            "--started-tx",
            &started_tx,
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
            "--mode",
            "stdio",
            "--harness",
            "codex",
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
            "--mode",
            "stdio",
            "--harness",
            "codex",
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

    let review_started_tx = started_tx_from_dispatch_stdout(&review_dispatch_stdout);
    // TASK-QGWK7.1.1 M-5: `:REPORT_PATH:` is committed to the tx log, so an
    // absolute path with no project-relative form is refused HERE, in
    // `cmd_dispatch_close`, before anything is destroyed — not silently written
    // into history for every other clone to read.
    let absolute_report_path = run_orgasmic_failure(
        &home,
        &running,
        &project_root,
        &path_env,
        &[
            "manager",
            "dispatch-close",
            "--task",
            "TASK-REVIEW",
            "--started-tx",
            &review_started_tx,
            "--status",
            "done",
            "--property",
            "VERDICT=clean",
            "--property",
            "REPORT_PATH=/tmp/task-review-report.md",
            "--reviewed-diff",
            "abc123..def456",
            "--reason",
            "review clean",
        ],
    );
    assert!(
        absolute_report_path.contains("project-relative"),
        "an absolute REPORT_PATH outside the project must be refused by name: \
         {absolute_report_path}"
    );
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
            "--started-tx",
            &review_started_tx,
            "--status",
            "done",
            "--property",
            "VERDICT=clean",
            "--property",
            "FINDINGS_TOTAL=0",
            "--property",
            &format!(
                "REPORT_PATH={}",
                project_root.join("docs/task-review-report.md").display()
            ),
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
    // Relativized on the way in, not committed as the manager typed it.
    assert!(tx_raw.contains(":REPORT_PATH:  docs/task-review-report.md"));
    assert!(tx_raw.contains(":REVIEWED_DIFF: abc123..def456"));
    // TASK-QGWK7.1.1 M-5/M-8: the acceptance sentence is "project-relative on
    // EVERY emitter", so assert it across the whole ledger this run produced —
    // finalize, close and the manager-supplied property alike — rather than
    // only on the helper in isolation.
    for line in tx_raw.lines().filter(|line| line.contains(":REPORT_PATH:")) {
        let value = line.split_once(":REPORT_PATH:").unwrap().1.trim();
        assert!(
            !value.starts_with('/'),
            "no emitter may commit a machine-specific absolute REPORT_PATH: {line}"
        );
    }

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
            "--mode",
            "ws",
            "--harness",
            "codex",
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
    let second_started_tx = started_tx_from_dispatch_stdout(&second_dispatch_stdout);
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
            "--started-tx",
            &second_started_tx,
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
            "--mode",
            "ws",
            "--harness",
            "codex",
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
    let fix_started_tx = started_tx_from_dispatch_stdout(&fix_stdout);
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
            "--started-tx",
            &fix_started_tx,
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
            "--mode",
            "ws",
            "--harness",
            "codex",
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
    let fix_decl_started_tx = started_tx_from_dispatch_stdout(&fix_decl_stdout);
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
            "--started-tx",
            &fix_decl_started_tx,
            "--status",
            "done",
            "--merge-sha",
            &head,
        ],
    );
    // orgasmic:TASK-4WKNX — read the stage without asserting on it yet. The
    // property under test is END TO END: the fix round must reach its reviewer
    // with NO manual state transition in between, so the reviewer dispatch is
    // the first thing allowed to fail. Asserting the stage first would report
    // `expected IN_REVIEW` and say nothing about the refusal that is the
    // actual production symptom.
    let sprint_after_fix_decl_close = sprint_source(&project_root);
    let fix_decl_review_brief = codex_dir.join("task-fix-decl-review-brief.md");
    write(&fix_decl_review_brief, "fix round reviewer brief");
    let fix_decl_review_worktree = tmp.path().join("worktrees/task-fix-decl-review");
    let fix_decl_review_stdout = run_orgasmic(
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
            "reviewer",
            "--mode",
            "stdio",
            "--harness",
            "codex",
            "--brief",
            fix_decl_review_brief.to_str().unwrap(),
            "--from",
            &head,
            "--worktree",
            fix_decl_review_worktree.to_str().unwrap(),
            "--branch",
            "task-fix-decl-review",
        ],
    );
    assert!(fix_decl_review_stdout.contains("dispatched: TASK-FIX-DECL reviewer pid="));
    assert!(
        sprint_after_fix_decl_close.contains("* IN_REVIEW TASK-FIX-DECL"),
        "a FIX_SUBTASK close must land in_review by default\n{sprint_after_fix_decl_close}"
    );
    // orgasmic:TASK-4WKNX — and the opt-out belongs to the implementer close
    // that decides the fix round's stage, not to the reviewer's own close.
    let fix_decl_review_started_tx = started_tx_from_dispatch_stdout(&fix_decl_review_stdout);
    let on_reviewer = run_orgasmic_output(
        &home,
        &running,
        &project_root,
        &path_env,
        &[
            "manager",
            "dispatch-close",
            "--task",
            "TASK-FIX-DECL",
            "--started-tx",
            &fix_decl_review_started_tx,
            "--status",
            "done",
            "--verdict",
            "approve",
            "--fix-round-final",
            "--reason",
            "wrong close for this flag",
        ],
    );
    let on_reviewer_stderr = String::from_utf8_lossy(&on_reviewer.stderr).to_string();
    assert!(
        !on_reviewer.status.success()
            && on_reviewer_stderr.contains(
                "--fix-round-final is valid only when closing an implementer dispatch as done"
            ),
        "unexpected --fix-round-final response on a reviewer close: {on_reviewer_stderr}"
    );

    // orgasmic:TASK-4WKNX — and the opt-out closes straight to done, but only
    // with a `--reason`, on the same argument that makes `--no-review-required`
    // require one: a bypass nobody has to justify is a bypass nobody audits.
    let fix_final_brief = codex_dir.join("task-fix-final-brief.md");
    write(&fix_final_brief, "final fix round brief");
    let fix_final_worktree = tmp.path().join("worktrees/task-fix-final");
    let fix_final_stdout = run_orgasmic(
        &home,
        &running,
        &project_root,
        &path_env,
        &[
            "manager",
            "dispatch",
            "--task",
            "TASK-FIX-FINAL",
            "--kind",
            "implementer",
            "--mode",
            "ws",
            "--harness",
            "codex",
            "--brief",
            fix_final_brief.to_str().unwrap(),
            "--from",
            &head,
            "--worktree",
            fix_final_worktree.to_str().unwrap(),
            "--branch",
            "task-fix-final-impl",
        ],
    );
    let fix_final_started_tx = started_tx_from_dispatch_stdout(&fix_final_stdout);
    assert!(
        fix_final_worktree.is_dir(),
        "the dispatched fix-round worktree must exist before close validation"
    );

    // orgasmic:TASK-4WKNX.1 — manager-owned close-tx properties cannot be
    // forged through the generic channel. This is deliberately the first
    // assertion in the injection proof: pre-fix the close succeeds, stamps the
    // caller's FIX_ROUND_FINAL, and removes the worktree.
    let forged_fix_round_final = run_orgasmic_output(
        &home,
        &running,
        &project_root,
        &path_env,
        &[
            "manager",
            "dispatch-close",
            "--task",
            "TASK-FIX-FINAL",
            "--started-tx",
            &fix_final_started_tx,
            "--status",
            "done",
            "--merge-sha",
            &head,
            "--property",
            "FIX_ROUND_FINAL=true",
            "--worktree-remove",
        ],
    );
    let forged_fix_round_final_stdout =
        String::from_utf8_lossy(&forged_fix_round_final.stdout).to_string();
    let forged_fix_round_final_stderr =
        String::from_utf8_lossy(&forged_fix_round_final.stderr).to_string();
    assert!(
        !forged_fix_round_final.status.success()
            && forged_fix_round_final_stderr.contains("--property FIX_ROUND_FINAL=true")
            && forged_fix_round_final_stderr.contains("--fix-round-final"),
        "--property FIX_ROUND_FINAL=true must be refused before manager-owned audit data can be forged\nstdout={forged_fix_round_final_stdout}\nstderr={forged_fix_round_final_stderr}"
    );
    assert_task_stage(
        &project_root,
        "TASK-FIX-FINAL",
        "IN_PROGRESS",
        "in_progress",
    );
    assert!(
        fix_final_worktree.is_dir(),
        "the property-only refusal must happen before worktree cleanup"
    );
    assert!(
        !tx_log(&project_root).split("\n\n* TX ").any(|block| {
            block.contains(":TYPE:         implementer.done")
                && block.contains(":TASK:         TASK-FIX-FINAL")
        }),
        "the property-only refusal must happen before the close tx append"
    );

    let contradicted_fix_round_final = run_orgasmic_output(
        &home,
        &running,
        &project_root,
        &path_env,
        &[
            "manager",
            "dispatch-close",
            "--task",
            "TASK-FIX-FINAL",
            "--started-tx",
            &fix_final_started_tx,
            "--status",
            "done",
            "--merge-sha",
            &head,
            "--fix-round-final",
            "--reason",
            "final round",
            "--property",
            "FIX_ROUND_FINAL=false",
            "--worktree-remove",
        ],
    );
    let contradicted_fix_round_final_stderr =
        String::from_utf8_lossy(&contradicted_fix_round_final.stderr).to_string();
    assert!(
        !contradicted_fix_round_final.status.success()
            && contradicted_fix_round_final_stderr.contains("--fix-round-final")
            && contradicted_fix_round_final_stderr
                .contains("--property FIX_ROUND_FINAL=false"),
        "a typed/property FIX_ROUND_FINAL collision must be refused naming both spellings: {contradicted_fix_round_final_stderr}"
    );
    assert_task_stage(
        &project_root,
        "TASK-FIX-FINAL",
        "IN_PROGRESS",
        "in_progress",
    );
    assert!(
        fix_final_worktree.is_dir(),
        "the FIX_ROUND_FINAL collision must be refused before worktree cleanup"
    );

    // NO_REVIEW_REQUIRED is the same boolean, flag-owned audit class. There
    // are no historical tx rows proving a supported property spelling, so it
    // is reserved instead of acquiring VERDICT's explicit legacy alias.
    for (typed_args, property) in [
        (&[][..], "NO_REVIEW_REQUIRED=true"),
        (
            &["--no-review-required", "--reason", "review bypass"] as &[&str],
            "NO_REVIEW_REQUIRED=false",
        ),
    ] {
        let mut argv = vec![
            "manager",
            "dispatch-close",
            "--task",
            "TASK-FIX-FINAL",
            "--started-tx",
            fix_final_started_tx.as_str(),
            "--status",
            "done",
            "--merge-sha",
            head.as_str(),
        ];
        argv.extend_from_slice(typed_args);
        argv.extend_from_slice(&["--property", property, "--worktree-remove"]);
        let refused = run_orgasmic_output(&home, &running, &project_root, &path_env, &argv);
        let stderr = String::from_utf8_lossy(&refused.stderr).to_string();
        assert!(
            !refused.status.success()
                && stderr.contains(&format!("--property {property}"))
                && stderr.contains("--no-review-required"),
            "reserved NO_REVIEW_REQUIRED spelling must be refused naming its typed flag: {stderr}"
        );
        assert_task_stage(
            &project_root,
            "TASK-FIX-FINAL",
            "IN_PROGRESS",
            "in_progress",
        );
        assert!(
            fix_final_worktree.is_dir(),
            "the NO_REVIEW_REQUIRED refusal must happen before worktree cleanup"
        );
    }

    let no_reason = run_orgasmic_output(
        &home,
        &running,
        &project_root,
        &path_env,
        &[
            "manager",
            "dispatch-close",
            "--task",
            "TASK-FIX-FINAL",
            "--started-tx",
            &fix_final_started_tx,
            "--status",
            "done",
            "--merge-sha",
            &head,
            "--fix-round-final",
        ],
    );
    let no_reason_stderr = String::from_utf8_lossy(&no_reason.stderr).to_string();
    assert!(
        !no_reason.status.success()
            && no_reason_stderr
                .contains("--fix-round-final requires --reason so the skipped review is auditable"),
        "unexpected reasonless --fix-round-final response: {no_reason_stderr}"
    );
    let forged_closed_tx = format!("CLOSED_TX={fix_decl_review_started_tx}");
    run_orgasmic(
        &home,
        &running,
        &project_root,
        &path_env,
        &[
            "manager",
            "dispatch-close",
            "--task",
            "TASK-FIX-FINAL",
            "--started-tx",
            &fix_final_started_tx,
            "--status",
            "done",
            "--merge-sha",
            &head,
            "--fix-round-final",
            "--reason",
            "one-line comment typo, nothing to review",
            "--property",
            &forged_closed_tx,
        ],
    );
    // orgasmic:TASK-4WKNX.1.1 — this pins the consumed consequence, not merely
    // the duplicate's order in the ledger. Before manager-first construction,
    // close_matching_dispatch reads the forged CLOSED_TX, misses this dispatch,
    // and leaves it open even though dispatch-close reported success.
    let fix_final_status = run_orgasmic(
        &home,
        &running,
        &project_root,
        &path_env,
        &["manager", "dispatch-status", "--task", "TASK-FIX-FINAL"],
    );
    assert!(
        fix_final_status.trim().is_empty(),
        "a forged CLOSED_TX must not change which dispatch the close consumer terminates: {fix_final_status}"
    );
    assert_task_stage(&project_root, "TASK-FIX-FINAL", "DONE", "done");
    assert!(
        tx_log(&project_root).contains(":FIX_ROUND_FINAL: true"),
        "the opt-out must be stamped on the close tx\n{}",
        tx_log(&project_root)
    );

    let abort_brief = codex_dir.join("task-abort-brief.md");
    write(&abort_brief, "abort brief");
    let abort_last = codex_dir.join("task-abort-last.txt");
    let abort_worktree = tmp.path().join("worktrees/task-abort");
    let abort_dispatch_stdout = run_orgasmic(
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
            "--mode",
            "ws",
            "--harness",
            "codex",
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
    let abort_started_tx = started_tx_from_dispatch_stdout(&abort_dispatch_stdout);
    let fix_round_final_on_abort = run_orgasmic_output(
        &home,
        &running,
        &project_root,
        &path_env,
        &[
            "manager",
            "dispatch-close",
            "--task",
            "TASK-ABORT",
            "--started-tx",
            &abort_started_tx,
            "--status",
            "aborted",
            "--fix-round-final",
            "--reason",
            "aborting is not a final fix round",
            "--worktree-remove",
        ],
    );
    let fix_round_final_on_abort_stderr =
        String::from_utf8_lossy(&fix_round_final_on_abort.stderr).to_string();
    assert!(
        !fix_round_final_on_abort.status.success()
            && fix_round_final_on_abort_stderr.contains(
                "--fix-round-final is valid only when closing an implementer dispatch as done"
            ),
        "an aborted close must reach the named --fix-round-final refusal: {fix_round_final_on_abort_stderr}"
    );
    assert!(
        abort_worktree.is_dir(),
        "the aborted-close refusal must happen before worktree cleanup"
    );
    assert_task_stage(&project_root, "TASK-ABORT", "IN_PROGRESS", "in_progress");

    // orgasmic:TASK-4WKNX — the opt-out is a FIX_SUBTASK concept. On a task
    // that carries no `:FIX_SUBTASK:` it would be a silent no-op (that close
    // already lands `in_review`), so it is refused by name instead.
    let not_a_fix_round = run_orgasmic_output(
        &home,
        &running,
        &project_root,
        &path_env,
        &[
            "manager",
            "dispatch-close",
            "--task",
            "TASK-ABORT",
            "--started-tx",
            &abort_started_tx,
            "--status",
            "done",
            "--merge-sha",
            &head,
            "--fix-round-final",
            "--reason",
            "not a fix round",
        ],
    );
    let not_a_fix_round_stderr = String::from_utf8_lossy(&not_a_fix_round.stderr).to_string();
    assert!(
        !not_a_fix_round.status.success()
            && not_a_fix_round_stderr
                .contains("--fix-round-final is valid only for a task carrying :FIX_SUBTASK:"),
        "unexpected --fix-round-final response on a non-fix task: {not_a_fix_round_stderr}"
    );
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
            "--started-tx",
            &abort_started_tx,
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
            "--mode",
            "ws",
            "--harness",
            "codex",
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
            "--mode",
            "ws",
            "--harness",
            "codex",
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
async fn reviewer_dispatches_before_reported_implementer_close_and_unlocks_main_merge() {
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
    let implementer_brief = tmp.path().join("codex/review-before-close-impl.md");
    let implementer_worktree = tmp.path().join("worktrees/review-before-close-impl");

    let running = boot(home.clone()).await;
    let implementer_started_tx = dispatch_sleeping_implementer(
        &home,
        &running,
        &project_root,
        &path_env,
        &head,
        &implementer_worktree,
        &implementer_brief,
    )
    .await;
    write(
        &implementer_worktree.join("reviewed-worker-change.txt"),
        "worker output\n",
    );
    let summary = tmp.path().join("implementer-summary.md");
    write(&summary, "reported implementer");
    run_orgasmic(
        &home,
        &running,
        &implementer_worktree,
        &path_env,
        &[
            "dispatch",
            "finalize",
            "--task",
            "TASK-DISPATCH",
            "--summary-file",
            summary.to_str().unwrap(),
            "--commit",
        ],
    );
    let worker_sha = run_git(&implementer_worktree, &["rev-parse", "HEAD"]);

    let second_impl_brief = tmp.path().join("codex/review-before-close-second-impl.md");
    write(&second_impl_brief, "second implementer must collide");
    let second_impl_worktree = tmp.path().join("worktrees/review-before-close-second-impl");
    let same_kind_error = run_orgasmic_failure(
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
            "--mode",
            "ws",
            "--harness",
            "codex",
            "--brief",
            second_impl_brief.to_str().unwrap(),
            "--from",
            &worker_sha,
            "--worktree",
            second_impl_worktree.to_str().unwrap(),
            "--branch",
            "task-review-before-close-second-impl",
        ],
    );
    assert!(
        same_kind_error.contains("a second implementer dispatch still collides"),
        "reported same-kind dispatch must remain blocked: {same_kind_error}"
    );

    let reviewer_brief = tmp.path().join("codex/review-before-close-review.md");
    write(&reviewer_brief, "review the reported implementer");
    let reviewer_worktree = tmp.path().join("worktrees/review-before-close-review");
    let reviewer_stdout = run_orgasmic(
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
            "reviewer",
            "--mode",
            "ws",
            "--harness",
            "codex",
            "--brief",
            reviewer_brief.to_str().unwrap(),
            "--from",
            &worker_sha,
            "--worktree",
            reviewer_worktree.to_str().unwrap(),
            "--branch",
            "task-review-before-close-review",
        ],
    );
    assert!(
        reviewer_stdout.contains("dispatched: TASK-DISPATCH reviewer pid="),
        "reviewer must dispatch before implementer close: {reviewer_stdout}"
    );
    let reviewer_started_tx = started_tx_from_dispatch_stdout(&reviewer_stdout);
    let tx_before_review_close = tx_log(&project_root);
    assert!(
        tx_before_review_close.contains(&format!(":REVIEWS_TX:   {implementer_started_tx}")),
        "reviewer generation must name the reported implementer generation: \
         {tx_before_review_close}"
    );
    run_orgasmic(
        &home,
        &running,
        &project_root,
        &path_env,
        &[
            "manager",
            "dispatch-close",
            "--task",
            "TASK-DISPATCH",
            "--started-tx",
            &reviewer_started_tx,
            "--status",
            "done",
            "--property",
            "VERDICT=ship",
            "--reviewed-diff",
            "main..task-dispatch-test-impl",
        ],
    );

    run_git(&project_root, &["checkout", "main"]);
    run_git(
        &project_root,
        &[
            "merge",
            "--no-ff",
            "-m",
            "merge reviewed worker",
            "task-dispatch-test-impl",
        ],
    );
    let merge_sha = run_git(&project_root, &["rev-parse", "HEAD"]);
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
            "--started-tx",
            &implementer_started_tx,
            "--status",
            "done",
            "--merge-sha",
            &merge_sha,
            "--worker-commit",
            &worker_sha,
        ],
    );
    assert!(
        close_stdout.contains("closed: TASK-DISPATCH implementer.done tx="),
        "linked reviewer verdict must unlock a verified default-branch close: {close_stdout}"
    );

    let _ = running.shutdown.send(());
    let _ = running.join.await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn dispatch_close_rejects_unverified_merge_evidence_and_records_review_bypass() {
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
    let brief = tmp.path().join("codex/merge-evidence.md");
    write(&brief, "merge evidence regression");
    let worktree = tmp.path().join("worktrees/merge-evidence");

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
            "--mode",
            "ws",
            "--harness",
            "codex",
            "--brief",
            brief.to_str().unwrap(),
            "--from",
            &head,
            "--worktree",
            worktree.to_str().unwrap(),
            "--branch",
            "task-merge-evidence-impl",
        ],
    );
    let started_tx = started_tx_from_dispatch_stdout(&dispatch_stdout);
    write(&worktree.join("worker-evidence.txt"), "worker evidence\n");
    run_git(&worktree, &["add", "worker-evidence.txt"]);
    run_git(&worktree, &["commit", "-m", "worker evidence"]);
    let worker_sha = run_git(&worktree, &["rev-parse", "HEAD"]);

    let close_prefix = [
        "manager",
        "dispatch-close",
        "--task",
        "TASK-DISPATCH",
        "--started-tx",
        started_tx.as_str(),
        "--status",
        "done",
        "--merge-sha",
    ];
    let unresolved = run_orgasmic_failure(
        &home,
        &running,
        &project_root,
        &path_env,
        &[&close_prefix[..], &["not-a-real-merge-sha"]].concat(),
    );
    assert!(
        unresolved.contains("--merge-sha `not-a-real-merge-sha` does not resolve"),
        "unresolved string must be refused by name: {unresolved}"
    );
    let non_merge = run_orgasmic_failure(
        &home,
        &running,
        &project_root,
        &path_env,
        &[&close_prefix[..], &[worker_sha.as_str()]].concat(),
    );
    assert!(
        non_merge.contains("is not a merge commit"),
        "non-merge commit must be refused by name: {non_merge}"
    );

    run_git(&project_root, &["checkout", "main"]);
    run_git(&project_root, &["checkout", "-b", "unrelated-worker"]);
    run_git(
        &project_root,
        &["commit", "--allow-empty", "-m", "unrelated worker"],
    );
    let unrelated_worker = run_git(&project_root, &["rev-parse", "HEAD"]);
    run_git(&project_root, &["checkout", "main"]);
    run_git(
        &project_root,
        &[
            "merge",
            "--no-ff",
            "-m",
            "merge evidence worker",
            "task-merge-evidence-impl",
        ],
    );
    let merge_sha = run_git(&project_root, &["rev-parse", "HEAD"]);

    let not_contained_args = [
        &close_prefix[..],
        &[
            merge_sha.as_str(),
            "--worker-commit",
            unrelated_worker.as_str(),
        ],
    ]
    .concat();
    let not_contained = run_orgasmic_failure(
        &home,
        &running,
        &project_root,
        &path_env,
        &not_contained_args,
    );
    assert!(
        not_contained.contains("does not contain --worker-commit"),
        "merge missing the worker commit must be refused by name: {not_contained}"
    );

    let verified_args = [
        &close_prefix[..],
        &[merge_sha.as_str(), "--worker-commit", worker_sha.as_str()],
    ]
    .concat();
    let no_verdict =
        run_orgasmic_failure(&home, &running, &project_root, &path_env, &verified_args);
    assert!(
        no_verdict.contains("no reviewer verdict exists")
            && no_verdict.contains("--no-review-required --reason <why>"),
        "default-branch refusal must name the missing verdict and remedy: {no_verdict}"
    );

    let no_reason_args = [&verified_args[..], &["--no-review-required"]].concat();
    let no_reason =
        run_orgasmic_failure(&home, &running, &project_root, &path_env, &no_reason_args);
    assert!(
        no_reason.contains("--no-review-required requires --reason"),
        "review bypass must refuse an invisible reason: {no_reason}"
    );

    let bypass_args = [
        &verified_args[..],
        &[
            "--no-review-required",
            "--reason",
            "documentation-only change",
            "--no-worktree-remove",
        ],
    ]
    .concat();
    let bypass_close = run_orgasmic(&home, &running, &project_root, &path_env, &bypass_args);
    assert!(bypass_close.contains("closed: TASK-DISPATCH implementer.done tx="));
    let tx = tx_log(&project_root);
    assert!(tx.contains(":NO_REVIEW_REQUIRED: true"));
    assert!(tx.contains(":REASON:       documentation-only change"));
    assert!(tx.contains(&format!(":MERGE_SHA:    {merge_sha}")));
    assert!(tx.contains(&format!(":WORKER_COMMIT: {worker_sha}")));

    let _ = running.shutdown.send(());
    let _ = running.join.await;
}

/// TASK-YN5FJ.1: the defect was that the refusal's own remedy did not clear the
/// refusal. `VERDICT` had no flag — only the generic `--property VERDICT=` set
/// it — so an operator who followed "dispatch and close a reviewer for that
/// reported generation" verbatim produced an ordinary `reviewer.done` with no
/// `VERDICT` and was refused again with the same message. A test that only
/// asserts `--verdict` is accepted cannot see that, so this walks the whole
/// loop: refuse, follow the PRINTED remedy, succeed.
///
/// TASK-YN5FJ.1.1 made "the PRINTED remedy" literal. The first pass wrote the
/// word VERBATIM over a hand-built argv, so the message could stop being
/// copyable — it shipped without its `orgasmic` token — with this test still
/// green. The close below is now extracted from the refusal string this run
/// produced, substituted only where the message says it is a placeholder, and
/// executed by the name the message printed.
async fn review_gate_refusal_remedy_loop(verdict: &str, expected_stage: (&str, &str)) {
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
    // TASK-YN5FJ.1.1: the reviewer close below is the command the refusal
    // printed, run by the name the refusal printed. That name has to resolve on
    // this test's PATH, or the printed remedy is not one a human could paste.
    link_orgasmic_onto_path(&bin_dir);
    let path_env = path_with_stub(&bin_dir);
    let implementer_brief = tmp.path().join("codex/verdict-remedy-impl.md");
    let implementer_worktree = tmp.path().join("worktrees/verdict-remedy-impl");

    let running = boot(home.clone()).await;
    let implementer_started_tx = dispatch_sleeping_implementer(
        &home,
        &running,
        &project_root,
        &path_env,
        &head,
        &implementer_worktree,
        &implementer_brief,
    )
    .await;
    write(
        &implementer_worktree.join("verdict-remedy-change.txt"),
        "worker output\n",
    );
    let summary = tmp.path().join("verdict-remedy-summary.md");
    write(&summary, "reported implementer");
    run_orgasmic(
        &home,
        &running,
        &implementer_worktree,
        &path_env,
        &[
            "dispatch",
            "finalize",
            "--task",
            "TASK-DISPATCH",
            "--summary-file",
            summary.to_str().unwrap(),
            "--commit",
        ],
    );
    let worker_sha = run_git(&implementer_worktree, &["rev-parse", "HEAD"]);

    run_git(&project_root, &["checkout", "main"]);
    run_git(
        &project_root,
        &[
            "merge",
            "--no-ff",
            "-m",
            "merge reviewed worker",
            "task-dispatch-test-impl",
        ],
    );
    let merge_sha = run_git(&project_root, &["rev-parse", "HEAD"]);

    let implementer_close = [
        "manager",
        "dispatch-close",
        "--task",
        "TASK-DISPATCH",
        "--started-tx",
        implementer_started_tx.as_str(),
        "--status",
        "done",
        "--merge-sha",
        merge_sha.as_str(),
        "--worker-commit",
        worker_sha.as_str(),
    ];

    let refusal = run_orgasmic_failure(
        &home,
        &running,
        &project_root,
        &path_env,
        &implementer_close,
    );
    assert!(
        refusal.contains("no reviewer verdict exists"),
        "default-branch refusal must name the missing verdict: {refusal}"
    );
    // The whole point of the fix: the refusal has to be SELF-CONTAINED — it
    // names the requirement (a reviewer.done carrying a VERDICT) and the exact
    // flag that records one, values included.
    assert!(
        refusal.contains("carries a VERDICT")
            && refusal.contains("--verdict <approve|approve-with-follow-ups|reject>")
            && refusal.contains("including reject"),
        "refusal must name the verdict requirement and the --verdict remedy: {refusal}"
    );

    // `--verdict` belongs to the reviewer close, fenced the same way
    // `--no-review-required` is fenced to the implementer close.
    let verdict_on_implementer = run_orgasmic_failure(
        &home,
        &running,
        &project_root,
        &path_env,
        &[&implementer_close[..], &["--verdict", verdict]].concat(),
    );
    assert!(
        verdict_on_implementer
            .contains("--verdict is valid only when closing a reviewer dispatch as done"),
        "--verdict on a non-reviewer close must be refused by name: {verdict_on_implementer}"
    );

    // TASK-YN5FJ.1.1: the close below is DERIVED from `refusal`, not written
    // here. Extract the command the CLI actually printed now, while the exact
    // bytes of that refusal are still in hand — everything after this point runs
    // what the message said, so an edit that makes the message unrunnable makes
    // this test red.
    let printed_remedy = backticked_command(&refusal, "dispatch-close");

    // Follow the printed remedy VERBATIM: dispatch a reviewer for that reported
    // generation, then close it with `--verdict <value>`.
    let reviewer_brief = tmp.path().join("codex/verdict-remedy-review.md");
    write(&reviewer_brief, "review the reported implementer");
    let reviewer_worktree = tmp.path().join("worktrees/verdict-remedy-review");
    let reviewer_stdout = run_orgasmic(
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
            "reviewer",
            "--mode",
            "ws",
            "--harness",
            "codex",
            "--brief",
            reviewer_brief.to_str().unwrap(),
            "--from",
            &worker_sha,
            "--worktree",
            reviewer_worktree.to_str().unwrap(),
            "--branch",
            "task-verdict-remedy-review",
        ],
    );
    let reviewer_started_tx = started_tx_from_dispatch_stdout(&reviewer_stdout);

    // Substitute the two documented placeholders and NOTHING else — anything
    // the refusal already knew has to arrive here as a literal token, and
    // `derive_remedy_argv` fails the message if it did not.
    let remedy_argv = derive_remedy_argv(&printed_remedy, &reviewer_started_tx, verdict);
    // The message names its own executable, and it must be this CLI's. Checked
    // against the built binary rather than assumed, because the shipped defect
    // was precisely a missing executable token that a helper supplied instead.
    let expected_exe = orgasmic_exe()
        .file_name()
        .expect("orgasmic binary file name")
        .to_string_lossy()
        .to_string();
    assert_eq!(
        remedy_argv.first().map(String::as_str),
        Some(expected_exe.as_str()),
        "printed remedy must begin with the executable an operator would type: {printed_remedy}"
    );
    let remedy_output = run_derived_argv(&home, &running, &project_root, &path_env, &remedy_argv);
    assert!(
        remedy_output.status.success(),
        "the reviewer close the refusal PRINTED must run as printed: {remedy_argv:?}\nstdout={}\nstderr={}",
        String::from_utf8_lossy(&remedy_output.stdout),
        String::from_utf8_lossy(&remedy_output.stderr)
    );
    let tx_after_review = tx_log(&project_root);
    assert!(
        tx_after_review
            .lines()
            .any(|line| line.starts_with(":VERDICT:") && line.contains(verdict)),
        "--verdict must record the same VERDICT property the gate reads: {tx_after_review}"
    );
    assert_task_stage(
        &project_root,
        "TASK-DISPATCH",
        expected_stage.0,
        expected_stage.1,
    );

    let close_stdout = run_orgasmic(
        &home,
        &running,
        &project_root,
        &path_env,
        &implementer_close,
    );
    assert!(
        close_stdout.contains("closed: TASK-DISPATCH implementer.done tx="),
        "following the refusal's printed remedy must clear the refusal: {close_stdout}"
    );

    let _ = running.shutdown.send(());
    let _ = running.join.await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn review_gate_refusal_remedy_clears_the_refusal() {
    review_gate_refusal_remedy_loop("approve", ("DONE", "done")).await;
}

/// TASK-YN5FJ.1 RULING 1: a `reject` verdict satisfies the gate too. The gate
/// asks whether an independent review happened and said something, not whether
/// it approved — a reject the manager then resolves in a follow-up commit is a
/// normal outcome here, and the alternative (`--no-review-required`) would
/// stamp NO_REVIEW_REQUIRED=true on a dispatch that WAS reviewed. The reject's
/// consequence lands on the task's stage instead.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn review_gate_is_cleared_by_a_reject_verdict() {
    review_gate_refusal_remedy_loop("reject", ("IN_PROGRESS", "in_progress")).await;
}

/// TASK-YN5FJ.1 RULING 3: both spellings write the same `VERDICT` key and the
/// property reader is last-wins, so a conflict is an error rather than a silent
/// winner.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn reviewer_close_refuses_verdict_flag_alongside_property_verdict() {
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
    let brief = codex_dir.join("task-verdict-conflict-brief.md");
    write(&brief, "reviewer verdict conflict brief");
    let worktree = tmp.path().join("worktrees/task-verdict-conflict");

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
            "TASK-SHIP-CLEAN",
            "--kind",
            "reviewer",
            "--mode",
            "stdio",
            "--harness",
            "codex",
            "--brief",
            brief.to_str().unwrap(),
            "--from",
            &head,
            "--worktree",
            worktree.to_str().unwrap(),
            "--branch",
            "task-verdict-conflict",
        ],
    );
    let started_tx = started_tx_from_dispatch_stdout(&dispatch_stdout);
    let close_prefix = [
        "manager",
        "dispatch-close",
        "--task",
        "TASK-SHIP-CLEAN",
        "--started-tx",
        started_tx.as_str(),
        "--status",
        "done",
    ];
    let conflict = run_orgasmic_failure(
        &home,
        &running,
        &project_root,
        &path_env,
        &[
            &close_prefix[..],
            &["--verdict", "approve", "--property", "VERDICT=clean"],
        ]
        .concat(),
    );
    assert!(
        conflict.contains("--verdict approve") && conflict.contains("--property VERDICT=clean"),
        "a VERDICT conflict must name both spellings: {conflict}"
    );

    run_orgasmic(
        &home,
        &running,
        &project_root,
        &path_env,
        &[
            &close_prefix[..],
            &[
                "--verdict",
                "approve",
                "--property",
                "RECOMMENDED_SUBTASKS=-",
            ],
        ]
        .concat(),
    );
    // `approve` joins the legacy `clean`/`ship` as a clean verdict.
    assert_task_stage(&project_root, "TASK-SHIP-CLEAN", "DONE", "done");
    let tx = tx_log(&project_root);
    assert!(
        tx.lines()
            .any(|line| line.starts_with(":VERDICT:") && line.contains("approve")),
        "--verdict must write the VERDICT property: {tx}"
    );

    let _ = running.shutdown.send(());
    let _ = running.join.await;
}

/// TASK-YN5FJ.1: the stage mapping is a pure superset of today's — the two
/// non-clean canonical verdicts land where every other non-clean value already
/// lands.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn reviewer_close_non_clean_verdict_flags_stay_in_progress() {
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
    for (task, verdict, slug) in [
        ("TASK-HAS-ISSUES", "reject", "task-verdict-reject"),
        (
            "TASK-REVIEW-ISSUES",
            "approve-with-follow-ups",
            "task-verdict-follow-ups",
        ),
    ] {
        let brief = codex_dir.join(format!("{slug}-brief.md"));
        write(&brief, "reviewer verdict stage brief");
        let worktree = tmp.path().join("worktrees").join(slug);
        let dispatch_stdout = run_orgasmic(
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
                "reviewer",
                "--mode",
                "stdio",
                "--harness",
                "codex",
                "--brief",
                brief.to_str().unwrap(),
                "--from",
                &head,
                "--worktree",
                worktree.to_str().unwrap(),
                "--branch",
                slug,
            ],
        );
        let started_tx = started_tx_from_dispatch_stdout(&dispatch_stdout);
        run_orgasmic(
            &home,
            &running,
            &project_root,
            &path_env,
            &[
                "manager",
                "dispatch-close",
                "--task",
                task,
                "--started-tx",
                &started_tx,
                "--status",
                "done",
                "--verdict",
                verdict,
                "--property",
                "RECOMMENDED_SUBTASKS=-",
            ],
        );
        assert_task_stage(&project_root, task, "IN_PROGRESS", "in_progress");
    }

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
            "--mode",
            "tmux",
            "--harness",
            "custom",
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
        stderr.contains("daemon dispatch failed") || stderr.contains("unsupported mode/harness"),
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
    let dispatch_stdout = run_orgasmic(
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
            "--mode",
            "stdio",
            "--harness",
            "codex",
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
    let started_tx = started_tx_from_dispatch_stdout(&dispatch_stdout);
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
            "--started-tx",
            &started_tx,
            "--status",
            "done",
            "--property",
            "VERDICT=has-issues",
            "--property",
            "REPORT_PATH=docs/task-review-issues.md",
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
    let dispatch_stdout = run_orgasmic(
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
            "--mode",
            "stdio",
            "--harness",
            "codex",
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
    let started_tx = started_tx_from_dispatch_stdout(&dispatch_stdout);
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
            "--started-tx",
            &started_tx,
            "--status",
            "done",
            "--property",
            "VERDICT=ship",
            "--property",
            "REPORT_PATH=docs/task-ship-clean.md",
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
    let dispatch_stdout = run_orgasmic(
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
            "--mode",
            "stdio",
            "--harness",
            "codex",
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
    let started_tx = started_tx_from_dispatch_stdout(&dispatch_stdout);
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
            "--started-tx",
            &started_tx,
            "--status",
            "done",
            "--property",
            "VERDICT=has-issues",
            "--property",
            "REPORT_PATH=docs/task-has-issues.md",
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
            "--mode",
            "ws",
            "--harness",
            "codex",
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

    let started_tx = started_tx_from_dispatch_stdout(&dispatch_stdout);
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
            "--started-tx",
            &started_tx,
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
            "--mode",
            "ws",
            "--harness",
            "codex",
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
            "--started-tx",
            &start_tx,
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
    let dispatch_stdout = run_orgasmic(
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
            "--mode",
            "ws",
            "--harness",
            "codex",
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
    let started_tx = started_tx_from_dispatch_stdout(&dispatch_stdout);
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
            "--started-tx",
            &started_tx,
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
async fn dispatch_address_shows_in_dry_run_plan() {
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
    let brief = codex_dir.join("task-address-brief.md");
    write(&brief, "dispatch address brief");

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
            "--mode",
            "stdio",
            "--harness",
            "codex",
            "--dry-run",
        ],
    );
    assert!(stdout.contains("dispatch plan:"));
    assert!(stdout.contains("mode:     stdio"));
    assert!(stdout.contains("harness:  codex"));

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

    // Default worktree suffixes mirror `default_worktree` in manager.rs: since
    // TASK-M47E5 the managed root is `<home>/worktrees/<project-id>/`, keyed on
    // the project id seeded by `seed_project` (`orgasmic`).
    // `TASK-DISPATCH` is BACKLOG (dispatchable as implementer);
    // `TASK-REVIEW-ISSUES` is IN_REVIEW (dispatchable as reviewer).
    let managed_root = home.root.join("worktrees/orgasmic");
    let dispatch_review_default = managed_root
        .join("task-dispatch-review")
        .display()
        .to_string();
    let review_issues_impl_default = managed_root
        .join("task-review-issues")
        .display()
        .to_string();

    // (task, kind, colliding --worktree, expected substring) — exhausts the
    // cross-kind matrix the collision loop guards.
    let cases: &[(&str, &str, &str, &str)] = &[
        (
            "TASK-DISPATCH",
            "implementer",
            dispatch_review_default.as_str(),
            "implementer worktree must not reuse reviewer default path:",
        ),
        (
            "TASK-REVIEW-ISSUES",
            "reviewer",
            review_issues_impl_default.as_str(),
            "reviewer worktree must not reuse implementer default path:",
        ),
    ];

    for (task, kind, worktree, expected) in cases {
        let (mode, harness) = match *kind {
            "reviewer" => ("stdio", "codex"),
            _ => ("ws", "codex"),
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
                "--mode",
                mode,
                "--harness",
                harness,
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
    for worktree in [&dispatch_review_default, &review_issues_impl_default] {
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
            "--mode",
            "ws",
            "--harness",
            "codex",
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

/// TASK-M47E5: default dispatch worktrees live under
/// `<home>/worktrees/<project-id>/<stem>` — OUTSIDE the project, because a
/// project rooted in a TCC-guarded directory (`~/Documents`) makes every
/// freshly built worker binary a stranger to macOS. `--dry-run` is the surface
/// a manager checks before trusting a dispatch, so the resolved path must be
/// visible there.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn dispatch_default_worktree_lives_under_the_home_worktrees_root() {
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
            "--mode",
            "ws",
            "--harness",
            "codex",
            "--brief",
            brief.to_str().unwrap(),
            "--from",
            &head,
            "--dry-run",
        ],
    );
    // The plan prints a normalized (symlink-resolved) path; on macOS the
    // tempdir's `/var` is a symlink to `/private/var`, so canonicalize the
    // expectation rather than the output.
    let expected = std::fs::canonicalize(&home.root)
        .unwrap()
        .join("worktrees/orgasmic/task-dispatch")
        .display()
        .to_string();
    assert!(
        stdout.contains(&format!("worktree: {expected}")),
        "dry-run should show the managed home worktree path, got:\n{stdout}"
    );
    // And nothing may still point at the old in-project location.
    let old_default = std::fs::canonicalize(&project_root)
        .unwrap()
        .join(".orgasmic/tmp/dispatch/task-dispatch/worktree")
        .display()
        .to_string();
    assert!(
        !stdout.contains(&old_default),
        "dry-run must not name the retired in-project worktree path, got:\n{stdout}"
    );
    // The dispatch RECORD stays in the project: only the scratch moved.
    let brief_dir = std::fs::canonicalize(&project_root)
        .unwrap()
        .join(".orgasmic/tmp/dispatch/task-dispatch")
        .display()
        .to_string();
    assert!(
        stdout.contains(&brief_dir),
        "dry-run should still place brief/last/stdout in the project, got:\n{stdout}"
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
            "--mode",
            "ws",
            "--harness",
            "codex",
            "--brief",
            brief.to_str().unwrap(),
            "--from",
            &head,
            "--reason",
            "layout regression",
        ],
    );
    let worktree = home.root.join("worktrees/orgasmic/task-dispatch");
    assert!(
        worktree.is_dir(),
        "default worktree should exist at {}",
        worktree.display()
    );
    assert!(
        !project_root
            .join(".orgasmic/tmp/dispatch/task-dispatch/worktree")
            .exists(),
        "nothing may be created at the retired in-project worktree path"
    );

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
        "the dispatch record under .orgasmic/tmp must stay gitignored; git status:\n{porcelain}"
    );

    let _ = running.shutdown.send(());
    let _ = running.join.await;
}

/// Closing a dispatch promotes the selected attempt's report out of gitignored
/// tmp into a tracked path keyed by the dispatch generation, while retaining
/// the brief and sibling attempts (TASK-QGWK7).
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
            "--mode",
            "ws",
            "--harness",
            "codex",
            "--brief",
            brief.to_str().unwrap(),
            "--from",
            &head,
            "--reason",
            "stem cleanup regression",
        ],
    );
    let started_tx = started_tx_from_dispatch_stdout(&dispatch_stdout);
    // TASK-M47E5: the scratch lives under the home; the stem dir keeps only the
    // RECORD, which close promotes out of tmp/ (TASK-QGWK7).
    let worktree = home.root.join("worktrees/orgasmic/task-dispatch");
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
            "--started-tx",
            &started_tx,
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
        "selected attempt last.txt should leave tmp/ on close"
    );
    assert!(
        !attempt_stdout.exists(),
        "selected attempt stdout.log should leave tmp/ on close"
    );
    let promoted_last =
        project_root.join(format!(".orgasmic/dispatch-records/{started_tx}/last.txt"));
    let promoted_stdout = project_root.join(format!(
        ".orgasmic/dispatch-records/{started_tx}/stdout.log"
    ));
    assert!(
        promoted_last.exists(),
        "after close the report must still be readable from the path the tx names: {}",
        promoted_last.display()
    );
    assert_eq!(
        std::fs::read_to_string(&promoted_last).unwrap(),
        "worker summary"
    );
    assert!(
        promoted_stdout.exists(),
        "stdout.log is promoted beside last.txt as harness evidence"
    );
    let close_tx = tx_log(&project_root);
    assert!(
        close_tx.contains(&format!(
            ":REPORT_PATH:  .orgasmic/dispatch-records/{started_tx}/last.txt"
        )),
        "close tx must name the promoted report: {close_tx}"
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
    let head = init_git_project(&project_root);
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
            "--started-tx",
            "tx-start-cleanup",
            "--status",
            "done",
            "--merge-sha",
            &head,
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

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn dispatch_close_with_recorded_run_id_does_not_enumerate_runs() {
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
    let brief = tmp.path().join("codex/direct-close-brief.md");
    write(&brief, "direct close without run enumeration");
    let worktree = tmp.path().join("worktrees/direct-close");

    let running = boot(home.clone()).await;
    let dispatched = run_orgasmic(
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
            "--mode",
            "ws",
            "--harness",
            "codex",
            "--brief",
            brief.to_str().unwrap(),
            "--from",
            &head,
            "--worktree",
            worktree.to_str().unwrap(),
            "--branch",
            "task-direct-close",
        ],
    );
    assert!(dispatched.contains("dispatched: TASK-DISPATCH implementer pid="));
    let started_tx = started_tx_from_dispatch_stdout(&dispatched);

    let proxy = start_runs_rejecting_proxy(running.addr).await;
    let output = run_orgasmic_output_with_daemon_url(
        &home,
        &format!("http://{}", proxy.addr),
        &project_root,
        &path_env,
        &[
            "manager",
            "dispatch-close",
            "--task",
            "TASK-DISPATCH",
            "--started-tx",
            &started_tx,
            "--status",
            "done",
            "--merge-sha",
            &head,
            "--codex-commit",
            &head,
            "--reason",
            "direct close regression",
        ],
        &[],
    );
    assert!(
        output.status.success(),
        "recorded-id close must succeed while /api/runs is unavailable\nstdout={}\nstderr={}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    let paths = proxy.paths.lock().unwrap().clone();
    assert!(
        !paths.iter().any(|path| path == "/api/runs"),
        "recorded-id close must not enumerate runs: {paths:?}"
    );
    assert!(
        paths
            .iter()
            .any(|path| path.starts_with("/api/runs/") && path.ends_with("/release")),
        "recorded-id close must release the exact run: {paths:?}"
    );

    drop(proxy);
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
) -> String {
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
            "--mode",
            "ws",
            "--harness",
            "codex",
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
    started_tx_from_dispatch_stdout(&dispatch_stdout)
}

/// TASK-6AYEJ.2 finding 2, driven rather than reasoned about: a dispatched run
/// is interrupted, a FRESH recovery run replaces it (a new run id, same task
/// and terminal contract), and the worker finalizes from the replacement. The
/// worker's `*.reported` therefore carries a run id the dispatch record has
/// never seen. The manager's `dispatch-status` must still say `[reported]`;
/// before the fix the fail-closed RUN_ID rule left it `[unreported]` forever.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn recovery_replacement_run_finalize_still_marks_the_dispatch_reported() {
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
    let origin_run_id = tx_property_for(
        &tx_log(&project_root),
        "run.created",
        "TASK-DISPATCH",
        "RUN_ID",
    );

    // Interrupt the origin: the daemon goes away with the run still live, so on
    // the next boot its session file classifies as an interrupted run.
    let _ = running.shutdown.send(());
    let _ = running.join.await;
    let running = boot(home.clone()).await;

    let recover_stdout = run_orgasmic(
        &home,
        &running,
        &project_root,
        &path_env,
        &[
            "run",
            "recover",
            &origin_run_id,
            "--project",
            "orgasmic",
            "--action",
            "start_recovery_run",
            "--force-inert",
        ],
    );
    let recovered: serde_json::Value = serde_json::from_str(&recover_stdout)
        .unwrap_or_else(|e| panic!("recover output is not json ({e}): {recover_stdout}"));
    let replacement_run_id = recovered["run_id"].as_str().unwrap().to_string();
    assert_ne!(
        replacement_run_id, origin_run_id,
        "start_recovery_run must acquire a REPLACEMENT run; without a new id \
         there is no mismatch to fix and this test proves nothing"
    );

    let summary_path = tmp.path().join("summary.md");
    write(&summary_path, "recovered implementer report");
    let finalize_stdout = run_orgasmic(
        &home,
        &running,
        &worktree,
        &path_env,
        &[
            "dispatch",
            "finalize",
            "--run-id",
            &replacement_run_id,
            "--summary-file",
            summary_path.to_str().unwrap(),
        ],
    );
    assert!(
        finalize_stdout.contains("finalized: TASK-DISPATCH implementer.reported tx="),
        "unexpected finalize output: {finalize_stdout}"
    );
    assert_eq!(
        tx_property_for(
            &tx_log(&project_root),
            "implementer.reported",
            "TASK-DISPATCH",
            "RUN_ID"
        ),
        replacement_run_id,
        "the report must carry the replacement run id — that is the mismatch"
    );

    let status_stdout = run_orgasmic(
        &home,
        &running,
        &project_root,
        &path_env,
        &["manager", "dispatch-status", "--task", "TASK-DISPATCH"],
    );
    assert!(
        status_stdout.contains("[reported]"),
        "a finalize from a recovery replacement run must still mark its own \
         dispatch generation reported: {status_stdout}"
    );

    let _ = running.shutdown.send(());
    let _ = running.join.await;
}

/// Live run ids the daemon currently holds a lease for.
fn live_run_ids(
    home: &Home,
    running: &RunningDaemon,
    cwd: &Path,
    path_env: &std::ffi::OsString,
) -> Vec<String> {
    let raw = run_orgasmic(home, running, cwd, path_env, &["run", "list"]);
    let parsed: serde_json::Value = serde_json::from_str(&raw)
        .unwrap_or_else(|e| panic!("run list output is not json ({e}): {raw}"));
    parsed["live"]
        .as_array()
        .unwrap_or_else(|| panic!("run list has no live array: {raw}"))
        .iter()
        .map(|run| run["run_id"].as_str().unwrap().to_string())
        .collect()
}

/// TASK-6AYEJ.3: the OTHER half of the recovery-replacement contract. Once a
/// recovery has replaced the dispatched run, the generation-bound close must
/// address the REPLACEMENT — the run that is actually live — and not the origin
/// id the manager still has in hand. Releasing the origin would 404 and, before
/// this task, that 404 was accepted straight through to worktree removal and
/// branch deletion while the replacement was still running: the exact
/// successor-teardown the TASK-6AYEJ line exists to prevent, reached through
/// recovery instead of through a stale token.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn exact_close_after_recovery_releases_the_replacement_run() {
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
    let started_tx = dispatch_sleeping_implementer(
        &home,
        &running,
        &project_root,
        &path_env,
        &head,
        &worktree,
        &brief,
    )
    .await;
    let origin_run_id = tx_property_for(
        &tx_log(&project_root),
        "run.created",
        "TASK-DISPATCH",
        "RUN_ID",
    );

    // Interrupt the origin, then replace it through recovery.
    let _ = running.shutdown.send(());
    let _ = running.join.await;
    let running = boot(home.clone()).await;
    let recover_stdout = run_orgasmic(
        &home,
        &running,
        &project_root,
        &path_env,
        &[
            "run",
            "recover",
            &origin_run_id,
            "--project",
            "orgasmic",
            "--action",
            "start_recovery_run",
            "--force-inert",
        ],
    );
    let recovered: serde_json::Value = serde_json::from_str(&recover_stdout)
        .unwrap_or_else(|e| panic!("recover output is not json ({e}): {recover_stdout}"));
    let replacement_run_id = recovered["run_id"].as_str().unwrap().to_string();
    assert_ne!(
        replacement_run_id, origin_run_id,
        "recovery must acquire a REPLACEMENT run, or this test proves nothing"
    );
    assert!(
        live_run_ids(&home, &running, &project_root, &path_env).contains(&replacement_run_id),
        "the replacement must be live before the close, or the close has nothing to release"
    );

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
            "--started-tx",
            &started_tx,
            "--status",
            "done",
            "--merge-sha",
            &head,
            "--worktree-remove",
            "--reason",
            "close after recovery",
        ],
    );
    assert!(
        close_stdout.contains("implementer.done"),
        "unexpected close output: {close_stdout}"
    );
    let live = live_run_ids(&home, &running, &project_root, &path_env);
    assert!(
        !live.contains(&replacement_run_id),
        "the close must release the REPLACEMENT run, not just the origin id: {live:?}"
    );
    assert!(
        !worktree.exists(),
        "cleanup should have run once nothing was live in the worktree"
    );

    let _ = running.shutdown.send(());
    let _ = running.join.await;
}

/// TASK-6AYEJ.3: defence in depth for the same hazard when the ledger link is
/// MISSING rather than present — a recovery whose association never landed
/// (every pre-fix crash-reconciled recovery, and any future write that fails
/// after the replacement is live). The record then names a run the daemon has
/// never heard of, the release 404s, and the worktree the close is about to
/// remove is occupied by a live worker.
///
/// The stale-id record is written by hand here because the production generator
/// of it is now fixed at the source; the guard must still hold for records that
/// already exist. A 404 says "I cannot confirm this run is gone", which is not
/// "it is gone" — so before anything destructive it must be corroborated.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn exact_close_refuses_destructive_cleanup_beneath_an_unassociated_live_run() {
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
    let live_run_id = tx_property_for(
        &tx_log(&project_root),
        "run.created",
        "TASK-DISPATCH",
        "RUN_ID",
    );
    assert!(worktree.is_dir());

    // A second generation over the SAME worktree whose recorded run id the
    // daemon does not know — the shape a lost recovery association leaves. It
    // carries the live dispatch's artifact paths so cleanup would genuinely
    // reach `git worktree remove`; without them the removal bails for an
    // unrelated reason and the test would pass on a technicality.
    let last_path = tx_property_for(
        &tx_log(&project_root),
        "run.created",
        "TASK-DISPATCH",
        "LAST_PATH",
    );
    let stdout_path = tx_property_for(
        &tx_log(&project_root),
        "run.created",
        "TASK-DISPATCH",
        "STDOUT_PATH",
    );
    let path = tx_file_path(&project_root);
    let mut raw = std::fs::read_to_string(&path).unwrap();
    raw.push_str(&format!(
        "\n\n* TX 2026-05-23 Sat 10:00:00 manager.dispatch_started TASK-CLEANUP\n:PROPERTIES:\n:TX_ID:        tx-start-ghost\n:TIME:         [2026-05-23 Sat 10:00:00]\n:TYPE:         manager.dispatch_started\n:ACTOR:        a@example.com\n:MACHINE:      host\n:PROJECT:      orgasmic\n:TASK:         TASK-CLEANUP\n:KIND:         implementer\n:WORKTREE:     {}\n:BRANCH:       task-dispatch-test-impl\n:CODEX_BRIEF_PATH: {}\n:STARTED_AT:   [2026-05-23 Sat 10:00:00]\n:END:\n\n* TX 2026-05-23 Sat 10:00:01 run.created TASK-CLEANUP\n:PROPERTIES:\n:TX_ID:        tx-ghost-run\n:TIME:         [2026-05-23 Sat 10:00:01]\n:TYPE:         run.created\n:ACTOR:        daemon\n:MACHINE:      host\n:PROJECT:      orgasmic\n:TASK:         TASK-CLEANUP\n:RUN_ID:       run-ghost-never-existed\n:ORIGIN:       cli_dispatch\n:KIND:         implementer\n:LAST_PATH:    {}\n:STDOUT_PATH:  {}\n:DISPATCH_TX:  tx-start-ghost\n:END:\n",
        worktree.display(),
        brief.display(),
        last_path,
        stdout_path
    ));
    write(&path, raw);

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
            "--started-tx",
            "tx-start-ghost",
            "--status",
            "done",
            "--merge-sha",
            &head,
            "--worktree-remove",
            "--reason",
            "close against a stale run id",
        ],
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        !output.status.success(),
        "close must refuse destructive cleanup it cannot prove is safe\nstdout={}\nstderr={stderr}",
        String::from_utf8_lossy(&output.stdout)
    );
    assert!(
        stderr.contains("refusing to clean up dispatch") && stderr.contains(&live_run_id),
        "the refusal must name the live run that blocked it: {stderr}"
    );
    assert!(
        worktree.is_dir(),
        "the live worker's worktree must survive the refused close"
    );
    assert!(
        live_run_ids(&home, &running, &project_root, &path_env).contains(&live_run_id),
        "the refused close must not have released the live run either"
    );

    // TASK-QGWK7.1 F-3, wired: `--no-worktree-remove` is not an escape hatch.
    // It promotes, and promotion UNLINKS the tmp artifacts, so it takes the
    // same fence. TASK-QGWK7.1.1 M-8: the predicate had only an in-isolation
    // test, which cannot fail if the call site stops consulting it — this
    // enters `cmd_dispatch_close` and watches the artifacts survive.
    assert!(PathBuf::from(&last_path).exists());
    let no_remove = run_orgasmic_output(
        &home,
        &running,
        &project_root,
        &path_env,
        &[
            "manager",
            "dispatch-close",
            "--task",
            "TASK-CLEANUP",
            "--started-tx",
            "tx-start-ghost",
            "--status",
            "done",
            "--merge-sha",
            &head,
            "--no-worktree-remove",
            "--reason",
            "promote-only close against a stale run id",
        ],
    );
    let no_remove_stderr = String::from_utf8_lossy(&no_remove.stderr);
    assert!(
        !no_remove.status.success(),
        "--no-worktree-remove still unlinks tmp artifacts, so it must take the same fence\
         \nstdout={}\nstderr={no_remove_stderr}",
        String::from_utf8_lossy(&no_remove.stdout)
    );
    assert!(
        no_remove_stderr.contains("refusing to clean up dispatch")
            && no_remove_stderr.contains(&live_run_id),
        "the promote-only refusal must name the live run that blocked it: {no_remove_stderr}"
    );
    assert!(
        PathBuf::from(&last_path).exists(),
        "the refused promote-only close must not have unlinked the live worker's report"
    );

    let _ = running.shutdown.send(());
    let _ = running.join.await;
}

/// TASK-1T3FZ: the interleaving neither TASK-6AYEJ.3 test covers, and the only
/// one that reproduced the defect — a recovery that starts AFTER the close has
/// decided nothing is live and BEFORE it removes anything.
///
/// The two existing tests put the competing run in place *before* the close
/// begins, which the CLI-side snapshot could see. The window this one drives is
/// between the decision and the removal, and it cannot be closed from inside
/// the CLI at all: `POST /runs/:origin/recover` acquires in the daemon, in
/// another request, in another process. An audit of `await` points in
/// `cmd_dispatch_close` concludes the code is safe and is wrong.
///
/// The ordering is a rendezvous, not a sleep. `test_hooks::gate_dispatch_close_guard`
/// parks the daemon inside the close-guard handler with the worktree reserved
/// and the liveness verdict already taken; the recovery is issued while it is
/// parked; only then is the close let go. A slow or fast machine changes
/// nothing about which happens first — which is the point, because a race test
/// that passes because the machine was slow is how this defect survived a
/// round.
///
/// Injection-checked: with the reservation install removed from
/// `Supervisor::reserve_dispatch_close`, the recovery below succeeds, and the
/// close then removes the worktree out from under it.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn recovery_interleaved_between_the_close_verdict_and_removal_cannot_take_the_worktree() {
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
    let started_tx = dispatch_sleeping_implementer(
        &home,
        &running,
        &project_root,
        &path_env,
        &head,
        &worktree,
        &brief,
    )
    .await;
    let origin_run_id = tx_property_for(
        &tx_log(&project_root),
        "run.created",
        "TASK-DISPATCH",
        "RUN_ID",
    );

    // Interrupt the origin so the close finds nothing live and the recovery
    // below has something real to recover. This is the state a crashed worker
    // leaves: a recoverable run, a worktree still on disk, an open record.
    let _ = running.shutdown.send(());
    let _ = running.join.await;
    let running = boot(home.clone()).await;
    assert!(
        !live_run_ids(&home, &running, &project_root, &path_env).contains(&origin_run_id),
        "the origin must be gone before the close, or the close never reaches the guard window"
    );

    let mut gate = orgasmic_daemon::api::test_hooks::gate_dispatch_close_guard("TASK-DISPATCH");
    let close = {
        let home = home.clone();
        let daemon_url = format!("http://{}", running.addr);
        let project_root = project_root.clone();
        let path_env = path_env.clone();
        let started_tx = started_tx.clone();
        let head = head.clone();
        tokio::task::spawn_blocking(move || {
            run_orgasmic_output_with_daemon_url(
                &home,
                &daemon_url,
                &project_root,
                &path_env,
                &[
                    "manager",
                    "dispatch-close",
                    "--task",
                    "TASK-DISPATCH",
                    "--started-tx",
                    &started_tx,
                    "--status",
                    "done",
                    "--merge-sha",
                    &head,
                    "--worktree-remove",
                    "--reason",
                    "close racing a recovery",
                ],
                &[],
            )
        })
    };

    // The close is now parked in the daemon: worktree reserved, liveness
    // decided, nothing removed yet.
    gate.reached().await;

    let recover = run_orgasmic_output(
        &home,
        &running,
        &project_root,
        &path_env,
        &[
            "run",
            "recover",
            &origin_run_id,
            "--project",
            "orgasmic",
            "--action",
            "start_recovery_run",
            "--force-inert",
        ],
    );
    let recover_stderr = String::from_utf8_lossy(&recover.stderr).to_string();
    let recover_stdout = String::from_utf8_lossy(&recover.stdout).to_string();
    assert!(
        !recover.status.success(),
        "a recovery landing inside the close's cleanup window must be refused, not admitted \
         into a worktree that is about to be removed\nstdout={recover_stdout}\nstderr={recover_stderr}"
    );
    assert!(
        recover_stderr.contains("cleanup") || recover_stdout.contains("cleanup"),
        "the refusal must name the cleanup reservation as the reason\nstdout={recover_stdout}\nstderr={recover_stderr}"
    );
    let live_during_close = live_run_ids(&home, &running, &project_root, &path_env);
    assert!(
        live_during_close.is_empty(),
        "the refused recovery must not have left a live run in the worktree: {live_during_close:?}"
    );

    gate.proceed();
    let close = close.await.expect("dispatch-close task");
    assert!(
        close.status.success(),
        "the close must still succeed once the recovery is refused\nstdout={}\nstderr={}",
        String::from_utf8_lossy(&close.stdout),
        String::from_utf8_lossy(&close.stderr)
    );
    assert!(
        !worktree.exists(),
        "the close held the worktree the whole time, so removal must have happened"
    );
    assert!(
        live_run_ids(&home, &running, &project_root, &path_env).is_empty(),
        "no run may be left living in a worktree that no longer exists"
    );

    // And the reservation is handed back, not leaked: a fence that outlived
    // its cleanup would make this worktree path permanently unacquirable, and
    // the path a task dispatches to is deterministic. Asked directly, because
    // "the next dispatch works" is a much slower way to learn the same thing.
    // The bare directory is recreated only so the endpoint's path validation
    // has something to look at; the reservation is keyed by the same
    // canonicalized path either way.
    std::fs::create_dir_all(&worktree).unwrap();
    let token = std::fs::read_to_string(home.auth_token())
        .unwrap()
        .trim()
        .to_string();
    let regrant: serde_json::Value = reqwest::Client::new()
        .post(format!(
            "http://{}/api/projects/orgasmic/tasks/TASK-DISPATCH/dispatch/close-guard",
            running.addr
        ))
        .header(AUTHORIZATION, format!("Bearer {token}"))
        .json(&serde_json::json!({ "worktree_path": worktree }))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(
        regrant["status"], "reserved",
        "the close's reservation must have been released: {regrant}"
    );

    let _ = running.shutdown.send(());
    let _ = running.join.await;
}

/// TASK-AK6EM: the interaction TASK-ATAXN created. The destructive work of a
/// close runs in the CLI, so the daemon that granted the guard can be replaced
/// while the holder is still deleting files. A replacement that starts with an
/// empty reservation map reopens exactly the race TASK-1T3FZ closed.
///
/// The pause is a rendezvous, not a sleep: the close parks in
/// `dispatch_close_pause_after_guard` *after* the guard response, which is the
/// only window where the holder is out of contact with the daemon. The daemon
/// is then replaced under it.
///
/// Injection-checked: drop the `close_guards.write(..)` in
/// `Supervisor::reserve_dispatch_close` (or the `restore()` in `Inner::new`)
/// and the recovery below is admitted by the replacement daemon, into a
/// worktree the parked close is about to remove.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_daemon_replaced_mid_close_still_refuses_recovery_until_the_holder_finishes() {
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

    let port = reserved_local_port();
    let running = boot_on_port(home.clone(), port).await;
    let started_tx = dispatch_sleeping_implementer(
        &home,
        &running,
        &project_root,
        &path_env,
        &head,
        &worktree,
        &brief,
    )
    .await;
    let origin_run_id = tx_property_for(
        &tx_log(&project_root),
        "run.created",
        "TASK-DISPATCH",
        "RUN_ID",
    );

    // Interrupt the origin so the close reaches its guard, and the recovery
    // below has something real to recover.
    let _ = running.shutdown.send(());
    let _ = running.join.await;
    let running = boot_on_port(home.clone(), port).await;
    assert!(
        !live_run_ids(&home, &running, &project_root, &path_env).contains(&origin_run_id),
        "the origin must be gone before the close, or the close never reaches the guard window"
    );

    let pause = tmp.path().join("close.pause");
    let reached = pause.with_extension("reached");
    std::fs::write(&pause, "hold").unwrap();

    let close = {
        let home = home.clone();
        let daemon_url = format!("http://{}", running.addr);
        let project_root = project_root.clone();
        let path_env = path_env.clone();
        let started_tx = started_tx.clone();
        let head = head.clone();
        let pause = pause.clone();
        tokio::task::spawn_blocking(move || {
            run_orgasmic_output_with_daemon_url(
                &home,
                &daemon_url,
                &project_root,
                &path_env,
                &[
                    "manager",
                    "dispatch-close",
                    "--task",
                    "TASK-DISPATCH",
                    "--started-tx",
                    &started_tx,
                    "--status",
                    "done",
                    "--merge-sha",
                    &head,
                    "--worktree-remove",
                    "--reason",
                    "close surviving a daemon replacement",
                ],
                &[(
                    "ORGASMIC_DISPATCH_CLOSE_PAUSE_FILE",
                    pause.to_str().unwrap(),
                )],
            )
        })
    };

    // The close holds the guard and is parked before any filesystem mutation.
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(60);
    while !reached.exists() {
        assert!(
            std::time::Instant::now() < deadline,
            "the close never reached its post-guard pause"
        );
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }
    assert!(
        worktree.is_dir(),
        "the parked close must not have removed anything yet"
    );

    // Replace the daemon under the holder — the TASK-ATAXN handoff.
    let _ = running.shutdown.send(());
    let _ = running.join.await;
    let replacement = boot_on_port(home.clone(), port).await;

    let recover = run_orgasmic_output(
        &home,
        &replacement,
        &project_root,
        &path_env,
        &[
            "run",
            "recover",
            &origin_run_id,
            "--project",
            "orgasmic",
            "--action",
            "start_recovery_run",
            "--force-inert",
        ],
    );
    let recover_stderr = String::from_utf8_lossy(&recover.stderr).to_string();
    let recover_stdout = String::from_utf8_lossy(&recover.stdout).to_string();
    assert!(
        !recover.status.success(),
        "the replacement daemon must inherit the in-flight close guard\nstdout={recover_stdout}\nstderr={recover_stderr}"
    );
    assert!(
        recover_stderr.contains("cleanup") || recover_stdout.contains("cleanup"),
        "the refusal must name the cleanup reservation\nstdout={recover_stdout}\nstderr={recover_stderr}"
    );
    assert!(
        live_run_ids(&home, &replacement, &project_root, &path_env).is_empty(),
        "the refused recovery must not have left a live run in the worktree"
    );

    // Let the holder finish its cleanup.
    std::fs::remove_file(&pause).unwrap();
    let close = close.await.expect("dispatch-close task");
    assert!(
        close.status.success(),
        "the close must still complete after its daemon was replaced\nstdout={}\nstderr={}",
        String::from_utf8_lossy(&close.stdout),
        String::from_utf8_lossy(&close.stderr)
    );
    assert!(
        !worktree.exists(),
        "the close held the worktree the whole time, so removal must have happened"
    );

    // The holder process is gone, so the replacement reclaims the guard it
    // inherited and the worktree is usable again — the reservation is a fence,
    // not a leak.
    std::fs::create_dir_all(&worktree).unwrap();
    let token = std::fs::read_to_string(home.auth_token())
        .unwrap()
        .trim()
        .to_string();
    let mut regrant = serde_json::Value::Null;
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
    while std::time::Instant::now() < deadline {
        regrant = reqwest::Client::new()
            .post(format!(
                "http://{}/api/projects/orgasmic/tasks/TASK-DISPATCH/dispatch/close-guard",
                replacement.addr
            ))
            .header(AUTHORIZATION, format!("Bearer {token}"))
            .json(&serde_json::json!({
                "worktree_path": worktree,
                "owner_pid": std::process::id(),
            }))
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        if regrant["status"] == "reserved" {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    }
    assert_eq!(
        regrant["status"], "reserved",
        "a guard whose holder has exited must be reclaimed: {regrant}"
    );

    let _ = replacement.shutdown.send(());
    let _ = replacement.join.await;
}

/// TASK-1T3FZ finding 2: an open record with NO `RUN_ID` at all. The real
/// shape is a CLI that died after the daemon acquire succeeded and before
/// `run.created` was appended — the worker is live in its worktree and the
/// ledger cannot name it.
///
/// That branch used to call `fetch_live_runs` purely to prove the daemon was
/// reachable, discard the runs it got back, and clean up anyway. Reachability
/// is not evidence that an unidentified live worker is absent; undetermined
/// liveness must refuse. It now takes the same daemon-owned worktree
/// reservation as every other destructive close, and the refusal comes from
/// the same verdict.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn close_without_a_recorded_run_id_refuses_cleanup_beneath_a_live_worker() {
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
    let live_run_id = tx_property_for(
        &tx_log(&project_root),
        "run.created",
        "TASK-DISPATCH",
        "RUN_ID",
    );
    assert!(worktree.is_dir());

    // The open record the crash leaves: a dispatch_started over the live
    // worktree, and no run.created to name what is running in it.
    let path = tx_file_path(&project_root);
    let mut raw = std::fs::read_to_string(&path).unwrap();
    raw.push_str(&format!(
        "\n\n* TX 2026-05-23 Sat 10:00:00 manager.dispatch_started TASK-NORUNID\n:PROPERTIES:\n:TX_ID:        tx-start-norunid\n:TIME:         [2026-05-23 Sat 10:00:00]\n:TYPE:         manager.dispatch_started\n:ACTOR:        a@example.com\n:MACHINE:      host\n:PROJECT:      orgasmic\n:TASK:         TASK-NORUNID\n:KIND:         implementer\n:WORKTREE:     {}\n:BRANCH:       task-dispatch-test-impl\n:CODEX_BRIEF_PATH: {}\n:STARTED_AT:   [2026-05-23 Sat 10:00:00]\n:END:\n",
        worktree.display(),
        brief.display()
    ));
    write(&path, raw);

    let output = run_orgasmic_output(
        &home,
        &running,
        &project_root,
        &path_env,
        &[
            "manager",
            "dispatch-close",
            "--task",
            "TASK-NORUNID",
            "--started-tx",
            "tx-start-norunid",
            "--status",
            "done",
            "--merge-sha",
            &head,
            "--worktree-remove",
            "--reason",
            "close a record that never learned its run id",
        ],
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        !output.status.success(),
        "a record with no RUN_ID cannot prove the worker is gone, so it must refuse\nstdout={}\nstderr={stderr}",
        String::from_utf8_lossy(&output.stdout)
    );
    assert!(
        stderr.contains("refusing to clean up dispatch") && stderr.contains(&live_run_id),
        "the refusal must name the live run that blocked it: {stderr}"
    );
    assert!(
        worktree.is_dir(),
        "the live worker's worktree must survive the refused close"
    );
    assert!(
        live_run_ids(&home, &running, &project_root, &path_env).contains(&live_run_id),
        "the refused close must not have released the live run either"
    );

    let _ = running.shutdown.send(());
    let _ = running.join.await;
}

/// The `manager.dispatch_started` tx id printed by `manager dispatch`, i.e. the
/// generation token `dispatch-close --started-tx` takes (TASK-6AYEJ.1).
fn started_tx_from_dispatch_stdout(stdout: &str) -> String {
    stdout
        .split_whitespace()
        .find_map(|token| token.strip_prefix("started_tx="))
        .unwrap_or_else(|| panic!("dispatch output has no started_tx=: {stdout}"))
        .to_string()
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
        finalize_stdout.contains("finalized: TASK-DISPATCH implementer.reported tx="),
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

/// TASK-WGXKD acceptance: a worker finalize whose process is killed the instant
/// the lease is released still produces its terminal tx.
///
/// This is the production death, not a synthetic one. Releasing the lease tears
/// down the driver, and the driver reaps the harness's whole setsid process
/// group (`reap_process_group`) — the `orgasmic dispatch finalize` process is a
/// member of that group, so the release signals the very process that used to
/// owe the tx afterwards. On stdio it lost the tx 3 times out of 3.
/// `ORGASMIC_TEST_FINALIZE_KILL_SELF_AFTER_RELEASE` SIGKILLs the CLI at exactly
/// that point: whatever the daemon has not already recorded is gone for good.
///
/// The tx must be there anyway, matched to this run by RUN_ID, and
/// `dispatch-status` must read `[reported]` — never the `[unreported]` that was
/// the only visible trace of the lost state.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn dispatch_finalize_terminal_tx_survives_client_death_at_release() {
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
    let started_tx = dispatch_sleeping_implementer(
        &home,
        &running,
        &project_root,
        &path_env,
        &head,
        &worktree,
        &brief,
    )
    .await;
    let run_id = tx_property_for(
        &tx_log(&project_root),
        "run.created",
        "TASK-DISPATCH",
        "RUN_ID",
    );

    write(&worktree.join("worker-change.txt"), "worker output\n");
    let summary_path = tmp.path().join("summary.md");
    write(&summary_path, "implementer report");
    let finalize = run_orgasmic_output_with_env(
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
        &[("ORGASMIC_TEST_FINALIZE_KILL_SELF_AFTER_RELEASE", "1")],
    );
    assert_eq!(
        finalize.status.signal(),
        Some(9),
        "the finalize process must have been SIGKILLed at the release, \
         reproducing the production death\nstdout={}\nstderr={}",
        String::from_utf8_lossy(&finalize.stdout),
        String::from_utf8_lossy(&finalize.stderr)
    );
    assert!(
        !String::from_utf8_lossy(&finalize.stdout).contains("finalized:"),
        "the client died before it could report success; anything it printed \
         would mean it got another turn the real one never gets"
    );

    let raw = tx_log(&project_root);
    assert_eq!(
        tx_property_for(&raw, "implementer.reported", "TASK-DISPATCH", "RUN_ID"),
        run_id,
        "the terminal tx must be on record for THIS run, written by the daemon \
         as part of the release the client did not survive"
    );

    let status_stdout = run_orgasmic(
        &home,
        &running,
        &project_root,
        &path_env,
        &["manager", "dispatch-status", "--task", "TASK-DISPATCH"],
    );
    assert!(
        status_stdout.contains(&format!("TX_ID={started_tx}")),
        "dispatch-status must still list this dispatch: {status_stdout}"
    );
    assert!(
        status_stdout.contains("[reported]"),
        "a released run must never show as [unreported] after a successful \
         finalize: {status_stdout}"
    );

    let _ = running.shutdown.send(());
    let _ = running.join.await;
}

/// TASK-WGXKD.1 finding 1: a finalize that cannot prove the daemon will write
/// its terminal tx must refuse BEFORE releasing, leaving the lease held.
///
/// The skew is real and routine: CLI and daemon ship in one runtime bundle, but
/// a source build — or the window between installing a runtime and kickstarting
/// the daemon — puts a new CLI in front of an old daemon. That daemon ignores
/// the unknown `terminal_tx` field, performs the release, and on stdio the
/// release reaps the finalize process's own group before any client-side
/// fallback could run. Committed, reported to last.txt, lease released, nothing
/// on record: the exact defect TASK-WGXKD closed.
///
/// So the handshake happens first, and here it 404s. What must be true after:
/// no release, no terminal tx, the lease STILL HELD (`[run-live]`), and the
/// worktree untouched so the same command can simply be re-run once the daemon
/// is restarted. A held lease is the deliberate trade — visible and rescuable,
/// versus a released run that reports nothing and flags nothing.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn dispatch_finalize_refuses_release_against_pre_wgxkd_daemon() {
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

    write(&worktree.join("worker-change.txt"), "worker output\n");
    let summary_path = tmp.path().join("summary.md");
    write(&summary_path, "implementer report");

    let proxy = start_pre_wgxkd_daemon_proxy(running.addr).await;
    let finalize = run_orgasmic_output_with_daemon_url(
        &home,
        &format!("http://{}", proxy.addr),
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
        &[],
    );
    let stderr = String::from_utf8_lossy(&finalize.stderr).to_string();
    assert!(
        !finalize.status.success(),
        "finalize against a pre-WGXKD daemon must fail, not release\nstdout={}\nstderr={stderr}",
        String::from_utf8_lossy(&finalize.stdout)
    );
    assert!(
        stderr.contains("refusing to release the lease"),
        "the refusal must say the lease was NOT released: {stderr}"
    );
    assert!(
        stderr.contains("orgasmic daemon restart"),
        "the refusal must name the operator remedy: {stderr}"
    );

    let paths = proxy.paths.lock().unwrap().clone();
    assert!(
        paths.iter().any(|path| path == "/api/daemon/capabilities"),
        "the handshake must actually be attempted: {paths:?}"
    );
    assert!(
        !paths
            .iter()
            .any(|path| path.starts_with("/api/runs/") && path.ends_with("/release")),
        "the release must never be sent once the handshake failed: {paths:?}"
    );
    assert!(
        !paths.iter().any(|path| path == "/api/tx"),
        "no terminal tx may be posted for a run whose lease was not released: {paths:?}"
    );

    let raw = tx_log(&project_root);
    assert!(
        !raw.contains(":TYPE:         implementer.reported"),
        "a refused finalize must leave no report behind:\n{raw}"
    );
    assert!(
        !run_git(&worktree, &["status", "--porcelain"]).is_empty(),
        "refusing before the commit is what makes the retry identical; the \
         worktree must be untouched"
    );

    drop(proxy);
    let status_stdout = run_orgasmic(
        &home,
        &running,
        &project_root,
        &path_env,
        &["manager", "dispatch-status", "--task", "TASK-DISPATCH"],
    );
    assert!(
        status_stdout.contains("[run-live]"),
        "the lease must still be held — visible-and-wrong is the whole point of \
         refusing: {status_stdout}"
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
        finalize_stdout.contains("finalized: TASK-DISPATCH implementer.reported tx="),
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
    // TASK-6AYEJ: the worker's commit is its own branch tip, recorded as
    // `:SHA:`. It is NOT a merge sha — nothing has merged yet — and writing it
    // as `:MERGE_SHA:` made every audit trust a commit that was never on main
    // as such. Only the manager's `dispatch-close` records `:MERGE_SHA:`.
    assert!(
        !tx_raw
            .lines()
            .any(|line| line.trim_start().starts_with(":MERGE_SHA:")),
        "a worker finalize must not claim a MERGE_SHA: {tx_raw}"
    );

    let _ = running.shutdown.send(());
    let _ = running.join.await;
}

/// Acceptance #3 (TASK-WFW1N), amended by TASK-6AYEJ: finalize emits the
/// correct worker-completion tx (`implementer.reported`/`reviewer.reported`)
/// and releases the lease — but does NOT close the dispatch, so
/// `manager dispatch-status` still lists it, flagged `[reported]`, waiting on
/// the manager's `dispatch-close`.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn dispatch_finalize_reports_without_closing_and_releases_lease() {
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
        finalize_stdout.contains("finalized: TASK-DISPATCH implementer.reported tx="),
        "unexpected finalize output: {finalize_stdout}"
    );

    let tx_raw = tx_log(&project_root);
    assert!(tx_raw.contains(":TYPE:         implementer.reported"));
    assert!(
        !tx_raw.contains(":TYPE:         implementer.done"),
        "the worker must not emit the manager's closing tx: {tx_raw}"
    );
    assert!(tx_raw.contains(":TASK:         TASK-DISPATCH"));

    let status_stdout = run_orgasmic(
        &home,
        &running,
        &project_root,
        &path_env,
        &["manager", "dispatch-status", "--task", "TASK-DISPATCH"],
    );
    assert!(
        status_stdout.contains("TASK=TASK-DISPATCH"),
        "a finalized dispatch stays open until the manager closes it: {status_stdout}"
    );
    assert!(
        status_stdout.contains("[reported]"),
        "dispatch-status must show the worker reported, so the manager can tell \
         `awaiting close` from `the run died`: {status_stdout}"
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

    // Finalize the reviewer kind too, proving the completion-tx type follows
    // the run's own kind (`reviewer.reported`), not a hardcoded implementer
    // path.
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
            "--mode",
            "stdio",
            "--harness",
            "codex",
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
        review_finalize_stdout.contains("finalized: TASK-REVIEW reviewer.reported tx="),
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
        review_status_stdout.contains("TASK=TASK-REVIEW")
            && review_status_stdout.contains("[reported]"),
        "a finalized reviewer dispatch also stays open for the manager: {review_status_stdout}"
    );

    let _ = running.shutdown.send(());
    let _ = running.join.await;
}

/// TASK-6AYEJ, the headline acceptance: after a worker runs `dispatch finalize
/// --commit`, the manager's `dispatch-close` must actually work. Every one of
/// its five clauses failed before this fix — the close bailed with "no open
/// manager.dispatch_started tx", so no manager tx, no merge sha, no worktree
/// removal, no branch deletion, and no lifecycle flip ever happened on a
/// successful dispatch.
///
/// Also covers the double-close criterion: re-running the same close (a manager
/// that died mid-integration) is a clean no-op, not an error.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn dispatch_finalize_then_manager_close_records_merge_sha_and_cleans_up() {
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
    let started_tx = dispatch_sleeping_implementer(
        &home,
        &running,
        &project_root,
        &path_env,
        &head,
        &worktree,
        &brief,
    )
    .await;

    // The worker does real work and finalizes with --commit, exactly as a
    // dispatched persona ends its turn.
    write(&worktree.join("worker-change.txt"), "worker output\n");
    let summary_path = tmp.path().join("summary.md");
    write(&summary_path, "implementer report");
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
        finalize_stdout.contains("finalized: TASK-DISPATCH implementer.reported tx="),
        "unexpected finalize output: {finalize_stdout}"
    );
    let worker_sha = run_git(&worktree, &["rev-parse", "HEAD"]);
    assert_ne!(
        worker_sha, head,
        "--commit must have produced a new worker commit"
    );

    // The manager merges. Its merge sha is a DIFFERENT commit from the worker's
    // branch tip — the exact distinction the old MERGE_SHA-from-finalize lost.
    // This pre-existing test owns finalize/cleanup behavior, so merge on the
    // fixture integration branch; the default-branch review gate is covered by
    // the focused regressions above.
    run_git(&project_root, &["checkout", "integration"]);
    run_git(
        &project_root,
        &[
            "merge",
            "--no-ff",
            "-m",
            "merge worker",
            "task-dispatch-test-impl",
        ],
    );
    let merge_sha = run_git(&project_root, &["rev-parse", "HEAD"]);
    assert_ne!(merge_sha, worker_sha, "a --no-ff merge is its own commit");

    // TASK-6AYEJ.2, on the production path: the same close WITHOUT
    // `--started-tx` is refused while the dispatch is live, and the refusal
    // hands the operator the token to copy. This runs before the real close so
    // the record is genuinely open.
    let tokenless_stderr = run_orgasmic_failure(
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
            &merge_sha,
            "--worktree-remove",
            "--branch-delete",
            "--reason",
            "merged",
        ],
    );
    assert!(
        tokenless_stderr.contains("--started-tx is required")
            && tokenless_stderr.contains(&format!("--started-tx {started_tx}")),
        "a tokenless close of a LIVE dispatch must be refused with a copyable \
         token: {tokenless_stderr}"
    );
    assert!(
        worktree.exists(),
        "a refused close must not have cleaned up the live worktree"
    );

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
            "--started-tx",
            &started_tx,
            "--status",
            "done",
            "--merge-sha",
            &merge_sha,
            "--worktree-remove",
            "--branch-delete",
            "--reason",
            "merged",
        ],
    );
    assert!(
        close_stdout.contains("closed: TASK-DISPATCH implementer.done tx="),
        "the manager's close must succeed after a worker finalize: {close_stdout}"
    );
    assert!(
        !worktree.exists(),
        "close must remove the worktree — the whole point of making it reachable"
    );
    let branches = run_git(
        &project_root,
        &["branch", "--list", "task-dispatch-test-impl"],
    );
    assert!(
        branches.trim().is_empty(),
        "close must delete the dispatch branch: {branches}"
    );

    let tx_raw = tx_log(&project_root);
    assert!(
        tx_raw.contains(":TYPE:         implementer.reported")
            && tx_raw.contains(":TYPE:         implementer.done"),
        "both the worker's report and the manager's close must be on record: {tx_raw}"
    );
    assert!(
        tx_raw
            .lines()
            .any(|line| line.trim_start().starts_with(":MERGE_SHA:") && line.contains(&merge_sha)),
        "the close tx must carry the MANAGER's merge sha {merge_sha}: {tx_raw}"
    );
    assert!(
        !tx_raw
            .lines()
            .any(|line| line.trim_start().starts_with(":MERGE_SHA:") && line.contains(&worker_sha)),
        "no tx may record the worker's branch tip as a merge sha: {tx_raw}"
    );
    assert!(tx_raw.contains(":CLEANUP_STATUS: ok"));
    // The lifecycle flip lives in dispatch-close, so it too was unreachable.
    assert_task_stage(&project_root, "TASK-DISPATCH", "IN_REVIEW", "in_review");

    let status_stdout = run_orgasmic(
        &home,
        &running,
        &project_root,
        &path_env,
        &["manager", "dispatch-status", "--task", "TASK-DISPATCH"],
    );
    assert!(
        status_stdout.trim().is_empty(),
        "the manager's close must close the dispatch: {status_stdout}"
    );

    // Double close: a manager that died mid-integration re-runs the same
    // command. Clean no-op, distinguishable from a real close, no second tx.
    let reclose = run_orgasmic_output(
        &home,
        &running,
        &project_root,
        &path_env,
        &[
            "manager",
            "dispatch-close",
            "--task",
            "TASK-DISPATCH",
            "--started-tx",
            &started_tx,
            "--status",
            "done",
            "--merge-sha",
            &merge_sha,
            "--worktree-remove",
            "--branch-delete",
            "--reason",
            "merged",
        ],
    );
    let reclose_stdout = String::from_utf8_lossy(&reclose.stdout).to_string();
    assert!(
        reclose.status.success(),
        "double close must not be an error\nstdout={reclose_stdout}\nstderr={}",
        String::from_utf8_lossy(&reclose.stderr)
    );
    assert!(
        reclose_stdout.contains("already-closed: TASK-DISPATCH started_tx="),
        "a repeated close must announce itself as a no-op: {reclose_stdout}"
    );
    let tx_after = tx_log(&project_root);
    assert_eq!(
        count_occurrences(&tx_after, ":TYPE:         implementer.done"),
        1,
        "a repeated close must not append a second closing tx: {tx_after}"
    );

    let _ = running.shutdown.send(());
    let _ = running.join.await;
}

/// TASK-6AYEJ.1, the ship blocker the double-close test above misses because it
/// replays IMMEDIATELY, before a successor exists. The real workflow closes the
/// implementer, which moves the task to IN_REVIEW, and then opens a REVIEWER
/// for the same task. A stale implementer close replayed at that moment used to
/// select the reviewer's open record: it released the live reviewer run,
/// removed its worktree, deleted its branch and appended a `reviewer.done`.
/// Bound to its own generation via `--started-tx`, it is a no-op instead.
///
/// The second half covers the other generation shape: abort → redispatch →
/// stale abort retry.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn stale_close_retry_does_not_touch_a_successor_dispatch_for_the_same_task() {
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
    let impl_started_tx = dispatch_sleeping_implementer(
        &home,
        &running,
        &project_root,
        &path_env,
        &head,
        &worktree,
        &brief,
    )
    .await;

    // The manager closes the implementer normally. This is the generation the
    // stale retry below belongs to.
    let close_args = |started_tx: &str| {
        vec![
            "manager".to_string(),
            "dispatch-close".to_string(),
            "--task".to_string(),
            "TASK-DISPATCH".to_string(),
            "--started-tx".to_string(),
            started_tx.to_string(),
            "--status".to_string(),
            "done".to_string(),
            "--merge-sha".to_string(),
            head.clone(),
            "--worktree-remove".to_string(),
            "--branch-delete".to_string(),
            "--reason".to_string(),
            "merged".to_string(),
        ]
    };
    let impl_close_args = close_args(&impl_started_tx);
    let impl_close_argv = impl_close_args
        .iter()
        .map(String::as_str)
        .collect::<Vec<_>>();
    let close_stdout = run_orgasmic(&home, &running, &project_root, &path_env, &impl_close_argv);
    assert!(
        close_stdout.contains("closed: TASK-DISPATCH implementer.done tx="),
        "unexpected implementer close output: {close_stdout}"
    );
    assert_task_stage(&project_root, "TASK-DISPATCH", "IN_REVIEW", "in_review");

    // ...and dispatches a reviewer for the SAME task, exactly as the workflow
    // prescribes. This is the successor a task-bound retry would grab.
    let review_brief = tmp.path().join("codex/task-dispatch-review-brief.md");
    write(&review_brief, "stub reviewer brief");
    let review_worktree = tmp.path().join("worktrees/task-dispatch-review");
    let review_dispatch_stdout = run_orgasmic(
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
            "reviewer",
            "--mode",
            "ws",
            "--harness",
            "codex",
            "--brief",
            review_brief.to_str().unwrap(),
            "--from",
            &head,
            "--worktree",
            review_worktree.to_str().unwrap(),
            "--branch",
            "task-dispatch-test-review",
            "--reason",
            "reviewer pass",
        ],
    );
    assert!(review_dispatch_stdout.contains("dispatched: TASK-DISPATCH reviewer pid="));
    let review_started_tx = started_tx_from_dispatch_stdout(&review_dispatch_stdout);
    assert!(review_worktree.is_dir(), "reviewer worktree should exist");

    // The stale retry: the same implementer close command, replayed.
    let stale = run_orgasmic_output(&home, &running, &project_root, &path_env, &impl_close_argv);
    let stale_stdout = String::from_utf8_lossy(&stale.stdout).to_string();
    assert!(
        stale.status.success(),
        "a stale retry must still be a clean no-op\nstdout={stale_stdout}\nstderr={}",
        String::from_utf8_lossy(&stale.stderr)
    );
    assert!(
        stale_stdout.contains(&format!(
            "already-closed: TASK-DISPATCH started_tx={impl_started_tx}"
        )),
        "the retry must no-op against ITS OWN generation: {stale_stdout}"
    );

    // The reviewer is untouched: still open, worktree on disk, branch alive,
    // and no reviewer terminal tx was appended.
    let status_stdout = run_orgasmic(
        &home,
        &running,
        &project_root,
        &path_env,
        &["manager", "dispatch-status", "--task", "TASK-DISPATCH"],
    );
    assert!(
        status_stdout.contains(&format!("TX_ID={review_started_tx}"))
            && status_stdout.contains("KIND=reviewer"),
        "the live reviewer dispatch must still be open: {status_stdout}"
    );
    assert!(
        review_worktree.is_dir(),
        "the stale retry must not remove the reviewer's worktree"
    );
    let branches = run_git(
        &project_root,
        &["branch", "--list", "task-dispatch-test-review"],
    );
    assert!(
        !branches.trim().is_empty(),
        "the stale retry must not delete the reviewer's branch: {branches}"
    );
    let tx_after = tx_log(&project_root);
    assert_eq!(
        count_occurrences(&tx_after, ":TYPE:         reviewer.done"),
        0,
        "the stale retry must not append a reviewer close: {tx_after}"
    );
    assert_eq!(
        count_occurrences(&tx_after, ":TYPE:         implementer.done"),
        1,
        "the stale retry must not append a second implementer close: {tx_after}"
    );

    // Second shape: abort the reviewer, redispatch for the same task, replay
    // the abort. Same fence, different terminal tx.
    let abort_args = vec![
        "manager",
        "dispatch-close",
        "--task",
        "TASK-DISPATCH",
        "--started-tx",
        review_started_tx.as_str(),
        "--status",
        "aborted",
        "--reason",
        "reviewer wedged",
        "--worktree-remove",
        "--branch-delete",
    ];
    let abort_stdout = run_orgasmic(&home, &running, &project_root, &path_env, &abort_args);
    assert!(
        abort_stdout.contains("closed: TASK-DISPATCH manager.dispatch_aborted tx="),
        "unexpected abort output: {abort_stdout}"
    );

    let redispatch_brief = tmp.path().join("codex/task-dispatch-review2-brief.md");
    write(&redispatch_brief, "stub reviewer brief 2");
    let redispatch_worktree = tmp.path().join("worktrees/task-dispatch-review2");
    let redispatch_stdout = run_orgasmic(
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
            "reviewer",
            "--mode",
            "ws",
            "--harness",
            "codex",
            "--brief",
            redispatch_brief.to_str().unwrap(),
            "--from",
            &head,
            "--worktree",
            redispatch_worktree.to_str().unwrap(),
            "--branch",
            "task-dispatch-test-review2",
            "--reason",
            "reviewer retry",
        ],
    );
    let redispatch_started_tx = started_tx_from_dispatch_stdout(&redispatch_stdout);

    let stale_abort = run_orgasmic_output(&home, &running, &project_root, &path_env, &abort_args);
    let stale_abort_stdout = String::from_utf8_lossy(&stale_abort.stdout).to_string();
    assert!(
        stale_abort.status.success(),
        "a stale abort retry must be a clean no-op\nstdout={stale_abort_stdout}\nstderr={}",
        String::from_utf8_lossy(&stale_abort.stderr)
    );
    assert!(
        stale_abort_stdout.contains(&format!(
            "already-closed: TASK-DISPATCH started_tx={review_started_tx}"
        )),
        "the stale abort must no-op against its own generation: {stale_abort_stdout}"
    );
    assert!(
        redispatch_worktree.is_dir(),
        "the stale abort must not remove the redispatched worktree"
    );
    let status_after = run_orgasmic(
        &home,
        &running,
        &project_root,
        &path_env,
        &["manager", "dispatch-status", "--task", "TASK-DISPATCH"],
    );
    assert!(
        status_after.contains(&format!("TX_ID={redispatch_started_tx}")),
        "the redispatched reviewer must still be open: {status_after}"
    );
    let tx_final = tx_log(&project_root);
    assert_eq!(
        count_occurrences(&tx_final, ":TYPE:         manager.dispatch_aborted"),
        1,
        "the stale abort must not append a second abort tx: {tx_final}"
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
        !tx_raw.contains(":TYPE:         implementer.done")
            && !tx_raw.contains(":TYPE:         implementer.reported"),
        "blocked finalize must not emit a done or reported tx: {tx_raw}"
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
        stdout.contains("finalized: TASK-DISPATCH implementer.reported tx="),
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
        tx_raw.contains(":TYPE:         implementer.reported"),
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

    // Default worktree layout: since TASK-M47E5 the managed root is
    // `<home>/worktrees/<project-id>/<task>` — outside the project entirely.
    let worktree = home.root.join("worktrees/orgasmic/task-dispatch");

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
            "--mode",
            "ws",
            "--harness",
            "codex",
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
        finalize_stdout.contains("finalized: TASK-DISPATCH implementer.reported tx="),
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

/// TASK-P4MGK: `orgasmic dispatch finalize` is accepted from stdio, not
/// only rmux/ws. PATH has no `codex` so the driver stays Simulated and
/// the lease stays live until finalize (protocol-end is not the success
/// signal).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn dispatch_finalize_from_stdio_mode() {
    // orgasmic:TASK-P4MGK
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
    write_git_proxy(&bin_dir);
    // No `codex` on PATH → stdio stays Simulated (Ready only, lease live).
    let path_env = path_only(&bin_dir);
    let brief = tmp.path().join("brief.md");
    let worktree = tmp.path().join("worktrees/task-dispatch-stdio");
    write(&brief, "stdio finalize smoke");

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
            "--mode",
            "stdio",
            "--harness",
            "codex",
            "--brief",
            brief.to_str().unwrap(),
            "--from",
            &head,
            "--worktree",
            worktree.to_str().unwrap(),
            "--branch",
            "task-dispatch-stdio-impl",
            "--reason",
            "stdio finalize smoke",
        ],
    );
    assert!(
        dispatch_stdout.contains("dispatched: TASK-DISPATCH implementer pid="),
        "unexpected dispatch output: {dispatch_stdout}"
    );

    let summary_path = tmp.path().join("summary.md");
    write(&summary_path, "stdio finalize report");
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
        finalize_stdout.contains("finalized: TASK-DISPATCH implementer.reported tx="),
        "unexpected finalize output: {finalize_stdout}"
    );
    let last_path = finalized_last_path(&finalize_stdout);
    assert_eq!(
        std::fs::read_to_string(&last_path).unwrap(),
        "stdio finalize report"
    );
    let tx_raw = tx_log(&project_root);
    assert!(
        tx_raw.contains(":TYPE:         implementer.reported"),
        "stdio finalize must emit implementer.reported: {tx_raw}"
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
            "--mode",
            "subprocess-stream-json",
            "--harness",
            "cursor-agent",
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
        finalize_stdout.contains("finalized: TASK-DISPATCH implementer.reported tx="),
        "unexpected finalize output: {finalize_stdout}"
    );
    let last_path = finalized_last_path(&finalize_stdout);
    assert_eq!(
        std::fs::read_to_string(&last_path).unwrap(),
        "subprocess-stream-json finalize report"
    );
    let tx_raw = tx_log(&project_root);
    assert!(
        tx_raw.contains(":TYPE:         implementer.reported"),
        "subprocess-stream-json finalize must emit implementer.reported: {tx_raw}"
    );

    let _ = running.shutdown.send(());
    let _ = running.join.await;
}

/// TASK-8PXDP / HIGH1: when protocol-end wins the finalize race, finalize must
/// not mask the 404 and emit `implementer.reported` (which would orphan AND
/// report completion).
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
            "--mode",
            "stdio",
            "--harness",
            "codex",
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
            || stderr.contains("no worker-finalize tombstone")
            // orgasmic:TASK-RB1ZN — the third legitimate answer, and the one
            // this race gets whenever the racer's release is still running when
            // finalize's own release lands. The daemon used to answer that with
            // the same 404 it gives a run that is gone, which sent finalize into
            // the already-released rescue to read a tombstone nobody had written
            // yet. It answers 409 now, and finalize refuses in terms of the
            // release that IS running. Same contract either way: nonzero exit,
            // no completion tx (asserted below).
            || stderr.contains("released nothing"),
        "expected protocol-end refusal on stderr: {stderr}"
    );

    let tx_raw = tx_log(&project_root);
    assert!(
        !tx_raw.contains(":TYPE:         implementer.done")
            && !tx_raw.contains(":TYPE:         implementer.reported"),
        "protocol-end race must never emit a worker completion tx: {tx_raw}"
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
    let done_count = tx_raw
        .matches(":TYPE:         implementer.reported")
        .count();
    assert_eq!(
        done_count, 1,
        "concurrent double-finalize must emit exactly one implementer.reported: {tx_raw}"
    );

    let _ = running.shutdown.send(());
    let _ = running.join.await;
}

fn seed_stage_workers(home: &Home) {
    for (id, kind) in [("griller", "griller"), ("planner", "planner")] {
        write(
            &home.user().join(format!("workers/{id}.org")),
            format!(
                "* WORKER {id}\n:PROPERTIES:\n:ID:                          {id}\n:KIND:             {kind}\n:DRIVER:                      stdio\n:HARNESS:                     codex\n:PROVIDERS:                   openai\n:DEFAULT_PROVIDER:            openai\n:LINKED_SKILLS:\n:APPLICABLE_STATES:           working\n:MAX_ITERATIONS:              1\n:CONTEXT_BUDGET:              4000\n:VERSION:                     1\n:END:\n\n** Persona\nTest {kind}.\n"
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
    let response = http
        .post(format!("http://{addr}/api/{stage}"))
        .header(AUTHORIZATION, format!("Bearer {token}"))
        .json(&serde_json::json!({
            "project": "orgasmic",
            "task_id": task_id,
            "mode": "stdio",
            "harness": "codex",
            "reason": "stage finalize smoke",
        }))
        .send()
        .await
        .expect("start stage");
    let status = response.status();
    let body = response.text().await.expect("stage body");
    assert!(status.is_success(), "stage HTTP {status}: {body}");
    let resp: serde_json::Value = serde_json::from_str(&body)
        .unwrap_or_else(|err| panic!("decode stage response ({err}): {body}"));
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

    let proxy = start_runs_rejecting_proxy(running.addr).await;
    let stdout = run_orgasmic_output_with_daemon_url(
        &home,
        &format!("http://{}", proxy.addr),
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
    let paths = proxy.paths.lock().unwrap().clone();
    assert!(
        !paths.iter().any(|path| path == "/api/runs"),
        "explicit-id finalize must not enumerate runs: {paths:?}"
    );
    assert!(
        paths
            .iter()
            .any(|path| path == &format!("/api/runs/{run_id}")),
        "explicit-id finalize must resolve the exact run: {paths:?}"
    );

    drop(proxy);
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

    // TASK-6AYEJ.2 finding 3: "a stage run has no `manager.dispatch_started`,
    // so there is no dispatch for its finalize tx to leave open" is asserted
    // HERE, on the production path, because this test drove the real
    // `POST /api/plan` launch and the real finalize. The daemon-side unit test
    // never runs that path, so its negative assertion could not fail. (This
    // pair of blocks used to live on the `architect` stage, retired by
    // dec_HBK6A; the property is a property of every stage.)
    let project_tx = tx_log(&project_root);
    let home_tx = std::fs::read_to_string(home.tx().join(tx_file_name())).unwrap_or_default();
    for (label, ledger) in [("project", &project_tx), ("home", &home_tx)] {
        assert!(
            !ledger.contains("manager.dispatch_started"),
            "a stage launch must create no dispatch record ({label} ledger): {ledger}"
        );
    }
    assert!(
        project_tx.contains(":TYPE:         planner.done"),
        "the stage finalize's tx must be on record: {project_tx}"
    );
    let status_stdout = run_orgasmic(
        &home,
        &running,
        &project_root,
        &path_env,
        &["manager", "dispatch-status", "--task", "TASK-STAGE-PLAN"],
    );
    assert!(
        status_stdout.trim().is_empty(),
        "the stage's finalize tx must leave no dispatch open: {status_stdout}"
    );

    // TASK-C0XMR: the other half — that stage completion comes off the finalize
    // TOMBSTONE — is asserted HERE too, on the production path. This used to be
    // declined as "a property of the stub": the harness is reaped as the release
    // tears the driver down, so the session carries a teardown-induced fatal
    // driver error BEFORE the authoritative worker-finalize release. That is not
    // a stub artifact — `orgasmic dispatch finalize` always finalizes from the
    // worker's still-active turn, so a real worker produces the same ordering.
    // The stage must be recorded completed, never failed.
    let deadline = tokio::time::Instant::now() + Duration::from_secs(20);
    loop {
        let ledger = tx_log(&project_root);
        assert!(
            !ledger.contains(":TYPE:         plan.failed"),
            "a worker finalize from its active turn must not be recorded as \
             plan.failed: {ledger}"
        );
        if ledger.contains(":TYPE:         plan.completed") {
            break;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "timed out waiting for plan.completed: {ledger}"
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }

    let _ = running.shutdown.send(());
    let _ = running.join.await;
}

/// TASK-99W9C: app manager release via real CLI + HTTP with `ORGASMIC_RUN_ID`.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn app_manager_release_via_cli_orgasmic_run_id() {
    let _live_guard = live_session_guard();
    if skip_test_if_missing(
        "app_manager_release_via_cli_orgasmic_run_id",
        &[("tmux", tmux_available_for_test())],
    ) {
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

fn tx_extra_property(project_root: &Path, ty: &str, task: &str, key: &str) -> Option<String> {
    tx_property_raw_bytes(project_root, ty, task, key)
}

/// Read a tx property value from disk without org whitespace normalization.
fn tx_property_raw_bytes(project_root: &Path, ty: &str, task: &str, key: &str) -> Option<String> {
    let raw = tx_log(project_root);
    for block in raw.split("\n\n* TX ") {
        if block.contains(&format!(":TYPE:         {ty}"))
            && block.contains(&format!(":TASK:         {task}"))
        {
            for line in block.lines() {
                let trimmed = line.trim_start();
                if let Some(rest) = trimmed.strip_prefix(&format!(":{key}:")) {
                    let prefix_len = 2 + key.len();
                    let pad = if prefix_len < 15 { 15 - prefix_len } else { 1 };
                    return Some(rest.get(pad..)?.to_string());
                }
            }
        }
    }
    None
}

fn run_id_from_dispatch_stdout(stdout: &str) -> String {
    stdout
        .split("run_id=")
        .nth(1)
        .and_then(|tail| tail.split_whitespace().next())
        .expect("run_id in dispatch stdout")
        .to_string()
}

fn session_driver_config_field(project_root: &Path, run_id: &str, field: &str) -> Option<String> {
    let sessions_dir = project_root.join(".orgasmic/tmp/sessions");
    let entries = std::fs::read_dir(&sessions_dir).ok()?;
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().and_then(|ext| ext.to_str()) != Some("jsonl") {
            continue;
        }
        let body = std::fs::read_to_string(&path).ok()?;
        if !body.contains(run_id) {
            continue;
        }
        for line in body.lines() {
            let envelope: serde_json::Value = serde_json::from_str(line).ok()?;
            if envelope["run_id"].as_str() != Some(run_id) {
                continue;
            }
            if envelope["event"]["phase"].as_str() == Some("run_meta") {
                return envelope["event"]["driver_config"][field]
                    .as_str()
                    .map(str::to_string);
            }
        }
    }
    None
}

#[allow(clippy::too_many_arguments)]
fn run_dispatch_with_model_effort(
    home: &Home,
    running: &RunningDaemon,
    project_root: &Path,
    path_env: &std::ffi::OsString,
    head: &str,
    task: &str,
    worktree: &Path,
    branch: &str,
    model: Option<&str>,
    effort: Option<&str>,
) -> String {
    run_dispatch_with_model_effort_output(
        home,
        running,
        project_root,
        path_env,
        head,
        task,
        worktree,
        branch,
        model,
        effort,
    )
}

#[allow(clippy::too_many_arguments)]
fn run_dispatch_with_model_effort_output(
    home: &Home,
    running: &RunningDaemon,
    project_root: &Path,
    path_env: &std::ffi::OsString,
    head: &str,
    task: &str,
    worktree: &Path,
    branch: &str,
    model: Option<&str>,
    effort: Option<&str>,
) -> String {
    let brief = worktree.with_file_name(format!("{task}-brief.md"));
    write(&brief, format!("wire test brief for {task}"));
    let mut args = vec![
        "manager",
        "dispatch",
        "--task",
        task,
        "--kind",
        "implementer",
        "--mode",
        "ws",
        "--harness",
        "codex",
        "--brief",
        brief.to_str().unwrap(),
        "--from",
        head,
        "--worktree",
        worktree.to_str().unwrap(),
        "--branch",
        branch,
        "--reason",
        "model/effort wire test",
    ];
    if let Some(model) = model {
        args.push("--model");
        args.push(model);
    }
    if let Some(effort) = effort {
        args.push("--effort");
        args.push(effort);
    }
    run_orgasmic(home, running, project_root, path_env, &args)
}

/// TASK-VQNZ9 P4: CLI subprocess → HTTP/API preserves mixed-case/whitespace model/effort bytes in tx.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn dispatch_cli_wire_mixed_case_whitespace_model_effort_verbatim_in_tx() {
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
    let worktree = tmp.path().join("worktrees/task-wire-mixed");
    let model = "  Composer-2.5-FAST  ";
    let effort = " XHIGH ";

    let running = boot(home.clone()).await;
    run_dispatch_with_model_effort(
        &home,
        &running,
        &project_root,
        &path_env,
        &head,
        "TASK-DISPATCH",
        &worktree,
        "task-wire-mixed-impl",
        Some(model),
        Some(effort),
    );

    assert_eq!(
        tx_property_raw_bytes(
            &project_root,
            "manager.dispatch_started",
            "TASK-DISPATCH",
            "MODEL"
        )
        .as_deref(),
        Some(model)
    );
    assert_eq!(
        tx_property_raw_bytes(
            &project_root,
            "manager.dispatch_started",
            "TASK-DISPATCH",
            "EFFORT"
        )
        .as_deref(),
        Some(effort)
    );
    assert_eq!(
        tx_property_raw_bytes(
            &project_root,
            "run.created",
            "TASK-DISPATCH",
            "MODEL_OVERRIDE"
        )
        .as_deref(),
        Some(model)
    );
    assert_eq!(
        tx_property_raw_bytes(
            &project_root,
            "run.created",
            "TASK-DISPATCH",
            "EFFORT_OVERRIDE"
        )
        .as_deref(),
        Some(effort)
    );

    let _ = running.shutdown.send(());
    let _ = running.join.await;
}

/// TASK-VQNZ9 P4: unknown off-list model values pass through verbatim (no worker catalog gate).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn dispatch_cli_wire_unknown_model_passes_through() {
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
    let worktree = tmp.path().join("worktrees/task-wire-unknown");

    let running = boot(home.clone()).await;
    run_dispatch_with_model_effort(
        &home,
        &running,
        &project_root,
        &path_env,
        &head,
        "TASK-ABORT",
        &worktree,
        "task-wire-unknown-impl",
        Some("gpt-99"),
        None,
    );

    assert_eq!(
        tx_extra_property(
            &project_root,
            "manager.dispatch_started",
            "TASK-ABORT",
            "MODEL"
        )
        .as_deref(),
        Some("gpt-99")
    );
    assert_eq!(
        tx_extra_property(&project_root, "run.created", "TASK-ABORT", "MODEL_OVERRIDE").as_deref(),
        Some("gpt-99")
    );

    let _ = running.shutdown.send(());
    let _ = running.join.await;
}

/// TASK-VQNZ9 P4: omitted model/effort flags do not synthesize tx properties from worker files.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn dispatch_cli_wire_omitted_model_effort_absent_from_tx() {
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
    let worktree = tmp.path().join("worktrees/task-wire-omit");

    let running = boot(home.clone()).await;
    run_dispatch_with_model_effort(
        &home,
        &running,
        &project_root,
        &path_env,
        &head,
        "TASK-FIX",
        &worktree,
        "task-wire-omit-impl",
        None,
        None,
    );

    let raw = tx_log(&project_root);
    for block in raw.split("\n\n* TX ") {
        if block.contains(":TASK:         TASK-FIX") {
            assert!(
                !block.contains(":MODEL:") && !block.contains(":EFFORT:"),
                "omitted overrides must not appear in tx: {block}"
            );
            assert!(
                !block.contains(":MODEL_OVERRIDE:") && !block.contains(":EFFORT_OVERRIDE:"),
                "omitted overrides must not appear in run.created: {block}"
            );
        }
    }

    let _ = running.shutdown.send(());
    let _ = running.join.await;
}

/// TASK-VQNZ9 P4: whitespace-only model/effort strings are preserved verbatim, not trimmed away.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn dispatch_cli_wire_whitespace_only_model_effort_preserved() {
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
    let worktree = tmp.path().join("worktrees/task-wire-blank");

    let running = boot(home.clone()).await;
    let dispatch_stdout = run_dispatch_with_model_effort_output(
        &home,
        &running,
        &project_root,
        &path_env,
        &head,
        "TASK-NO-MERGE",
        &worktree,
        "task-wire-blank-impl",
        Some("   "),
        Some("\t"),
    );
    let run_id = run_id_from_dispatch_stdout(&dispatch_stdout);

    assert!(
        tx_log(&project_root).contains(":MODEL:"),
        "whitespace-only model must still emit a MODEL tx property"
    );
    assert_eq!(
        session_driver_config_field(&project_root, &run_id, "model").as_deref(),
        Some("   ")
    );
    assert_eq!(
        session_driver_config_field(&project_root, &run_id, "reasoning_effort").as_deref(),
        Some("\t")
    );

    let _ = running.shutdown.send(());
    let _ = running.join.await;
}

/// TASK-VQNZ9 P4: CLI → API → session driver_config preserves verbatim model bytes.
#[cfg(unix)]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn dispatch_cli_subprocess_wire_preserves_verbatim_model_in_session() {
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
    write_git_proxy(&bin_dir);
    let path_env = path_only(&bin_dir);
    let worktree = tmp.path().join("worktrees/task-wire-argv");
    let model = "  Composer-2.5-FAST  ";

    let running = boot(home.clone()).await;
    let brief = tmp.path().join("task-cleanup-brief.md");
    write(&brief, "session driver_config wire test");
    let dispatch_stdout = run_orgasmic(
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
            "--mode",
            "subprocess-stream-json",
            "--harness",
            "cursor-agent",
            "--brief",
            brief.to_str().unwrap(),
            "--from",
            &head,
            "--worktree",
            worktree.to_str().unwrap(),
            "--branch",
            "task-wire-argv-impl",
            "--model",
            model,
            "--reason",
            "session driver_config wire test",
        ],
    );
    let run_id = run_id_from_dispatch_stdout(&dispatch_stdout);
    assert_eq!(
        session_driver_config_field(&project_root, &run_id, "model").as_deref(),
        Some(model)
    );

    let _ = running.shutdown.send(());
    let _ = running.join.await;
}

/// TASK-GQPGR injection. A dispatch worktree carries a FROZEN `.orgasmic/`
/// snapshot from the commit it was created at, so the cwd marker walk in
/// `find_project_root` hands `manager dispatch-status` a plausibly-shaped but
/// stale project. Measured 2026-07-28 against the live project: the verb
/// printed EMPTY from inside a worktree while three dispatches were open and
/// their workers healthy — read, briefly, as "all workers died".
///
/// Red signature without the guard: the second invocation SUCCEEDS with empty
/// stdout, so `run_orgasmic_output(...).status.success()` is true and this
/// test fails on `refusing to read project state`.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn dispatch_status_from_dispatch_worktree_refuses_frozen_snapshot_answer() {
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
    let brief = project_root.join(".orgasmic/tmp/dispatch/task-dispatch/task-dispatch-brief.md");
    write(&brief, "frozen snapshot guard brief");

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
            "--mode",
            "ws",
            "--harness",
            "codex",
            "--brief",
            brief.to_str().unwrap(),
            "--from",
            &head,
            "--reason",
            "frozen snapshot guard",
        ],
    );
    assert!(dispatch_stdout.contains("dispatched: TASK-DISPATCH implementer pid="));
    // Since TASK-M47E5 the default layout puts the worktree under the HOME,
    // not inside the project. The guard this test is about is about the frozen
    // `.orgasmic/` a LINKED WORKTREE carries, not about where that worktree
    // sits, so it must keep firing from the new location.
    let worktree = home.root.join("worktrees/orgasmic/task-dispatch");
    assert!(
        worktree.is_dir(),
        "default worktree should exist at {}",
        worktree.display()
    );
    assert!(
        worktree.join(".orgasmic/project.org").is_file(),
        "the worktree must carry the frozen .orgasmic snapshot this test is about"
    );

    // Live truth, read from the primary root.
    let from_root = run_orgasmic(
        &home,
        &running,
        &project_root,
        &path_env,
        &["manager", "dispatch-status"],
    );
    assert!(
        from_root.contains("TASK=TASK-DISPATCH"),
        "primary root must see the open dispatch, got:\n{from_root}"
    );

    // Same verb, same machine, cwd inside the worktree: must not answer from
    // the snapshot.
    let output = run_orgasmic_output(
        &home,
        &running,
        &worktree,
        &path_env,
        &["manager", "dispatch-status"],
    );
    let stdout = String::from_utf8_lossy(&output.stdout).to_string();
    let stderr = String::from_utf8_lossy(&output.stderr).to_string();
    assert!(
        !output.status.success(),
        "dispatch-status from a dispatch worktree must refuse, not answer from the \
         frozen snapshot; exit ok with stdout={stdout:?} stderr={stderr:?}"
    );
    assert!(
        stderr.contains("refusing to read project state"),
        "refusal must name the situation, got stderr:\n{stderr}"
    );
    let primary = std::fs::canonicalize(&project_root).unwrap();
    assert!(
        stderr.contains(&primary.display().to_string()),
        "refusal must name the primary project root {}, got stderr:\n{stderr}",
        primary.display()
    );
    assert!(
        stderr.contains("TASK-DISPATCH"),
        "refusal should name the dispatch this worktree belongs to, got stderr:\n{stderr}"
    );

    let _ = running.shutdown.send(());
    let _ = running.join.await;
}

/// TASK-GQPGR carve-out. The worker-authority `dispatch` verb group
/// (dec_3M7M0) runs from inside the worker's own worktree by definition, and
/// the frozen-snapshot guard must not touch it — a guard that bricks workers
/// is worse than no guard. Uses the default nested layout, where the worktree
/// does carry a `.orgasmic/project.org` snapshot, so the guard's detection
/// condition is genuinely satisfied here and still must not fire.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn dispatch_finalize_still_works_from_inside_its_own_worktree() {
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
    let brief = project_root.join(".orgasmic/tmp/dispatch/task-dispatch/task-dispatch-brief.md");
    write(&brief, "worker carve-out brief");

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
            "--mode",
            "ws",
            "--harness",
            "codex",
            "--brief",
            brief.to_str().unwrap(),
            "--from",
            &head,
            "--reason",
            "worker carve-out",
        ],
    );
    assert!(dispatch_stdout.contains("dispatched: TASK-DISPATCH implementer pid="));
    let worktree = home.root.join("worktrees/orgasmic/task-dispatch");
    assert!(
        worktree.join(".orgasmic/project.org").is_file(),
        "carve-out is only meaningful when the worktree carries the snapshot"
    );

    let summary_path = tmp.path().join("summary.md");
    write(&summary_path, "worker carve-out summary");
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
        finalize_stdout.contains("finalized: TASK-DISPATCH implementer.reported tx="),
        "worker finalize must keep working from its own worktree: {finalize_stdout}"
    );

    let _ = running.shutdown.send(());
    let _ = running.join.await;
}

/// TASK-GQPGR acceptance #2. Daemon-routed writes resolve the project by ID
/// and the daemon owns the live root, so `task update` from inside a dispatch
/// worktree must land in the LIVE ledger — never return a tx id that only the
/// frozen snapshot would explain. Workers run these verbs from their worktree
/// constantly, so this path is deliberately NOT guarded; this test is what
/// makes "unguarded" a checked claim rather than an assumption.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn task_update_from_dispatch_worktree_lands_in_the_live_ledger() {
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

    // A real dispatch-shaped worktree at the RETIRED in-project layout, frozen
    // at `head`. Kept there deliberately (TASK-M47E5): worktrees created before
    // the managed root moved are still on disk after an upgrade, and every verb
    // a worker runs from one has to keep working.
    let worktree = project_root.join(".orgasmic/tmp/dispatch/task-dispatch/worktree");
    run_git(
        &project_root,
        &[
            "worktree",
            "add",
            "-b",
            "task-dispatch-ledger",
            worktree.to_str().unwrap(),
            &head,
        ],
    );
    assert!(worktree.join(".orgasmic/project.org").is_file());

    let running = boot(home.clone()).await;
    let stdout = run_orgasmic(
        &home,
        &running,
        &worktree,
        &path_env,
        &[
            "task",
            "update",
            "TASK-DISPATCH",
            "--state",
            "todo",
            "--reason",
            "frozen snapshot ledger check",
        ],
    );
    let payload: serde_json::Value = serde_json::from_str(&stdout).expect("task update json");
    let tx_id = payload["tx_id"]
        .as_str()
        .unwrap_or_else(|| panic!("task update returned no tx_id: {stdout}"))
        .to_string();

    assert!(
        tx_log(&project_root).contains(&tx_id),
        "tx {tx_id} returned from inside the worktree must exist in the LIVE ledger"
    );
    assert!(
        !worktree.join(".orgasmic/tx").exists(),
        "the frozen snapshot must not be the thing that absorbed the write"
    );
    let from_root = run_orgasmic(
        &home,
        &running,
        &project_root,
        &path_env,
        &["task", "get", "TASK-DISPATCH"],
    );
    let task: serde_json::Value = serde_json::from_str(&from_root).expect("task get json");
    assert_eq!(
        task["lifecycle_stage"].as_str(),
        Some("todo"),
        "the update must be visible from the primary root: {from_root}"
    );

    let _ = running.shutdown.send(());
    let _ = running.join.await;
}

/// A listener that completes the TCP handshake and then never answers, so a
/// client request runs out its own timeout instead of failing to connect.
/// This is what scheduler starvation looks like from the CLI's side of the
/// socket: the daemon is *there*, it just does not get a turn (TASK-EP3H1).
struct HangingDaemon {
    addr: std::net::SocketAddr,
    join: tokio::task::JoinHandle<()>,
}

impl Drop for HangingDaemon {
    fn drop(&mut self) {
        self.join.abort();
    }
}

async fn start_hanging_daemon() -> HangingDaemon {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind hanging daemon");
    let addr = listener.local_addr().unwrap();
    let join = tokio::spawn(async move {
        let mut held = Vec::new();
        while let Ok((stream, _)) = listener.accept().await {
            held.push(stream);
        }
    });
    HangingDaemon { addr, join }
}

/// A daemon whose task lifecycle write for TASK-CLEANUP is unavailable, so a
/// `dispatch-close` appends its close tx and then loses the lifecycle leg —
/// the exact tear TASK-EP3H1 was measured on. Every other route is the real
/// daemon, so the close really closes.
async fn start_lifecycle_rejecting_proxy(backend: std::net::SocketAddr) -> InterceptingProxy {
    start_intercepting_proxy(backend, |path| {
        (path == "/api/projects/orgasmic/tasks/TASK-CLEANUP").then_some((
            503,
            "Service Unavailable",
            "{\"error\":\"lifecycle write unavailable\"}",
        ))
    })
    .await
}

/// Seed a hand-written open implementer dispatch for TASK-CLEANUP, so a close
/// can be driven without spawning a worker.
fn seed_open_dispatch_tx(project_root: &Path, started_tx: &str, worktree: &Path, brief: &Path) {
    write(
        &tx_file_path(project_root),
        format!(
            "#+title: tx\n#+orgasmic_version: 1\n\n* TX 2026-07-29 Wed 10:00:00 manager.dispatch_started TASK-CLEANUP\n:PROPERTIES:\n:TX_ID:        {started_tx}\n:TIME:         [2026-07-29 Wed 10:00:00]\n:TYPE:         manager.dispatch_started\n:ACTOR:        a@example.com\n:MACHINE:      host\n:PROJECT:      orgasmic\n:TASK:         TASK-CLEANUP\n:KIND:         implementer\n:WORKTREE:     {}\n:BRANCH:       task-cleanup-impl\n:CODEX_BRIEF_PATH: {}\n:STARTED_AT:   [2026-07-29 Wed 10:00:00]\n:END:\n",
            worktree.display(),
            brief.display()
        ),
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn torn_dispatch_close_lifecycle_is_repaired_by_next_manager_command() {
    // TASK-EP3H1: `dispatch-close` writes the close tx and the task lifecycle
    // transition as two daemon requests. When the second one fails — measured
    // as a client-side timeout at load average ~190 — the close tx is on the
    // ledger and the task is stranded at its pre-close stage, and the operator
    // is left to repair it by hand. The close records the transition it
    // intended, so the NEXT manager command finishes it.
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
    let worktree = tmp.path().join("worktrees/task-cleanup");
    std::fs::create_dir_all(&worktree).unwrap();
    let brief = tmp.path().join("codex/task-cleanup-brief.md");
    write(&brief, "cleanup brief");
    seed_open_dispatch_tx(&project_root, "tx-start-torn", &worktree, &brief);

    let running = boot(home.clone()).await;
    let proxy = start_lifecycle_rejecting_proxy(running.addr).await;
    let output = run_orgasmic_output_with_daemon_url(
        &home,
        &format!("http://{}", proxy.addr),
        &project_root,
        &path_env,
        &[
            "manager",
            "dispatch-close",
            "--task",
            "TASK-CLEANUP",
            "--started-tx",
            "tx-start-torn",
            "--status",
            "done",
            "--merge-sha",
            &head,
            "--no-worktree-remove",
        ],
        &[],
    );
    assert!(
        output.status.success(),
        "the close tx leg succeeds, so the command still exits 0\nstdout={}\nstderr={}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    let close_stderr = String::from_utf8_lossy(&output.stderr).to_string();
    assert!(
        close_stderr.contains("close tx appended but lifecycle update failed"),
        "the tear must stay loud: {close_stderr}"
    );
    assert_task_stage(&project_root, "TASK-CLEANUP", "BACKLOG", "backlog");
    drop(proxy);

    // The next manager command finishes the transition the close recorded.
    let status_stdout = run_orgasmic(
        &home,
        &running,
        &project_root,
        &path_env,
        &["manager", "dispatch-status", "--task", "TASK-CLEANUP"],
    );
    assert!(
        status_stdout.contains("reconciled: TASK-CLEANUP backlog -> in_review"),
        "the next manager command must repair the torn close and say so: {status_stdout}"
    );
    assert_task_stage(&project_root, "TASK-CLEANUP", "IN_REVIEW", "in_review");

    // ...and it is not a standing repair loop: a second run has nothing to do.
    let repeat_stdout = run_orgasmic(
        &home,
        &running,
        &project_root,
        &path_env,
        &["manager", "dispatch-status", "--task", "TASK-CLEANUP"],
    );
    assert!(
        !repeat_stdout.contains("reconciled:"),
        "a repaired close must not reconcile twice: {repeat_stdout}"
    );

    let _ = running.shutdown.send(());
    let _ = running.join.await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn resubmitted_close_and_transition_are_labelled_no_ops() {
    // TASK-EP3H1: a timed-out request can succeed server-side, so the repair
    // path re-submits work that already landed. The daemon must say which of
    // the two no-ops it is — "this exact request already applied" (with the
    // tx it wrote) or "the task was already in that state and this request did
    // nothing" — instead of an unlabelled `{"changed":{},"tx_id":""}`.
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
    let worktree = tmp.path().join("worktrees/task-cleanup");
    std::fs::create_dir_all(&worktree).unwrap();
    let brief = tmp.path().join("codex/task-cleanup-brief.md");
    write(&brief, "cleanup brief");
    seed_open_dispatch_tx(&project_root, "tx-start-noop", &worktree, &brief);

    let running = boot(home.clone()).await;
    let close_args = [
        "manager",
        "dispatch-close",
        "--task",
        "TASK-CLEANUP",
        "--started-tx",
        "tx-start-noop",
        "--status",
        "done",
        "--merge-sha",
        &head,
        "--no-worktree-remove",
    ];
    let first = run_orgasmic(&home, &running, &project_root, &path_env, &close_args);
    assert!(first.contains("closed: TASK-CLEANUP implementer.done tx="));
    assert_task_stage(&project_root, "TASK-CLEANUP", "IN_REVIEW", "in_review");

    let second = run_orgasmic(&home, &running, &project_root, &path_env, &close_args);
    assert!(
        second.contains("already-closed: TASK-CLEANUP started_tx=tx-start-noop (no-op)"),
        "a re-submitted close must be a labelled no-op: {second}"
    );

    // The lifecycle leg's own no-op contract, on the wire.
    let client = reqwest::Client::new();
    let token = std::fs::read_to_string(home.root.join("user/auth/token")).unwrap();
    let url = format!(
        "http://{}/api/projects/orgasmic/tasks/TASK-CLEANUP",
        running.addr
    );
    let post = |body: serde_json::Value| {
        let client = client.clone();
        let url = url.clone();
        let token = token.trim().to_string();
        async move {
            client
                .post(&url)
                .header(AUTHORIZATION, format!("Bearer {token}"))
                .json(&body)
                .send()
                .await
                .expect("post task state")
                .json::<serde_json::Value>()
                .await
                .expect("task state json")
        }
    };

    let applied = post(serde_json::json!({"state": "done", "request_id": "ep3h1-repair"})).await;
    assert_eq!(applied["changed"]["STATE"].as_str(), Some("done"));
    let applied_tx = applied["tx_id"].as_str().unwrap_or_default().to_string();
    assert!(!applied_tx.is_empty(), "a real transition writes a tx");

    let replayed = post(serde_json::json!({"state": "done", "request_id": "ep3h1-repair"})).await;
    assert_eq!(
        replayed["status"].as_str(),
        Some("already_applied"),
        "a re-submitted transition must say the request already landed: {replayed}"
    );
    assert_eq!(
        replayed["tx_id"].as_str(),
        Some(applied_tx.as_str()),
        "an already-applied replay must carry the tx it wrote: {replayed}"
    );

    let untouched = post(serde_json::json!({"state": "done", "request_id": "ep3h1-fresh"})).await;
    assert_eq!(
        untouched["status"].as_str(),
        Some("already_in_state"),
        "a different request that finds the state already set is a distinct no-op: {untouched}"
    );
    assert_eq!(
        untouched["tx_id"].as_str(),
        Some(""),
        "nothing-to-do writes no tx: {untouched}"
    );

    let _ = running.shutdown.send(());
    let _ = running.join.await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn client_timeout_reports_load_not_daemon_death() {
    // TASK-EP3H1: three times on 2026-07-29 the CLI told an operator "is the
    // daemon reachable?" while the daemon was healthy and answering raw HTTP
    // in 0.4s. A client timeout is a statement about the client's patience,
    // not about the daemon's health.
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

    let running = boot(home.clone()).await;
    let hanging = start_hanging_daemon().await;
    let output = run_orgasmic_output_with_daemon_url(
        &home,
        &format!("http://{}", hanging.addr),
        &project_root,
        &path_env,
        &["task", "get", "--project", "orgasmic", "TASK-CLEANUP"],
        &[],
    );
    assert!(
        !output.status.success(),
        "a request that never gets an answer must still fail"
    );
    let stderr = String::from_utf8_lossy(&output.stderr).to_string();
    assert!(
        stderr.contains("daemon request timed out after 10s"),
        "the timeout must name itself and the budget it spent: {stderr}"
    );
    assert!(
        stderr.contains("the daemon may be healthy but the system is under load"),
        "a timeout must not be reported as daemon death: {stderr}"
    );
    assert!(
        !stderr.contains("is the daemon reachable?"),
        "the misdiagnosis must be gone from the timeout path: {stderr}"
    );

    drop(hanging);
    let _ = running.shutdown.send(());
    let _ = running.join.await;
}

// ===== TASK-M47E5: managed worktree relocation and reclamation ===========

/// Create a managed-layout worktree by hand, the way `manager dispatch` now
/// does: `<home>/worktrees/<project-id>/<stem>`.
fn add_managed_worktree(
    home: &Home,
    project_root: &Path,
    stem: &str,
    branch: &str,
    from: &str,
) -> PathBuf {
    let path = home.root.join("worktrees/orgasmic").join(stem);
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    run_git(
        project_root,
        &[
            "worktree",
            "add",
            "-b",
            branch,
            path.to_str().unwrap(),
            from,
        ],
    );
    path
}

fn field(stdout: &str, line_prefix: &str, key: &str) -> Option<String> {
    let line = stdout.lines().find(|line| line.starts_with(line_prefix))?;
    let rest = line.split(&format!("{key}=")).nth(1)?;
    Some(
        rest.split_whitespace()
            .next()
            .unwrap_or_default()
            .to_string(),
    )
}

/// TASK-M47E5 acceptance: the prune verb reclaims a worktree no open dispatch
/// owns, salvages its uncommitted work first, and reports the bytes it
/// returned. `--dry-run` measures the same thing and changes nothing.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn worktree_prune_reclaims_an_unclaimed_worktree_after_salvaging_it() {
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

    let worktree = add_managed_worktree(
        &home,
        &project_root,
        "task-dispatch",
        "task-dispatch-impl",
        &head,
    );
    // Uncommitted worker output — the thing TASK-2BPWM/TASK-D0GA3 exist about.
    write(
        &worktree.join("worker-output.txt"),
        "unmerged worker output",
    );

    let running = boot(home.clone()).await;

    // Dry run first: it must measure and refuse to touch anything.
    let dry = run_orgasmic(
        &home,
        &running,
        &project_root,
        &path_env,
        &["manager", "worktree-prune", "--dry-run"],
    );
    assert!(
        dry.contains(&format!("WOULD_RECLAIM PATH={}", worktree.display())),
        "dry run must name the reclaimable worktree, got:\n{dry}"
    );
    assert!(
        dry.contains("DRY_RUN RECLAIMABLE=1"),
        "dry run must report the count, got:\n{dry}"
    );
    assert!(worktree.is_dir(), "a dry run must remove nothing");

    let stdout = run_orgasmic(
        &home,
        &running,
        &project_root,
        &path_env,
        &["manager", "worktree-prune"],
    );
    assert!(
        stdout.contains(&format!("SALVAGED PATH={}", worktree.display())),
        "a dirty worktree must be salvaged before removal, got:\n{stdout}"
    );
    let salvage_ref = field(&stdout, "SALVAGED ", "REF").expect("salvage ref in output");
    assert!(
        salvage_ref.starts_with("refs/orgasmic/salvage/"),
        "salvage must reuse the existing ref namespace, got {salvage_ref}"
    );
    assert!(
        Command::new("git")
            .args(["rev-parse", "--verify", "--quiet", &salvage_ref])
            .current_dir(&project_root)
            .output()
            .expect("git rev-parse")
            .status
            .success(),
        "the salvage ref must exist in the repo: {salvage_ref}"
    );
    assert!(
        stdout.contains(&format!("RECLAIMED PATH={}", worktree.display())),
        "the worktree must be reclaimed, got:\n{stdout}"
    );
    let bytes: u64 = field(&stdout, "PRUNE_SUMMARY ", "BYTES")
        .expect("summary bytes")
        .parse()
        .expect("bytes is a number");
    assert!(bytes > 0, "bytes reclaimed must be measured, got {bytes}");
    assert!(
        stdout.contains("PRUNE_SUMMARY RECLAIMED=1"),
        "the summary must count what it reclaimed, got:\n{stdout}"
    );
    assert!(!worktree.exists(), "the worktree must be gone");

    let _ = running.shutdown.send(());
    let _ = running.join.await;
}

/// TASK-RMA18.1 finding 1: `git worktree remove` refuses a worktree containing
/// an INITIALIZED SUBMODULE outright, before it considers cleanliness, and
/// TASK-RMA18 reproduced only its locked and unclean refusals — so this verb
/// recursively deleted a tree git declines to touch without `--force`.
///
/// This is the DATA-LOSS variant, and every part of it is committed fixture
/// state rather than a contrivance: `submodule.<name>.ignore = all` lives in
/// `.gitmodules`, so every clone inherits it and the parent reports the tree
/// CLEAN while the submodule holds untracked work. No salvage runs, and salvage
/// could not have captured it anyway — the parent records a submodule as a
/// gitlink, so `git add -A` never sees files inside one.
///
/// The sentinel SURVIVING is the assertion. Against the pre-fix code this test
/// fails on exactly that line.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn worktree_prune_refuses_a_worktree_containing_an_initialized_submodule() {
    let tmp = tempfile::tempdir().unwrap();
    let home = Home::at(tmp.path().join("home"));
    home.ensure().unwrap();
    let project_root = tmp.path().join("project");
    std::fs::create_dir_all(&project_root).unwrap();
    seed_project(&home, &project_root);
    init_git_project(&project_root);

    // A real second repository to be the submodule.
    let sub_origin = tmp.path().join("subrepo");
    std::fs::create_dir_all(&sub_origin).unwrap();
    write(&sub_origin.join("lib.txt"), "library source");
    run_git(&sub_origin, &["init", "-b", "main"]);
    run_git(&sub_origin, &["config", "user.email", "tester@example.com"]);
    run_git(&sub_origin, &["config", "user.name", "Test User"]);
    run_git(&sub_origin, &["add", "."]);
    run_git(&sub_origin, &["commit", "-m", "sub init"]);

    run_git(
        &project_root,
        &[
            "-c",
            "protocol.file.allow=always",
            "submodule",
            "add",
            sub_origin.to_str().unwrap(),
            "vendor/sub",
        ],
    );
    // COMMITTED into `.gitmodules`: this is the one config line that turns a
    // submodule holding uncommitted work into a tree the parent calls clean.
    run_git(
        &project_root,
        &[
            "config",
            "-f",
            ".gitmodules",
            "submodule.vendor/sub.ignore",
            "all",
        ],
    );
    run_git(&project_root, &["add", "-A"]);
    run_git(&project_root, &["commit", "-m", "add submodule"]);
    let head = run_git(&project_root, &["rev-parse", "HEAD"]);

    let bin_dir = tmp.path().join("bin");
    std::fs::create_dir_all(&bin_dir).unwrap();
    let path_env = path_with_stub(&bin_dir);

    let worktree = add_managed_worktree(
        &home,
        &project_root,
        "task-submodule",
        "task-submodule-impl",
        &head,
    );
    // `git worktree add` does NOT init submodules; a worker that must BUILD
    // does exactly this, which is what makes the finding reachable.
    run_git(
        &worktree,
        &[
            "-c",
            "protocol.file.allow=always",
            "submodule",
            "update",
            "--init",
        ],
    );
    let sentinel = worktree.join("vendor/sub/SENTINEL.txt");
    write(&sentinel, "uncommitted worker output no salvage can reach");

    // The premise, measured in the fixture rather than asserted in prose: the
    // parent reports CLEAN, and git still refuses to remove the worktree.
    let porcelain = run_git(&worktree, &["status", "--porcelain"]);
    assert!(
        porcelain.trim().is_empty(),
        "fixture premise: with ignore=all the parent must report the tree clean, got:\n{porcelain}"
    );
    let refusal = Command::new("git")
        .args(["worktree", "remove", worktree.to_str().unwrap()])
        .current_dir(&project_root)
        .output()
        .expect("git worktree remove");
    assert!(
        !refusal.status.success()
            && String::from_utf8_lossy(&refusal.stderr).contains("submodules"),
        "fixture premise: git itself must refuse this removal, got status={} stderr={}",
        refusal.status,
        String::from_utf8_lossy(&refusal.stderr)
    );

    let running = boot(home.clone()).await;
    let stdout = run_orgasmic(
        &home,
        &running,
        &project_root,
        &path_env,
        &["manager", "worktree-prune"],
    );

    assert!(
        sentinel.is_file(),
        "the untracked file inside the submodule must SURVIVE the prune, got:\n{stdout}"
    );
    assert!(
        worktree.is_dir(),
        "the worktree itself must survive, got:\n{stdout}"
    );
    assert!(
        !stdout.contains(&format!("RECLAIMED PATH={}", worktree.display())),
        "the worktree must not be reclaimed, got:\n{stdout}"
    );
    let kept = stdout
        .lines()
        .find(|line| line.starts_with(&format!("KEPT PATH={}", worktree.display())))
        .unwrap_or_else(|| panic!("a KEPT line naming the worktree, got:\n{stdout}"));
    assert!(
        kept.contains("submodule"),
        "the report must say WHY it was kept, got:\n{kept}"
    );
    assert!(
        !stdout.contains(&format!("SALVAGED PATH={}", worktree.display())),
        "nothing was salvageable, so nothing must claim to have been salvaged, got:\n{stdout}"
    );

    let _ = running.shutdown.send(());
    let _ = running.join.await;
}

/// TASK-RMA18.1.1 finding 1: the refusal asked `.gitmodules`; GIT ASKS THE
/// INDEX. A gitlink committed with `git update-index --cacheinfo 160000` and
/// populated by an ordinary standalone clone has NO `.gitmodules` entry and NO
/// worktree admin `modules/` directory, so neither implemented branch fired and
/// this verb recursively deleted a tree git refuses to touch.
///
/// The fixture proves git's own verdict FIRST — `git status` and
/// `git status --ignore-submodules=none` both empty, `git worktree remove`
/// exiting 128 with `working trees containing submodules cannot be moved or
/// removed` — so the refusal under test is git's, not this suite's invention.
/// The sentinel SURVIVING is the assertion.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn worktree_prune_refuses_a_submodule_recorded_only_in_the_index() {
    let tmp = tempfile::tempdir().unwrap();
    let home = Home::at(tmp.path().join("home"));
    home.ensure().unwrap();
    let project_root = tmp.path().join("project");
    std::fs::create_dir_all(&project_root).unwrap();
    seed_project(&home, &project_root);
    init_git_project(&project_root);

    // A real second repository, cloned in as a PLAIN repository below — never
    // registered as a submodule, so nothing ever writes `.gitmodules`.
    let sub_origin = tmp.path().join("subrepo");
    std::fs::create_dir_all(&sub_origin).unwrap();
    write(&sub_origin.join("lib.txt"), "library source");
    run_git(&sub_origin, &["init", "-b", "main"]);
    run_git(&sub_origin, &["config", "user.email", "tester@example.com"]);
    run_git(&sub_origin, &["config", "user.name", "Test User"]);
    run_git(&sub_origin, &["add", "."]);
    run_git(&sub_origin, &["commit", "-m", "sub init"]);
    let sub_head = run_git(&sub_origin, &["rev-parse", "HEAD"]);

    // The whole fixture premise in one command: a mode-160000 index entry, and
    // no other record of the submodule anywhere.
    run_git(
        &project_root,
        &[
            "update-index",
            "--add",
            "--cacheinfo",
            &format!("160000,{sub_head},vendor/sub"),
        ],
    );
    run_git(&project_root, &["commit", "-m", "gitlink, no .gitmodules"]);
    let head = run_git(&project_root, &["rev-parse", "HEAD"]);

    let bin_dir = tmp.path().join("bin");
    std::fs::create_dir_all(&bin_dir).unwrap();
    let path_env = path_with_stub(&bin_dir);

    let worktree = add_managed_worktree(
        &home,
        &project_root,
        "task-indexsub",
        "task-indexsub-impl",
        &head,
    );
    // A worker that needs the dependency to build clones it itself. This is the
    // shape `git submodule update --init` never touched: no `.gitmodules` to
    // read, so no admin `modules/` directory is ever created either.
    let checkout = worktree.join("vendor/sub");
    let _ = std::fs::remove_dir(&checkout);
    run_git(
        &worktree,
        &[
            "clone",
            sub_origin.to_str().unwrap(),
            checkout.to_str().unwrap(),
        ],
    );
    // Ignored inside the nested repository, so the parent reports CLEAN and the
    // deletion happens with no salvage and no refusal.
    write(&checkout.join(".git/info/exclude"), "SENTINEL.txt\n");
    let sentinel = checkout.join("SENTINEL.txt");
    write(&sentinel, "uncommitted worker output no salvage can reach");

    // Fixture premises, MEASURED: neither of the two branches TASK-RMA18.1
    // implemented has anything to fire on.
    assert!(
        !worktree.join(".gitmodules").exists(),
        "fixture premise: there must be no .gitmodules for the old predicate to read"
    );
    let admin = project_root.join(".git/worktrees/task-indexsub");
    assert!(
        !admin.join("modules").exists(),
        "fixture premise: the worktree admin directory must hold no `modules` directory"
    );
    assert_eq!(
        run_git(&worktree, &["ls-files", "-s", "vendor/sub"])
            .split_whitespace()
            .next()
            .unwrap_or_default(),
        "160000",
        "fixture premise: the index entry must be a gitlink"
    );
    for args in [
        ["status", "--porcelain"].as_slice(),
        ["status", "--porcelain", "--ignore-submodules=none"].as_slice(),
    ] {
        let porcelain = run_git(&worktree, args);
        assert!(
            porcelain.trim().is_empty(),
            "fixture premise: `git {}` must report the tree clean, got:\n{porcelain}",
            args.join(" ")
        );
    }
    let refusal = Command::new("git")
        .args(["worktree", "remove", worktree.to_str().unwrap()])
        .current_dir(&project_root)
        .output()
        .expect("git worktree remove");
    assert!(
        !refusal.status.success()
            && String::from_utf8_lossy(&refusal.stderr).contains("submodules"),
        "fixture premise: git itself must refuse this removal, got status={} stderr={}",
        refusal.status,
        String::from_utf8_lossy(&refusal.stderr)
    );

    let running = boot(home.clone()).await;
    let stdout = run_orgasmic(
        &home,
        &running,
        &project_root,
        &path_env,
        &["manager", "worktree-prune"],
    );

    assert!(
        sentinel.is_file(),
        "the untracked file inside the submodule must SURVIVE the prune, got:\n{stdout}"
    );
    assert!(
        worktree.is_dir(),
        "the worktree itself must survive, got:\n{stdout}"
    );
    assert!(
        !stdout.contains(&format!("RECLAIMED PATH={}", worktree.display())),
        "the worktree must not be reclaimed, got:\n{stdout}"
    );
    let kept = stdout
        .lines()
        .find(|line| line.starts_with(&format!("KEPT PATH={}", worktree.display())))
        .unwrap_or_else(|| panic!("a KEPT line naming the worktree, got:\n{stdout}"));
    assert!(
        kept.contains("submodule") && kept.contains("vendor/sub"),
        "the report must say WHY it was kept and NAME the submodule, got:\n{kept}"
    );

    let _ = running.shutdown.send(());
    let _ = running.join.await;
}

/// TASK-RMA18.1.1 finding 2: the categorical submodule refusal sat BELOW the
/// `RepoGone` early return, so it guarded only the `Unclaimed` branch. The
/// `RepoGone` branch is the one that deletes with NO salvage at all, and it
/// reached `remove_child` without ever consulting the refusal — reachable with
/// the ORDINARY submodule shape, no exotic index required.
///
/// Same fixture as
/// `worktree_prune_refuses_a_worktree_containing_an_initialized_submodule`,
/// including its proof that git refuses the removal, plus one move: the linked
/// worktree's admin directory goes away, which is exactly what classifies the
/// worktree `RepoGone`. `--dry-run` pins that classification before the
/// destructive run, so a green here cannot come from the `Unclaimed` path.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn worktree_prune_refuses_a_repo_gone_worktree_containing_a_submodule() {
    let tmp = tempfile::tempdir().unwrap();
    let home = Home::at(tmp.path().join("home"));
    home.ensure().unwrap();
    let project_root = tmp.path().join("project");
    std::fs::create_dir_all(&project_root).unwrap();
    seed_project(&home, &project_root);
    init_git_project(&project_root);

    let sub_origin = tmp.path().join("subrepo");
    std::fs::create_dir_all(&sub_origin).unwrap();
    write(&sub_origin.join("lib.txt"), "library source");
    run_git(&sub_origin, &["init", "-b", "main"]);
    run_git(&sub_origin, &["config", "user.email", "tester@example.com"]);
    run_git(&sub_origin, &["config", "user.name", "Test User"]);
    run_git(&sub_origin, &["add", "."]);
    run_git(&sub_origin, &["commit", "-m", "sub init"]);

    run_git(
        &project_root,
        &[
            "-c",
            "protocol.file.allow=always",
            "submodule",
            "add",
            sub_origin.to_str().unwrap(),
            "vendor/sub",
        ],
    );
    run_git(&project_root, &["add", "-A"]);
    run_git(&project_root, &["commit", "-m", "add submodule"]);
    let head = run_git(&project_root, &["rev-parse", "HEAD"]);

    let bin_dir = tmp.path().join("bin");
    std::fs::create_dir_all(&bin_dir).unwrap();
    let path_env = path_with_stub(&bin_dir);

    let worktree = add_managed_worktree(
        &home,
        &project_root,
        "task-gonesub",
        "task-gonesub-impl",
        &head,
    );
    run_git(
        &worktree,
        &[
            "-c",
            "protocol.file.allow=always",
            "submodule",
            "update",
            "--init",
        ],
    );
    let sentinel = worktree.join("vendor/sub/SENTINEL.txt");
    write(&sentinel, "uncommitted worker output no salvage can reach");

    // git's own verdict, taken while the repository is still whole — this is
    // the refusal the verb must reproduce.
    let refusal = Command::new("git")
        .args(["worktree", "remove", worktree.to_str().unwrap()])
        .current_dir(&project_root)
        .output()
        .expect("git worktree remove");
    assert!(
        !refusal.status.success()
            && String::from_utf8_lossy(&refusal.stderr).contains("submodules"),
        "fixture premise: git itself must refuse this removal, got status={} stderr={}",
        refusal.status,
        String::from_utf8_lossy(&refusal.stderr)
    );

    // Now take the admin directory away. The submodule checkout and its
    // sentinel are untouched; only the classification changes.
    let admin = project_root.join(".git/worktrees/task-gonesub");
    std::fs::rename(&admin, admin.with_extension("moved")).unwrap();
    assert!(
        worktree.join(".git").is_file() && sentinel.is_file(),
        "fixture premise: the .git link and the sentinel must both still be there"
    );

    let running = boot(home.clone()).await;
    let planned = run_orgasmic(
        &home,
        &running,
        &project_root,
        &path_env,
        &["manager", "worktree-prune", "--dry-run"],
    );
    let would = planned
        .lines()
        .find(|line| line.starts_with(&format!("WOULD_RECLAIM PATH={}", worktree.display())))
        .unwrap_or_else(|| panic!("a WOULD_RECLAIM line naming the worktree, got:\n{planned}"));
    assert!(
        would.contains("repo gone"),
        "fixture premise: this must reach the RepoGone branch, not Unclaimed, got:\n{would}"
    );

    let stdout = run_orgasmic(
        &home,
        &running,
        &project_root,
        &path_env,
        &["manager", "worktree-prune"],
    );

    assert!(
        sentinel.is_file(),
        "the untracked file inside the submodule must SURVIVE the prune, got:\n{stdout}"
    );
    assert!(
        worktree.is_dir(),
        "the worktree itself must survive, got:\n{stdout}"
    );
    assert!(
        !stdout.contains(&format!("RECLAIMED PATH={}", worktree.display())),
        "the worktree must not be reclaimed, got:\n{stdout}"
    );
    let kept = stdout
        .lines()
        .find(|line| line.starts_with(&format!("KEPT PATH={}", worktree.display())))
        .unwrap_or_else(|| panic!("a KEPT line naming the worktree, got:\n{stdout}"));
    assert!(
        kept.contains("submodule") && kept.contains("vendor/sub"),
        "the report must say WHY it was kept and NAME the submodule, got:\n{kept}"
    );

    let _ = running.shutdown.send(());
    let _ = running.join.await;
}

/// TASK-RMA18.1.1.1 finding A: `RepoGone` + a gitlink recorded ONLY in the
/// index, with no `.gitmodules` anywhere — the intersection of the two
/// previous regressions, and the hole both of them left open.
///
/// On `RepoGone` all three of the shipped refusal's sources go quiet AT ONCE:
/// the admin directory is gone so it holds no `modules/`; the index lives in
/// that same gone directory so `worktree_index_gitlinks` answers
/// `NoRepository`, which is deliberately not a refusal; and there is no
/// `.gitmodules` to fall back to. The tree that was deleted holds a populated
/// independent repository with uncommitted work, and `RepoGone` is the ONE
/// branch that removes with no salvage at all.
///
/// The fixture takes git's verdict FIRST, while the repository is still whole,
/// so the refusal under test is git's own and not this suite's invention. Then
/// the admin directory goes away — nothing inside the worktree is touched — and
/// `--dry-run` pins that the classification really is `RepoGone`, so a green
/// here cannot come from the `Unclaimed` path. The sentinel SURVIVING is the
/// assertion.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn worktree_prune_refuses_a_repo_gone_worktree_holding_a_nested_repository() {
    let tmp = tempfile::tempdir().unwrap();
    let home = Home::at(tmp.path().join("home"));
    home.ensure().unwrap();
    let project_root = tmp.path().join("project");
    std::fs::create_dir_all(&project_root).unwrap();
    seed_project(&home, &project_root);
    init_git_project(&project_root);

    let sub_origin = tmp.path().join("subrepo");
    std::fs::create_dir_all(&sub_origin).unwrap();
    write(&sub_origin.join("lib.txt"), "library source");
    run_git(&sub_origin, &["init", "-b", "main"]);
    run_git(&sub_origin, &["config", "user.email", "tester@example.com"]);
    run_git(&sub_origin, &["config", "user.name", "Test User"]);
    run_git(&sub_origin, &["add", "."]);
    run_git(&sub_origin, &["commit", "-m", "sub init"]);
    let sub_head = run_git(&sub_origin, &["rev-parse", "HEAD"]);

    // A mode-160000 index entry and no other record of the submodule anywhere.
    run_git(
        &project_root,
        &[
            "update-index",
            "--add",
            "--cacheinfo",
            &format!("160000,{sub_head},vendor/sub"),
        ],
    );
    run_git(&project_root, &["commit", "-m", "gitlink, no .gitmodules"]);
    let head = run_git(&project_root, &["rev-parse", "HEAD"]);

    let bin_dir = tmp.path().join("bin");
    std::fs::create_dir_all(&bin_dir).unwrap();
    let path_env = path_with_stub(&bin_dir);

    let worktree = add_managed_worktree(
        &home,
        &project_root,
        "task-goneindexsub",
        "task-goneindexsub-impl",
        &head,
    );
    // The worker clones the dependency it needs itself: an ordinary standalone
    // repository, never registered as a submodule, so nothing writes
    // `.gitmodules` and nothing creates an admin `modules/` directory.
    let checkout = worktree.join("vendor/sub");
    let _ = std::fs::remove_dir(&checkout);
    run_git(
        &worktree,
        &[
            "clone",
            sub_origin.to_str().unwrap(),
            checkout.to_str().unwrap(),
        ],
    );
    write(&checkout.join(".git/info/exclude"), "SENTINEL.txt\n");
    let sentinel = checkout.join("SENTINEL.txt");
    write(&sentinel, "uncommitted worker output no salvage can reach");

    // Fixture premises, MEASURED: neither of the two record-reading branches
    // has anything to fire on once the repository is gone.
    assert!(
        !worktree.join(".gitmodules").exists(),
        "fixture premise: there must be no .gitmodules for the fallback to read"
    );
    let admin = project_root.join(".git/worktrees/task-goneindexsub");
    assert!(
        !admin.join("modules").exists(),
        "fixture premise: the worktree admin directory must hold no `modules` directory"
    );
    assert_eq!(
        run_git(&worktree, &["ls-files", "-s", "vendor/sub"])
            .split_whitespace()
            .next()
            .unwrap_or_default(),
        "160000",
        "fixture premise: the index entry must be a gitlink"
    );
    for args in [
        ["status", "--porcelain"].as_slice(),
        ["status", "--porcelain", "--ignore-submodules=none"].as_slice(),
    ] {
        let porcelain = run_git(&worktree, args);
        assert!(
            porcelain.trim().is_empty(),
            "fixture premise: `git {}` must report the tree clean, got:\n{porcelain}",
            args.join(" ")
        );
    }
    let refusal = Command::new("git")
        .args(["worktree", "remove", worktree.to_str().unwrap()])
        .current_dir(&project_root)
        .output()
        .expect("git worktree remove");
    assert!(
        !refusal.status.success()
            && String::from_utf8_lossy(&refusal.stderr).contains("submodules"),
        "fixture premise: git itself must refuse this removal while the repository is still \
         there, got status={} stderr={}",
        refusal.status,
        String::from_utf8_lossy(&refusal.stderr)
    );

    // Now take the admin directory away. The nested checkout and its sentinel
    // are untouched; only the classification changes — and with it the index
    // this verb reads, which lived in the directory that just went away.
    std::fs::rename(&admin, admin.with_extension("moved")).unwrap();
    assert!(
        worktree.join(".git").is_file() && sentinel.is_file(),
        "fixture premise: the .git link and the sentinel must both still be there"
    );

    let running = boot(home.clone()).await;
    let planned = run_orgasmic(
        &home,
        &running,
        &project_root,
        &path_env,
        &["manager", "worktree-prune", "--dry-run"],
    );
    let would = planned
        .lines()
        .find(|line| line.starts_with(&format!("WOULD_RECLAIM PATH={}", worktree.display())))
        .unwrap_or_else(|| panic!("a WOULD_RECLAIM line naming the worktree, got:\n{planned}"));
    assert!(
        would.contains("repo gone"),
        "fixture premise: this must reach the RepoGone branch, not Unclaimed, got:\n{would}"
    );

    let stdout = run_orgasmic(
        &home,
        &running,
        &project_root,
        &path_env,
        &["manager", "worktree-prune"],
    );

    assert!(
        sentinel.is_file(),
        "the untracked file inside the nested repository must SURVIVE the prune, got:\n{stdout}"
    );
    assert!(
        worktree.is_dir(),
        "the worktree itself must survive, got:\n{stdout}"
    );
    assert!(
        !stdout.contains(&format!("RECLAIMED PATH={}", worktree.display())),
        "the worktree must not be reclaimed, got:\n{stdout}"
    );
    let kept = stdout
        .lines()
        .find(|line| line.starts_with(&format!("KEPT PATH={}", worktree.display())))
        .unwrap_or_else(|| panic!("a KEPT line naming the worktree, got:\n{stdout}"));
    assert!(
        kept.contains("vendor/sub/.git"),
        "the report must NAME the nested repository it refused over, got:\n{kept}"
    );
    // The shared advice string used to offer `git worktree remove --force` on
    // every refusal, and that escape CANNOT run once the repository is gone —
    // nor does this verb have a `--force` of its own (TASK-RMA18.1.1.1, the
    // reviewer's second correction to the C1 ruling). Naming the flag in order
    // to say it is unavailable is right; offering it as the remedy is the
    // defect, so the remedy must be one the operator can actually perform.
    assert!(
        !kept.contains("or remove the worktree with `git worktree remove --force`"),
        "the remedy offered here is impossible: the repository `git worktree remove` would run \
         against is the one that is gone, got:\n{kept}"
    );
    assert!(
        kept.contains("CANNOT run") && kept.contains("delete vendor/sub/.git yourself"),
        "the refusal must say the --force escape is unavailable and name what the operator \
         must clear by hand, got:\n{kept}"
    );
    assert!(
        !stdout.contains(&format!("SALVAGED PATH={}", worktree.display())),
        "nothing was salvageable, so nothing must claim to have been salvaged, got:\n{stdout}"
    );

    let _ = running.shutdown.send(());
    let _ = running.join.await;
}

/// TASK-RMA18.1.1.1.1 finding 3: `RepoGone` + `.gitmodules` and NO nested
/// `.git` — the one shape on this branch that NOTHING covered, which is how
/// finding 2 shipped.
///
/// `worktree_prune_refuses_a_repo_gone_worktree_containing_a_submodule` was
/// written to exercise this arm and stopped doing so: `git submodule update
/// --init` inside a LINKED worktree writes the submodule's `.git` as a FILE
/// (measured on git 2.52.0), the walk finds it, and the nested-`.git` arm
/// returns BEFORE `gitmodules_paths` is ever called. Its assertions matched the
/// new message verbatim, so nothing went red — a test passing for a different
/// reason than the one it was written for.
///
/// So this fixture removes exactly that entry and asserts the MECHANISM, not
/// merely that some refusal happened: with no nested `.git` anywhere, the ONLY
/// record left is `.gitmodules`, which lives in the WORKTREE rather than the
/// admin directory and therefore survives `RepoGone` intact. The `KEPT` line
/// must be the `.gitmodules` arm's and must NOT be the nested-`.git` arm's, so
/// this test fails if the wrong predicate refuses.
///
/// Git's own verdict is taken in the exact post-deletion state — it refuses the
/// removal over the INDEX gitlink, with the submodule's `.git` already gone —
/// so the refusal under test is git's own and not this suite's invention.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn worktree_prune_refuses_a_repo_gone_worktree_whose_gitmodules_names_a_populated_directory()
{
    let tmp = tempfile::tempdir().unwrap();
    let home = Home::at(tmp.path().join("home"));
    home.ensure().unwrap();
    let project_root = tmp.path().join("project");
    std::fs::create_dir_all(&project_root).unwrap();
    seed_project(&home, &project_root);
    init_git_project(&project_root);

    let sub_origin = tmp.path().join("subrepo");
    std::fs::create_dir_all(&sub_origin).unwrap();
    write(&sub_origin.join("lib.txt"), "library source");
    run_git(&sub_origin, &["init", "-b", "main"]);
    run_git(&sub_origin, &["config", "user.email", "tester@example.com"]);
    run_git(&sub_origin, &["config", "user.name", "Test User"]);
    run_git(&sub_origin, &["add", "."]);
    run_git(&sub_origin, &["commit", "-m", "sub init"]);

    run_git(
        &project_root,
        &[
            "-c",
            "protocol.file.allow=always",
            "submodule",
            "add",
            sub_origin.to_str().unwrap(),
            "vendor/sub",
        ],
    );
    run_git(&project_root, &["add", "-A"]);
    run_git(&project_root, &["commit", "-m", "add submodule"]);
    let head = run_git(&project_root, &["rev-parse", "HEAD"]);

    let bin_dir = tmp.path().join("bin");
    std::fs::create_dir_all(&bin_dir).unwrap();
    let path_env = path_with_stub(&bin_dir);

    let worktree = add_managed_worktree(
        &home,
        &project_root,
        "task-gonemodules",
        "task-gonemodules-impl",
        &head,
    );
    run_git(
        &worktree,
        &[
            "-c",
            "protocol.file.allow=always",
            "submodule",
            "update",
            "--init",
        ],
    );
    let sentinel = worktree.join("vendor/sub/SENTINEL.txt");
    write(&sentinel, "uncommitted worker output no salvage can reach");
    // THE POINT OF THIS FIXTURE. Take the nested `.git` away, so the arm that
    // covered the previous fixture cannot fire and `.gitmodules` is the only
    // record left. The checkout stays populated, which is what makes it an
    // initialized submodule as far as this verb can tell.
    std::fs::remove_file(worktree.join("vendor/sub/.git")).unwrap();

    // Fixture premises, MEASURED rather than assumed — each one is a way this
    // test could silently stop covering the arm it exists for.
    assert!(
        worktree.join(".gitmodules").is_file(),
        "fixture premise: `.gitmodules` must survive inside the worktree — it is the only \
         submodule record this branch has left"
    );
    assert!(
        worktree.join("vendor/sub/lib.txt").is_file(),
        "fixture premise: the submodule checkout must stay populated"
    );
    fn nested_git_entries(dir: &Path, depth: u32, found: &mut Vec<PathBuf>) {
        let Ok(entries) = std::fs::read_dir(dir) else {
            return;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if depth > 1 && entry.file_name() == std::ffi::OsStr::new(".git") {
                found.push(path.clone());
            }
            if path.is_dir() && !path.is_symlink() {
                nested_git_entries(&path, depth + 1, found);
            }
        }
    }
    let mut nested = Vec::new();
    nested_git_entries(&worktree, 1, &mut nested);
    assert!(
        nested.is_empty(),
        "fixture premise: NO nested `.git` may remain, or the nested-`.git` arm answers instead \
         of the `.gitmodules` one and this test covers nothing new; found {nested:?}"
    );

    // git's own verdict, taken in this exact state (the repository is still
    // whole, the submodule's `.git` is already gone) — this is the refusal the
    // verb must reproduce.
    let refusal = Command::new("git")
        .args(["worktree", "remove", worktree.to_str().unwrap()])
        .current_dir(&project_root)
        .output()
        .expect("git worktree remove");
    assert!(
        !refusal.status.success()
            && String::from_utf8_lossy(&refusal.stderr).contains("submodules"),
        "fixture premise: git itself must refuse this removal, got status={} stderr={}",
        refusal.status,
        String::from_utf8_lossy(&refusal.stderr)
    );

    // Now take the admin directory away — the index this verb reads lived in
    // it, so `worktree_index_gitlinks` answers `NoRepository` and contributes no
    // candidate. Nothing inside the worktree is touched.
    let admin = project_root.join(".git/worktrees/task-gonemodules");
    std::fs::rename(&admin, admin.with_extension("moved")).unwrap();
    assert!(
        worktree.join(".git").is_file() && sentinel.is_file(),
        "fixture premise: the .git link and the sentinel must both still be there"
    );

    let running = boot(home.clone()).await;
    let planned = run_orgasmic(
        &home,
        &running,
        &project_root,
        &path_env,
        &["manager", "worktree-prune", "--dry-run"],
    );
    let would = planned
        .lines()
        .find(|line| line.starts_with(&format!("WOULD_RECLAIM PATH={}", worktree.display())))
        .unwrap_or_else(|| panic!("a WOULD_RECLAIM line naming the worktree, got:\n{planned}"));
    assert!(
        would.contains("repo gone"),
        "fixture premise: this must reach the RepoGone branch, not Unclaimed, got:\n{would}"
    );

    let stdout = run_orgasmic(
        &home,
        &running,
        &project_root,
        &path_env,
        &["manager", "worktree-prune"],
    );

    assert!(
        sentinel.is_file(),
        "the untracked file inside the submodule must SURVIVE the prune, got:\n{stdout}"
    );
    assert!(
        worktree.is_dir(),
        "the worktree itself must survive, got:\n{stdout}"
    );
    assert!(
        !stdout.contains(&format!("RECLAIMED PATH={}", worktree.display())),
        "the worktree must not be reclaimed, got:\n{stdout}"
    );
    let kept = stdout
        .lines()
        .find(|line| line.starts_with(&format!("KEPT PATH={}", worktree.display())))
        .unwrap_or_else(|| panic!("a KEPT line naming the worktree, got:\n{stdout}"));

    // WHICH ARM refused, not merely that one did. The nested-`.git` arm names
    // the entry it found; this one names the record it read. A future change
    // that makes the other arm answer here fails these two lines instead of
    // quietly passing, which is the defect this test was filed on.
    assert!(
        kept.contains("`.gitmodules`") && kept.contains("vendor/sub"),
        "the report must name the record it refused over (`.gitmodules`) and the submodule path, \
         got:\n{kept}"
    );
    assert!(
        !kept.contains("nested repository") && !kept.contains("vendor/sub/.git"),
        "this refusal must NOT be the nested-`.git` arm's — there is no nested `.git` here, and a \
         message that says otherwise means the fixture stopped covering the `.gitmodules` arm, \
         got:\n{kept}"
    );

    // The remedy must be one the operator can perform. `submodule_advice`'s
    // "while the repository is still there … `git worktree remove --force`" is
    // a conditional escape whose condition put the worktree on this branch in
    // the first place (TASK-RMA18.1.1.1.1 finding 2). Naming the flag in order
    // to say it is unavailable is right; offering it as the remedy is not.
    assert!(
        !kept.contains("while the repository is still there"),
        "the remedy offered here is impossible: the repository `git worktree remove` would run \
         against is the one that is gone, got:\n{kept}"
    );
    assert!(
        kept.contains("CANNOT run") && kept.contains("remove vendor/sub yourself"),
        "the refusal must say the --force escape is unavailable and name what the operator must \
         clear by hand, got:\n{kept}"
    );
    assert!(
        !stdout.contains(&format!("SALVAGED PATH={}", worktree.display())),
        "nothing was salvageable, so nothing must claim to have been salvaged, got:\n{stdout}"
    );

    let _ = running.shutdown.send(());
    let _ = running.join.await;
}

/// TASK-M47E5: a worktree an OPEN dispatch names is never reclaimed, whatever
/// its run health, and the refusal says why. Ending a dispatch is
/// `dispatch-close`'s authority, not this verb's.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn worktree_prune_refuses_a_worktree_an_open_dispatch_holds() {
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
    let brief = project_root.join(".orgasmic/tmp/dispatch/task-dispatch/task-dispatch-brief.md");
    write(&brief, "prune refusal brief");

    let running = boot(home.clone()).await;
    let dispatched = run_orgasmic(
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
            "--mode",
            "ws",
            "--harness",
            "codex",
            "--brief",
            brief.to_str().unwrap(),
            "--from",
            &head,
            "--reason",
            "prune refusal",
        ],
    );
    assert!(dispatched.contains("dispatched: TASK-DISPATCH implementer pid="));
    let worktree = home.root.join("worktrees/orgasmic/task-dispatch");
    assert!(worktree.is_dir(), "the dispatch must create the worktree");

    let stdout = run_orgasmic(
        &home,
        &running,
        &project_root,
        &path_env,
        &["manager", "worktree-prune"],
    );
    assert!(
        stdout.contains(&format!("SKIP PATH={}", worktree.display())),
        "prune must refuse a held worktree, got:\n{stdout}"
    );
    assert!(
        stdout.contains("is open for TASK-DISPATCH"),
        "the refusal must name the dispatch holding it, got:\n{stdout}"
    );
    assert!(
        stdout.contains("PRUNE_SUMMARY RECLAIMED=0"),
        "nothing may be reclaimed while a dispatch is open, got:\n{stdout}"
    );
    assert!(worktree.is_dir(), "the held worktree must survive");

    // The same fact reaches the automatic surface.
    let status = run_orgasmic(
        &home,
        &running,
        &project_root,
        &path_env,
        &["manager", "dispatch-status"],
    );
    assert!(
        status.contains(&format!("HELD_WORKTREE PATH={}", worktree.display())),
        "dispatch-status must report the held worktree, got:\n{status}"
    );
    assert!(
        !status.contains("RECLAIMABLE_WORKTREE"),
        "a held worktree must never be advertised as reclaimable, got:\n{status}"
    );

    let _ = running.shutdown.send(());
    let _ = running.join.await;
}

/// TASK-M47E5 acceptance: a worktree whose REPO is gone is detected and
/// removable. This case did not exist before the move — worktrees used to die
/// with their repo — and `git worktree remove` cannot help, because there is no
/// repository to run it from.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn worktree_prune_removes_a_worktree_whose_repo_is_gone() {
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

    let worktree =
        add_managed_worktree(&home, &project_root, "task-abort", "task-abort-impl", &head);
    // Destroy the admin directory the worktree's `.git` link points at.
    std::fs::remove_dir_all(project_root.join(".git/worktrees/task-abort")).unwrap();
    assert!(
        worktree.join(".git").is_file(),
        "the .git link must still be there — that is what makes it detectable"
    );

    let running = boot(home.clone()).await;
    let stdout = run_orgasmic(
        &home,
        &running,
        &project_root,
        &path_env,
        &["manager", "worktree-prune"],
    );
    assert!(
        stdout.contains(&format!("RECLAIMED PATH={}", worktree.display())),
        "a repo-gone worktree must be removable, got:\n{stdout}"
    );
    assert!(!worktree.exists(), "the orphaned worktree must be gone");

    let _ = running.shutdown.send(());
    let _ = running.join.await;
}

/// TASK-M47E5: DETECTION is automatic even though reclamation is not — the
/// move puts worktrees where `git status` no longer shows them, so the
/// inventory verb has to. And `git worktree prune` runs, so `.git/worktrees`
/// metadata for an out-of-band removal does not accumulate.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn dispatch_status_detects_reclaimable_worktrees_and_prune_clears_stale_metadata() {
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

    let stale = add_managed_worktree(
        &home,
        &project_root,
        "task-cleanup",
        "task-cleanup-impl",
        &head,
    );

    let running = boot(home.clone()).await;
    let status = run_orgasmic(
        &home,
        &running,
        &project_root,
        &path_env,
        &["manager", "dispatch-status"],
    );
    assert!(
        status.contains(&format!("RECLAIMABLE_WORKTREE PATH={}", stale.display())),
        "dispatch-status must surface a worktree nothing owns, got:\n{status}"
    );
    assert!(
        status.contains("WHY=no open dispatch names it"),
        "detection must say WHY it is reclaimable, got:\n{status}"
    );
    assert!(
        status.contains("RECLAIMABLE_TOTAL COUNT=1"),
        "detection must total the bytes at stake, got:\n{status}"
    );
    assert!(
        status.contains("RECLAIM_WITH=orgasmic manager worktree-prune"),
        "detection must name the verb that reclaims, got:\n{status}"
    );

    // Now remove the directory out of band, exactly as an operator clearing
    // `~/.orgasmic` would, leaving only stale git metadata behind.
    std::fs::remove_dir_all(&stale).unwrap();
    let listed = run_git(&project_root, &["worktree", "list", "--porcelain"]);
    assert!(
        listed.contains("task-cleanup"),
        "git must still be carrying the stale admin entry: {listed}"
    );

    let pruned = run_orgasmic(
        &home,
        &running,
        &project_root,
        &path_env,
        &["manager", "worktree-prune"],
    );
    assert!(
        pruned.contains("PRUNED_METADATA"),
        "stale .git/worktrees metadata must be pruned, got:\n{pruned}"
    );
    let listed = run_git(&project_root, &["worktree", "list", "--porcelain"]);
    assert!(
        !listed.contains("task-cleanup"),
        "the stale admin entry must be gone: {listed}"
    );

    let _ = running.shutdown.send(());
    let _ = running.join.await;
}

// ===== TASK-M47E5.2: prune must refuse what it cannot prove is safe =======

/// Strip one org property line from the single tx file, leaving the entry
/// otherwise intact. This is the "torn record" the reviewer named: an open
/// dispatch whose `WORKTREE` never reached the ledger, or reached it and was
/// lost. It is the reason the tx ledger cannot be the ownership authority — a
/// live worker is still in that directory whatever the ledger says.
fn tear_tx_property(project_root: &Path, key: &str) {
    let path = tx_file_path(project_root);
    let raw = std::fs::read_to_string(&path).unwrap();
    let prefix = format!(":{key}:");
    let torn: Vec<&str> = raw
        .lines()
        .filter(|line| !line.trim_start().starts_with(prefix.as_str()))
        .collect();
    assert!(
        torn.len() < raw.lines().count(),
        "there was no :{key}: property to tear out of the tx log:\n{raw}"
    );
    std::fs::write(&path, format!("{}\n", torn.join("\n"))).unwrap();
}

/// TASK-M47E5.2: anchoring the root must not turn an ABSENT root into an early
/// exit. That root not existing is exactly the state an operator who `rm -rf`'d
/// `~/.orgasmic/worktrees` leaves behind, and it is the state in which stale
/// `.git/worktrees` admin entries most need clearing — the relocation half's
/// whole reason for running `git worktree prune`.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn worktree_prune_still_clears_stale_metadata_when_the_managed_root_is_gone() {
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

    add_managed_worktree(&home, &project_root, "task-swept", "task-swept-impl", &head);
    // The operator's `rm -rf`: the whole managed root, not just one worktree.
    std::fs::remove_dir_all(home.root.join("worktrees")).unwrap();
    let listed = run_git(&project_root, &["worktree", "list", "--porcelain"]);
    assert!(
        listed.contains("task-swept"),
        "git must still be carrying the stale admin entry: {listed}"
    );

    let running = boot(home.clone()).await;
    let pruned = run_orgasmic(
        &home,
        &running,
        &project_root,
        &path_env,
        &["manager", "worktree-prune"],
    );
    assert!(
        pruned.contains("PRUNED_METADATA"),
        "an absent managed root must not skip the metadata prune, got:\n{pruned}"
    );
    let listed = run_git(&project_root, &["worktree", "list", "--porcelain"]);
    assert!(
        !listed.contains("task-swept"),
        "the stale admin entry must be gone: {listed}"
    );

    let _ = running.shutdown.send(());
    let _ = running.join.await;
}

/// TASK-M47E5.2 finding 1: a symlinked managed root redirected `remove_dir_all`
/// outside the root entirely.
///
/// `scan_managed_worktrees` called `std::fs::read_dir` on the managed root
/// without first proving that root was a real directory, and `read_dir` FOLLOWS
/// a symlink. Every child it then enumerated was a real directory belonging to
/// the victim. The direct-child fence in `remove_orphaned_worktree_dir` could
/// not catch it and could not fail: `path` is built as `<managed_root>/<child>`,
/// so `path.parent()` IS `managed_root`, and `normalize_path` canonicalizes both
/// sides through the same link. The final `symlink_metadata` saw a real
/// directory because it was one — it just belonged to somebody else.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn worktree_prune_refuses_a_symlinked_managed_root_and_every_sentinel_survives() {
    let tmp = tempfile::tempdir().unwrap();
    let home = Home::at(tmp.path().join("home"));
    home.ensure().unwrap();
    let project_root = tmp.path().join("project");
    std::fs::create_dir_all(&project_root).unwrap();
    seed_project(&home, &project_root);
    init_git_project(&project_root);
    let bin_dir = tmp.path().join("bin");
    std::fs::create_dir_all(&bin_dir).unwrap();
    let path_env = path_with_stub(&bin_dir);

    // The victim: real directories somewhere else entirely. They carry no
    // `.git`, which is what makes the unfixed classifier call them `RepoGone` —
    // the one disposition that skips salvage and calls `remove_dir_all`.
    let victim = tmp.path().join("victim");
    let sentinels = [
        victim.join("task-precious/keep-me.txt"),
        victim.join("task-precious/nested/deep/also-keep-me.txt"),
        victim.join("task-other/keep-me-too.txt"),
    ];
    for sentinel in &sentinels {
        write(sentinel, "sentinel");
    }

    // Point this project's managed root at the victim. The root is user-writable
    // and this codebase runs AI workers with filesystem access, so a same-user
    // replacement is not a hypothetical threat model.
    let managed_root = home.root.join("worktrees/orgasmic");
    std::fs::create_dir_all(managed_root.parent().unwrap()).unwrap();
    std::os::unix::fs::symlink(&victim, &managed_root).unwrap();

    let running = boot(home.clone()).await;
    let output = run_orgasmic_output(
        &home,
        &running,
        &project_root,
        &path_env,
        &["manager", "worktree-prune"],
    );
    let stdout = String::from_utf8_lossy(&output.stdout).to_string();
    let stderr = String::from_utf8_lossy(&output.stderr).to_string();

    for sentinel in &sentinels {
        assert!(
            sentinel.is_file(),
            "a symlinked managed root must never redirect removal outside it, but {} is gone\nstdout={stdout}\nstderr={stderr}",
            sentinel.display()
        );
    }
    assert!(
        !output.status.success(),
        "prune must refuse a managed root it cannot prove is a real directory\nstdout={stdout}\nstderr={stderr}"
    );
    assert!(
        format!("{stdout}{stderr}").contains("managed worktree root")
            && format!("{stdout}{stderr}").contains("symlink"),
        "the refusal must name the root and say it is a symlink\nstdout={stdout}\nstderr={stderr}"
    );

    // The same refusal reaches the automatic detection surface, rather than
    // dispatch-status quietly reporting the victim's directories as reclaimable.
    let status = run_orgasmic_output(
        &home,
        &running,
        &project_root,
        &path_env,
        &["manager", "dispatch-status"],
    );
    let status_all = format!(
        "{}{}",
        String::from_utf8_lossy(&status.stdout),
        String::from_utf8_lossy(&status.stderr)
    );
    assert!(
        !status_all.contains("RECLAIMABLE_WORKTREE"),
        "detection must never advertise a victim directory as reclaimable:\n{status_all}"
    );

    let _ = running.shutdown.send(());
    let _ = running.join.await;
}

/// TASK-M47E5.2 finding 3: "repo gone" failed OPEN on any I/O error, into the
/// only disposition that skips salvage and calls `remove_dir_all`.
///
/// `worktree_repo_gone` treated every `read_to_string(.git)` failure as evidence
/// the repository was gone: the `else` branch fell through to `(!dot_git.is_dir())`,
/// which is false for an unreadable regular file, yielding "no .git link". A
/// permission or transient I/O failure therefore selected `RepoGone`, and the
/// worker's uncommitted output went with it.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn worktree_prune_keeps_a_worktree_whose_git_link_is_unreadable_and_names_the_reason() {
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

    let worktree = add_managed_worktree(
        &home,
        &project_root,
        "task-unreadable",
        "task-unreadable-impl",
        &head,
    );
    // The thing the unsalvaged delete path destroys.
    write(
        &worktree.join("worker-output.txt"),
        "unmerged worker output",
    );
    let dot_git = worktree.join(".git");
    assert!(
        dot_git.is_file(),
        "a linked worktree's .git is a FILE holding `gitdir:`"
    );
    std::fs::set_permissions(&dot_git, std::fs::Permissions::from_mode(0o000)).unwrap();
    // If this passes, the process can read a mode-000 file (running as root)
    // and the test proves nothing. Fail loudly rather than pass silently.
    assert!(
        std::fs::read_to_string(&dot_git).is_err(),
        "the .git link must be genuinely unreadable for this regression to mean anything"
    );

    let running = boot(home.clone()).await;
    let output = run_orgasmic_output(
        &home,
        &running,
        &project_root,
        &path_env,
        &["manager", "worktree-prune"],
    );
    let stdout = String::from_utf8_lossy(&output.stdout).to_string();
    let stderr = String::from_utf8_lossy(&output.stderr).to_string();

    assert!(
        worktree.is_dir() && worktree.join("worker-output.txt").is_file(),
        "an I/O error on .git must never classify as repo-gone, which is the one path that deletes without salvaging\nstdout={stdout}\nstderr={stderr}"
    );
    assert!(
        stdout.contains(&format!("SKIP PATH={}", worktree.display())),
        "the worktree must be reported as kept, not silently ignored\nstdout={stdout}\nstderr={stderr}"
    );
    assert!(
        stdout.contains("repository state undetermined"),
        "the kept reason must say the repository state could not be determined\nstdout={stdout}"
    );
    assert!(
        stdout.contains("Permission denied") || stdout.contains("permission denied"),
        "the underlying I/O error must be printed, not swallowed\nstdout={stdout}"
    );
    assert!(
        stdout.contains("PRUNE_SUMMARY RECLAIMED=0"),
        "nothing may be reclaimed on an undetermined repository\nstdout={stdout}"
    );

    // Restore, so the tempdir teardown is not fighting a mode-000 file.
    std::fs::set_permissions(&dot_git, std::fs::Permissions::from_mode(0o644)).unwrap();
    let _ = running.shutdown.send(());
    let _ = running.join.await;
}

/// TASK-M47E5.2 finding 2, half one: a LIVE run whose open dispatch record is
/// torn is not reclaimed.
///
/// `scan_managed_worktrees` made the tx ledger the sole ownership decision and
/// used live-run data only to decorate a record it already held — and the CLI's
/// `RunSummary` deserialized `run_id` and nothing else, so it could not match a
/// live run to a worktree at all. Tear `WORKTREE` out of the open dispatch entry
/// and a worker that is still running in that directory classifies as UNCLAIMED.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn worktree_prune_refuses_a_live_run_whose_open_dispatch_record_is_torn() {
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
    let brief = project_root.join(".orgasmic/tmp/dispatch/task-dispatch/task-dispatch-brief.md");
    write(&brief, "torn ledger brief");

    let running = boot(home.clone()).await;
    let dispatched = run_orgasmic(
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
            "--mode",
            "ws",
            "--harness",
            "codex",
            "--brief",
            brief.to_str().unwrap(),
            "--from",
            &head,
            "--reason",
            "torn ledger",
        ],
    );
    assert!(dispatched.contains("dispatched: TASK-DISPATCH implementer pid="));
    let worktree = home.root.join("worktrees/orgasmic/task-dispatch");
    assert!(worktree.is_dir(), "the dispatch must create the worktree");
    write(
        &worktree.join("worker-output.txt"),
        "output of a worker that is still running",
    );
    let run_id = tx_property_for(
        &tx_log(&project_root),
        "run.created",
        "TASK-DISPATCH",
        "RUN_ID",
    );
    assert!(
        live_run_ids(&home, &running, &project_root, &path_env).contains(&run_id),
        "the worker must still be live, or this test proves nothing"
    );

    // The tear: the ledger no longer says which worktree this open dispatch
    // holds. The worker has not moved.
    tear_tx_property(&project_root, "WORKTREE");

    let output = run_orgasmic_output(
        &home,
        &running,
        &project_root,
        &path_env,
        &["manager", "worktree-prune"],
    );
    let stdout = String::from_utf8_lossy(&output.stdout).to_string();
    let stderr = String::from_utf8_lossy(&output.stderr).to_string();

    assert!(
        worktree.is_dir() && worktree.join("worker-output.txt").is_file(),
        "a live worker's worktree must never be reclaimed, whatever the ledger says\nstdout={stdout}\nstderr={stderr}"
    );
    assert!(
        stdout.contains(&format!("SKIP PATH={}", worktree.display())),
        "the refusal must name the worktree it left alone\nstdout={stdout}\nstderr={stderr}"
    );
    assert!(
        stdout.contains(&run_id),
        "the refusal must name the live run that owns it, which is the fact the ledger lost\nstdout={stdout}"
    );
    assert!(
        stdout.contains("PRUNE_SUMMARY RECLAIMED=0"),
        "nothing may be reclaimed while a live run occupies the root\nstdout={stdout}"
    );

    let _ = running.shutdown.send(());
    let _ = running.join.await;
}

/// TASK-M47E5.2 finding 2, half two: a run acquired IN THE
/// CLASSIFICATION-TO-REMOVAL GAP is not reclaimed either.
///
/// This is the interleaving no CLI-side audit can close, and it is the reason
/// the fix routes through `Supervisor::reserve_dispatch_close` rather than
/// widening the local file lock: `POST /runs/:origin/recover` acquires in
/// ANOTHER PROCESS. The rendezvous is deterministic, not a race — prune parks in
/// `worktree_prune_pause_after_guard` after the daemon has answered `reserved`
/// and before any filesystem mutation, which is exactly the window.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_recovery_acquired_in_the_prune_gap_is_refused_and_the_worktree_survives() {
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
    let brief = project_root.join(".orgasmic/tmp/dispatch/task-dispatch/task-dispatch-brief.md");
    write(&brief, "prune gap brief");

    let port = reserved_local_port();
    let running = boot_on_port(home.clone(), port).await;
    let dispatched = run_orgasmic(
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
            "--mode",
            "ws",
            "--harness",
            "codex",
            "--brief",
            brief.to_str().unwrap(),
            "--from",
            &head,
            "--reason",
            "prune gap",
        ],
    );
    assert!(dispatched.contains("dispatched: TASK-DISPATCH implementer pid="));
    let worktree = home.root.join("worktrees/orgasmic/task-dispatch");
    assert!(worktree.is_dir(), "the dispatch must create the worktree");
    let origin_run_id = tx_property_for(
        &tx_log(&project_root),
        "run.created",
        "TASK-DISPATCH",
        "RUN_ID",
    );

    // Interrupt the origin so nothing is live in the worktree at classification
    // time — the recovery below is the ONLY occupant, and it arrives late.
    let _ = running.shutdown.send(());
    let _ = running.join.await;
    let running = boot_on_port(home.clone(), port).await;
    assert!(
        !live_run_ids(&home, &running, &project_root, &path_env).contains(&origin_run_id),
        "the origin must be gone before prune, or prune never classifies this as reclaimable"
    );
    // And the ledger no longer names it, so nothing but the daemon reservation
    // stands between the recovery and this directory.
    tear_tx_property(&project_root, "WORKTREE");

    let pause = tmp.path().join("prune.pause");
    let reached = pause.with_extension("reached");
    std::fs::write(&pause, "hold").unwrap();

    let prune = {
        let home = home.clone();
        let daemon_url = format!("http://{}", running.addr);
        let project_root = project_root.clone();
        let path_env = path_env.clone();
        let pause = pause.clone();
        tokio::task::spawn_blocking(move || {
            run_orgasmic_output_with_daemon_url(
                &home,
                &daemon_url,
                &project_root,
                &path_env,
                &["manager", "worktree-prune"],
                &[(
                    "ORGASMIC_WORKTREE_PRUNE_PAUSE_FILE",
                    pause.to_str().unwrap(),
                )],
            )
        })
    };

    let deadline = Instant::now() + Duration::from_secs(120);
    while !reached.exists() {
        assert!(
            Instant::now() < deadline,
            "prune never reached its post-guard pause; it does not take a daemon reservation at all"
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert!(
        worktree.is_dir(),
        "the parked prune must not have removed anything yet"
    );

    // The other process, in the gap.
    let recover = run_orgasmic_output(
        &home,
        &running,
        &project_root,
        &path_env,
        &[
            "run",
            "recover",
            &origin_run_id,
            "--project",
            "orgasmic",
            "--action",
            "start_recovery_run",
            "--force-inert",
        ],
    );
    let recover_all = format!(
        "{}{}",
        String::from_utf8_lossy(&recover.stdout),
        String::from_utf8_lossy(&recover.stderr)
    );
    assert!(
        !recover.status.success(),
        "a recovery must not be admitted into a worktree prune has reserved:\n{recover_all}"
    );
    assert!(
        recover_all.contains("cleanup"),
        "the refusal must name the cleanup reservation:\n{recover_all}"
    );
    assert!(
        live_run_ids(&home, &running, &project_root, &path_env).is_empty(),
        "the refused recovery must not have left a live run in the worktree"
    );

    std::fs::remove_file(&pause).unwrap();
    let prune = prune.await.expect("worktree-prune task");
    let prune_all = format!(
        "{}{}",
        String::from_utf8_lossy(&prune.stdout),
        String::from_utf8_lossy(&prune.stderr)
    );
    assert!(
        prune.status.success(),
        "prune must complete once its reservation is released:\n{prune_all}"
    );
    assert!(
        !worktree.exists(),
        "prune held the worktree the whole time, so removal must have happened:\n{prune_all}"
    );

    let _ = running.shutdown.send(());
    let _ = running.join.await;
}

// ===== TASK-RMA18: fd-anchored reclamation, end to end ====================
//
// The TASK-M47E5.3 gate is gone and the nine behaviour tests above run again.
// What follows is what the gate was standing in for: the four regressions the
// redesign's acceptance names, each of which fails against the implementation
// the gate was protecting the machine from.

/// TASK-RMA18 finding 4: `O_NOFOLLOW` guards only the FINAL component.
///
/// The round-1 fix opened `<home>/worktrees/<project-id>` in one syscall, so the
/// kernel resolved `<home>/worktrees` by pathname and followed whatever it was.
/// The round-1 regression replaced only the final component and therefore could
/// not catch this: put the symlink one level UP and the handle anchors a victim
/// directory with every fd-relative guarantee below it intact and aimed at the
/// wrong tree.
///
/// The sentinels are the assertion that matters. They carry no `.git`, which is
/// what makes the classifier call them `RepoGone` — the one disposition that
/// skips salvage and deletes.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn worktree_prune_refuses_an_ancestor_symlink_and_every_sentinel_survives() {
    let tmp = tempfile::tempdir().unwrap();
    let home = Home::at(tmp.path().join("home"));
    home.ensure().unwrap();
    let project_root = tmp.path().join("project");
    std::fs::create_dir_all(&project_root).unwrap();
    seed_project(&home, &project_root);
    init_git_project(&project_root);
    let bin_dir = tmp.path().join("bin");
    std::fs::create_dir_all(&bin_dir).unwrap();
    let path_env = path_with_stub(&bin_dir);

    // The victim, laid out so that following the ancestor lands on a plausible
    // managed root: `<victim>/orgasmic/<worktree>`.
    let victim = tmp.path().join("victim");
    let sentinels = [
        victim.join("orgasmic/task-precious/keep-me.txt"),
        victim.join("orgasmic/task-precious/nested/deep/also-keep-me.txt"),
        victim.join("orgasmic/task-other/keep-me-too.txt"),
    ];
    for sentinel in &sentinels {
        write(sentinel, "sentinel");
    }

    // The ANCESTOR is the symlink. `<home>/worktrees/orgasmic` is never itself
    // a link, so a check on the final component alone sees nothing wrong.
    std::os::unix::fs::symlink(&victim, home.root.join("worktrees")).unwrap();
    assert!(
        !std::fs::symlink_metadata(home.root.join("worktrees/orgasmic"))
            .unwrap()
            .file_type()
            .is_symlink(),
        "the managed root itself must NOT be a symlink, or this is the round-1 case again"
    );

    let running = boot(home.clone()).await;
    let output = run_orgasmic_output(
        &home,
        &running,
        &project_root,
        &path_env,
        &["manager", "worktree-prune"],
    );
    let all = format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );

    for sentinel in &sentinels {
        assert!(
            sentinel.is_file(),
            "an ancestor symlink must never redirect the scan or the removal outside the root, \
             but {} is gone\n{all}",
            sentinel.display()
        );
    }
    assert!(
        !output.status.success(),
        "prune must refuse a root it reached through a symlinked ancestor\n{all}"
    );
    assert!(
        all.contains("managed worktree root") && all.contains("symlink"),
        "the refusal must name the root and say a component is a symlink\n{all}"
    );
    assert!(
        !all.contains("RECLAIMED") && !all.contains("WOULD_RECLAIM"),
        "nothing behind a followed ancestor may be reported as reclaimable\n{all}"
    );

    // The automatic detection surface refuses identically, rather than quietly
    // advertising the victim's directories as reclaimable.
    let status = run_orgasmic_output(
        &home,
        &running,
        &project_root,
        &path_env,
        &["manager", "dispatch-status"],
    );
    let status_all = format!(
        "{}{}",
        String::from_utf8_lossy(&status.stdout),
        String::from_utf8_lossy(&status.stderr)
    );
    assert!(
        !status_all.contains("RECLAIMABLE_WORKTREE"),
        "detection must never advertise a victim directory as reclaimable:\n{status_all}"
    );

    let _ = running.shutdown.send(());
    let _ = running.join.await;
}

/// TASK-RMA18 finding 5: the identity classified, reserved and deleted is the
/// same one, proved through the real binary.
///
/// The rendezvous is the deterministic version of the interleaving: prune parks
/// after the daemon has answered `reserved` and before any filesystem mutation,
/// and the substitution happens THERE. Everything about the worktree that the
/// path names has changed — a different directory answers to it — and the only
/// thing that has not is the inode the classification, the reservation and the
/// removal all refer to.
///
/// Against the implementation this replaces, the parked prune resumes and
/// deletes whatever the path now reaches, because classification rebuilt
/// `root.path().join(name)` and every later step re-resolved it.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_worktree_substituted_in_the_prune_gap_is_refused_and_the_impostor_survives() {
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

    let worktree = add_managed_worktree(
        &home,
        &project_root,
        "task-substituted",
        "task-substituted-impl",
        &head,
    );
    // Repo gone: the direct-removal path, so the substituted tree meets the ONE
    // disposition that deletes without salvaging — which is what makes the
    // sentinel below a real victim rather than a tree git would have refused.
    std::fs::remove_dir_all(project_root.join(".git/worktrees/task-substituted")).unwrap();

    // The tree that will answer to the same NAME once the swap happens. Its
    // sentinel is the whole assertion.
    let impostor = tmp.path().join("impostor");
    write(&impostor.join("nested/deep/sentinel.txt"), "sentinel");

    let running = boot(home.clone()).await;
    let pause = tmp.path().join("prune.pause");
    let reached = pause.with_extension("reached");
    std::fs::write(&pause, "hold").unwrap();

    let prune = {
        let home = home.clone();
        let daemon_url = format!("http://{}", running.addr);
        let project_root = project_root.clone();
        let path_env = path_env.clone();
        let pause = pause.clone();
        tokio::task::spawn_blocking(move || {
            run_orgasmic_output_with_daemon_url(
                &home,
                &daemon_url,
                &project_root,
                &path_env,
                &["manager", "worktree-prune"],
                &[(
                    "ORGASMIC_WORKTREE_PRUNE_PAUSE_FILE",
                    pause.to_str().unwrap(),
                )],
            )
        })
    };

    let deadline = Instant::now() + Duration::from_secs(120);
    while !reached.exists() {
        assert!(
            Instant::now() < deadline,
            "prune never reached its post-guard pause; it does not take a daemon reservation at all"
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }

    // The substitution, in the gap: the classified tree is moved aside and the
    // impostor takes its name.
    let moved_aside = tmp.path().join("moved-aside");
    std::fs::rename(&worktree, &moved_aside).unwrap();
    std::fs::rename(&impostor, &worktree).unwrap();

    std::fs::remove_file(&pause).unwrap();
    let prune = prune.await.expect("worktree-prune task");
    let all = format!(
        "{}{}",
        String::from_utf8_lossy(&prune.stdout),
        String::from_utf8_lossy(&prune.stderr)
    );

    assert!(
        worktree.join("nested/deep/sentinel.txt").is_file(),
        "the substituted directory must survive: the removal may only reach the inode that was \
         classified and reserved\n{all}"
    );
    assert!(
        !all.contains(&format!("RECLAIMED PATH={}", worktree.display())),
        "a substituted worktree must not be reported as reclaimed\n{all}"
    );
    assert!(
        all.contains("PRUNE_SUMMARY RECLAIMED=0"),
        "nothing may be reclaimed once the entry stopped naming what was reserved\n{all}"
    );

    let _ = running.shutdown.send(());
    let _ = running.join.await;
}

/// TASK-RMA18 finding 6: a dispatch whose `--worktree` names a NONCONFORMING
/// directory is never reclaimed.
///
/// The reservation's task-id is derived from the directory name, and a name
/// outside the `task-<slug>` scheme yields a task id that matches nothing. That
/// is the string the daemon's pending-admission check was keyed on, so for
/// exactly this shape the CLI's guess was the fence. The daemon now addresses a
/// pending admission by WORKTREE — see
/// `a_run_admitted_but_not_yet_recorded_blocks_a_close_that_cannot_name_its_task`
/// in the supervisor, which drives the admitted-not-yet-recorded window itself.
/// Here the same directory is proved unreclaimable end to end.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn worktree_prune_refuses_a_live_run_in_a_nonconforming_worktree_name() {
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
    let brief = project_root.join(".orgasmic/tmp/dispatch/task-dispatch/task-dispatch-brief.md");
    write(&brief, "nonconforming worktree brief");

    // Inside the managed root, so prune enumerates it, but named nothing like a
    // task: `worktree_reservation_task_id` can only echo it back.
    let worktree = home.root.join("worktrees/orgasmic/scratch-dir");
    std::fs::create_dir_all(worktree.parent().unwrap()).unwrap();

    let running = boot(home.clone()).await;
    let dispatched = run_orgasmic(
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
            "--mode",
            "ws",
            "--harness",
            "codex",
            "--brief",
            brief.to_str().unwrap(),
            "--from",
            &head,
            "--worktree",
            worktree.to_str().unwrap(),
            "--reason",
            "nonconforming worktree",
        ],
    );
    assert!(dispatched.contains("dispatched: TASK-DISPATCH implementer pid="));
    assert!(worktree.is_dir(), "the dispatch must create the worktree");
    write(
        &worktree.join("worker-output.txt"),
        "output of a worker that is still running",
    );
    let run_id = tx_property_for(
        &tx_log(&project_root),
        "run.created",
        "TASK-DISPATCH",
        "RUN_ID",
    );
    assert!(
        live_run_ids(&home, &running, &project_root, &path_env).contains(&run_id),
        "the worker must still be live, or this test proves nothing"
    );
    // The ledger no longer names the directory, so the reservation is the only
    // thing left between prune and a live worker's tree.
    tear_tx_property(&project_root, "WORKTREE");

    let output = run_orgasmic_output(
        &home,
        &running,
        &project_root,
        &path_env,
        &["manager", "worktree-prune"],
    );
    let all = format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        worktree.is_dir() && worktree.join("worker-output.txt").is_file(),
        "a live worker's worktree must never be reclaimed, whatever its directory is called\n{all}"
    );
    assert!(
        all.contains(&format!("SKIP PATH={}", worktree.display())),
        "the refusal must name the worktree it left alone\n{all}"
    );
    assert!(
        all.contains("PRUNE_SUMMARY RECLAIMED=0"),
        "nothing may be reclaimed while a live run occupies the root\n{all}"
    );

    let _ = running.shutdown.send(());
    let _ = running.join.await;
}

/// TASK-GRCWC: a mode-000 descendant used to make the preliminary walk silently
/// under-count the tree and then authorize deletion. The real verb must now
/// refuse during classification, before either an earlier or blocked sentinel
/// can be removed.
#[cfg(unix)]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn worktree_prune_refuses_an_unreadable_descendant_before_deletion() {
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

    let worktree = add_managed_worktree(
        &home,
        &project_root,
        "task-unreadable-walk",
        "task-unreadable-walk-impl",
        &head,
    );
    std::fs::remove_dir_all(project_root.join(".git/worktrees/task-unreadable-walk")).unwrap();
    let earlier = worktree.join("a-earlier-sentinel.txt");
    write(&earlier, "must survive because removal never starts");
    let blocked = worktree.join("z-unreadable");
    let nested = blocked.join("nested-sentinel.txt");
    write(&nested, "must survive");
    let _restore = PermissionRestore::new(&blocked, 0o755);
    std::fs::set_permissions(&blocked, std::fs::Permissions::from_mode(0o000)).unwrap();
    assert!(
        std::fs::File::open(&blocked).is_err(),
        "mode 000 must actually make the directory unreadable; a privileged test process \
         must fail this fixture rather than report a meaningless pass"
    );

    let running = boot(home.clone()).await;
    let output = run_orgasmic_output(
        &home,
        &running,
        &project_root,
        &path_env,
        &["manager", "worktree-prune"],
    );
    let all = format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    std::fs::set_permissions(&blocked, std::fs::Permissions::from_mode(0o755)).unwrap();

    assert!(
        output.status.success(),
        "a per-worktree safety refusal remains a reported skip, not a command crash\n{all}"
    );
    assert!(
        earlier.is_file() && nested.is_file(),
        "classification must refuse before deleting any content\n{all}"
    );
    assert!(
        all.contains(&format!("SKIP PATH={}", worktree.display()))
            && all.contains("worktree traversal incomplete")
            && all.contains("z-unreadable")
            && all.contains("Permission denied"),
        "the refusal must name the worktree, unreadable descendant, and full OS error chain\n{all}"
    );
    assert!(
        all.contains("the whole worktree was skipped and nothing within it was deleted"),
        "the refusal must guarantee that classification authorized no deletion anywhere in the \
         worktree\n{all}"
    );
    assert!(
        all.contains("make the offending descendant readable")
            && all.contains("chmod")
            && all.contains("remove it by hand, then re-run")
            && all.contains("no `--force` override"),
        "the refusal must give the safe remedies and say that no force escape exists\n{all}"
    );
    assert!(
        !all.contains(&format!("RECLAIMED PATH={}", worktree.display()))
            && !all.contains(&format!("PARTIAL PATH={}", worktree.display())),
        "an unsafe preliminary walk must authorize no deletion at all\n{all}"
    );
    assert!(
        all.contains("PRUNE_SUMMARY RECLAIMED=0") && all.contains("SKIPPED=1"),
        "the refusal must remain visible in the summary\n{all}"
    );

    let _ = running.shutdown.send(());
    let _ = running.join.await;
}

/// TASK-GRCWC: generate a real tree beyond the production bound. The sentinel
/// at the bottom and the one sorted before the chain prove the preliminary
/// refusal does not become destructive partial success.
#[cfg(unix)]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn worktree_prune_refuses_an_over_depth_tree_before_deletion() {
    const MAX_DEPTH_UNDER_TEST: usize = 64;

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

    let worktree = add_managed_worktree(
        &home,
        &project_root,
        "task-deep-walk",
        "task-deep-walk-impl",
        &head,
    );
    std::fs::remove_dir_all(project_root.join(".git/worktrees/task-deep-walk")).unwrap();
    let earlier = worktree.join("a-earlier-sentinel.txt");
    write(&earlier, "must survive because removal never starts");
    let mut deepest = worktree.join("z-deep");
    std::fs::create_dir(&deepest).unwrap();
    for _ in 0..=MAX_DEPTH_UNDER_TEST {
        deepest.push("d");
        std::fs::create_dir(&deepest).unwrap();
    }
    let nested = deepest.join("nested-sentinel.txt");
    write(&nested, "must survive");

    let running = boot(home.clone()).await;
    let output = run_orgasmic_output(
        &home,
        &running,
        &project_root,
        &path_env,
        &["manager", "worktree-prune"],
    );
    let all = format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );

    assert!(
        output.status.success(),
        "a per-worktree safety refusal remains a reported skip, not a command crash\n{all}"
    );
    assert!(
        earlier.is_file() && nested.is_file(),
        "classification must refuse before deleting any content\n{all}"
    );
    assert!(
        all.contains(&format!("SKIP PATH={}", worktree.display()))
            && all.contains("refusing to descend deeper than 64 directory levels"),
        "the refusal must name the production depth bound\n{all}"
    );
    assert!(
        !all.contains(&format!("RECLAIMED PATH={}", worktree.display()))
            && !all.contains(&format!("PARTIAL PATH={}", worktree.display())),
        "an unsafe preliminary walk must authorize no deletion at all\n{all}"
    );
    assert!(
        all.contains("PRUNE_SUMMARY RECLAIMED=0") && all.contains("SKIPPED=1"),
        "the refusal must remain visible in the summary\n{all}"
    );

    let _ = running.shutdown.send(());
    let _ = running.join.await;
}

/// TASK-RMA18: KEPT MEANS UNTOUCHED.
///
/// A removal that stops part-way has already destroyed files. Reporting that as
/// `KEPT` tells an operator the tree is intact when it is a ruin, and "kept"
/// is the word they act on. So a removal that touched anything reports
/// `PARTIAL`, and the two words now mean different things.
///
/// The fixture makes the removal fail deterministically at a known point: a
/// subdirectory whose write bit is off cannot have its entries unlinked, while
/// the siblings sorted before it are removed first.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_removal_that_stops_part_way_reports_partial_and_never_kept() {
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

    let worktree = add_managed_worktree(
        &home,
        &project_root,
        "task-partial",
        "task-partial-impl",
        &head,
    );
    // Repo gone: the direct-removal path, so no salvage or clean check stands
    // between the reservation and the removal this test is about.
    std::fs::remove_dir_all(project_root.join(".git/worktrees/task-partial")).unwrap();

    write(&worktree.join("a-first.txt"), "removed before the failure");
    let blocked = worktree.join("zz-blocked");
    write(&blocked.join("inner.txt"), "cannot be unlinked");
    let _restore = PermissionRestore::new(&blocked, 0o755);
    std::fs::set_permissions(&blocked, std::fs::Permissions::from_mode(0o500)).unwrap();
    assert!(
        std::fs::remove_file(blocked.join("inner.txt")).is_err(),
        "mode 0500 must actually prevent removal from this directory; a privileged test process \
         must fail this fixture rather than report a meaningless PARTIAL/KEPT pass"
    );

    let running = boot(home.clone()).await;
    let output = run_orgasmic_output(
        &home,
        &running,
        &project_root,
        &path_env,
        &["manager", "worktree-prune"],
    );
    let all = format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );

    assert!(
        !worktree.join("a-first.txt").exists(),
        "the fixture must actually reach a partial removal, or it proves nothing\n{all}"
    );
    assert!(
        all.contains(&format!("PARTIAL PATH={}", worktree.display())),
        "a removal that destroyed something must be reported as PARTIAL\n{all}"
    );
    assert!(
        !all.contains(&format!("KEPT PATH={}", worktree.display())),
        "KEPT must mean untouched, and this worktree was touched\n{all}"
    );
    assert!(
        all.contains("PRUNE_SUMMARY RECLAIMED=0")
            && all.contains("PARTIAL=1")
            && all.contains("KEPT=0"),
        "a partial removal remains a failed, incomplete removal and must not be collapsed into \
         RECLAIMED or KEPT in the summary\n{all}"
    );

    std::fs::set_permissions(&blocked, std::fs::Permissions::from_mode(0o755)).unwrap();
    let _ = running.shutdown.send(());
    let _ = running.join.await;
}

/// TASK-M47E5 hazard: worktrees created under the RETIRED in-project scheme are
/// still on disk when this lands, so closing one must keep working end to end —
/// salvage, non-forced removal, artifact prune. `--worktree` reproduces that
/// layout exactly, which is also the acceptance that the override still wins.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn dispatch_close_still_salvages_and_removes_an_old_path_worktree() {
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
    let brief = project_root.join(".orgasmic/tmp/dispatch/task-dispatch/task-dispatch-brief.md");
    write(&brief, "old path close brief");

    let old_path_worktree = project_root.join(".orgasmic/tmp/dispatch/task-dispatch/worktree");

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
            "--mode",
            "ws",
            "--harness",
            "codex",
            "--brief",
            brief.to_str().unwrap(),
            "--from",
            &head,
            "--worktree",
            old_path_worktree.to_str().unwrap(),
            "--reason",
            "old path close",
        ],
    );
    let started_tx = started_tx_from_dispatch_stdout(&dispatch_stdout);
    assert!(
        old_path_worktree.is_dir(),
        "--worktree must still override the managed default"
    );
    assert!(
        !home.root.join("worktrees/orgasmic/task-dispatch").exists(),
        "an explicit --worktree must not also create the managed default"
    );
    // Uncommitted worker output: the close must rescue it, not destroy it.
    write(
        &old_path_worktree.join("worker-output.txt"),
        "unmerged worker output",
    );

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
            "--started-tx",
            &started_tx,
            "--status",
            "done",
            "--merge-sha",
            &head,
            "--codex-commit",
            &head,
            "--reason",
            "old path close",
        ],
    );
    assert!(
        close_stdout.contains("cleanup: worktree salvaged sha="),
        "an old-path worktree must still be salvaged on close, got:\n{close_stdout}"
    );
    assert!(
        close_stdout.contains("worktree removed"),
        "an old-path worktree must still be removed on close, got:\n{close_stdout}"
    );
    assert!(!old_path_worktree.exists(), "the old-path worktree is gone");
    assert!(brief.is_file(), "the dispatch record survives the close");

    let _ = running.shutdown.send(());
    let _ = running.join.await;
}

/// A `git` shim that performs a same-OID sibling checkout the first time the
/// close reaches `git commit-tree`, and forwards everything to the real git.
///
/// TASK-QGWK7.1.1.1 F-2 needs the one interleaving the reviewer measured — HEAD
/// repointed to a DIFFERENT branch sitting at the SAME OID, between the close
/// resolving HEAD and the close swapping it. That window is milliseconds wide
/// in production and cannot be hit by a sleep; putting it on the real `git`
/// this close runs makes it deterministic, and puts it inside
/// `cmd_dispatch_close` rather than beside it.
fn write_same_oid_sibling_git_shim(bin_dir: &Path, sibling: &str) {
    let output = Command::new("sh")
        .args(["-c", "command -v git"])
        .output()
        .expect("locate git");
    assert!(output.status.success(), "command -v git failed");
    let git = String::from_utf8_lossy(&output.stdout).trim().to_string();
    let path = bin_dir.join("git");
    write(
        &path,
        format!(
            "#!/bin/sh\nfor arg in \"$@\"; do\n  if [ \"$arg\" = \"commit-tree\" ]; then\n    \
             marker=\"$({git} rev-parse --absolute-git-dir)/orgasmic-sibling-swapped\"\n    \
             if [ ! -e \"$marker\" ]; then\n      : > \"$marker\"\n      \
             {git} branch -f {sibling} HEAD\n      \
             {git} symbolic-ref HEAD refs/heads/{sibling}\n    fi\n  fi\ndone\nexec {git} \"$@\"\n"
        ),
    );
    #[cfg(unix)]
    {
        let mut perms = std::fs::metadata(&path).unwrap().permissions();
        perms.set_mode(0o755);
        std::fs::set_permissions(&path, perms).unwrap();
    }
}

/// TASK-QGWK7.1.1.1 F-1. A record persist that failed used to be terminal.
/// Promotion unlinks the tmp artifacts as soon as the COPIES succeed, so a
/// failed commit left the record on disk, untracked, with tmp gone: the
/// promote-in-place path bails on `last_path missing`, the re-run of the close
/// is an `already-closed` no-op by design, and there is no re-persist verb. The
/// only signal was one `warning:` line. The one path where the guarantee fails
/// was the one path with no repair.
///
/// The trigger is M-1's, one step later: an index lock this close does not own.
/// The close must still succeed (a failed persist never fails a close), keep the
/// promoted files, and — on the re-run every manager already performs — put them
/// into history without hand repair.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_record_persist_that_failed_is_recovered_by_re_running_the_close() {
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
    let brief = project_root.join(".orgasmic/tmp/dispatch/task-dispatch/task-dispatch-brief.md");
    write(&brief, "record recovery brief");

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
            "--mode",
            "ws",
            "--harness",
            "codex",
            "--brief",
            brief.to_str().unwrap(),
            "--from",
            &head,
            "--reason",
            "record recovery regression",
        ],
    );
    let started_tx = started_tx_from_dispatch_stdout(&dispatch_stdout);
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

    let close_args = [
        "manager",
        "dispatch-close",
        "--task",
        "TASK-DISPATCH",
        "--started-tx",
        &started_tx,
        "--status",
        "done",
        "--merge-sha",
        &head,
        "--codex-commit",
        &head,
        "--no-worktree-remove",
        "--reason",
        "record recovery regression",
    ];

    // The failure this finding is about: a lock this close does not own, held
    // across the persist.
    let index_lock = project_root.join(".git/index.lock");
    write(&index_lock, "");
    let first = run_orgasmic_output(&home, &running, &project_root, &path_env, &close_args);
    let first_stderr = String::from_utf8_lossy(&first.stderr).to_string();
    assert!(
        first.status.success(),
        "a failed record persist must never fail the close\nstdout={}\nstderr={first_stderr}",
        String::from_utf8_lossy(&first.stdout)
    );
    assert!(
        first_stderr.contains("warning: dispatch cleanup status=partial")
            && first_stderr.contains("commit promoted dispatch record"),
        "the failed persist must be reported through CLEANUP_ERROR: {first_stderr}"
    );
    let promoted_last =
        project_root.join(format!(".orgasmic/dispatch-records/{started_tx}/last.txt"));
    assert!(
        promoted_last.exists(),
        "the report itself is never the casualty of a failed persist"
    );
    let record_in_history = |root: &Path| {
        Command::new("git")
            .args([
                "cat-file",
                "-e",
                &format!("HEAD:.orgasmic/dispatch-records/{started_tx}/last.txt"),
            ])
            .current_dir(root)
            .status()
            .unwrap()
            .success()
    };
    assert!(
        !record_in_history(&project_root),
        "the fixture must really reproduce the unrecoverable state: record not in history"
    );
    assert!(
        !attempt_last.exists(),
        "tmp is already unlinked, which is why promote-in-place can no longer recover this"
    );

    // The manager clears the lock and re-runs the close, which is the only
    // command they have. Before this fix that re-run was a pure no-op.
    std::fs::remove_file(&index_lock).unwrap();
    let second = run_orgasmic(&home, &running, &project_root, &path_env, &close_args);
    assert!(
        second.contains("already-closed: TASK-DISPATCH"),
        "cleanup itself is still a no-op on a re-run: {second}"
    );
    assert!(
        second.contains(&format!(
            "re-persisted: dispatch record {started_tx} committed"
        )),
        "the re-run must repair the record it could not persist, and say so: {second}"
    );
    assert!(
        record_in_history(&project_root),
        "after the re-run a fresh clone must be able to read the record"
    );
    let staged = run_git(&project_root, &["diff", "--cached", "--name-only"]);
    assert!(
        staged.is_empty(),
        "a recovered persist must leave the index clean, or the next merge refuses: {staged}"
    );
    let log = run_git(&project_root, &["log", "-1", "--pretty=%s"]);
    assert_eq!(
        log,
        format!("chore(orgasmic): dispatch record {started_tx}"),
        "the recovery writes the same record-only commit the close would have"
    );

    // ...and it does not keep repairing: a third run finds it already in
    // history and says nothing.
    let third = run_orgasmic(&home, &running, &project_root, &path_env, &close_args);
    assert!(
        !third.contains("re-persisted:"),
        "a record already in history must not be re-committed: {third}"
    );

    let _ = running.shutdown.send(());
    let _ = running.join.await;
}

/// TASK-QGWK7.1.1.1 F-2. `update-ref HEAD <new> <old>` is a real
/// compare-and-swap on the branch through the symref — but it compares OIDs,
/// not ref IDENTITY. With a sibling branch at the same OID, a checkout landing
/// between the close resolving HEAD and the close swapping it makes the check
/// PASS and puts the record commit on a branch the close never resolved.
///
/// The fix resolves `git symbolic-ref -q HEAD` alongside the OID and swaps THAT
/// ref, so the record lands on the branch the close read, or nowhere.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_record_commit_cannot_land_on_a_branch_the_close_never_resolved() {
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
    let brief = project_root.join(".orgasmic/tmp/dispatch/task-dispatch/task-dispatch-brief.md");
    write(&brief, "sibling branch brief");

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
            "--mode",
            "ws",
            "--harness",
            "codex",
            "--brief",
            brief.to_str().unwrap(),
            "--from",
            &head,
            "--reason",
            "sibling branch regression",
        ],
    );
    let started_tx = started_tx_from_dispatch_stdout(&dispatch_stdout);
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

    let main_before = run_git(&project_root, &["rev-parse", "refs/heads/main"]);
    // Arm the interleaving only now, so the dispatch above ran against real git.
    write_same_oid_sibling_git_shim(&bin_dir, "sibling");

    let close = run_orgasmic_output(
        &home,
        &running,
        &project_root,
        &path_env,
        &[
            "manager",
            "dispatch-close",
            "--task",
            "TASK-DISPATCH",
            "--started-tx",
            &started_tx,
            "--status",
            "done",
            "--merge-sha",
            &head,
            "--codex-commit",
            &head,
            "--no-worktree-remove",
            "--reason",
            "sibling branch regression",
        ],
    );
    assert!(
        close.status.success(),
        "the close still succeeds\nstdout={}\nstderr={}",
        String::from_utf8_lossy(&close.stdout),
        String::from_utf8_lossy(&close.stderr)
    );
    assert_eq!(
        run_git(&project_root, &["rev-parse", "refs/heads/sibling"]),
        main_before,
        "the record must never land on a branch the close did not resolve"
    );
    assert_ne!(
        run_git(&project_root, &["rev-parse", "refs/heads/main"]),
        main_before,
        "the branch the close resolved is the one that advances"
    );
    assert_eq!(
        run_git(
            &project_root,
            &["log", "-1", "--pretty=%s", "refs/heads/main"]
        ),
        format!("chore(orgasmic): dispatch record {started_tx}"),
        "and it advances by exactly the record commit"
    );
    assert!(
        Command::new("git")
            .args([
                "cat-file",
                "-e",
                &format!("refs/heads/main:.orgasmic/dispatch-records/{started_tx}/last.txt"),
            ])
            .current_dir(&project_root)
            .status()
            .unwrap()
            .success(),
        "the record must be readable from the branch the close resolved"
    );

    let _ = running.shutdown.send(());
    let _ = running.join.await;
}

/// TASK-QGWK7.1.1.1 F-6. TASK-QGWK7.1.1's M-5 refusal was inserted ABOVE
/// `reconcile_torn_closes_best_effort`, whose own comment says it must run
/// "before anything else, including the already-closed no-op below". A torn
/// close is re-run with the SAME command line, so a command line carrying an
/// absolute `REPORT_PATH` bailed before the stranded transition was reconciled.
/// Refusing is still right; the ordering silently demoted a path that was
/// explicitly ordered first.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_torn_close_is_reconciled_before_an_absolute_report_path_is_refused() {
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
    let worktree = tmp.path().join("worktrees/task-cleanup");
    std::fs::create_dir_all(&worktree).unwrap();
    let brief = tmp.path().join("codex/task-cleanup-brief.md");
    write(&brief, "cleanup brief");
    seed_open_dispatch_tx(&project_root, "tx-start-order", &worktree, &brief);
    let outside_report = tmp.path().join("outside/last.txt");

    let running = boot(home.clone()).await;
    let proxy = start_lifecycle_rejecting_proxy(running.addr).await;
    let close_args = [
        "manager".to_string(),
        "dispatch-close".to_string(),
        "--task".to_string(),
        "TASK-CLEANUP".to_string(),
        "--started-tx".to_string(),
        "tx-start-order".to_string(),
        "--status".to_string(),
        "done".to_string(),
        "--merge-sha".to_string(),
        head.clone(),
        "--no-worktree-remove".to_string(),
        "--property".to_string(),
        format!("REPORT_PATH={}", outside_report.display()),
    ];
    let borrowed: Vec<&str> = close_args.iter().map(String::as_str).collect();
    let torn = run_orgasmic_output_with_daemon_url(
        &home,
        &format!("http://{}", proxy.addr),
        &project_root,
        &path_env,
        &borrowed,
        &[],
    );
    let torn_stderr = String::from_utf8_lossy(&torn.stderr).to_string();
    assert!(
        !torn.status.success(),
        "an absolute REPORT_PATH is refused, which is what makes the ordering matter\
         \nstdout={}\nstderr={torn_stderr}",
        String::from_utf8_lossy(&torn.stdout)
    );
    // Tear the close for real, with the property the manager can actually pass.
    let torn_close_args: Vec<&str> = borrowed[..borrowed.len() - 2].to_vec();
    let torn = run_orgasmic_output_with_daemon_url(
        &home,
        &format!("http://{}", proxy.addr),
        &project_root,
        &path_env,
        &torn_close_args,
        &[],
    );
    assert!(
        String::from_utf8_lossy(&torn.stderr)
            .contains("close tx appended but lifecycle update failed"),
        "the fixture must really tear the close: {}",
        String::from_utf8_lossy(&torn.stderr)
    );
    assert_task_stage(&project_root, "TASK-CLEANUP", "BACKLOG", "backlog");
    drop(proxy);

    // The re-run carries the same absolute REPORT_PATH the torn command line
    // had. It must still be refused — and the stranded transition must be
    // finished first, not left behind by the refusal.
    let rerun = run_orgasmic_output(&home, &running, &project_root, &path_env, &borrowed);
    let rerun_stdout = String::from_utf8_lossy(&rerun.stdout).to_string();
    let rerun_stderr = String::from_utf8_lossy(&rerun.stderr).to_string();
    assert!(
        !rerun.status.success() && rerun_stderr.contains("REPORT_PATH"),
        "the refusal still stands: stdout={rerun_stdout}\nstderr={rerun_stderr}"
    );
    assert!(
        rerun_stdout.contains("reconciled: TASK-CLEANUP backlog -> in_review"),
        "the torn transition must be reconciled BEFORE the refusal: stdout={rerun_stdout}\
         \nstderr={rerun_stderr}"
    );
    assert_task_stage(&project_root, "TASK-CLEANUP", "IN_REVIEW", "in_review");

    let _ = running.shutdown.send(());
    let _ = running.join.await;
}

/// A `git` shim that fails the named subcommands outright and forwards
/// everything else to the real git.
///
/// TASK-QGWK7.1.1.1.1 B-4 needs a record-commit failure that happens AFTER the
/// real-index `git add` at the top of `commit_promoted_dispatch_record`, which
/// is the only way to reach the rollback below it. Every commit-failure fixture
/// in this file injects with a held `.git/index.lock`, and that makes the `add`
/// itself fail — so the rollback was unreachable in the whole suite.
fn write_failing_git_shim(bin_dir: &Path, failing: &[&str]) {
    let output = Command::new("sh")
        .args(["-c", "command -v git"])
        .output()
        .expect("locate git");
    assert!(output.status.success(), "command -v git failed");
    let git = String::from_utf8_lossy(&output.stdout).trim().to_string();
    let path = bin_dir.join("git");
    let cases = failing.join("|");
    write(
        &path,
        format!(
            "#!/bin/sh\nfor arg in \"$@\"; do\n  case \"$arg\" in\n    {cases})\n      \
             echo \"fatal: injected $arg failure\" >&2\n      exit 1\n      ;;\n  esac\ndone\n\
             exec {git} \"$@\"\n"
        ),
    );
    #[cfg(unix)]
    {
        let mut perms = std::fs::metadata(&path).unwrap().permissions();
        perms.set_mode(0o755);
        std::fs::set_permissions(&path, perms).unwrap();
    }
}

/// TASK-QGWK7.1.1.1.1 B-4. TASK-QGWK7.1.1.1 F-1 made the record-commit rollback
/// REPORT itself instead of dropping the result behind `let _ =`, because a
/// silently left-staged record is M-0's symptom verbatim — the manager's next
/// merge refuses with no clue why. Nothing reached either arm: every
/// commit-failure fixture holds `.git/index.lock`, which makes the real-index
/// `git add` fail before `write_dispatch_record_commit` is ever called.
///
/// A shim that fails `update-ref` with the lock FREE gets past the `add` and
/// into the rollback. The first close takes the arm where the rollback works
/// (the index must come back clean); the re-run takes the arm where the
/// rollback fails too (the record really is left staged, and the message has to
/// say so and name the command that clears it).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_record_commit_rollback_reports_both_the_clean_and_the_failed_arm() {
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
    let brief = project_root.join(".orgasmic/tmp/dispatch/task-dispatch/task-dispatch-brief.md");
    write(&brief, "rollback brief");

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
            "--mode",
            "ws",
            "--harness",
            "codex",
            "--brief",
            brief.to_str().unwrap(),
            "--from",
            &head,
            "--reason",
            "rollback regression",
        ],
    );
    let started_tx = started_tx_from_dispatch_stdout(&dispatch_stdout);
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

    let close_args = [
        "manager",
        "dispatch-close",
        "--task",
        "TASK-DISPATCH",
        "--started-tx",
        &started_tx,
        "--status",
        "done",
        "--merge-sha",
        &head,
        "--codex-commit",
        &head,
        "--no-worktree-remove",
        "--reason",
        "rollback regression",
    ];
    let staged_record = |root: &Path| {
        run_git(
            root,
            &[
                "diff",
                "--cached",
                "--name-only",
                "--",
                ".orgasmic/dispatch-records",
            ],
        )
    };

    // Arm one: the commit fails at `update-ref`, and the rollback works.
    write_failing_git_shim(&bin_dir, &["update-ref"]);
    let first = run_orgasmic_output(&home, &running, &project_root, &path_env, &close_args);
    let first_stderr = String::from_utf8_lossy(&first.stderr).to_string();
    assert!(
        first.status.success(),
        "a failed record persist must never fail the close: {first_stderr}"
    );
    assert!(
        first_stderr.contains("injected update-ref failure"),
        "the fixture must really fail INSIDE the commit, past the real-index `git add`: \
         {first_stderr}"
    );
    assert!(
        !first_stderr.contains("left STAGED"),
        "the rollback succeeded here, so the close must not claim it did not: {first_stderr}"
    );
    assert_eq!(
        staged_record(&project_root),
        "",
        "the working arm of the rollback must really unstage the record, or the manager's \
         next merge refuses: {first_stderr}"
    );

    // Arm two: the rollback fails too, on the re-run that tries to repair.
    write_failing_git_shim(&bin_dir, &["update-ref", "restore"]);
    let second = run_orgasmic_output(&home, &running, &project_root, &path_env, &close_args);
    let second_stderr = String::from_utf8_lossy(&second.stderr).to_string();
    assert!(
        second.status.success(),
        "a failed re-persist must never fail the close either: {second_stderr}"
    );
    assert!(
        second_stderr.contains("the record is left STAGED because the rollback failed too")
            && second_stderr.contains("injected restore failure"),
        "the failed arm must name BOTH failures, not just the first: {second_stderr}"
    );
    assert!(
        second_stderr.contains("run `git restore --staged -- ")
            && second_stderr.contains(&format!(
                ".orgasmic/dispatch-records/{started_tx}` before your next"
            )),
        "and it must hand over the exact command that clears the state: {second_stderr}"
    );
    // The claim in that message has to be true, or it is the M-0 symptom with
    // a warning bolted on.
    std::fs::remove_file(bin_dir.join("git")).unwrap();
    assert!(
        staged_record(&project_root).contains(&started_tx),
        "the message says the record is left staged, so it had better be"
    );

    let _ = running.shutdown.send(());
    let _ = running.join.await;
}

/// TASK-QGWK7.1.1.1.1 B-2. The re-persist re-resolves `symbolic-ref -q HEAD` at
/// RE-RUN time, and the branch the failed close resolved is recorded nowhere.
/// Measured through the binary: close on `feature-x` with the index locked,
/// `git checkout main` (untracked files survive), re-run — the record lands on
/// `main`, `feature-x` never moves, and checking `feature-x` back out REMOVES
/// the now-tracked files from the working tree, so a further re-run there is a
/// silent no-op.
///
/// Refusing the mismatch would trade a record that IS in history for one that
/// is in none until the manager remembers which branch they were on, and no
/// consumer reads the record's home branch. So the behaviour stands, the
/// convention states it, the `re-persisted:` line names the ref — and this pins
/// all three, because an undocumented version of it is what made it a finding.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_re_persist_lands_on_the_branch_the_re_run_is_standing_on() {
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
    let brief = project_root.join(".orgasmic/tmp/dispatch/task-dispatch/task-dispatch-brief.md");
    write(&brief, "re-persist branch brief");

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
            "--mode",
            "ws",
            "--harness",
            "codex",
            "--brief",
            brief.to_str().unwrap(),
            "--from",
            &head,
            "--reason",
            "re-persist branch regression",
        ],
    );
    let started_tx = started_tx_from_dispatch_stdout(&dispatch_stdout);
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

    // The close runs while the manager stands on `feature-x`.
    run_git(&project_root, &["checkout", "-b", "feature-x"]);
    let close_args = [
        "manager",
        "dispatch-close",
        "--task",
        "TASK-DISPATCH",
        "--started-tx",
        &started_tx,
        "--status",
        "done",
        "--merge-sha",
        &head,
        "--codex-commit",
        &head,
        "--no-worktree-remove",
        "--reason",
        "re-persist branch regression",
    ];
    let index_lock = project_root.join(".git/index.lock");
    write(&index_lock, "");
    let first = run_orgasmic_output(&home, &running, &project_root, &path_env, &close_args);
    assert!(
        first.status.success(),
        "a failed record persist must never fail the close: {}",
        String::from_utf8_lossy(&first.stderr)
    );
    std::fs::remove_file(&index_lock).unwrap();
    let feature_before = run_git(&project_root, &["rev-parse", "refs/heads/feature-x"]);
    let main_before = run_git(&project_root, &["rev-parse", "refs/heads/main"]);

    // ...and the manager has moved on to `main` before re-running it.
    run_git(&project_root, &["checkout", "main"]);
    let promoted_last =
        project_root.join(format!(".orgasmic/dispatch-records/{started_tx}/last.txt"));
    assert!(
        promoted_last.exists(),
        "an untracked promoted record survives the checkout, which is what makes the re-run \
         on another branch possible at all"
    );
    let second = run_orgasmic(&home, &running, &project_root, &path_env, &close_args);
    assert!(
        second.contains(&format!(
            "re-persisted: dispatch record {started_tx} committed onto refs/heads/main"
        )),
        "the repair lands on the branch the RE-RUN resolved, and must name it: {second}"
    );
    assert_ne!(
        run_git(&project_root, &["rev-parse", "refs/heads/main"]),
        main_before,
        "`main` is the branch that advances by the record commit"
    );
    assert_eq!(
        run_git(&project_root, &["rev-parse", "refs/heads/feature-x"]),
        feature_before,
        "the branch the failed close resolved is NOT retro-fitted with the record"
    );

    // The tail of that trade, measured and worth stating: back on `feature-x`
    // the now-tracked record leaves the working tree, so the re-run there has
    // nothing on disk to repair and stays silent.
    run_git(&project_root, &["checkout", "feature-x"]);
    assert!(
        !promoted_last.exists(),
        "checking out a branch without the record commit removes the tracked files"
    );
    let third = run_orgasmic(&home, &running, &project_root, &path_env, &close_args);
    assert!(
        !third.contains("re-persisted:"),
        "and with nothing on disk the repair must stay a silent no-op: {third}"
    );

    let _ = running.shutdown.send(());
    let _ = running.join.await;
}

/// TASK-QGWK7.1.1.1.1 B-3. `Ok(())` from the record commit is not proof that a
/// commit happened. If `promote last.txt` fails after `create_dir_all`
/// succeeds, the destination directory exists and is EMPTY, and every step
/// downstream reports success over nothing (measured): `git add -- <empty dir>`
/// exits 0, the throwaway index's `write-tree` equals `head_tree` so the commit
/// takes its early return, and `verify_dispatch_record_staged` finds no file to
/// miss. The close then printed `re-persisted: dispatch record <tx> committed`
/// with nothing committed — and every future re-run repeated the same false
/// claim, because the record never entered `HEAD`.
///
/// The empty directory is produced here the short way, by emptying it after a
/// persist that really failed; the state it puts the repair in is identical.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_empty_promoted_record_directory_is_never_reported_as_committed() {
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
    let brief = project_root.join(".orgasmic/tmp/dispatch/task-dispatch/task-dispatch-brief.md");
    write(&brief, "empty record brief");

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
            "--mode",
            "ws",
            "--harness",
            "codex",
            "--brief",
            brief.to_str().unwrap(),
            "--from",
            &head,
            "--reason",
            "empty record regression",
        ],
    );
    let started_tx = started_tx_from_dispatch_stdout(&dispatch_stdout);
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

    let close_args = [
        "manager",
        "dispatch-close",
        "--task",
        "TASK-DISPATCH",
        "--started-tx",
        &started_tx,
        "--status",
        "done",
        "--merge-sha",
        &head,
        "--codex-commit",
        &head,
        "--no-worktree-remove",
        "--reason",
        "empty record regression",
    ];
    let index_lock = project_root.join(".git/index.lock");
    write(&index_lock, "");
    let first = run_orgasmic_output(&home, &running, &project_root, &path_env, &close_args);
    assert!(
        first.status.success(),
        "a failed record persist must never fail the close: {}",
        String::from_utf8_lossy(&first.stderr)
    );
    std::fs::remove_file(&index_lock).unwrap();

    // The torn-promote shape: the directory is there, the files are not.
    let dest_dir = project_root
        .join(".orgasmic/dispatch-records")
        .join(&started_tx);
    for entry in std::fs::read_dir(&dest_dir).unwrap() {
        std::fs::remove_file(entry.unwrap().path()).unwrap();
    }
    assert!(
        dest_dir.is_dir() && std::fs::read_dir(&dest_dir).unwrap().next().is_none(),
        "the fixture must be an EMPTY promoted directory, which is what git add accepts"
    );
    let head_before = run_git(&project_root, &["rev-parse", "HEAD"]);

    let second = run_orgasmic_output(&home, &running, &project_root, &path_env, &close_args);
    let second_stdout = String::from_utf8_lossy(&second.stdout).to_string();
    let second_stderr = String::from_utf8_lossy(&second.stderr).to_string();
    assert!(
        !second_stdout.contains("re-persisted:"),
        "a repair that committed nothing must not claim it committed: {second_stdout}"
    );
    assert!(
        second_stderr.contains("had nothing to commit and is still not in git history"),
        "and it must say what it found instead, once per re-run: {second_stderr}{second_stdout}"
    );
    assert_eq!(
        run_git(&project_root, &["rev-parse", "HEAD"]),
        head_before,
        "nothing was committed, so HEAD must not have moved"
    );

    let _ = running.shutdown.send(());
    let _ = running.join.await;
}
