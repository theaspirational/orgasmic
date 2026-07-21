#![cfg(unix)]

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::Duration;

use orgasmic_core::{
    project_sessions_dir, Home, Lifecycle, ReleaseOutcome, RuntimeIdentity, SessionEventKind,
    SessionWriter,
};
use orgasmic_daemon::recovery_claim::{load_recovery_claim, RecoveryClaim, RecoveryClaimStatus};
use orgasmic_daemon::{Daemon, DaemonOptions, RunningDaemon};
use serde_json::{json, Value};

const PROJECT_ID: &str = "orgasmic";
const ORIGIN_RUN_ID: &str = "run-fault-origin";
const REQUEST_ID: &str = "task-tmm5h-fault-replay";

struct EnvGuard {
    key: &'static str,
    previous: Option<std::ffi::OsString>,
}

impl EnvGuard {
    fn set(key: &'static str, value: impl AsRef<std::ffi::OsStr>) -> Self {
        let previous = std::env::var_os(key);
        std::env::set_var(key, value);
        Self { key, previous }
    }
}

impl Drop for EnvGuard {
    fn drop(&mut self) {
        match self.previous.take() {
            Some(value) => std::env::set_var(self.key, value),
            None => std::env::remove_var(self.key),
        }
    }
}

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

    // Ensure parent_fsync targets the claim transaction rather than initial
    // directory creation, and give cleanup a forged stale temp to remove.
    std::fs::create_dir_all(home.state().join("recovery-claims").join(PROJECT_ID)).unwrap();
    (home, project_root)
}

fn daemon_options() -> DaemonOptions {
    DaemonOptions {
        bind_override: Some("127.0.0.1".parse().unwrap()),
        port_override: Some(0),
        fs_watcher_enabled: false,
        ..DaemonOptions::default()
    }
}

fn token(home: &Home) -> String {
    std::fs::read_to_string(home.auth_token())
        .unwrap()
        .trim()
        .to_string()
}

async fn stop(running: RunningDaemon) {
    let _ = running.shutdown.send(());
    tokio::time::timeout(Duration::from_secs(10), running.join)
        .await
        .expect("daemon shutdown timeout")
        .expect("daemon join");
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

fn kill_tmux(name: &str) {
    let _ = Command::new("tmux")
        .args(["kill-session", "-t", name])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status();
}

fn recreate_planned_handle(claim: &RecoveryClaim) {
    let name = claim.planned_tmux_session.as_deref().unwrap();
    kill_tmux(name);
    let status = Command::new("tmux")
        .args([
            "new-session",
            "-d",
            "-s",
            name,
            "sh",
            "-lc",
            "while :; do sleep 60; done",
        ])
        .status()
        .unwrap();
    assert!(status.success(), "recreate planned tmux handle {name}");
}

fn assert_complete_lifecycle(claim: &RecoveryClaim) {
    let envelopes = orgasmic_core::read_session_file(&claim.replacement_session_path).unwrap();
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

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn recovery_faults_restart_with_stable_plan_and_one_replacement() {
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
        ("commit", false),
        ("response", false),
    ];

    for (point, runtime_launched) in points {
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
        let path = std::env::var("PATH").unwrap_or_default();
        let _path_guard = EnvGuard::set("PATH", format!("{}:{path}", home.bin().display()));
        let running = Daemon::run(home.clone(), daemon_options()).await.unwrap();
        let bearer = token(&home);
        let client = reqwest::Client::new();
        let url = format!("http://{}/api/runs/{ORIGIN_RUN_ID}/recover", running.addr);
        let body = json!({
            "action": "start_recovery_run",
            "project": PROJECT_ID,
            "request_id": REQUEST_ID,
            "force_inert": false,
        });

        let failpoint_guard = EnvGuard::set("ORGASMIC_RECOVERY_FAILPOINT", point);
        let first = tokio::time::timeout(
            Duration::from_secs(20),
            client.post(&url).bearer_auth(&bearer).json(&body).send(),
        )
        .await
        .unwrap_or_else(|_| panic!("{point}: failpoint request timed out"));
        drop(failpoint_guard);
        if let Ok(response) = first {
            assert!(
                !response.status().is_success(),
                "{point}: failpoint did not interrupt recovery"
            );
        }
        stop(running).await;

        // Loading after the crash is the first handle-bound restart action;
        // it deterministically promotes a single valid durable temp.
        let planned = load_recovery_claim(&home, PROJECT_ID, ORIGIN_RUN_ID).unwrap();
        if runtime_launched {
            let claim = planned
                .as_ref()
                .unwrap_or_else(|| panic!("{point}: spawn occurred without durable plan"));
            assert_eq!(claim.status, RecoveryClaimStatus::Pending);
            recreate_planned_handle(claim);
        }

        let restarted = Daemon::run(home.clone(), daemon_options()).await.unwrap();
        let retry_url = format!("http://{}/api/runs/{ORIGIN_RUN_ID}/recover", restarted.addr);
        let response = client
            .post(retry_url)
            .bearer_auth(&bearer)
            .json(&body)
            .send()
            .await
            .unwrap_or_else(|err| panic!("{point}: replay request failed: {err}"));
        assert!(
            response.status().is_success(),
            "{point}: replay status {}",
            response.status()
        );
        let response: Value = response.json().await.unwrap();
        let committed = load_recovery_claim(&home, PROJECT_ID, ORIGIN_RUN_ID)
            .unwrap()
            .unwrap();
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
                .filter(
                    |entry| entry.path().extension().and_then(|ext| ext.to_str()) == Some("jsonl")
                )
                .count(),
            2,
            "{point}: origin plus exactly one replacement"
        );
        if runtime_launched {
            let session = committed.planned_tmux_session.as_deref().unwrap();
            assert!(
                tmux_has_session(session),
                "{point}: reconciliation must not kill the exact planned pane"
            );
        }
        kill_tmux(committed.planned_tmux_session.as_deref().unwrap());
        stop(restarted).await;
    }
}
