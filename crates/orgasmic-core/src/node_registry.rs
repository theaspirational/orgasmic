use std::collections::BTreeMap;
use std::path::Path;

use crate::{Home, NodeTypeDescriptor};
use anyhow::{bail, Context, Result};

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
    pub label: String,
    pub lint: Option<NodeTypeLint>,
}

#[derive(Debug, Clone, Default)]
pub struct NodeTypeRegistry {
    descriptors: BTreeMap<String, NodeTypeDescriptor>,
    hooks: BTreeMap<String, WriteHooks>,
}

pub type WriteValidator = fn(&crate::OrgFile, &crate::Heading) -> Result<()>;
pub type TransitionValidator =
    fn(&crate::OrgFile, &crate::Heading, Option<&crate::Heading>) -> Result<()>;

#[derive(Debug, Clone, Copy, Default)]
pub struct WriteHooks {
    pub validate_write: Option<WriteValidator>,
    pub validate_transition: Option<TransitionValidator>,
}

impl NodeTypeRegistry {
    pub fn register_hooks(&mut self, collection: &str, hooks: WriteHooks) -> Result<()> {
        if !self.descriptors.contains_key(collection) {
            bail!("unknown collection {collection}");
        }
        self.hooks.insert(collection.to_owned(), hooks);
        Ok(())
    }

    pub fn validate_write(
        &self,
        collection: &str,
        file: &crate::OrgFile,
        heading: &crate::Heading,
    ) -> Result<()> {
        let descriptor = self
            .descriptor(collection)
            .ok_or_else(|| anyhow::anyhow!("unknown collection {collection}"))?;
        let id = heading
            .property("ID")
            .ok_or_else(|| anyhow::anyhow!("missing ID"))?;
        crate::node_type::validate_component(id)?;
        if !(id.starts_with(&descriptor.id_prefix)
            || collection == "glossary" && id.starts_with("term:"))
        {
            bail!("id does not belong to collection {collection}");
        }
        for required in &descriptor.required_properties {
            if heading
                .property(required)
                .is_none_or(|value| value.trim().is_empty())
            {
                bail!("missing required property {required}");
            }
        }
        let mut keys = std::collections::BTreeSet::new();
        for property in heading.property_entries() {
            if !keys.insert(&property.key) {
                bail!("duplicate property {}", property.key);
            }
        }
        if !descriptor.states.is_empty()
            && !heading
                .todo
                .as_ref()
                .is_some_and(|state| descriptor.states.contains(&state.to_ascii_lowercase()))
        {
            bail!("state must be one of {}; for a hand-written node, add `#+todo: {}` before the first heading so Org can parse its state", descriptor.states.join(", "), descriptor.states.join(" ").to_ascii_uppercase());
        }
        if let Some(validate) = self
            .hooks
            .get(collection)
            .and_then(|hooks| hooks.validate_write)
        {
            validate(file, heading)?;
        }
        Ok(())
    }

    pub fn validate_transition(
        &self,
        collection: &str,
        before: Option<&crate::Heading>,
        file: &crate::OrgFile,
        after: &crate::Heading,
    ) -> Result<()> {
        let descriptor = self
            .descriptor(collection)
            .ok_or_else(|| anyhow::anyhow!("unknown collection {collection}"))?;
        let from = before
            .and_then(|heading| heading.todo.as_ref())
            .map(|state| state.to_ascii_lowercase());
        let to = after.todo.as_ref().map(|state| state.to_ascii_lowercase());
        if let Some(validate) = self
            .hooks
            .get(collection)
            .and_then(|hooks| hooks.validate_transition)
        {
            validate(file, after, before)?;
        }
        if from == to {
            return Ok(());
        }
        if let Some(next) = to.as_deref() {
            if let Some(previous) = from.as_deref() {
                if !descriptor.allows_transition(previous, next) {
                    bail!("transition {previous} -> {next} is not allowed");
                }
            } else if descriptor.states.first().map(String::as_str) != Some(next) {
                bail!("new nodes must start in the descriptor's initial state");
            }
        } else if !descriptor.states.is_empty() {
            bail!("state cannot be removed");
        }
        Ok(())
    }
    /// One loader shared by daemon and CLI. User files replace shipped files
    /// with the same relative name; ownership collisions are always refused.
    pub fn for_home(home: &Home) -> Result<Self> {
        let mut registry = Self::embedded()?;
        let relative = Path::new("schema/node-types");
        let mut names = std::collections::BTreeSet::new();
        let mut owners = BTreeMap::new();
        for root in [home.source().join("shipped"), home.user()] {
            if let Ok(entries) = std::fs::read_dir(root.join(relative)) {
                for entry in entries {
                    let path = entry?.path();
                    if path.extension().is_some_and(|ext| ext == "org") {
                        names.insert(path.file_name().unwrap().to_owned());
                    }
                }
            }
        }
        for name in names {
            let path = crate::resolve_loader(home, &relative.join(name)).unwrap();
            let descriptor = NodeTypeDescriptor::parse(
                &std::fs::read_to_string(&path)?,
                &path.display().to_string(),
            )?;
            if let Some(owner) = owners.insert(descriptor.collection.clone(), path.clone()) {
                bail!(
                    "collection {} claimed by both {} and {}",
                    descriptor.collection,
                    owner.display(),
                    path.display()
                );
            }
            registry.register(descriptor, true)?;
        }
        let plugins = home.user().join("plugins");
        if let Ok(entries) = std::fs::read_dir(plugins) {
            for entry in entries {
                let path = entry?.path().join("plugin.org");
                if path.is_file() {
                    let source = std::fs::read_to_string(&path)?;
                    let file = crate::OrgFile::parse(&source, path.display().to_string())?;
                    if file.headings.iter().any(|heading| {
                        heading
                            .sections
                            .iter()
                            .any(|section| section.property("COLLECTION").is_some())
                    }) {
                        registry.register(
                            NodeTypeDescriptor::parse(&source, &path.display().to_string())?,
                            false,
                        )?;
                    }
                }
            }
        }
        // Compiled behavior still interprets legacy IDs and task stages.
        // User overrides may change presentation, but cannot silently make
        // those compiled readers disagree with the registry.
        for builtin in Self::embedded()?.descriptors() {
            let loaded = registry.descriptor(&builtin.collection).unwrap();
            if loaded.id_prefix != builtin.id_prefix {
                bail!(
                    "compiled collection {} must retain prefix {}",
                    builtin.collection,
                    builtin.id_prefix
                );
            }
            if builtin.collection == "tasks" {
                let actual: std::collections::BTreeSet<_> = loaded.states.iter().collect();
                let expected: std::collections::BTreeSet<_> = builtin.states.iter().collect();
                if actual != expected {
                    bail!("task descriptor states must agree with compiled lifecycle behavior");
                }
            }
        }
        Ok(registry)
    }

    pub fn register(&mut self, descriptor: NodeTypeDescriptor, replace: bool) -> Result<()> {
        if ["goal-", "handoff-current"].iter().any(|reserved| {
            reserved.starts_with(&descriptor.id_prefix)
                || descriptor.id_prefix.starts_with(reserved)
        }) {
            bail!(
                "prefix {} conflicts with core singleton storage",
                descriptor.id_prefix
            );
        }
        if !replace && self.descriptors.contains_key(&descriptor.collection) {
            bail!("collection {} already has an owner", descriptor.collection);
        }
        for other in self.descriptors.values() {
            if other.collection != descriptor.collection
                && (other.id_prefix.starts_with(&descriptor.id_prefix)
                    || descriptor.id_prefix.starts_with(&other.id_prefix))
            {
                bail!(
                    "prefix {} conflicts with collection {}",
                    descriptor.id_prefix,
                    other.collection
                );
            }
        }
        self.descriptors
            .insert(descriptor.collection.clone(), descriptor);
        Ok(())
    }

    pub fn descriptors(&self) -> impl Iterator<Item = &NodeTypeDescriptor> {
        self.descriptors.values()
    }

    pub fn collection_for_kind(&self, kind: &str) -> Option<&NodeTypeDescriptor> {
        let collection = match kind {
            "task" => "tasks",
            "decision" => "decisions",
            "term" => "glossary",
            "artifact" => "artifacts",
            other => other,
        };
        self.resolve(collection).descriptor
    }

    pub fn resolve_node<'a>(&'a self, kind: Option<&str>, id: &str) -> Result<crate::NodeKind<'a>> {
        crate::node_type::validate_component(id)?;
        let inferred = if id == "handoff-current" {
            Some(crate::NodeKind::Handoff)
        } else if id.starts_with("goal-") {
            Some(crate::NodeKind::Goal)
        } else if id.starts_with("term:") {
            Some(crate::NodeKind::Glossary)
        } else {
            self.descriptor_for_id(id)
                .map(|descriptor| crate::NodeKind::collection(&descriptor.collection))
        };
        match kind {
            Some(kind) => {
                let explicit = crate::NodeKind::singleton(kind)
                    .or_else(|| {
                        self.collection_for_kind(kind)
                            .map(|descriptor| crate::NodeKind::collection(&descriptor.collection))
                    })
                    .ok_or_else(|| anyhow::anyhow!("unknown node kind {kind}"))?;
                if inferred.is_some_and(|inferred| inferred != explicit) {
                    bail!("node {id} does not belong to kind {kind}");
                }
                if inferred.is_none() && explicit.collection_name().is_some() {
                    bail!("unknown node id {id}; no descriptor owns its prefix");
                }
                Ok(explicit)
            }
            None => inferred.ok_or_else(|| {
                anyhow::anyhow!("unknown node id {id}; no descriptor owns its prefix")
            }),
        }
    }
    pub fn load(dir: &Path) -> Result<Self> {
        let mut registry = Self::default();
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
            registry.register(descriptor, false)?;
        }
        Ok(registry)
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
        Ok(Self {
            descriptors,
            hooks: BTreeMap::new(),
        })
    }

    pub fn descriptor(&self, collection: &str) -> Option<&NodeTypeDescriptor> {
        self.descriptors.get(collection)
    }

    pub fn descriptor_for_id(&self, id: &str) -> Option<&NodeTypeDescriptor> {
        self.descriptors
            .values()
            .filter(|descriptor| id.starts_with(&descriptor.id_prefix))
            .max_by_key(|descriptor| descriptor.id_prefix.len())
    }

    pub fn len(&self) -> usize {
        self.descriptors.len()
    }

    pub fn is_empty(&self) -> bool {
        self.descriptors.is_empty()
    }

    /// Unknown collections keep the type-agnostic node kernel. The lint is
    /// advisory because indexing, journals, and comments need no descriptor.
    pub fn resolve(&self, collection: &str) -> ResolvedCollection<'_> {
        match self.descriptor(collection) {
            Some(descriptor) => ResolvedCollection {
                descriptor: Some(descriptor),
                label: descriptor.label.clone(),
                lint: None,
            },
            None => ResolvedCollection {
                descriptor: None,
                label: collection.to_owned(),
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
        for id in ["TASK-ABCDE", "dec_ABCDE", "term_ABCDE", "ART-ABCDE"] {
            assert!(registry.descriptor_for_id(id).is_some(), "{id}");
        }

        let generic = registry.resolve("problems");
        assert!(generic.descriptor.is_none());
        assert_eq!(generic.label, "problems");
        assert_eq!(generic.lint.unwrap().severity, NodeTypeLintSeverity::Low);
    }

    #[test]
    fn user_tier_and_nested_plugins_share_ownership_and_resolution() {
        let temp = tempfile::tempdir().unwrap();
        let home = Home::at(temp.path().to_owned());
        let root = home.user().join("plugins/meetings");
        std::fs::create_dir_all(&root).unwrap();
        let source = "* Plugin\n:PROPERTIES:\n:ID: meetings\n:END:\n** Node type\n:PROPERTIES:\n:COLLECTION: meetings\n:ID_PREFIX: MEET-\n:LABEL: Meeting\n:LABEL_PLURAL: Meetings\n:REQUIRED_PROPERTIES: ID\n:STATES: active archived\n:TRANSITIONS: active>archived archived>-\n:END:\n";
        std::fs::write(root.join("plugin.org"), source).unwrap();
        let registry = NodeTypeRegistry::for_home(&home).unwrap();
        assert_eq!(
            registry
                .resolve_node(None, "MEET-ABCDE")
                .unwrap()
                .collection_name(),
            Some("meetings")
        );
        assert!(registry
            .resolve_node(Some("glossary"), "MEET-ABCDE")
            .is_err());
        assert!(registry.resolve_node(None, "UNKNOWN-ABCDE").is_err());
        let handwritten = crate::OrgFile::parse(
            "* ACTIVE MEET-ABCDE Notes\n:PROPERTIES:\n:ID: MEET-ABCDE\n:END:\n",
            "node.org",
        )
        .unwrap();
        let error = registry
            .validate_write("meetings", &handwritten, &handwritten.headings[0])
            .unwrap_err()
            .to_string();
        assert!(error.contains("add `#+todo: ACTIVE ARCHIVED`"), "{error}");
        assert!(registry
            .resolve_node(Some("meetings"), "../escape")
            .is_err());
        let duplicate = home.user().join("plugins/duplicate");
        std::fs::create_dir_all(&duplicate).unwrap();
        std::fs::write(
            duplicate.join("plugin.org"),
            source.replace(":COLLECTION: meetings", ":COLLECTION: other"),
        )
        .unwrap();
        assert!(NodeTypeRegistry::for_home(&home)
            .unwrap_err()
            .to_string()
            .contains("prefix"));
    }
}
