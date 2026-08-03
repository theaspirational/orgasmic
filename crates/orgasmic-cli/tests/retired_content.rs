// orgasmic:dec_WDR5K
//! Retired-content residue must be reachable by an agent, not only by whoever
//! tails the daemon log (TASK-8ED6V).
//!
//! The 2026-07-25 incident is the shape these guard against: a manager found
//! `~/.orgasmic/user/workers/*.org`, read `:DEFAULT_MODEL: gpt-5.6-sol` out of
//! files the runtime had stopped loading a week earlier, told the operator it
//! had bypassed a configured reviewer, and proposed restoring the very concept
//! `dec_WDR5K` had removed. The daemon had logged the correct warning at every
//! boot for days. Nothing the manager read said the files were dead.

use std::path::PathBuf;

use orgasmic_core::{Home, RETIRED_CONTENT};

mod common;

use common::{orgasmic_command, seed_required_shipped, write};

fn repo_root() -> PathBuf {
    let mut here = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    loop {
        if here.join(".orgasmic").is_dir() && here.join("shipped").is_dir() {
            return here;
        }
        if !here.pop() {
            panic!("could not locate orgasmic repo root from CARGO_MANIFEST_DIR");
        }
    }
}

/// The maintenance discipline, enforced rather than remembered: a decision that
/// retires a content family lands its path in the shared table, and this fails
/// until the entry point every agent actually reads names it too. `doctor` alone
/// would not have caught the incident, because the manager never ran `doctor`
/// either — but every agent reads the router on every session.
#[test]
fn entry_router_names_every_retired_path_and_its_deciding_node() {
    let path = repo_root().join("shipped/entry/router.org");
    let router =
        std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("read {}: {e}", path.display()));

    assert!(
        router.contains("** Retired content"),
        "{} lost its `** Retired content` section; retired paths are then named \
         nowhere an agent reads",
        path.display()
    );
    for entry in RETIRED_CONTENT {
        assert!(
            router.contains(entry.rel_path),
            "shipped/entry/router.org does not name the retired path `{}`. Add it to the \
             `** Retired content` section in the same release that added it to \
             orgasmic_core::retired::RETIRED_CONTENT — a residue path known only to \
             `doctor` is invisible to an agent that never runs `doctor`.",
            entry.rel_path
        );
        assert!(
            router.contains(entry.deciding_node),
            "shipped/entry/router.org names `{}` without its deciding node `{}`. A reader \
             who disbelieves the claim has to be able to look the rationale up, or the \
             cheapest next move is to argue the concept back.",
            entry.rel_path,
            entry.deciding_node
        );
    }
}

/// The file-content guard above is not enough on its own: `orgasmic entry`
/// renders the router rather than printing it, and the workflow injection
/// truncates everything from `** Default workflow` onward. A section that
/// drifted below that point would still satisfy the guard and reach no agent.
/// This drives the real binary over the real shipped router.
#[test]
fn entry_output_carries_the_retired_content_section() {
    let tmp = tempfile::tempdir().unwrap();
    let home = Home::at(tmp.path().join("home"));
    home.ensure().unwrap();
    let shipped = repo_root().join("shipped");
    for rel in ["entry/router.org", "workflows/default.org"] {
        let source = shipped.join(rel);
        write(
            &home.source().join("shipped").join(rel),
            &std::fs::read_to_string(&source)
                .unwrap_or_else(|e| panic!("read {}: {e}", source.display())),
        );
    }

    let output = orgasmic_command()
        .arg("entry")
        .env("ORGASMIC_HOME", &home.root)
        .current_dir(tmp.path())
        .output()
        .expect("run orgasmic entry");
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        output.status.success(),
        "entry failed\nstdout={stdout}\nstderr={}",
        String::from_utf8_lossy(&output.stderr)
    );

    assert!(
        stdout.contains("** Retired content"),
        "`orgasmic entry` output has no `** Retired content` section\nstdout={stdout}"
    );
    for entry in RETIRED_CONTENT {
        assert!(
            stdout.contains(entry.rel_path) && stdout.contains(entry.deciding_node),
            "`orgasmic entry` output does not name `{}` with `{}`; the section is in the \
             file but not in what an agent reads\nstdout={stdout}",
            entry.rel_path,
            entry.deciding_node
        );
    }
}

/// Doctor names the residue, says which decision retired it, and offers removal.
#[test]
fn doctor_reports_retired_content_with_deciding_node_and_offers_removal() {
    let tmp = tempfile::tempdir().unwrap();
    let home = Home::at(tmp.path().join("home"));
    home.ensure().unwrap();
    seed_required_shipped(&home.source());
    // The exact residue of the 2026-07-25 incident, contents and all.
    write(
        &home.root.join("user/workers/reviewer-codex-acp.org"),
        "* WORKER reviewer-codex-acp\n:PROPERTIES:\n:DEFAULT_MODEL: gpt-5.6-sol\n:END:\n",
    );

    let output = orgasmic_command()
        .arg("doctor")
        .env("ORGASMIC_HOME", &home.root)
        .output()
        .expect("run orgasmic doctor");
    let stdout = String::from_utf8_lossy(&output.stdout);

    assert!(
        stdout.contains("[warn] retired content on disk:"),
        "doctor did not report the residue\nstdout={stdout}"
    );
    assert!(
        stdout.contains("user/workers"),
        "doctor did not name the residue path\nstdout={stdout}"
    );
    assert!(
        stdout.contains("dec_WDR5K"),
        "doctor named the path but not the deciding node\nstdout={stdout}"
    );
    assert!(
        stdout.contains("orgasmic decision get dec_WDR5K"),
        "doctor did not say how to read the rationale\nstdout={stdout}"
    );
    assert!(
        stdout.contains("orgasmic doctor --remove-retired"),
        "doctor did not offer removal\nstdout={stdout}"
    );
    // Reporting must not delete: this is the operator's data.
    assert!(
        home.root
            .join("user/workers/reviewer-codex-acp.org")
            .is_file(),
        "plain `doctor` removed the operator's files"
    );
}

/// Removal is opt-in and never a side effect. `--fix` repairs runtime-owned
/// things; it must not take an operator's files with it.
#[test]
fn doctor_fix_does_not_remove_retired_content() {
    let tmp = tempfile::tempdir().unwrap();
    let home = Home::at(tmp.path().join("home"));
    home.ensure().unwrap();
    seed_required_shipped(&home.source());
    write(
        &home.root.join("user/workers/implementer-codex-acp.org"),
        "* WORKER implementer-codex-acp\n",
    );

    let output = orgasmic_command()
        .arg("doctor")
        .arg("--fix")
        .arg("--no-modify-path")
        .env("ORGASMIC_HOME", &home.root)
        .output()
        .expect("run orgasmic doctor --fix");

    assert!(
        home.root
            .join("user/workers/implementer-codex-acp.org")
            .is_file(),
        "`doctor --fix` silently removed retired content\nstdout={}\nstderr={}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

#[test]
fn doctor_remove_retired_deletes_on_request_and_reports_each_path() {
    let tmp = tempfile::tempdir().unwrap();
    let home = Home::at(tmp.path().join("home"));
    home.ensure().unwrap();
    seed_required_shipped(&home.source());
    write(
        &home.root.join("user/workers/implementer-codex-acp.org"),
        "* WORKER implementer-codex-acp\n",
    );

    let output = orgasmic_command()
        .arg("doctor")
        .arg("--remove-retired")
        .env("ORGASMIC_HOME", &home.root)
        .output()
        .expect("run orgasmic doctor --remove-retired");
    let stdout = String::from_utf8_lossy(&output.stdout);

    assert!(
        stdout.contains("removed retired content:"),
        "removal was silent\nstdout={stdout}"
    );
    assert!(
        stdout.contains("user/workers"),
        "removal did not name what it removed\nstdout={stdout}"
    );
    assert!(
        !home.root.join("user/workers").exists(),
        "residue survived --remove-retired\nstdout={stdout}"
    );
    // The warning is gone on the same run that removed it.
    assert!(
        !stdout.contains("retired content on disk:"),
        "doctor still reported residue it had just removed\nstdout={stdout}"
    );
}

#[test]
fn doctor_remove_retired_is_a_no_op_on_a_clean_home() {
    let tmp = tempfile::tempdir().unwrap();
    let home = Home::at(tmp.path().join("home"));
    home.ensure().unwrap();
    seed_required_shipped(&home.source());

    let output = orgasmic_command()
        .arg("doctor")
        .arg("--remove-retired")
        .env("ORGASMIC_HOME", &home.root)
        .output()
        .expect("run orgasmic doctor --remove-retired");
    let stdout = String::from_utf8_lossy(&output.stdout);

    assert!(
        stdout.contains("no retired content on disk"),
        "clean home did not say so plainly\nstdout={stdout}"
    );
}
