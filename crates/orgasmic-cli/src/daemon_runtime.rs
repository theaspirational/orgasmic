//! Temporary daemon runtime overrides for testing a local source checkout.
//!
//! Bundle installs remain the update authority. This module only lets the
//! local daemon service run a managed copy of a built checkout binary until
//! bundle update clears the override. The service never executes from the
//! checkout itself: on macOS that would make every ad-hoc-signed rebuild a new
//! TCC client and would put daemon launch behind Documents-folder approval.

use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{bail, Context, Result};
use chrono::Utc;
use serde::{Deserialize, Serialize};

use crate::home::Home;
use crate::managed_binary;
use crate::path_env;

const OVERRIDE_FILE: &str = "daemon-runtime-override.json";
const LOCAL_SOURCE_KIND: &str = "local_source";

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub(crate) struct DaemonRuntimeOverride {
    pub kind: String,
    pub source_checkout: PathBuf,
    pub binary: PathBuf,
    pub build_profile: String,
    pub set_at: String,
}

impl DaemonRuntimeOverride {
    pub(crate) fn description(&self) -> String {
        format!(
            "{} binary={} checkout={}",
            self.kind,
            self.binary.display(),
            self.source_checkout.display()
        )
    }
}

pub(crate) fn override_path(home: &Home) -> PathBuf {
    home.state().join(OVERRIDE_FILE)
}

pub(crate) fn read(home: &Home) -> Result<Option<DaemonRuntimeOverride>> {
    let path = override_path(home);
    if !path.exists() {
        return Ok(None);
    }
    let raw = std::fs::read_to_string(&path).with_context(|| format!("read {}", path.display()))?;
    let value = serde_json::from_str(&raw).with_context(|| format!("parse {}", path.display()))?;
    Ok(Some(value))
}

pub(crate) fn active(home: &Home) -> Result<Option<DaemonRuntimeOverride>> {
    let Some(value) = read(home)? else {
        return Ok(None);
    };
    validate(home, &value).with_context(|| {
        format!(
            "invalid daemon runtime override at {}; run `orgasmic daemon restart --clear-runtime-override` to return to the installed runtime",
            override_path(home).display()
        )
    })?;
    Ok(Some(value))
}

pub(crate) fn set_local_source(
    home: &Home,
    checkout: &Path,
    build: bool,
) -> Result<DaemonRuntimeOverride> {
    home.ensure().context("prepare ORGASMIC_HOME")?;
    let source_checkout = checkout
        .canonicalize()
        .with_context(|| format!("resolve source checkout {}", checkout.display()))?;
    if !source_checkout.join("Cargo.toml").is_file() {
        bail!(
            "source checkout {} does not contain Cargo.toml",
            source_checkout.display()
        );
    }

    if build {
        let status = Command::new("cargo")
            .args(["build", "--release"])
            .current_dir(&source_checkout)
            .status()
            .with_context(|| format!("build release binary in {}", source_checkout.display()))?;
        if !status.success() {
            bail!("cargo build --release failed with {status}");
        }
    }

    let build_binary = path_env::resolve_source_binary(&source_checkout).ok_or_else(|| {
        anyhow::anyhow!(
            "no built orgasmic binary under {} (looked in target/release and target/<triple>/release)",
            source_checkout.join("target").display()
        )
    })?;
    validate_executable(&build_binary)?;
    let build_binary = build_binary
        .canonicalize()
        .with_context(|| format!("resolve built binary {}", build_binary.display()))?;
    let binary = publish_source_override(home, &build_binary)?;

    let value = DaemonRuntimeOverride {
        kind: LOCAL_SOURCE_KIND.to_string(),
        source_checkout,
        binary,
        build_profile: "release".to_string(),
        set_at: Utc::now().to_rfc3339(),
    };
    write(home, &value)?;
    Ok(value)
}

#[cfg(unix)]
fn publish_source_override(home: &Home, source: &Path) -> Result<PathBuf> {
    let installed = managed_binary::install_source_daemon_override(
        home,
        source,
        managed_binary::IdentityGuard::Enforce,
    )?;
    installed.path.canonicalize().with_context(|| {
        format!(
            "resolve published daemon override {}",
            installed.path.display()
        )
    })
}

#[cfg(not(unix))]
fn publish_source_override(_home: &Home, source: &Path) -> Result<PathBuf> {
    // Windows cannot replace a running executable. Keep its existing direct
    // override behavior; the managed copy is specifically the Unix/macOS TCC
    // boundary and can be atomically replaced while the old inode is running.
    source
        .canonicalize()
        .with_context(|| format!("resolve daemon override {}", source.display()))
}

pub(crate) fn clear(home: &Home) -> Result<bool> {
    let path = override_path(home);
    match std::fs::remove_file(&path) {
        Ok(()) => Ok(true),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(err) => Err(err).with_context(|| format!("remove {}", path.display())),
    }
}

fn write(home: &Home, value: &DaemonRuntimeOverride) -> Result<()> {
    let raw = serde_json::to_string_pretty(value).context("serialize daemon runtime override")?;
    write_raw(home, format!("{raw}\n").as_bytes())
}

fn write_raw(home: &Home, raw: &[u8]) -> Result<()> {
    let path = override_path(home);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).with_context(|| format!("create {}", parent.display()))?;
    }
    let tmp = path.with_extension("json.tmp");
    std::fs::write(&tmp, raw).with_context(|| format!("write {}", tmp.display()))?;
    std::fs::rename(&tmp, &path)
        .with_context(|| format!("replace {} with {}", path.display(), tmp.display()))
}

fn validate(home: &Home, value: &DaemonRuntimeOverride) -> Result<()> {
    if value.kind != LOCAL_SOURCE_KIND {
        bail!("unsupported daemon runtime override kind: {}", value.kind);
    }
    // The checkout is provenance, not a runtime dependency. `set_local_source`
    // validates it before building; subsequent service starts must not traverse
    // a protected Documents checkout merely to launch the already-published
    // managed binary.
    validate_executable(&value.binary)?;
    validate_managed_override_path(home, value)?;
    Ok(())
}

#[cfg(unix)]
fn validate_managed_override_path(home: &Home, value: &DaemonRuntimeOverride) -> Result<()> {
    let expected = managed_binary::source_daemon_override_path(home)
        .canonicalize()
        .with_context(|| {
            format!(
                "resolve managed daemon override {}",
                managed_binary::source_daemon_override_path(home).display()
            )
        })?;
    let actual = value
        .binary
        .canonicalize()
        .with_context(|| format!("resolve daemon override {}", value.binary.display()))?;
    if actual != expected {
        bail!(
            "daemon override points outside its managed runtime path: {} (expected {}); rerun `orgasmic daemon restart --from-source {}`",
            actual.display(),
            expected.display(),
            value.source_checkout.display()
        );
    }
    Ok(())
}

#[cfg(not(unix))]
fn validate_managed_override_path(_home: &Home, _value: &DaemonRuntimeOverride) -> Result<()> {
    Ok(())
}

/// State needed to put the previous runtime selection back if a candidate
/// daemon fails to become ready. The backup exists only when a prior source
/// override occupied the stable path that a new candidate will replace.
pub(crate) struct RuntimeSnapshot {
    previous_raw: Option<Vec<u8>>,
    binary_backup: Option<PathBuf>,
}

impl RuntimeSnapshot {
    pub(crate) fn capture(home: &Home) -> Result<Self> {
        let previous_raw = match std::fs::read(override_path(home)) {
            Ok(raw) => Some(raw),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
            Err(error) => return Err(error).context("read daemon runtime override snapshot"),
        };
        // A malformed record must still be clearable. Parse only to discover
        // whether the stable source binary itself needs a rollback copy; the
        // raw bytes remain the restoration authority.
        let previous = previous_raw
            .as_deref()
            .and_then(|raw| serde_json::from_slice::<DaemonRuntimeOverride>(raw).ok());
        let managed = managed_binary::source_daemon_override_path(home);
        let binary_backup = previous
            .as_ref()
            .filter(|runtime| paths_resolve_equal(&runtime.binary, &managed))
            .map(|runtime| -> Result<PathBuf> {
                let backup = home.bin().join(format!(
                    ".orgasmic-daemon-source.rollback.{}",
                    std::process::id()
                ));
                let _ = std::fs::remove_file(&backup);
                std::fs::copy(&runtime.binary, &backup).with_context(|| {
                    format!(
                        "snapshot daemon override {} as {}",
                        runtime.binary.display(),
                        backup.display()
                    )
                })?;
                Ok(backup)
            })
            .transpose()?;
        Ok(Self {
            previous_raw,
            binary_backup,
        })
    }

    pub(crate) fn restore(mut self, home: &Home) -> Result<()> {
        let result = (|| -> Result<()> {
            if let Some(backup) = &self.binary_backup {
                restore_override_binary(home, backup)?;
            } else if self.previous_raw.is_none() {
                remove_new_override_binary(home)?;
            }
            match &self.previous_raw {
                Some(previous) => write_raw(home, previous),
                None => clear(home).map(|_| ()),
            }
        })();
        if result.is_ok() {
            self.remove_backup();
        }
        result
    }

    pub(crate) fn discard(mut self) {
        self.remove_backup();
    }

    fn remove_backup(&mut self) {
        if let Some(backup) = self.binary_backup.take() {
            let _ = std::fs::remove_file(backup);
        }
    }
}

fn paths_resolve_equal(left: &Path, right: &Path) -> bool {
    match (left.canonicalize(), right.canonicalize()) {
        (Ok(left), Ok(right)) => left == right,
        _ => false,
    }
}

#[cfg(unix)]
fn restore_override_binary(home: &Home, backup: &Path) -> Result<()> {
    managed_binary::restore_source_daemon_override(home, backup).map(|_| ())
}

#[cfg(unix)]
fn remove_new_override_binary(home: &Home) -> Result<()> {
    let path = managed_binary::source_daemon_override_path(home);
    match std::fs::remove_file(&path) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error).with_context(|| format!("remove {}", path.display())),
    }
}

#[cfg(not(unix))]
fn restore_override_binary(_home: &Home, _backup: &Path) -> Result<()> {
    bail!("source daemon override binary rollback is unsupported on this platform")
}

#[cfg(not(unix))]
fn remove_new_override_binary(_home: &Home) -> Result<()> {
    Ok(())
}

#[cfg(unix)]
fn validate_executable(path: &Path) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;

    let meta = std::fs::metadata(path).with_context(|| format!("stat {}", path.display()))?;
    if !meta.is_file() {
        bail!("daemon override binary is not a file: {}", path.display());
    }
    if meta.permissions().mode() & 0o111 == 0 {
        bail!(
            "daemon override binary is not executable: {}",
            path.display()
        );
    }
    Ok(())
}

#[cfg(not(unix))]
fn validate_executable(path: &Path) -> Result<()> {
    if !path.is_file() {
        bail!("daemon override binary is not a file: {}", path.display());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_executable(path: &Path) {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).unwrap();
        }
        std::fs::write(path, b"#!/bin/sh\n").unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
    }

    #[test]
    fn local_source_override_roundtrips_and_clears() {
        let tmp = tempfile::tempdir().unwrap();
        let home = Home::at(tmp.path().join("home"));
        let checkout = tmp.path().join("checkout");
        std::fs::create_dir_all(&checkout).unwrap();
        std::fs::write(checkout.join("Cargo.toml"), "[workspace]\n").unwrap();
        let binary = checkout.join("target/aarch64-apple-darwin/release/orgasmic");
        make_executable(&binary);

        let stored = set_local_source(&home, &checkout, false).unwrap();
        assert_eq!(stored.kind, LOCAL_SOURCE_KIND);
        assert_eq!(stored.source_checkout, checkout.canonicalize().unwrap());
        assert_eq!(
            stored.binary,
            managed_binary::source_daemon_override_path(&home)
                .canonicalize()
                .unwrap()
        );
        assert_eq!(std::fs::read(&stored.binary).unwrap(), b"#!/bin/sh\n");
        assert_ne!(stored.binary, binary.canonicalize().unwrap());

        let active = active(&home).unwrap().unwrap();
        assert_eq!(active, stored);
        assert!(clear(&home).unwrap());
        assert!(read(&home).unwrap().is_none());
    }

    #[test]
    fn local_source_override_requires_built_binary() {
        let tmp = tempfile::tempdir().unwrap();
        let home = Home::at(tmp.path().join("home"));
        let checkout = tmp.path().join("checkout");
        std::fs::create_dir_all(&checkout).unwrap();
        std::fs::write(checkout.join("Cargo.toml"), "[workspace]\n").unwrap();

        let err = set_local_source(&home, &checkout, false)
            .expect_err("unbuilt source checkout should be rejected")
            .to_string();
        assert!(err.contains("no built orgasmic binary"), "{err}");
    }

    #[test]
    fn active_override_does_not_reopen_its_source_checkout() {
        let tmp = tempfile::tempdir().unwrap();
        let home = Home::at(tmp.path().join("home"));
        let checkout = tmp.path().join("checkout");
        std::fs::create_dir_all(&checkout).unwrap();
        std::fs::write(checkout.join("Cargo.toml"), "[workspace]\n").unwrap();
        let binary = checkout.join("target/release/orgasmic");
        make_executable(&binary);

        let stored = set_local_source(&home, &checkout, false).unwrap();
        std::fs::remove_dir_all(&checkout).unwrap();

        assert_eq!(active(&home).unwrap(), Some(stored));
    }

    #[test]
    fn restoring_snapshot_reinstates_previous_override_bytes_and_metadata() {
        let tmp = tempfile::tempdir().unwrap();
        let home = Home::at(tmp.path().join("home"));
        let checkout = tmp.path().join("checkout");
        std::fs::create_dir_all(&checkout).unwrap();
        std::fs::write(checkout.join("Cargo.toml"), "[workspace]\n").unwrap();
        let binary = checkout.join("target/release/orgasmic");
        make_executable(&binary);

        let previous = set_local_source(&home, &checkout, false).unwrap();
        let snapshot = RuntimeSnapshot::capture(&home).unwrap();
        std::fs::write(&binary, b"#!/bin/sh\necho candidate\n").unwrap();
        let candidate = set_local_source(&home, &checkout, false).unwrap();
        assert_ne!(std::fs::read(&candidate.binary).unwrap(), b"#!/bin/sh\n");

        snapshot.restore(&home).unwrap();

        assert_eq!(read(&home).unwrap().unwrap(), previous);
        assert_eq!(
            std::fs::read(managed_binary::source_daemon_override_path(&home)).unwrap(),
            b"#!/bin/sh\n"
        );
    }

    #[test]
    fn snapshot_does_not_prevent_clearing_a_malformed_override() {
        let tmp = tempfile::tempdir().unwrap();
        let home = Home::at(tmp.path().join("home"));
        home.ensure().unwrap();
        let malformed = b"{not valid json\n";
        std::fs::write(override_path(&home), malformed).unwrap();

        let snapshot = RuntimeSnapshot::capture(&home).unwrap();
        assert!(clear(&home).unwrap());
        assert!(!override_path(&home).exists());

        snapshot.restore(&home).unwrap();
        assert_eq!(std::fs::read(override_path(&home)).unwrap(), malformed);
    }

    #[test]
    fn restoring_an_empty_snapshot_removes_an_unaccepted_candidate() {
        let tmp = tempfile::tempdir().unwrap();
        let home = Home::at(tmp.path().join("home"));
        let checkout = tmp.path().join("checkout");
        std::fs::create_dir_all(&checkout).unwrap();
        std::fs::write(checkout.join("Cargo.toml"), "[workspace]\n").unwrap();
        let binary = checkout.join("target/release/orgasmic");
        make_executable(&binary);

        let snapshot = RuntimeSnapshot::capture(&home).unwrap();
        set_local_source(&home, &checkout, false).unwrap();
        assert!(managed_binary::source_daemon_override_path(&home).is_file());

        snapshot.restore(&home).unwrap();

        assert!(!override_path(&home).exists());
        #[cfg(unix)]
        assert!(!managed_binary::source_daemon_override_path(&home).exists());
    }
}
