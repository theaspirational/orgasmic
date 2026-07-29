use std::path::Path;
use std::process::Command;

use orgasmic_core::Home;
use orgasmic_daemon::DaemonOptions;

pub mod env_isolation;

// orgasmic:task_K5NDR
// The CLI is spawned through `env_isolation::orgasmic_command`, never through a
// bare `Command::new(orgasmic_exe())`, so ambient `ORGASMIC_*` cannot steer a
// test's child. Re-exported here so callers keep their existing import path.
#[allow(unused_imports)]
pub use env_isolation::{orgasmic_command, orgasmic_exe};

#[allow(dead_code)]
pub fn test_options() -> DaemonOptions {
    DaemonOptions {
        bind_override: Some("127.0.0.1".parse().unwrap()),
        port_override: Some(0),
        ..DaemonOptions::default()
    }
}

#[allow(dead_code)]
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
    "workflows/default.org",
    "project-scaffold/.gitignore",
    "project-scaffold/entry.org",
    "project-scaffold/project.org",
    "project-scaffold/decisions.org",
    "project-scaffold/tasks/backlog.org",
    "project-scaffold/tasks/todo.org",
    "project-scaffold/tasks/in_progress.org",
    "project-scaffold/tasks/in_review.org",
    "project-scaffold/tasks/done.org",
    "project-scaffold/tasks/cancelled.org",
    "project-scaffold/tasks/goal.org",
    "project-scaffold/tasks/handoff.org",
    "project-scaffold/gotchas.org",
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
