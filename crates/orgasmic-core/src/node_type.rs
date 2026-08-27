//! Shipped node-type descriptors and descriptor-driven node creation.

use std::collections::{BTreeMap, BTreeSet};
use std::io::ErrorKind;
use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};

use crate::id::random_stem;
use crate::OrgFile;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NodeTypeDescriptor {
    pub collection: String,
    pub id_prefix: String,
    pub label: String,
    pub label_plural: String,
    pub required_properties: Vec<String>,
    pub states: Vec<String>,
    pub transitions: BTreeMap<String, Vec<String>>,
    pub regenerate_prompt: Option<String>,
    pub reserved_files: Vec<String>,
}

impl NodeTypeDescriptor {
    pub fn parse(source: &str, display: &str) -> Result<Self> {
        let file = OrgFile::parse(source, display).context("parse node-type descriptor")?;
        if file.headings.len() != 1 {
            bail!("{display}: node-type descriptor must contain exactly one top-level heading");
        }
        let heading = &file.headings[0];
        let required = |key: &str| -> Result<String> {
            heading
                .property(key)
                .map(str::trim)
                .filter(|value| !value.is_empty())
                .map(str::to_string)
                .ok_or_else(|| anyhow::anyhow!("{display}: missing required :{key}:"))
        };
        let collection = required("COLLECTION")?;
        if collection == "."
            || collection == ".."
            || collection.contains(['/', '\\'])
            || collection.is_empty()
        {
            bail!("{display}: invalid :COLLECTION: {collection:?}");
        }
        let id_prefix = required("ID_PREFIX")?;
        if id_prefix.contains(['/', '\\']) {
            bail!("{display}: :ID_PREFIX: must not contain a path separator");
        }
        let states = words(heading.property("STATES"));
        let state_set: BTreeSet<_> = states.iter().map(String::as_str).collect();
        let mut transitions = BTreeMap::new();
        for rule in words(heading.property("TRANSITIONS")) {
            let Some((from, targets)) = rule.split_once('>') else {
                bail!("{display}: invalid transition {rule:?}; expected from>to,to");
            };
            if !state_set.contains(from) {
                bail!("{display}: transition source {from:?} is not in :STATES:");
            }
            let targets = if targets == "-" {
                Vec::new()
            } else {
                targets.split(',').map(str::to_string).collect()
            };
            if let Some(target) = targets
                .iter()
                .find(|target| !state_set.contains(target.as_str()))
            {
                bail!("{display}: transition target {target:?} is not in :STATES:");
            }
            if transitions.insert(from.to_string(), targets).is_some() {
                bail!("{display}: duplicate transition source {from:?}");
            }
        }
        if !states.is_empty() && transitions.len() != states.len() {
            bail!("{display}: every state must declare one :TRANSITIONS: rule");
        }
        if states.is_empty() && !transitions.is_empty() {
            bail!("{display}: :TRANSITIONS: requires :STATES:");
        }
        Ok(Self {
            collection,
            id_prefix,
            label: required("LABEL")?,
            label_plural: required("LABEL_PLURAL")?,
            required_properties: words(heading.property("REQUIRED_PROPERTIES")),
            states,
            transitions,
            regenerate_prompt: heading
                .property("REGENERATE_PROMPT")
                .map(str::trim)
                .filter(|value| !value.is_empty())
                .map(str::to_string),
            reserved_files: words(heading.property("RESERVED_FILES")),
        })
    }

    pub fn allows_transition(&self, from: &str, to: &str) -> bool {
        self.transitions
            .get(from)
            .is_some_and(|targets| targets.iter().any(|target| target == to))
    }
}

fn words(value: Option<&str>) -> Vec<String> {
    value
        .into_iter()
        .flat_map(str::split_whitespace)
        .map(str::to_string)
        .collect()
}

/// Mint one id using the descriptor's prefix and the established 5-char stem.
pub fn mint_node_id(descriptor: &NodeTypeDescriptor) -> String {
    format!("{}{}", descriptor.id_prefix, random_stem())
}

/// Mint and atomically reserve `<project>/.orgasmic/<collection>/<id>/`.
pub fn create_node_dir(
    project_root: &Path,
    descriptor: &NodeTypeDescriptor,
) -> std::io::Result<(String, PathBuf)> {
    create_node_dir_with(project_root, descriptor, || mint_node_id(descriptor))
}

fn create_node_dir_with(
    project_root: &Path,
    descriptor: &NodeTypeDescriptor,
    mut mint: impl FnMut() -> String,
) -> std::io::Result<(String, PathBuf)> {
    let collection = project_root.join(".orgasmic").join(&descriptor.collection);
    std::fs::create_dir_all(&collection)?;
    loop {
        let id = mint();
        let dir = collection.join(&id);
        match std::fs::create_dir(&dir) {
            Ok(()) => return Ok((id, dir)),
            Err(error) if error.kind() == ErrorKind::AlreadyExists => continue,
            Err(error) => return Err(error),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn descriptor_drives_mint_and_directory_collision_retry() {
        let descriptor = NodeTypeDescriptor::parse(
            "* Node type\n:PROPERTIES:\n:COLLECTION: things\n:ID_PREFIX: x_\n:LABEL: Thing\n:LABEL_PLURAL: Things\n:REQUIRED_PROPERTIES: ID\n:STATES:\n:TRANSITIONS:\n:REGENERATE_PROMPT:\n:END:\n",
            "things.org",
        )
        .unwrap();
        let root = tempfile::tempdir().unwrap();
        let collection = root.path().join(".orgasmic/things");
        std::fs::create_dir_all(collection.join("x_FIRST")).unwrap();
        let mut ids = ["x_FIRST", "x_SECOND"].into_iter();
        let (id, dir) =
            create_node_dir_with(root.path(), &descriptor, || ids.next().unwrap().into()).unwrap();
        assert_eq!(id, "x_SECOND");
        assert!(dir.is_dir());
        let minted = mint_node_id(&descriptor);
        assert!(minted.starts_with("x_"));
        assert_eq!(minted.len(), 7);
    }
}
