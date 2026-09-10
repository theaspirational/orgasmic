use orgasmic_core::Home;
use orgasmic_daemon::{Daemon, DaemonOptions, RunningDaemon};
use serde_json::{json, Value};
use std::path::Path;
use std::process::{Command, Output};

fn write(path: impl AsRef<Path>, source: &str) {
    let path = path.as_ref();
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, source).unwrap();
}

fn git(cwd: &Path, args: &[&str]) -> Output {
    Command::new("git")
        .args(args)
        .current_dir(cwd)
        .output()
        .unwrap()
}

fn git_ok(cwd: &Path, args: &[&str]) -> String {
    let output = git(cwd, args);
    assert!(
        output.status.success(),
        "git {args:?}: {}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stdout).unwrap()
}
async fn post(
    client: &reqwest::Client,
    base: &str,
    token: &str,
    path: &str,
    payload: Value,
    expected: u16,
) -> Value {
    let res = client
        .post(format!("{base}/api{path}"))
        .bearer_auth(token)
        .json(&payload)
        .send()
        .await
        .unwrap();
    let status = res.status().as_u16();
    let text = res.text().await.unwrap();
    let diagnostic =
        if path.ends_with("/run") || path.contains("/access") || path.contains("/auth/") {
            "[credential response omitted]"
        } else {
            &text
        };
    assert_eq!(status, expected, "POST {path}: {diagnostic}");
    serde_json::from_str(&text).unwrap_or(Value::Null)
}
async fn get(client: &reqwest::Client, base: &str, token: &str, path: &str) -> Value {
    let res = client
        .get(format!("{base}/api{path}"))
        .bearer_auth(token)
        .send()
        .await
        .unwrap();
    let status = res.status();
    let text = res.text().await.unwrap();
    assert!(status.is_success(), "GET {path}: {status}: {text}");
    serde_json::from_str(&text).unwrap()
}
async fn fixture_with(
    attachment_storage: Option<&str>,
    remote_backed: bool,
) -> (tempfile::TempDir, Home, RunningDaemon, String, String) {
    let _ = tracing_subscriber::fmt()
        .with_env_filter("error")
        .with_test_writer()
        .try_init();
    let temp = tempfile::tempdir().unwrap();
    let home = Home::at(temp.path().join("home"));
    home.ensure().unwrap();
    let root = temp.path().join("project");
    let storage = attachment_storage
        .map(|value| format!(":ATTACHMENT_STORAGE: {value}\n"))
        .unwrap_or_default();
    write(
        root.join(".orgasmic/project.org"),
        &format!("* PROJECT demo\n:PROPERTIES:\n:ID: demo\n{storage}:END:\n"),
    );
    if remote_backed {
        let remote = temp.path().join("remote.git");
        std::fs::create_dir(&remote).unwrap();
        git_ok(&remote, &["init", "--bare"]);
        git_ok(&root, &["init", "-b", "orgasmic"]);
        git_ok(&root, &["add", ".orgasmic/project.org"]);
        git_ok(
            &root,
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
        git_ok(
            &root,
            &["remote", "add", "origin", remote.to_str().unwrap()],
        );
        git_ok(&root, &["push", "-u", "origin", "orgasmic"]);
    }
    write(
        home.board(),
        &format!(
            "* PROJECT demo\n:PROPERTIES:\n:ID: demo\n:PATH: {}\n:BRANCH: {}\n:END:\n",
            root.display(),
            if remote_backed { "orgasmic" } else { "main" }
        ),
    );
    let example = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../examples/plugins/meetings");
    for file in [
        "plugin.org",
        "prompts/meeting-chat.org",
        "ui/index.js",
        "ui/player.js",
        "bin/import",
    ] {
        let dest = home.user().join("plugins/meetings").join(file);
        std::fs::create_dir_all(dest.parent().unwrap()).unwrap();
        std::fs::copy(example.join(file), dest).unwrap();
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
    post(&client, &base, &token, "/plugins/meetings/activation", json!({"project":"demo", "enabled":true, "approved_capabilities":["nodes.read","nodes.write","links.read","links.write","attachments.read","attachments.write","ui.execute"]}), 200).await;
    (temp, home, running, base, token)
}

async fn fixture() -> (tempfile::TempDir, Home, RunningDaemon, String, String) {
    fixture_with(None, false).await
}
async fn member_cookie(client: &reqwest::Client, base: &str, token: &str) -> String {
    let response = client
        .post(format!("{base}/api/login"))
        .json(&json!({"token":token}))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 200);
    response.headers()["set-cookie"]
        .to_str()
        .unwrap()
        .split(';')
        .next()
        .unwrap()
        .to_owned()
}
// A real PCM WAV; the large smoke streams silence without allocating the whole recording.
fn wav_chunk(size: u64, chunk_size: usize) -> Vec<u8> {
    let mut bytes = vec![0; chunk_size];
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
    bytes
}

async fn upload_text(client: &reqwest::Client, base: &str, token: &str) -> (String, String, Value) {
    let node = post(
        client,
        base,
        token,
        "/org/node",
        json!({"project":"demo","kind":"meetings","title":"Attachment storage"}),
        200,
    )
    .await["id"]
        .as_str()
        .unwrap()
        .to_owned();
    let id = uuid::Uuid::new_v4().to_string();
    post(
        client,
        base,
        token,
        "/attachments/uploads",
        json!({"project":"demo","node":node,"name":"payload.txt","media_type":"text/plain","size":8,"request_id":id}),
        200,
    )
    .await;
    let response = client
        .put(format!(
            "{base}/api/attachments/uploads/{id}?project=demo&offset=0"
        ))
        .bearer_auth(token)
        .body("payload\n")
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 200, "{}", response.text().await.unwrap());
    let asset = post(
        client,
        base,
        token,
        &format!("/attachments/uploads/{id}/finish?project=demo"),
        json!({}),
        200,
    )
    .await;
    (node, id, asset)
}

async fn exercise(size: u64, adversarial: bool) {
    let (temp, home, mut running, mut base, token) = fixture().await;
    let client = reqwest::Client::new();
    let meeting = post(
        &client,
        &base,
        &token,
        "/org/node",
        json!({"project":"demo","kind":"meetings","title":"Recorded planning"}),
        200,
    )
    .await["id"]
        .as_str()
        .unwrap()
        .to_owned();
    let task = post(
        &client,
        &base,
        &token,
        "/org/node",
        json!({"project":"demo","kind":"task","title":"Follow up"}),
        200,
    )
    .await["id"]
        .as_str()
        .unwrap()
        .to_owned();
    let editor =
        orgasmic_core::add_member(&home, "editor", &[("demo".into(), "editor".into())]).unwrap();
    let viewer =
        orgasmic_core::add_member(&home, "viewer", &[("demo".into(), "viewer".into())]).unwrap();
    let foreign =
        orgasmic_core::add_member(&home, "foreign", &[("other".into(), "editor".into())]).unwrap();
    let id = uuid::Uuid::new_v4().to_string();
    let start = json!({"project":"demo","node":meeting,"name":"Planning.wav","media_type":"audio/wav","size":size,"request_id":id});
    post(
        &client,
        &base,
        &viewer,
        "/attachments/uploads",
        start.clone(),
        403,
    )
    .await;
    post(
        &client,
        &base,
        &editor,
        "/attachments/uploads",
        start.clone(),
        200,
    )
    .await;
    post(&client, &base, &editor, "/attachments/uploads", start, 200).await;
    let chunk_size = 4 * 1024 * 1024;
    let first = wav_chunk(size, chunk_size);
    for offset in (0..size).step_by(chunk_size) {
        let bytes = if offset == 0 {
            first.clone()
        } else {
            vec![0; (size - offset).min(chunk_size as u64) as usize]
        };
        let response = client
            .put(format!(
                "{base}/api/attachments/uploads/{id}?project=demo&offset={offset}"
            ))
            .bearer_auth(&editor)
            .body(bytes)
            .send()
            .await
            .unwrap();
        let status = response.status();
        let body = response.text().await.unwrap();
        assert_eq!(status, 200, "chunk at {offset}: {body}");
        if adversarial && offset == 0 {
            let response = client
                .put(format!(
                    "{base}/api/attachments/uploads/{id}?project=demo&offset=0"
                ))
                .bearer_auth(&editor)
                .body(vec![0; 100])
                .send()
                .await
                .unwrap();
            assert_eq!(response.status(), 409);
            let response = client
                .put(format!(
                    "{base}/api/attachments/uploads/{id}?project=demo&offset={chunk_size}"
                ))
                .bearer_auth(&editor)
                .body(vec![0; chunk_size + 1])
                .send()
                .await
                .unwrap();
            assert_eq!(response.status(), 413);
        }
    }
    let upload = get(
        &client,
        &base,
        &editor,
        &format!("/attachments/uploads/{id}?project=demo"),
    )
    .await;
    assert_eq!(upload["offset"], size);
    let finish = format!("/attachments/uploads/{id}/finish?project=demo");
    post(
        &client,
        &base,
        &editor,
        &finish,
        json!({"sha256":"incorrect"}),
        400,
    )
    .await;
    let asset = post(&client, &base, &editor, &finish, json!({}), 200).await;
    assert!(asset["machine"].as_str().is_some());
    assert_eq!(
        post(&client, &base, &editor, &finish, json!({}), 200).await,
        asset
    );
    let revision = asset["revision"].as_str().unwrap();
    let content = format!("/attachments/{meeting}/{id}/{revision}/content?project=demo");
    let response = client
        .get(format!("{base}/api{content}"))
        .bearer_auth(&viewer)
        .header("Range", "bytes=44-99")
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 206);
    assert_eq!(
        response.headers()["content-range"],
        format!("bytes 44-99/{size}")
    );
    assert_eq!(response.headers()["x-content-type-options"], "nosniff");
    assert_eq!(response.headers()["content-disposition"], "attachment");
    assert_eq!(response.bytes().await.unwrap().len(), 56);
    let response = client
        .head(format!("{base}/api{content}"))
        .bearer_auth(&viewer)
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 200);
    assert_eq!(response.headers()["content-length"], size.to_string());
    let response = client
        .get(format!("{base}/api{content}"))
        .bearer_auth(&viewer)
        .header("Range", format!("bytes={size}-"))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 416);
    assert_eq!(
        client
            .get(format!("{base}/api{content}"))
            .bearer_auth(&foreign)
            .send()
            .await
            .unwrap()
            .status(),
        403
    );
    let cookie = member_cookie(&client, &base, &viewer).await;
    let url = format!("{base}/api{content}");
    let ticket = post(&client, &base, &token, "/auth/ui-session", json!({}), 200).await;
    let response = reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .unwrap()
        .get(format!("{base}{}", ticket["path"].as_str().unwrap()))
        .send()
        .await
        .unwrap();
    assert!(response.status().is_redirection());
    let cookie_header = response.headers()["set-cookie"].to_str().unwrap();
    assert!(cookie_header.contains("Max-Age=43200"));
    let admin_cookie = cookie_header.split(';').next().unwrap();
    assert_eq!(
        client
            .get(&url)
            .header("Cookie", admin_cookie)
            .header("Range", "bytes=0-10")
            .send()
            .await
            .unwrap()
            .status(),
        206
    );
    assert_eq!(client.get(&url).send().await.unwrap().status(), 401);
    assert_eq!(
        client
            .get(&url)
            .header("Cookie", &cookie)
            .header("Range", "bytes=-10")
            .send()
            .await
            .unwrap()
            .status(),
        206
    );
    let other = url.replace("project=demo", "project=other");
    assert_eq!(
        client
            .get(other)
            .header("Cookie", &cookie)
            .send()
            .await
            .unwrap()
            .status(),
        404
    );
    let link = json!({"project":"demo","source":meeting,"target":task,"kind":"RELATES_TO","anchors":[{"attachment":id,"revision":revision,"start_ms":1000,"end_ms":2000,"label":"Follow up"}],"base_revision":0,"request_id":"link-create"});
    post(&client, &base, &viewer, "/links", link.clone(), 403).await;
    post(&client, &base, &editor, "/links", link.clone(), 200).await;
    post(&client, &base, &editor, "/links", link.clone(), 200).await;
    let mut conflict = link.clone();
    conflict["request_id"] = json!("conflict");
    post(&client, &base, &editor, "/links", conflict, 409).await;
    let incoming = get(
        &client,
        &base,
        &viewer,
        &format!("/links?project=demo&node={task}&incoming=true"),
    )
    .await;
    assert_eq!(incoming.as_array().unwrap().len(), 1);
    assert_eq!(incoming[0]["anchors"][0]["start_ms"], 1000);
    let ledger = temp.path().join("project/.orgasmic");
    assert!(!ledger.join("attachments").exists());
    assert!(home.root.join("assets").is_dir());
    let node_blob = ledger.join(format!("meetings/{meeting}/attachments/{revision}"));
    assert_eq!(std::fs::metadata(&node_blob).unwrap().len(), size);
    let metadata =
        std::fs::read_to_string(ledger.join(format!("meetings/{meeting}/attachments.org")))
            .unwrap();
    assert!(metadata.contains(revision) && !metadata.contains("/assets/"));
    if adversarial {
        let duplicate_id = uuid::Uuid::new_v4().to_string();
        post(&client, &base, &editor, "/attachments/uploads", json!({"project":"demo","node":meeting,"name":"Planning copy.wav","media_type":"audio/wav","size":size,"request_id":duplicate_id}), 200).await;
        for offset in (0..size).step_by(chunk_size) {
            let bytes = if offset == 0 {
                first.clone()
            } else {
                vec![0; (size - offset).min(chunk_size as u64) as usize]
            };
            assert_eq!(
                client
                    .put(format!("{base}/api/attachments/uploads/{duplicate_id}?project=demo&offset={offset}"))
                    .bearer_auth(&editor)
                    .body(bytes)
                    .send()
                    .await
                    .unwrap()
                    .status(),
                200
            );
        }
        assert_eq!(
            post(
                &client,
                &base,
                &editor,
                &format!("/attachments/uploads/{duplicate_id}/finish?project=demo"),
                json!({}),
                200,
            )
            .await["revision"],
            revision
        );
        let artifact_reader = orgasmic_core::add_member(
            &home,
            "artifact-reader",
            &[("demo".into(), "artifacts".into())],
        )
        .unwrap();
        assert_eq!(
            client
                .get(format!("{base}/api{content}"))
                .bearer_auth(&artifact_reader)
                .header("Range", "bytes=0-10")
                .send()
                .await
                .unwrap()
                .status(),
            403
        );
        let lease = post(
            &client,
            &base,
            &editor,
            "/plugins/meetings/run",
            json!({"project":"demo","command":"import"}),
            200,
        )
        .await;
        let plugin_token = lease["token"].as_str().unwrap();
        post(&client, &base, plugin_token, "/attachments/uploads", json!({"project":"demo","node":task,"name":"wrong.wav","media_type":"audio/wav","size":100,"request_id":uuid::Uuid::new_v4().to_string()}), 403).await;
        let mut foreign_link = link.clone();
        foreign_link["source"] = json!(task);
        foreign_link["target"] = json!(meeting);
        foreign_link["anchors"] = json!([]);
        post(&client, &base, plugin_token, "/links", foreign_link, 403).await;
        let upload_id = uuid::Uuid::new_v4().to_string();
        post(&client, &base, &editor, "/attachments/uploads", json!({"project":"demo","node":meeting,"name":"malformed.wav","media_type":"audio/wav","size":8,"request_id":upload_id}), 200).await;
        let chunk_url = format!("{base}/api/attachments/uploads/{upload_id}?project=demo&offset=0");
        let (a, b) = tokio::join!(
            client
                .put(&chunk_url)
                .bearer_auth(&editor)
                .body("<script>")
                .send(),
            client
                .put(&chunk_url)
                .bearer_auth(&editor)
                .body("<script>")
                .send()
        );
        let mut statuses = [a.unwrap().status().as_u16(), b.unwrap().status().as_u16()];
        statuses.sort();
        assert_eq!(statuses, [200, 409]);
        post(
            &client,
            &base,
            &editor,
            &format!("/attachments/uploads/{upload_id}/finish?project=demo"),
            json!({}),
            400,
        )
        .await;
        assert_eq!(
            client
                .delete(format!(
                    "{base}/api/attachments/uploads/{upload_id}?project=demo"
                ))
                .bearer_auth(&editor)
                .send()
                .await
                .unwrap()
                .status(),
            200
        );
        let mut a = link.clone();
        a["base_revision"] = json!(1);
        a["request_id"] = json!("race-a");
        let mut b = a.clone();
        b["request_id"] = json!("race-b");
        let endpoint = format!("{base}/api/links");
        let (a, b) = tokio::join!(
            client.post(&endpoint).bearer_auth(&editor).json(&a).send(),
            client.post(&endpoint).bearer_auth(&editor).json(&b).send()
        );
        let mut statuses = [a.unwrap().status().as_u16(), b.unwrap().status().as_u16()];
        statuses.sort();
        assert_eq!(statuses, [200, 409]);
        // Both extras journal under the source node and survive a cold index rebuild.
        let journal =
            std::fs::read_to_string(ledger.join(format!("meetings/{meeting}/journal.org")))
                .unwrap();
        let entries = orgasmic_core::node_kernel::parse_journal(&journal, "journal.org").unwrap();
        for ty in ["attachment.created", "link.updated"] {
            assert!(entries.iter().any(|e| e.ty == ty && e.actor == "editor"));
        }
        let resume_id = uuid::Uuid::new_v4().to_string();
        post(&client, &base, &editor, "/attachments/uploads", json!({"project":"demo","node":meeting,"name":"resume.txt","media_type":"text/plain","size":8,"request_id":resume_id}), 200).await;
        assert_eq!(
            client
                .put(format!(
                    "{base}/api/attachments/uploads/{resume_id}?project=demo&offset=0"
                ))
                .bearer_auth(&editor)
                .body("abcd")
                .send()
                .await
                .unwrap()
                .status(),
            200
        );
        let _ = running.shutdown.send(());
        running.join.await.unwrap();
        let assets = home.root.join("assets");
        let store = std::fs::read_dir(&assets)
            .unwrap()
            .next()
            .unwrap()
            .unwrap()
            .path();
        let legacy_blob = store.join(format!("blobs/{revision}"));
        std::fs::create_dir_all(legacy_blob.parent().unwrap()).unwrap();
        std::fs::rename(&node_blob, &legacy_blob).unwrap();
        let moved = temp.path().join("renamed-project");
        std::fs::rename(temp.path().join("project"), &moved).unwrap();
        write(
            home.board(),
            &format!(
                "* PROJECT demo\n:PROPERTIES:\n:ID: demo\n:PATH: {}\n:BRANCH: main\n:END:\n",
                moved.display()
            ),
        );
        running = Daemon::run(
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
        base = format!("http://{}", running.addr);
        let migrated_blob = moved.join(format!(
            ".orgasmic/meetings/{meeting}/attachments/{revision}"
        ));
        assert!(legacy_blob.exists());
        assert_eq!(std::fs::metadata(migrated_blob).unwrap().len(), size);
        let resumed = get(
            &client,
            &base,
            &editor,
            &format!("/attachments/uploads/{resume_id}?project=demo"),
        )
        .await;
        assert_eq!(resumed["offset"], 4);
        assert_eq!(
            client
                .put(format!(
                    "{base}/api/attachments/uploads/{resume_id}?project=demo&offset=4"
                ))
                .bearer_auth(&editor)
                .body("efgh")
                .send()
                .await
                .unwrap()
                .status(),
            200
        );
        let complete = post(
            &client,
            &base,
            &editor,
            &format!("/attachments/uploads/{resume_id}/finish?project=demo"),
            json!({}),
            200,
        )
        .await;
        assert_eq!(complete["size"], 8);
        // Finished receipts must not consume the pending-upload slot limit.
        let receipt =
            std::fs::read_to_string(store.join(format!("uploads/{resume_id}/state.json"))).unwrap();
        for n in 0..4096 {
            write(
                store.join(format!("uploads/receipt-{n}/state.json")),
                &receipt,
            );
        }
        post(&client, &base, &editor, "/attachments/uploads", json!({"project":"demo","node":meeting,"name":"after-receipts.txt","media_type":"text/plain","size":8,"request_id":uuid::Uuid::new_v4().to_string()}), 200).await;
        let reloaded = get(
            &client,
            &base,
            &viewer,
            &format!("/links?project=demo&node={task}&incoming=true"),
        )
        .await;
        assert_eq!(reloaded[0]["revision"], 2);
        assert_eq!(
            client
                .get(format!("{base}/api{content}"))
                .bearer_auth(&viewer)
                .header("Range", "bytes=1024-2047")
                .send()
                .await
                .unwrap()
                .status(),
            206
        );
        let cookie = member_cookie(&client, &base, &viewer).await;
        let url = format!("{base}/api{content}");
        let bad_id = uuid::Uuid::new_v4().to_string();
        post(&client, &base, &editor, "/attachments/uploads", json!({"project":"demo","node":meeting,"name":"x.html","media_type":"text/html","size":100,"request_id":bad_id}), 400).await;
        post(&client, &base, &editor, "/attachments/uploads", json!({"project":"demo","node":meeting,"name":"large.wav","media_type":"audio/wav","size":9u64*1024*1024*1024,"request_id":bad_id}), 400).await;
        let mut invalid = link.clone();
        invalid["base_revision"] = json!(2);
        invalid["request_id"] = json!("invalid-anchor");
        invalid["anchors"][0]["end_ms"] = json!(0);
        post(&client, &base, &editor, "/links", invalid, 400).await;
        post(
            &client,
            &base,
            &token,
            "/plugins/meetings/activation",
            json!({"project":"demo","enabled":false}),
            200,
        )
        .await;
        post(&client, &base, &editor, "/links", link, 400).await;
        assert_eq!(
            client
                .get(&url)
                .header("Cookie", &cookie)
                .header("Range", "bytes=0-10")
                .send()
                .await
                .unwrap()
                .status(),
            206
        );
        orgasmic_core::revoke_member(&home, "viewer").unwrap();
        assert_eq!(
            client
                .get(&url)
                .header("Cookie", &cookie)
                .header("Range", "bytes=0-10")
                .send()
                .await
                .unwrap()
                .status(),
            401
        );
    }
    if std::env::var_os("ORGASMIC_SERVICE_BROWSER_SMOKE").is_some() {
        let session = post(&client, &base, &token, "/auth/ui-session", json!({}), 200).await;
        // Do not print the one-use ticket: leave it in a mode-0600 fixture file.
        let path = temp.path().join("browser-session.json");
        std::fs::write(&path, serde_json::to_vec(&json!({"url":format!("{base}{}",session["path"].as_str().unwrap()), "base":base,"meeting":meeting,"task":task})).unwrap()).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
        }
        eprintln!("Browser fixture: {}", path.display());
        // Browser automation creates this marker only after its assertions pass.
        while !temp.path().join("browser-done").exists() {
            tokio::select! {
                _ = tokio::signal::ctrl_c() => panic!("browser verification interrupted"),
                _ = tokio::time::sleep(std::time::Duration::from_secs(1)) => {}
            }
        }
    }
    let _ = running.shutdown.send(());
    running.join.await.unwrap();
}

#[tokio::test]
async fn node_services_authorize_stream_and_index() {
    exercise(8 * 1024 * 1024, true).await;
}

#[tokio::test]
async fn attachment_storage_modes_stage_migrate_validate_and_explain_missing_payloads() {
    let (temp, _home, running, base, token) = fixture_with(None, true).await;
    let root = temp.path().join("project");
    let client = reqwest::Client::new();
    let project = get(
        &client,
        &base,
        &token,
        "/org/node?project=demo&id=demo&kind=project",
    )
    .await;
    let response = client
        .post(format!("{base}/api/org/node/demo/edit"))
        .bearer_auth(&token)
        .json(&json!({
            "project":"demo",
            "kind":"project",
            "base_version":project["source"]["base_version"],
            "ops":[{"op":"set_property","key":"ATTACHMENT_STORAGE","value":"cloud"}]
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 400);
    let error = response.text().await.unwrap();
    assert!(error.contains("ATTACHMENT_STORAGE"), "{error}");
    assert!(error.contains("cloud"), "{error}");
    assert!(error.contains("lfs and local"), "{error}");
    assert!(error.contains("orgasmic node prop set"), "{error}");

    let (node, id, asset) = upload_text(&client, &base, &token).await;
    let revision = asset["revision"].as_str().unwrap().to_owned();
    let payload = format!(".orgasmic/meetings/{node}/attachments/{revision}");
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(10);
    loop {
        if git(&root, &["show", &format!("origin/orgasmic:{payload}")])
            .status
            .success()
        {
            break;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "lfs attachment payload did not reach the remote"
        );
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    }
    let tree = git_ok(&root, &["ls-tree", "-r", "--name-only", "origin/orgasmic"]);
    assert!(tree.lines().any(|path| path == payload));

    write(
        root.join(".orgasmic/project.org"),
        "* PROJECT demo\n:PROPERTIES:\n:ID: demo\n:ATTACHMENT_STORAGE: LFS\n:END:\n",
    );
    let content = format!("{base}/api/attachments/{node}/{id}/{revision}/content?project=demo");
    let response = client
        .get(&content)
        .bearer_auth(&token)
        .send()
        .await
        .unwrap();
    let status = response.status();
    let body = response.text().await.unwrap();
    assert_eq!(status, 200, "{body}");
    std::fs::remove_file(root.join(&payload)).unwrap();
    let response = client
        .get(content)
        .bearer_auth(&token)
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 404);
    assert!(response
        .text()
        .await
        .unwrap()
        .contains("attachment payload missing; run git lfs pull in the ledger"));
    let _ = running.shutdown.send(());
    running.join.await.unwrap();

    let (temp, home, mut running, mut base, token) = fixture_with(Some("local"), true).await;
    let root = temp.path().join("project");
    let (node, id, asset) = upload_text(&client, &base, &token).await;
    let revision = asset["revision"].as_str().unwrap().to_owned();
    let machine = asset["machine"].as_str().unwrap().to_owned();
    let metadata = format!(".orgasmic/meetings/{node}/attachments.org");
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(10);
    loop {
        let object = format!("origin/orgasmic:{metadata}");
        if git(&root, &["show", &object]).status.success() {
            break;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "local attachment record did not reach the remote"
        );
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    }
    let tree = git_ok(&root, &["ls-tree", "-r", "--name-only", "origin/orgasmic"]);
    assert!(tree.lines().any(|path| path == metadata));
    assert!(
        !tree.lines().any(|path| path.ends_with(&revision)),
        "local payload reached the remote tree: {tree}"
    );

    let _ = running.shutdown.send(());
    running.join.await.unwrap();
    let node_blob = root.join(format!(".orgasmic/meetings/{node}/attachments/{revision}"));
    let store = std::fs::read_dir(home.root.join("assets"))
        .unwrap()
        .next()
        .unwrap()
        .unwrap()
        .path();
    let legacy_blob = store.join(format!("blobs/{revision}"));
    std::fs::create_dir_all(legacy_blob.parent().unwrap()).unwrap();
    std::fs::rename(&node_blob, &legacy_blob).unwrap();
    running = Daemon::run(
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
    base = format!("http://{}", running.addr);
    assert!(legacy_blob.exists());
    assert!(
        node_blob.exists(),
        "local boot migration did not link the blob"
    );
    std::fs::remove_file(&node_blob).unwrap();
    let response = client
        .get(format!(
            "{base}/api/attachments/{node}/{id}/{revision}/content?project=demo"
        ))
        .bearer_auth(&token)
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 404);
    let error = response.text().await.unwrap();
    assert!(error.contains(&format!(
        "attachment payload is missing from this machine's ledger at .orgasmic/meetings/{node}/attachments/{revision}; this project stores attachments locally, not in git; restore that file from backup or upload it again"
    )));
    let _ = running.shutdown.send(());
    running.join.await.unwrap();

    std::fs::remove_file(&legacy_blob).unwrap();
    let records = root.join(&metadata);
    let source = std::fs::read_to_string(&records).unwrap();
    write(
        &records,
        &source.replace(
            &format!(":MACHINE: {machine}\n"),
            ":MACHINE: foreign-machine\n",
        ),
    );
    running = Daemon::run(
        home,
        DaemonOptions {
            bind_override: Some("127.0.0.1".parse().unwrap()),
            port_override: Some(0),
            fs_watcher_enabled: false,
            ..Default::default()
        },
    )
    .await
    .unwrap();
    base = format!("http://{}", running.addr);
    let response = client
        .get(format!(
            "{base}/api/attachments/{node}/{id}/{revision}/content?project=demo"
        ))
        .bearer_auth(&token)
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 404);
    let error = response.text().await.unwrap();
    assert!(error.contains(&format!(
        "attachment payload is not on this machine (uploaded on machine foreign-machine); this project stores attachments locally, not in git; copy the payload from that machine to .orgasmic/meetings/{node}/attachments/{revision}"
    )));
    let _ = running.shutdown.send(());
    running.join.await.unwrap();
}

#[tokio::test]
async fn lfs_mode_without_git_lfs_names_both_fixes() {
    const CHILD: &str = "ORGASMIC_TEST_NO_GIT_LFS";
    if std::env::var_os(CHILD).is_none() {
        let bin = tempfile::tempdir().unwrap();
        let git = std::env::split_paths(&std::env::var_os("PATH").unwrap())
            .map(|dir| dir.join("git"))
            .find(|path| path.is_file())
            .expect("git on PATH");
        #[cfg(unix)]
        std::os::unix::fs::symlink(git, bin.path().join("git")).unwrap();
        let output = Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "lfs_mode_without_git_lfs_names_both_fixes",
                "--nocapture",
            ])
            .env(CHILD, "1")
            .env("PATH", bin.path())
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_SYSTEM", "/dev/null")
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        return;
    }

    let lfs = Command::new("git")
        .args(["lfs", "version"])
        .output()
        .unwrap();
    assert!(
        !lfs.status.success(),
        "git lfs unexpectedly available in no-lfs child: {}{}",
        String::from_utf8_lossy(&lfs.stdout),
        String::from_utf8_lossy(&lfs.stderr)
    );
    let (_temp, _home, running, base, token) = fixture_with(None, true).await;
    let client = reqwest::Client::new();
    let node = post(
        &client,
        &base,
        &token,
        "/org/node",
        json!({"project":"demo","kind":"meetings","title":"No LFS"}),
        200,
    )
    .await["id"]
        .as_str()
        .unwrap()
        .to_owned();
    let id = uuid::Uuid::new_v4().to_string();
    post(
        &client,
        &base,
        &token,
        "/attachments/uploads",
        json!({"project":"demo","node":node,"name":"payload.txt","media_type":"text/plain","size":8,"request_id":id}),
        200,
    )
    .await;
    assert_eq!(
        client
            .put(format!(
                "{base}/api/attachments/uploads/{id}?project=demo&offset=0"
            ))
            .bearer_auth(&token)
            .body("payload\n")
            .send()
            .await
            .unwrap()
            .status(),
        200
    );
    let response = client
        .post(format!(
            "{base}/api/attachments/uploads/{id}/finish?project=demo"
        ))
        .bearer_auth(&token)
        .json(&json!({}))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 503);
    let error = response.text().await.unwrap();
    assert!(error.contains("install git-lfs"), "{error}");
    assert!(
        error.contains(
            "orgasmic node prop set demo ATTACHMENT_STORAGE local --kind project --project demo"
        ),
        "{error}"
    );
    assert!(error.contains(":ATTACHMENT_STORAGE: local"), "{error}");
    let _ = running.shutdown.send(());
    running.join.await.unwrap();
}

#[tokio::test]
#[ignore = "2 GiB streaming disk/network gate; run explicitly"]
async fn two_gib_recording_upload_and_seek() {
    exercise(2 * 1024 * 1024 * 1024, false).await;
}
