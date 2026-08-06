//! Canonical, parity-tested node examples printed by `orgasmic decision schema`
//! and `glossary schema` (TASK-SPBTA).
//!
//! The battle-test finding this closes: the property vocabulary for these
//! node types was only discoverable by reading another project's durable state.
//! These functions print one canonical node per supported type instead.
//!
//! Each example's property drawer is built from a single ordered spec list, so
//! the keys shown in the human-readable legend
//! and the keys actually written into the parseable example can never
//! diverge from each other. `schema_example_parity` (below) round-trips
//! every example back through this crate's own [`crate::schema`] parsers
//! and exhaustively destructures the result (no `..`), so a struct field
//! added to `schema.rs` without a matching update here fails to compile.

/// One `:KEY: value` line plus the plain-English note shown in the legend.
struct PropertySpec {
    key: &'static str,
    value: &'static str,
    note: &'static str,
}

fn drawer(id_key: &str, id_value: &str, specs: &[PropertySpec]) -> String {
    let mut out = format!(":PROPERTIES:\n:{id_key}: {id_value}\n");
    for spec in specs {
        out.push_str(&format!(":{}: {}\n", spec.key, spec.value));
    }
    out.push_str(":END:\n");
    out
}

fn legend(id_key: &str, id_note: &str, specs: &[PropertySpec]) -> String {
    let mut out = format!("  :{id_key}: — {id_note}\n");
    for spec in specs {
        out.push_str(&format!("  :{}: — {}\n", spec.key, spec.note));
    }
    out
}

// --- decision ------------------------------------------------------------

const DEC_ID: &str = "dec_7K2QX";

const DEC_PROPERTIES: &[PropertySpec] = &[
    PropertySpec {
        key: "PARENT",
        value: "dec_9M4NP",
        note: "parent decision id for a superseding/refining decision (omit for a root decision)",
    },
    PropertySpec {
        key: "GLOSSARY_REFS",
        value: "term_7K2QX",
        note: "glossary term ids this decision's language depends on",
    },
    PropertySpec {
        key: "DECIDED_AT",
        value: "2026-01-01",
        note: "date this decision was made",
    },
    PropertySpec {
        key: "SOURCE",
        value: "TASK-7K2QX",
        note: "free-text provenance, e.g. the task that motivated the decision",
    },
];

/// One parseable canonical decision node.
pub fn decision_schema_example() -> String {
    let mut out = format!("* {DEC_ID} Example Decision   :topic:\n");
    out.push_str(&drawer("ID", DEC_ID, DEC_PROPERTIES));
    out.push_str(
        "\n** Context\nWhat prompted this decision.\n\n\
         ** Decision\nWhat was decided.\n\n\
         ** Consequences\nWhat follows from it.\n",
    );
    out
}

/// Human-readable per-property legend for [`decision_schema_example`].
pub fn decision_schema_legend() -> String {
    let mut out = legend(
        "ID",
        "stable node id; mint with `orgasmic id mint --class decision`",
        DEC_PROPERTIES,
    );
    out.push_str(
        "\nTrailing `:topic:` tokens on the title line are Org tags (heading.tags), \
         not a property — repeat for more than one.\n",
    );
    out
}

// --- glossary ------------------------------------------------------------

const TERM_ID: &str = "term_7K2QX";

const TERM_PROPERTIES: &[PropertySpec] = &[
    PropertySpec {
        key: "CANONICAL",
        value: "feature flag",
        note: "the preferred term for this concept",
    },
    PropertySpec {
        key: "AVOID",
        value: "toggle; switch",
        note: "free-text discouraged synonyms (not id-validated, unlike RELATES_TO)",
    },
    PropertySpec {
        key: "RELATES_TO",
        value: "term_9M4NP",
        note: "glossary term ids related to this one (space-separated node ids)",
    },
    PropertySpec {
        key: "DEFINITION",
        value: "A named on/off switch gating a shippable-but-unfinished code path.",
        note: "the definition itself is a single-line property, NOT a `**` section",
    },
    PropertySpec {
        key: "DECIDED_AT",
        value: "2026-01-01",
        note: "date this definition was settled",
    },
];

/// One parseable canonical glossary term.
pub fn glossary_schema_example() -> String {
    let mut out = format!("* {TERM_ID} Feature Flag\n");
    out.push_str(&drawer("ID", TERM_ID, TERM_PROPERTIES));
    out
}

/// Human-readable per-property legend for [`glossary_schema_example`].
pub fn glossary_schema_legend() -> String {
    legend(
        "ID",
        "stable term id; mint with `orgasmic id mint --class term`",
        TERM_PROPERTIES,
    )
}

#[cfg(test)]
mod schema_example_parity {
    use super::*;
    use crate::org::OrgFile;
    use crate::schema::{DecisionNode, GlossaryTerm};

    fn parsed(body: &str) -> OrgFile {
        let source = format!("#+title: example\n\n{body}");
        OrgFile::parse(source, "example.org").expect("example must be valid org")
    }

    #[test]
    fn decision_example_round_trips_and_covers_every_field() {
        let file = parsed(&decision_schema_example());
        let heading = file.find_by_id(DEC_ID).expect("heading present");
        let node = DecisionNode::from_heading(&file, heading, "example.org").expect("parses");
        let DecisionNode {
            id,
            title,
            tags,
            parent,
            glossary_refs,
            decided_at,
            source,
            context,
            decision,
            consequences,
        } = &node;
        assert_eq!(*id, DEC_ID);
        assert!(!title.is_empty());
        assert!(!tags.is_empty(), "trailing :topic: tag must parse");
        assert!(parent.is_some());
        assert!(!glossary_refs.is_empty());
        assert!(decided_at.is_some());
        assert!(source.is_some());
        assert!(context.is_some());
        assert!(decision.is_some());
        assert!(consequences.is_some());

        let legend_text = decision_schema_legend();
        for spec in DEC_PROPERTIES {
            assert!(
                legend_text.contains(&format!(":{}:", spec.key)),
                "legend missing {}",
                spec.key
            );
        }
    }

    #[test]
    fn glossary_example_round_trips_and_covers_every_field() {
        let file = parsed(&glossary_schema_example());
        let heading = file.find_by_id(TERM_ID).expect("heading present");
        let term = GlossaryTerm::from_heading(heading, "example.org").expect("parses");
        let GlossaryTerm {
            id,
            canonical,
            avoid,
            relates_to,
            definition,
            decided_at,
        } = &term;
        assert_eq!(*id, TERM_ID);
        assert!(canonical.is_some());
        assert!(avoid.is_some());
        assert!(!relates_to.is_empty());
        assert!(definition.is_some());
        assert!(decided_at.is_some());

        let legend_text = glossary_schema_legend();
        for spec in TERM_PROPERTIES {
            assert!(
                legend_text.contains(&format!(":{}:", spec.key)),
                "legend missing {}",
                spec.key
            );
        }
    }
}
