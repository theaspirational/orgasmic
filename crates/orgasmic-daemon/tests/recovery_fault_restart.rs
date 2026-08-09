#![cfg(unix)]

use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use orgasmic_core::{
    project_sessions_dir, Home, Lifecycle, ReleaseOutcome, RuntimeIdentity, SessionEventKind,
    SessionWriter, TxEntry,
};
use orgasmic_daemon::recovery_claim::{load_recovery_claim, RecoveryClaim, RecoveryClaimStatus};
use orgasmic_daemon::{Daemon, DaemonOptions};
use orgasmic_drivers::modes::rmux::test_tooling::{
    assert_required_test_tooling, skip_test_if_missing, ToolRequirement,
};
use orgasmic_drivers::modes::tmux::{
    own_tmux_server_for_tests, real_tmux_on_path, tmux_command, TMUX_SOCKET_ENV,
};
use serde_json::{json, Value};

const PROJECT_ID: &str = "orgasmic";
const ORIGIN_RUN_ID: &str = "run-fault-origin";
const REQUEST_ID: &str = "task-hqr91-fault-replay";

fn write(path: impl AsRef<Path>, contents: &str) {
    let path = path.as_ref();
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).unwrap();
    }
    std::fs::write(path, contents).unwrap();
}

fn repo_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(Path::parent)
        .unwrap()
        .canonicalize()
        .unwrap()
}

fn seed_home_and_project(root: &Path) -> (Home, PathBuf) {
    let home = Home::at(root.join("home"));
    home.ensure().unwrap();
    std::os::unix::fs::symlink(repo_root(), home.source()).unwrap();

    let claude = home.bin().join("claude");
    write(&claude, "#!/bin/sh\nwhile :; do sleep 60; done\n");
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(&claude, std::fs::Permissions::from_mode(0o755)).unwrap();
    let wrapper = home.bin_orgasmic();
    write(
        &wrapper,
        "#!/bin/sh\n[ \"$1\" = __exec-pinned ] || exit 97\nshift\ntarget=$1\nshift\nshift 3\nexec \"$target\" \"$@\"\n",
    );
    std::fs::set_permissions(&wrapper, std::fs::Permissions::from_mode(0o755)).unwrap();

    let project_root = root.join("project");
    write(
        project_root.join(".orgasmic/project.org"),
        "#+title: orgasmic\n#+orgasmic_version: 1\n\n* PROJECT orgasmic\n:PROPERTIES:\n:ID:               orgasmic\n:END:\n",
    );
    write(
        project_root.join(".orgasmic/tasks/backlog.org"),
        "#+title: sprint\n#+orgasmic_version: 1\n\n* BACKLOG TASK-FAULT Recovery fault matrix :work:\n:PROPERTIES:\n:ID:               TASK-FAULT\n:END:\n",
    );
    write(
        home.board(),
        &format!(
            "#+title: board\n#+orgasmic_version: 1\n\n* PROJECT orgasmic\n:PROPERTIES:\n:ID:               orgasmic\n:PATH:             {}\n:BRANCH:           main\n:STATUS:           active\n:END:\n",
            project_root.display()
        ),
    );

    let session_path = project_sessions_dir(&project_root).join(format!("{ORIGIN_RUN_ID}.jsonl"));
    std::fs::create_dir_all(session_path.parent().unwrap()).unwrap();
    let identity = RuntimeIdentity {
        run_id: ORIGIN_RUN_ID.into(),
        runtime_id: "rt-fault-origin".into(),
        boot_id: "boot-fault-origin".into(),
    };
    let mut writer = SessionWriter::open(&session_path, identity).unwrap();
    writer
        .append(
            SessionEventKind::Lifecycle,
            serde_json::to_value(Lifecycle::Acquire {
                task_id: "TASK-FAULT".into(),
                kind: "worker".into(),
                worker_id: "implementer-claude-stream-json".into(),
            })
            .unwrap(),
        )
        .unwrap();
    writer
        .append(
            SessionEventKind::Lifecycle,
            serde_json::to_value(Lifecycle::RunMeta {
                transport: "tmux".into(),
                harness: Some("claude".into()),
                project_id: Some(PROJECT_ID.into()),
                worktree: Some(project_root.clone()),
                last_path: None,
                stdout_path: None,
                dispatch_attempt_token: None,
                role: Some("implementer".into()),
                requires_worker_finalize: Some(true),
                credential_mode: None,
                driver_config: json!({"harness": "claude"}),
            })
            .unwrap(),
        )
        .unwrap();
    writer
        .append(
            SessionEventKind::Lifecycle,
            serde_json::to_value(Lifecycle::Release {
                reason: "protocol_end_without_finalize".into(),
                outcome: ReleaseOutcome::Failed,
                finalized_by_worker: false,
            })
            .unwrap(),
        )
        .unwrap();

    // Keep parent_fsync aimed at the claim transaction and give cleanup a
    // forged stale temp to remove when that boundary is selected.
    std::fs::create_dir_all(home.state().join("recovery-claims").join(PROJECT_ID)).unwrap();
    (home, project_root)
}

/// How many `run.created ORIGIN=recovery` entries in the project tx ledger link
/// the origin run to `replacement_run_id` (TASK-6AYEJ.3).
///
/// This is the manager-visible half of a recovery: `dispatch-status` resolves a
/// worker's `*.reported` through this entry and nothing else, and
/// `dispatch-close` addresses the generation by the run id it carries. The rest
/// of this file asserts runtime identity and pane preservation, which stay green
/// whether or not the link is ever written.
fn recovery_association_count(project_root: &Path, replacement_run_id: &str) -> usize {
    let dir = project_root.join(".orgasmic/tx");
    let mut ledger = String::new();
    if let Ok(read) = std::fs::read_dir(&dir) {
        for entry in read.flatten() {
            if entry.path().extension().and_then(|ext| ext.to_str()) == Some("org") {
                ledger.push_str(&std::fs::read_to_string(entry.path()).unwrap());
            }
        }
    }
    ledger
        .split("\n* TX ")
        .filter(|block| {
            block.contains(":TYPE:         run.created")
                && block.contains(&format!(":ORIGIN_RUN_ID: {ORIGIN_RUN_ID}"))
                && block.contains(":ORIGIN:")
                && block.contains("recovery")
                && block.contains(replacement_run_id)
        })
        .count()
}

/// Give dispatch-wait a genuine root generation. Recovery itself never emits
/// this edge: it must append the replacement edge only after acquiring it.
fn seed_dispatch_generation(project_root: &Path) {
    let mut started = TxEntry::new(
        "dispatch-started",
        "manager.dispatch_started",
        "[2026-08-09 Sun 00:00:00]",
        "test",
        "recovery fault dispatch generation",
    );
    started.project = Some(PROJECT_ID.into());
    let mut root = TxEntry::new(
        "dispatch-root",
        "run.created",
        "[2026-08-09 Sun 00:00:00]",
        "test",
        "dispatch root",
    );
    root.project = Some(PROJECT_ID.into());
    root.extra = vec![
        ("ORIGIN".into(), "cli_dispatch".into()),
        ("DISPATCH_TX".into(), "dispatch-started".into()),
        ("RUN_ID".into(), ORIGIN_RUN_ID.into()),
    ];
    write(
        project_root.join(".orgasmic/tx/2026-08.org"),
        &format!("{}{}", started.render(), root.render()),
    );
}

async fn dispatch_wait(client: &reqwest::Client, addr: SocketAddr, home: &Home) -> Value {
    let response = client
        .post(format!("http://{addr}/api/manager/dispatch-wait"))
        .bearer_auth(token(home))
        .json(&json!({
            "project_id": PROJECT_ID,
            "started_tx": ["dispatch-started"],
        }))
        .send()
        .await
        .unwrap();
    let status = response.status();
    let body = response.text().await.unwrap_or_default();
    assert!(status.is_success(), "dispatch-wait status {status}: {body}");
    serde_json::from_str(&body).unwrap()
}

fn daemon_options() -> DaemonOptions {
    DaemonOptions {
        bind_override: Some("127.0.0.1".parse().unwrap()),
        port_override: Some(0),
        fs_watcher_enabled: false,
        trusted_exec_wrapper_override: std::env::var_os("ORGASMIC_RECOVERY_EXEC_WRAPPER")
            .map(PathBuf::from),
        ..DaemonOptions::default()
    }
}

fn token(home: &Home) -> String {
    std::fs::read_to_string(home.auth_token())
        .unwrap()
        .trim()
        .to_string()
}

// orgasmic:TASK-FJCE9
/// Is a *real* tmux usable here, and — once the answer is yes — the server this
/// process owns claimed?
///
/// Both halves are load-bearing, and this file learned each the hard way:
///
/// 1. *Strictness.* The former gate was `tmux -V`, and inside an orgasmic
///    worker rmux prepends a shim directory in which `tmux` is a symlink to
///    `rmux`; the shim answers `-V` and prints `tmux 3.4`. So this file's tmux
///    test ran in every worker suite, reported tmux, and executed rmux. The
///    rule is TASK-K4G1D's, reached here through `orgasmic_drivers` because an
///    integration-test crate cannot import the daemon's `#[cfg(test)]` copy;
///    `api::tests::daemon_and_driver_tmux_strictness_agree` keeps the two from
///    drifting and TASK-VJ633 collapses them.
/// 2. *Isolation* (TASK-0RCRY). Claimed here, before any session exists,
///    because this is the one gate every tmux-touching path below passes
///    through. Deliberately after the strictness check: claiming starts a
///    keepalive session, and claiming through the shim would start it on the
///    rmux server — the thing being prevented.
///
/// The claim also covers the *child daemons*, which are what actually create
/// the panes: [`spawn_daemon_child`] hands them the owned socket explicitly,
/// and they inherit this process's `PATH`, so the binary that passed the
/// strictness check above is the binary they resolve.
fn tmux_available() -> bool {
    if !real_tmux_on_path() {
        return false;
    }
    own_tmux_server_for_tests();
    true
}

#[test]
fn required_test_tooling_is_present() {
    assert_required_test_tooling(&[ToolRequirement::new("tmux", 1, tmux_available())]);
}

// orgasmic:TASK-FJCE9
/// Every tmux invocation in this file is built here.
///
/// One choke point, for the same reason `tmux_command` is one in the driver:
/// the `-L` selection is what keeps `kill-session` below off a server this test
/// did not create. Unpinned, it reached whatever server the environment
/// selected — inside a worker, `/private/tmp/rmux-501/default`, the rmux server
/// hosting live worker panes, which this file then ran `kill-session` against.
fn tmux(args: &[&str]) -> Command {
    let mut command = tmux_command();
    command.args(args);
    command
}

fn tmux_has_session_command(name: &str) -> Command {
    tmux(&["has-session", "-t", name])
}

fn tmux_pane_identity_command(name: &str) -> Command {
    tmux(&[
        "display-message",
        "-p",
        "-t",
        name,
        "#{session_id}:#{pane_id}:#{pane_pid}",
    ])
}

fn tmux_kill_session_command(name: &str) -> Command {
    tmux(&["kill-session", "-t", name])
}

fn tmux_has_session(name: &str) -> bool {
    tmux_has_session_command(name)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_ok_and(|status| status.success())
}

fn tmux_pane_identity(name: &str) -> String {
    let output = tmux_pane_identity_command(name).output().unwrap();
    assert!(output.status.success(), "planned pane {name} must be live");
    String::from_utf8(output.stdout).unwrap().trim().to_string()
}

fn kill_tmux(name: &str) {
    let _ = tmux_kill_session_command(name)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status();
}

// orgasmic:TASK-FJCE9
/// No invocation this file builds can reach a server it did not create.
///
/// Argv only: the assertion is on the command line, so it contacts no server
/// and cannot damage the thing it proves. That matters more here than in most
/// proofs — a behavioural replay of the defect *is* the defect, and the last
/// argv in the list is `kill-session`.
#[test]
fn every_tmux_invocation_is_pinned_to_an_owned_server() {
    // Publish a socket the way an operator would, without claiming one: a claim
    // starts a keepalive session, and this test must start nothing. If the
    // gated test already claimed, its label wins (the driver prefers the
    // in-process record) and the assertion below holds on that one instead.
    if std::env::var_os(TMUX_SOCKET_ENV).is_none() {
        std::env::set_var(TMUX_SOCKET_ENV, "orgasmic-test-fjce9-argv-proof");
    }

    for command in [
        tmux_has_session_command("orgasmic-run-proof"),
        tmux_pane_identity_command("orgasmic-run-proof"),
        tmux_kill_session_command("orgasmic-run-proof"),
    ] {
        let argv: Vec<String> = command
            .get_args()
            .map(|arg| arg.to_string_lossy().into_owned())
            .collect();
        let rendered = format!("tmux {}", argv.join(" "));
        assert_eq!(
            argv.first().map(String::as_str),
            Some("-L"),
            "unpinned tmux invocation `{rendered}`: without -L it reaches the server the \
             environment selects — the operator's own on a dev box, and inside an orgasmic \
             worker the rmux server hosting live worker panes"
        );
        assert!(
            argv.get(1).is_some_and(|socket| !socket.is_empty()),
            "`{rendered}` carries -L with no server label"
        );
    }
}

struct TmuxGuard(String);

impl Drop for TmuxGuard {
    fn drop(&mut self) {
        kill_tmux(&self.0);
    }
}

struct ChildDaemon {
    child: Child,
    addr: SocketAddr,
    log_path: PathBuf,
}

impl ChildDaemon {
    fn terminate(&mut self) {
        if self.child.try_wait().unwrap().is_none() {
            self.child.kill().unwrap();
        }
        let _ = self.child.wait();
    }

    fn diagnostics(&self) -> String {
        std::fs::read_to_string(&self.log_path).unwrap_or_default()
    }
}

impl Drop for ChildDaemon {
    fn drop(&mut self) {
        self.terminate();
    }
}

fn spawn_daemon_child(
    root: &Path,
    home: &Home,
    failpoint: Option<&str>,
    marker: Option<&Path>,
) -> ChildDaemon {
    let nonce = uuid::Uuid::new_v4().simple().to_string();
    let addr_path = root.join(format!("child-{nonce}.addr"));
    let log_path = root.join(format!("child-{nonce}.log"));
    let stdout = std::fs::File::create(&log_path).unwrap();
    let stderr = stdout.try_clone().unwrap();
    let mut command = Command::new(std::env::current_exe().unwrap());
    command
        .args([
            "--exact",
            "recovery_fault_child_daemon",
            "--nocapture",
            "--test-threads=1",
        ])
        .env("ORGASMIC_RECOVERY_CHILD", "1")
        .env("ORGASMIC_RECOVERY_CHILD_HOME", &home.root)
        .env("ORGASMIC_RECOVERY_CHILD_ADDR", &addr_path)
        .env("ORGASMIC_RECOVERY_EXEC_WRAPPER", home.bin_orgasmic())
        // orgasmic:TASK-FJCE9
        // The child daemon is what creates the pane, so the owned server has to
        // reach it too. Passed explicitly rather than left to inheritance: the
        // claim also exports this variable, but a spawn that depends on when
        // some other test happened to claim is exactly the kind of ordering the
        // default shared server already punished this file for.
        .env(TMUX_SOCKET_ENV, own_tmux_server_for_tests())
        .env(
            "PATH",
            format!(
                "{}:{}",
                home.bin().display(),
                std::env::var("PATH").unwrap_or_default()
            ),
        )
        .stdout(Stdio::from(stdout))
        .stderr(Stdio::from(stderr));
    if let Some(point) = failpoint {
        command.env("ORGASMIC_RECOVERY_FAILPOINT", point);
    } else {
        command.env_remove("ORGASMIC_RECOVERY_FAILPOINT");
    }
    if let Some(marker) = marker {
        command.env("ORGASMIC_RECOVERY_FAILPOINT_BLOCK_FILE", marker);
    } else {
        command.env_remove("ORGASMIC_RECOVERY_FAILPOINT_BLOCK_FILE");
    }
    let child = command.spawn().unwrap();
    let mut daemon = ChildDaemon {
        child,
        addr: "127.0.0.1:1".parse().unwrap(),
        log_path,
    };
    let deadline = Instant::now() + Duration::from_secs(20);
    loop {
        if let Ok(raw) = std::fs::read_to_string(&addr_path) {
            daemon.addr = raw.trim().parse().unwrap();
            return daemon;
        }
        if let Some(status) = daemon.child.try_wait().unwrap() {
            panic!(
                "child daemon exited before bind ({status}): {}",
                daemon.diagnostics()
            );
        }
        assert!(
            Instant::now() < deadline,
            "child daemon bind timeout: {}",
            daemon.diagnostics()
        );
        std::thread::sleep(Duration::from_millis(20));
    }
}

async fn wait_for_marker(daemon: &mut ChildDaemon, marker: &Path, point: &str) {
    let deadline = Instant::now() + Duration::from_secs(20);
    loop {
        if std::fs::read_to_string(marker).ok().as_deref() == Some(point) {
            return;
        }
        if let Some(status) = daemon.child.try_wait().unwrap() {
            panic!(
                "{point}: child exited before failpoint ({status}): {}",
                daemon.diagnostics()
            );
        }
        assert!(
            Instant::now() < deadline,
            "{point}: failpoint timeout: {}",
            daemon.diagnostics()
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

fn recovery_url(addr: SocketAddr) -> String {
    format!("http://{addr}/api/runs/{ORIGIN_RUN_ID}/recover")
}

fn recovery_body() -> Value {
    json!({
        "action": "start_recovery_run",
        "project": PROJECT_ID,
        "request_id": REQUEST_ID,
        "force_inert": false,
    })
}

async fn kill_child_at_boundary(
    root: &Path,
    home: &Home,
    point: &str,
) -> (Option<RecoveryClaim>, Option<String>) {
    let marker = root.join(format!("{point}.blocked"));
    let mut daemon = spawn_daemon_child(root, home, Some(point), Some(&marker));
    let client = reqwest::Client::new();
    let url = recovery_url(daemon.addr);
    let bearer = token(home);
    let body = recovery_body();
    let request = tokio::spawn(async move {
        client
            .post(url)
            .bearer_auth(bearer)
            .json(&body)
            .send()
            .await
    });
    wait_for_marker(&mut daemon, &marker, point).await;
    let planned = load_recovery_claim(home, PROJECT_ID, ORIGIN_RUN_ID).unwrap();
    let pane = planned.as_ref().and_then(|claim| {
        claim
            .planned_tmux_session
            .as_deref()
            .filter(|name| tmux_has_session(name))
            .map(tmux_pane_identity)
    });
    // This is the crash under test: SIGKILL the real daemon while its request
    // thread is blocked at the durable boundary. No Rust Drop or orderly
    // driver shutdown can destroy/recreate the original pane.
    daemon.terminate();
    let _ = tokio::time::timeout(Duration::from_secs(5), request).await;
    (planned, pane)
}

fn assert_complete_lifecycle(claim: &RecoveryClaim) {
    let envelopes = orgasmic_core::read_session_file(&claim.replacement_session_path).unwrap();
    let first = envelopes.first().expect("replacement lifecycle");
    assert_eq!(first.kind, SessionEventKind::Lifecycle);
    assert_eq!(
        first.event.get("phase").and_then(Value::as_str),
        Some("acquire"),
        "Acquire must be the first replacement envelope"
    );
    assert!(envelopes.iter().all(|envelope| {
        envelope.run_id == claim.replacement_run_id
            && envelope.runtime_id == claim.replacement_runtime_id
            && Some(envelope.boot_id.as_str()) == claim.boot_id.as_deref()
    }));
    let phases: Vec<_> = envelopes
        .iter()
        .filter(|envelope| envelope.kind == SessionEventKind::Lifecycle)
        .filter_map(|envelope| envelope.event.get("phase").and_then(Value::as_str))
        .collect();
    let required = [
        "acquire",
        "run_meta",
        "native_runtime",
        "prompt_draft",
        "recovery_origin",
    ];
    let positions: Vec<_> = required
        .iter()
        .map(|phase| {
            assert_eq!(
                phases.iter().filter(|actual| *actual == phase).count(),
                1,
                "{phase} must be durable exactly once: {phases:?}"
            );
            phases.iter().position(|actual| actual == phase).unwrap()
        })
        .collect();
    assert!(positions.windows(2).all(|pair| pair[0] < pair[1]));
}

async fn replay_live_original_pane(point: &str, runtime_launched: bool) {
    let tmp = tempfile::tempdir().unwrap();
    let (home, project_root) = seed_home_and_project(tmp.path());
    if point == "cleanup" {
        write(
            home.state()
                .join("recovery-claims")
                .join(PROJECT_ID)
                .join(format!("{ORIGIN_RUN_ID}.json.tmp.forged")),
            "not a recovery plan",
        );
    }
    let (planned, original_pane) = kill_child_at_boundary(tmp.path(), &home, point).await;
    let _original_guard = planned
        .as_ref()
        .and_then(|claim| claim.planned_tmux_session.clone())
        .map(TmuxGuard);
    if runtime_launched {
        let claim = planned
            .as_ref()
            .unwrap_or_else(|| panic!("{point}: spawn occurred without durable plan"));
        if matches!(point, "commit" | "response") {
            assert_eq!(claim.status, RecoveryClaimStatus::Committed);
        } else {
            assert_eq!(claim.status, RecoveryClaimStatus::Pending);
        }
        assert!(
            claim.spawn_started,
            "{point}: spawn authority was not durable"
        );
        assert!(
            original_pane.is_some(),
            "{point}: original pane was not live"
        );
    }

    let mut restarted = spawn_daemon_child(tmp.path(), &home, None, None);
    let client = reqwest::Client::new();
    let response = client
        .post(recovery_url(restarted.addr))
        .bearer_auth(token(&home))
        .json(&recovery_body())
        .send()
        .await
        .unwrap_or_else(|err| panic!("{point}: replay request failed: {err}"));
    let status = response.status();
    let raw_body = response.text().await.unwrap_or_default();
    assert!(
        status.is_success(),
        "{point}: replay status {status}: {raw_body} :: {}",
        restarted.diagnostics()
    );
    let response: Value = serde_json::from_str(&raw_body).unwrap();
    let committed = load_recovery_claim(&home, PROJECT_ID, ORIGIN_RUN_ID)
        .unwrap()
        .unwrap();
    let _committed_guard = TmuxGuard(committed.planned_tmux_session.clone().unwrap());
    assert_eq!(committed.status, RecoveryClaimStatus::Committed);
    assert_eq!(response["run_id"], committed.replacement_run_id);
    assert_eq!(response["runtime_id"], committed.replacement_runtime_id);
    assert_eq!(response["boot_id"], committed.boot_id.as_deref().unwrap());
    assert_eq!(
        response["session_path"],
        committed
            .replacement_session_path
            .to_string_lossy()
            .as_ref()
    );
    if let Some(original_plan) = planned {
        assert_eq!(
            committed.replacement_run_id,
            original_plan.replacement_run_id
        );
        assert_eq!(
            committed.replacement_runtime_id,
            original_plan.replacement_runtime_id
        );
        assert_eq!(committed.boot_id, original_plan.boot_id);
        assert_eq!(
            committed.replacement_session_path,
            original_plan.replacement_session_path
        );
    }
    assert_complete_lifecycle(&committed);
    assert_eq!(
        std::fs::read_dir(project_sessions_dir(&project_root))
            .unwrap()
            .filter_map(Result::ok)
            .filter(|entry| entry.path().extension().and_then(|ext| ext.to_str()) == Some("jsonl"))
            .count(),
        2,
        "{point}: origin plus exactly one replacement"
    );
    if let Some(original_pane) = original_pane {
        let name = committed.planned_tmux_session.as_deref().unwrap();
        assert_eq!(
            tmux_pane_identity(name),
            original_pane,
            "{point}: restart must retain the original pane, not recreate it"
        );
    }

    // TASK-6AYEJ.3: the replay returns a run id DIFFERENT from the origin at
    // every one of these boundaries, so the origin→replacement association is
    // the only thing keeping the dispatch generation resolvable. At and after
    // `acquire_append` the retry takes the reattach path, and that path used to
    // suppress the association — leaving the dispatch `[unreported]` forever and
    // its recorded id resolving to a 404 while the replacement ran on.
    assert_ne!(
        committed.replacement_run_id, ORIGIN_RUN_ID,
        "{point}: replacement must not reuse the origin id, or this asserts nothing"
    );
    assert_eq!(
        recovery_association_count(&project_root, &committed.replacement_run_id),
        1,
        "{point}: the ledger must carry exactly one origin→replacement \
         `run.created ORIGIN=recovery` for {}",
        committed.replacement_run_id
    );
    restarted.terminate();
}

async fn dead_pending_handle_fails_closed(point: &str) {
    let tmp = tempfile::tempdir().unwrap();
    let (home, project_root) = seed_home_and_project(tmp.path());
    let (planned, original_pane) = kill_child_at_boundary(tmp.path(), &home, point).await;
    let planned = planned.unwrap_or_else(|| panic!("{point}: missing durable pending plan"));
    assert_eq!(planned.status, RecoveryClaimStatus::Pending);
    assert!(
        planned.spawn_started,
        "{point}: spawn authority was not durable"
    );
    assert!(
        original_pane.is_some(),
        "{point}: pane was not live before forced death"
    );
    let name = planned.planned_tmux_session.as_deref().unwrap();
    kill_tmux(name);
    assert!(!tmux_has_session(name));

    let mut restarted = spawn_daemon_child(tmp.path(), &home, None, None);
    let response = reqwest::Client::new()
        .post(recovery_url(restarted.addr))
        .bearer_auth(token(&home))
        .json(&recovery_body())
        .send()
        .await
        .unwrap();
    assert!(
        !response.status().is_success(),
        "{point}: dead pending handle must fail closed"
    );
    let after = load_recovery_claim(&home, PROJECT_ID, ORIGIN_RUN_ID)
        .unwrap()
        .unwrap();
    assert_eq!(after.status, RecoveryClaimStatus::Pending);
    assert_eq!(after.replacement_run_id, planned.replacement_run_id);
    assert_eq!(after.replacement_runtime_id, planned.replacement_runtime_id);
    assert!(
        !tmux_has_session(name),
        "{point}: dead pane must not be recreated"
    );
    assert!(
        std::fs::read_dir(project_sessions_dir(&project_root))
            .unwrap()
            .filter_map(Result::ok)
            .filter(|entry| entry.path().extension().and_then(|ext| ext.to_str()) == Some("jsonl"))
            .count()
            <= 2,
        "{point}: dead-plan retry created a duplicate replacement"
    );
    restarted.terminate();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn recovery_fault_child_daemon() {
    if std::env::var("ORGASMIC_RECOVERY_CHILD").as_deref() != Ok("1") {
        return;
    }
    let home = Home::at(std::env::var_os("ORGASMIC_RECOVERY_CHILD_HOME").unwrap());
    let running = Daemon::run(home, daemon_options()).await.unwrap();
    std::fs::write(
        std::env::var_os("ORGASMIC_RECOVERY_CHILD_ADDR").unwrap(),
        running.addr.to_string(),
    )
    .unwrap();
    std::future::pending::<()>().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn recovery_faults_kill_real_daemon_and_preserve_original_pane() {
    if skip_test_if_missing(
        "recovery_faults_kill_real_daemon_and_preserve_original_pane",
        &[("tmux", tmux_available())],
    ) {
        return;
    }
    let points = [
        ("cleanup", false),
        ("temp_write", false),
        ("temp_fsync", false),
        ("rename", false),
        ("parent_fsync", false),
        ("pending", false),
        ("spawn_before_jsonl", true),
        ("acquire_append", true),
        ("run_meta_append", true),
        ("native_runtime_append", true),
        ("prompt_draft_append", true),
        ("recovery_origin_append", true),
        ("lifecycle_append", true),
        ("commit", true),
        ("response", true),
    ];
    for (point, runtime_launched) in points {
        replay_live_original_pane(point, runtime_launched).await;
    }

    // Every pending post-spawn boundary also gets the negative matrix: the
    // exact planned pane is killed after the daemon crash, and the next daemon
    // must reject rather than relaunch or mint a replacement identity.
    for point in [
        "spawn_before_jsonl",
        "acquire_append",
        "run_meta_append",
        "native_runtime_append",
        "prompt_draft_append",
        "recovery_origin_append",
        "lifecycle_append",
    ] {
        dead_pending_handle_fails_closed(point).await;
    }
}

// TASK-D6N77.1: an accepted recovery may have acquired its replacement's
// pane before it can append the public origin->replacement tx edge. The
// daemon's in-memory transition guard naturally dies with the daemon, so this
// proves the signed pending claim is the restart authority used by the *real*
// dispatch-wait HTTP path. It deliberately avoids manually beginning any
// tracker or fabricating a claim.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn dispatch_wait_survives_crash_between_recovery_acquire_and_association() {
    if skip_test_if_missing(
        "dispatch_wait_survives_crash_between_recovery_acquire_and_association",
        &[("tmux", tmux_available())],
    ) {
        return;
    }
    let tmp = tempfile::tempdir().unwrap();
    let (home, project_root) = seed_home_and_project(tmp.path());
    seed_dispatch_generation(&project_root);

    let (planned, pane) = kill_child_at_boundary(tmp.path(), &home, "association_pending").await;
    let planned = planned.expect("the live replacement must retain its signed pending claim");
    let _pane_guard = TmuxGuard(planned.planned_tmux_session.clone().unwrap());
    assert_eq!(planned.status, RecoveryClaimStatus::Pending);
    assert!(planned.spawn_started);
    assert!(
        pane.is_some(),
        "replacement pane must be live at crash boundary"
    );
    assert_eq!(
        recovery_association_count(&project_root, &planned.replacement_run_id),
        0,
        "the test must stop before public lineage association"
    );

    let mut restarted = spawn_daemon_child(tmp.path(), &home, None, None);
    let client = reqwest::Client::new();
    let waiting = dispatch_wait(&client, restarted.addr, &home).await;
    assert_eq!(waiting["generations"][0]["status"], "waiting");
    assert_eq!(
        waiting["generations"][0]["run_id"], planned.replacement_run_id,
        "restart must expose the planned replacement instead of false-death"
    );

    // Replay reattaches the exact pane, appends the durable edge, and commits
    // the claim. The child fixture's replacement process is intentionally not
    // a supervisor-live run after its owning daemon died, so wait now
    // converges through the ordinary durable lineage rather than retaining a
    // stale pending intent forever.
    let replay = client
        .post(recovery_url(restarted.addr))
        .bearer_auth(token(&home))
        .json(&recovery_body())
        .send()
        .await
        .unwrap();
    let status = replay.status();
    let body = replay.text().await.unwrap_or_default();
    assert!(
        status.is_success(),
        "recovery replay status {status}: {body}"
    );
    assert_eq!(
        recovery_association_count(&project_root, &planned.replacement_run_id),
        1,
        "replay must publish exactly one durable replacement edge"
    );
    let after_replay = dispatch_wait(&client, restarted.addr, &home).await;
    assert_eq!(after_replay["generations"][0]["status"], "died");
    assert_eq!(
        after_replay["generations"][0]["run_id"],
        planned.replacement_run_id
    );

    restarted.terminate();
}
