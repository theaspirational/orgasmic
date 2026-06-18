//! End-to-end integration tests for the daemon: HTTP routes, tx append,
//! parse-error reporting, and the WebSocket stream.

mod common;

use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

use futures::{SinkExt, StreamExt};
use orgasmic_core::Home;
use orgasmic_daemon::{Daemon, DaemonOptions, RunningDaemon};

fn test_options() -> DaemonOptions {
    DaemonOptions {
        bind_override: Some("127.0.0.1".parse().unwrap()),
        port_override: Some(0),
        // The vast majority of these tests never assert on watcher-driven
        // index refresh; disabling the `notify` backend avoids the ~0.8s
        // per-watch macOS FSEvents registration latency at every boot.
        // Tests that exercise the watcher (e.g.
        // `daemon_watcher_refreshes_after_direct_sprint_write`) boot with
        // `fs_watcher_enabled: true` explicitly.
        fs_watcher_enabled: false,
        // `stage_acquire_blocked_sessions_is_path_free` drives the real
        // claude/tmux architect stage. Its assertion is about the acquire
        // failure being path-free, not about TUI-input detection, so shrink the
        // driver's 10s input-ready window to the 1s floor to avoid waiting it
        // out. Other tests never reach the claude/tmux spawn path.
        tmux_input_ready_timeout_secs: Some(1),
        ..DaemonOptions::default()
    }
}

async fn boot(home: Home) -> RunningDaemon {
    Daemon::run(home, test_options())
        .await
        .expect("boot daemon")
}

async fn boot_with_watcher(home: Home) -> RunningDaemon {
    Daemon::run(
        home,
        DaemonOptions {
            fs_watcher_enabled: true,
            ..test_options()
        },
    )
    .await
    .expect("boot daemon with watcher")
}

fn read_token(home: &Home) -> String {
    let mut path = home.auth_token();
    // The test creates `home` at tempdir/home; ensure() generated the token.
    if !path.exists() {
        // Fallback: poll briefly in case ensure() hadn't flushed.
        for _ in 0..20 {
            std::thread::sleep(Duration::from_millis(50));
            if path.exists() {
                break;
            }
        }
    }
    if let Ok(meta) = std::fs::metadata(&path) {
        if !meta.is_file() {
            panic!("token at {} is not a file", path.display());
        }
    }
    std::fs::read_to_string(&path)
        .unwrap_or_else(|_| {
            path = home.user().join("auth").join("token");
            std::fs::read_to_string(&path).expect("token file")
        })
        .trim()
        .to_string()
}

fn write(path: &Path, contents: impl AsRef<str>) {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).unwrap();
    }
    std::fs::write(path, contents.as_ref()).unwrap();
}

fn seed_git_origin(repo: &Path, origin: &str) {
    let status = Command::new("git")
        .arg("init")
        .current_dir(repo)
        .status()
        .expect("git init");
    assert!(status.success(), "git init failed");
    let status = Command::new("git")
        .args(["config", "remote.origin.url", origin])
        .current_dir(repo)
        .status()
        .expect("git config remote.origin.url");
    assert!(status.success(), "git config remote.origin.url failed");
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
    std::os::unix::fs::symlink(repo_root(), home.source()).unwrap();
}

fn seed_project(home: &Home, project_root: &Path, project_id: &str) {
    write(
        &project_root.join(".orgasmic/project.org"),
        format!(
            "#+title: {project_id}\n#+orgasmic_version: 1\n\n* PROJECT {project_id}\n:PROPERTIES:\n:ID:               {project_id}\n:END:\n"
        ),
    );
    write(
        &project_root.join(".orgasmic/tasks/backlog.org"),
        "#+title: sprint\n#+orgasmic_version: 1\n\n* BACKLOG TASK-PRE Pre-boot task :work:\n:PROPERTIES:\n:ID:               TASK-PRE\n:END:\n",
    );
    write(
        &home.board(),
        format!(
            "#+title: orgasmic board\n#+orgasmic_version: 1\n\n* PROJECT {project_id}\n:PROPERTIES:\n:ID:               {project_id}\n:PATH:             {}\n:BRANCH:           main\n:END:\n",
            project_root.display()
        ),
    );
}

struct SparseProject {
    _tmp: tempfile::TempDir,
    home_root: PathBuf,
    project_root: PathBuf,
    project_id: String,
    running: RunningDaemon,
    token: String,
    client: reqwest::Client,
}

impl SparseProject {
    async fn boot(project_id: &str) -> Self {
        Self::boot_inner(project_id, false).await
    }

    async fn boot_with_source(project_id: &str) -> Self {
        Self::boot_inner(project_id, true).await
    }

    async fn boot_inner(project_id: &str, with_source: bool) -> Self {
        let tmp = tempfile::tempdir().unwrap();
        let home = Home::at(tmp.path().join("home"));
        home.ensure().unwrap();
        if with_source {
            symlink_repo_source(&home);
        }
        let project_root = tmp.path().join("proj");
        seed_project(&home, &project_root, project_id);
        let running = boot(home.clone()).await;
        let token = read_token(&home);
        let client = reqwest::Client::new();
        Self {
            _tmp: tmp,
            home_root: home.root.clone(),
            project_root,
            project_id: project_id.to_string(),
            running,
            token,
            client,
        }
    }

    async fn shutdown(self) {
        let _ = self.running.shutdown.send(());
        let _ = self.running.join.await;
    }
}

use common::{assert_body_rejects_paths, assert_path_free_error, assert_path_free_error_response};

async fn get_org_file_response(ctx: &SparseProject, path: &str) -> reqwest::Response {
    ctx.client
        .get(format!("http://{}/api/org/file", ctx.running.addr))
        .bearer_auth(&ctx.token)
        .query(&[("project", ctx.project_id.as_str()), ("path", path)])
        .send()
        .await
        .unwrap()
}

fn seed_unregistered_project(project_root: &Path, project_id: &str) {
    write(
        &project_root.join(".orgasmic/project.org"),
        format!(
            "#+title: {project_id}\n#+orgasmic_version: 1\n\n* PROJECT {project_id}\n:PROPERTIES:\n:ID:               {project_id}\n:DEFAULT_BRANCH:   main\n:END:\n"
        ),
    );
    write(
        &project_root.join(".orgasmic/tasks/backlog.org"),
        "#+title: sprint\n#+orgasmic_version: 1\n\n* BACKLOG TASK-NEW New task :work:\n:PROPERTIES:\n:ID:               TASK-NEW\n:END:\n",
    );
}

fn seed_project_scaffold(home: &Home) {
    let dst = home.source().join("shipped/skills/orgasmic/scaffold");
    std::fs::create_dir_all(dst.join("tasks")).unwrap();
    std::fs::create_dir_all(dst.join("conventions")).unwrap();
    write(&dst.join(".gitignore"), "tmp/\n");
    write(
        &dst.join("entry.org"),
        "#+title: orgasmic entry\n#+orgasmic_version: 1\n\n* Entry\n\nRead the file your task touches.\n",
    );
    write(
        &dst.join("conventions/contributing.org"),
        "#+title: contributing\n#+orgasmic_version: 1\n\n* Convention: contributing changes\n\n- Follow `project.org`.\n",
    );
    write(
        &dst.join("conventions/orgasmic-tooling.org"),
        "#+title: orgasmic tooling\n#+orgasmic_version: 1\n\n* Convention: working with orgasmic installed (optional)\n\nOptional.\n",
    );
    write(
        &dst.join("conventions/no-skill-installed.org"),
        "#+title: no skill installed\n#+orgasmic_version: 1\n\n* Convention: manager fallback without the `/orgasmic` skill\n\nBrief from plain files.\n",
    );
    write(
        &dst.join("conventions/manager-implementer.org"),
        "#+title: manager implementer\n#+orgasmic_version: 1\n\n* Convention: manager implementer\n\nStay scoped.\n",
    );
    write(
        &dst.join("project.org"),
        "#+title: {{PROJECT_NAME}}\n#+orgasmic_version: 1\n\n* PROJECT {{PROJECT_NAME}}\n:PROPERTIES:\n:ID:               {{PROJECT_ID}}\n:END:\n",
    );
    write(
        &dst.join("decisions.org"),
        "#+title: {{PROJECT_NAME}} decisions\n#+orgasmic_version: 1\n\n* dec_001 Bootstrap project state    :bootstrap:\n:PROPERTIES:\n:ID:               dec_001\n:DECIDED_AT:       [2026-06-09 Tue]\n:END:\n** Context\nThe project has just been scaffolded.\n** Decision\nKeep an initial bootstrap decision as an anchor for repo-specific decisions.\n** Consequences\nLater decisions continue with sequential dec_NNN records.\n",
    );
    write(
        &dst.join("config.org"),
        "#+title: {{PROJECT_NAME}} config\n#+orgasmic_version: 1\n\n* CONFIG {{PROJECT_ID}}\n:PROPERTIES:\n:ID:               {{PROJECT_ID}}\n:DEFAULT_BRANCH:   {{DEFAULT_BRANCH}}\n:END:\n",
    );
    write(
        &dst.join("tasks/backlog.org"),
        "#+title: sprint\n#+orgasmic_version: 1\n\n* BACKLOG TASK-001 Example task\n:PROPERTIES:\n:ID:               TASK-001\n:END:\n",
    );
    for (rel, title) in [
        ("tasks/todo.org", "Todo"),
        ("tasks/in_progress.org", "In progress"),
        ("tasks/in_review.org", "In review"),
        ("tasks/done.org", "Done"),
        ("tasks/cancelled.org", "Cancelled"),
    ] {
        write(
            &dst.join(rel),
            format!("#+title: {title}\n#+orgasmic_version: 1\n"),
        );
    }
    write(
        &dst.join("tasks/goal.org"),
        "#+title: Goal\n#+orgasmic_version: 1\n\n* GOAL Bootstrap project state\n:PROPERTIES:\n:ID: goal-bootstrap\n:STATUS: active\n:END:\n\n** Statement\nBootstrap.\n",
    );
    write(
        &dst.join("tasks/handoff.org"),
        "#+title: Handoff\n#+orgasmic_version: 1\n\n* HANDOFF current\n:PROPERTIES:\n:ID: handoff-current\n:GOAL_ID: goal-bootstrap\n:END:\n\n** Next likely actions\n- Start TASK-001.\n",
    );
    write(
        &dst.join("gotchas.org"),
        "#+title: Gotchas\n#+orgasmic_version: 1\n\n* Gotchas\n:PROPERTIES:\n:ID:               gotchas\n:END:\n",
    );
}

async fn post_project_json(
    client: reqwest::Client,
    addr: std::net::SocketAddr,
    token: String,
    body: serde_json::Value,
) -> (reqwest::StatusCode, String) {
    let resp = client
        .post(format!("http://{addr}/api/projects"))
        .bearer_auth(token)
        .json(&body)
        .send()
        .await
        .unwrap();
    let status = resp.status();
    let body = resp.text().await.unwrap();
    (status, body)
}

fn valid_scaffold_request() -> serde_json::Value {
    serde_json::json!({
        "scaffold": true
    })
}

async fn assert_invalid_scaffold_request(
    mut body: serde_json::Value,
    project_dir_name: &str,
    expected: &str,
) {
    let tmp = tempfile::tempdir().unwrap();
    let home = Home::at(tmp.path().join("home"));
    home.ensure().unwrap();
    seed_project_scaffold(&home);
    let project_root = tmp.path().join(project_dir_name);
    std::fs::create_dir_all(&project_root).unwrap();
    body["path"] = serde_json::json!(project_root.display().to_string());

    let running = boot(home.clone()).await;
    let token = read_token(&home);
    let client = reqwest::Client::new();
    let (status, body) = post_project_json(client, running.addr, token, body).await;
    let expected_status = if expected.starts_with("unknown field") {
        reqwest::StatusCode::UNPROCESSABLE_ENTITY
    } else {
        reqwest::StatusCode::BAD_REQUEST
    };
    assert_eq!(status, expected_status, "{body}");
    assert!(body.contains(expected), "expected {expected:?}, got {body}");
    assert!(
        !project_root.join(".orgasmic").exists(),
        "invalid input must not scaffold files"
    );
    assert!(
        !home.board().exists(),
        "invalid scaffold request must not create board.org"
    );

    let _ = running.shutdown.send(());
    let _ = running.join.await;
}

async fn get_runs_json(
    client: &reqwest::Client,
    addr: std::net::SocketAddr,
    token: &str,
) -> serde_json::Value {
    client
        .get(format!("http://{addr}/api/runs"))
        .bearer_auth(token)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap()
}

#[tokio::test]
async fn post_projects_registers_existing_orgasmic_project_and_refreshes_index() {
    let tmp = tempfile::tempdir().unwrap();
    let home = Home::at(tmp.path().join("home"));
    home.ensure().unwrap();
    let project_root = tmp.path().join("existing");
    seed_unregistered_project(&project_root, "proj-existing");
    seed_git_origin(&project_root, "git@example.com:org/existing.git");

    let running = boot(home.clone()).await;
    let token = read_token(&home);
    let client = reqwest::Client::new();
    let resp = client
        .post(format!("http://{}/api/projects", running.addr))
        .bearer_auth(&token)
        .json(&serde_json::json!({
            "path": project_root.display().to_string()
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), reqwest::StatusCode::CREATED);
    let project: serde_json::Value = resp.json().await.unwrap();
    assert_eq!(project["project_id"], "proj-existing");
    assert_eq!(project["repo_url"], "git@example.com:org/existing.git");
    assert_eq!(project["branch"], "main");

    let resp = client
        .get(format!("http://{}/api/projects", running.addr))
        .bearer_auth(&token)
        .send()
        .await
        .unwrap();
    assert!(
        resp.status().is_success(),
        "get /projects: {}",
        resp.status()
    );
    let projects: serde_json::Value = resp.json().await.unwrap();
    assert!(projects.as_array().unwrap().iter().any(|project| {
        project["project_id"] == "proj-existing"
            && project["tasks"]
                .as_array()
                .map(|tasks| tasks.iter().any(|task| task["id"] == "TASK-NEW"))
                .unwrap_or(false)
    }));

    let _ = running.shutdown.send(());
    let _ = running.join.await;
}

#[tokio::test]
async fn post_projects_rejects_uninitialized_project_without_scaffold() {
    let tmp = tempfile::tempdir().unwrap();
    let home = Home::at(tmp.path().join("home"));
    let project_root = tmp.path().join("empty");
    std::fs::create_dir_all(&project_root).unwrap();

    let running = boot(home.clone()).await;
    let token = read_token(&home);
    let client = reqwest::Client::new();
    let resp = client
        .post(format!("http://{}/api/projects", running.addr))
        .bearer_auth(&token)
        .json(&serde_json::json!({
            "path": project_root.display().to_string()
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), reqwest::StatusCode::BAD_REQUEST);
    let body = resp.text().await.unwrap();
    assert!(body.contains("not an orgasmic project"));

    let _ = running.shutdown.send(());
    let _ = running.join.await;
}

#[tokio::test]
async fn post_projects_scaffolds_and_registers_uninitialized_project() {
    let tmp = tempfile::tempdir().unwrap();
    let home = Home::at(tmp.path().join("home"));
    home.ensure().unwrap();
    seed_project_scaffold(&home);
    let project_root = tmp.path().join("fresh");
    std::fs::create_dir_all(&project_root).unwrap();
    seed_git_origin(&project_root, "git@example.com:org/fresh.git");

    let running = boot(home.clone()).await;
    let token = read_token(&home);
    let client = reqwest::Client::new();
    let resp = client
        .post(format!("http://{}/api/projects", running.addr))
        .bearer_auth(&token)
        .json(&serde_json::json!({
            "path": project_root.display().to_string(),
            "scaffold": true
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), reqwest::StatusCode::CREATED);
    let project: serde_json::Value = resp.json().await.unwrap();
    assert_eq!(project["project_id"], "fresh");
    assert_eq!(project["repo_url"], "git@example.com:org/fresh.git");
    assert_eq!(project["branch"], "main");
    let project_org = project_root.join(".orgasmic/project.org");
    assert!(project_org.exists());
    let raw = std::fs::read_to_string(project_org).unwrap();
    assert!(raw.contains(":ID:               fresh"));
    assert!(!raw.contains(":REPO_URL:"));

    let _ = running.shutdown.send(());
    let _ = running.join.await;
}

#[tokio::test]
async fn post_projects_rejects_unknown_project_id_field_without_writes() {
    let mut body = valid_scaffold_request();
    body["project_id"] = serde_json::json!("proj-bad\n* INJECTED");
    assert_invalid_scaffold_request(body, "fresh", "unknown field `project_id`").await;
}

#[tokio::test]
async fn post_projects_rejects_unknown_repo_url_field_without_writes() {
    let mut body = valid_scaffold_request();
    body["repo_url"] = serde_json::json!("git@example.com:org/repo.git\n:STATUS: corrupted");
    assert_invalid_scaffold_request(body, "fresh", "unknown field `repo_url`").await;
}

#[tokio::test]
async fn post_projects_rejects_unknown_default_branch_field_without_writes() {
    let mut body = valid_scaffold_request();
    body["default_branch"] = serde_json::json!("main\n:STATUS: corrupted");
    assert_invalid_scaffold_request(body, "fresh", "unknown field `default_branch`").await;
}

#[tokio::test]
async fn post_projects_rejects_oversized_derived_project_id_without_writes() {
    let body = valid_scaffold_request();
    assert_invalid_scaffold_request(body, &"x".repeat(65), "project_id must be 1-64 chars").await;
}

#[tokio::test]
async fn post_projects_rejects_project_root_newline_without_writes() {
    let body = valid_scaffold_request();
    assert_invalid_scaffold_request(
        body,
        "fresh\npath",
        "project_root must not contain newlines or control characters",
    )
    .await;
}

#[tokio::test]
async fn post_projects_existing_project_parse_error_is_path_free() {
    let tmp = tempfile::tempdir().unwrap();
    let home = Home::at(tmp.path().join("home"));
    home.ensure().unwrap();
    let project_root = tmp.path().join("bad-project");
    write(
        &project_root.join(".orgasmic/project.org"),
        "#+title: bad\n#+orgasmic_version: 1\n\n* PROJECT bad\n:PROPERTIES:\nnot-a-property\n",
    );

    let running = boot(home.clone()).await;
    let token = read_token(&home);
    let client = reqwest::Client::new();
    let resp = client
        .post(format!("http://{}/api/projects", running.addr))
        .bearer_auth(&token)
        .json(&serde_json::json!({
            "path": project_root.display().to_string()
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), reqwest::StatusCode::BAD_REQUEST);
    let body = resp.text().await.unwrap();
    assert!(body.contains("project file is not valid orgasmic markup"));
    assert!(!body.contains(project_root.to_string_lossy().as_ref()));

    let _ = running.shutdown.send(());
    let _ = running.join.await;
}

#[tokio::test]
async fn get_org_file_missing_decisions_is_path_free() {
    let ctx = SparseProject::boot("proj-missing-decisions").await;
    let resp = get_org_file_response(&ctx, ".orgasmic/decisions.org").await;
    let reject = [ctx.project_root.as_path(), ctx.home_root.as_path()];
    let error =
        assert_path_free_error_response(resp, reqwest::StatusCode::NOT_FOUND, "not found", &reject)
            .await;
    assert!(error.contains("decisions file"));
    assert!(!error.contains("read "));
    ctx.shutdown().await;
}

#[tokio::test]
async fn get_project_missing_task_is_path_free() {
    let ctx = SparseProject::boot("proj-missing-task").await;
    let resp = ctx
        .client
        .get(format!(
            "http://{}/api/projects/{}/tasks/TASK-MISSING",
            ctx.running.addr, ctx.project_id
        ))
        .bearer_auth(&ctx.token)
        .send()
        .await
        .unwrap();
    let reject = [ctx.project_root.as_path(), ctx.home_root.as_path()];
    let error =
        assert_path_free_error_response(resp, reqwest::StatusCode::NOT_FOUND, "not found", &reject)
            .await;
    assert!(!error.contains("read "));
    ctx.shutdown().await;
}

#[tokio::test]
async fn get_org_file_directory_read_failure_is_path_free_500() {
    let ctx = SparseProject::boot("proj-dir-read-failure").await;
    let dir_path = ctx.project_root.join(".orgasmic/decisions.org");
    std::fs::create_dir_all(&dir_path).unwrap();

    let resp = get_org_file_response(&ctx, ".orgasmic/decisions.org").await;
    let reject = [
        ctx.project_root.as_path(),
        ctx.home_root.as_path(),
        dir_path.as_path(),
    ];
    let error = assert_path_free_error_response(
        resp,
        reqwest::StatusCode::INTERNAL_SERVER_ERROR,
        "failed to",
        &reject,
    )
    .await;
    assert!(error.starts_with("failed to"));
    ctx.shutdown().await;
}

#[tokio::test]
async fn worker_routes_sanitize_loader_failures() {
    let tmp = tempfile::tempdir().unwrap();
    let home = Home::at(tmp.path().join("home"));
    home.ensure().unwrap();
    let bad_path = home.user().join("workers/broken.org");
    std::fs::create_dir_all(&bad_path).unwrap();
    let running = boot(home.clone()).await;
    let token = read_token(&home);
    let client = reqwest::Client::new();
    let reject = [home.root.as_path(), bad_path.as_path()];

    for route in ["/api/workers", "/api/workers/broken"] {
        let resp = client
            .get(format!("http://{}{}", running.addr, route))
            .bearer_auth(&token)
            .send()
            .await
            .unwrap();
        let error = assert_path_free_error_response(
            resp,
            reqwest::StatusCode::INTERNAL_SERVER_ERROR,
            "failed to",
            &reject,
        )
        .await;
        assert!(error.starts_with("failed to"), "{route}: {error}");
    }

    let _ = running.shutdown.send(());
    let _ = running.join.await;
}

#[tokio::test]
async fn worker_validate_routes_return_structured_diagnostics() {
    let tmp = tempfile::tempdir().unwrap();
    let home = Home::at(tmp.path().join("home"));
    home.ensure().unwrap();
    symlink_repo_source(&home);
    write(
        &home.user().join("workers/validator-ok.org"),
        "* WORKER validator-ok\n:PROPERTIES:\n:ID: validator-ok\n:KIND: implementer\n:DRIVER: acp-stdio\n:HARNESS: claude\n:PROVIDERS: anthropic\n:MODELS: claude-sonnet-4-6\n:REASONING_EFFORTS: high\n:DEFAULT_PROVIDER: anthropic\n:DEFAULT_MODEL: claude-sonnet-4-6\n:DEFAULT_EFFORT: high\n:LINKED_SKILLS:\n:END:\n",
    );
    write(
        &home.user().join("workers/validator-missing-skill.org"),
        "* WORKER validator-missing-skill\n:PROPERTIES:\n:ID: validator-missing-skill\n:KIND: implementer\n:DRIVER: acp-stdio\n:HARNESS: claude\n:LINKED_SKILLS: no-such-skill\n:END:\n",
    );
    let running = boot(home.clone()).await;
    let token = read_token(&home);
    let client = reqwest::Client::new();

    let sweep = client
        .get(format!("http://{}/api/workers/validate", running.addr))
        .bearer_auth(&token)
        .send()
        .await
        .unwrap();
    assert_eq!(sweep.status(), reqwest::StatusCode::OK);
    let sweep: Vec<serde_json::Value> = sweep.json().await.unwrap();
    let valid = sweep
        .iter()
        .find(|result| result["id"] == "validator-ok")
        .expect("valid worker result");
    assert_eq!(valid["ok"], true);
    let missing = sweep
        .iter()
        .find(|result| result["id"] == "validator-missing-skill")
        .expect("missing skill result");
    assert_eq!(missing["ok"], false);
    assert_eq!(missing["errors"][0]["code"], "missing_skill");

    let by_id = client
        .post(format!("http://{}/api/workers/validate", running.addr))
        .bearer_auth(&token)
        .json(&serde_json::json!({ "id": "validator-ok" }))
        .send()
        .await
        .unwrap();
    assert_eq!(by_id.status(), reqwest::StatusCode::OK);
    let by_id: serde_json::Value = by_id.json().await.unwrap();
    assert_eq!(by_id["ok"], true);

    let inline = client
        .post(format!("http://{}/api/workers/validate", running.addr))
        .bearer_auth(&token)
        .json(&serde_json::json!({
            "source": "* WORKER inline-bad\n:PROPERTIES:\n:ID: inline-bad\n:KIND: implementer\n:DRIVER: acp-ws\n:HARNESS: claude\n:END:\n"
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(inline.status(), reqwest::StatusCode::OK);
    let inline: serde_json::Value = inline.json().await.unwrap();
    assert_eq!(inline["ok"], false);
    assert_eq!(inline["errors"][0]["code"], "unsupported_driver_harness");

    let _ = running.shutdown.send(());
    let _ = running.join.await;
}

#[tokio::test]
async fn prompt_routes_sanitize_loader_failures() {
    let tmp = tempfile::tempdir().unwrap();
    let home = Home::at(tmp.path().join("home"));
    home.ensure().unwrap();
    let bad_path = home.user().join("prompt-studio/prompt-specs/broken.org");
    std::fs::create_dir_all(&bad_path).unwrap();
    let running = boot(home.clone()).await;
    let token = read_token(&home);
    let client = reqwest::Client::new();
    let reject = [home.root.as_path(), bad_path.as_path()];

    for route in ["/api/prompt-specs", "/api/prompt-specs/broken"] {
        let resp = client
            .get(format!("http://{}{}", running.addr, route))
            .bearer_auth(&token)
            .send()
            .await
            .unwrap();
        let error = assert_path_free_error_response(
            resp,
            reqwest::StatusCode::INTERNAL_SERVER_ERROR,
            "failed to",
            &reject,
        )
        .await;
        assert!(error.starts_with("failed to"), "{route}: {error}");
    }

    let _ = running.shutdown.send(());
    let _ = running.join.await;
}

#[tokio::test]
async fn skill_routes_sanitize_loader_failures() {
    let tmp = tempfile::tempdir().unwrap();
    let home = Home::at(tmp.path().join("home"));
    home.ensure().unwrap();
    let bad_path = home.user().join("skills/broken.org");
    std::fs::create_dir_all(&bad_path).unwrap();
    let running = boot(home.clone()).await;
    let token = read_token(&home);
    let client = reqwest::Client::new();
    let reject = [home.root.as_path(), bad_path.as_path()];

    for route in ["/api/skills", "/api/skills/broken"] {
        let resp = client
            .get(format!("http://{}{}", running.addr, route))
            .bearer_auth(&token)
            .send()
            .await
            .unwrap();
        let error = assert_path_free_error_response(
            resp,
            reqwest::StatusCode::INTERNAL_SERVER_ERROR,
            "failed to",
            &reject,
        )
        .await;
        assert!(error.starts_with("failed to"), "{route}: {error}");
    }

    let _ = running.shutdown.send(());
    let _ = running.join.await;
}

#[tokio::test]
async fn skill_routes_include_shipped_and_user_markdown_skills() {
    let tmp = tempfile::tempdir().unwrap();
    let home = Home::at(tmp.path().join("home"));
    home.ensure().unwrap();
    symlink_repo_source(&home);
    write(
        &home.user().join("skills/user-skill/SKILL.md"),
        r#"---
name: user-skill
description: 'User skill for tests. Triggers: "/user-skill", "/user-skill run".'
---

# user-skill
"#,
    );
    let running = boot(home.clone()).await;
    let token = read_token(&home);
    let client = reqwest::Client::new();

    let resp = client
        .get(format!("http://{}/api/skills", running.addr))
        .bearer_auth(&token)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), reqwest::StatusCode::OK);
    let skills: Vec<serde_json::Value> = resp.json().await.unwrap();
    let orgasmic = skills
        .iter()
        .find(|skill| skill["id"] == "orgasmic")
        .expect("shipped markdown skill should be listed");
    assert!(orgasmic["source_path"]
        .as_str()
        .unwrap()
        .ends_with("shipped/skills/orgasmic/SKILL.md"));
    assert!(orgasmic["triggers"]
        .as_array()
        .unwrap()
        .iter()
        .any(|trigger| trigger == "/orgasmic"));

    let user = skills
        .iter()
        .find(|skill| skill["id"] == "user-skill")
        .expect("user markdown skill should be listed");
    assert!(user["source_path"]
        .as_str()
        .unwrap()
        .ends_with("user/skills/user-skill/SKILL.md"));
    assert!(user["triggers"]
        .as_array()
        .unwrap()
        .iter()
        .any(|trigger| trigger == "/user-skill run"));

    for id in ["orgasmic", "user-skill"] {
        let resp = client
            .get(format!("http://{}/api/skills/{id}", running.addr))
            .bearer_auth(&token)
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), reqwest::StatusCode::OK, "{id}");
    }

    let _ = running.shutdown.send(());
    let _ = running.join.await;
}

#[tokio::test]
async fn content_load_routes_sanitize_loader_rule_not_found() {
    let tmp = tempfile::tempdir().unwrap();
    let home = Home::at(tmp.path().join("home"));
    home.ensure().unwrap();
    let running = boot(home.clone()).await;
    let token = read_token(&home);
    let client = reqwest::Client::new();
    let reject = [home.root.as_path()];

    for route in [
        "/api/workers/missing",
        "/api/prompt-specs/missing",
        "/api/skills/missing",
    ] {
        let resp = client
            .get(format!("http://{}{}", running.addr, route))
            .bearer_auth(&token)
            .send()
            .await
            .unwrap();
        let error = assert_path_free_error_response(
            resp,
            reqwest::StatusCode::NOT_FOUND,
            "not found",
            &reject,
        )
        .await;
        assert!(!error.contains("read "), "{route}: {error}");
    }

    let _ = running.shutdown.send(());
    let _ = running.join.await;
}

#[tokio::test]
async fn index_only_routes_are_path_free_on_empty_state() {
    let ctx = SparseProject::boot("proj-index-only").await;
    let reject = [ctx.project_root.as_path(), ctx.home_root.as_path()];

    let resp = ctx
        .client
        .get(format!(
            "http://{}/api/tasks/TASK-PRE/activity",
            ctx.running.addr
        ))
        .bearer_auth(&ctx.token)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), reqwest::StatusCode::OK);
    let body = resp.text().await.unwrap();
    assert_body_rejects_paths(&body, &reject);
    let activity: serde_json::Value = serde_json::from_str(&body).unwrap();
    assert!(activity.as_array().unwrap().is_empty());

    ctx.shutdown().await;
}

#[tokio::test]
async fn task_activity_missing_task_is_path_free_404() {
    let ctx = SparseProject::boot("proj-missing-activity").await;
    let resp = ctx
        .client
        .get(format!(
            "http://{}/api/tasks/TASK-MISSING/activity",
            ctx.running.addr
        ))
        .bearer_auth(&ctx.token)
        .send()
        .await
        .unwrap();
    let reject = [ctx.project_root.as_path(), ctx.home_root.as_path()];
    let error =
        assert_path_free_error_response(resp, reqwest::StatusCode::NOT_FOUND, "not found", &reject)
            .await;
    assert!(!error.contains("read "));
    ctx.shutdown().await;
}

#[cfg(unix)]
fn make_readonly(path: &Path) {
    use std::os::unix::fs::PermissionsExt;
    let mut perms = std::fs::metadata(path).unwrap().permissions();
    perms.set_mode(0o444);
    std::fs::set_permissions(path, perms).unwrap();
}

#[tokio::test]
async fn post_org_file_writer_failure_is_path_free() {
    #[cfg(not(unix))]
    return;

    let ctx = SparseProject::boot("proj-org-rewrite-fail").await;
    let org_path = ctx.project_root.join(".orgasmic/decisions.org");
    write(
        &org_path,
        "#+title: decisions\n#+orgasmic_version: 1\n\n* DECISION dec_001\n:PROPERTIES:\n:ID: dec_001\n:END:\n",
    );
    #[cfg(unix)]
    make_readonly(&org_path);

    let resp = ctx
        .client
        .post(format!("http://{}/api/org/file", ctx.running.addr))
        .bearer_auth(&ctx.token)
        .json(&serde_json::json!({
            "project": ctx.project_id,
            "path": ".orgasmic/decisions.org",
            "contents": "#+title: decisions\n#+orgasmic_version: 1\n\n* DECISION dec_001\n:PROPERTIES:\n:ID: dec_001\n:END:\n"
        }))
        .send()
        .await
        .unwrap();
    let reject = [
        ctx.project_root.as_path(),
        ctx.home_root.as_path(),
        org_path.as_path(),
    ];
    assert_path_free_error_response(
        resp,
        reqwest::StatusCode::INTERNAL_SERVER_ERROR,
        "failed to rewrite",
        &reject,
    )
    .await;
    ctx.shutdown().await;
}

#[tokio::test]
async fn stage_acquire_blocked_sessions_is_path_free() {
    let ctx = SparseProject::boot_with_source("proj-stage-acquire-fail").await;
    // Stage transcripts write to the project's per-project sessions dir; block
    // it (replace the dir with a file) so the session writer fails on acquire.
    let sessions = ctx.project_root.join(".orgasmic/tmp/sessions");
    let _ = std::fs::remove_dir_all(&sessions);
    std::fs::create_dir_all(sessions.parent().unwrap()).unwrap();
    std::fs::write(&sessions, "blocked").unwrap();

    let resp = ctx
        .client
        .post(format!("http://{}/api/architect", ctx.running.addr))
        .bearer_auth(&ctx.token)
        .json(&serde_json::json!({
            "project": ctx.project_id,
            "reason": "path-free acquire error test",
        }))
        .send()
        .await
        .unwrap();
    let reject = [
        ctx.project_root.as_path(),
        ctx.home_root.as_path(),
        sessions.as_path(),
    ];
    assert_path_free_error_response(
        resp,
        reqwest::StatusCode::INTERNAL_SERVER_ERROR,
        "failed to acquire",
        &reject,
    )
    .await;
    ctx.shutdown().await;
}

#[tokio::test]
async fn post_org_file_parse_error_is_path_free() {
    let ctx = SparseProject::boot("proj-org-parse-fail").await;
    let resp = ctx
        .client
        .post(format!("http://{}/api/org/file", ctx.running.addr))
        .bearer_auth(&ctx.token)
        .json(&serde_json::json!({
            "project": ctx.project_id,
            "path": ".orgasmic/decisions.org",
            "contents": "* DECISION broken\n:PROPERTIES:\nnot-a-property-line\n:END:\n"
        }))
        .send()
        .await
        .unwrap();
    let reject = [ctx.project_root.as_path(), ctx.home_root.as_path()];
    assert_path_free_error_response(
        resp,
        reqwest::StatusCode::BAD_REQUEST,
        "org file is not valid orgasmic markup",
        &reject,
    )
    .await;
    ctx.shutdown().await;
}

#[tokio::test]
async fn create_subtask_writer_transaction_failure_is_path_free() {
    #[cfg(not(unix))]
    return;

    let tmp = tempfile::tempdir().unwrap();
    let home = Home::at(tmp.path().join("home"));
    home.ensure().unwrap();
    let project_root = tmp.path().join("proj");
    seed_project(&home, &project_root, "proj-writer-tx-fail");
    write(
        &project_root.join(".orgasmic/tasks/backlog.org"),
        "#+title: sprint\n#+orgasmic_version: 1\n\n* BACKLOG TASK-001 Parent task\n:PROPERTIES:\n:ID:               TASK-001\n:END:\n",
    );

    let running = boot(home.clone()).await;
    let sprint_path = project_root.join(".orgasmic/tasks/backlog.org");
    make_readonly(&sprint_path);

    let token = read_token(&home);
    let client = reqwest::Client::new();
    let resp = client
        .post(format!(
            "http://{}/api/tasks/TASK-001/subtasks",
            running.addr
        ))
        .bearer_auth(&token)
        .json(&serde_json::json!({
            "title": "Child task",
            "kind": "implementer",
            "description": "Child details.",
            "request_id": "writer-tx-fail-test"
        }))
        .send()
        .await
        .unwrap();
    let reject = [
        project_root.as_path(),
        home.root.as_path(),
        sprint_path.as_path(),
    ];
    assert_path_free_error_response(
        resp,
        reqwest::StatusCode::INTERNAL_SERVER_ERROR,
        "failed to apply changes",
        &reject,
    )
    .await;

    let _ = running.shutdown.send(());
    let _ = running.join.await;
}

#[tokio::test]
async fn post_projects_rejects_duplicate_registration() {
    let tmp = tempfile::tempdir().unwrap();
    let home = Home::at(tmp.path().join("home"));
    home.ensure().unwrap();
    let project_root = tmp.path().join("proj");
    seed_project(&home, &project_root, "proj-dupe");

    let running = boot(home.clone()).await;
    let token = read_token(&home);
    let client = reqwest::Client::new();
    let resp = client
        .post(format!("http://{}/api/projects", running.addr))
        .bearer_auth(&token)
        .json(&serde_json::json!({
            "path": project_root.display().to_string()
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), reqwest::StatusCode::CONFLICT);
    let body = resp.text().await.unwrap();
    assert!(body.contains("already on the board"));

    let _ = running.shutdown.send(());
    let _ = running.join.await;
}

#[tokio::test]
async fn concurrent_post_projects_distinct_ids_are_all_registered() {
    let tmp = tempfile::tempdir().unwrap();
    let home = Home::at(tmp.path().join("home"));
    home.ensure().unwrap();
    let project_a = tmp.path().join("race-a");
    let project_b = tmp.path().join("race-b");
    seed_unregistered_project(&project_a, "proj-race-a");
    seed_unregistered_project(&project_b, "proj-race-b");

    let running = boot(home.clone()).await;
    let token = read_token(&home);
    let client = reqwest::Client::new();
    let first = tokio::spawn(post_project_json(
        client.clone(),
        running.addr,
        token.clone(),
        serde_json::json!({ "path": project_a.display().to_string() }),
    ));
    let second = tokio::spawn(post_project_json(
        client,
        running.addr,
        token,
        serde_json::json!({ "path": project_b.display().to_string() }),
    ));
    let (first, second) = tokio::join!(first, second);
    let (first_status, first_body) = first.unwrap();
    let (second_status, second_body) = second.unwrap();
    assert_eq!(
        first_status,
        reqwest::StatusCode::CREATED,
        "first response: {first_body}"
    );
    assert_eq!(
        second_status,
        reqwest::StatusCode::CREATED,
        "second response: {second_body}"
    );
    let board = std::fs::read_to_string(home.board()).unwrap();
    assert!(board.contains(":ID:               proj-race-a"));
    assert!(board.contains(":ID:               proj-race-b"));

    let _ = running.shutdown.send(());
    let _ = running.join.await;
}

#[tokio::test]
async fn concurrent_post_projects_same_id_returns_one_created_one_conflict() {
    let tmp = tempfile::tempdir().unwrap();
    let home = Home::at(tmp.path().join("home"));
    home.ensure().unwrap();
    let project_a = tmp.path().join("same-a");
    let project_b = tmp.path().join("same-b");
    seed_unregistered_project(&project_a, "proj-race-same");
    seed_unregistered_project(&project_b, "proj-race-same");

    let running = boot(home.clone()).await;
    let token = read_token(&home);
    let client = reqwest::Client::new();
    let first = tokio::spawn(post_project_json(
        client.clone(),
        running.addr,
        token.clone(),
        serde_json::json!({ "path": project_a.display().to_string() }),
    ));
    let second = tokio::spawn(post_project_json(
        client,
        running.addr,
        token,
        serde_json::json!({ "path": project_b.display().to_string() }),
    ));
    let (first, second) = tokio::join!(first, second);
    let responses = [first.unwrap(), second.unwrap()];
    let created = responses
        .iter()
        .filter(|(status, _)| *status == reqwest::StatusCode::CREATED)
        .count();
    let conflicts = responses
        .iter()
        .filter(|(status, body)| {
            *status == reqwest::StatusCode::CONFLICT
                && body.contains("project proj-race-same already on the board")
        })
        .count();
    assert_eq!(created, 1, "responses: {responses:?}");
    assert_eq!(conflicts, 1, "responses: {responses:?}");
    let board = std::fs::read_to_string(home.board()).unwrap();
    assert_eq!(
        board.matches(":ID:               proj-race-same").count(),
        1
    );

    let _ = running.shutdown.send(());
    let _ = running.join.await;
}

#[tokio::test]
async fn post_projects_rejects_relative_path() {
    let tmp = tempfile::tempdir().unwrap();
    let home = Home::at(tmp.path().join("home"));

    let running = boot(home.clone()).await;
    let token = read_token(&home);
    let client = reqwest::Client::new();
    let resp = client
        .post(format!("http://{}/api/projects", running.addr))
        .bearer_auth(&token)
        .json(&serde_json::json!({
            "path": "relative/project"
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), reqwest::StatusCode::BAD_REQUEST);
    let body = resp.text().await.unwrap();
    assert!(body.contains("path must be absolute"));

    let _ = running.shutdown.send(());
    let _ = running.join.await;
}

#[tokio::test]
async fn corrupted_historical_tx_blocks_daemon_start() {
    let tmp = tempfile::tempdir().unwrap();
    let home = Home::at(tmp.path().join("home"));
    home.ensure().unwrap();
    let tx_path = home.tx().join("2026-05.org");
    write(
        &tx_path,
        "#+title: orgasmic tx 2026-05\n#+orgasmic_version: 1\n\n* TX malformed historical record\n:PROPERTIES:\nnot-a-property\n",
    );

    let result = Daemon::run(home, test_options()).await;
    assert!(result.is_err(), "corrupted historical tx should block boot");
    let err = result.err().unwrap();
    let startup = err
        .downcast_ref::<orgasmic_daemon::HistoricalTxStartupError>()
        .expect("historical tx startup error");
    assert_eq!(startup.path, tx_path);
    assert_eq!(startup.line, Some(6));
    let message = err.to_string();
    assert!(message.contains("historical tx parse error blocks daemon start"));
    assert!(message.contains(&startup.path.display().to_string()));
    assert!(message.contains(":6:"));
    assert!(message.contains("Revert the modified tx file"));
    assert!(message.contains("explicit tx reseal"));
}

#[tokio::test]
async fn corrupted_working_file_does_not_block_start() {
    let tmp = tempfile::tempdir().unwrap();
    let home = Home::at(tmp.path().join("home"));
    home.ensure().unwrap();
    let project_root = tmp.path().join("proj");
    seed_project(&home, &project_root, "proj-working");
    let sprint = project_root.join(".orgasmic/tasks/backlog.org");
    write(
        &sprint,
        "#+title: sprint\n#+orgasmic_version: 1\n\n* BACKLOG TASK-BAD Broken task\n:PROPERTIES:\nno-end-marker\n",
    );

    let running = boot(home.clone()).await;
    let token = read_token(&home);
    let client = reqwest::Client::new();
    let resp = client
        .get(format!("http://{}/api/graph/parse-errors", running.addr))
        .bearer_auth(&token)
        .send()
        .await
        .unwrap();
    assert!(resp.status().is_success(), "parse errors endpoint");
    let errors: serde_json::Value = resp.json().await.unwrap();
    let arr = errors.as_array().unwrap();
    assert!(arr.iter().any(|error| {
        error["path"].as_str() == Some(sprint.to_string_lossy().as_ref())
            && error["kind"].as_str() == Some("working-file")
    }));
    assert!(!arr
        .iter()
        .any(|error| error["kind"].as_str() == Some("historical-tx")));

    let _ = running.shutdown.send(());
    let _ = running.join.await;
}

#[tokio::test]
async fn end_to_end_tx_record_appends_to_disk() {
    let tmp = tempfile::tempdir().unwrap();
    let home = Home::at(tmp.path().join("home"));
    let running = boot(home.clone()).await;
    let token = read_token(&home);
    let client = reqwest::Client::new();

    let body = serde_json::json!({
        "request_id": "req-int-1",
        "type": "manager.action",
        "actor": "tester@example.com",
        "machine": "ci.local",
        "reason": "integration test",
        "extra": [["FROM_STATE", "ready"], ["TO_STATE", "in_progress"]],
    });
    let resp = client
        .post(format!("http://{}/api/tx", running.addr))
        .bearer_auth(&token)
        .json(&body)
        .send()
        .await
        .unwrap();
    assert!(resp.status().is_success(), "post /tx: {}", resp.status());
    let json: serde_json::Value = resp.json().await.unwrap();
    let tx_id = json["tx_id"].as_str().unwrap().to_string();
    let tx_path_str = json["tx_path"].as_str().unwrap().to_string();
    let tx_path = std::path::PathBuf::from(&tx_path_str);
    assert!(tx_path.exists(), "tx file should exist at {tx_path_str}");
    let raw = std::fs::read_to_string(&tx_path).unwrap();
    assert!(raw.contains(&tx_id), "tx file should contain {tx_id}");
    assert!(raw.contains(":FROM_STATE:"), "extras should round-trip");

    // Replaying with the same request_id is a no-op.
    let resp = client
        .post(format!("http://{}/api/tx", running.addr))
        .bearer_auth(&token)
        .json(&body)
        .send()
        .await
        .unwrap();
    assert!(resp.status().is_success());
    let raw = std::fs::read_to_string(&tx_path).unwrap();
    assert_eq!(
        raw.matches(&tx_id).count(),
        1,
        "idempotent replay must not double-append"
    );

    // /tx listing reflects the appended entry.
    let resp = client
        .get(format!("http://{}/api/tx?limit=10", running.addr))
        .bearer_auth(&token)
        .send()
        .await
        .unwrap();
    let list: serde_json::Value = resp.json().await.unwrap();
    let arr = list.as_array().expect("array");
    assert!(
        arr.iter().any(|item| item["entry"]["tx_id"] == tx_id),
        "tx list must contain the new entry"
    );

    let _ = running.shutdown.send(());
    let _ = running.join.await;
}

#[tokio::test]
async fn project_tx_record_routes_to_project_and_uses_project_sequence() {
    let tmp = tempfile::tempdir().unwrap();
    let home = Home::at(tmp.path().join("home"));
    home.ensure().unwrap();
    write(
        &home.config(),
        "bind_host: 127.0.0.1\nbind_port: 4848\ntx:\n  commit_to_project: true\nmanager:\n  actor: manager@example.com\n",
    );
    let project_root = tmp.path().join("proj");
    seed_project(&home, &project_root, "orgasmic");
    write(
        &project_root.join(".orgasmic/tx/2026-05.org"),
        "#+title: orgasmic project tx 2026-05\n#+orgasmic_version: 1\n\n* TX 2026-05-21 22:10 manager.action orgasmic\n:PROPERTIES:\n:TX_ID:        tx-20260521-orgasmic-0036\n:TIME:         [2026-05-21 Thu 22:10:00]\n:TYPE:         manager.action\n:ACTOR:        prior@example.com\n:MACHINE:      host.local\n:PROJECT:      orgasmic\n:END:\n",
    );

    let running = boot(home.clone()).await;
    let token = read_token(&home);
    let client = reqwest::Client::new();
    let resp = client
        .post(format!("http://{}/api/tx", running.addr))
        .bearer_auth(&token)
        .json(&serde_json::json!({
            "request_id": "req-project-routing",
            "type": "manager.action",
            "machine": "ci.local",
            "project": "orgasmic",
            "reason": "project tx route",
        }))
        .send()
        .await
        .unwrap();
    assert!(resp.status().is_success(), "post /tx: {}", resp.status());
    let json: serde_json::Value = resp.json().await.unwrap();
    let tx_id = json["tx_id"].as_str().unwrap();
    let tx_path = std::path::PathBuf::from(json["tx_path"].as_str().unwrap());
    assert!(tx_path.starts_with(project_root.join(".orgasmic/tx")));
    assert!(
        tx_id.ends_with("-orgasmic-0037"),
        "project tx id should continue existing sequence, got {tx_id}"
    );
    let raw = std::fs::read_to_string(&tx_path).unwrap();
    assert!(raw.contains(":ACTOR:        manager@example.com"));
    assert!(raw.contains(":TX_ID:        tx-"));

    let resp = client
        .get(format!(
            "http://{}/api/tx?project=orgasmic&limit=10",
            running.addr
        ))
        .bearer_auth(&token)
        .send()
        .await
        .unwrap();
    assert!(resp.status().is_success(), "get /tx: {}", resp.status());
    let list: serde_json::Value = resp.json().await.unwrap();
    assert!(list
        .as_array()
        .unwrap()
        .iter()
        .any(|item| item["entry"]["tx_id"] == tx_id));

    let _ = running.shutdown.send(());
    let _ = running.join.await;
}

#[tokio::test]
async fn project_tx_record_errors_when_project_missing_from_board() {
    let tmp = tempfile::tempdir().unwrap();
    let home = Home::at(tmp.path().join("home"));
    home.ensure().unwrap();
    write(
        &home.config(),
        "bind_host: 127.0.0.1\nbind_port: 4848\ntx:\n  commit_to_project: true\n",
    );
    let home_tx = home
        .tx()
        .join(format!("{}.org", chrono::Utc::now().format("%Y-%m")));

    let running = boot(home.clone()).await;
    let token = read_token(&home);
    let client = reqwest::Client::new();
    let resp = client
        .post(format!("http://{}/api/tx", running.addr))
        .bearer_auth(&token)
        .json(&serde_json::json!({
            "request_id": "req-project-missing",
            "type": "manager.action",
            "machine": "ci.local",
            "project": "missing-project",
            "reason": "missing project route",
        }))
        .send()
        .await
        .unwrap();

    assert_eq!(resp.status(), reqwest::StatusCode::BAD_REQUEST);
    let body = resp.text().await.unwrap();
    assert_path_free_error(&body, "project could not be resolved", &[tmp.path()]);
    assert!(
        !body.contains("tx_id"),
        "error response must not return a tx_id: {body}"
    );
    assert!(
        !home_tx.exists(),
        "unresolvable project must not fall back to home tx at {}",
        home_tx.display()
    );

    let _ = running.shutdown.send(());
    let _ = running.join.await;
}

#[tokio::test]
async fn project_tx_record_errors_when_project_root_invalid() {
    let tmp = tempfile::tempdir().unwrap();
    let home = Home::at(tmp.path().join("home"));
    home.ensure().unwrap();
    write(
        &home.config(),
        "bind_host: 127.0.0.1\nbind_port: 4848\ntx:\n  commit_to_project: true\n",
    );
    let invalid_root = tmp.path().join("invalid-root");
    std::fs::create_dir_all(&invalid_root).unwrap();
    write(
        &home.board(),
        format!(
            "#+title: orgasmic board\n#+orgasmic_version: 1\n\n* PROJECT broken-project\n:PROPERTIES:\n:ID:               broken-project\n:PATH:             {}\n:BRANCH:           main\n:END:\n",
            invalid_root.display()
        ),
    );
    let home_tx = home
        .tx()
        .join(format!("{}.org", chrono::Utc::now().format("%Y-%m")));
    let invalid_project_tx = invalid_root
        .join(".orgasmic")
        .join("tx")
        .join(format!("{}.org", chrono::Utc::now().format("%Y-%m")));

    let running = boot(home.clone()).await;
    let token = read_token(&home);
    let client = reqwest::Client::new();
    let resp = client
        .post(format!("http://{}/api/tx", running.addr))
        .bearer_auth(&token)
        .json(&serde_json::json!({
            "request_id": "req-invalid-root",
            "type": "manager.action",
            "machine": "ci.local",
            "project": "broken-project",
            "reason": "invalid project route",
        }))
        .send()
        .await
        .unwrap();

    assert_eq!(resp.status(), reqwest::StatusCode::BAD_REQUEST);
    let body = resp.text().await.unwrap();
    assert_path_free_error(&body, "project could not be resolved", &[tmp.path()]);
    assert!(
        !body.contains("tx_id"),
        "error response must not return a tx_id: {body}"
    );
    assert!(
        !home_tx.exists(),
        "invalid project root must not fall back to home tx at {}",
        home_tx.display()
    );
    assert!(
        !invalid_project_tx.exists(),
        "invalid project root must not create project tx at {}",
        invalid_project_tx.display()
    );

    let _ = running.shutdown.send(());
    let _ = running.join.await;
}

#[tokio::test]
async fn task_comment_appends_and_activity_returns_tail_entry() {
    let tmp = tempfile::tempdir().unwrap();
    let home = Home::at(tmp.path().join("home"));
    home.ensure().unwrap();
    let project_root = tmp.path().join("proj");
    seed_project(&home, &project_root, "orgasmic");

    let running = boot(home.clone()).await;
    let token = read_token(&home);
    let client = reqwest::Client::new();
    let resp = client
        .post(format!(
            "http://{}/api/tasks/TASK-PRE/comments",
            running.addr
        ))
        .bearer_auth(&token)
        .json(&serde_json::json!({
            "actor": "agent.implementer",
            "body": "first line\nsecond line",
            "artifacts": ["dec_035", "TASK-PRE"],
            "request_id": "comment-activity-test"
        }))
        .send()
        .await
        .unwrap();
    assert!(
        resp.status().is_success(),
        "post comment: {}",
        resp.status()
    );
    let created: serde_json::Value = resp.json().await.unwrap();
    let tx_id = created["tx_id"].as_str().unwrap().to_string();

    let resp = client
        .get(format!(
            "http://{}/api/tasks/TASK-PRE/activity",
            running.addr
        ))
        .bearer_auth(&token)
        .send()
        .await
        .unwrap();
    assert!(
        resp.status().is_success(),
        "get activity: {}",
        resp.status()
    );
    let activity: serde_json::Value = resp.json().await.unwrap();
    let arr = activity.as_array().unwrap();
    let tail = arr.last().expect("activity tail");
    assert_eq!(tail["tx_id"], tx_id);
    assert_eq!(tail["kind"], "comment");
    assert_eq!(tail["actor"], "agent.implementer");
    assert_eq!(tail["body"], "first line\nsecond line");

    let _ = running.shutdown.send(());
    let _ = running.join.await;
}

#[tokio::test]
async fn create_subtask_rewrites_sprint_and_indexes_edge() {
    let tmp = tempfile::tempdir().unwrap();
    let home = Home::at(tmp.path().join("home"));
    home.ensure().unwrap();
    let project_root = tmp.path().join("proj");
    seed_project(&home, &project_root, "orgasmic");
    write(
        &project_root.join(".orgasmic/tasks/backlog.org"),
        "#+title: sprint\n#+orgasmic_version: 1\n\n* BACKLOG TASK-001 Parent task\n:PROPERTIES:\n:ID:               TASK-001\n:END:\n",
    );

    let running = boot(home.clone()).await;
    let token = read_token(&home);
    let client = reqwest::Client::new();
    let resp = client
        .post(format!(
            "http://{}/api/tasks/TASK-001/subtasks",
            running.addr
        ))
        .bearer_auth(&token)
        .json(&serde_json::json!({
            "title": "Child task",
            "kind": "implementer",
            "description": "Child details.",
            "request_id": "subtask-create-test"
        }))
        .send()
        .await
        .unwrap();
    let status = resp.status();
    let body: serde_json::Value = resp.json().await.unwrap();
    assert!(status.is_success(), "create subtask: {status} {body}");
    assert_eq!(body["id"], "TASK-001.1");
    assert!(body["heading"].as_str().unwrap().contains("TASK-001.1"));

    let raw = std::fs::read_to_string(project_root.join(".orgasmic/tasks/backlog.org")).unwrap();
    assert!(raw.contains("* BACKLOG TASK-001.1 Child task"));
    assert!(raw.contains(":ID:               TASK-001.1"));

    let resp = client
        .get(format!("http://{}/api/projects/orgasmic", running.addr))
        .bearer_auth(&token)
        .send()
        .await
        .unwrap();
    let project: serde_json::Value = resp.json().await.unwrap();
    assert_eq!(project["subtasks"]["TASK-001"][0], "TASK-001.1");

    let _ = running.shutdown.send(());
    let _ = running.join.await;
}

#[tokio::test]
async fn orphan_parent_task_surfaces_via_parse_errors_endpoint() {
    let tmp = tempfile::tempdir().unwrap();
    let home = Home::at(tmp.path().join("home"));
    home.ensure().unwrap();
    let project_root = tmp.path().join("proj");
    seed_project(&home, &project_root, "orgasmic");
    write(
        &project_root.join(".orgasmic/tasks/backlog.org"),
        "#+title: sprint\n#+orgasmic_version: 1\n\n* BACKLOG TASK-999.1 Orphan child\n:PROPERTIES:\n:ID:               TASK-999.1\n:END:\n",
    );

    let running = boot(home.clone()).await;
    let token = read_token(&home);
    let client = reqwest::Client::new();
    let resp = client
        .get(format!("http://{}/api/graph/parse-errors", running.addr))
        .bearer_auth(&token)
        .send()
        .await
        .unwrap();
    assert!(
        resp.status().is_success(),
        "parse errors: {}",
        resp.status()
    );
    let errors: serde_json::Value = resp.json().await.unwrap();
    assert!(errors.as_array().unwrap().iter().any(|error| {
        error["message"]
            .as_str()
            .unwrap_or("")
            .contains("orphan derived parent TASK-999")
    }));

    let _ = running.shutdown.send(());
    let _ = running.join.await;
}

#[tokio::test]
async fn release_inactive_run_returns_not_found() {
    let tmp = tempfile::tempdir().unwrap();
    let home = Home::at(tmp.path().join("home"));
    let running = boot(home.clone()).await;
    let token = read_token(&home);
    let client = reqwest::Client::new();
    let resp = client
        .post(format!(
            "http://{}/api/runs/run-missing/release",
            running.addr
        ))
        .bearer_auth(&token)
        .json(&serde_json::json!({}))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), reqwest::StatusCode::NOT_FOUND);

    let _ = running.shutdown.send(());
    let _ = running.join.await;
}

#[tokio::test]
async fn commit_to_project_false_keeps_home_tx_and_uses_git_actor() {
    let tmp = tempfile::tempdir().unwrap();
    let home = Home::at(tmp.path().join("home"));
    home.ensure().unwrap();
    write(
        &home.config(),
        "bind_host: 127.0.0.1\nbind_port: 4848\ntx:\n  commit_to_project: false\n",
    );
    let project_root = tmp.path().join("proj");
    seed_project(&home, &project_root, "orgasmic");
    Command::new("git")
        .arg("init")
        .current_dir(&project_root)
        .output()
        .expect("git init");
    Command::new("git")
        .args(["config", "user.email", "git@example.com"])
        .current_dir(&project_root)
        .output()
        .expect("git config user.email");

    let running = boot(home.clone()).await;
    let token = read_token(&home);
    let client = reqwest::Client::new();
    let resp = client
        .post(format!("http://{}/api/tx", running.addr))
        .bearer_auth(&token)
        .json(&serde_json::json!({
            "request_id": "req-home-routing",
            "type": "manager.action",
            "machine": "ci.local",
            "project": "orgasmic",
            "reason": "home tx route",
        }))
        .send()
        .await
        .unwrap();
    assert!(resp.status().is_success(), "post /tx: {}", resp.status());
    let json: serde_json::Value = resp.json().await.unwrap();
    let tx_id = json["tx_id"].as_str().unwrap();
    let tx_path = std::path::PathBuf::from(json["tx_path"].as_str().unwrap());
    assert!(tx_path.starts_with(home.tx()));
    assert!(!tx_id.contains("-proj-"));
    let raw = std::fs::read_to_string(&tx_path).unwrap();
    assert!(raw.contains(":ACTOR:        git@example.com"));

    let _ = running.shutdown.send(());
    let _ = running.join.await;
}

#[tokio::test]
async fn daemon_honors_config_bind_port_without_cli_override() {
    let tmp = tempfile::tempdir().unwrap();
    let home = Home::at(tmp.path().join("home"));
    std::fs::create_dir_all(&home.root).unwrap();
    let socket = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let port = socket.local_addr().unwrap().port();
    drop(socket);
    write(
        &home.config(),
        format!("bind_host: 127.0.0.1\nbind_port: {port}\n"),
    );
    let running = Daemon::run(
        home,
        DaemonOptions {
            fs_watcher_enabled: false,
            ..DaemonOptions::default()
        },
    )
    .await
    .expect("boot daemon with config port");
    assert_eq!(running.addr.port(), port);
    let _ = running.shutdown.send(());
    let _ = running.join.await;
}

#[tokio::test]
async fn end_to_end_ws_emits_event_after_tx_post() {
    let tmp = tempfile::tempdir().unwrap();
    let home = Home::at(tmp.path().join("home"));
    let running = boot(home.clone()).await;
    let token = read_token(&home);
    let addr = running.addr;

    // Connect WS first so we don't miss the daemon event.
    let url = format!("ws://{}/api/ws?topic=daemon,run,board", addr);
    let request = tokio_tungstenite::tungstenite::http::Request::builder()
        .uri(&url)
        .header("Authorization", format!("Bearer {token}"))
        .header(
            "Host",
            tokio_tungstenite::tungstenite::http::HeaderValue::from_str(&addr.to_string()).unwrap(),
        )
        .header("Upgrade", "websocket")
        .header("Connection", "Upgrade")
        .header("Sec-WebSocket-Version", "13")
        .header(
            "Sec-WebSocket-Key",
            tokio_tungstenite::tungstenite::handshake::client::generate_key(),
        )
        .body(())
        .unwrap();
    let (mut ws, _) = tokio_tungstenite::connect_async(request)
        .await
        .expect("ws connect");

    // Trigger a tx append to generate a daemon event.
    let client = reqwest::Client::new();
    let resp = client
        .post(format!("http://{addr}/api/tx"))
        .bearer_auth(&token)
        .json(&serde_json::json!({
            "type": "ws.smoke",
            "reason": "trigger event for ws test"
        }))
        .send()
        .await
        .unwrap();
    assert!(resp.status().is_success());

    let mut saw = false;
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    while std::time::Instant::now() < deadline {
        match tokio::time::timeout(Duration::from_millis(500), ws.next()).await {
            Ok(Some(Ok(msg))) => {
                if let tokio_tungstenite::tungstenite::Message::Text(text) = msg {
                    let v: serde_json::Value = serde_json::from_str(&text).unwrap();
                    if v["payload"]["kind"] == "tx_appended"
                        || v["payload"]["kind"] == "daemon_started"
                        || v["payload"]["kind"] == "daemon_heartbeat"
                    {
                        saw = true;
                        break;
                    }
                }
            }
            _ => continue,
        }
    }
    assert!(
        saw,
        "WS subscriber should observe at least one daemon event"
    );

    let _ = ws
        .send(tokio_tungstenite::tungstenite::Message::Close(None))
        .await;
    let _ = running.shutdown.send(());
    let _ = running.join.await;
}

#[tokio::test]
async fn rebuilt_index_serves_board_before_normal_reads() {
    let tmp = tempfile::tempdir().unwrap();
    let home = Home::at(tmp.path().join("home"));
    home.ensure().unwrap();
    // Seed a board entry before booting.
    let project_root = tmp.path().join("proj");
    write(
        &project_root.join(".orgasmic/project.org"),
        "#+title: x\n#+orgasmic_version: 1\n\n* PROJECT proj-pre\n:PROPERTIES:\n:ID:               proj-pre\n:END:\n",
    );
    write(
        &project_root.join(".orgasmic/tasks/backlog.org"),
        "#+title: x\n#+orgasmic_version: 1\n\n* BACKLOG TASK-PRE Pre-boot task :work:\n:PROPERTIES:\n:ID:               TASK-PRE\n:END:\n",
    );
    std::fs::write(
        home.board(),
        format!(
            "#+title: orgasmic board\n#+orgasmic_version: 1\n\n* PROJECT proj-pre\n:PROPERTIES:\n:ID:               proj-pre\n:PATH:             {}\n:BRANCH:           main\n:END:\n",
            project_root.display()
        ),
    )
    .unwrap();

    let running = boot(home.clone()).await;
    let token = read_token(&home);
    let client = reqwest::Client::new();

    // The very first GET /board after boot should reflect the seeded entry,
    // proving AC #1 (rebuild before serving normal reads).
    let resp = client
        .get(format!("http://{}/api/board", running.addr))
        .bearer_auth(&token)
        .send()
        .await
        .unwrap();
    assert!(resp.status().is_success());
    let board: serde_json::Value = resp.json().await.unwrap();
    let arr = board.as_array().unwrap();
    assert_eq!(arr.len(), 1);
    assert_eq!(arr[0]["id"], "proj-pre");

    let resp = client
        .get(format!(
            "http://{}/api/projects/proj-pre/tasks",
            running.addr
        ))
        .bearer_auth(&token)
        .send()
        .await
        .unwrap();
    let tasks: serde_json::Value = resp.json().await.unwrap();
    assert_eq!(tasks.as_array().unwrap().len(), 1);
    assert_eq!(tasks[0]["id"], "TASK-PRE");

    let _ = running.shutdown.send(());
    let _ = running.join.await;
}

#[tokio::test]
async fn task_list_route_serializes_depends_on_arrays() {
    let tmp = tempfile::tempdir().unwrap();
    let home = Home::at(tmp.path().join("home"));
    home.ensure().unwrap();
    let project_root = tmp.path().join("proj");
    seed_project(&home, &project_root, "proj-pre");
    write(
        &project_root.join(".orgasmic/tasks/backlog.org"),
        "#+title: sprint\n#+orgasmic_version: 1\n\n* BACKLOG TASK-BKC12 Blocked task :work:\n:PROPERTIES:\n:ID:               TASK-BKC12\n:DEPENDS_ON:       TASK-A TASK-B\n:END:\n\n* BACKLOG TASK-A First dependency :work:\n:PROPERTIES:\n:ID:               TASK-A\n:END:\n\n* BACKLOG TASK-B Second dependency :work:\n:PROPERTIES:\n:ID:               TASK-B\n:END:\n\n* BACKLOG TASK-OPEN Open task :work:\n:PROPERTIES:\n:ID:               TASK-OPEN\n:END:\n",
    );

    let running = boot(home.clone()).await;
    let token = read_token(&home);
    let client = reqwest::Client::new();
    let resp = client
        .get(format!(
            "http://{}/api/projects/proj-pre/tasks",
            running.addr
        ))
        .bearer_auth(&token)
        .send()
        .await
        .unwrap();
    assert!(resp.status().is_success());
    let tasks: serde_json::Value = resp.json().await.unwrap();
    assert_eq!(
        tasks[0]["depends_on"],
        serde_json::json!(["TASK-A", "TASK-B"])
    );
    assert_eq!(tasks[1]["depends_on"], serde_json::json!([]));

    let _ = running.shutdown.send(());
    let _ = running.join.await;
}

#[tokio::test]
async fn graph_edges_route_exposes_forward_and_inverse_queries() {
    let tmp = tempfile::tempdir().unwrap();
    let home = Home::at(tmp.path().join("home"));
    home.ensure().unwrap();
    let project_root = tmp.path().join("proj");
    seed_project(&home, &project_root, "proj-pre");
    write(
        &project_root.join(".orgasmic/tasks/backlog.org"),
        "#+title: sprint\n#+orgasmic_version: 1\n\n* BACKLOG TASK-WRK12 Work task :work:\n:PROPERTIES:\n:ID:               TASK-WRK12\n:DEPENDS_ON:       TASK-BAS12\n:IMPLEMENTS:       arch_APP12\n:PRODUCES:         crates/example.rs\n:END:\n\n* BACKLOG TASK-BAS12 Base task :work:\n:PROPERTIES:\n:ID:               TASK-BAS12\n:END:\n",
    );
    write(
        &project_root.join(".orgasmic/decisions.org"),
        "#+title: decisions\n#+orgasmic_version: 1\n\n* dec_KEEP1 Keep it\n:PROPERTIES:\n:ID:               dec_KEEP1\n:END:\n",
    );
    write(
        &project_root.join(".orgasmic/architecture.org"),
        "#+title: architecture\n#+orgasmic_version: 1\n\n* arch_APP12 App\n:PROPERTIES:\n:ID:               arch_APP12\n:MOTIVATED_BY:     dec_KEEP1\n:END:\n",
    );

    let running = boot(home.clone()).await;
    let token = read_token(&home);
    let client = reqwest::Client::new();

    let resp = client
        .get(format!(
            "http://{}/api/graph/edges?project=proj-pre&node=TASK-WRK12&dir=out",
            running.addr
        ))
        .bearer_auth(&token)
        .send()
        .await
        .unwrap();
    assert!(resp.status().is_success());
    let forward: serde_json::Value = resp.json().await.unwrap();
    assert!(forward.as_array().unwrap().iter().any(|edge| {
        edge["kind"] == "depends_on" && edge["from"] == "TASK-WRK12" && edge["to"] == "TASK-BAS12"
    }));

    let resp = client
        .get(format!(
            "http://{}/api/graph/edges?project=proj-pre&node=arch_APP12&relation=implemented_by",
            running.addr
        ))
        .bearer_auth(&token)
        .send()
        .await
        .unwrap();
    assert!(resp.status().is_success());
    let inverse: serde_json::Value = resp.json().await.unwrap();
    assert_eq!(
        inverse,
        serde_json::json!([{"kind": "implements", "from": "TASK-WRK12", "to": "arch_APP12"}])
    );

    let resp = client
        .get(format!(
            "http://{}/api/graph/edges?project=proj-pre&node=dec_KEEP1&relation=motivates",
            running.addr
        ))
        .bearer_auth(&token)
        .send()
        .await
        .unwrap();
    assert!(resp.status().is_success());
    let motivates: serde_json::Value = resp.json().await.unwrap();
    assert_eq!(
        motivates,
        serde_json::json!([{"kind": "motivated_by", "from": "arch_APP12", "to": "dec_KEEP1"}])
    );

    let _ = running.shutdown.send(());
    let _ = running.join.await;
}

#[tokio::test]
async fn daemon_watcher_refreshes_after_direct_sprint_write() {
    let tmp = tempfile::tempdir().unwrap();
    let home = Home::at(tmp.path().join("home"));
    home.ensure().unwrap();
    let project_root = tmp.path().join("proj");
    seed_project(&home, &project_root, "proj-pre");

    // This test asserts on watcher-driven refresh, so it boots with the real
    // `notify` backend (unlike the suite default).
    let running = boot_with_watcher(home.clone()).await;
    let token = read_token(&home);
    let client = reqwest::Client::new();
    tokio::time::sleep(Duration::from_millis(300)).await;

    write(
        &project_root.join(".orgasmic/tasks/backlog.org"),
        "#+title: sprint\n#+orgasmic_version: 1\n\n* IN_PROGRESS TASK-PRE Pre-boot task :work:\n:PROPERTIES:\n:ID:               TASK-PRE\n:END:\n",
    );

    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    let mut refreshed = false;
    while tokio::time::Instant::now() < deadline {
        let task: serde_json::Value = client
            .get(format!(
                "http://{}/api/projects/proj-pre/tasks/TASK-PRE",
                running.addr
            ))
            .bearer_auth(&token)
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        if task["lifecycle_stage"] == "in_progress" {
            refreshed = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }

    let _ = running.shutdown.send(());
    let _ = running.join.await;
    assert!(
        refreshed,
        "direct std::fs::write to backlog.org should refresh project index"
    );
}

#[tokio::test]
async fn stub_routes_return_501_with_tracking_task() {
    let tmp = tempfile::tempdir().unwrap();
    let home = Home::at(tmp.path().join("home"));
    let running = boot(home.clone()).await;
    let token = read_token(&home);
    let client = reqwest::Client::new();

    for route in &["/api/graph/nodes", "/api/runs"] {
        let resp = client
            .post(format!("http://{}{}", running.addr, route))
            .bearer_auth(&token)
            .json(&serde_json::json!({}))
            .send()
            .await
            .unwrap();
        assert_eq!(
            resp.status(),
            reqwest::StatusCode::NOT_IMPLEMENTED,
            "{route} should return 501"
        );
        let body: serde_json::Value = resp.json().await.unwrap();
        assert!(
            body["tracked_by"].as_str().is_some(),
            "{route} 501 payload must name the tracking task"
        );
    }

    let _ = running.shutdown.send(());
    let _ = running.join.await;
}

#[tokio::test]
async fn task_008_graph_routes_are_real() {
    let tmp = tempfile::tempdir().unwrap();
    let home = Home::at(tmp.path().join("home"));
    home.ensure().unwrap();
    symlink_repo_source(&home);
    let project_root = tmp.path().join("proj");
    seed_project(&home, &project_root, "orgasmic");
    write(
        &project_root.join(".orgasmic/decisions.org"),
        "#+title: decisions\n#+orgasmic_version: 1\n\n* dec_001 Choice :product-scope:\n:PROPERTIES:\n:ID:                 dec_001\n:DECIDES:            dec_002\n:GLOSSARY_REFS:      term-a\n:END:\n** Context\nA test decision.\n** Decision\nChoose option a.\n** Consequences\nNone notable.\n",
    );
    write(
        &project_root.join(".orgasmic/architecture.org"),
        "#+title: architecture\n#+orgasmic_version: 1\n\n* arch_001 Component\n:PROPERTIES:\n:ID:                 arch_001\n:DECIDES:            dec_001\n:GLOSSARY_REFS:      term-a\n:INTERFACE:          read write\n:CONSTRAINTS:        conservative\n:DEPENDS_ON:\n:END:\n",
    );
    write(
        &project_root.join(".orgasmic/glossary.org"),
        "#+title: glossary\n#+orgasmic_version: 1\n\n* term:term-a Term A\n:PROPERTIES:\n:ID:                 term-a\n:CANONICAL:          term a\n:RELATES_TO:         dec_001\n:DEFINITION:         A test term.\n:END:\n",
    );
    let running = boot(home.clone()).await;
    let token = read_token(&home);
    let client = reqwest::Client::new();

    for route in &[
        "/api/graph/nodes?project=orgasmic",
        "/api/decisions?project=orgasmic",
        "/api/architecture?project=orgasmic",
        "/api/glossary?project=orgasmic",
    ] {
        let resp = client
            .get(format!("http://{}{}", running.addr, route))
            .bearer_auth(&token)
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), reqwest::StatusCode::OK, "{route}");
    }

    let resp = client
        .get(format!(
            "http://{}/api/decisions/dec_001?project=orgasmic",
            running.addr
        ))
        .bearer_auth(&token)
        .send()
        .await
        .unwrap();
    assert!(resp.status().is_success());
    let decision: serde_json::Value = resp.json().await.unwrap();
    assert_eq!(decision["id"], "dec_001");

    let resp = client
        .post(format!("http://{}/api/glossary", running.addr))
        .bearer_auth(&token)
        .json(&serde_json::json!({
            "project": "orgasmic",
            "request_id": "task-008-bad-glossary",
            "id": "bad-term",
            "title": "bad term",
            "properties": {
                "CANONICAL": "bad term",
                "DEFINITION": "Calls crates/orgasmic-core/src/schema.rs directly."
            }
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), reqwest::StatusCode::BAD_REQUEST);

    let resp = client
        .post(format!("http://{}/api/decisions", running.addr))
        .bearer_auth(&token)
        .json(&serde_json::json!({
            "project": "orgasmic",
            "request_id": "task-008-create-decision",
            "title": "Follow-up decision",
            "properties": {
                "DECIDES": "",
                "GLOSSARY_REFS": "term-a"
            }
        }))
        .send()
        .await
        .unwrap();
    assert!(
        resp.status().is_success(),
        "create decision: {}",
        resp.status()
    );
    let created: serde_json::Value = resp.json().await.unwrap();
    let minted_dec = created["id"].as_str().expect("minted decision id");
    assert!(minted_dec.starts_with("dec_"));
    assert!(!minted_dec["dec_".len()..]
        .chars()
        .all(|c| c.is_ascii_digit()));

    let resp = client
        .post(format!(
            "http://{}/api/decisions/{minted_dec}",
            running.addr
        ))
        .bearer_auth(&token)
        .json(&serde_json::json!({
            "project": "orgasmic",
            "request_id": "task-008-accept-decision",
            "action": "accept"
        }))
        .send()
        .await
        .unwrap();
    assert!(resp.status().is_success(), "accept: {}", resp.status());

    // Revising a decision no longer propagates staleness to architecture; the
    // removed consistency engine does not mutate architecture graph metadata.
    let resp = client
        .post(format!("http://{}/api/decisions/dec_001", running.addr))
        .bearer_auth(&token)
        .json(&serde_json::json!({
            "project": "orgasmic",
            "request_id": "task-008-revise-decision",
            "action": "revise"
        }))
        .send()
        .await
        .unwrap();
    assert!(resp.status().is_success(), "revise: {}", resp.status());
    let resp = client
        .get(format!(
            "http://{}/api/architecture/arch_001?project=orgasmic",
            running.addr
        ))
        .bearer_auth(&token)
        .send()
        .await
        .unwrap();
    let arch: serde_json::Value = resp.json().await.unwrap();
    assert_eq!(arch["id"], "arch_001");
    assert!(arch.get("status").is_none());

    let _ = running.shutdown.send(());
    let _ = running.join.await;
}

#[tokio::test]
async fn architecture_nodes_endpoint_returns_child_nodes_artifacts_and_edges() {
    let tmp = tempfile::tempdir().unwrap();
    let home = Home::at(tmp.path().join("home"));
    home.ensure().unwrap();
    let project_root = tmp.path().join("proj");
    seed_project(&home, &project_root, "orgasmic");
    write(
        &project_root.join(".orgasmic/architecture.org"),
        "#+title: architecture\n#+orgasmic_version: 1\n\n* arch_006 Daemon API\n:PROPERTIES:\n:ID:                 arch_006\n:DEPENDS_ON:\n:END:\n\n** Purpose\nOwns the daemon HTTP and materialized graph surface.\n\n** arch_006.1 HTTP router\n:PROPERTIES:\n:ID:                 arch_006.1\n:SOURCE_PATHS:       crates/orgasmic-daemon/src/api.rs\n:TESTS:              cargo test -p orgasmic-daemon\n:READS:              projection:materialized-index arch_006.2\n:WRITES:             file:tx\n:EXPOSES_WS:         socket:events\n:END:\nServes architecture graph data over HTTP.\n\n** arch_006.2 Materialized index\n:PROPERTIES:\n:ID:                 arch_006.2\n:SOURCE_PATHS:       crates/orgasmic-daemon/src/index.rs\n:WRITES:             projection:materialized-index\n:END:\n",
    );
    let running = boot(home.clone()).await;
    let token = read_token(&home);
    let client = reqwest::Client::new();

    let resp = client
        .get(format!(
            "http://{}/api/architecture?project=orgasmic",
            running.addr
        ))
        .bearer_auth(&token)
        .send()
        .await
        .unwrap();
    assert!(resp.status().is_success());
    let architecture: serde_json::Value = resp.json().await.unwrap();
    assert!(architecture.as_array().unwrap().iter().any(|node| {
        node["id"] == "arch_006"
            && node["description"]
                .as_str()
                .map(|description| {
                    description.trim() == "Owns the daemon HTTP and materialized graph surface."
                })
                .unwrap_or(false)
    }));

    let resp = client
        .get(format!(
            "http://{}/api/architecture/nodes?project=orgasmic",
            running.addr
        ))
        .bearer_auth(&token)
        .send()
        .await
        .unwrap();
    assert!(resp.status().is_success());
    let body: serde_json::Value = resp.json().await.unwrap();
    let nodes = body["nodes"].as_array().unwrap();
    assert!(nodes
        .iter()
        .any(|node| node["id"] == "arch_006" && node["kind"] == "arch"));
    assert!(nodes.iter().any(|node| {
        node["id"] == "arch_006.1"
            && node["parent_id"] == "arch_006"
            && node["source_paths"][0] == "crates/orgasmic-daemon/src/api.rs"
            && node["tests"][0] == "cargo test -p orgasmic-daemon"
    }));
    // Leaf nodes without :TESTS: omit the field entirely (skip_serializing_if).
    assert!(nodes
        .iter()
        .any(|node| node["id"] == "arch_006.2" && node["tests"].is_null()));
    assert!(nodes.iter().any(|node| {
        node["id"] == "projection:materialized-index"
            && node["kind"] == "artifact"
            && node["scheme"] == "projection"
            && node["name"] == "materialized-index"
    }));
    let edges = body["edges"].as_array().unwrap();
    assert!(edges.iter().any(|edge| {
        edge["kind"] == "reads" && edge["from"] == "arch_006.1" && edge["to"] == "arch_006.2"
    }));
    assert!(edges.iter().any(|edge| {
        edge["kind"] == "writes" && edge["from"] == "arch_006.1" && edge["to"] == "file:tx"
    }));

    let _ = running.shutdown.send(());
    let _ = running.join.await;
}

#[tokio::test]
async fn org_node_leaf_architecture_tests_round_trip() {
    let tmp = tempfile::tempdir().unwrap();
    let home = Home::at(tmp.path().join("home"));
    home.ensure().unwrap();
    let project_root = tmp.path().join("proj");
    seed_project(&home, &project_root, "orgasmic");
    write(
        &project_root.join(".orgasmic/architecture.org"),
        "#+title: architecture\n#+orgasmic_version: 1\n\n* arch_006 Daemon API\n:PROPERTIES:\n:ID:                 arch_006\n:END:\n\n** arch_006.3 Filesystem watcher\n:PROPERTIES:\n:ID:                 arch_006.3\n:SOURCE_PATHS:       crates/orgasmic-daemon/src/watcher.rs\n:TESTS:              cargo test -p orgasmic-daemon\n:END:\nDebounces fs events.\n",
    );
    let running = boot(home.clone()).await;
    let token = read_token(&home);
    let client = reqwest::Client::new();
    let base = format!("http://{}", running.addr);

    // Reading a nested leaf node resolves through recursive id lookup and
    // exposes its :TESTS: property.
    let doc: serde_json::Value = client
        .get(format!("{base}/api/org/node"))
        .bearer_auth(&token)
        .query(&[("project", "orgasmic"), ("id", "arch_006.3")])
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(doc["id"], "arch_006.3");
    let tests_prop = doc["properties"]
        .as_array()
        .unwrap()
        .iter()
        .find(|p| p["key"] == "TESTS")
        .expect("TESTS property present on leaf doc");
    assert_eq!(tests_prop["value"], "cargo test -p orgasmic-daemon");
    let base_version = doc["source"]["base_version"].as_str().unwrap().to_string();

    // Editing the leaf node's :TESTS: writes only that nested heading.
    let resp = client
        .post(format!("{base}/api/org/node/arch_006.3/edit"))
        .bearer_auth(&token)
        .json(&serde_json::json!({
            "project": "orgasmic",
            "request_id": "leaf-tests-edit",
            "base_version": base_version,
            "ops": [
                { "op": "set_property", "key": "TESTS", "value": "cargo test -p orgasmic-daemon; cargo clippy -p orgasmic-daemon" }
            ]
        }))
        .send()
        .await
        .unwrap();
    let status = resp.status();
    let updated: serde_json::Value = resp.json().await.unwrap();
    assert!(status.is_success(), "edit leaf node: {status} {updated}");
    let updated_tests = updated["properties"]
        .as_array()
        .unwrap()
        .iter()
        .find(|p| p["key"] == "TESTS")
        .unwrap();
    assert_eq!(
        updated_tests["value"],
        "cargo test -p orgasmic-daemon; cargo clippy -p orgasmic-daemon"
    );

    // On-disk file reflects the edit and keeps the parent heading intact.
    let on_disk = std::fs::read_to_string(project_root.join(".orgasmic/architecture.org")).unwrap();
    assert!(on_disk.contains("cargo clippy -p orgasmic-daemon"));
    assert!(on_disk.contains("* arch_006 Daemon API"));

    let _ = running.shutdown.send(());
    let _ = running.join.await;
}

#[tokio::test]
async fn org_node_handoff_get_section_and_property_round_trip() {
    let tmp = tempfile::tempdir().unwrap();
    let home = Home::at(tmp.path().join("home"));
    home.ensure().unwrap();
    let project_root = tmp.path().join("proj");
    seed_project(&home, &project_root, "handoffnode");
    write(
        &project_root.join(".orgasmic/tasks/handoff.org"),
        "#+title: Handoff\n#+orgasmic_version: 1\n\n\
         * HANDOFF current :handoff:\n\
         :PROPERTIES:\n\
         :ID:               handoff-current\n\
         :GOAL_ID:          goal-active\n\
         :LIVENESS:         base-sha\n\
         :END:\n\n\
         ** Done so far\nOriginal line.\n",
    );
    write(
        &project_root.join(".orgasmic/tasks/goal.org"),
        "#+title: Goal\n#+orgasmic_version: 1\n\n\
         * GOAL Active goal :goal:\n\
         :PROPERTIES:\n\
         :ID:               goal-active\n\
         :STATUS:           active\n\
         :END:\n\n\
         ** Statement\nShip the thing.\n",
    );

    let running = boot(home.clone()).await;
    let token = read_token(&home);
    let client = reqwest::Client::new();
    let base = format!("http://{}", running.addr);

    let doc: serde_json::Value = client
        .get(format!("{base}/api/org/node"))
        .bearer_auth(&token)
        .query(&[("project", "handoffnode"), ("id", "handoff-current")])
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(doc["id"], "handoff-current");
    assert_eq!(doc["kind"], "handoff");
    assert_eq!(doc["todo"], serde_json::Value::Null);
    assert_eq!(
        doc["tags"].as_array().unwrap(),
        &vec![serde_json::json!("handoff")]
    );
    assert_eq!(doc["source"]["file"], ".orgasmic/tasks/handoff.org");
    let base_version = doc["source"]["base_version"].as_str().unwrap().to_string();

    let section_resp = client
        .post(format!("{base}/api/org/node/handoff-current/edit"))
        .bearer_auth(&token)
        .json(&serde_json::json!({
            "project": "handoffnode",
            "base_version": base_version,
            "request_id": "handoff-section-edit",
            "ops": [
                { "op": "set_section_body", "title": "Done so far", "body": "Original line.\nAppended by test." }
            ]
        }))
        .send()
        .await
        .unwrap();
    let status = section_resp.status();
    let section_doc: serde_json::Value = section_resp.json().await.unwrap();
    assert!(
        status.is_success(),
        "handoff section edit failed: {status} {section_doc}"
    );
    assert_eq!(section_doc["kind"], "handoff");
    let done_section = section_doc["sections"]
        .as_array()
        .unwrap()
        .iter()
        .find(|section| section["title"] == "Done so far")
        .expect("Done so far section");
    assert_eq!(done_section["body"], "Original line.\nAppended by test.");
    let next_version = section_doc["source"]["base_version"]
        .as_str()
        .unwrap()
        .to_string();
    assert_ne!(next_version, base_version);

    let prop_resp = client
        .post(format!("{base}/api/org/node/handoff-current/edit"))
        .bearer_auth(&token)
        .json(&serde_json::json!({
            "project": "handoffnode",
            "base_version": next_version,
            "request_id": "handoff-property-edit",
            "ops": [
                { "op": "set_property", "key": "LIVENESS", "value": "next-sha" }
            ]
        }))
        .send()
        .await
        .unwrap();
    let status = prop_resp.status();
    let prop_doc: serde_json::Value = prop_resp.json().await.unwrap();
    assert!(
        status.is_success(),
        "handoff property edit failed: {status} {prop_doc}"
    );
    let liveness = prop_doc["properties"]
        .as_array()
        .unwrap()
        .iter()
        .find(|prop| prop["key"] == "LIVENESS")
        .expect("LIVENESS property");
    assert_eq!(liveness["value"], "next-sha");

    let goal_doc: serde_json::Value = client
        .get(format!("{base}/api/org/node"))
        .bearer_auth(&token)
        .query(&[("project", "handoffnode"), ("id", "goal-active")])
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(goal_doc["id"], "goal-active");
    assert_eq!(goal_doc["kind"], "goal");
    assert_eq!(goal_doc["source"]["file"], ".orgasmic/tasks/goal.org");

    let parse_errors: serde_json::Value = client
        .get(format!("{base}/api/graph/parse-errors"))
        .bearer_auth(&token)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(parse_errors.as_array().unwrap().len(), 0, "{parse_errors}");

    let on_disk =
        std::fs::read_to_string(project_root.join(".orgasmic/tasks/handoff.org")).unwrap();
    assert!(on_disk.contains("** Done so far\nOriginal line.\nAppended by test.\n"));
    assert!(on_disk.contains("next-sha"));

    let _ = running.shutdown.send(());
    let _ = running.join.await;
}

#[tokio::test]
async fn task_010_recovery_routes_classify_and_continue_runs() {
    let tmp = tempfile::tempdir().unwrap();
    let home = Home::at(tmp.path().join("home"));
    home.ensure().unwrap();
    let project_root = tmp.path().join("proj");
    seed_project(&home, &project_root, "orgasmic");
    // Per-run transcripts live per project under `.orgasmic/tmp/sessions/`.
    let sessions_dir = project_root.join(".orgasmic/tmp/sessions");
    write(
        &sessions_dir.join("run-old.jsonl"),
        r#"{"seq":0,"time":"2026-05-21T20:00:00Z","run_id":"run-old","runtime_id":"rt-old","boot_id":"boot-old","kind":"lifecycle","event":{"phase":"acquire","task_id":"TASK-OLD","kind":"implementer","worker_id":"implementer-claude-acp"}}
"#,
    );
    write(
        &sessions_dir.join("run-done.jsonl"),
        r#"{"seq":0,"time":"2026-05-21T20:00:00Z","run_id":"run-done","runtime_id":"rt-done","boot_id":"boot-old","kind":"lifecycle","event":{"phase":"acquire","task_id":"TASK-DONE","kind":"implementer","worker_id":"implementer-claude-acp"}}
{"seq":1,"time":"2026-05-21T20:01:00Z","run_id":"run-done","runtime_id":"rt-done","boot_id":"boot-old","kind":"lifecycle","event":{"phase":"release","reason":"done","outcome":"completed"}}
"#,
    );
    let running = boot(home.clone()).await;
    let token = read_token(&home);
    let client = reqwest::Client::new();

    let resp = client
        .get(format!("http://{}/api/recovery/status", running.addr))
        .bearer_auth(&token)
        .send()
        .await
        .unwrap();
    assert!(resp.status().is_success());
    let recovery: serde_json::Value = resp.json().await.unwrap();
    assert_eq!(recovery["interrupted_runs"][0]["run_id"], "run-old");
    assert_eq!(recovery["terminal_noop_runs"][0]["run_id"], "run-done");

    // run-old has no live tmux and no native metadata, so its only recovery
    // action is start_recovery_run (no best-effort discovery, dec_052).
    let actions = recovery["interrupted_runs"][0]["recovery_actions"]
        .as_array()
        .unwrap();
    assert_eq!(actions.len(), 1, "{recovery}");
    assert_eq!(actions[0]["kind"], "start_recovery_run");

    let resp = client
        .post(format!("http://{}/api/runs/run-old/recover", running.addr))
        .bearer_auth(&token)
        .json(&serde_json::json!({
            "action": "start_recovery_run",
            "project": "orgasmic",
            "force_inert": true,
            "request_id": "task-010-recover"
        }))
        .send()
        .await
        .unwrap();
    assert!(resp.status().is_success(), "recover: {}", resp.status());
    let continued: serde_json::Value = resp.json().await.unwrap();
    assert!(continued["run_id"].as_str().unwrap().starts_with("run-"));
    assert_eq!(continued["action"], "start_recovery_run");
    // The recovery prompt is staged for manual send, not auto-sent.
    assert!(
        continued["draft_prompt"]
            .as_str()
            .unwrap()
            .contains("run-old"),
        "recovery prompt should reference the prior run: {continued}"
    );

    let resp = client
        .post(format!("http://{}/api/daemon/restart", running.addr))
        .bearer_auth(&token)
        .json(&serde_json::json!({
            "reason": "integration restart",
            "request_id": "task-010-restart"
        }))
        .send()
        .await
        .unwrap();
    assert!(resp.status().is_success(), "restart: {}", resp.status());
    let restart: serde_json::Value = resp.json().await.unwrap();
    assert_eq!(restart["status"], "restart_requested");
    assert_eq!(restart["acquisition_paused"], true);
    // Restart no longer captures graph snapshots (snapshots were removed); the
    // response contract drops the `snapshots` field entirely.
    assert!(restart.get("snapshots").is_none());

    let _ = running.shutdown.send(());
    let _ = running.join.await;
}

#[tokio::test]
async fn task_007_content_routes_are_real() {
    let tmp = tempfile::tempdir().unwrap();
    let home = Home::at(tmp.path().join("home"));
    home.ensure().unwrap();
    symlink_repo_source(&home);
    let running = boot(home.clone()).await;
    let token = read_token(&home);
    let client = reqwest::Client::new();

    for route in &[
        "/api/workers",
        "/api/prompt-specs",
        "/api/prompt-specs/parts",
        "/api/prompt-specs/context-packs",
        "/api/skills",
        "/api/manager/state",
    ] {
        let resp = client
            .get(format!("http://{}{}", running.addr, route))
            .bearer_auth(&token)
            .send()
            .await
            .unwrap();
        assert_eq!(
            resp.status(),
            reqwest::StatusCode::OK,
            "{route} should be real"
        );
    }

    let resp = client
        .post(format!(
            "http://{}/api/prompt-specs/implementer/compile",
            running.addr
        ))
        .bearer_auth(&token)
        .json(&serde_json::json!({
            "values": {
                "task.id": "TASK-X",
                "task.title": "Title",
                "task.acceptance": "- [ ] done",
                "task.write_scope": "src/**",
                "task.read_scope": ".orgasmic/**",
                "project.name": "proj",
                "project.path": "/tmp/proj",
                "project.test_cmd": "cargo test",
                "skills.all": ""
            }
        }))
        .send()
        .await
        .unwrap();
    let status = resp.status();
    let text = resp.text().await.unwrap();
    assert!(status.is_success(), "compile: {status} {text}");
    let body: serde_json::Value = serde_json::from_str(&text).unwrap();
    assert!(body["diagnostics"].as_array().unwrap().is_empty());
    assert!(body["text"]
        .as_str()
        .unwrap()
        .contains("Prompt Spec: implementer"));

    let resp = client
        .post(format!(
            "http://{}/api/prompt-specs/griller/compile",
            running.addr
        ))
        .bearer_auth(&token)
        .json(&serde_json::json!({
            "values": {
                "task.id": "TASK-X",
                "task.title": "Title",
                "task.description": "Question target",
                "task.acceptance": "- [ ] findings",
                "task.write_scope": ".orgasmic/**",
                "task.read_scope": ".orgasmic/**",
                "project.name": "proj",
                "project.path": "/tmp/proj",
                "evidence.so_far": "",
                "worklog.tail": "",
                "skills.all": ""
            }
        }))
        .send()
        .await
        .unwrap();
    let status = resp.status();
    let text = resp.text().await.unwrap();
    assert!(status.is_success(), "griller compile: {status} {text}");
    let body: serde_json::Value = serde_json::from_str(&text).unwrap();
    assert_eq!(body["spec"]["id"], "griller");
    assert!(body["diagnostics"].as_array().unwrap().is_empty());
    assert!(body["text"]
        .as_str()
        .unwrap()
        .contains(".orgasmic/glossary.org"));

    let _ = running.shutdown.send(());
    let _ = running.join.await;
}

#[tokio::test]
async fn dispatch_subprocess_stream_json_classifies_live_then_terminal_noop() {
    let tmp = tempfile::tempdir().unwrap();
    let home = Home::at(tmp.path().join("home"));
    home.ensure().unwrap();
    symlink_repo_source(&home);
    let project_root = tmp.path().join("proj");
    write(
        &home.user().join("workers/implementer-codex-appserver.org"),
        "* WORKER implementer-codex-appserver\n:PROPERTIES:\n:ID:                          implementer-codex-appserver\n:KIND:             implementer\n:DRIVER:                      acp-ws\n:HARNESS:                     codex\n:PROVIDERS:                   openai\n:MODELS:                      gpt-5.5\n:REASONING_EFFORTS:           high xhigh\n:DEFAULT_PROVIDER:            openai\n:DEFAULT_MODEL:               gpt-5.5\n:DEFAULT_EFFORT:              high\n:LINKED_SKILLS:\n:APPLICABLE_STATES:           claimed, analyzing, implementing, testing, fixing\n:MAX_ITERATIONS:              1\n:CONTEXT_BUDGET:              4000\n:VERSION:                     1\n:END:\n\n** Persona\nTest dispatch worker.\n\n** Operating Rules\n- Keep the test run minimal.\n",
    );
    write(
        &project_root.join(".orgasmic/config.org"),
        "#+title: orgasmic\n#+orgasmic_version: 1\n\n* CONFIG orgasmic\n:PROPERTIES:\n:ID:                     orgasmic\n:PIPELINE:                 implementer-codex-appserver\n:END:\n",
    );
    write(
        &project_root.join(".orgasmic/tasks/backlog.org"),
        "#+title: sprint\n#+orgasmic_version: 1\n\n* BACKLOG TASK-DISPATCH-CLASS Classifier dispatch smoke\n:PROPERTIES:\n:ID:               TASK-DISPATCH-CLASS\n:END:\n",
    );
    write(
        &home.board(),
        format!(
            "#+title: orgasmic board\n#+orgasmic_version: 1\n\n* PROJECT orgasmic\n:PROPERTIES:\n:ID:               orgasmic\n:PATH:             {}\n:BRANCH:           main\n:END:\n",
            project_root.display()
        ),
    );
    let brief = tmp.path().join("brief.md");
    let worktree = tmp.path().join("worktree");
    let last = tmp.path().join("last.txt");
    let stdout = tmp.path().join("stdout.log");
    write(&brief, "classifier dispatch brief\n");
    std::fs::create_dir_all(&worktree).unwrap();

    let running = boot(home.clone()).await;
    let token = read_token(&home);
    let client = reqwest::Client::new();
    let dispatch = client
        .post(format!(
            "http://{}/api/projects/orgasmic/tasks/TASK-DISPATCH-CLASS/dispatch",
            running.addr
        ))
        .bearer_auth(&token)
        .json(&serde_json::json!({
            "kind": "implementer",
            "brief_path": brief,
            "worktree_path": worktree,
            "last_path": last,
            "stdout_path": stdout,
            "worker_id": "implementer-codex-appserver",
            "reason": "classifier integration test",
        }))
        .send()
        .await
        .unwrap();
    assert!(
        dispatch.status().is_success(),
        "dispatch: {}",
        dispatch.status()
    );
    let body: serde_json::Value = dispatch.json().await.unwrap();
    let run_id = body["run_id"].as_str().unwrap().to_string();

    let live_deadline = std::time::Instant::now() + Duration::from_secs(5);
    while std::time::Instant::now() < live_deadline {
        let runs = get_runs_json(&client, running.addr, &token).await;
        let live = runs["live"].as_array().unwrap();
        let interrupted = runs["interrupted"].as_array().unwrap();
        if live.iter().any(|run| run["run_id"] == run_id) {
            assert!(
                !interrupted.iter().any(|run| run["run_id"] == run_id),
                "live subprocess dispatch must not appear in interrupted: {runs}"
            );
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert!(
        get_runs_json(&client, running.addr, &token).await["live"]
            .as_array()
            .unwrap()
            .iter()
            .any(|run| run["run_id"] == run_id),
        "dispatch run should appear in live runs while active"
    );

    let release = client
        .post(format!(
            "http://{}/api/runs/{}/release",
            running.addr, run_id
        ))
        .bearer_auth(&token)
        .json(&serde_json::json!({
            "reason": "classifier integration release",
            "request_id": "dispatch-classifier-release"
        }))
        .send()
        .await
        .unwrap();
    assert!(
        release.status().is_success(),
        "release dispatch run: {}",
        release.status()
    );

    let terminal_deadline = std::time::Instant::now() + Duration::from_secs(10);
    while std::time::Instant::now() < terminal_deadline {
        let runs = get_runs_json(&client, running.addr, &token).await;
        let live = runs["live"].as_array().unwrap();
        let interrupted = runs["interrupted"].as_array().unwrap();
        let terminal = runs["terminal_noop"].as_array().unwrap();
        if !live.iter().any(|run| run["run_id"] == run_id)
            && terminal.iter().any(|run| run["run_id"] == run_id)
        {
            assert!(
                !interrupted.iter().any(|run| run["run_id"] == run_id),
                "completed dispatch must not be interrupted: {runs}"
            );
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    let runs = get_runs_json(&client, running.addr, &token).await;
    assert!(
        runs["terminal_noop"]
            .as_array()
            .unwrap()
            .iter()
            .any(|run| run["run_id"] == run_id),
        "completed dispatch should land in terminal_noop, not interrupted: {runs}"
    );
    assert!(
        !runs["interrupted"]
            .as_array()
            .unwrap()
            .iter()
            .any(|run| run["run_id"] == run_id),
        "completed dispatch must not be interrupted: {runs}"
    );

    let _ = running.shutdown.send(());
    let _ = running.join.await;
}

#[tokio::test]
async fn get_task_detail_returns_parsed_body_sections() {
    let tmp = tempfile::tempdir().unwrap();
    let home = Home::at(tmp.path().join("home"));
    home.ensure().unwrap();
    let project_root = tmp.path().join("proj");
    seed_project(&home, &project_root, "orgasmic");
    write(
        &project_root.join(".orgasmic/tasks/backlog.org"),
        "#+title: sprint\n#+orgasmic_version: 1\n\n* BACKLOG TASK-BODY Body sections task :work:\n:PROPERTIES:\n:ID:               TASK-BODY\n:END:\n\n** Description\nFirst *bold* paragraph.\n\n** Acceptance Criteria\n- [ ] Opens with parsed sections.\n\n** Evidence\n- cli passes\n\n** Notes\nKeep scoped.\n",
    );
    let running = boot(home.clone()).await;
    let token = read_token(&home);
    let client = reqwest::Client::new();

    let task: serde_json::Value = client
        .get(format!(
            "http://{}/api/projects/orgasmic/tasks/TASK-BODY",
            running.addr
        ))
        .bearer_auth(&token)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();

    assert_eq!(task["body"]["description"], "First *bold* paragraph.");
    assert_eq!(
        task["body"]["acceptance_criteria"][0]["text"],
        "Opens with parsed sections."
    );
    assert_eq!(task["body"]["evidence"][0], "cli passes");
    assert_eq!(task["body"]["notes"], "Keep scoped.");

    let _ = running.shutdown.send(());
    let _ = running.join.await;
}

#[tokio::test]
async fn goal_lifecycle_set_clear_supersede_round_trip() {
    let tmp = tempfile::tempdir().unwrap();
    let home = Home::at(tmp.path().join("home"));
    home.ensure().unwrap();
    write(
        &home.config(),
        "bind_host: 127.0.0.1\nbind_port: 4848\ntx:\n  commit_to_project: true\nmanager:\n  actor: manager@example.com\n",
    );
    let project_root = tmp.path().join("proj");
    seed_project(&home, &project_root, "orgasmic");
    write(
        &project_root.join(".orgasmic/tasks/goal.org"),
        "#+title: Goal\n#+orgasmic_version: 1\n#+scope: project\n\n* GOAL Bootstrap goal :goal:\n:PROPERTIES:\n:ID:               goal-bootstrap\n:SET_AT:           [2026-06-01 Sat]\n:SET_BY:           seed@example.com\n:REPLACES:         —\n:END:\n\n** Statement\nBootstrap.\n",
    );

    let running = boot(home.clone()).await;
    let token = read_token(&home);
    let client = reqwest::Client::new();
    let base = format!("http://{}", running.addr);

    let set: serde_json::Value = client
        .post(format!("{base}/api/projects/orgasmic/goal/set"))
        .bearer_auth(&token)
        .json(&serde_json::json!({
            "request_id": "goal-set-1",
            "id": "goal-test-first",
            "title": "First test goal",
            "statement": "Do the first thing.",
            "reached_when": "- [ ] First goal reached.",
            "reason": "integration set"
        }))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(set["goal_id"], "goal-test-first");
    let tx_path = std::path::PathBuf::from(set["tx_path"].as_str().unwrap());
    assert!(tx_path.starts_with(project_root.join(".orgasmic/tx")));

    let goal_after_set =
        std::fs::read_to_string(project_root.join(".orgasmic/tasks/goal.org")).unwrap();
    assert!(goal_after_set.contains("* GOAL First test goal :goal:"));
    assert!(goal_after_set.contains(":ID:               goal-test-first"));
    assert!(goal_after_set.contains("* SUPERSEDED Bootstrap goal :goal:"));
    assert!(goal_after_set.contains(":STATUS:           superseded"));
    let tx_after_set = std::fs::read_to_string(&tx_path).unwrap();
    assert!(tx_after_set.contains("manager.set_goal"));
    assert!(tx_after_set.contains(":GOAL_ID:      goal-test-first"));

    let clear: serde_json::Value = client
        .post(format!("{base}/api/projects/orgasmic/goal/clear"))
        .bearer_auth(&token)
        .json(&serde_json::json!({
            "request_id": "goal-clear-1",
            "reason": "integration clear"
        }))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(clear["goal_id"], "goal-test-first");
    let goal_after_clear =
        std::fs::read_to_string(project_root.join(".orgasmic/tasks/goal.org")).unwrap();
    assert!(goal_after_clear.contains("* CLEARED First test goal :goal:"));
    assert!(goal_after_clear.contains(":STATUS:           cleared"));
    assert!(goal_after_clear.contains(":CLEARED_BY:"));
    let tx_after_clear = std::fs::read_to_string(&tx_path).unwrap();
    assert!(tx_after_clear.contains("manager.clear_goal"));

    let set2: serde_json::Value = client
        .post(format!("{base}/api/projects/orgasmic/goal/set"))
        .bearer_auth(&token)
        .json(&serde_json::json!({
            "request_id": "goal-set-2",
            "id": "goal-test-second",
            "title": "Second test goal",
            "statement": "Do the second thing."
        }))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(set2["goal_id"], "goal-test-second");
    let goal_after_set2 =
        std::fs::read_to_string(project_root.join(".orgasmic/tasks/goal.org")).unwrap();
    assert!(goal_after_set2.contains("* GOAL Second test goal :goal:"));

    let supersede: serde_json::Value = client
        .post(format!("{base}/api/projects/orgasmic/goal/supersede"))
        .bearer_auth(&token)
        .json(&serde_json::json!({
            "request_id": "goal-supersede-1",
            "reason": "integration supersede"
        }))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(supersede["goal_id"], "goal-test-second");
    let goal_after_supersede =
        std::fs::read_to_string(project_root.join(".orgasmic/tasks/goal.org")).unwrap();
    assert!(goal_after_supersede.contains("* SUPERSEDED Second test goal :goal:"));
    assert!(goal_after_supersede.contains(":SUPERSEDED_AT:"));
    let tx_after_supersede = std::fs::read_to_string(&tx_path).unwrap();
    assert!(tx_after_supersede.contains("manager.supersede_goal"));

    let _ = running.shutdown.send(());
    let _ = running.join.await;
}

#[tokio::test]
async fn project_tx_record_survives_daemon_restart() {
    let tmp = tempfile::tempdir().unwrap();
    let home = Home::at(tmp.path().join("home"));
    home.ensure().unwrap();
    write(
        &home.config(),
        "bind_host: 127.0.0.1\nbind_port: 4848\ntx:\n  commit_to_project: true\nmanager:\n  actor: manager@example.com\n",
    );
    let project_root = tmp.path().join("proj");
    seed_project(&home, &project_root, "orgasmic");

    let running = boot(home.clone()).await;
    let token = read_token(&home);
    let client = reqwest::Client::new();
    let append: serde_json::Value = client
        .post(format!("http://{}/api/tx", running.addr))
        .bearer_auth(&token)
        .json(&serde_json::json!({
            "request_id": "project-tx-restart-survival",
            "type": "manager.action",
            "project": "orgasmic",
            "reason": "survive restart"
        }))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let tx_id = append["tx_id"].as_str().unwrap().to_string();
    let tx_path = std::path::PathBuf::from(append["tx_path"].as_str().unwrap());
    assert!(tx_path.starts_with(project_root.join(".orgasmic/tx")));
    assert!(std::fs::read_to_string(&tx_path).unwrap().contains(&tx_id));

    let _ = running.shutdown.send(());
    let _ = running.join.await;

    let running = boot(home.clone()).await;
    let token = read_token(&home);
    let client = reqwest::Client::new();
    let raw = std::fs::read_to_string(&tx_path).unwrap();
    assert!(
        raw.contains(&tx_id),
        "project tx must survive daemon restart at {}",
        tx_path.display()
    );
    let list: serde_json::Value = client
        .get(format!(
            "http://{}/api/tx?project=orgasmic&limit=20",
            running.addr
        ))
        .bearer_auth(&token)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert!(list
        .as_array()
        .unwrap()
        .iter()
        .any(|item| item["entry"]["tx_id"] == tx_id));

    let _ = running.shutdown.send(());
    let _ = running.join.await;
}

#[tokio::test]
async fn task_state_flip_persists_through_daemon_writer() {
    let tmp = tempfile::tempdir().unwrap();
    let home = Home::at(tmp.path().join("home"));
    home.ensure().unwrap();
    write(
        &home.config(),
        "bind_host: 127.0.0.1\nbind_port: 4848\ntx:\n  commit_to_project: true\nmanager:\n  actor: manager@example.com\n",
    );
    let project_root = tmp.path().join("proj");
    seed_project(&home, &project_root, "orgasmic");
    let sprint_path = project_root.join(".orgasmic/tasks/backlog.org");
    write(
        &sprint_path,
        "#+title: sprint\n#+orgasmic_version: 1\n\n* BACKLOG TASK-STATE State flip task :work:\n:PROPERTIES:\n:ID:               TASK-STATE\n:END:\n\n** Description\nFlip me.\n",
    );

    let running = boot(home.clone()).await;
    let token = read_token(&home);
    let client = reqwest::Client::new();
    let updated: serde_json::Value = client
        .post(format!(
            "http://{}/api/projects/orgasmic/tasks/TASK-STATE",
            running.addr
        ))
        .bearer_auth(&token)
        .json(&serde_json::json!({
            "request_id": "task-state-flip-1",
            "state": "done",
            "reason": "integration state flip"
        }))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(updated["id"], "TASK-STATE");
    assert_eq!(updated["lifecycle_stage"], "done");

    let done_path = project_root.join(".orgasmic/tasks/done.org");
    let done = std::fs::read_to_string(&done_path).unwrap();
    assert!(done.contains("* DONE TASK-STATE State flip task"));
    let sprint = std::fs::read_to_string(&sprint_path).unwrap();
    assert!(!sprint.contains("TASK-STATE"));

    let tx_dir = project_root.join(".orgasmic/tx");
    let tx_raw = std::fs::read_dir(&tx_dir)
        .unwrap()
        .filter_map(|entry| entry.ok())
        .map(|entry| std::fs::read_to_string(entry.path()).unwrap())
        .collect::<String>();
    assert!(tx_raw.contains("task.state_transitioned"));
    assert!(tx_raw.contains(":FROM_STATE:   backlog"));
    assert!(tx_raw.contains(":TO_STATE:     done"));

    let property_updated: serde_json::Value = client
        .post(format!(
            "http://{}/api/projects/orgasmic/tasks/TASK-STATE",
            running.addr
        ))
        .bearer_auth(&token)
        .json(&serde_json::json!({
            "request_id": "task-property-flip-1",
            "priority": "P1",
        }))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(property_updated["id"], "TASK-STATE");
    assert_eq!(property_updated["priority"], "P1");

    let done_after_property = std::fs::read_to_string(&done_path).unwrap();
    assert!(done_after_property.lines().any(|line| {
        line.starts_with(":PRIORITY:") && line.split_whitespace().last() == Some("P1")
    }));
    let tx_raw_after_property = std::fs::read_dir(&tx_dir)
        .unwrap()
        .filter_map(|entry| entry.ok())
        .map(|entry| std::fs::read_to_string(entry.path()).unwrap())
        .collect::<String>();
    assert!(tx_raw_after_property.contains("task.property_updated"));

    let _ = running.shutdown.send(());
    let _ = running.join.await;
}
