use std::collections::BTreeMap;
use std::path::Path;

use anyhow::{bail, Context, Result};
use orgasmic_core::NodeTypeDescriptor;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NodeTypeLintSeverity {
    Low,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NodeTypeLint {
    pub severity: NodeTypeLintSeverity,
    pub message: String,
}

pub struct ResolvedCollection<'a> {
    pub descriptor: Option<&'a NodeTypeDescriptor>,
    pub label: &'a str,
    pub lint: Option<NodeTypeLint>,
}

#[derive(Debug, Clone, Default)]
pub struct NodeTypeRegistry {
    descriptors: BTreeMap<String, NodeTypeDescriptor>,
}

impl NodeTypeRegistry {
    pub fn load(dir: &Path) -> Result<Self> {
        let mut descriptors = BTreeMap::new();
        let entries = std::fs::read_dir(dir)
            .with_context(|| format!("read node-type descriptors from {}", dir.display()))?;
        for entry in entries {
            let entry = entry.with_context(|| format!("read entry in {}", dir.display()))?;
            let path = entry.path();
            if path.extension().and_then(|value| value.to_str()) != Some("org") {
                continue;
            }
            let source = std::fs::read_to_string(&path)
                .with_context(|| format!("read {}", path.display()))?;
            let descriptor = NodeTypeDescriptor::parse(&source, &path.display().to_string())?;
            let collection = descriptor.collection.clone();
            if descriptors.insert(collection.clone(), descriptor).is_some() {
                bail!("duplicate node-type descriptor for collection {collection:?}");
            }
        }
        Ok(Self { descriptors })
    }

    pub fn embedded() -> Result<Self> {
        let mut descriptors = BTreeMap::new();
        for (name, source) in [
            (
                "task.org",
                include_str!("../../../shipped/schema/node-types/task.org"),
            ),
            (
                "decision.org",
                include_str!("../../../shipped/schema/node-types/decision.org"),
            ),
            (
                "glossary.org",
                include_str!("../../../shipped/schema/node-types/glossary.org"),
            ),
            (
                "artifact.org",
                include_str!("../../../shipped/schema/node-types/artifact.org"),
            ),
        ] {
            let descriptor = NodeTypeDescriptor::parse(source, name)?;
            descriptors.insert(descriptor.collection.clone(), descriptor);
        }
        Ok(Self { descriptors })
    }

    pub fn descriptor(&self, collection: &str) -> Option<&NodeTypeDescriptor> {
        self.descriptors.get(collection)
    }

    pub fn len(&self) -> usize {
        self.descriptors.len()
    }

    pub fn is_empty(&self) -> bool {
        self.descriptors.is_empty()
    }

    /// Unknown collections keep the type-agnostic node kernel. The lint is
    /// advisory because indexing, journals, and comments need no descriptor.
    pub fn resolve<'a>(&'a self, collection: &'a str) -> ResolvedCollection<'a> {
        match self.descriptor(collection) {
            Some(descriptor) => ResolvedCollection {
                descriptor: Some(descriptor),
                label: &descriptor.label,
                lint: None,
            },
            None => ResolvedCollection {
                descriptor: None,
                label: collection,
                lint: Some(NodeTypeLint {
                    severity: NodeTypeLintSeverity::Low,
                    message: format!(
                        "collection {collection:?} has no shipped node-type descriptor; using generic indexing, comments, and journals"
                    ),
                }),
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn repo_root() -> std::path::PathBuf {
        Path::new(env!("CARGO_MANIFEST_DIR")).join("../..")
    }

    #[test]
    fn shipped_descriptors_load_and_unknown_collection_is_generic() {
        let registry = NodeTypeRegistry::load(&repo_root().join("shipped/schema/node-types"))
            .expect("load shipped node types");
        assert_eq!(registry.len(), 4);
        for (collection, prefix) in [
            ("tasks", "TASK-"),
            ("decisions", "dec_"),
            ("glossary", "term_"),
            ("artifacts", "ART-"),
        ] {
            assert_eq!(registry.descriptor(collection).unwrap().id_prefix, prefix);
        }
        let tasks = registry.descriptor("tasks").unwrap();
        assert!(tasks.states.contains(&"in_review".to_string()));
        assert!(tasks.allows_transition("in_review", "done"));
        assert!(!tasks.allows_transition("done", "in_progress"));

        let generic = registry.resolve("problems");
        assert!(generic.descriptor.is_none());
        assert_eq!(generic.label, "problems");
        assert_eq!(generic.lint.unwrap().severity, NodeTypeLintSeverity::Low);
    }
}
