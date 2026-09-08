use orgasmic_core::Home;
use orgasmic_daemon::{Daemon, DaemonOptions};
use serde_json::{json, Value};

fn write(path: impl AsRef<std::path::Path>, source: &str) {
    let path = path.as_ref();
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, source).unwrap();
}
fn manifest(id: &str, prefix: &str) -> String {
    format!("* Plugin\n:PROPERTIES:\n:ID: {id}\n:VERSION: 0.1.0\n:PLUGIN_API: 1\n:SCHEMA: 1\n:REQUIRES: core.nodes@1\n:CAPABILITIES: nodes.read nodes.write\n:COMMANDS: probe\n:END:\n** Node type {id}\n:PROPERTIES:\n:COLLECTION: {id}\n:ID_PREFIX: {prefix}\n:LABEL: {id}\n:LABEL_PLURAL: {id}\n:REQUIRED_PROPERTIES: ID\n:STATES: active archived\n:TRANSITIONS: active>archived archived>active\n:END:\n")
}
async fn send(
    client: &reqwest::Client,
    base: &str,
    token: &str,
    path: &str,
    body: Value,
    status: u16,
) -> Value {
    let response = client
        .post(format!("{base}{path}"))
        .bearer_auth(token)
        .json(&body)
        .send()
        .await
        .unwrap();
    let actual = response.status().as_u16();
    let text = response.text().await.unwrap();
    // Credentials are returned only by /run; never include its body in failure output.
    assert_eq!(
        actual,
        status,
        "{path}: {}",
        if path.ends_with("/run") {
            "credential response omitted"
        } else {
            &text
        }
    );
    serde_json::from_str(&text).unwrap_or(Value::Null)
}
async fn get(client: &reqwest::Client, base: &str, token: &str, path: &str) -> Value {
    let response = client
        .get(format!("{base}{path}"))
        .bearer_auth(token)
        .send()
        .await
        .unwrap();
    assert!(
        response.status().is_success(),
        "GET {path}: {}",
        response.status()
    );
    response.json().await.unwrap()
}

#[tokio::test]
async fn ui_assets_require_approval_project_access_and_safe_paths() {
    let temp = tempfile::tempdir().unwrap();
    let home = Home::at(temp.path().join("home"));
    home.ensure().unwrap();
    let root = temp.path().join("demo");
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
    let dir = home.user().join("plugins/meetings");
    let source = manifest("meetings", "MEET-").replace(":COMMANDS: probe\n", "");
    write(dir.join("plugin.org"), &source);
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
    let api = format!("{base}/api");
    let token = std::fs::read_to_string(home.auth_token()).unwrap();
    let token = token.trim();
    let viewer =
        orgasmic_core::add_member(&home, "viewer", &[("demo".into(), "viewer".into())]).unwrap();
    let foreign =
        orgasmic_core::add_member(&home, "foreign", &[("elsewhere".into(), "viewer".into())])
            .unwrap();
    let client = reqwest::Client::new();
    let approval = json!({"project":"demo", "enabled":true, "approved_capabilities":["nodes.read", "nodes.write"]});
    send(
        &client,
        &api,
        token,
        "/plugins/meetings/activation",
        approval.clone(),
        200,
    )
    .await;
    write(
        dir.join("ui/index.js"),
        "export const marker = 'plugin asset';",
    );
    write(
        dir.join("plugin.org"),
        &source.replace(":SCHEMA: 1", ":UI: ui/index.js\n:SDK: ^1.0\n:SCHEMA: 1"),
    );
    send(&client, &api, token, "/plugins/reconcile", json!({}), 200).await;
    let asset = format!("{base}/plugins/meetings/ui/demo/index.js");
    assert_eq!(client.get(&asset).send().await.unwrap().status(), 401);
    assert_eq!(
        client
            .get(&asset)
            .bearer_auth(token)
            .send()
            .await
            .unwrap()
            .status(),
        404
    );
    send(
        &client,
        &api,
        token,
        "/plugins/meetings/activation",
        approval,
        400,
    )
    .await;
    send(&client, &api, token, "/plugins/meetings/activation", json!({"project":"demo", "enabled":true, "approved_capabilities":["nodes.read", "nodes.write", "ui.execute"]}), 200).await;
    let response = client
        .get(&asset)
        .bearer_auth(&viewer)
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 200);
    assert_eq!(response.headers()["x-content-type-options"], "nosniff");
    assert_eq!(response.headers()["cache-control"], "no-store");
    assert!(response.text().await.unwrap().contains("plugin asset"));
    assert_eq!(
        client
            .get(&asset)
            .bearer_auth(&foreign)
            .send()
            .await
            .unwrap()
            .status(),
        403
    );
    for path in ["%2e%2e%2fplugin.org", "missing.js"] {
        assert_eq!(
            client
                .get(format!("{base}/plugins/meetings/ui/demo/{path}"))
                .bearer_auth(token)
                .send()
                .await
                .unwrap()
                .status(),
            404
        );
    }
    #[cfg(unix)]
    {
        write(temp.path().join("secret.js"), "secret");
        std::os::unix::fs::symlink(temp.path().join("secret.js"), dir.join("ui/escape.js"))
            .unwrap();
        assert_eq!(
            client
                .get(format!("{base}/plugins/meetings/ui/demo/escape.js"))
                .bearer_auth(token)
                .send()
                .await
                .unwrap()
                .status(),
            404
        );
    }
    let response = client.get(format!("{base}/")).send().await.unwrap();
    let csp = response.headers()["content-security-policy"]
        .to_str()
        .unwrap();
    assert!(csp.contains("script-src 'self' 'nonce-"));
    assert!(csp.contains("connect-src 'self' http: https: ws: wss:"));
    assert!(!csp.contains("unsafe-eval"));
    assert!(response
        .text()
        .await
        .unwrap()
        .contains("<script type=\"importmap\" nonce=\""));
    let preview = client
        .get(format!("{base}/prototype-frame.html"))
        .send()
        .await
        .unwrap();
    let csp = preview.headers()["content-security-policy"]
        .to_str()
        .unwrap();
    assert!(csp.contains("sandbox allow-scripts;") && !csp.contains("allow-same-origin"));
    let alias = client
        .get(format!("{base}//prototype-frame.html"))
        .send()
        .await
        .unwrap();
    assert_eq!(alias.headers()["content-security-policy"], csp);
    send(
        &client,
        &api,
        token,
        "/plugins/meetings/activation",
        json!({"project":"demo", "enabled":false}),
        200,
    )
    .await;
    assert_eq!(
        client
            .get(&asset)
            .bearer_auth(token)
            .send()
            .await
            .unwrap()
            .status(),
        404
    );
    let _ = running.shutdown.send(());
    running.join.await.unwrap();
}

#[tokio::test]
async fn plugins_reload_scope_and_revoke_through_existing_routes() {
    let temp = tempfile::tempdir().unwrap();
    let home = Home::at(temp.path().join("home"));
    home.ensure().unwrap();
    let root = temp.path().join("demo");
    let foreign = temp.path().join("foreign");
    for (path, id) in [(&root, "demo"), (&foreign, "foreign")] {
        write(
            path.join(".orgasmic/project.org"),
            &format!("* PROJECT {id}\n:PROPERTIES:\n:ID: {id}\n:END:\n"),
        );
    }
    write(home.board(), &format!("* PROJECT demo\n:PROPERTIES:\n:ID: demo\n:PATH: {}\n:BRANCH: main\n:END:\n* PROJECT foreign\n:PROPERTIES:\n:ID: foreign\n:PATH: {}\n:BRANCH: main\n:END:\n", root.display(), foreign.display()));
    let options = || DaemonOptions {
        bind_override: Some("127.0.0.1".parse().unwrap()),
        port_override: Some(0),
        fs_watcher_enabled: false,
        ..Default::default()
    };
    let running = Daemon::run(home.clone(), options()).await.unwrap();
    let base = format!("http://{}/api", running.addr);
    let admin = std::fs::read_to_string(home.auth_token()).unwrap();
    let admin = admin.trim();
    let editor =
        orgasmic_core::add_member(&home, "editor", &[("demo".into(), "editor".into())]).unwrap();
    let viewer =
        orgasmic_core::add_member(&home, "viewer", &[("demo".into(), "viewer".into())]).unwrap();
    let client = reqwest::Client::new();
    get(&client, &base, admin, "/node-types?project=demo").await;
    for (id, prefix) in [("meetings", "MEET-"), ("notes", "NOTE-")] {
        let dir = home.user().join("plugins").join(id);
        write(dir.join("plugin.org"), &manifest(id, prefix));
        write(dir.join("bin/probe"), "#!/bin/sh\nexit 0\n");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(
                dir.join("bin/probe"),
                std::fs::Permissions::from_mode(0o755),
            )
            .unwrap();
        }
    }
    write(
        home.user().join("plugins/broken/plugin.org"),
        "not a manifest",
    );
    // Automatic reconciliation, not a daemon restart or a GET-triggered disk scan.
    tokio::time::timeout(std::time::Duration::from_secs(10), async {
        loop {
            let plugins = get(&client, &base, admin, "/plugins?project=demo").await;
            if plugins
                .as_array()
                .unwrap()
                .iter()
                .any(|p| p["id"] == "meetings")
            {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        }
    })
    .await
    .unwrap();
    let approval = json!({"project":"demo", "enabled":true, "approved_capabilities":["nodes.read", "nodes.write"]});
    send(
        &client,
        &base,
        &editor,
        "/plugins/meetings/activation",
        approval.clone(),
        403,
    )
    .await;
    for id in ["meetings", "notes"] {
        send(
            &client,
            &base,
            admin,
            &format!("/plugins/{id}/activation"),
            approval.clone(),
            200,
        )
        .await;
    }
    let types = get(&client, &base, &viewer, "/node-types?project=demo").await;
    assert!(types
        .as_array()
        .unwrap()
        .iter()
        .any(|d| d["collection"] == "meetings"));
    let types = get(&client, &base, admin, "/node-types?project=foreign").await;
    assert!(!types
        .as_array()
        .unwrap()
        .iter()
        .any(|d| d["collection"] == "meetings"));
    send(
        &client,
        &base,
        admin,
        "/org/node",
        json!({"project":"foreign", "kind":"meetings", "title":"denied"}),
        400,
    )
    .await;
    send(
        &client,
        &base,
        &viewer,
        "/plugins/meetings/run",
        json!({"project":"demo", "command":"probe"}),
        403,
    )
    .await;
    let issued = send(
        &client,
        &base,
        &editor,
        "/plugins/meetings/run",
        json!({"project":"demo", "command":"probe"}),
        200,
    )
    .await;
    let token = issued["token"].as_str().unwrap();
    // Explicit revoke is final, and a plugin bearer cannot borrow an admin cookie.
    let extra = send(
        &client,
        &base,
        admin,
        "/plugins/meetings/run",
        json!({"project":"demo", "command":"probe"}),
        200,
    )
    .await;
    send(
        &client,
        &base,
        admin,
        "/plugins/run/revoke",
        json!({"lease":extra["lease"]}),
        200,
    )
    .await;
    let sessions = reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .unwrap();
    let ticket: Value = sessions
        .post(format!("{base}/auth/ui-session"))
        .bearer_auth(admin)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let login = sessions
        .get(format!(
            "{}{}",
            base.trim_end_matches("/api"),
            ticket["path"].as_str().unwrap()
        ))
        .send()
        .await
        .unwrap();
    let cookie = login.headers()["set-cookie"]
        .to_str()
        .unwrap()
        .split(';')
        .next()
        .unwrap();
    assert_eq!(
        client
            .get(format!("{base}/node-types?project=demo"))
            .bearer_auth(extra["token"].as_str().unwrap())
            .header("cookie", cookie)
            .send()
            .await
            .unwrap()
            .status()
            .as_u16(),
        401
    );
    let created = send(
        &client,
        &base,
        token,
        "/org/node",
        json!({"project":"demo", "kind":"meetings", "title":"Planning", "request_id":"create"}),
        200,
    )
    .await;
    let id = created["id"].as_str().unwrap();
    let journal =
        std::fs::read_to_string(root.join(format!(".orgasmic/meetings/{id}/journal.org"))).unwrap();
    let journal = orgasmic_core::OrgFile::parse(&journal, "journal.org").unwrap();
    assert_eq!(
        journal.headings[0].property("ACTOR"),
        Some("plugin:meetings")
    );
    assert_eq!(
        journal.headings[0].property("PLUGIN_CALLER"),
        Some("editor")
    );
    let node_path = root.join(format!(".orgasmic/meetings/{id}/node.org"));
    assert!(std::fs::read_to_string(&node_path)
        .unwrap()
        .contains("#+plugin: meetings schema=1"));
    let doc = get(
        &client,
        &base,
        token,
        &format!("/org/node?project=demo&id={id}"),
    )
    .await;
    assert_eq!(doc["schema_matches"], true);
    let other = send(
        &client,
        &base,
        admin,
        "/org/node",
        json!({"project":"demo", "kind":"notes", "title":"Other"}),
        200,
    )
    .await;
    let other_id = other["id"].as_str().unwrap();
    for path in ["/org/node", "/graph/nodes"] {
        send(
            &client,
            &base,
            token,
            path,
            json!({"project":"demo", "kind":"notes", "title":"denied"}),
            403,
        )
        .await;
    }
    for op in ["edit", "delete"] {
        send(
            &client,
            &base,
            token,
            &format!("/org/node/{other_id}/{op}"),
            json!({"project":"demo", "base_version":"ignored", "ops":[]}),
            403,
        )
        .await;
    }
    send(
        &client,
        &base,
        token,
        "/org/node",
        json!({"project":"foreign", "kind":"meetings", "title":"denied"}),
        403,
    )
    .await;
    send(
        &client,
        &base,
        token,
        "/plugins/notes/run",
        json!({"project":"demo", "command":"probe"}),
        403,
    )
    .await;
    send(&client, &base, token, &format!("/org/node/{id}/edit?json=true"), json!({"project":"demo", "base_version":doc["source"]["base_version"], "ops":[{"op":"set_title", "title":"Revised"}]}), 200).await;
    send(
        &client,
        &base,
        admin,
        "/plugins/meetings/activation",
        json!({"project":"demo", "enabled":false}),
        200,
    )
    .await;
    assert_eq!(
        client
            .get(format!("{base}/node-types?project=demo"))
            .bearer_auth(token)
            .send()
            .await
            .unwrap()
            .status()
            .as_u16(),
        401
    );
    let disabled = get(
        &client,
        &base,
        admin,
        &format!("/org/node?project=demo&id={id}"),
    )
    .await;
    assert_eq!(disabled["schema_matches"], false);
    assert!(disabled["descriptor"].is_null());
    send(
        &client,
        &base,
        admin,
        &format!("/org/node/{id}/delete"),
        json!({"project":"demo", "base_version":disabled["source"]["base_version"]}),
        400,
    )
    .await;
    send(
        &client,
        &base,
        admin,
        "/plugins/meetings/activation",
        approval.clone(),
        200,
    )
    .await;
    // Disabling revokes, not merely suspends, a credential.
    assert_eq!(
        client
            .get(format!("{base}/node-types?project=demo"))
            .bearer_auth(token)
            .send()
            .await
            .unwrap()
            .status()
            .as_u16(),
        401
    );
    let membership_lease = send(
        &client,
        &base,
        &editor,
        "/plugins/meetings/run",
        json!({"project":"demo", "command":"probe"}),
        200,
    )
    .await;
    orgasmic_core::revoke_member(&home, "editor").unwrap();
    assert_eq!(
        client
            .get(format!("{base}/node-types?project=demo"))
            .bearer_auth(membership_lease["token"].as_str().unwrap())
            .send()
            .await
            .unwrap()
            .status()
            .as_u16(),
        401
    );
    // Growth of requested capabilities requires explicit re-approval.
    write(
        home.user().join("plugins/meetings/plugin.org"),
        &manifest("meetings", "MEET-").replace("nodes.read nodes.write", "nodes.read"),
    );
    send(
        &client,
        &base,
        admin,
        "/plugins/meetings/activation",
        json!({"project":"demo", "enabled":true, "approved_capabilities":["nodes.read"]}),
        200,
    )
    .await;
    let read_only = send(
        &client,
        &base,
        &viewer,
        "/plugins/meetings/run",
        json!({"project":"demo", "command":"probe"}),
        200,
    )
    .await;
    get(
        &client,
        &base,
        read_only["token"].as_str().unwrap(),
        "/graph/nodes?project=demo&layer=meetings",
    )
    .await;
    send(
        &client,
        &base,
        read_only["token"].as_str().unwrap(),
        "/org/node",
        json!({"project":"demo", "kind":"meetings", "title":"denied"}),
        403,
    )
    .await;
    write(
        home.user().join("plugins/meetings/plugin.org"),
        &manifest("meetings", "MEET-"),
    );
    send(&client, &base, admin, "/plugins/reconcile", json!({}), 200).await;
    let unavailable = get(
        &client,
        &base,
        admin,
        &format!("/org/node?project=demo&id={id}"),
    )
    .await;
    assert_eq!(unavailable["schema_matches"], false);
    send(
        &client,
        &base,
        admin,
        "/plugins/meetings/activation",
        approval.clone(),
        200,
    )
    .await;
    let incompatible = manifest("meetings", "MEET-").replace(":SCHEMA: 1", ":SCHEMA: 2");
    write(
        home.user().join("plugins/meetings/plugin.org"),
        &incompatible,
    );
    send(&client, &base, admin, "/plugins/reconcile", json!({}), 200).await;
    let mismatch = get(
        &client,
        &base,
        admin,
        &format!("/org/node?project=demo&id={id}"),
    )
    .await;
    assert_eq!(mismatch["schema_matches"], false);
    for op in ["edit", "delete"] {
        send(
            &client,
            &base,
            admin,
            &format!("/org/node/{id}/{op}"),
            json!({"project":"demo", "base_version":mismatch["source"]["base_version"], "ops":[]}),
            400,
        )
        .await;
    }
    // Corrupt one plugin: unrelated registry routes remain usable.
    write(home.user().join("plugins/meetings/plugin.org"), "broken");
    send(&client, &base, admin, "/plugins/reconcile", json!({}), 200).await;
    let types = get(&client, &base, admin, "/node-types?project=demo").await;
    assert!(types
        .as_array()
        .unwrap()
        .iter()
        .any(|d| d["collection"] == "notes"));
    assert!(!types
        .as_array()
        .unwrap()
        .iter()
        .any(|d| d["collection"] == "meetings"));
    // Ownership survives removal and restart, including overlapping prefixes.
    std::fs::rename(
        home.user().join("plugins/meetings"),
        temp.path().join("removed-meetings"),
    )
    .unwrap();
    write(
        home.user().join("plugins/imposter/plugin.org"),
        &manifest("imposter", "MEET-"),
    );
    write(
        home.user().join("plugins/imposter/bin/probe"),
        "#!/bin/sh\nexit 0\n",
    );
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(
            home.user().join("plugins/imposter/bin/probe"),
            std::fs::Permissions::from_mode(0o755),
        )
        .unwrap();
    }
    send(
        &client,
        &base,
        admin,
        "/plugins/imposter/activation",
        approval,
        400,
    )
    .await;
    let _ = running.shutdown.send(());
    running.join.await.unwrap();
    let running = Daemon::run(home.clone(), options()).await.unwrap();
    let base = format!("http://{}/api", running.addr);
    let retained = get(
        &client,
        &base,
        admin,
        &format!("/org/node?project=demo&id={id}"),
    )
    .await;
    assert_eq!(retained["title"], "Revised");
    assert_eq!(retained["schema_matches"], false);
    let _ = running.shutdown.send(());
    running.join.await.unwrap();
}
