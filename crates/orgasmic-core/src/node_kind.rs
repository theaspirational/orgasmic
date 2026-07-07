//! Canonical `--kind` vocabulary for `org node` reads/writes (TASK-JJ9RD).
//!
//! The daemon's node-layer resolver and the CLI's `--kind` argument must
//! accept exactly the same set of strings, or a request can silently resolve
//! against the wrong `.org` file. Both derive from [`NodeKind`] instead of
//! keeping independent lists, and a parity test in `orgasmic-cli` asserts the
//! CLI's advertised kinds match this list.

/// One selectable org-node layer. Node ids that lack a distinctive prefix
/// (`project`, `config` — both scaffold the same `:ID:`) can only be resolved
/// through an explicit `--kind`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NodeKind {
    Decision,
    Architecture,
    Glossary,
    Project,
    Task,
    Goal,
    Handoff,
    Config,
}

impl NodeKind {
    /// Every accepted kind, in the order shown in `--help`.
    pub const ALL: [NodeKind; 8] = [
        NodeKind::Decision,
        NodeKind::Architecture,
        NodeKind::Glossary,
        NodeKind::Project,
        NodeKind::Task,
        NodeKind::Goal,
        NodeKind::Handoff,
        NodeKind::Config,
    ];

    pub fn as_str(self) -> &'static str {
        match self {
            NodeKind::Decision => "decision",
            NodeKind::Architecture => "architecture",
            NodeKind::Glossary => "glossary",
            NodeKind::Project => "project",
            NodeKind::Task => "task",
            NodeKind::Goal => "goal",
            NodeKind::Handoff => "handoff",
            NodeKind::Config => "config",
        }
    }

    pub fn parse(s: &str) -> Option<NodeKind> {
        NodeKind::ALL.into_iter().find(|kind| kind.as_str() == s)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn as_str_and_parse_round_trip_for_every_kind() {
        for kind in NodeKind::ALL {
            assert_eq!(NodeKind::parse(kind.as_str()), Some(kind));
        }
    }

    #[test]
    fn parse_rejects_unknown_kind() {
        assert_eq!(NodeKind::parse("bogus"), None);
        assert_eq!(NodeKind::parse(""), None);
    }

    #[test]
    fn all_kinds_have_distinct_strings() {
        let mut seen = std::collections::BTreeSet::new();
        for kind in NodeKind::ALL {
            assert!(seen.insert(kind.as_str()), "duplicate kind string {kind:?}");
        }
    }
}
