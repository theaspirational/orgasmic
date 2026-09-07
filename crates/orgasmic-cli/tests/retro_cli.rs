//! Actual CLI -> isolated daemon -> offline SDK-host protocol. No provider calls.
#![cfg(unix)]
mod common;
use common::{orgasmic_command, write};
use orgasmic_core::{Home, RuntimeIdentity, SessionEventKind, SessionWriter};
use serde_json::{json, Value};

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn manual_retro_cli_dispatches_scoped_worker_and_requires_report_submission() {
    let tmp = tempfile::tempdir().unwrap();
    let home = Home::at(tmp.path().join("home"));
    home.ensure().unwrap();
    let source = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .canonicalize()
        .unwrap();
    std::os::unix::fs::symlink(source, home.source()).unwrap();
    let project = tmp.path().join("project");
    write(
        &project.join(".orgasmic/project.org"),
        "#+title: proj\n#+orgasmic_version: 1\n* PROJECT proj\n:PROPERTIES:\n:ID: proj\n:END:\n",
    );
    write(&home.board(),&format!("#+title: board\n#+orgasmic_version: 1\n* PROJECT proj\n:PROPERTIES:\n:ID: proj\n:PATH: {}\n:STATUS: active\n:END:\n",project.display()));
    write(&orgasmic_core::task_node_file_path(&project,"TASK-RETRO"),"#+title: retro\n#+orgasmic_version: 2\n* DONE TASK-RETRO Completed fixture\n:PROPERTIES:\n:ID: TASK-RETRO\n:END:\n");
    let session = orgasmic_core::project_sessions_dir(&project).join("run-retro-cli.jsonl");
    let mut writer =
        SessionWriter::open(&session, RuntimeIdentity::new("run-retro-cli", "old-boot")).unwrap();
    writer
        .append(
            SessionEventKind::Lifecycle,
            json!({"phase":"acquire","task_id":"TASK-RETRO","kind":"worker","worker_id":"offline"}),
        )
        .unwrap();
    writer.append(SessionEventKind::Lifecycle,json!({"phase":"run_meta","harness":"custom","transport":"stdio","worktree":project,"driver_config":{}})).unwrap();
    writer.append(SessionEventKind::Lifecycle,json!({"phase":"release","reason":"fixture","outcome":"failed","finalized_by_worker":false})).unwrap();
    drop(writer);
    let before = std::fs::read(&session).unwrap();
    let mut options = common::test_options();
    options.fs_watcher_enabled = false;
    let running = orgasmic_daemon::Daemon::run(home.clone(), options)
        .await
        .unwrap();
    let host = tmp.path().join("offline-host");
    write(
        &host,
        r#"#!/usr/bin/env python3
import json, os, sys
assert sys.argv[1] == 'retro'
assert not any(k.startswith('ORGASMIC_') for k in os.environ)
config = json.loads(sys.stdin.readline())
assert config['model'] == 'offline-fixture'
assert 'Call catalog first' in config['prompt']
def call(i, request):
 print(json.dumps({'id': i, 'request': request}), flush=True)
 return json.loads(sys.stdin.readline())
assert len(call(1, {'op':'catalog'})['result']['runs']) == 1
assert 'error' in call(2, {'op':'materialize','run':'outside-scope'})
assert 'error' in call(3, {'op':'materialize','run':'run-retro-cli'})
assert call(4, {'op':'submit','findings':[], 'coverage':'Catalog inspected; native evidence unavailable', 'limitations':'Offline fixture, no native correlation or measurable tool/token evidence'})['result']['status'] == 'report_submitted'
"#,
    );
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(&host, std::fs::Permissions::from_mode(0o700)).unwrap();
    let execute = |extra: &[&str]| {
        orgasmic_command()
            .args([
                "manager",
                "retro",
                "--project",
                "proj",
                "--run",
                "run-retro-cli",
            ])
            .args(extra)
            .env("ORGASMIC_HOME", &home.root)
            .env("ORGASMIC_DAEMON_URL", format!("http://{}", running.addr))
            .env("ORGASMIC_PROVIDER_HOST", &host)
            .env("ANTHROPIC_API_KEY", "offline-placeholder-never-used")
            .env_remove("CLAUDE_CODE_OAUTH_TOKEN")
            .current_dir(&project)
            .output()
            .unwrap()
    };
    let prepared = execute(&["--prepare-only"]);
    assert!(
        prepared.status.success(),
        "{}",
        String::from_utf8_lossy(&prepared.stderr)
    );
    let manifest: Value = serde_json::from_slice(&prepared.stdout).unwrap();
    assert!(
        !std::path::Path::new(manifest["directory"].as_str().unwrap())
            .join("report.json")
            .exists()
    );
    let output = execute(&["--model", "offline-fixture"]);
    assert!(
        output.status.success(),
        "stdout={} stderr={}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    let result: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(result["status"], "report_submitted");
    let report: Value =
        serde_json::from_slice(&std::fs::read(result["report"].as_str().unwrap()).unwrap())
            .unwrap();
    assert_eq!(report["terminal_declaration"], "report_submitted");
    assert_eq!(std::fs::read(&session).unwrap(), before);
    write(
        &host,
        "#!/usr/bin/env python3\nimport sys\nsys.stdin.readline()\n",
    );
    let failed = execute(&["--model", "offline-fixture"]);
    assert!(!failed.status.success());
    assert!(String::from_utf8_lossy(&failed.stderr).contains("without submitting a report"));
    assert_eq!(std::fs::read(&session).unwrap(), before);
    let _ = running.shutdown.send(());
    let _ = running.join.await;
}
