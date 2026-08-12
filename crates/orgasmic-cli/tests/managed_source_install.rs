#![cfg(unix)]

use std::os::unix::fs::{MetadataExt, PermissionsExt};

#[path = "common/env_isolation.rs"]
mod env_isolation;
use env_isolation::orgasmic_command;

#[test]
fn internal_source_publisher_installs_a_fresh_inode_at_the_managed_path() {
    let tmp = tempfile::tempdir().unwrap();
    let home = tmp.path().join("home");
    let source = tmp.path().join("source-orgasmic");
    std::fs::copy(env!("CARGO_BIN_EXE_orgasmic"), &source).unwrap();
    std::fs::set_permissions(&source, std::fs::Permissions::from_mode(0o755)).unwrap();
    let source_inode = std::fs::metadata(&source).unwrap().ino();

    let output = orgasmic_command()
        .env("ORGASMIC_HOME", &home)
        .args(["__install-managed-source", source.to_str().unwrap()])
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "stdout={}\nstderr={}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );

    let installed = home.join("bin/orgasmic");
    let installed_meta = std::fs::symlink_metadata(&installed).unwrap();
    assert!(installed_meta.is_file());
    assert!(!installed_meta.file_type().is_symlink());
    assert_ne!(
        installed_meta.ino(),
        source_inode,
        "publication must use a fresh inode"
    );
    assert_eq!(
        std::fs::read(&installed).unwrap(),
        std::fs::read(&source).unwrap()
    );
}

#[test]
fn contributor_installer_delegates_source_publication_to_the_rust_guard() {
    let installer =
        std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../scripts/install.sh");
    let script = std::fs::read_to_string(installer).unwrap();
    assert!(
        script.contains("publish_source_binary \"$source_bin\" \"$ORGASMIC_HOME/bin/orgasmic\"")
    );
    assert!(script.contains("__install-managed-source"));
    assert!(script.contains("publisher=\"$dest\""));
    assert!(script.contains("staged_source=\"${dest}.source.$$\""));
    assert!(script.contains("\"$publisher\" __install-managed-source \"$staged_source\""));
    assert!(!script.contains("\"$source\" __install-managed-source \"$source\""));
    assert!(
        !script.contains("install_managed_binary \"$source_bin\" \"$ORGASMIC_HOME/bin/orgasmic\"")
    );
}
