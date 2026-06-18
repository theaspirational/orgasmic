//! `orgasmic doctor --fix-id-collisions` repair path (TASK-QRD3Y).

use std::collections::HashMap;
use std::path::Path;

use orgasmic_core::{
    collect_identity_occurrences, duplicate_id_groups, mint_node_id,
    repair_id_collisions_with_incoming, NodeIdClass,
};

fn write(path: &Path, contents: &str) {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).unwrap();
    }
    std::fs::write(path, contents).unwrap();
}

#[test]
fn repair_re_mints_decision_duplicate_and_rerun_is_noop() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path();
    let dup = mint_node_id(NodeIdClass::Decision);
    let keep_path = root.join(".orgasmic/decisions.org");
    let incoming_path = root.join(".orgasmic/tasks/backlog.org");
    write(
        &keep_path,
        &format!(
            "#+title: decisions\n\n* {dup} Keep\n:PROPERTIES:\n:ID: {dup}\n:END:\n** Decision\nKeep side.\n"
        ),
    );
    write(
        &incoming_path,
        &format!(
            "#+title: backlog\n\n* BACKLOG {dup} Incoming\n:PROPERTIES:\n:ID: {dup}\n:DEPENDS_ON: {dup}\n:END:\n"
        ),
    );
    let mut incoming = HashMap::new();
    incoming.insert(dup.clone(), incoming_path.clone());
    let mappings = repair_id_collisions_with_incoming(root, &incoming).unwrap();
    assert_eq!(mappings.len(), 1);
    let new_id = mappings[0].new_id.clone();
    let keep_text = std::fs::read_to_string(&keep_path).unwrap();
    let incoming_text = std::fs::read_to_string(&incoming_path).unwrap();
    assert!(keep_text.contains(&dup));
    assert!(!incoming_text.contains(&format!(":ID: {dup}")));
    assert!(incoming_text.contains(&new_id));
    assert!(incoming_text.contains(&format!(":DEPENDS_ON: {new_id}")));
    assert!(duplicate_id_groups(&collect_identity_occurrences(root)).is_empty());
}

#[test]
fn ambiguous_attribution_refuses_without_incoming_path() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path();
    let dup = mint_node_id(NodeIdClass::Task);
    write(
        &root.join(".orgasmic/tasks/backlog.org"),
        &format!(
            "#+title: backlog\n\n* BACKLOG {dup} A\n:PROPERTIES:\n:ID: {dup}\n:END:\n\n* BACKLOG {dup} B\n:PROPERTIES:\n:ID: {dup}\n:END:\n"
        ),
    );
    let err = repair_id_collisions_with_incoming(root, &HashMap::new()).unwrap_err();
    assert!(err.to_string().contains("ambiguous") || err.to_string().contains("incoming"));
}
