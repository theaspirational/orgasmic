// arch: arch_QXS5W.1
// orgasmic:arch_QXS5W
//! Strict slot compiler for prompt templates.
//!
//! Syntax: `{{namespace.field}}`. No conditionals, no loops, no
//! expressions. Whitespace inside the braces is tolerated (`{{ foo.bar }}`).
//! Slots reference the fixed registry defined in
//! [`shipped/prompt-studio/slots.org`](../../../../shipped/prompt-studio/slots.org).
//!
//! A dry-run pass collects two diagnostic categories:
//! - **Unknown slots**: name not registered in the slot registry.
//! - **Unfilled slots**: name is registered but the caller did not provide a
//!   value when compiling.

use std::collections::{BTreeMap, BTreeSet};

use serde::Serialize;
use thiserror::Error;

/// Map of `namespace.field` → string value supplied by the caller.
pub type SlotValues = BTreeMap<String, String>;

#[derive(Debug, Error)]
pub enum SlotError {
    #[error("malformed slot at byte {offset}: {detail}")]
    Malformed { offset: usize, detail: String },
    #[error("compile failed: {unknown:?} unknown, {unfilled:?} unfilled")]
    DryRunFailed {
        unknown: Vec<String>,
        unfilled: Vec<String>,
    },
}

/// Static registry of allowed slot names. Mirrors `shipped/prompt-studio/slots.org`.
pub fn default_registry() -> BTreeSet<&'static str> {
    [
        "task.id",
        "task.title",
        "task.description",
        "task.priority",
        "task.tags",
        "task.acceptance",
        "task.write_scope",
        "task.read_scope",
        "task.depends_on",
        "task.activity",
        "dispatch.brief",
        "project.id",
        "project.name",
        "project.path",
        "project.test_cmd",
        "project.lint_cmd",
        "project.build_cmd",
        "project.default_branch",
        "worker.id",
        "worker.kind",
        "skills.all",
        "run.id",
        "run.iteration_count",
        "run.previous_state",
        "evidence.so_far",
        "worklog.tail",
        "artifact.subject_nodes",
        "artifact.user_prompt",
        "artifact.regen_context",
    ]
    .into_iter()
    .collect()
}

/// True for slot names matching dynamic loader-backed families:
/// `skills.<id>`, `skill_path.<id>`, and `conventions.<name>`, plus the
/// materialized task activity slot.
pub fn is_dynamic_skill_slot(name: &str) -> bool {
    name == "task.activity"
        || matches!(name.split_once('.'), Some(("skills", rest)) | Some(("skill_path", rest)) | Some(("conventions", rest)) if !rest.is_empty())
}

fn is_known_slot(
    name: &str,
    registry: &BTreeSet<&'static str>,
    linked_skills: Option<&BTreeSet<String>>,
) -> bool {
    if registry.contains(name) {
        return true;
    }
    if let Some(linked_skills) = linked_skills {
        return match name.split_once('.') {
            Some(("skills", id)) | Some(("skill_path", id)) => linked_skills.contains(id),
            Some(("conventions", name)) => !name.is_empty(),
            _ => false,
        };
    }
    is_dynamic_skill_slot(name)
}

/// A located slot reference inside a template.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SlotRef {
    /// Slot name, e.g. `task.title`.
    pub name: String,
    /// Byte offset of the opening `{{` in the source template.
    pub start: usize,
    /// Byte offset just past the closing `}}`.
    pub end: usize,
}

/// Scan all slot references in `template`. Errors only on malformed
/// `{{ … }}` syntax; unknown/unfilled diagnostics come from [`dry_run`].
pub fn scan(template: &str) -> Result<Vec<SlotRef>, SlotError> {
    let bytes = template.as_bytes();
    let mut out = Vec::new();
    let mut i = 0;
    while i + 1 < bytes.len() {
        if bytes[i] == b'{' && bytes[i + 1] == b'{' {
            let start = i;
            let mut j = i + 2;
            // Find closing `}}`.
            loop {
                if j + 1 >= bytes.len() {
                    return Err(SlotError::Malformed {
                        offset: start,
                        detail: "unterminated `{{`".into(),
                    });
                }
                if bytes[j] == b'}' && bytes[j + 1] == b'}' {
                    break;
                }
                if bytes[j] == b'\n' {
                    return Err(SlotError::Malformed {
                        offset: start,
                        detail: "newline inside slot reference".into(),
                    });
                }
                j += 1;
            }
            let inside = template[start + 2..j].trim();
            if inside.is_empty() {
                return Err(SlotError::Malformed {
                    offset: start,
                    detail: "empty slot reference".into(),
                });
            }
            if !is_valid_name(inside) {
                return Err(SlotError::Malformed {
                    offset: start,
                    detail: format!("invalid slot name `{inside}`"),
                });
            }
            // Reject any obvious control syntax to keep the profile strict.
            if inside.contains('#') || inside.contains(' ') || inside.contains('|') {
                return Err(SlotError::Malformed {
                    offset: start,
                    detail: format!("control syntax not allowed in slot `{inside}`"),
                });
            }
            out.push(SlotRef {
                name: inside.to_string(),
                start,
                end: j + 2,
            });
            i = j + 2;
        } else {
            i += 1;
        }
    }
    Ok(out)
}

fn is_valid_name(s: &str) -> bool {
    // Slot names are `namespace.field` where namespace is `[A-Za-z_][A-Za-z0-9_]*`
    // and field is `[A-Za-z0-9_-]+` (skill IDs use kebab-case, e.g. `skills.my-skill`).
    let Some((ns, field)) = s.split_once('.') else {
        return false;
    };
    if ns.is_empty() || field.is_empty() {
        return false;
    }
    let mut chars = ns.chars();
    let Some(first) = chars.next() else {
        return false;
    };
    if !(first.is_ascii_alphabetic() || first == '_') {
        return false;
    }
    if !chars.all(|c| c.is_ascii_alphanumeric() || c == '_') {
        return false;
    }
    if !field
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
    {
        return false;
    }
    true
}

/// Dry-run report: every slot reference categorized as known+filled, known
/// but unfilled, or unknown.
#[derive(Debug, Clone, Default, Serialize)]
pub struct DryRunReport {
    pub filled: Vec<String>,
    pub unfilled: Vec<String>,
    pub unknown: Vec<String>,
}

impl DryRunReport {
    pub fn ok(&self) -> bool {
        self.unfilled.is_empty() && self.unknown.is_empty()
    }
}

/// Run the slot scan and classify each reference against the registry +
/// supplied values. Result is informational only; use [`compile`] to also
/// produce the substituted text.
pub fn dry_run(
    template: &str,
    values: &SlotValues,
    registry: &BTreeSet<&'static str>,
) -> Result<DryRunReport, SlotError> {
    dry_run_inner(template, values, registry, None)
}

/// Run a dry-run with a template-scoped linked-skill allowlist.
///
/// `skills.<id>` and `skill_path.<id>` references are known only when `<id>`
/// is present in `linked_skills`. `skills.all` remains a static registry slot.
pub fn dry_run_with_linked_skills(
    template: &str,
    values: &SlotValues,
    registry: &BTreeSet<&'static str>,
    linked_skills: &BTreeSet<String>,
) -> Result<DryRunReport, SlotError> {
    dry_run_inner(template, values, registry, Some(linked_skills))
}

fn dry_run_inner(
    template: &str,
    values: &SlotValues,
    registry: &BTreeSet<&'static str>,
    linked_skills: Option<&BTreeSet<String>>,
) -> Result<DryRunReport, SlotError> {
    let refs = scan(template)?;
    let mut seen_unknown: BTreeSet<String> = BTreeSet::new();
    let mut seen_unfilled: BTreeSet<String> = BTreeSet::new();
    let mut seen_filled: BTreeSet<String> = BTreeSet::new();
    for r in refs {
        let known = is_known_slot(&r.name, registry, linked_skills);
        let filled = values.contains_key(&r.name);
        if !known {
            seen_unknown.insert(r.name);
            continue;
        }
        if filled {
            seen_filled.insert(r.name);
        } else {
            seen_unfilled.insert(r.name);
        }
    }
    Ok(DryRunReport {
        filled: seen_filled.into_iter().collect(),
        unfilled: seen_unfilled.into_iter().collect(),
        unknown: seen_unknown.into_iter().collect(),
    })
}

/// Compile a template by substituting every `{{ name }}` reference with the
/// value supplied in `values`. Returns an error if any reference is unknown
/// or unfilled. Strict: no fallback to empty strings.
pub fn compile(
    template: &str,
    values: &SlotValues,
    registry: &BTreeSet<&'static str>,
) -> Result<String, SlotError> {
    let report = dry_run(template, values, registry)?;
    if !report.ok() {
        return Err(SlotError::DryRunFailed {
            unknown: report.unknown,
            unfilled: report.unfilled,
        });
    }
    let refs = scan(template)?;
    let mut out = String::with_capacity(template.len());
    let mut cursor = 0;
    for r in refs {
        out.push_str(&template[cursor..r.start]);
        let value = values.get(&r.name).expect("dry_run validated presence");
        out.push_str(value);
        cursor = r.end;
    }
    out.push_str(&template[cursor..]);
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn registry() -> BTreeSet<&'static str> {
        default_registry()
    }

    #[test]
    fn scans_all_slot_references() {
        let tpl = "Hello {{task.title}} for {{ project.name }}";
        let refs = scan(tpl).unwrap();
        assert_eq!(refs.len(), 2);
        assert_eq!(refs[0].name, "task.title");
        assert_eq!(refs[1].name, "project.name");
    }

    #[test]
    fn rejects_conditionals_and_loops() {
        let err = scan("{{#if foo}}").unwrap_err();
        match err {
            SlotError::Malformed { .. } => {}
            o => panic!("{o:?}"),
        }
        let err = scan("{{foo bar}}").unwrap_err();
        match err {
            SlotError::Malformed { .. } => {}
            o => panic!("{o:?}"),
        }
    }

    #[test]
    fn rejects_unterminated_slot() {
        let err = scan("{{task.title").unwrap_err();
        matches!(err, SlotError::Malformed { .. });
    }

    #[test]
    fn dry_run_reports_unknown_and_unfilled() {
        let tpl = "{{task.title}} {{nonsense.value}} {{project.name}}";
        let mut values = SlotValues::new();
        values.insert("task.title".into(), "Implement core".into());
        let report = dry_run(tpl, &values, &registry()).unwrap();
        assert_eq!(report.filled, vec!["task.title".to_string()]);
        assert_eq!(report.unfilled, vec!["project.name".to_string()]);
        assert_eq!(report.unknown, vec!["nonsense.value".to_string()]);
        assert!(!report.ok());
    }

    #[test]
    fn compile_substitutes_values() {
        let tpl = "Task {{task.id}} at {{project.path}} runs {{project.test_cmd}}";
        let mut values = SlotValues::new();
        values.insert("task.id".into(), "TASK-003".into());
        values.insert("project.path".into(), "/repo".into());
        values.insert("project.test_cmd".into(), "cargo test".into());
        let out = compile(tpl, &values, &registry()).unwrap();
        assert_eq!(out, "Task TASK-003 at /repo runs cargo test");
    }

    #[test]
    fn compile_fails_on_unknown() {
        let tpl = "{{nope.x}}";
        let err = compile(tpl, &SlotValues::new(), &registry()).unwrap_err();
        match err {
            SlotError::DryRunFailed { unknown, unfilled } => {
                assert_eq!(unknown, vec!["nope.x".to_string()]);
                assert!(unfilled.is_empty());
            }
            o => panic!("{o:?}"),
        }
    }

    #[test]
    fn dynamic_skill_slots_are_known() {
        let tpl = "Use {{skills.my-skill}} body, {{skill_path.my-skill}} path, and {{conventions.code_markers}} rules";
        let mut values = SlotValues::new();
        values.insert("skills.my-skill".into(), "BODY".into());
        values.insert("skill_path.my-skill".into(), "/a/b".into());
        values.insert("conventions.code_markers".into(), "RULES".into());
        let out = compile(tpl, &values, &registry()).unwrap();
        assert_eq!(out, "Use BODY body, /a/b path, and RULES rules");
    }

    #[test]
    fn scoped_skill_slots_accept_manifest_body_and_path() {
        let tpl = "{{skills.all}}\n{{skills.alpha}}\n{{skill_path.alpha}}";
        let mut values = SlotValues::new();
        values.insert("skills.all".into(), "alpha - manifest".into());
        values.insert("skills.alpha".into(), "BODY".into());
        values.insert("skill_path.alpha".into(), "/skills/alpha.org".into());
        let linked_skills = BTreeSet::from(["alpha".to_string()]);

        let report = dry_run_with_linked_skills(tpl, &values, &registry(), &linked_skills).unwrap();

        assert!(report.ok(), "{report:?}");
        assert_eq!(
            report.filled,
            vec![
                "skill_path.alpha".to_string(),
                "skills.all".to_string(),
                "skills.alpha".to_string(),
            ]
        );
    }

    #[test]
    fn scoped_skill_slots_reject_unlinked_body_and_path() {
        let tpl = "{{skills.beta}}\n{{skill_path.beta}}\n{{skills.all}}";
        let mut values = SlotValues::new();
        values.insert("skills.all".into(), "alpha - manifest".into());
        values.insert("skills.beta".into(), "BODY".into());
        values.insert("skill_path.beta".into(), "/skills/beta.org".into());
        let linked_skills = BTreeSet::from(["alpha".to_string()]);

        let report = dry_run_with_linked_skills(tpl, &values, &registry(), &linked_skills).unwrap();

        assert!(!report.ok());
        assert_eq!(report.filled, vec!["skills.all".to_string()]);
        assert_eq!(
            report.unknown,
            vec!["skill_path.beta".to_string(), "skills.beta".to_string()]
        );
        assert!(report.unfilled.is_empty());
    }
}
