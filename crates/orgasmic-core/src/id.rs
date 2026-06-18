//! Short-random node id minting and validation (dec_073a).
//!
//! Minted ids use 5-character Crockford base32 stems with class prefixes.
//! Legacy sequential numeric ids remain valid for reference resolution.

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
}

impl NodeIdClass {
    pub fn prefix(self) -> &'static str {
        match self {
            Self::Task => "TASK-",
            Self::Decision => "dec_",
            Self::Architecture => "arch_",
            Self::Term => "term_",
        }
    }
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
}
