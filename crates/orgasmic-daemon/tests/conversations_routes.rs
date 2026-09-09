//! C1 conversation core over the real daemon: a fake ACP agent on PATH stands
//! in for the Chat provider, so create / continue (live, resumed, cold), the
//! CONV- record, and the authorization matrix run end to end.
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

/// A fake ACP agent: answers the handshake, echoes one `agent_message_chunk`
/// per prompt, and appends every incoming line to `.orgasmic/tmp/fake-acp.log`
/// under its cwd (the project root). `load_session` is what `initialize`
/// advertises, which decides resumed versus cold on continue.
fn fake_acp_script(load_session: bool) -> String {
    format!(
        r#"#!/bin/sh
mkdir -p .orgasmic/tmp
LOG="$PWD/.orgasmic/tmp/fake-acp.log"
MODES='[{{"id":"dont_ask","name":"Don'"'"'t ask"}},{{"id":"default","name":"Default"}},{{"id":"build","name":"Build"}}]'
OPTIONS='[{{"category":"speed","id":"speed","currentValue":"normal","options":[{{"value":"normal","name":"Normal"}},{{"value":"fast","name":"Fast"}}]}}]'
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
      printf '{{"jsonrpc":"2.0","id":%s,"result":{{"sessionId":"sess-%s","modes":{{"currentModeId":"default","availableModes":%s}},"configOptions":%s}}}}\n' "$id" "$$" "$MODES" "$OPTIONS"
      ;;
    session/load)
      printf '{{"jsonrpc":"2.0","id":%s,"result":{{"modes":{{"currentModeId":"default","availableModes":%s}},"configOptions":%s}}}}\n' "$id" "$MODES" "$OPTIONS"
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

/// Install `hermes` (loadSession true) and `opencode` (loadSession false) on
/// PATH once per test process, before any daemon boots.
fn install_fake_agents() {
    static INSTALLED: std::sync::OnceLock<PathBuf> = std::sync::OnceLock::new();
    INSTALLED.get_or_init(|| {
        let dir = std::env::temp_dir().join(format!(
            "orgasmic-conversations-fake-acp-{}",
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

async fn fixture() -> (tempfile::TempDir, Home, RunningDaemon, String, String) {
    let _ = tracing_subscriber::fmt()
        .with_env_filter("error")
        .with_test_writer()
        .try_init();
    install_fake_agents();
    let temp = tempfile::tempdir().unwrap();
    let home = Home::at(temp.path().join("home"));
    home.ensure().unwrap();
    // Shipped prompt specs (node-chat, project-chat) resolve through the
    // source tree, exactly as the installed runtime finds them.
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
            // The shipped example predates chat.*; this fixture's plugin
            // asks for both so the conversation visibility gate is exercised.
            let manifest = std::fs::read_to_string(&dest).unwrap();
            let manifest =
                manifest.replacen(":CAPABILITIES: ", ":CAPABILITIES: chat.read chat.write ", 1);
            assert!(manifest.contains("chat.read"), "{manifest}");
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
    (temp, home, running, base, token)
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

fn fake_log(temp: &tempfile::TempDir) -> PathBuf {
    temp.path().join("project/.orgasmic/tmp/fake-acp.log")
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

/// The release journal entry is written by a daemon task the release does not
/// wait for, so a test polls for it.
async fn wait_for_journal(path: &Path, needle: &str) -> String {
    let deadline = std::time::Instant::now() + Duration::from_secs(20);
    loop {
        let body = std::fs::read_to_string(path).unwrap_or_default();
        if body.contains(needle) {
            return body;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "journal never recorded {needle:?}: {body}"
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

#[tokio::test]
async fn create_continue_live_resume_and_archive() {
    let (temp, _home, running, base, token) = fixture().await;
    let client = reqwest::Client::new();
    let log = fake_log(&temp);
    let task = create_node(&client, &base, &token, "task", "Ship it").await;

    // Generic node create refuses the conversation collection.
    post(
        &client,
        &base,
        &token,
        "/org/node",
        json!({"project":"demo","kind":"conversation","title":"nope"}),
        400,
    )
    .await;

    let first = format!("first-question-{}", request_id());
    let created = post(
        &client,
        &base,
        &token,
        "/conversations?project=demo",
        json!({"purpose":"discuss","node":task,"provider":"hermes","message":first,"request_id":request_id()}),
        200,
    )
    .await;
    let id = created["id"].as_str().unwrap().to_owned();
    let run1 = created["run_id"].as_str().unwrap().to_owned();
    assert!(id.starts_with("CONV-"), "{created}");
    assert_eq!(created["mode"], "cold");
    let prompt = wait_for_log(&log, &first).await;
    assert!(prompt.contains("session/prompt"), "{prompt}");
    assert!(prompt.contains("prompt_spec: node-chat"), "{prompt}");
    assert!(prompt.contains("orgasmic-context"), "{prompt}");
    assert!(
        prompt.contains(&task),
        "scoped node chip rides along: {prompt}"
    );

    let doc = get(
        &client,
        &base,
        &token,
        &format!("/org/node?project=demo&id={id}"),
        200,
    )
    .await;
    assert_eq!(doc["todo"], "OPEN");
    assert_eq!(doc["title"], "Chat about Ship it");
    assert_eq!(property(&doc, "PURPOSE"), "discuss");
    assert_eq!(property(&doc, "OWNER"), "admin");
    assert_eq!(property(&doc, "PROVIDER"), "hermes");
    assert_eq!(property(&doc, "MODE"), "chat");
    assert_eq!(property(&doc, "RUNS"), run1);
    assert!(!property(&doc, "MACHINE").is_empty());
    assert!(!property(&doc, "CREATED_AT").is_empty());

    let backlinks = get(
        &client,
        &base,
        &token,
        &format!("/links?project=demo&node={task}&incoming=true"),
        200,
    )
    .await;
    assert_eq!(backlinks.as_array().unwrap().len(), 1, "{backlinks}");
    assert_eq!(backlinks[0]["source"], id);
    assert_eq!(backlinks[0]["kind"], "RELATES_TO");
    let nodes = get(
        &client,
        &base,
        &token,
        "/graph/nodes?project=demo&layer=conversations",
        200,
    )
    .await;
    assert!(
        nodes.as_array().unwrap().iter().any(|n| n["id"] == id),
        "{nodes}"
    );

    // Live continue with chips: the chips ride the composer_send record.
    let second = format!("second-question-{}", request_id());
    let live = post(
        &client,
        &base,
        &token,
        &format!("/conversations/{id}/input?project=demo"),
        json!({"message":second,"context":[{"kind":"node","id":task},{"kind":"selection","text":"quoted words"}],"request_id":request_id()}),
        200,
    )
    .await;
    assert_eq!(live["mode"], "live");
    assert_eq!(live["run_id"], run1);
    let prompt = wait_for_log(&log, &second).await;
    assert!(
        !prompt.contains("prompt_spec"),
        "live sends carry no scope context: {prompt}"
    );
    assert!(prompt.contains("quoted words"), "{prompt}");
    let run = get(&client, &base, &token, &format!("/runs/{run1}"), 200).await;
    let source = run["source"].as_str().unwrap();
    assert!(source.contains("composer_send"), "{source}");
    assert!(source.contains(r#""context":[{"kind":"node""#), "{source}");
    assert!(
        !source.contains("fake-acp.log"),
        "no paths leak into the record"
    );

    post(
        &client,
        &base,
        &token,
        &format!("/conversations/{id}/input?project=demo"),
        json!({"message":"too big","context":[{"kind":"selection","text":"x".repeat(4097)}],"request_id":request_id()}),
        400,
    )
    .await;

    // A retried send (same request_id) replays its answer and delivers nothing
    // a second time; a fresh id with the same text would deliver again.
    let retry_id = request_id();
    let retried = format!("retried-question-{}", request_id());
    let answered = post(
        &client,
        &base,
        &token,
        &format!("/conversations/{id}/input?project=demo"),
        json!({"message":retried,"request_id":retry_id}),
        200,
    )
    .await;
    wait_for_log(&log, &retried).await;
    let again = post(
        &client,
        &base,
        &token,
        &format!("/conversations/{id}/input?project=demo"),
        json!({"message":retried,"request_id":retry_id}),
        200,
    )
    .await;
    assert_eq!(
        answered, again,
        "a replayed request_id returns its first answer"
    );
    let body = std::fs::read_to_string(&log).unwrap_or_default();
    assert_eq!(
        body.lines().filter(|line| line.contains(&retried)).count(),
        1,
        "the agent heard the retried message once: {body}"
    );

    // 64 KiB is the cap, after trim.
    post(
        &client,
        &base,
        &token,
        &format!("/conversations/{id}/input?project=demo"),
        json!({"message":"y".repeat(64 * 1024 + 1),"request_id":request_id()}),
        400,
    )
    .await;
    post(
        &client,
        &base,
        &token,
        "/conversations/CONV-ZZZZZ/input?project=demo",
        json!({"message":"ghost","request_id":request_id()}),
        404,
    )
    .await;

    // Release, then continue: hermes advertises loadSession, same machine.
    release_run(&client, &base, &token, &run1).await;
    let third = format!("third-question-{}", request_id());
    let resumed = post(
        &client,
        &base,
        &token,
        &format!("/conversations/{id}/input?project=demo"),
        json!({"message":third,"request_id":request_id()}),
        200,
    )
    .await;
    assert_eq!(resumed["mode"], "resumed", "{resumed}");
    let run2 = resumed["run_id"].as_str().unwrap().to_owned();
    assert_ne!(run2, run1);
    let load = wait_for_log(&log, "session/load").await;
    assert!(load.contains(r#""sessionId":"sess-"#), "{load}");
    let prompt = wait_for_log(&log, &third).await;
    assert!(
        !prompt.contains("prompt_spec"),
        "resumed sends carry no scope context: {prompt}"
    );
    let doc = get(
        &client,
        &base,
        &token,
        &format!("/org/node?project=demo&id={id}"),
        200,
    )
    .await;
    assert_eq!(property(&doc, "RUNS"), format!("{run1} {run2}:resumed"));
    let journal_path = temp
        .path()
        .join("project/.orgasmic/conversations")
        .join(&id)
        .join("journal.org");
    // Every run start AND every release is on the record, with the reason the
    // supervisor released for.
    let journal = wait_for_journal(&journal_path, "conversation.run_released").await;
    assert!(journal.contains("conversation.run_started"), "{journal}");
    assert!(journal.contains("conversation.run_resumed"), "{journal}");
    assert!(
        journal.contains(&run1),
        "the released run is named: {journal}"
    );
    assert!(
        journal.contains(":REASON: test release"),
        "the reason the run was released for: {journal}"
    );
    assert!(
        !journal.contains(".jsonl"),
        "no session paths in journals: {journal}"
    );

    // Write hooks: daemon-owned and immutable fields refuse user edits.
    let base_version = doc["source"]["base_version"].as_str().unwrap().to_owned();
    for (key, value) in [
        ("PURPOSE", "review"),
        ("RUNS", "run-x"),
        ("OWNER", "someone"),
    ] {
        let (status, body) = request(
            &client,
            &base,
            &token,
            reqwest::Method::POST,
            &format!("/org/node/{id}/edit"),
            Some(json!({"project":"demo","base_version":base_version,"ops":[{"op":"set_property","key":key,"value":value}]})),
        )
        .await;
        assert_eq!(status, 400, "{key}: {body}");
    }
    let (status, body) = request(
        &client,
        &base,
        &token,
        reqwest::Method::POST,
        &format!("/org/node/{id}/edit"),
        Some(json!({"project":"demo","base_version":base_version,"ops":[{"op":"set_title","title":"Renamed chat"}]})),
    )
    .await;
    assert_eq!(status, 200, "{body}");

    // Released runs still answer with the whole session text, and the scope
    // link reads outgoing from the conversation.
    release_run(&client, &base, &token, &run2).await;
    let released = get(&client, &base, &token, &format!("/runs/{run1}"), 200).await;
    assert!(
        released["source"].as_str().unwrap().contains(&first),
        "{released}"
    );
    let outgoing = get(
        &client,
        &base,
        &token,
        &format!("/links?project=demo&node={id}"),
        200,
    )
    .await;
    assert_eq!(outgoing.as_array().unwrap().len(), 1, "{outgoing}");
    assert_eq!(outgoing[0]["source"], id);
    assert_eq!(outgoing[0]["target"], task);

    // Archive, then input is refused.
    let doc = get(
        &client,
        &base,
        &token,
        &format!("/org/node?project=demo&id={id}"),
        200,
    )
    .await;
    let base_version = doc["source"]["base_version"].as_str().unwrap().to_owned();
    post(
        &client,
        &base,
        &token,
        &format!("/org/node/{id}/edit"),
        json!({"project":"demo","base_version":base_version,"ops":[{"op":"set_state","state":"ARCHIVED"}]}),
        200,
    )
    .await;
    post(
        &client,
        &base,
        &token,
        &format!("/conversations/{id}/input?project=demo"),
        json!({"message":"still there?","request_id":request_id()}),
        400,
    )
    .await;

    let _ = running.shutdown.send(());
}

#[tokio::test]
async fn cold_continue_carries_scope_context_and_bounded_tail() {
    let (temp, _home, running, base, token) = fixture().await;
    let client = reqwest::Client::new();
    let log = fake_log(&temp);

    let first = format!("opening-{}", request_id());
    let created = post(
        &client,
        &base,
        &token,
        "/conversations",
        json!({"project":"demo","purpose":"discuss","provider":"opencode","access":"auto","service_tier":"fast","title":"Project talk","message":first,"request_id":request_id()}),
        200,
    )
    .await;
    let id = created["id"].as_str().unwrap().to_owned();
    let run1 = created["run_id"].as_str().unwrap().to_owned();
    let prompt = wait_for_log(&log, &first).await;
    assert!(prompt.contains("prompt_spec: project-chat"), "{prompt}");
    // The reply must reach the session before the run is released.
    let deadline = std::time::Instant::now() + Duration::from_secs(20);
    loop {
        let run = get(&client, &base, &token, &format!("/runs/{run1}"), 200).await;
        if run["source"].as_str().unwrap().contains("fake reply") {
            break;
        }
        assert!(std::time::Instant::now() < deadline, "reply never recorded");
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    release_run(&client, &base, &token, &run1).await;

    let second = format!("continuing-{}", request_id());
    let cold = post(
        &client,
        &base,
        &token,
        &format!("/conversations/{id}/input?project=demo"),
        json!({"message":second,"request_id":request_id()}),
        200,
    )
    .await;
    assert_eq!(cold["mode"], "cold", "{cold}");
    let run2 = cold["run_id"].as_str().unwrap().to_owned();
    let prompt = wait_for_log(&log, &second).await;
    assert!(prompt.contains("prompt_spec: project-chat"), "{prompt}");
    assert!(prompt.contains(&format!("Operator: {first}")), "{prompt}");
    assert!(prompt.contains("Assistant: fake reply"), "{prompt}");
    assert!(!prompt.contains("session/load"), "{prompt}");
    let doc = get(
        &client,
        &base,
        &token,
        &format!("/org/node?project=demo&id={id}"),
        200,
    )
    .await;
    assert_eq!(property(&doc, "RUNS"), format!("{run1} {run2}:cold"));
    assert_eq!(doc["title"], "Project talk");
    assert_eq!(property(&doc, "ACCESS"), "auto");
    // The tier the conversation was created with is its own, and the cold
    // relaunch addresses the new run with it rather than dropping it.
    assert_eq!(property(&doc, "SERVICE_TIER"), "fast");
    let agent_log = std::fs::read_to_string(&log).unwrap_or_default();
    assert_eq!(
        agent_log
            .lines()
            .filter(|line| line.contains(r#""configId":"speed","value":"fast""#))
            .count(),
        2,
        "the first launch and the cold relaunch both address the tier: {agent_log}"
    );

    let _ = running.shutdown.send(());
}

#[tokio::test]
async fn two_live_conversations_and_the_chat_launch_shim() {
    let (temp, _home, running, base, token) = fixture().await;
    let client = reqwest::Client::new();
    let log = fake_log(&temp);
    let task = create_node(&client, &base, &token, "task", "Parallel").await;

    let a = post(
        &client,
        &base,
        &token,
        "/conversations",
        json!({"project":"demo","purpose":"discuss","node":task,"provider":"hermes","harness_args":null,"request_id":request_id()}),
        200,
    )
    .await;
    let b = post(
        &client,
        &base,
        &token,
        "/conversations",
        json!({"project":"demo","purpose":"discuss","node":task,"provider":"hermes","title":"Second","request_id":request_id()}),
        200,
    )
    .await;
    assert_ne!(a["id"], b["id"]);
    assert_ne!(a["run_id"], b["run_id"]);
    let live = get(&client, &base, &token, "/runs/live", 200).await;
    for conv in [&a, &b] {
        assert!(
            live["live"]
                .as_array()
                .unwrap()
                .iter()
                .any(|run| run["run_id"] == conv["run_id"] && run["task_id"] == conv["id"]),
            "{live}"
        );
    }
    let ping_a = format!("ping-a-{}", request_id());
    let ping_b = format!("ping-b-{}", request_id());
    for (conv, ping) in [(&a, &ping_a), (&b, &ping_b)] {
        let sent = post(
            &client,
            &base,
            &token,
            &format!(
                "/conversations/{}/input?project=demo",
                conv["id"].as_str().unwrap()
            ),
            json!({"message":ping,"request_id":request_id()}),
            200,
        )
        .await;
        assert_eq!(sent["mode"], "live");
        assert_eq!(sent["run_id"], conv["run_id"]);
        wait_for_log(&log, ping).await;
    }
    let backlinks = get(
        &client,
        &base,
        &token,
        &format!("/links?project=demo&node={task}&incoming=true"),
        200,
    )
    .await;
    assert_eq!(backlinks.as_array().unwrap().len(), 2, "{backlinks}");

    // The shim is a discuss conversation titled Chat.
    let shim = post(
        &client,
        &base,
        &token,
        "/manager/chat/launch",
        json!({"project_id":"demo","provider":"hermes"}),
        200,
    )
    .await;
    let conversation_id = shim["conversation_id"].as_str().unwrap().to_owned();
    assert!(conversation_id.starts_with("CONV-"), "{shim}");
    assert!(shim["run_id"].as_str().is_some_and(|run| !run.is_empty()));
    let doc = get(
        &client,
        &base,
        &token,
        &format!("/org/node?project=demo&id={conversation_id}"),
        200,
    )
    .await;
    assert_eq!(doc["title"], "Chat");
    assert_eq!(property(&doc, "PURPOSE"), "discuss");
    assert_eq!(property(&doc, "RUNS"), shim["run_id"]);
    post(
        &client,
        &base,
        &token,
        "/manager/chat/launch",
        json!({"project_id":"demo","provider":"nope"}),
        400,
    )
    .await;

    let types = get(&client, &base, &token, "/node-types?project=demo", 200).await;
    let conversations = types
        .as_array()
        .unwrap()
        .iter()
        .find(|d| d["collection"] == "conversations")
        .expect("conversations descriptor");
    assert_eq!(conversations["chat_prompt"], "node-chat");
    assert_eq!(conversations["id_prefix"], "CONV-");
    let tasks = types
        .as_array()
        .unwrap()
        .iter()
        .find(|d| d["collection"] == "tasks")
        .unwrap();
    assert_eq!(tasks["chat_prompt"], "node-chat");

    let _ = running.shutdown.send(());
}

#[tokio::test]
async fn members_and_plugins_are_gated_by_chat_actions_and_ownership() {
    let (temp, home, running, base, token) = fixture().await;
    let client = reqwest::Client::new();
    let log = fake_log(&temp);
    let task = create_node(&client, &base, &token, "task", "Gated").await;
    let meeting = create_node(&client, &base, &token, "meetings", "Standup").await;

    let viewer =
        orgasmic_core::add_member(&home, "viewer", &[("demo".into(), "viewer".into())]).unwrap();
    let editor =
        orgasmic_core::add_member(&home, "editor", &[("demo".into(), "editor".into())]).unwrap();
    let anna = orgasmic_core::add_member_with_actions(
        &home,
        "anna",
        &[("demo".into(), "editor".into())],
        &["chat.execute".into()],
    )
    .unwrap();
    let bob = orgasmic_core::add_member_with_actions(
        &home,
        "bob",
        &[("demo".into(), "editor".into())],
        &["chat.execute".into()],
    )
    .unwrap();
    let artifacts = orgasmic_core::add_member(
        &home,
        "artifacts-only",
        &[("demo".into(), "artifacts".into())],
    )
    .unwrap();

    let me = get(&client, &base, &anna, "/me", 200).await;
    let caps = me["projects"][0]["capabilities"].as_array().unwrap();
    assert!(caps.iter().any(|c| c == "chat.execute"), "{me}");
    assert!(caps.iter().any(|c| c == "chat.write"), "{me}");
    let me = get(&client, &base, &viewer, "/me", 200).await;
    let caps = me["projects"][0]["capabilities"].as_array().unwrap();
    assert!(caps.iter().any(|c| c == "chat.read"), "{me}");
    assert!(!caps.iter().any(|c| c == "chat.write"), "{me}");

    let create = |node: &str| json!({"project":"demo","purpose":"discuss","node":node,"provider":"hermes","request_id":request_id()});
    post(
        &client,
        &base,
        &viewer,
        "/conversations",
        create(&task),
        403,
    )
    .await;
    post(
        &client,
        &base,
        &editor,
        "/conversations",
        create(&task),
        403,
    )
    .await;
    post(
        &client,
        &base,
        &artifacts,
        "/conversations",
        create(&task),
        403,
    )
    .await;
    post(
        &client,
        &base,
        &viewer,
        "/manager/chat/launch",
        json!({"project_id":"demo","provider":"hermes"}),
        403,
    )
    .await;

    // Dispatch purposes are daemon-created: pre-owning one would let a member
    // steer the admin's worker.
    post(
        &client,
        &base,
        &anna,
        "/conversations",
        json!({"project":"demo","purpose":"implement","node":task,"provider":"hermes","request_id":request_id()}),
        400,
    )
    .await;
    let annas = post(&client, &base, &anna, "/conversations", create(&task), 200).await;
    let conv = annas["id"].as_str().unwrap().to_owned();
    let run = annas["run_id"].as_str().unwrap().to_owned();
    let doc = get(
        &client,
        &base,
        &viewer,
        &format!("/org/node?project=demo&id={conv}"),
        200,
    )
    .await;
    assert_eq!(property(&doc, "OWNER"), r#"["member","anna"]"#);

    let input = |text: &str| json!({"message":text,"request_id":request_id()});
    post(
        &client,
        &base,
        &viewer,
        &format!("/conversations/{conv}/input?project=demo"),
        input("viewer"),
        403,
    )
    .await;
    post(
        &client,
        &base,
        &editor,
        &format!("/conversations/{conv}/input?project=demo"),
        input("editor"),
        403,
    )
    .await;
    post(
        &client,
        &base,
        &bob,
        &format!("/conversations/{conv}/input?project=demo"),
        input("bob"),
        403,
    )
    .await;
    let own = format!("anna-says-{}", request_id());
    let sent = post(
        &client,
        &base,
        &anna,
        &format!("/conversations/{conv}/input?project=demo"),
        input(&own),
        200,
    )
    .await;
    assert_eq!(sent["mode"], "live");
    wait_for_log(&log, &own).await;
    let admin_says = format!("admin-says-{}", request_id());
    post(
        &client,
        &base,
        &token,
        &format!("/conversations/{conv}/input?project=demo"),
        input(&admin_says),
        200,
    )
    .await;
    wait_for_log(&log, &admin_says).await;

    // Run reads: sessions.watch plus chat.read on a conversation run.
    get(&client, &base, &viewer, &format!("/runs/{run}"), 200).await;
    get(&client, &base, &artifacts, &format!("/runs/{run}"), 403).await;

    // Live list for members: sessions.watch shows every run, chat.read alone
    // shows only conversation runs, and nothing is ever a 403.
    let reader = orgasmic_core::add_member_with_actions(
        &home,
        "reader",
        &[("demo".into(), "artifacts".into())],
        &["chat.read".into(), "graph.read".into()],
    )
    .unwrap();
    let registered = post(
        &client,
        &base,
        &token,
        "/manager/register",
        json!({"project_id":"demo","pid":std::process::id()}),
        200,
    )
    .await;
    let manager_run = registered["run_id"].as_str().unwrap().to_owned();
    let ids = |live: &Value| -> Vec<String> {
        live["live"]
            .as_array()
            .unwrap()
            .iter()
            .map(|r| r["run_id"].as_str().unwrap().to_owned())
            .collect()
    };
    let seen = ids(&get(&client, &base, &reader, "/runs/live", 200).await);
    assert!(seen.contains(&run), "{seen:?}");
    assert!(!seen.contains(&manager_run), "{seen:?}");
    let seen = ids(&get(&client, &base, &viewer, "/runs/live", 200).await);
    assert!(
        seen.contains(&run) && seen.contains(&manager_run),
        "{seen:?}"
    );
    let seen = ids(&get(&client, &base, &artifacts, "/runs/live", 200).await);
    assert!(seen.is_empty(), "{seen:?}");
    let public = get(&client, &base, &reader, &format!("/runs/{run}"), 200).await;
    assert!(public["run"]["session_path"].is_null(), "{public}");
    assert!(public["run"]["worktree"].is_null(), "{public}");
    assert_eq!(public["run"]["task_id"], conv);
    let admin_view = get(&client, &base, &token, &format!("/runs/{run}"), 200).await;
    assert!(
        admin_view["run"]["session_path"].is_string(),
        "{admin_view}"
    );
    let listed = get(&client, &base, &reader, "/runs/live", 200).await;
    assert!(listed["live"][0]["session_path"].is_null(), "{listed}");
    get(
        &client,
        &base,
        &reader,
        &format!("/runs/{manager_run}"),
        403,
    )
    .await;
    let shim = post(
        &client,
        &base,
        &anna,
        "/manager/chat/launch",
        json!({"project_id":"demo","provider":"hermes"}),
        200,
    )
    .await;
    assert!(shim["conversation_id"]
        .as_str()
        .unwrap()
        .starts_with("CONV-"));

    // Plugins: chat.read, and only conversations about their own collection.
    let about_meeting = post(
        &client,
        &base,
        &token,
        "/conversations",
        create(&meeting),
        200,
    )
    .await["id"]
        .as_str()
        .unwrap()
        .to_owned();
    let lease = post(
        &client,
        &base,
        &editor,
        "/plugins/meetings/run",
        json!({"project":"demo","command":"import"}),
        200,
    )
    .await;
    let plugin = lease["token"].as_str().unwrap();
    get(
        &client,
        &base,
        plugin,
        &format!("/org/node?project=demo&id={about_meeting}"),
        200,
    )
    .await;
    get(
        &client,
        &base,
        plugin,
        &format!("/org/node?project=demo&id={conv}"),
        403,
    )
    .await;
    get(
        &client,
        &base,
        plugin,
        &format!("/links?project=demo&node={about_meeting}"),
        200,
    )
    .await;
    get(
        &client,
        &base,
        plugin,
        &format!("/links?project=demo&node={conv}"),
        403,
    )
    .await;
    let listed = get(
        &client,
        &base,
        plugin,
        "/graph/nodes?project=demo&layer=conversations",
        200,
    )
    .await;
    let ids: Vec<&str> = listed
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|n| n["id"].as_str())
        .collect();
    assert!(ids.contains(&about_meeting.as_str()), "{listed}");
    assert!(!ids.contains(&conv.as_str()), "{listed}");
    post(
        &client,
        &base,
        plugin,
        "/conversations",
        create(&meeting),
        403,
    )
    .await;
    post(
        &client,
        &base,
        plugin,
        &format!("/conversations/{about_meeting}/input?project=demo"),
        input("plugin"),
        403,
    )
    .await;
    let doc = get(
        &client,
        &base,
        plugin,
        &format!("/org/node?project=demo&id={about_meeting}"),
        200,
    )
    .await;
    let base_version = doc["source"]["base_version"].as_str().unwrap().to_owned();
    post(
        &client,
        &base,
        plugin,
        &format!("/org/node/{about_meeting}/edit"),
        json!({"project":"demo","base_version":base_version,"ops":[{"op":"set_property","key":"MODEL","value":"x"}]}),
        403,
    )
    .await;
    post(
        &client,
        &base,
        plugin,
        &format!("/org/node/{about_meeting}/edit"),
        json!({"project":"demo","base_version":base_version,"ops":[{"op":"set_title","title":"Standup notes"}]}),
        200,
    )
    .await;
    let doc = get(
        &client,
        &base,
        &token,
        &format!("/org/node?project=demo&id={about_meeting}"),
        200,
    )
    .await;
    assert_eq!(doc["title"], "Standup notes");

    // Stop run: the owner with chat.write, or an admin; nobody else, and no
    // member stops a non-conversation run.
    post(
        &client,
        &base,
        &bob,
        &format!("/runs/{run}/release"),
        json!({}),
        403,
    )
    .await;
    post(
        &client,
        &base,
        &viewer,
        &format!("/runs/{run}/release"),
        json!({}),
        403,
    )
    .await;
    post(
        &client,
        &base,
        &anna,
        &format!("/runs/{manager_run}/release"),
        json!({}),
        403,
    )
    .await;
    post(
        &client,
        &base,
        &anna,
        &format!("/runs/{run}/release"),
        json!({"finalized_by_worker":true}),
        403,
    )
    .await;
    release_run(&client, &base, &anna, &run).await;

    let _ = running.shutdown.send(());
}
