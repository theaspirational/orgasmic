//! Contract discoverability (TASK-SPBTA): `artifact blocks --full` and the
//! `architecture|decision|glossary schema` verbs must be daemon-free and
//! must not point at dangling paths. These are process-level smoke checks
//! that never touch the daemon.

use std::path::PathBuf;
use std::process::{Command, Output};

fn run(args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_orgasmic"))
        .args(args)
        .output()
        .unwrap_or_else(|e| panic!("run orgasmic {args:?}: {e}"))
}

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

#[test]
fn artifact_blocks_full_points_at_real_files_without_a_daemon() {
    let output = run(&["artifact", "blocks", "--full"]);
    assert!(
        output.status.success(),
        "artifact blocks --full failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8(output.stdout).expect("utf8 stdout");

    let spec_path = "shipped/prompt-studio/prompt-specs/artifact-generator.org";
    let fixture_path = "ui/src/lib/artifacts/__fixtures__/all-blocks.ts";
    assert!(stdout.contains(spec_path), "missing pointer to {spec_path}");
    assert!(
        stdout.contains(fixture_path),
        "missing pointer to {fixture_path}"
    );

    let root = repo_root();
    assert!(
        root.join(spec_path).is_file(),
        "block contract pointer is dangling: {spec_path}"
    );
    assert!(
        root.join(fixture_path).is_file(),
        "block contract fixture pointer is dangling: {fixture_path}"
    );
}

#[test]
fn artifact_submit_help_points_at_block_contract() {
    let output = run(&["artifact", "submit", "--help"]);
    assert!(output.status.success(), "artifact submit --help failed");
    let stdout = String::from_utf8(output.stdout).expect("utf8 stdout");
    assert!(
        stdout.contains("blocks --full") || stdout.contains("artifact-generator.org"),
        "artifact submit --help does not point at the block contract:\n{stdout}"
    );
}

#[test]
fn node_schema_verbs_run_without_a_daemon() {
    for (kind, id_prefix) in [
        ("architecture", "arch_"),
        ("decision", "dec_"),
        ("glossary", "term_"),
    ] {
        let output = run(&[kind, "schema"]);
        assert!(
            output.status.success(),
            "{kind} schema failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        let stdout = String::from_utf8(output.stdout).expect("utf8 stdout");
        assert!(
            stdout.contains(id_prefix),
            "{kind} schema output missing a {id_prefix} example id:\n{stdout}"
        );
        assert!(
            stdout.contains("Property legend:"),
            "{kind} schema output missing the property legend"
        );
    }
}
