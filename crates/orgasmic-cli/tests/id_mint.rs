//! Daemon-free `orgasmic id mint` and minted-format smoke checks.

use std::process::Command;

use orgasmic_core::{is_minted_stem, mint_node_id, NodeIdClass, CROCKFORD};

#[test]
fn id_mint_prints_conforming_task_id_without_daemon() {
    let output = Command::new(env!("CARGO_BIN_EXE_orgasmic"))
        .args(["id", "mint", "--class", "task"])
        .output()
        .expect("run orgasmic id mint");
    assert!(
        output.status.success(),
        "id mint failed: status={:?} stderr={}",
        output.status,
        String::from_utf8_lossy(&output.stderr)
    );
    let id = String::from_utf8(output.stdout)
        .expect("utf8 stdout")
        .trim()
        .to_string();
    assert!(id.starts_with("TASK-"));
    let stem = id.strip_prefix("TASK-").expect("task prefix");
    assert!(is_minted_stem(stem));
    assert!(stem.chars().any(|c| c.is_ascii_alphabetic()));
}

#[test]
fn id_mint_all_classes_use_crockford_alphabet() {
    for (class, prefix) in [
        (NodeIdClass::Decision, "dec_"),
        (NodeIdClass::Architecture, "arch_"),
        (NodeIdClass::Term, "term_"),
        (NodeIdClass::Artifact, "ART-"),
    ] {
        let output = Command::new(env!("CARGO_BIN_EXE_orgasmic"))
            .args([
                "id",
                "mint",
                "--class",
                match class {
                    NodeIdClass::Decision => "decision",
                    NodeIdClass::Architecture => "architecture",
                    NodeIdClass::Term => "term",
                    NodeIdClass::Artifact => "artifact",
                    NodeIdClass::Task => unreachable!(),
                },
            ])
            .output()
            .expect("run orgasmic id mint");
        assert!(output.status.success(), "{prefix} mint failed");
        let id = String::from_utf8(output.stdout).unwrap().trim().to_string();
        assert!(id.starts_with(prefix));
        let stem = id.strip_prefix(prefix).unwrap();
        assert!(stem.chars().all(|c| CROCKFORD.contains(c)));
    }
}

#[test]
fn core_mint_never_emits_legacy_numeric_task_id() {
    for _ in 0..1000 {
        let id = mint_node_id(NodeIdClass::Task);
        let stem = id.strip_prefix("TASK-").unwrap();
        assert!(
            !stem.chars().all(|c| c.is_ascii_digit()),
            "all-digit task mint: {id}"
        );
    }
}
