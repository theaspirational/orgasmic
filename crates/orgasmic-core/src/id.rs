//! Short-random node id minting and validation (dec_073a).
//!
//! Minted ids use 5-character Crockford base32 stems with class prefixes.
//! Legacy sequential numeric ids remain valid for reference resolution.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;

use rand::Rng;

/// Crockford base32 alphabet (uppercase; excludes I, L, O, U).
pub const CROCKFORD: &str = "0123456789ABCDEFGHJKMNPQRSTVWXYZ";

const STEM_LEN: usize = 5;

/// Node class for [`mint_node_id`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NodeIdClass {
    Task,
    Decision,
    Architecture,
    Term,
    Artifact,
}

impl NodeIdClass {
    pub fn prefix(self) -> &'static str {
        match self {
            Self::Task => "TASK-",
            Self::Decision => "dec_",
            Self::Architecture => "arch_",
            Self::Term => "term_",
            Self::Artifact => "ART-",
        }
    }

    pub fn matches_id_prefix(self, id: &str) -> bool {
        match self {
            Self::Task => id.starts_with("TASK-"),
            Self::Decision => id.starts_with("dec_"),
            Self::Architecture => id.starts_with("arch_"),
            Self::Term => id.starts_with("term_") || id.starts_with("term:"),
            Self::Artifact => id.starts_with("ART-"),
        }
    }
}

/// Return the node class implied by an id's stable prefix. This is deliberately
/// prefix-based (not full greenfield-id validation) so legacy/test ids such as
/// `dec_X` can still participate in class checks.
pub fn node_id_class_by_prefix(id: &str) -> Option<NodeIdClass> {
    if id.starts_with("TASK-") {
        Some(NodeIdClass::Task)
    } else if id.starts_with("dec_") {
        Some(NodeIdClass::Decision)
    } else if id.starts_with("arch_") {
        Some(NodeIdClass::Architecture)
    } else if id.starts_with("term_") || id.starts_with("term:") {
        Some(NodeIdClass::Term)
    } else if id.starts_with("ART-") {
        Some(NodeIdClass::Artifact)
    } else {
        None
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ParentTreeError {
    MalformedParent {
        id: String,
        value: String,
    },
    WrongClass {
        id: String,
        parent: String,
        expected: NodeIdClass,
    },
    MissingParent {
        id: String,
        parent: String,
    },
    SelfParent {
        id: String,
    },
    Cycle {
        chain: Vec<String>,
    },
    DuplicateId {
        id: String,
    },
    UnknownId {
        id: String,
    },
}

impl fmt::Display for ParentTreeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::MalformedParent { id, value } => {
                write!(f, "{id} has malformed :PARENT: value {value:?}")
            }
            Self::WrongClass {
                id,
                parent,
                expected,
            } => write!(
                f,
                "{id} has parent {parent}, but :PARENT: must point at a {:?} id",
                expected
            ),
            Self::MissingParent { id, parent } => {
                write!(f, "{id} has orphan parent {parent}")
            }
            Self::SelfParent { id } => write!(f, "{id} cannot be its own parent"),
            Self::Cycle { chain } => write!(f, ":PARENT: cycle detected: {}", chain.join(" -> ")),
            Self::DuplicateId { id } => write!(f, "duplicate parent-tree id {id}"),
            Self::UnknownId { id } => write!(f, "unknown parent-tree id {id}"),
        }
    }
}

impl std::error::Error for ParentTreeError {}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParentTreeNode {
    pub id: String,
    pub parent: Option<String>,
}

/// Normalize and validate a single stored `:PARENT:` value. Empty/missing means
/// no parent; otherwise the value must be exactly one same-class id and not the
/// node itself. Existence and cycle checks require the complete node set and
/// are handled by [`validate_parent_tree`].
pub fn parse_parent_value(
    class: NodeIdClass,
    id: &str,
    raw: Option<&str>,
) -> Result<Option<String>, ParentTreeError> {
    let Some(raw) = raw.map(str::trim).filter(|value| !value.is_empty()) else {
        return Ok(None);
    };
    let mut tokens = raw.split_whitespace();
    let parent = tokens.next().unwrap_or_default();
    if tokens.next().is_some() {
        return Err(ParentTreeError::MalformedParent {
            id: id.to_string(),
            value: raw.to_string(),
        });
    }
    validate_parent_pointer(class, id, parent)?;
    Ok(Some(parent.to_string()))
}

pub fn validate_parent_pointer(
    class: NodeIdClass,
    id: &str,
    parent: &str,
) -> Result<(), ParentTreeError> {
    if parent == id {
        return Err(ParentTreeError::SelfParent { id: id.to_string() });
    }
    if !class.matches_id_prefix(parent) {
        return Err(ParentTreeError::WrongClass {
            id: id.to_string(),
            parent: parent.to_string(),
            expected: class,
        });
    }
    Ok(())
}

pub fn validate_parent_exists<'a>(
    id: &str,
    parent: Option<&str>,
    ids: impl IntoIterator<Item = &'a str>,
) -> Result<(), ParentTreeError> {
    let Some(parent) = parent else {
        return Ok(());
    };
    if ids.into_iter().any(|candidate| candidate == parent) {
        Ok(())
    } else {
        Err(ParentTreeError::MissingParent {
            id: id.to_string(),
            parent: parent.to_string(),
        })
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParentTree {
    parents: BTreeMap<String, Option<String>>,
    children: BTreeMap<String, Vec<String>>,
}

impl ParentTree {
    pub fn parent_of(&self, id: &str) -> Result<Option<&str>, ParentTreeError> {
        self.parents
            .get(id)
            .map(|parent| parent.as_deref())
            .ok_or_else(|| ParentTreeError::UnknownId { id: id.to_string() })
    }

    pub fn children_of(&self, id: &str) -> &[String] {
        self.children.get(id).map(Vec::as_slice).unwrap_or(&[])
    }

    /// Ancestors from nearest parent toward the root.
    pub fn ancestor_chain(&self, id: &str) -> Result<Vec<String>, ParentTreeError> {
        if !self.parents.contains_key(id) {
            return Err(ParentTreeError::UnknownId { id: id.to_string() });
        }
        let mut out = Vec::new();
        let mut current = id;
        while let Some(parent) = self.parent_of(current)? {
            out.push(parent.to_string());
            current = parent;
        }
        Ok(out)
    }
}

/// Validate a same-class `:PARENT:` tree with create-time/file-time existence,
/// self-parent rejection, and cycle detection. Child ordering is the caller's
/// iteration order, so daemon read-models can pass document order through.
pub fn validate_parent_tree<I>(class: NodeIdClass, nodes: I) -> Result<ParentTree, ParentTreeError>
where
    I: IntoIterator<Item = ParentTreeNode>,
{
    let mut parents: BTreeMap<String, Option<String>> = BTreeMap::new();
    let mut ordered_nodes = Vec::new();
    for node in nodes {
        if parents.contains_key(&node.id) {
            return Err(ParentTreeError::DuplicateId { id: node.id });
        }
        if let Some(parent) = node.parent.as_deref() {
            validate_parent_pointer(class, &node.id, parent)?;
        }
        ordered_nodes.push(node.id.clone());
        parents.insert(node.id, node.parent);
    }
    for (id, parent) in &parents {
        if let Some(parent) = parent.as_deref() {
            if !parents.contains_key(parent) {
                return Err(ParentTreeError::MissingParent {
                    id: id.clone(),
                    parent: parent.to_string(),
                });
            }
        }
    }
    for id in parents.keys() {
        let mut seen: BTreeSet<String> = BTreeSet::new();
        let mut chain = vec![id.clone()];
        let mut current = id.as_str();
        while let Some(parent) = parents.get(current).and_then(|parent| parent.as_deref()) {
            if !seen.insert(parent.to_string()) {
                chain.push(parent.to_string());
                return Err(ParentTreeError::Cycle { chain });
            }
            chain.push(parent.to_string());
            current = parent;
        }
    }
    let mut children: BTreeMap<String, Vec<String>> = BTreeMap::new();
    for id in ordered_nodes {
        if let Some(parent) = parents.get(&id).and_then(|parent| parent.as_ref()) {
            children.entry(parent.clone()).or_default().push(id);
        }
    }
    Ok(ParentTree { parents, children })
}

/// Mint one conforming node id for `class`.
///
/// The 5-character stem is cryptographically random, drawn from [`CROCKFORD`],
/// and re-rolled until it contains at least one letter (keeping minted task ids
/// disjoint from legacy all-numeric `TASK-NNN` ids).
pub fn mint_node_id(class: NodeIdClass) -> String {
    format!("{}{}", class.prefix(), random_stem())
}

fn random_stem() -> String {
    let mut rng = rand::thread_rng();
    loop {
        let stem: String = (0..STEM_LEN)
            .map(|_| {
                let idx = rng.gen_range(0..CROCKFORD.len());
                CROCKFORD.as_bytes()[idx] as char
            })
            .collect();
        if stem.chars().any(|c| c.is_ascii_alphabetic()) {
            return stem;
        }
    }
}

/// True when `stem` is a 5-char Crockford token with at least one letter.
pub fn is_minted_stem(stem: &str) -> bool {
    stem.len() == STEM_LEN
        && stem.chars().all(is_crockford_char)
        && stem.chars().any(|c| c.is_ascii_alphabetic())
}

fn is_crockford_char(c: char) -> bool {
    c.is_ascii_digit() || (c.is_ascii_uppercase() && c != 'I' && c != 'L' && c != 'O' && c != 'U')
}

/// Legacy sequential numeric task stem (`001`, `158`, …).
pub fn is_legacy_numeric_task_stem(stem: &str) -> bool {
    !stem.is_empty() && stem.chars().all(|c| c.is_ascii_digit())
}

fn is_valid_task_stem(stem: &str) -> bool {
    is_legacy_numeric_task_stem(stem) || is_minted_stem(stem)
}

/// Derive parent task id from subtask grammar (`TASK-PARENT.N`).
pub fn derive_task_parent_id(id: &str) -> Option<String> {
    let rest = id.strip_prefix("TASK-")?;
    let (stem, suffix) = rest.rsplit_once('.')?;
    if suffix.is_empty() || !suffix.chars().all(|c| c.is_ascii_digit()) {
        return None;
    }
    let parent = format!("TASK-{stem}");
    if is_valid_task_path_id(&parent) {
        Some(parent)
    } else {
        None
    }
}

/// Validate a task path id: `TASK-<stem>` or `TASK-<stem>.<n>…`.
pub fn is_valid_task_path_id(value: &str) -> bool {
    let Some(rest) = value.strip_prefix("TASK-") else {
        return false;
    };
    let mut parts = rest.split('.');
    let Some(first) = parts.next() else {
        return false;
    };
    if !is_valid_task_stem(first) {
        return false;
    }
    for part in parts {
        if part.is_empty() || !part.chars().all(|c| c.is_ascii_digit()) {
            return false;
        }
    }
    true
}

fn arch_stem_valid(stem: &str) -> bool {
    if is_minted_stem(stem) {
        return true;
    }
    stem.len() == 3 && stem.chars().all(|c| c.is_ascii_digit())
}

/// Validate an architecture node id (`arch_<stem>` or `arch_<stem>.<n>`).
pub fn is_arch_id(value: &str) -> bool {
    let Some(rest) = value.strip_prefix("arch_") else {
        return false;
    };
    let mut parts = rest.split('.');
    let Some(first) = parts.next() else {
        return false;
    };
    if !arch_stem_valid(first) {
        return false;
    }
    if let Some(part) = parts.next() {
        if part.is_empty() || !part.chars().all(|c| c.is_ascii_digit()) {
            return false;
        }
        if parts.next().is_some() {
            return false;
        }
    }
    true
}

fn dec_stem_valid(stem: &str) -> bool {
    is_minted_stem(stem) || (!stem.is_empty() && stem.chars().all(|c| c.is_ascii_digit()))
}

/// Validate a decision node id (`dec_<stem>`).
pub fn is_dec_id(value: &str) -> bool {
    let Some(stem) = value.strip_prefix("dec_") else {
        return false;
    };
    dec_stem_valid(stem)
}

/// True when `id` is a legacy all-numeric create id that must not be minted post-cutover.
pub fn is_legacy_sequential_create_id(class: NodeIdClass, id: &str) -> bool {
    match class {
        NodeIdClass::Task => {
            id.starts_with("TASK-")
                && !id.contains('.')
                && id["TASK-".len()..].chars().all(|c| c.is_ascii_digit())
        }
        NodeIdClass::Decision => {
            id.starts_with("dec_") && id[4..].chars().all(|c| c.is_ascii_digit())
        }
        NodeIdClass::Architecture => {
            id.starts_with("arch_")
                && !id.contains('.')
                && id[5..].chars().all(|c| c.is_ascii_digit())
        }
        NodeIdClass::Term => false,
        NodeIdClass::Artifact => false,
    }
}

/// True when a minted id matches `^TASK-[0-9]+$` (must never happen).
pub fn looks_like_legacy_numeric_task(id: &str) -> bool {
    is_legacy_sequential_create_id(NodeIdClass::Task, id)
}

/// Post-migration task identity: minted stem only (no legacy all-numeric).
pub fn is_valid_greenfield_task_id(value: &str) -> bool {
    let Some(rest) = value.strip_prefix("TASK-") else {
        return false;
    };
    let mut parts = rest.split('.');
    let Some(stem) = parts.next() else {
        return false;
    };
    if !is_minted_stem(stem) {
        return false;
    }
    for part in parts {
        if part.is_empty() || !part.chars().all(|c| c.is_ascii_digit()) {
            return false;
        }
    }
    true
}

/// Post-migration decision identity: `dec_<minted-stem>` only.
pub fn is_valid_greenfield_dec_id(value: &str) -> bool {
    let Some(stem) = value.strip_prefix("dec_") else {
        return false;
    };
    is_minted_stem(stem)
}

/// Post-migration architecture identity: minted stem with optional `.N` sub-id.
pub fn is_valid_greenfield_arch_id(value: &str) -> bool {
    let Some(rest) = value.strip_prefix("arch_") else {
        return false;
    };
    let mut parts = rest.split('.');
    let Some(stem) = parts.next() else {
        return false;
    };
    if !is_minted_stem(stem) {
        return false;
    }
    if let Some(part) = parts.next() {
        if part.is_empty() || !part.chars().all(|c| c.is_ascii_digit()) {
            return false;
        }
        if parts.next().is_some() {
            return false;
        }
    }
    true
}

/// Post-migration glossary identity: `term_<minted-stem>` only.
pub fn is_valid_greenfield_term_id(value: &str) -> bool {
    let Some(stem) = value.strip_prefix("term_") else {
        return false;
    };
    is_minted_stem(stem)
}

/// Artifact identity: `ART-<minted-stem>` only. Artifacts have no legacy
/// sequential form, so this is the sole accepted shape (no separate
/// "greenfield" relaxation needed).
pub fn is_valid_greenfield_artifact_id(value: &str) -> bool {
    let Some(stem) = value.strip_prefix("ART-") else {
        return false;
    };
    is_minted_stem(stem)
}

/// True when `id` is a well-formed post-migration node identity.
pub fn is_valid_greenfield_identity(id: &str) -> bool {
    if id.starts_with("TASK-") {
        is_valid_greenfield_task_id(id)
    } else if id.starts_with("dec_") {
        is_valid_greenfield_dec_id(id)
    } else if id.starts_with("arch_") {
        is_valid_greenfield_arch_id(id)
    } else if id.starts_with("term_") {
        is_valid_greenfield_term_id(id)
    } else if id.starts_with("ART-") {
        is_valid_greenfield_artifact_id(id)
    } else {
        false
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mint_node_id_property_test() {
        for class in [
            NodeIdClass::Task,
            NodeIdClass::Decision,
            NodeIdClass::Architecture,
            NodeIdClass::Term,
            NodeIdClass::Artifact,
        ] {
            for _ in 0..10_000 {
                let id = mint_node_id(class);
                assert!(id.starts_with(class.prefix()));
                let stem = id.strip_prefix(class.prefix()).expect("prefix");
                assert_eq!(stem.len(), STEM_LEN);
                assert!(is_minted_stem(stem));
                assert!(stem.chars().any(|c| c.is_ascii_alphabetic()));
                if class == NodeIdClass::Task {
                    assert!(
                        !looks_like_legacy_numeric_task(&id),
                        "minted task id looked numeric: {id}"
                    );
                }
            }
        }
    }

    #[test]
    fn derive_task_parent_id_from_subtask_grammar() {
        assert_eq!(
            derive_task_parent_id("TASK-C9V29.1").as_deref(),
            Some("TASK-C9V29")
        );
        assert_eq!(
            derive_task_parent_id("TASK-8KX2M.3").as_deref(),
            Some("TASK-8KX2M")
        );
        assert!(derive_task_parent_id("TASK-8KX2M").is_none());
        assert!(derive_task_parent_id("dec_001").is_none());
    }

    #[test]
    fn task_path_id_accepts_legacy_and_minted() {
        assert!(is_valid_task_path_id("TASK-001"));
        assert!(is_valid_task_path_id("TASK-158"));
        assert!(is_valid_task_path_id("TASK-001.1"));
        assert!(is_valid_task_path_id("TASK-8KX2M"));
        assert!(is_valid_task_path_id("TASK-8KX2M.1"));
        assert!(!is_valid_task_path_id("TASK-"));
        assert!(!is_valid_task_path_id("dec_001"));
    }

    #[test]
    fn arch_id_accepts_legacy_and_minted() {
        assert!(is_arch_id("arch_001"));
        assert!(is_arch_id("arch_006.3"));
        assert!(is_arch_id("arch_8KX2M"));
        assert!(!is_arch_id("arch_"));
        assert!(!is_arch_id("arch_12345"));
    }

    #[test]
    fn legacy_sequential_create_id_detection() {
        assert!(is_legacy_sequential_create_id(
            NodeIdClass::Task,
            "TASK-001"
        ));
        assert!(!is_legacy_sequential_create_id(
            NodeIdClass::Task,
            "TASK-8KX2M"
        ));
        assert!(is_legacy_sequential_create_id(
            NodeIdClass::Decision,
            "dec_073"
        ));
        assert!(!is_legacy_sequential_create_id(
            NodeIdClass::Decision,
            "dec_8KX2M"
        ));
    }

    #[test]
    fn greenfield_identity_rejects_legacy_numeric() {
        assert!(!is_valid_greenfield_task_id("TASK-001"));
        assert!(!is_valid_greenfield_task_id("TASK-001.1"));
        assert!(is_valid_greenfield_task_id("TASK-8KX2M"));
        assert!(is_valid_greenfield_task_id("TASK-8KX2M.1"));
        assert!(!is_valid_greenfield_dec_id("dec_073"));
        assert!(is_valid_greenfield_dec_id("dec_8KX2M"));
        assert!(!is_valid_greenfield_arch_id("arch_001"));
        assert!(is_valid_greenfield_arch_id("arch_8KX2M"));
        assert!(is_valid_greenfield_arch_id("arch_8KX2M.3"));
    }

    #[test]
    fn greenfield_artifact_id_accepts_only_minted_stem() {
        assert!(is_valid_greenfield_artifact_id(&mint_node_id(
            NodeIdClass::Artifact
        )));
        assert!(is_valid_greenfield_artifact_id("ART-8KX2M"));
        assert!(!is_valid_greenfield_artifact_id("ART-"));
        assert!(!is_valid_greenfield_artifact_id("ART-1234"));
        assert!(!is_valid_greenfield_artifact_id("ART-123456"));
        assert!(!is_valid_greenfield_artifact_id("ART-00000"));
        assert!(!is_valid_greenfield_artifact_id("dec_8KX2M"));
    }

    #[test]
    fn greenfield_artifact_id_rejects_traversal_and_malformed_shapes() {
        for bad in [
            "ART-../..",
            "ART-..%2F",
            "../../etc/passwd",
            "ART-/etc/passwd",
            "ART-AAAA/",
            "art-8kx2m",
            "ART-8KX2M/../secret",
        ] {
            assert!(
                !is_valid_greenfield_artifact_id(bad),
                "expected rejection: {bad}"
            );
        }
    }

    #[test]
    fn parent_value_derives_optional_parent() {
        assert_eq!(
            parse_parent_value(NodeIdClass::Decision, "dec_CHILD", Some(" dec_PARENT "))
                .unwrap()
                .as_deref(),
            Some("dec_PARENT")
        );
        assert_eq!(
            parse_parent_value(NodeIdClass::Decision, "dec_CHILD", Some("   ")).unwrap(),
            None
        );
    }

    #[test]
    fn parent_tree_rejects_self_parent() {
        let err = parse_parent_value(NodeIdClass::Decision, "dec_A", Some("dec_A")).unwrap_err();
        assert!(matches!(err, ParentTreeError::SelfParent { id } if id == "dec_A"));
    }

    #[test]
    fn parent_tree_rejects_cycle() {
        let err = validate_parent_tree(
            NodeIdClass::Decision,
            [
                ParentTreeNode {
                    id: "dec_A".to_string(),
                    parent: Some("dec_C".to_string()),
                },
                ParentTreeNode {
                    id: "dec_B".to_string(),
                    parent: Some("dec_A".to_string()),
                },
                ParentTreeNode {
                    id: "dec_C".to_string(),
                    parent: Some("dec_B".to_string()),
                },
            ],
        )
        .unwrap_err();
        assert!(matches!(err, ParentTreeError::Cycle { .. }));
    }

    #[test]
    fn parent_tree_derives_parent_and_ancestor_chain() {
        let tree = validate_parent_tree(
            NodeIdClass::Decision,
            [
                ParentTreeNode {
                    id: "dec_ROOT".to_string(),
                    parent: None,
                },
                ParentTreeNode {
                    id: "dec_CHILD".to_string(),
                    parent: Some("dec_ROOT".to_string()),
                },
                ParentTreeNode {
                    id: "dec_GRAND".to_string(),
                    parent: Some("dec_CHILD".to_string()),
                },
            ],
        )
        .unwrap();
        assert_eq!(tree.parent_of("dec_CHILD").unwrap(), Some("dec_ROOT"));
        assert_eq!(
            tree.ancestor_chain("dec_GRAND").unwrap(),
            vec!["dec_CHILD".to_string(), "dec_ROOT".to_string()]
        );
        assert_eq!(tree.children_of("dec_ROOT"), &["dec_CHILD".to_string()]);
    }

    #[test]
    fn parent_tree_requires_create_time_existence() {
        let err =
            validate_parent_exists("dec_CHILD", Some("dec_MISSING"), ["dec_CHILD"]).unwrap_err();
        assert!(
            matches!(err, ParentTreeError::MissingParent { id, parent } if id == "dec_CHILD" && parent == "dec_MISSING")
        );
    }
}
