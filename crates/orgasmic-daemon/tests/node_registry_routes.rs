use orgasmic_core::Home;
use orgasmic_daemon::{Daemon, DaemonOptions};
use serde_json::{json, Value};

fn write(path: impl AsRef<std::path::Path>, source: impl AsRef<str>) {
    let path = path.as_ref();
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, source.as_ref()).unwrap();
}

async fn cookie(client: &reqwest::Client, base: &str, token: &str) -> String {
    let response = client
        .post(format!("{base}/login"))
        .json(&json!({"token":token}))
        .send()
        .await
        .unwrap();
    assert!(response.status().is_success());
    response.headers()["set-cookie"]
        .to_str()
        .unwrap()
        .split(';')
        .next()
        .unwrap()
        .to_owned()
}

#[tokio::test]
async fn user_descriptor_uses_existing_node_routes_and_preserves_state() {
    let tmp = tempfile::tempdir().unwrap();
    let home = Home::at(tmp.path().join("home"));
    home.ensure().unwrap();
    let project = tmp.path().join("project");
    write(
        project.join(".orgasmic/project.org"),
        "* PROJECT demo\n:PROPERTIES:\n:ID: demo\n:END:\n",
    );
    write(
        tmp.path().join("elsewhere/.orgasmic/project.org"),
        "* PROJECT elsewhere\n:PROPERTIES:\n:ID: elsewhere\n:END:\n",
    );
    write(
        home.board(),
        format!(
            "* PROJECT demo\n:PROPERTIES:\n:ID: demo\n:PATH: {}\n:BRANCH: main\n:END:\n* PROJECT elsewhere\n:PROPERTIES:\n:ID: elsewhere\n:PATH: {}\n:BRANCH: main\n:END:\n",
            project.display(), tmp.path().join("elsewhere").display()
        ),
    );
    write(home.user().join("schema/node-types/meetings.org"), "* NODE-TYPE meeting\n:PROPERTIES:\n:COLLECTION: meetings\n:ID_PREFIX: MEET-\n:LABEL: Meeting\n:LABEL_PLURAL: Meetings\n:REQUIRED_PROPERTIES: ID\n:STATES: active archived\n:TRANSITIONS: active>archived archived>active\n:END:\n");
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
    let token = std::fs::read_to_string(home.auth_token()).unwrap();
    let client = reqwest::Client::new();
    let base = format!("http://{}/api", running.addr);
    let editor_token =
        orgasmic_core::add_member(&home, "editor-user", &[("demo".into(), "editor".into())])
            .unwrap();
    let viewer_token =
        orgasmic_core::add_member(&home, "viewer-user", &[("demo".into(), "viewer".into())])
            .unwrap();
    let editor = cookie(&client, &base, &editor_token).await;
    let viewer = cookie(&client, &base, &viewer_token).await;
    let metadata = client
        .get(format!("{base}/node-types?project=demo"))
        .header("cookie", &viewer)
        .send()
        .await
        .unwrap();
    assert!(metadata.status().is_success());
    let descriptors: Vec<Value> = metadata.json().await.unwrap();
    let meetings = descriptors
        .iter()
        .find(|item| item["collection"] == "meetings")
        .unwrap();
    assert_eq!(meetings["label_plural"], "Meetings");
    assert_eq!(meetings["states"], json!(["active", "archived"]));
    assert_eq!(meetings["transitions"]["active"], json!(["archived"]));
    assert_eq!(
        client
            .get(format!("{base}/node-types?project=demo"))
            .send()
            .await
            .unwrap()
            .status(),
        reqwest::StatusCode::UNAUTHORIZED
    );
    assert_eq!(
        client
            .get(format!("{base}/node-types?project=elsewhere"))
            .header("cookie", &viewer)
            .send()
            .await
            .unwrap()
            .status(),
        reqwest::StatusCode::FORBIDDEN
    );
    let payload = json!({"project":"demo", "kind":"meetings", "title":"Member meeting", "request_id":"member-meeting"});
    let denied = client
        .post(format!("{base}/org/node"))
        .header("cookie", &viewer)
        .json(&payload)
        .send()
        .await
        .unwrap();
    assert_eq!(denied.status(), reqwest::StatusCode::FORBIDDEN);
    let denied_alias = client
        .post(format!("{base}/graph/nodes"))
        .header("cookie", &viewer)
        .json(&payload)
        .send()
        .await
        .unwrap();
    assert_eq!(denied_alias.status(), reqwest::StatusCode::FORBIDDEN);
    let created = client
        .post(format!("{base}/graph/nodes"))
        .header("cookie", &editor)
        .json(&payload)
        .send()
        .await
        .unwrap();
    let status = created.status();
    let body = created.text().await.unwrap();
    assert!(status.is_success(), "member create: {status}: {body}");
    let member_created: Value = serde_json::from_str(&body).unwrap();
    let member_id = member_created["id"].as_str().unwrap();
    let journal = std::fs::read_to_string(
        project.join(format!(".orgasmic/meetings/{member_id}/journal.org")),
    )
    .unwrap();
    assert!(
        journal.contains("editor-user"),
        "member attribution missing"
    );
    let doc: Value = client
        .get(format!("{base}/org/node"))
        .header("cookie", &editor)
        .query(&[("project", "demo"), ("id", member_id)])
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let member_edit = json!({"project":"demo", "base_version":doc["source"]["base_version"], "ops":[{"op":"set_title", "title":"Member correction"}]});
    let denied = client
        .post(format!("{base}/org/node/{member_id}/edit"))
        .header("cookie", &viewer)
        .json(&member_edit)
        .send()
        .await
        .unwrap();
    assert_eq!(denied.status(), reqwest::StatusCode::FORBIDDEN);
    let edited = client
        .post(format!("{base}/org/node/{member_id}/edit"))
        .header("cookie", &editor)
        .json(&member_edit)
        .send()
        .await
        .unwrap();
    assert!(
        edited.status().is_success(),
        "member edit: {}",
        edited.text().await.unwrap()
    );
    let response = client.post(format!("{base}/org/node")).bearer_auth(token.trim())
        .json(&json!({"project":"demo", "kind":"meetings", "title":"First meeting", "body":"Original notes", "request_id":"meeting-create"}))
        .send().await.unwrap();
    let status = response.status();
    let body = response.text().await.unwrap();
    assert!(status.is_success(), "create: {status}: {body}");
    let created: Value = serde_json::from_str(&body).unwrap();
    let id = created["id"].as_str().unwrap();
    assert!(id.starts_with("MEET-"));
    let doc: Value = client
        .get(format!("{base}/org/node"))
        .bearer_auth(token.trim())
        .query(&[("project", "demo"), ("id", id)])
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(doc["title"], "First meeting");
    assert_eq!(doc["todo"], "ACTIVE");
    assert_eq!(doc["collection"], "meetings");
    let nodes: Vec<Value> = client
        .get(format!("{base}/graph/nodes?project=demo"))
        .header("cookie", &viewer)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let meeting = nodes.iter().find(|node| node["id"] == id).unwrap();
    assert_eq!(meeting["layer"], "meetings");
    assert_eq!(meeting["title"], "First meeting");
    assert_eq!(meeting["todo"], "ACTIVE");
    let edited = client.post(format!("{base}/org/node/{id}/edit?json=true")).bearer_auth(token.trim())
        .json(&json!({"project":"demo", "base_version":doc["source"]["base_version"], "ops":[{"op":"set_title", "title":"Corrected"}]}))
        .send().await.unwrap();
    assert!(
        edited.status().is_success(),
        "edit: {}",
        edited.text().await.unwrap()
    );
    let file = orgasmic_core::OrgFile::parse(
        std::fs::read_to_string(project.join(format!(".orgasmic/meetings/{id}/node.org"))).unwrap(),
        "node.org",
    )
    .unwrap();
    assert_eq!(file.headings[0].todo.as_deref(), Some("ACTIVE"));
    assert_eq!(file.headings[0].title, format!("{id} Corrected"));
    let traversal = client
        .get(format!("{base}/org/node"))
        .bearer_auth(token.trim())
        .query(&[
            ("project", "demo"),
            ("id", "../outside"),
            ("kind", "meetings"),
        ])
        .send()
        .await
        .unwrap();
    assert_eq!(traversal.status(), reqwest::StatusCode::BAD_REQUEST);

    let read = |id: String| {
        client
            .get(format!("{base}/org/node"))
            .bearer_auth(token.trim())
            .query(&[("project", "demo"), ("id", id.as_str())])
    };
    let artifact = client.post(format!("{base}/org/node")).bearer_auth(token.trim())
        .json(&json!({"project":"demo", "kind":"artifact", "title":"Review", "body":"<Section>Review notes</Section>", "request_id":"artifact-create"}))
        .send().await.unwrap();
    let status = artifact.status();
    let body = artifact.text().await.unwrap();
    assert!(status.is_success(), "artifact create {status}: {body}");
    let artifact: Value = serde_json::from_str(&body).unwrap();
    let art_id = artifact["id"].as_str().unwrap();
    assert_eq!(
        std::fs::read_to_string(project.join(format!(".orgasmic/artifacts/{art_id}/artifact.mdx")))
            .unwrap(),
        "<Section>Review notes</Section>"
    );
    let art_doc: Value = read(art_id.to_owned())
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(art_doc["kind"], "artifact");
    assert_eq!(art_doc["collection"], "artifacts");
    for layer in ["meetings", "artifacts", "missing"] {
        let nodes: Vec<Value> = client
            .get(format!("{base}/graph/nodes"))
            .header("cookie", &viewer)
            .query(&[("project", "demo"), ("layer", layer)])
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        assert!(nodes.iter().all(|node| node["layer"] == layer));
        assert_eq!(nodes.is_empty(), layer == "missing");
    }
    let bypass = client.post(format!("{base}/org/node/{art_id}/edit")).bearer_auth(token.trim())
        .json(&json!({"project":"demo", "base_version":art_doc["source"]["base_version"], "ops":[{"op":"set_property", "key":"VERSION", "value":"99"}]}))
        .send().await.unwrap();
    assert_eq!(bypass.status(), reqwest::StatusCode::BAD_REQUEST);
    assert!(bypass
        .text()
        .await
        .unwrap()
        .contains("owned by submit/regenerate"));
    let current: Value = read(id.to_owned())
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let edit = |title: &str| {
        client.post(format!("{base}/org/node/{id}/edit?json=true")).bearer_auth(token.trim())
        .json(&json!({"project":"demo", "base_version":current["source"]["base_version"], "ops":[{"op":"set_title", "title":title}]}))
    };
    let (left, right) = tokio::join!(edit("Left").send(), edit("Right").send());
    let statuses = [left.unwrap().status(), right.unwrap().status()];
    assert_eq!(
        statuses.iter().filter(|status| status.is_success()).count(),
        1,
        "{statuses:?}"
    );
    assert_eq!(
        statuses
            .iter()
            .filter(|status| **status == reqwest::StatusCode::CONFLICT)
            .count(),
        1,
        "{statuses:?}"
    );

    let current: Value = read(id.to_owned())
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let archived = client.post(format!("{base}/org/node/{id}/edit?json=true")).bearer_auth(token.trim())
        .json(&json!({"project":"demo", "base_version":current["source"]["base_version"], "ops":[{"op":"set_state", "state":"archived"}]}))
        .send().await.unwrap();
    assert!(
        archived.status().is_success(),
        "state: {}",
        archived.text().await.unwrap()
    );
    let graph: Vec<Value> = client
        .get(format!("{base}/graph/nodes?project=demo"))
        .bearer_auth(token.trim())
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert!(graph
        .iter()
        .any(|node| node["id"] == id && node["layer"] == "meetings"));

    let task: Value = client
        .post(format!("{base}/org/node"))
        .bearer_auth(token.trim())
        .json(&json!({"project":"demo", "kind":"task", "title":"Task with no evidence"}))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let task_id = task["id"].as_str().unwrap();
    let current: Value = read(task_id.to_owned())
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let start = || {
        client.post(format!("{base}/org/node/{task_id}/edit?json=true"))
        .bearer_auth(token.trim())
        .json(&json!({"project":"demo", "base_version":current["source"]["base_version"], "ops":[{"op":"set_state", "state":"in_progress"}]}))
    };
    let (left, right) = tokio::join!(start().send(), start().send());
    let statuses = [left.unwrap().status(), right.unwrap().status()];
    assert_eq!(
        statuses.iter().filter(|status| status.is_success()).count(),
        1,
        "{statuses:?}"
    );
    assert_eq!(
        statuses
            .iter()
            .filter(|status| **status == reqwest::StatusCode::CONFLICT)
            .count(),
        1,
        "{statuses:?}"
    );
    for next in ["in_progress", "in_review", "done"] {
        let current: Value = read(task_id.to_owned())
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        let response = client.post(format!("{base}/org/node/{task_id}/edit?json=true")).bearer_auth(token.trim())
            .json(&json!({"project":"demo", "base_version":current["source"]["base_version"], "ops":[{"op":"set_state", "state":next}]}))
            .send().await.unwrap();
        let status = response.status();
        let body = response.text().await.unwrap();
        if next == "done" {
            assert_eq!(status, reqwest::StatusCode::BAD_REQUEST, "{body}");
            assert!(body.contains("evidence"), "{body}");
        } else {
            assert!(status.is_success(), "{next}: {status}: {body}");
            let journal = std::fs::read_to_string(
                project.join(format!(".orgasmic/tasks/{task_id}/journal.org")),
            )
            .unwrap();
            assert!(
                journal.contains("task.state_transitioned"),
                "generic state changes must use compiled lifecycle bookkeeping: {journal}"
            );
        }
    }
    let _ = running.shutdown.send(());
    running.join.await.unwrap();
    let restarted = Daemon::run(
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
    let base = format!("http://{}/api", restarted.addr);
    let reindexed = client
        .post(format!("{base}/reindex/demo"))
        .bearer_auth(token.trim())
        .send()
        .await
        .unwrap();
    assert!(reindexed.status().is_success());
    let doc: Value = client
        .get(format!("{base}/org/node"))
        .bearer_auth(token.trim())
        .query(&[("project", "demo"), ("id", id)])
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(doc["todo"], "ARCHIVED");
    let deleted = client.post(format!("{base}/org/node/{id}/delete")).bearer_auth(token.trim())
        .json(&json!({"project":"demo", "base_version":doc["source"]["base_version"], "request_id":"delete-meeting"}))
        .send().await.unwrap();
    assert!(
        deleted.status().is_success(),
        "delete: {}",
        deleted.text().await.unwrap()
    );
    let missing = client
        .get(format!("{base}/org/node"))
        .bearer_auth(token.trim())
        .query(&[("project", "demo"), ("id", id)])
        .send()
        .await
        .unwrap();
    assert_eq!(missing.status(), reqwest::StatusCode::NOT_FOUND);
    assert!(
        project
            .join(format!(".orgasmic/meetings/{id}/journal.org"))
            .is_file(),
        "deletion preserves history"
    );
    let _ = restarted.shutdown.send(());
    restarted.join.await.unwrap();
}
