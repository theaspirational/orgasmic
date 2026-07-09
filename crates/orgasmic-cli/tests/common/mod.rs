use std::path::Path;
use std::path::PathBuf;
use std::process::Command;

use orgasmic_core::Home;
use orgasmic_daemon::DaemonOptions;

pub fn test_options() -> DaemonOptions {
    DaemonOptions {
        bind_override: Some("127.0.0.1".parse().unwrap()),
        port_override: Some(0),
        ..DaemonOptions::default()
    }
}

pub fn write(path: &Path, contents: &str) {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).unwrap();
    }
    std::fs::write(path, contents).unwrap();
}

#[allow(dead_code)]
const REQUIRED_SHIPPED: &[&str] = &[
    "project.org",
    "schema/tx.org",
    "prompt-studio/slots.org",
    "schema/state-machine.org",
    "entry/router.org",
    "skills/orgasmic/scaffold/.gitignore",
    "skills/orgasmic/scaffold/entry.org",
    "skills/orgasmic/scaffold/project.org",
    "skills/orgasmic/scaffold/decisions.org",
    "skills/orgasmic/scaffold/tasks/backlog.org",
    "skills/orgasmic/scaffold/tasks/todo.org",
    "skills/orgasmic/scaffold/tasks/in_progress.org",
    "skills/orgasmic/scaffold/tasks/in_review.org",
    "skills/orgasmic/scaffold/tasks/done.org",
    "skills/orgasmic/scaffold/tasks/cancelled.org",
    "skills/orgasmic/scaffold/tasks/goal.org",
    "skills/orgasmic/scaffold/tasks/handoff.org",
    "skills/orgasmic/scaffold/gotchas.org",
];

#[allow(dead_code)]
pub fn seed_required_shipped(source: &Path) {
    for rel in REQUIRED_SHIPPED {
        write(&source.join("shipped").join(rel), "# test fixture\n");
    }
}

#[allow(dead_code)]
pub fn init_git_repo(repo: &Path) {
    std::fs::create_dir_all(repo).unwrap();
    run_git(repo, &["init"]);
    run_git(repo, &["config", "user.email", "tester@example.com"]);
    run_git(repo, &["config", "user.name", "Test User"]);
}

#[allow(dead_code)]
pub fn run_git(repo: &Path, args: &[&str]) -> String {
    let output = Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(args)
        .output()
        .expect("run git");
    assert!(
        output.status.success(),
        "git {:?} failed\nstdout={}\nstderr={}",
        args,
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8_lossy(&output.stdout).trim().to_string()
}

#[allow(dead_code)]
pub fn unused_port() -> u16 {
    let listener = std::net::TcpListener::bind(("127.0.0.1", 0)).unwrap();
    listener.local_addr().unwrap().port()
}

#[allow(dead_code)]
pub fn write_config_port(home: &Home, port: u16) {
    write(
        &home.config(),
        &format!("bind_host: 127.0.0.1\nbind_port: {port}\n"),
    );
}

pub fn orgasmic_exe() -> PathBuf {
    let exe = PathBuf::from(env!("CARGO_BIN_EXE_orgasmic"));
    if exe.is_absolute() {
        exe
    } else {
        std::env::current_dir().unwrap().join(exe)
    }
}
