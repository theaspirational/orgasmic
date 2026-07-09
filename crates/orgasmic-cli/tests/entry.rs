#[allow(dead_code)]
mod common;

use std::process::Command;

use common::{orgasmic_exe, write};
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
<!-- workflow: resolved by TASK-PCDA2 -->
";

fn seed_router(home: &Home) {
    write(
        &home.source().join("shipped/entry/router.org"),
        ROUTER_FIXTURE,
    );
}

#[test]
fn entry_prints_runtime_router_outside_project() {
    let tmp = tempfile::tempdir().unwrap();
    let home = Home::at(tmp.path().join("home"));
    seed_router(&home);

    let output = Command::new(orgasmic_exe())
        .arg("entry")
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
    assert!(stdout.contains("<!-- workflow: resolved by TASK-PCDA2 -->"));
    assert!(!stdout.contains("notice: .orgasmic/entry.org"));
}

#[test]
fn entry_warns_on_project_stub_version_skew() {
    let tmp = tempfile::tempdir().unwrap();
    let home = Home::at(tmp.path().join("home"));
    seed_router(&home);

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

    let output = Command::new(orgasmic_exe())
        .arg("entry")
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
