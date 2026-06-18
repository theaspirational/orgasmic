use std::process::Command;

use orgasmic_core::Home;

fn write(path: &std::path::Path, contents: &str) {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).unwrap();
    }
    std::fs::write(path, contents).unwrap();
}

#[test]
fn init_hints_when_source_checkout_missing() {
    let tmp = tempfile::tempdir().unwrap();
    let home = Home::at(tmp.path().join("home"));

    let output = Command::new(env!("CARGO_BIN_EXE_orgasmic"))
        .arg("init")
        .env("ORGASMIC_HOME", &home.root)
        .output()
        .expect("run orgasmic init");
    assert!(
        output.status.success(),
        "init failed\nstdout={}\nstderr={}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("hint: runtime content missing"));
    assert!(stdout.contains("scripts/install.sh"));
    assert!(stdout.contains(&home.source().display().to_string()));
    assert!(stdout.contains("Next:"));
    assert!(stdout.contains("orgasmic doctor"));
}

#[test]
fn init_registers_source_checkout_as_default_project() {
    let tmp = tempfile::tempdir().unwrap();
    let home = Home::at(tmp.path().join("home"));
    let source = home.source();
    write(
        &source.join(".orgasmic/project.org"),
        "#+title: orgasmic project\n#+orgasmic_version: 1\n\n* PROJECT orgasmic\n:PROPERTIES:\n:ID:                  orgasmic\n:DEFAULT_BRANCH:      main\n:END:\n",
    );

    let output = Command::new(env!("CARGO_BIN_EXE_orgasmic"))
        .arg("init")
        .env("ORGASMIC_HOME", &home.root)
        .output()
        .expect("run orgasmic init");
    assert!(
        output.status.success(),
        "init failed\nstdout={}\nstderr={}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("registered default project orgasmic"));

    let board = std::fs::read_to_string(home.board()).unwrap();
    assert!(board.contains(":ID:               orgasmic"));
    let canonical_source = std::fs::canonicalize(&source).unwrap();
    assert!(board.contains(&format!(
        ":PATH:             {}",
        canonical_source.display()
    )));
    assert!(!board.contains(":REPO_URL:"));
    assert!(board.contains(":BRANCH:           main"));

    let rerun = Command::new(env!("CARGO_BIN_EXE_orgasmic"))
        .arg("init")
        .env("ORGASMIC_HOME", &home.root)
        .output()
        .expect("rerun orgasmic init");
    assert!(rerun.status.success());
    let rerun_stdout = String::from_utf8_lossy(&rerun.stdout);
    assert!(rerun_stdout.contains("default project orgasmic already registered"));
}

#[test]
#[cfg(unix)]
fn init_hints_when_binary_symlink_is_broken() {
    let tmp = tempfile::tempdir().unwrap();
    let home = Home::at(tmp.path().join("home"));
    home.ensure().unwrap();
    let bin = home.bin_orgasmic();
    std::os::unix::fs::symlink(tmp.path().join("does-not-exist/orgasmic"), &bin).unwrap();

    let output = Command::new(env!("CARGO_BIN_EXE_orgasmic"))
        .arg("init")
        .env("ORGASMIC_HOME", &home.root)
        .output()
        .expect("run orgasmic init");
    assert!(
        output.status.success(),
        "init failed\nstdout={}\nstderr={}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("hint: broken binary symlink"));
    assert!(stdout.contains("orgasmic update"));
    assert!(stdout.contains("Next:"));
    assert!(stdout.contains("orgasmic doctor"));
}
