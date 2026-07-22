#![cfg(unix)]

use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use orgasmic_core::{
    project_sessions_dir, Home, Lifecycle, ReleaseOutcome, RuntimeIdentity, SessionEventKind,
    SessionWriter,
};
use orgasmic_daemon::recovery_claim::{load_recovery_claim, RecoveryClaim, RecoveryClaimStatus};
use orgasmic_daemon::{Daemon, DaemonOptions};
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
                worker_id: "implementer-claude-acp".into(),
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
                role: Some("implementer".into()),
                requires_worker_finalize: Some(true),
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

fn tmux_available() -> bool {
    Command::new("tmux")
        .arg("-V")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_ok_and(|status| status.success())
}

fn tmux_has_session(name: &str) -> bool {
    Command::new("tmux")
        .args(["has-session", "-t", name])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_ok_and(|status| status.success())
}

fn tmux_pane_identity(name: &str) -> String {
    let output = Command::new("tmux")
        .args([
            "display-message",
            "-p",
            "-t",
            name,
            "#{session_id}:#{pane_id}:#{pane_pid}",
        ])
        .output()
        .unwrap();
    assert!(output.status.success(), "planned pane {name} must be live");
    String::from_utf8(output.stdout).unwrap().trim().to_string()
}

fn kill_tmux(name: &str) {
    let _ = Command::new("tmux")
        .args(["kill-session", "-t", name])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status();
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
    assert!(
        response.status().is_success(),
        "{point}: replay status {}: {}",
        response.status(),
        restarted.diagnostics()
    );
    let response: Value = response.json().await.unwrap();
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
    if !tmux_available() {
        eprintln!("skipping recovery fault/restart matrix: tmux unavailable");
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
