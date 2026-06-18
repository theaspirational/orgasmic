// orgasmic:arch_WZFAX, dec_XSV21
//! Install metadata for separating a source checkout from an installed runtime.

use std::path::PathBuf;

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

use crate::home::Home;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum InstallMode {
    Bundle,
    Source,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct InstallState {
    pub mode: InstallMode,
    #[serde(default)]
    pub channel: Option<String>,
    #[serde(default)]
    pub version: Option<String>,
    #[serde(default)]
    pub target: Option<String>,
    #[serde(default)]
    pub manifest_url: Option<String>,
    #[serde(default)]
    pub runtime_dir: Option<PathBuf>,
    #[serde(default)]
    pub source_checkout: Option<PathBuf>,
}

impl InstallState {
    pub fn source(checkout: PathBuf) -> Self {
        Self {
            mode: InstallMode::Source,
            channel: None,
            version: None,
            target: None,
            manifest_url: None,
            runtime_dir: None,
            source_checkout: Some(checkout),
        }
    }
}

pub fn read(home: &Home) -> Result<Option<InstallState>> {
    let path = home.install_json();
    if !path.exists() {
        return Ok(None);
    }
    let raw = std::fs::read_to_string(&path).with_context(|| format!("read {}", path.display()))?;
    let state = serde_json::from_str(&raw).with_context(|| format!("parse {}", path.display()))?;
    Ok(Some(state))
}

pub fn write(home: &Home, state: &InstallState) -> Result<()> {
    let path = home.install_json();
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).with_context(|| format!("create {}", parent.display()))?;
    }
    let tmp = path.with_extension("json.tmp");
    let raw = serde_json::to_string_pretty(state).context("serialize install.json")?;
    std::fs::write(&tmp, format!("{raw}\n")).with_context(|| format!("write {}", tmp.display()))?;
    std::fs::rename(&tmp, &path)
        .with_context(|| format!("replace {} with {}", path.display(), tmp.display()))?;
    Ok(())
}
