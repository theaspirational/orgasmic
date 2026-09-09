//! C2 over the real daemon: dispatch attempts recorded as conversations, the
//! relaxed anchor rule, range chips becoming link anchors, and a plugin's
//! `:CHAT_PROMPT:`. A fake ACP agent on PATH stands in for every harness, so
//! `stdio/opencode` dispatches and `hermes` chats both run end to end.
use orgasmic_core::Home;
use orgasmic_daemon::{Daemon, DaemonOptions, RunningDaemon};
use serde_json::{json, Value};
use std::path::{Path, PathBuf};
use std::time::Duration;

fn write(path: impl AsRef<Path>, source: &str) {
    let path = path.as_ref();
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, source).unwrap();
}

/// The same fake ACP agent `conversations_routes.rs` uses: handshake, one
/// `agent_message_chunk` per prompt, every incoming line appended to
/// `.orgasmic/tmp/fake-acp.log` under its cwd (the project root for chats,
/// the worktree for dispatches).
fn fake_acp_script(load_session: bool) -> String {
    format!(
        r#"#!/bin/sh
mkdir -p .orgasmic/tmp
LOG="$PWD/.orgasmic/tmp/fake-acp.log"
MODES='[{{"id":"dont_ask","name":"Don'"'"'t ask"}},{{"id":"default","name":"Default"}},{{"id":"build","name":"Build"}}]'
while IFS= read -r line; do
  [ -z "$line" ] && continue
  printf '%s\n' "$line" >> "$LOG"
  head=${{line%%\"params\"*}}
  method=$(printf '%s' "$head" | sed -n 's/.*"method":"\([^"]*\)".*/\1/p')
  id=$(printf '%s' "$head" | sed -n 's/.*"id":\([0-9][0-9]*\).*/\1/p')
  [ -z "$id" ] && continue
  case "$method" in
    initialize)
      printf '{{"jsonrpc":"2.0","id":%s,"result":{{"protocolVersion":1,"agentCapabilities":{{"loadSession":{load_session}}}}}}}\n' "$id"
      ;;
    session/new)
      printf '{{"jsonrpc":"2.0","id":%s,"result":{{"sessionId":"sess-%s","modes":{{"currentModeId":"default","availableModes":%s}}}}}}\n' "$id" "$$" "$MODES"
      ;;
    session/load)
      printf '{{"jsonrpc":"2.0","id":%s,"result":{{"modes":{{"currentModeId":"default","availableModes":%s}}}}}}\n' "$id" "$MODES"
      ;;
    session/prompt)
      printf '{{"jsonrpc":"2.0","method":"session/update","params":{{"sessionId":"sess-%s","update":{{"sessionUpdate":"agent_message_chunk","content":{{"type":"text","text":"fake reply"}}}}}}}}\n' "$$"
      printf '{{"jsonrpc":"2.0","id":%s,"result":{{"stopReason":"end_turn"}}}}\n' "$id"
      ;;
    *)
      printf '{{"jsonrpc":"2.0","id":%s,"result":{{}}}}\n' "$id"
      ;;
  esac
done
"#
    )
}

/// `hermes` (loadSession true) for chats, `opencode` (loadSession false) for
/// dispatched attempts, installed on PATH once per test process.
fn install_fake_agents() {
    static INSTALLED: std::sync::OnceLock<PathBuf> = std::sync::OnceLock::new();
    INSTALLED.get_or_init(|| {
        let dir = std::env::temp_dir().join(format!(
            "orgasmic-conversations-dispatch-fake-acp-{}",
            std::process::id()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        for (name, load_session) in [("hermes", true), ("opencode", false)] {
            let path = dir.join(name);
            std::fs::write(&path, fake_acp_script(load_session)).unwrap();
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
            }
        }
        let current = std::env::var("PATH").unwrap_or_default();
        std::env::set_var("PATH", format!("{}:{current}", dir.display()));
        dir
    });
}

async fn request(
    client: &reqwest::Client,
    base: &str,
    token: &str,
    method: reqwest::Method,
    path: &str,
    payload: Option<Value>,
) -> (u16, Value) {
    let mut req = client
        .request(method, format!("{base}/api{path}"))
        .bearer_auth(token);
    if let Some(payload) = payload {
        req = req.json(&payload);
    }
    let res = req.send().await.unwrap();
    let status = res.status().as_u16();
    let text = res.text().await.unwrap();
    (
        status,
        serde_json::from_str(&text).unwrap_or(Value::String(text)),
    )
}

async fn post(
    client: &reqwest::Client,
    base: &str,
    token: &str,
    path: &str,
    payload: Value,
    expected: u16,
) -> Value {
    let (status, body) = request(
        client,
        base,
        token,
        reqwest::Method::POST,
        path,
        Some(payload),
    )
    .await;
    assert_eq!(status, expected, "POST {path}: {body}");
    body
}

async fn get(
    client: &reqwest::Client,
    base: &str,
    token: &str,
    path: &str,
    expected: u16,
) -> Value {
    let (status, body) = request(client, base, token, reqwest::Method::GET, path, None).await;
    assert_eq!(status, expected, "GET {path}: {body}");
    body
}

/// The meetings plugin's `:CHAT_PROMPT:` for this daemon, when any.
struct Fixture {
    temp: tempfile::TempDir,
    home: Home,
    running: RunningDaemon,
    base: String,
    token: String,
}

async fn fixture(chat_prompt: Option<&str>) -> Fixture {
    let _ = tracing_subscriber::fmt()
        .with_env_filter("error")
        .with_test_writer()
        .try_init();
    install_fake_agents();
    let temp = tempfile::tempdir().unwrap();
    let home = Home::at(temp.path().join("home"));
    home.ensure().unwrap();
    let repo = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .canonicalize()
        .unwrap();
    std::os::unix::fs::symlink(&repo, home.source()).unwrap();
    let root = temp.path().join("project");
    write(
        root.join(".orgasmic/project.org"),
        "* PROJECT demo\n:PROPERTIES:\n:ID: demo\n:END:\n",
    );
    write(
        home.board(),
        &format!(
            "* PROJECT demo\n:PROPERTIES:\n:ID: demo\n:PATH: {}\n:BRANCH: main\n:END:\n",
            root.display()
        ),
    );
    let example = repo.join("examples/plugins/meetings");
    for file in [
        "plugin.org",
        "prompts/meeting-chat.org",
        "ui/index.js",
        "ui/player.js",
        "bin/import",
    ] {
        let dest = home.user().join("plugins/meetings").join(file);
        std::fs::create_dir_all(dest.parent().unwrap()).unwrap();
        std::fs::copy(example.join(file), &dest).unwrap();
        if file == "plugin.org" {
            let mut manifest = std::fs::read_to_string(&dest).unwrap().replacen(
                ":CAPABILITIES: ",
                ":CAPABILITIES: chat.read chat.write ",
                1,
            );
            if let Some(chat_prompt) = chat_prompt {
                manifest = manifest.replacen(
                    ":COLLECTION: meetings\n",
                    &format!(":COLLECTION: meetings\n:CHAT_PROMPT: {chat_prompt}\n"),
                    1,
                );
                assert!(manifest.contains(":CHAT_PROMPT:"), "{manifest}");
            }
            std::fs::write(&dest, manifest).unwrap();
        }
    }
    let running = Daemon::run(
        home.clone(),
        DaemonOptions {
            bind_override: Some("127.0.0.1".parse().unwrap()),
            port_override: Some(0),
            fs_watcher_enabled: false,
            ..Default::default()
        },
    )
    .await
    .unwrap();
    let base = format!("http://{}", running.addr);
    let token = std::fs::read_to_string(home.auth_token())
        .unwrap()
        .trim()
        .to_owned();
    let client = reqwest::Client::new();
    post(&client, &base, &token, "/plugins/meetings/activation", json!({"project":"demo", "enabled":true, "approved_capabilities":["nodes.read","nodes.write","links.read","links.write","attachments.read","attachments.write","ui.execute","chat.read","chat.write"]}), 200).await;
    Fixture {
        temp,
        home,
        running,
        base,
        token,
    }
}

async fn create_node(
    client: &reqwest::Client,
    base: &str,
    token: &str,
    kind: &str,
    title: &str,
) -> String {
    post(
        client,
        base,
        token,
        "/org/node",
        json!({"project":"demo","kind":kind,"title":title}),
        200,
    )
    .await["id"]
        .as_str()
        .unwrap()
        .to_owned()
}

/// The fake agent's log line that carries `needle`, once it lands.
async fn wait_for_log(log: &Path, needle: &str) -> String {
    let deadline = std::time::Instant::now() + Duration::from_secs(20);
    loop {
        if let Ok(body) = std::fs::read_to_string(log) {
            if let Some(line) = body.lines().find(|line| line.contains(needle)) {
                return line.to_string();
            }
        }
        assert!(
            std::time::Instant::now() < deadline,
            "fake agent never received {needle:?}: {}",
            std::fs::read_to_string(log).unwrap_or_default()
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

async fn release_run(client: &reqwest::Client, base: &str, token: &str, run_id: &str) {
    post(
        client,
        base,
        token,
        &format!("/runs/{run_id}/release"),
        json!({"reason":"test release"}),
        200,
    )
    .await;
    let deadline = std::time::Instant::now() + Duration::from_secs(20);
    loop {
        let live = get(client, base, token, "/runs/live", 200).await;
        let still = live["live"]
            .as_array()
            .unwrap()
            .iter()
            .any(|run| run["run_id"] == run_id);
        if !still {
            return;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "run {run_id} never left the live set"
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

fn property<'a>(doc: &'a Value, key: &str) -> &'a str {
    doc["properties"]
        .as_array()
        .unwrap()
        .iter()
        .find(|p| p["key"] == key)
        .and_then(|p| p["value"].as_str())
        .unwrap_or_default()
}

fn request_id() -> String {
    uuid::Uuid::new_v4().to_string()
}

/// A small real PCM WAV uploaded onto `node`; returns `(attachment id, revision)`.
async fn upload_wav(
    client: &reqwest::Client,
    base: &str,
    token: &str,
    node: &str,
) -> (String, String) {
    let size: u64 = 1024;
    let mut bytes = vec![0u8; size as usize];
    bytes[..4].copy_from_slice(b"RIFF");
    bytes[4..8].copy_from_slice(&((size - 8) as u32).to_le_bytes());
    bytes[8..16].copy_from_slice(b"WAVEfmt ");
    bytes[16..20].copy_from_slice(&16u32.to_le_bytes());
    bytes[20..22].copy_from_slice(&1u16.to_le_bytes());
    bytes[22..24].copy_from_slice(&2u16.to_le_bytes());
    bytes[24..28].copy_from_slice(&48000u32.to_le_bytes());
    bytes[28..32].copy_from_slice(&192000u32.to_le_bytes());
    bytes[32..34].copy_from_slice(&4u16.to_le_bytes());
    bytes[34..36].copy_from_slice(&16u16.to_le_bytes());
    bytes[36..40].copy_from_slice(b"data");
    bytes[40..44].copy_from_slice(&((size - 44) as u32).to_le_bytes());
    let id = request_id();
    post(
        client,
        base,
        token,
        "/attachments/uploads",
        json!({"project":"demo","node":node,"name":"Planning.wav","media_type":"audio/wav","size":size,"request_id":id}),
        200,
    )
    .await;
    let response = client
        .put(format!(
            "{base}/api/attachments/uploads/{id}?project=demo&offset=0"
        ))
        .bearer_auth(token)
        .body(bytes)
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 200);
    let asset = post(
        client,
        base,
        token,
        &format!("/attachments/uploads/{id}/finish?project=demo"),
        json!({}),
        200,
    )
    .await;
    (id, asset["revision"].as_str().unwrap().to_owned())
}

fn dispatch_body(kind: &str, temp: &Path, attempt: &str) -> (Value, PathBuf) {
    let worktree = temp.join(format!("worktree-{attempt}"));
    std::fs::create_dir_all(&worktree).unwrap();
    let brief = temp.join(format!("brief-{attempt}.md"));
    write(&brief, "c2 dispatch brief\n");
    let body = json!({
        "kind": kind,
        "mode": "stdio",
        "harness": "opencode",
        "brief_path": brief,
        "worktree_path": worktree,
        "last_path": temp.join(format!("{attempt}-last.txt")),
        "stdout_path": temp.join(format!("{attempt}-stdout.log")),
        "branch": format!("task-{attempt}"),
        "reason": "c2 conversations test",
    });
    (body, worktree)
}

fn conversation_backlinks(links: &Value) -> Vec<String> {
    links
        .as_array()
        .unwrap()
        .iter()
        .filter(|l| l["kind"] == "RELATES_TO")
        .filter_map(|l| l["source"].as_str())
        .filter(|s| s.starts_with("CONV-"))
        .map(str::to_owned)
        .collect()
}

#[tokio::test]
async fn dispatch_attempts_are_recorded_as_task_conversations() {
    let fx = fixture(None).await;
    let (client, base, token) = (reqwest::Client::new(), fx.base.clone(), fx.token.clone());
    let task = create_node(&client, &base, &token, "task", "Build the thing").await;
    let dispatch_path = format!("/projects/demo/tasks/{task}/dispatch");

    // First implementer attempt.
    let (body, worktree1) = dispatch_body("implementer", fx.temp.path(), "impl-1");
    let first = post(&client, &base, &token, &dispatch_path, body, 200).await;
    let run1 = first["run_id"].as_str().unwrap().to_owned();
    let conv = first["conversation_id"].as_str().unwrap().to_owned();
    assert!(conv.starts_with("CONV-"), "{first}");
    let log1 = worktree1.join(".orgasmic/tmp/fake-acp.log");
    let prompt = wait_for_log(&log1, "prompt_spec:").await;
    assert!(prompt.contains("session/prompt"), "{prompt}");

    let backlinks = get(
        &client,
        &base,
        &token,
        &format!("/links?project=demo&node={task}&incoming=true"),
        200,
    )
    .await;
    assert_eq!(conversation_backlinks(&backlinks), vec![conv.clone()]);
    let doc = get(
        &client,
        &base,
        &token,
        &format!("/org/node?project=demo&id={conv}"),
        200,
    )
    .await;
    assert_eq!(doc["todo"], "OPEN");
    assert_eq!(doc["title"], format!("Implement {task}"));
    assert_eq!(property(&doc, "PURPOSE"), "implement");
    assert_eq!(property(&doc, "OWNER"), "admin");
    assert_eq!(property(&doc, "PROVIDER"), "opencode");
    assert_eq!(property(&doc, "MODE"), "chat");
    assert_eq!(property(&doc, "WORKTREE"), worktree1.display().to_string());
    assert_eq!(property(&doc, "RUNS"), run1);

    // The live run carries the conversation id beside its task lease.
    let live = get(&client, &base, &token, "/runs/live", 200).await;
    let run = live["live"]
        .as_array()
        .unwrap()
        .iter()
        .find(|run| run["run_id"] == run1)
        .unwrap_or_else(|| panic!("{live}"));
    assert_eq!(run["task_id"], task);
    assert_eq!(run["conversation_id"], conv);
    assert_eq!(run["role"], "implementer");

    // Input while live delivers to the worker.
    let ping = format!("status-please-{}", request_id());
    let sent = post(
        &client,
        &base,
        &token,
        &format!("/conversations/{conv}/input?project=demo"),
        json!({"message":ping,"request_id":request_id()}),
        200,
    )
    .await;
    assert_eq!(sent["mode"], "live");
    assert_eq!(sent["run_id"], run1);
    let delivered = wait_for_log(&log1, &ping).await;
    assert!(delivered.contains("session/prompt"), "{delivered}");

    // After release the fake records no resumable session: 409 no_resume,
    // never cold.
    release_run(&client, &base, &token, &run1).await;
    let (status, body) = request(
        &client,
        &base,
        &token,
        reqwest::Method::POST,
        &format!("/conversations/{conv}/input?project=demo"),
        Some(json!({"message":"still there?","request_id":request_id()})),
    )
    .await;
    assert_eq!(status, 409, "{body}");
    assert_eq!(body["code"], "no_resume", "{body}");
    assert_eq!(
        body["error"],
        "no native session to resume; dispatch a new attempt"
    );
    let doc = get(
        &client,
        &base,
        &token,
        &format!("/org/node?project=demo&id={conv}"),
        200,
    )
    .await;
    assert_eq!(doc["todo"], "OPEN");
    assert_eq!(property(&doc, "RUNS"), run1, "no cold run was appended");

    // A second attempt appends to the same conversation and moves WORKTREE.
    let (body, worktree2) = dispatch_body("implementer", fx.temp.path(), "impl-2");
    let second = post(&client, &base, &token, &dispatch_path, body, 200).await;
    let run2 = second["run_id"].as_str().unwrap().to_owned();
    assert_eq!(second["conversation_id"], conv);
    assert_ne!(run2, run1);
    wait_for_log(
        &worktree2.join(".orgasmic/tmp/fake-acp.log"),
        "prompt_spec:",
    )
    .await;
    let doc = get(
        &client,
        &base,
        &token,
        &format!("/org/node?project=demo&id={conv}"),
        200,
    )
    .await;
    assert_eq!(property(&doc, "RUNS"), format!("{run1} {run2}"));
    assert_eq!(property(&doc, "WORKTREE"), worktree2.display().to_string());
    let journal = std::fs::read_to_string(
        fx.temp
            .path()
            .join("project/.orgasmic/conversations")
            .join(&conv)
            .join("journal.org"),
    )
    .unwrap();
    let started = journal
        .lines()
        .filter(|line| line.starts_with("* ") && line.ends_with("conversation.run_started"))
        .count();
    assert_eq!(started, 2, "{journal}");
    assert!(journal.contains("graph.conversations.created"), "{journal}");

    // A reviewer attempt gets its own conversation on the same task. It
    // shares the task's worker lease, so the implementer goes first.
    release_run(&client, &base, &token, &run2).await;
    let (body, worktree3) = dispatch_body("reviewer", fx.temp.path(), "review-1");
    let review = post(&client, &base, &token, &dispatch_path, body, 200).await;
    let review_conv = review["conversation_id"].as_str().unwrap().to_owned();
    assert_ne!(review_conv, conv);
    wait_for_log(
        &worktree3.join(".orgasmic/tmp/fake-acp.log"),
        "prompt_spec:",
    )
    .await;
    let doc = get(
        &client,
        &base,
        &token,
        &format!("/org/node?project=demo&id={review_conv}"),
        200,
    )
    .await;
    assert_eq!(property(&doc, "PURPOSE"), "review");
    assert_eq!(doc["title"], format!("Review {task}"));
    assert_eq!(property(&doc, "RUNS"), review["run_id"]);
    let backlinks = get(
        &client,
        &base,
        &token,
        &format!("/links?project=demo&node={task}&incoming=true"),
        200,
    )
    .await;
    let mut sources = conversation_backlinks(&backlinks);
    sources.sort();
    let mut expected = vec![conv.clone(), review_conv.clone()];
    expected.sort();
    assert_eq!(sources, expected);

    release_run(&client, &base, &token, review["run_id"].as_str().unwrap()).await;
    let _ = fx.running.shutdown.send(());
}

#[tokio::test]
async fn link_anchors_may_be_owned_by_the_target() {
    let fx = fixture(None).await;
    let (client, base, token) = (reqwest::Client::new(), fx.base.clone(), fx.token.clone());
    let meeting = create_node(&client, &base, &token, "meetings", "Planning").await;
    let (attachment, revision) = upload_wav(&client, &base, &token, &meeting).await;
    let created = post(
        &client,
        &base,
        &token,
        "/conversations?project=demo",
        json!({"purpose":"meeting","node":meeting,"provider":"hermes","request_id":request_id()}),
        200,
    )
    .await;
    let conv = created["id"].as_str().unwrap().to_owned();

    // Owned by the target (the meeting): accepted.
    let owned = json!({"attachment":attachment,"revision":revision,"start_ms":1000,"end_ms":4000,"label":"the decision"});
    post(
        &client,
        &base,
        &token,
        "/links",
        json!({"project":"demo","source":conv,"target":meeting,"kind":"RELATES_TO","anchors":[owned],"base_revision":1,"request_id":request_id()}),
        200,
    )
    .await;
    let links = get(
        &client,
        &base,
        &token,
        &format!("/links?project=demo&node={conv}"),
        200,
    )
    .await;
    assert_eq!(links[0]["revision"], 2, "{links}");
    assert_eq!(links[0]["anchors"][0]["label"], "the decision");

    // Owned by neither: refused, and the message names both ends.
    let (status, body) = request(
        &client,
        &base,
        &token,
        reqwest::Method::POST,
        "/links",
        Some(json!({"project":"demo","source":conv,"target":meeting,"kind":"RELATES_TO","anchors":[{"attachment":request_id(),"revision":revision,"start_ms":1,"end_ms":2,"label":"x"}],"base_revision":2,"request_id":request_id()})),
    )
    .await;
    assert_eq!(status, 400, "{body}");
    assert!(
        body["error"]
            .as_str()
            .unwrap()
            .contains("owned by the source or target node"),
        "{body}"
    );
    // A stale revision of a real attachment is refused the same way.
    let (status, _) = request(
        &client,
        &base,
        &token,
        reqwest::Method::POST,
        "/links",
        Some(json!({"project":"demo","source":conv,"target":meeting,"kind":"RELATES_TO","anchors":[{"attachment":attachment,"revision":"deadbeef","start_ms":1,"end_ms":2,"label":"x"}],"base_revision":2,"request_id":request_id()})),
    )
    .await;
    assert_eq!(status, 400);

    release_run(&client, &base, &token, created["run_id"].as_str().unwrap()).await;
    let _ = fx.running.shutdown.send(());
}

#[tokio::test]
async fn range_chips_on_the_scoped_node_become_scope_link_anchors() {
    let fx = fixture(None).await;
    let (client, base, token) = (reqwest::Client::new(), fx.base.clone(), fx.token.clone());
    let log = fx.temp.path().join("project/.orgasmic/tmp/fake-acp.log");
    let meeting = create_node(&client, &base, &token, "meetings", "Standup").await;
    let other = create_node(&client, &base, &token, "task", "Elsewhere").await;
    let (attachment, revision) = upload_wav(&client, &base, &token, &meeting).await;
    let message = format!("What was decided here? {}", "detail ".repeat(30).trim_end());
    let chip = json!({"kind":"range","node":meeting,"attachment":attachment,"revision":revision,"start_ms":60000,"end_ms":75000});

    // "Chat about this moment" on a meeting without a conversation: the chip
    // rides on the create call, which needs a message to attach it to.
    post(
        &client,
        &base,
        &token,
        "/conversations?project=demo",
        json!({"purpose":"meeting","node":meeting,"provider":"hermes","context":[chip],"request_id":request_id()}),
        400,
    )
    .await;
    let created = post(
        &client,
        &base,
        &token,
        "/conversations?project=demo",
        json!({"purpose":"meeting","node":meeting,"provider":"hermes","message":message,"context":[chip],"request_id":request_id()}),
        200,
    )
    .await;
    let conv = created["id"].as_str().unwrap().to_owned();
    let run = created["run_id"].as_str().unwrap().to_owned();
    let input = format!("/conversations/{conv}/input?project=demo");
    let links_path = format!("/links?project=demo&node={conv}");
    let opening = wait_for_log(&log, "What was decided here?").await;
    // The chip block is JSON inside the prompt's JSON string, hence `\"`.
    assert!(opening.contains("start_ms\\\":60000"), "{opening}");
    let links = get(&client, &base, &token, &links_path, 200).await;
    assert_eq!(links.as_array().unwrap().len(), 1, "{links}");
    assert_eq!(links[0]["target"], meeting);
    assert_eq!(links[0]["revision"], 2, "{links}");
    let anchors = links[0]["anchors"].as_array().unwrap();
    assert_eq!(anchors.len(), 1, "{links}");
    assert_eq!(anchors[0]["attachment"], attachment);
    assert_eq!(anchors[0]["revision"], revision);
    assert_eq!(anchors[0]["start_ms"], 60000);
    assert_eq!(anchors[0]["end_ms"], 75000);
    let label = anchors[0]["label"].as_str().unwrap();
    assert_eq!(label.chars().count(), 80, "{label}");
    assert!(message.starts_with(label), "{label}");

    // The same range again is deduped without a write; a chip on another
    // node, and one naming a revision the meeting does not have, are ignored.
    let again = format!("again-{}", request_id());
    post(
        &client,
        &base,
        &token,
        &input,
        json!({"message":again,"context":[chip.clone()],"request_id":request_id()}),
        200,
    )
    .await;
    wait_for_log(&log, &again).await;
    let elsewhere = format!("elsewhere-{}", request_id());
    post(
        &client,
        &base,
        &token,
        &input,
        json!({"message":elsewhere,"context":[
            {"kind":"range","node":other,"attachment":attachment,"revision":revision,"start_ms":1,"end_ms":2},
            {"kind":"range","node":meeting,"attachment":attachment,"revision":"deadbeef","start_ms":1,"end_ms":2}
        ],"request_id":request_id()}),
        200,
    )
    .await;
    wait_for_log(&log, &elsewhere).await;
    let links = get(&client, &base, &token, &links_path, 200).await;
    assert_eq!(links[0]["revision"], 2, "{links}");
    assert_eq!(links[0]["anchors"].as_array().unwrap().len(), 1, "{links}");
    let other_links = get(
        &client,
        &base,
        &token,
        &format!("/links?project=demo&node={other}&incoming=true"),
        200,
    )
    .await;
    assert!(other_links.as_array().unwrap().is_empty(), "{other_links}");

    // A second moment appends and bumps the revision once more.
    let more = format!("and later {}", request_id());
    post(
        &client,
        &base,
        &token,
        &input,
        json!({"message":more,"context":[{"kind":"range","node":meeting,"attachment":attachment,"revision":revision,"start_ms":90000,"end_ms":91000}],"request_id":request_id()}),
        200,
    )
    .await;
    wait_for_log(&log, &more).await;
    let links = get(&client, &base, &token, &links_path, 200).await;
    assert_eq!(links[0]["revision"], 3, "{links}");
    assert_eq!(links[0]["anchors"].as_array().unwrap().len(), 2, "{links}");
    assert_eq!(links[0]["anchors"][1]["label"], more);

    release_run(&client, &base, &token, &run).await;
    let _ = fx.running.shutdown.send(());
}

async fn meeting_chat_prompt_spec(fx: &Fixture) -> String {
    let (client, base, token) = (reqwest::Client::new(), fx.base.clone(), fx.token.clone());
    let log = fx.temp.path().join("project/.orgasmic/tmp/fake-acp.log");
    let meeting = create_node(&client, &base, &token, "meetings", "Roadmap").await;
    let opening = format!("opening-{}", request_id());
    let created = post(
        &client,
        &base,
        &token,
        "/conversations?project=demo",
        json!({"purpose":"meeting","node":meeting,"provider":"hermes","message":opening,"request_id":request_id()}),
        200,
    )
    .await;
    let prompt = wait_for_log(&log, &opening).await;
    release_run(&client, &base, &token, created["run_id"].as_str().unwrap()).await;
    prompt
}

#[tokio::test]
async fn plugin_chat_prompt_inside_the_folder_compiles_the_chat_context() {
    // The example plugin's root `:CHAT_PROMPT: prompts/meeting-chat.org`.
    let fx = fixture(None).await;
    let types = get(
        &reqwest::Client::new(),
        &fx.base,
        &fx.token,
        "/node-types?project=demo",
        200,
    )
    .await;
    let meetings = types
        .as_array()
        .unwrap()
        .iter()
        .find(|d| d["collection"] == "meetings")
        .unwrap();
    assert_eq!(meetings["chat_prompt"], "prompts/meeting-chat.org");
    let prompt = meeting_chat_prompt_spec(&fx).await;
    assert!(prompt.contains("prompt_spec: meeting-chat"), "{prompt}");
    assert!(prompt.contains("recordings' moments"), "{prompt}");
    let _ = fx.running.shutdown.send(());
}

#[tokio::test]
async fn plugin_chat_prompt_escaping_the_folder_falls_back_to_node_chat() {
    let fx = fixture(Some("../escape.org")).await;
    std::fs::copy(
        fx.home
            .user()
            .join("plugins/meetings/prompts/meeting-chat.org"),
        fx.home.user().join("plugins/escape.org"),
    )
    .unwrap();
    let prompt = meeting_chat_prompt_spec(&fx).await;
    assert!(prompt.contains("prompt_spec: node-chat"), "{prompt}");
    assert!(!prompt.contains("meeting-chat"), "{prompt}");
    let _ = fx.running.shutdown.send(());
}
