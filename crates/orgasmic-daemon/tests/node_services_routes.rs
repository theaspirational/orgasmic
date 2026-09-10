use orgasmic_core::Home;
use orgasmic_daemon::{Daemon, DaemonOptions, RunningDaemon};
use serde_json::{json, Value};
use std::path::Path;

fn write(path: impl AsRef<Path>, source: &str) {
    let path = path.as_ref();
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, source).unwrap();
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
async fn fixture() -> (tempfile::TempDir, Home, RunningDaemon, String, String) {
    let _ = tracing_subscriber::fmt()
        .with_env_filter("error")
        .with_test_writer()
        .try_init();
    let temp = tempfile::tempdir().unwrap();
    let home = Home::at(temp.path().join("home"));
    home.ensure().unwrap();
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
    assert_eq!(
        std::fs::read_to_string(ledger.join(".gitattributes"))
            .unwrap()
            .lines()
            .filter(|line| *line == "*/*/attachments/** filter=lfs diff=lfs merge=lfs -text")
            .count(),
        1
    );
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
#[ignore = "2 GiB streaming disk/network gate; run explicitly"]
async fn two_gib_recording_upload_and_seek() {
    exercise(2 * 1024 * 1024 * 1024, false).await;
}
