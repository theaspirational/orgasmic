//! orgasmic:task_K5NDR
//!
//! Isolation of spawned-CLI tests from the operator's ambient orgasmic
//! environment.
//!
//! A `std::process::Command` inherits the parent's environment, and the parent
//! of every test here is whatever shell ran `cargo test`. Inside a dispatch
//! that shell is a worker pane, and the driver exports `ORGASMIC_RUN_ID` into
//! it (`modes/tmux.rs`) while the dispatch harness exports
//! `ORGASMIC_HOME`. Both are read by production paths the tests drive —
//! `manager.rs` resolves `dispatch finalize` through `ORGASMIC_RUN_ID`,
//! `orgasmic_core::Home` resolves through `ORGASMIC_HOME` — so a test that
//! inherits them addresses the OPERATOR's live run instead of the one its own
//! fixture just created, and dies with `no live run ...`.
//!
//! That made the suite's result depend on who ran it and from where: a manager
//! shell has neither variable set and sees green, a dispatched worker running
//! the identical command on the identical tree sees red (TASK-K5NDR). The
//! `env -u ORGASMIC_RUN_ID -u ORGASMIC_HOME` prefix in `scripts/run-tests.sh`
//! and `orgasmic verify` is the wrapper workaround for exactly this; the
//! fixture below is the fix, so a bare `cargo test -p orgasmic-cli` is safe.
//!
//! The rule is deny-by-default: every `ORGASMIC_*` variable present in the
//! parent is removed from the child, and a test that wants one says so with an
//! explicit `.env(...)` after construction. Deny-by-default (rather than a
//! list of known-bad names) is what keeps the next steering variable to land
//! from silently reopening this hole.
#![allow(dead_code)]

use std::path::PathBuf;
use std::process::Command;

/// The only `ORGASMIC_*` variables a spawned child may inherit.
///
/// `ORGASMIC_ALLOW_MISSING_TOOLS` is the operator's explicit waiver for absent
/// tooling (`orgasmic_drivers::test_tooling`), passed in on
/// purpose by `scripts/run-tests.sh`. It records a decision about the machine,
/// not about which run/home/daemon to address, and a child that probed tooling
/// without it would waive differently from its parent — so it propagates.
pub const INHERITED_ORGASMIC_ENV: &[&str] = &["ORGASMIC_ALLOW_MISSING_TOOLS"];

/// A `Command` for the built `orgasmic` binary with the ambient orgasmic
/// environment already scrubbed.
///
/// Use this in place of `Command::new(orgasmic_exe())` everywhere a test
/// spawns the CLI. Callers keep chaining `.env(...)` for the values the test
/// actually means to set; those run after the scrub and win.
pub fn orgasmic_command() -> Command {
    let mut command = Command::new(orgasmic_exe());
    scrub_ambient_orgasmic_env(&mut command);
    command
}

/// Remove every inherited `ORGASMIC_*` variable except [`INHERITED_ORGASMIC_ENV`].
///
/// Exposed separately for the few commands that are not the orgasmic binary
/// but still resolve through the same variables.
pub fn scrub_ambient_orgasmic_env(command: &mut Command) -> &mut Command {
    for (key, _) in std::env::vars_os() {
        let Some(key) = key.to_str() else { continue };
        if key.starts_with("ORGASMIC_") && !INHERITED_ORGASMIC_ENV.contains(&key) {
            command.env_remove(key);
        }
    }
    command
}

/// Absolute path to the `orgasmic` binary cargo built for this test target.
pub fn orgasmic_exe() -> PathBuf {
    let exe = PathBuf::from(env!("CARGO_BIN_EXE_orgasmic"));
    if exe.is_absolute() {
        exe
    } else {
        std::env::current_dir().unwrap().join(exe)
    }
}
