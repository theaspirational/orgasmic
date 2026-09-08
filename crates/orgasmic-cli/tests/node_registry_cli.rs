mod common;

use orgasmic_core::Home;
use orgasmic_daemon::{Daemon, DaemonOptions};
use serde_json::Value;

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn descriptor_only_collection_round_trips_through_cli_without_kind() {
    let temp = tempfile::tempdir().unwrap();
    let home = Home::at(temp.path().join("home"));
    home.ensure().unwrap();
    let project = temp.path().join("project");
    common::write(
        &project.join(".orgasmic/project.org"),
        "* PROJECT demo\n:PROPERTIES:\n:ID: demo\n:END:\n",
    );
    common::write(
        &home.board(),
        &format!(
            "* PROJECT demo\n:PROPERTIES:\n:ID: demo\n:PATH: {}\n:BRANCH: main\n:END:\n",
            project.display()
        ),
    );
    common::write(&home.user().join("schema/node-types/meetings.org"), "* NODE-TYPE meeting\n:PROPERTIES:\n:COLLECTION: meetings\n:ID_PREFIX: MEET-\n:LABEL: Meeting\n:LABEL_PLURAL: Meetings\n:REQUIRED_PROPERTIES: ID OCCURRED_AT\n:STATES: active archived\n:TRANSITIONS: active>archived archived>active\n:END:\n");
    let running = Daemon::run(
        home.clone(),
        DaemonOptions {
            fs_watcher_enabled: false,
            ..common::test_options()
        },
    )
    .await
    .unwrap();
    let cli = |args: &[&str]| {
        let output = common::orgasmic_command()
            .args(args)
            .current_dir(&project)
            .env("ORGASMIC_HOME", &home.root)
            .env("ORGASMIC_DAEMON_URL", format!("http://{}", running.addr))
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{args:?}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        String::from_utf8(output.stdout).unwrap()
    };
    let minted = cli(&["id", "mint", "--class", "meetings"]);
    assert!(minted.contains("MEET-"), "{minted}");
    let args = [
        "node",
        "create",
        "--kind",
        "meetings",
        "--title",
        "Weekly",
        "--property",
        "OCCURRED_AT=2026-09-07",
        "--project",
        "demo",
        "--request-id",
        "meeting-cli",
    ];
    let created: Value = serde_json::from_str(&cli(&args)).unwrap();
    let replay: Value = serde_json::from_str(&cli(&args)).unwrap();
    assert_eq!(created, replay);
    let id = created["id"].as_str().unwrap();
    cli(&[
        "node",
        "body",
        "set",
        id,
        "--body",
        "Corrected notes",
        "--project",
        "demo",
    ]);
    cli(&["node", "state", id, "archived", "--project", "demo"]);
    let path = project.join(format!(".orgasmic/meetings/{id}/node.org"));
    let file =
        orgasmic_core::OrgFile::parse(std::fs::read_to_string(&path).unwrap(), "node.org").unwrap();
    let heading = file.find_by_id(id).unwrap();
    assert_eq!(heading.todo.as_deref(), Some("ARCHIVED"));
    assert_eq!(heading.property("OCCURRED_AT"), Some("2026-09-07"));
    assert!(file.source().contains("Corrected notes"));
    assert!(!project.join(format!(".orgasmic/glossary/{id}")).exists());
    let _ = running.shutdown.send(());
    running.join.await.unwrap();
}
