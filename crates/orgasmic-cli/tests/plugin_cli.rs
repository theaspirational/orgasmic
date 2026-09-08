mod common;
use orgasmic_core::Home;
use orgasmic_daemon::{Daemon, DaemonOptions};
use serde_json::Value;

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn meetings_example_imports_notes_via_plugin_run() {
    let temp = tempfile::tempdir().unwrap();
    let home = Home::at(temp.path().join("home"));
    home.ensure().unwrap();
    let root = temp.path().join("project");
    common::write(
        &root.join(".orgasmic/project.org"),
        "* PROJECT demo\n:PROPERTIES:\n:ID: demo\n:END:\n",
    );
    common::write(
        &home.board(),
        &format!(
            "* PROJECT demo\n:PROPERTIES:\n:ID: demo\n:PATH: {}\n:BRANCH: main\n:END:\n",
            root.display()
        ),
    );
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
            .current_dir(&root)
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
    let example =
        std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../examples/plugins/meetings");
    cli(&["plugin", "add", example.to_str().unwrap()]);
    cli(&["plugin", "check", "meetings"]);
    cli(&["plugin", "enable", "meetings", "--project", "demo", "--yes"]);
    let notes = temp.path().join("notes.txt");
    common::write(&notes, "Протокол зустрічі — plain notes.\n");
    let args = [
        "plugin",
        "run",
        "meetings",
        "import",
        "--project",
        "demo",
        "--",
        notes.to_str().unwrap(),
        "--title",
        "Planning",
        "--request-id",
        "meeting-import-retry",
    ];
    let first: Value = serde_json::from_str(&cli(&args)).unwrap();
    let retry: Value = serde_json::from_str(&cli(&args)).unwrap();
    assert_eq!(first["id"], retry["id"]);
    let id = first["id"].as_str().unwrap();
    assert!(id.starts_with("MEET-"));
    let document =
        std::fs::read_to_string(root.join(format!(".orgasmic/meetings/{id}/node.org"))).unwrap();
    assert!(document.contains("#+plugin: meetings schema=1"));
    assert!(
        document.contains("ACTIVE")
            && document.contains("Planning")
            && document.contains("Протокол зустрічі")
    );
    let journal =
        std::fs::read_to_string(root.join(format!(".orgasmic/meetings/{id}/journal.org"))).unwrap();
    assert!(journal.contains("plugin:meetings"));
    let _ = running.shutdown.send(());
    running.join.await.unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn plugin_packaging_and_child_cli_use_the_scoped_principal() {
    let temp = tempfile::tempdir().unwrap();
    let home = Home::at(temp.path().join("home"));
    home.ensure().unwrap();
    let root = temp.path().join("project");
    common::write(
        &root.join(".orgasmic/project.org"),
        "* PROJECT demo\n:PROPERTIES:\n:ID: demo\n:END:\n",
    );
    common::write(
        &home.board(),
        &format!(
            "* PROJECT demo\n:PROPERTIES:\n:ID: demo\n:PATH: {}\n:BRANCH: main\n:END:\n",
            root.display()
        ),
    );
    let running = Daemon::run(
        home.clone(),
        DaemonOptions {
            fs_watcher_enabled: false,
            ..common::test_options()
        },
    )
    .await
    .unwrap();
    let viewer =
        orgasmic_core::add_member(&home, "viewer", &[("demo".into(), "viewer".into())]).unwrap();
    let cli = |args: &[&str], member: Option<&str>, success: bool| {
        let mut command = common::orgasmic_command();
        command
            .args(args)
            .current_dir(&root)
            .env("ORGASMIC_HOME", &home.root)
            .env("ORGASMIC_DAEMON_URL", format!("http://{}", running.addr))
            .env("ORGASMIC_TEST_CLI", common::orgasmic_exe());
        if let Some(token) = member {
            command.env("ORGASMIC_DAEMON_TOKEN", token);
        }
        let output = command.output().unwrap();
        assert_eq!(
            output.status.success(),
            success,
            "{args:?}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        String::from_utf8(output.stdout).unwrap()
    };
    cli(&["plugin", "scaffold", "meetings"], None, true);
    let dir = home.user().join("plugins/meetings");
    let original = std::fs::read_to_string(dir.join("plugin.org")).unwrap();
    common::write(
        &dir.join("plugin.org"),
        &original.replace(":CAPABILITIES:", ":COMMANDS: create escape\n:CAPABILITIES:"),
    );
    common::write(&dir.join("bin/create"), "#!/bin/sh\nexec \"$ORGASMIC_TEST_CLI\" node create --kind meetings --title 'From child CLI' --project \"$ORGASMIC_PROJECT\"\n");
    common::write(&dir.join("bin/escape"), "#!/bin/sh\nexec \"$ORGASMIC_TEST_CLI\" node create --kind notes --title forbidden --project \"$ORGASMIC_PROJECT\"\n");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        for name in ["create", "escape"] {
            std::fs::set_permissions(
                dir.join("bin").join(name),
                std::fs::Permissions::from_mode(0o755),
            )
            .unwrap();
        }
    }
    cli(&["plugin", "check", "meetings"], None, true);
    cli(&["plugin", "scaffold", "notes"], None, true);
    cli(
        &["plugin", "enable", "notes", "--project", "demo", "--yes"],
        None,
        true,
    );
    cli(
        &["plugin", "enable", "meetings", "--project", "demo", "--yes"],
        Some(&viewer),
        false,
    );
    cli(
        &["plugin", "enable", "meetings", "--project", "demo", "--yes"],
        None,
        true,
    );
    let listed: Value =
        serde_json::from_str(&cli(&["plugin", "list", "--project", "demo"], None, true)).unwrap();
    assert!(listed
        .as_array()
        .unwrap()
        .iter()
        .any(|p| p["id"] == "meetings" && p["enabled"] == true));
    let minted = cli(
        &["id", "mint", "--class", "meetings", "--project", "demo"],
        None,
        true,
    );
    assert!(minted.starts_with("MEETINGS-"));
    cli(
        &["plugin", "run", "meetings", "create", "--project", "demo"],
        Some(&viewer),
        false,
    );
    let created: Value = serde_json::from_str(&cli(
        &["plugin", "run", "meetings", "create", "--project", "demo"],
        None,
        true,
    ))
    .unwrap();
    let id = created["id"].as_str().unwrap();
    assert!(root
        .join(format!(".orgasmic/meetings/{id}/node.org"))
        .is_file());
    cli(
        &["plugin", "run", "meetings", "escape", "--project", "demo"],
        None,
        false,
    );
    cli(
        &["plugin", "disable", "meetings", "--project", "demo"],
        None,
        true,
    );
    cli(
        &["id", "mint", "--class", "meetings", "--project", "demo"],
        None,
        false,
    );
    cli(
        &[
            "node",
            "body",
            "set",
            id,
            "--project",
            "demo",
            "--body",
            "denied",
        ],
        None,
        false,
    );
    cli(
        &["plugin", "enable", "meetings", "--project", "demo", "--yes"],
        None,
        true,
    );
    cli(
        &[
            "node",
            "body",
            "set",
            id,
            "--project",
            "demo",
            "--body",
            "retained",
        ],
        None,
        true,
    );
    let removed: Value =
        serde_json::from_str(&cli(&["plugin", "remove", "meetings"], None, true)).unwrap();
    assert!(removed["data_retained"].as_bool().unwrap());
    let backup = removed["recoverable_at"].as_str().unwrap();
    assert!(std::path::Path::new(backup).is_dir());
    cli(&["plugin", "add", backup], None, true);
    // Removal disables even a same-version reinstallation.
    cli(
        &["plugin", "run", "meetings", "create", "--project", "demo"],
        None,
        false,
    );
    cli(
        &["plugin", "enable", "meetings", "--project", "demo", "--yes"],
        None,
        true,
    );
    cli(
        &[
            "node",
            "body",
            "set",
            id,
            "--project",
            "demo",
            "--body",
            "restored",
        ],
        None,
        true,
    );
    let _ = running.shutdown.send(());
    running.join.await.unwrap();
}
