//! P2 manifests and durable per-ledger activation. Execution stays outside core.
use crate::{NodeTypeDescriptor, OrgFile};
use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

pub const PLUGIN_API: u32 = 1;
pub const PLUGIN_SDK_VERSION: &str = "1.0.0";

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PluginManifest {
    pub id: String,
    pub version: String,
    pub schema: u32,
    pub schema_accepts: BTreeSet<u32>,
    pub capabilities: BTreeSet<String>,
    pub commands: Vec<String>,
    #[serde(default)]
    pub ui: Option<String>,
    #[serde(default)]
    pub sdk: Option<String>,
    pub node_type: Option<NodeTypeDescriptor>,
}

pub fn validate_id(id: &str) -> Result<()> {
    if id.is_empty()
        || id.len() > 64
        || id.starts_with('-')
        || id.ends_with('-')
        || !id
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
    {
        bail!("invalid plugin id; use lowercase letters, digits and hyphens");
    }
    Ok(())
}

impl PluginManifest {
    pub fn parse(source: &str, display: &str) -> Result<Self> {
        anyhow::ensure!(
            source.len() <= 128 * 1024,
            "plugin manifest exceeds 128 KiB"
        );
        let file = OrgFile::parse(source, display)?;
        anyhow::ensure!(
            file.headings.len() == 1 && file.headings[0].title.eq_ignore_ascii_case("Plugin"),
            "manifest must contain one top-level Plugin heading"
        );
        let root = &file.headings[0];
        for heading in std::iter::once(root).chain(root.sections.iter()) {
            let mut keys = BTreeSet::new();
            for property in heading.property_entries() {
                anyhow::ensure!(
                    keys.insert(&property.key),
                    "duplicate manifest property {}",
                    property.key
                );
            }
        }
        let required = |key| {
            root.property(key)
                .map(str::trim)
                .filter(|v| !v.is_empty())
                .with_context(|| format!("{display}: missing :{key}:"))
        };
        let words = |key| {
            root.property(key)
                .unwrap_or_default()
                .split_whitespace()
                .map(str::to_string)
                .collect::<Vec<_>>()
        };
        let id = required("ID")?.to_string();
        validate_id(&id)?;
        anyhow::ensure!(
            required("PLUGIN_API")?.parse::<u32>()? == PLUGIN_API,
            "unsupported PLUGIN_API"
        );
        let version = required("VERSION")?.to_string();
        anyhow::ensure!(
            version.split('.').count() == 3 && version.split('.').all(|n| n.parse::<u32>().is_ok()),
            "VERSION must be major.minor.patch"
        );
        let schema = required("SCHEMA")?.parse::<u32>()?;
        anyhow::ensure!(schema > 0, "SCHEMA must be positive");
        let mut schema_accepts = words("SCHEMA_ACCEPTS")
            .iter()
            .map(|v| v.parse::<u32>())
            .collect::<std::result::Result<BTreeSet<_>, _>>()?;
        anyhow::ensure!(
            !schema_accepts.contains(&0),
            "SCHEMA_ACCEPTS must be positive"
        );
        schema_accepts.insert(schema);
        anyhow::ensure!(
            root.property("SIDECAR").is_none_or(|v| v.trim().is_empty()),
            "SIDECAR is not supported"
        );
        for service in words("REQUIRES") {
            anyhow::ensure!(
                service == "core.nodes@1",
                "required service {service} is unavailable"
            );
        }
        let ui = root
            .property("UI")
            .filter(|v| !v.trim().is_empty())
            .map(|v| v.trim().to_string());
        let sdk = root
            .property("SDK")
            .filter(|v| !v.trim().is_empty())
            .map(|v| v.trim().to_string());
        if let Some(ui) = &ui {
            anyhow::ensure!(ui == "ui/index.js", "UI must be ui/index.js");
            let requirement =
                semver::VersionReq::parse(sdk.as_deref().context("UI requires SDK")?)?;
            anyhow::ensure!(
                requirement.matches(&semver::Version::parse(PLUGIN_SDK_VERSION)?),
                "unsupported SDK version"
            );
        } else {
            anyhow::ensure!(sdk.is_none(), "SDK requires UI");
        }
        let mut capabilities: BTreeSet<_> = words("CAPABILITIES").into_iter().collect();
        // A declarative install becoming executable UI must require re-approval.
        // This is a trust acknowledgement, not a same-origin authority boundary.
        if ui.is_some() {
            capabilities.insert("ui.execute".into());
        }
        for capability in &capabilities {
            anyhow::ensure!(
                ["nodes.read", "nodes.write", "ui.execute"].contains(&capability.as_str()),
                "unknown or unavailable capability {capability}"
            );
        }
        let commands = words("COMMANDS");
        let mut unique = BTreeSet::new();
        for command in &commands {
            anyhow::ensure!(
                !command.is_empty()
                    && command
                        .bytes()
                        .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_')),
                "invalid command name"
            );
            anyhow::ensure!(unique.insert(command), "duplicate command {command}");
        }
        let node_type = if root
            .sections
            .iter()
            .any(|h| h.property("COLLECTION").is_some())
        {
            Some(NodeTypeDescriptor::parse(source, display)?)
        } else {
            None
        };
        Ok(Self {
            id,
            version,
            schema,
            schema_accepts,
            capabilities,
            commands,
            ui,
            sdk,
            node_type,
        })
    }

    pub fn read_dir(dir: &Path) -> Result<Self> {
        anyhow::ensure!(
            !std::fs::symlink_metadata(dir)?.file_type().is_symlink(),
            "plugin root must not be a symlink"
        );
        let path = dir.join("plugin.org");
        anyhow::ensure!(
            std::fs::metadata(&path)?.len() <= 128 * 1024,
            "plugin manifest exceeds 128 KiB"
        );
        anyhow::ensure!(
            !std::fs::symlink_metadata(&path)?.file_type().is_symlink(),
            "manifest must not be a symlink"
        );
        let manifest = Self::parse(
            &std::fs::read_to_string(&path)?,
            &path.display().to_string(),
        )?;
        for command in &manifest.commands {
            manifest.command_path(dir, command)?;
        }
        if manifest.ui.is_some() {
            manifest.ui_asset_path(dir, "index.js")?;
        }
        Ok(manifest)
    }

    pub fn ui_asset_path(&self, dir: &Path, asset: &str) -> Result<std::path::PathBuf> {
        anyhow::ensure!(self.ui.is_some(), "plugin has no UI");
        anyhow::ensure!(
            !asset.is_empty()
                && asset.split('/').all(|part| {
                    !part.is_empty() && part != "." && part != ".." && !part.contains('\\')
                }),
            "invalid UI asset path"
        );
        anyhow::ensure!(
            !dir.symlink_metadata()?.file_type().is_symlink(),
            "plugin root must not be a symlink"
        );
        let root = dir.canonicalize()?;
        let ui = root.join("ui").canonicalize()?;
        let path = ui.join(asset).canonicalize()?;
        anyhow::ensure!(
            ui.starts_with(&root) && path.starts_with(&ui) && path.is_file(),
            "UI asset escapes plugin folder"
        );
        Ok(path)
    }

    pub fn command_path(&self, dir: &Path, command: &str) -> Result<std::path::PathBuf> {
        anyhow::ensure!(
            self.commands.iter().any(|c| c == command),
            "undeclared plugin command"
        );
        let root = dir.canonicalize()?;
        let path = dir.join("bin").join(command).canonicalize()?;
        anyhow::ensure!(
            path.starts_with(&root) && path.is_file(),
            "command must stay inside plugin directory"
        );
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            anyhow::ensure!(
                path.metadata()?.permissions().mode() & 0o111 != 0,
                "command is not executable"
            );
        }
        Ok(path)
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct PluginActivation {
    pub enabled: bool,
    pub approved_capabilities: BTreeSet<String>,
}
pub type PluginActivations = BTreeMap<String, PluginActivation>;

pub fn read_json<T: serde::de::DeserializeOwned + Default>(path: &Path) -> Result<T> {
    match std::fs::read(path) {
        Ok(bytes) => {
            serde_json::from_slice(&bytes).with_context(|| format!("parse {}", path.display()))
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(T::default()),
        Err(e) => Err(e.into()),
    }
}

/// Caller serializes changes. Unique staging names also survive interrupted writes.
pub fn write_json(path: &Path, value: &impl Serialize) -> Result<()> {
    let parent = path.parent().context("missing parent")?;
    std::fs::create_dir_all(parent)?;
    let temporary = parent.join(format!(".plugin-{}.tmp", uuid::Uuid::new_v4()));
    let result = (|| {
        use std::io::Write;
        let mut file = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&temporary)?;
        file.write_all(&serde_json::to_vec_pretty(value)?)?;
        file.sync_all()?;
        std::fs::rename(&temporary, path)?;
        Ok(())
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(&temporary);
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    const SOURCE: &str = "* Plugin\n:PROPERTIES:\n:ID: meetings\n:VERSION: 0.1.0\n:PLUGIN_API: 1\n:SCHEMA: 2\n:SCHEMA_ACCEPTS: 1\n:REQUIRES: core.nodes@1\n:CAPABILITIES: nodes.read nodes.write\n:END:\n** Node type meetings\n:PROPERTIES:\n:COLLECTION: meetings\n:ID_PREFIX: MEET-\n:LABEL: Meeting\n:LABEL_PLURAL: Meetings\n:END:\n";
    #[test]
    fn ui_requires_supported_sdk_and_executable_trust_approval() {
        let source = SOURCE.replace(":SCHEMA: 2", ":UI: ui/index.js\n:SDK: ^1.0\n:SCHEMA: 2");
        let manifest = PluginManifest::parse(&source, "UI").unwrap();
        assert!(manifest.capabilities.contains("ui.execute"));
        for invalid in [
            source.replace("^1.0", "^2.0"),
            source.replace("ui/index.js", "../index.js"),
            source.replace(":SDK: ^1.0\n", ""),
        ] {
            assert!(PluginManifest::parse(&invalid, "UI").is_err());
        }
    }

    #[test]
    fn author_skill_manifest_is_valid() {
        let skill = include_str!("../../../shipped/skills/orgasmic-plugin-author/SKILL.md");
        let source = skill
            .split_once("```org\n")
            .unwrap()
            .1
            .split_once("```")
            .unwrap()
            .0;
        let manifest = PluginManifest::parse(source, "author skill example").unwrap();
        assert_eq!(manifest.commands, ["create"]);
        assert_eq!(manifest.node_type.unwrap().collection, "meetings");
    }

    #[test]
    fn manifest_validates_scope_and_paths() {
        let m = PluginManifest::parse(SOURCE, "plugin.org").unwrap();
        assert_eq!(m.schema_accepts, BTreeSet::from([1, 2]));
        assert_eq!(m.node_type.unwrap().collection, "meetings");
        for (before, after) in [
            ("meetings\n:VERSION", "../escape\n:VERSION"),
            ("nodes.write", "host.root"),
            ("core.nodes@1", "core.links@1"),
            (":SCHEMA: 2", ":SCHEMA: 0"),
            (":VERSION: 0.1.0", ":VERSION: bad"),
        ] {
            assert!(PluginManifest::parse(&SOURCE.replace(before, after), "plugin.org").is_err());
        }
    }
}
