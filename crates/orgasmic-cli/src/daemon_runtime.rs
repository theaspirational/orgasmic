//! Temporary daemon runtime overrides for testing a local source checkout.
//!
//! Bundle installs remain the update authority. This module only lets the
//! local daemon service point at a built checkout binary until bundle update
//! clears the override.

use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{bail, Context, Result};
use chrono::Utc;
use serde::{Deserialize, Serialize};

use crate::home::Home;
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
    validate(&value).with_context(|| {
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

    let binary = path_env::resolve_source_binary(&source_checkout).ok_or_else(|| {
        anyhow::anyhow!(
            "no built orgasmic binary under {} (looked in target/release and target/<triple>/release)",
            source_checkout.join("target").display()
        )
    })?;
    validate_executable(&binary)?;
    let binary = binary
        .canonicalize()
        .with_context(|| format!("resolve built binary {}", binary.display()))?;

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

pub(crate) fn clear(home: &Home) -> Result<bool> {
    let path = override_path(home);
    match std::fs::remove_file(&path) {
        Ok(()) => Ok(true),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(err) => Err(err).with_context(|| format!("remove {}", path.display())),
    }
}

fn write(home: &Home, value: &DaemonRuntimeOverride) -> Result<()> {
    let path = override_path(home);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).with_context(|| format!("create {}", parent.display()))?;
    }
    let tmp = path.with_extension("json.tmp");
    let raw = serde_json::to_string_pretty(value).context("serialize daemon runtime override")?;
    std::fs::write(&tmp, format!("{raw}\n")).with_context(|| format!("write {}", tmp.display()))?;
    std::fs::rename(&tmp, &path)
        .with_context(|| format!("replace {} with {}", path.display(), tmp.display()))?;
    Ok(())
}

fn validate(value: &DaemonRuntimeOverride) -> Result<()> {
    if value.kind != LOCAL_SOURCE_KIND {
        bail!("unsupported daemon runtime override kind: {}", value.kind);
    }
    if !value.source_checkout.is_dir() {
        bail!(
            "source checkout does not exist: {}",
            value.source_checkout.display()
        );
    }
    validate_executable(&value.binary)
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
        assert_eq!(stored.binary, binary.canonicalize().unwrap());

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
}
