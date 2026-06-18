// orgasmic:dec_WWAHT,task_AQMTA
//! Integration tests for GET/PATCH /graph/layout overlay endpoint.

mod common;

use std::path::Path;

use orgasmic_core::Home;
use orgasmic_daemon::{Daemon, DaemonOptions, RunningDaemon};

fn test_options() -> DaemonOptions {
    DaemonOptions {
        bind_override: Some("127.0.0.1".parse().unwrap()),
        port_override: Some(0),
        fs_watcher_enabled: false,
        ..DaemonOptions::default()
    }
}

async fn boot(home: Home) -> RunningDaemon {
    Daemon::run(home, test_options())
        .await
        .expect("boot daemon")
}

fn read_token(home: &Home) -> String {
    std::fs::read_to_string(home.auth_token())
        .or_else(|_| std::fs::read_to_string(home.user().join("auth/token")))
        .expect("token file")
        .trim()
        .to_string()
}

fn write(path: &Path, contents: impl AsRef<str>) {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).unwrap();
    }
    std::fs::write(path, contents.as_ref()).unwrap();
}

fn seed_project(home: &Home, project_root: &Path, project_id: &str) {
    write(
        &project_root.join(".orgasmic/project.org"),
        format!(
            "#+title: {project_id}\n#+orgasmic_version: 1\n\n\
             * PROJECT {project_id}\n:PROPERTIES:\n:ID:               {project_id}\n:END:\n"
        ),
    );
    write(
        &home.board(),
        format!(
            "#+title: orgasmic board\n#+orgasmic_version: 1\n\n\
             * PROJECT {project_id}\n:PROPERTIES:\n:ID:               {project_id}\n\
             :PATH:             {}\n:BRANCH:           main\n:END:\n",
            project_root.display()
        ),
    );
}

#[tokio::test]
async fn patch_then_get_roundtrips_one_node() {
    let tmp = tempfile::tempdir().unwrap();
    let home = Home::at(tmp.path().join("home"));
    home.ensure().unwrap();
    let project_root = tmp.path().join("proj");
    seed_project(&home, &project_root, "layouttest");

    let running = boot(home.clone()).await;
    let token = read_token(&home);
    let client = reqwest::Client::new();
    let base = format!("http://{}", running.addr);

    // PATCH a node's layout
    let resp = client
        .patch(format!("{base}/api/graph/layout"))
        .bearer_auth(&token)
        .query(&[("project", "layouttest")])
        .json(&serde_json::json!({
            "node_id": "dec_WWAHT",
            "x": 120,
            "y": 40,
            "w": 220,
            "h": 120,
            "hidden": false,
            "pinned": true
        }))
        .send()
        .await
        .unwrap();
    assert!(
        resp.status().is_success(),
        "PATCH must succeed: {} {}",
        resp.status(),
        resp.text().await.unwrap()
    );

    // GET and verify round-trip
    let layout: serde_json::Value = client
        .get(format!("{base}/api/graph/layout"))
        .bearer_auth(&token)
        .query(&[("project", "layouttest")])
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();

    let entry = layout
        .get("dec_WWAHT")
        .expect("dec_WWAHT must be in overlay");
    assert_eq!(entry["x"], 120, "x round-trips");
    assert_eq!(entry["y"], 40, "y round-trips");
    assert_eq!(entry["w"], 220, "w round-trips");
    assert_eq!(entry["h"], 120, "h round-trips");
    assert_eq!(entry["hidden"], false, "hidden round-trips");
    assert_eq!(entry["pinned"], true, "pinned round-trips");

    let _ = running.shutdown.send(());
    let _ = running.join.await;
}

#[tokio::test]
async fn concurrent_patches_to_different_nodes_both_persist() {
    // Acceptance: concurrent PATCHes against different node_ids must both
    // survive. The pre-fix code read graph_layout.org OUTSIDE the writer
    // flock, so two PATCHes against an empty starting overlay could each
    // compute a single-node file and clobber each other on rewrite.
    let tmp = tempfile::tempdir().unwrap();
    let home = Home::at(tmp.path().join("home"));
    home.ensure().unwrap();
    let project_root = tmp.path().join("proj");
    seed_project(&home, &project_root, "layoutconc");

    let running = boot(home.clone()).await;
    let token = read_token(&home);
    let client = reqwest::Client::new();
    let base = format!("http://{}", running.addr);

    // The race is timing-sensitive (~half of unfixed runs see both nodes
    // survive by chance), so we iterate. Any single round losing an
    // update trips the assertion. Fresh node-id pair per round keeps
    // rounds independent.
    const ROUNDS: usize = 8;
    for round in 0..ROUNDS {
        let id_a = format!("dec_AA{round:03}");
        let id_b = format!("dec_BB{round:03}");
        let req_a = client
            .patch(format!("{base}/api/graph/layout"))
            .bearer_auth(&token)
            .query(&[("project", "layoutconc")])
            .json(&serde_json::json!({
                "node_id": id_a,
                "x": round as i64,
                "y": (round as i64) * 10,
            }))
            .send();
        let req_b = client
            .patch(format!("{base}/api/graph/layout"))
            .bearer_auth(&token)
            .query(&[("project", "layoutconc")])
            .json(&serde_json::json!({
                "node_id": id_b,
                "x": (round as i64) + 100,
                "y": (round as i64) * 10 + 1000,
            }))
            .send();

        let (resp_a, resp_b) = tokio::join!(req_a, req_b);
        let resp_a = resp_a.unwrap();
        let resp_b = resp_b.unwrap();
        assert!(
            resp_a.status().is_success(),
            "round {round} PATCH A must succeed: {} {}",
            resp_a.status(),
            resp_a.text().await.unwrap()
        );
        assert!(
            resp_b.status().is_success(),
            "round {round} PATCH B must succeed: {} {}",
            resp_b.status(),
            resp_b.text().await.unwrap()
        );

        let layout: serde_json::Value = client
            .get(format!("{base}/api/graph/layout"))
            .bearer_auth(&token)
            .query(&[("project", "layoutconc")])
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();

        let a = layout.get(&id_a).unwrap_or_else(|| {
            panic!("round {round}: {id_a} dropped by concurrent PATCH (race lost): {layout}")
        });
        assert_eq!(a["x"], round as i64);
        let b = layout.get(&id_b).unwrap_or_else(|| {
            panic!("round {round}: {id_b} dropped by concurrent PATCH (race lost): {layout}")
        });
        assert_eq!(b["x"], (round as i64) + 100);
    }

    let _ = running.shutdown.send(());
    let _ = running.join.await;
}

#[tokio::test]
async fn invalid_node_id_is_rejected_and_not_written() {
    let tmp = tempfile::tempdir().unwrap();
    let home = Home::at(tmp.path().join("home"));
    home.ensure().unwrap();
    let project_root = tmp.path().join("proj");
    seed_project(&home, &project_root, "layoutinvalid");

    let running = boot(home.clone()).await;
    let token = read_token(&home);
    let client = reqwest::Client::new();
    let base = format!("http://{}", running.addr);

    // An id containing a newline + property line would inject a forged
    // heading if interpolated. Must 400 and never touch the overlay.
    let bad = "dec_AAAA\n:INJECTED: t\n";
    let resp = client
        .patch(format!("{base}/api/graph/layout"))
        .bearer_auth(&token)
        .query(&[("project", "layoutinvalid")])
        .json(&serde_json::json!({ "node_id": bad, "x": 1, "y": 2 }))
        .send()
        .await
        .unwrap();
    assert_eq!(
        resp.status().as_u16(),
        400,
        "newline in node_id must 400, got {}",
        resp.status()
    );

    // Unknown prefix: no class -> reject.
    let resp = client
        .patch(format!("{base}/api/graph/layout"))
        .bearer_auth(&token)
        .query(&[("project", "layoutinvalid")])
        .json(&serde_json::json!({ "node_id": "totally-bogus", "x": 1 }))
        .send()
        .await
        .unwrap();
    assert_eq!(
        resp.status().as_u16(),
        400,
        "unknown-class node_id must 400"
    );

    let overlay = project_root.join(".orgasmic/graph_layout.org");
    if overlay.exists() {
        let source = std::fs::read_to_string(&overlay).unwrap();
        assert!(
            !source.contains("INJECTED"),
            "rejected id must never reach disk: {source}"
        );
        assert!(
            !source.contains("totally-bogus"),
            "rejected id must never reach disk: {source}"
        );
    }

    let _ = running.shutdown.send(());
    let _ = running.join.await;
}

#[tokio::test]
async fn two_patches_to_different_nodes_do_not_conflict() {
    let tmp = tempfile::tempdir().unwrap();
    let home = Home::at(tmp.path().join("home"));
    home.ensure().unwrap();
    let project_root = tmp.path().join("proj");
    seed_project(&home, &project_root, "layouttest2");

    let running = boot(home.clone()).await;
    let token = read_token(&home);
    let client = reqwest::Client::new();
    let base = format!("http://{}", running.addr);

    // PATCH node A
    let resp_a = client
        .patch(format!("{base}/api/graph/layout"))
        .bearer_auth(&token)
        .query(&[("project", "layouttest2")])
        .json(&serde_json::json!({
            "node_id": "dec_AAAA",
            "x": 10,
            "y": 20
        }))
        .send()
        .await
        .unwrap();
    assert!(
        resp_a.status().is_success(),
        "PATCH A must succeed: {} {}",
        resp_a.status(),
        resp_a.text().await.unwrap()
    );

    // PATCH node B
    let resp_b = client
        .patch(format!("{base}/api/graph/layout"))
        .bearer_auth(&token)
        .query(&[("project", "layouttest2")])
        .json(&serde_json::json!({
            "node_id": "dec_BBBB",
            "x": 300,
            "y": 400
        }))
        .send()
        .await
        .unwrap();
    assert!(
        resp_b.status().is_success(),
        "PATCH B must succeed: {} {}",
        resp_b.status(),
        resp_b.text().await.unwrap()
    );

    // GET and verify both entries persist
    let layout: serde_json::Value = client
        .get(format!("{base}/api/graph/layout"))
        .bearer_auth(&token)
        .query(&[("project", "layouttest2")])
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();

    let a = layout.get("dec_AAAA").expect("dec_AAAA must be in overlay");
    assert_eq!(a["x"], 10, "node A x must persist");
    assert_eq!(a["y"], 20, "node A y must persist");

    let b = layout.get("dec_BBBB").expect("dec_BBBB must be in overlay");
    assert_eq!(b["x"], 300, "node B x must persist");
    assert_eq!(b["y"], 400, "node B y must persist");

    let _ = running.shutdown.send(());
    let _ = running.join.await;
}
