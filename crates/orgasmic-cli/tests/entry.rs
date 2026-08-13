#[allow(dead_code)]
mod common;

use common::{orgasmic_command, write};
use orgasmic_core::{projects, Home};

const ROUTER_FIXTURE: &str = "\
#+title: test router
#+orgasmic_version: 1

* Entry

** Agent router
runtime agent router

** Project map
runtime project map

** Write rules
runtime write rules

** Default workflow
";

#[test]
fn shipped_router_maps_only_the_exact_manager_wake_marker_to_resume() {
    let router = include_str!("../../../shipped/entry/router.org");
    assert!(
        router.contains("The exact injected marker =: 'ORGASMIC_MANAGER_WAKE_V1'=")
            && router.contains("Treat it exactly as =/orgasmic resume=:")
            && router.contains("It carries no task,\nproject, or instruction text"),
        "the shipped entry router must recognize exactly the fixed, content-free wake marker"
    );
}

fn seed_router(home: &Home) {
    write(
        &home.source().join("shipped/entry/router.org"),
        ROUTER_FIXTURE,
    );
}

fn seed_shipped_workflow(home: &Home, body: &str) {
    write(&home.source().join("shipped/workflows/default.org"), body);
}

#[test]
fn entry_prints_runtime_router_outside_project() {
    let tmp = tempfile::tempdir().unwrap();
    let home = Home::at(tmp.path().join("home"));
    seed_router(&home);
    seed_shipped_workflow(
        &home,
        "* WORKFLOW default\n\nshipped default workflow body\n",
    );

    let output = orgasmic_command()
        .arg("entry")
        .arg("--full")
        .env("ORGASMIC_HOME", &home.root)
        .current_dir(tmp.path())
        .output()
        .expect("run orgasmic entry");

    assert!(
        output.status.success(),
        "entry failed\nstdout={}\nstderr={}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("** Agent router"));
    assert!(stdout.contains("** Project map"));
    assert!(stdout.contains("** Write rules"));
    assert!(stdout.contains("shipped default workflow body"));
    assert!(!stdout.contains("notice: .orgasmic/entry.org"));
}

#[test]
fn entry_resolves_workflow_project_user_shipped_order() {
    let tmp = tempfile::tempdir().unwrap();
    let home = Home::at(tmp.path().join("home"));
    seed_router(&home);
    seed_shipped_workflow(&home, "* WORKFLOW default\n\nSHIPPED-WORKFLOW\n");
    write(
        &home.user().join("workflows/default.org"),
        "* WORKFLOW default\n\nUSER-WORKFLOW\n",
    );

    let outside = orgasmic_command()
        .arg("entry")
        .arg("--full")
        .env("ORGASMIC_HOME", &home.root)
        .current_dir(tmp.path())
        .output()
        .expect("run orgasmic entry outside project");
    assert!(
        outside.status.success(),
        "entry failed\nstdout={}\nstderr={}",
        String::from_utf8_lossy(&outside.stdout),
        String::from_utf8_lossy(&outside.stderr)
    );
    let outside_stdout = String::from_utf8_lossy(&outside.stdout);
    assert!(outside_stdout.contains("USER-WORKFLOW"));
    assert!(!outside_stdout.contains("SHIPPED-WORKFLOW"));

    let project_root = tmp.path().join("repo");
    let nested = project_root.join("src");
    std::fs::create_dir_all(&nested).unwrap();
    write(
        &project_root.join(".orgasmic/project.org"),
        "#+title: entry project\n#+orgasmic_version: 1\n\n* PROJECT entry-project\n:PROPERTIES:\n:ID:                  entry-project\n:END:\n",
    );
    write(
        &project_root.join(".orgasmic/entry.org"),
        "#+title: orgasmic entry\n#+orgasmic_version: 1\n\n* Entry\n",
    );
    write(
        &project_root.join(".orgasmic/workflows/default.org"),
        "* WORKFLOW default\n\nPROJECT-WORKFLOW\n",
    );

    let inside = orgasmic_command()
        .arg("entry")
        .arg("--full")
        .env("ORGASMIC_HOME", &home.root)
        .current_dir(&nested)
        .output()
        .expect("run orgasmic entry inside project");
    assert!(
        inside.status.success(),
        "entry failed\nstdout={}\nstderr={}",
        String::from_utf8_lossy(&inside.stdout),
        String::from_utf8_lossy(&inside.stderr)
    );
    let inside_stdout = String::from_utf8_lossy(&inside.stdout);
    assert!(inside_stdout.contains("PROJECT-WORKFLOW"));
    assert!(!inside_stdout.contains("USER-WORKFLOW"));
    assert!(!inside_stdout.contains("SHIPPED-WORKFLOW"));
}

#[test]
fn entry_warns_on_project_stub_version_skew() {
    let tmp = tempfile::tempdir().unwrap();
    let home = Home::at(tmp.path().join("home"));
    seed_router(&home);
    seed_shipped_workflow(
        &home,
        "* WORKFLOW default\n\nshipped default workflow body\n",
    );

    let project_root = tmp.path().join("repo");
    let nested = project_root.join("src");
    std::fs::create_dir_all(&nested).unwrap();
    write(
        &project_root.join(".orgasmic/project.org"),
        "#+title: entry project\n#+orgasmic_version: 1\n\n* PROJECT entry-project\n:PROPERTIES:\n:ID:                  entry-project\n:END:\n",
    );
    write(
        &project_root.join(".orgasmic/entry.org"),
        "#+title: orgasmic entry\n#+orgasmic_version: 2\n\n* Entry\n\nRun `orgasmic entry`.\n",
    );
    projects::register_project(&home, &project_root, "entry-project", "main").unwrap();

    let output = orgasmic_command()
        .arg("entry")
        .arg("--full")
        .env("ORGASMIC_HOME", &home.root)
        .current_dir(&nested)
        .output()
        .expect("run orgasmic entry");

    assert!(
        output.status.success(),
        "entry failed\nstdout={}\nstderr={}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains(
        "notice: .orgasmic/entry.org scaffold version 2 does not match runtime version 1"
    ));
    assert!(stdout.contains("run `orgasmic project migrate` when available"));
    assert!(stdout.contains("runtime agent router"));
}

#[test]
fn entry_defaults_to_a_small_versioned_fast_route() {
    let tmp = tempfile::tempdir().unwrap();
    let home = Home::at(tmp.path().join("home"));
    seed_router(&home);
    seed_shipped_workflow(
        &home,
        "* WORKFLOW default\n\nworkflow content that should stay out of fast output\n",
    );

    let output = orgasmic_command()
        .arg("entry")
        .env("ORGASMIC_HOME", &home.root)
        .current_dir(tmp.path())
        .output()
        .expect("run fast orgasmic entry");
    assert!(output.status.success());
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.starts_with("ORGASMIC_ENTRY_FAST_V1 "));
    assert!(stdout.contains("STATE use orgasmic CLI/daemon writes"));
    assert!(stdout.contains("FULL orgasmic entry --full"));
    assert!(!stdout.contains("workflow content that should stay out of fast output"));
    assert!(
        stdout.len() < 2_000,
        "fast route grew unexpectedly: {} bytes",
        stdout.len()
    );
}

#[test]
fn entry_ack_collapses_an_unchanged_router_to_one_line() {
    let tmp = tempfile::tempdir().unwrap();
    let home = Home::at(tmp.path().join("home"));
    seed_router(&home);
    seed_shipped_workflow(&home, "* WORKFLOW default\n\nworkflow body\n");

    let first = orgasmic_command()
        .arg("entry")
        .env("ORGASMIC_HOME", &home.root)
        .current_dir(tmp.path())
        .output()
        .expect("run first orgasmic entry");
    let first_stdout = String::from_utf8_lossy(&first.stdout);
    let fingerprint = first_stdout
        .lines()
        .next()
        .and_then(|line| line.split_whitespace().nth(1))
        .expect("fast route fingerprint");

    let ack = orgasmic_command()
        .args(["entry", "--ack", fingerprint])
        .env("ORGASMIC_HOME", &home.root)
        .current_dir(tmp.path())
        .output()
        .expect("ack orgasmic entry");
    assert!(ack.status.success());
    assert_eq!(
        String::from_utf8_lossy(&ack.stdout),
        format!("ORGASMIC_ENTRY_UNCHANGED {fingerprint}\n")
    );
}
