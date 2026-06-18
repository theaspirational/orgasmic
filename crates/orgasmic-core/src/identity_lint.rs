//! Post-migration identity lint: duplicate ids, malformed mints, dangling refs,
//! and heading-token vs `:ID:` equality on task/decision/arch nodes.

use std::collections::{BTreeMap, BTreeSet, HashSet};
use std::path::{Path, PathBuf};

use crate::id::is_valid_greenfield_identity;
use crate::iter_task_file_paths;
use crate::org::{Heading, OrgFile};
use crate::schema::{ArchitectureNode, DecisionNode, GlossaryTerm};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IdentityOccurrence {
    pub id: String,
    pub path: PathBuf,
    pub line: Option<usize>,
    pub context: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum IdentityLintKind {
    DuplicateId,
    MalformedIdentity,
    DanglingReference,
    HeadingIdTokenMismatch,
}

/// Return a lint message when the heading title's leading ID token disagrees
/// with (or omits) the drawer `:ID:` for a task-shaped heading.
pub fn lint_task_heading_id_token(heading: &Heading) -> Option<String> {
    let id = heading.property("ID")?;
    if !id.starts_with("TASK-") || heading.todo.is_none() {
        return None;
    }
    heading_id_token_drift_message(heading, id)
}

/// Return a lint message when the heading title's leading ID token disagrees
/// with (or omits) the drawer `:ID:` for a decision node.
pub fn lint_decision_heading_id_token(heading: &Heading) -> Option<String> {
    let id = heading.property("ID")?;
    if !id.starts_with("dec_") {
        return None;
    }
    heading_id_token_drift_message(heading, id)
}

/// Return a lint message when the heading title's leading ID token disagrees
/// with (or omits) the drawer `:ID:` for an architecture node.
pub fn lint_arch_heading_id_token(heading: &Heading) -> Option<String> {
    let id = heading.property("ID")?;
    if !id.starts_with("arch_") {
        return None;
    }
    heading_id_token_drift_message(heading, id)
}

fn heading_id_token_drift_message(heading: &Heading, id: &str) -> Option<String> {
    let leading = heading.title.split_whitespace().next();
    match leading {
        Some(token) if token == id => None,
        Some(token) => Some(format!(
            "heading-token/:ID: mismatch: heading title leading token `{token}` \
             disagrees with :ID: `{id}` (heading '{}')",
            heading.title
        )),
        None => Some(format!(
            "heading-token/:ID: mismatch: heading title omits leading ID token \
             (expected `{id}` as first token; heading '{}')",
            heading.title
        )),
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IdentityLintFinding {
    pub kind: IdentityLintKind,
    pub message: String,
    pub path: PathBuf,
    pub line: Option<usize>,
}

fn read_org(path: &Path) -> Option<OrgFile> {
    let contents = std::fs::read_to_string(path).ok()?;
    OrgFile::parse(&contents, path.to_string_lossy().into_owned()).ok()
}

fn tokenize_property(value: Option<&str>) -> Vec<String> {
    value
        .map(|raw| {
            raw.split_whitespace()
                .map(str::trim)
                .filter(|token| !token.is_empty())
                .map(str::to_string)
                .collect()
        })
        .unwrap_or_default()
}

fn push_identity(
    out: &mut Vec<IdentityOccurrence>,
    id: &str,
    path: &Path,
    line: Option<usize>,
    context: &str,
) {
    if id.trim().is_empty() {
        return;
    }
    out.push(IdentityOccurrence {
        id: id.trim().to_string(),
        path: path.to_path_buf(),
        line,
        context: context.to_string(),
    });
}

fn collect_task_identities(path: &Path, file: &OrgFile, out: &mut Vec<IdentityOccurrence>) {
    for heading in &file.headings {
        let Some(id) = heading.property("ID") else {
            continue;
        };
        push_identity(
            out,
            id,
            path,
            None,
            &format!("task :ID: in heading {}", heading.title),
        );
    }
}

fn collect_decision_identities(path: &Path, file: &OrgFile, out: &mut Vec<IdentityOccurrence>) {
    for heading in &file.headings {
        if !heading.title.starts_with("dec_") {
            continue;
        }
        let Ok(node) = DecisionNode::from_heading(file, heading, &path.to_string_lossy()) else {
            continue;
        };
        push_identity(
            out,
            node.id,
            path,
            None,
            &format!("decision :ID: in heading {}", heading.title),
        );
    }
}

fn collect_architecture_identities(path: &Path, file: &OrgFile, out: &mut Vec<IdentityOccurrence>) {
    let Ok(nodes) = ArchitectureNode::from_org(file, &path.to_string_lossy()) else {
        return;
    };
    for node in nodes {
        push_identity(
            out,
            node.id,
            path,
            None,
            &format!("architecture node {}", node.id),
        );
    }
}

fn collect_glossary_identities(path: &Path, file: &OrgFile, out: &mut Vec<IdentityOccurrence>) {
    for heading in &file.headings {
        if heading.property("ID").is_none() {
            continue;
        }
        let Ok(term) = GlossaryTerm::from_heading(heading, &path.to_string_lossy()) else {
            continue;
        };
        push_identity(
            out,
            term.id,
            path,
            None,
            &format!("glossary :ID: in heading {}", heading.title),
        );
    }
}

fn collect_dangling_reference_tokens(
    path: &Path,
    file: &OrgFile,
    out: &mut Vec<(String, PathBuf, Option<usize>, String)>,
) {
    for heading in &file.headings {
        let owner = heading
            .property("ID")
            .or_else(|| heading.title.split_whitespace().next());
        let Some(owner) = owner else {
            continue;
        };
        for key in ["RELATES_TO", "GLOSSARY_REFS"] {
            for token in tokenize_property(heading.property(key)) {
                out.push((
                    token,
                    path.to_path_buf(),
                    None,
                    format!(":{key}: on {owner}"),
                ));
            }
        }
    }
}

/// Collect every node identity position under `.orgasmic/`.
pub fn collect_identity_occurrences(project_root: &Path) -> Vec<IdentityOccurrence> {
    let orgasmic = project_root.join(".orgasmic");
    let mut out = Vec::new();
    for path in iter_task_file_paths(project_root) {
        if !path.exists() {
            continue;
        }
        let Some(file) = read_org(&path) else {
            continue;
        };
        collect_task_identities(&path, &file, &mut out);
    }
    for path in [
        orgasmic.join("decisions.org"),
        orgasmic.join("architecture.org"),
        orgasmic.join("glossary.org"),
    ] {
        if !path.exists() {
            continue;
        }
        let Some(file) = read_org(&path) else {
            continue;
        };
        if path.ends_with("decisions.org") {
            collect_decision_identities(&path, &file, &mut out);
        } else if path.ends_with("architecture.org") {
            collect_architecture_identities(&path, &file, &mut out);
        } else {
            collect_glossary_identities(&path, &file, &mut out);
        }
    }
    out
}

/// Collect `:RELATES_TO:` / `:GLOSSARY_REFS:` / task drawer reference tokens.
pub fn collect_reference_occurrences(
    project_root: &Path,
) -> Vec<(String, PathBuf, Option<usize>, String)> {
    let orgasmic = project_root.join(".orgasmic");
    let mut out = Vec::new();
    for path in iter_task_file_paths(project_root) {
        if !path.exists() {
            continue;
        }
        if let Some(file) = read_org(&path) {
            collect_dangling_reference_tokens(&path, &file, &mut out);
        }
    }
    for rel in ["decisions.org", "architecture.org", "glossary.org"] {
        let path = orgasmic.join(rel);
        if let Some(file) = read_org(&path) {
            collect_dangling_reference_tokens(&path, &file, &mut out);
        }
    }
    out
}

pub fn lint_identity_occurrences(occurrences: &[IdentityOccurrence]) -> Vec<IdentityLintFinding> {
    let expects_greenfield = occurrences
        .iter()
        .any(|occ| is_valid_greenfield_identity(&occ.id));
    let mut by_id: BTreeMap<String, Vec<&IdentityOccurrence>> = BTreeMap::new();
    for occ in occurrences {
        by_id.entry(occ.id.clone()).or_default().push(occ);
    }

    let mut findings = Vec::new();
    for (id, occs) in &by_id {
        if expects_greenfield && !is_valid_greenfield_identity(id) {
            for occ in occs {
                findings.push(IdentityLintFinding {
                    kind: IdentityLintKind::MalformedIdentity,
                    message: format!("malformed identity id `{id}` ({})", occ.context),
                    path: occ.path.clone(),
                    line: occ.line,
                });
            }
        }
        if occs.len() > 1 {
            for occ in occs {
                let other = occs
                    .iter()
                    .find(|other| other.path != occ.path || other.line != occ.line)
                    .map(|other| format!("{}:{}", other.path.display(), other.line.unwrap_or(0)))
                    .unwrap_or_else(|| "?".into());
                findings.push(IdentityLintFinding {
                    kind: IdentityLintKind::DuplicateId,
                    message: format!(
                        "duplicate identity id `{id}` also at {other} ({})",
                        occ.context
                    ),
                    path: occ.path.clone(),
                    line: occ.line,
                });
            }
        }
    }
    findings
}

pub fn lint_dangling_references(
    known_ids: &BTreeSet<String>,
    references: &[(String, PathBuf, Option<usize>, String)],
    marker_ids: &BTreeMap<String, Vec<PathBuf>>,
) -> Vec<IdentityLintFinding> {
    let mut findings = Vec::new();
    for (token, path, line, context) in references {
        if known_ids.contains(token) {
            continue;
        }
        findings.push(IdentityLintFinding {
            kind: IdentityLintKind::DanglingReference,
            message: format!("dangling reference `{token}` ({context})"),
            path: path.clone(),
            line: *line,
        });
    }
    for (marker_id, files) in marker_ids {
        if known_ids.contains(marker_id) {
            continue;
        }
        for file in files {
            findings.push(IdentityLintFinding {
                kind: IdentityLintKind::DanglingReference,
                message: format!(
                    "dangling advisory marker `{marker_id}` in {}",
                    file.display()
                ),
                path: project_relative_or_abs(file),
                line: None,
            });
        }
    }
    findings
}

fn project_relative_or_abs(path: &Path) -> PathBuf {
    path.to_path_buf()
}

pub fn known_ids_from_occurrences(occurrences: &[IdentityOccurrence]) -> BTreeSet<String> {
    occurrences.iter().map(|occ| occ.id.clone()).collect()
}

/// Scan a project tree for identity lint findings.
pub fn lint_project_identities(
    project_root: &Path,
    marker_ids: &BTreeMap<String, Vec<PathBuf>>,
) -> Vec<IdentityLintFinding> {
    let occurrences = collect_identity_occurrences(project_root);
    let mut findings = lint_identity_occurrences(&occurrences);
    let known = known_ids_from_occurrences(&occurrences);
    let references = collect_reference_occurrences(project_root);
    findings.extend(lint_dangling_references(&known, &references, marker_ids));
    findings
}

/// Paths (relative to project root) that hold org identities or references.
pub fn org_state_rel_paths(project_root: &Path) -> Vec<PathBuf> {
    let mut paths = iter_task_file_paths(project_root)
        .filter(|path| path.exists())
        .map(|path| {
            path.strip_prefix(project_root)
                .unwrap_or(&path)
                .to_path_buf()
        })
        .collect::<Vec<_>>();
    for rel in [
        ".orgasmic/decisions.org",
        ".orgasmic/architecture.org",
        ".orgasmic/glossary.org",
    ] {
        let path = project_root.join(rel);
        if path.exists() {
            paths.push(PathBuf::from(rel));
        }
    }
    paths
}

/// Collect duplicate id groups keyed by the shared id.
pub fn duplicate_id_groups(
    occurrences: &[IdentityOccurrence],
) -> BTreeMap<String, Vec<IdentityOccurrence>> {
    let mut by_id: BTreeMap<String, Vec<IdentityOccurrence>> = BTreeMap::new();
    for occ in occurrences {
        by_id.entry(occ.id.clone()).or_default().push(occ.clone());
    }
    by_id
        .into_iter()
        .filter(|(_, occs)| occs.len() > 1)
        .collect()
}

/// Return paths introduced on `incoming_ref` relative to `base_ref` for org-state files.
pub fn paths_introduced_by_ref(
    project_root: &Path,
    base_ref: &str,
    incoming_ref: &str,
) -> Result<HashSet<PathBuf>, String> {
    let output = std::process::Command::new("git")
        .arg("-C")
        .arg(project_root)
        .args(["diff", "--name-only", base_ref, incoming_ref, "--"])
        .output()
        .map_err(|e| e.to_string())?;
    if !output.status.success() {
        return Err(String::from_utf8_lossy(&output.stderr).into_owned());
    }
    let mut paths = HashSet::new();
    for line in String::from_utf8_lossy(&output.stdout).lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        paths.insert(PathBuf::from(line));
    }
    Ok(paths)
}

/// Pick the incoming duplicate occurrence using git path attribution.
pub fn select_incoming_occurrence<'a>(
    occs: &'a [IdentityOccurrence],
    introduced_paths: &HashSet<PathBuf>,
) -> Option<&'a IdentityOccurrence> {
    let introduced: Vec<&IdentityOccurrence> = occs
        .iter()
        .filter(|occ| {
            introduced_paths.iter().any(|rel| {
                occ.path.ends_with(rel)
                    || occ
                        .path
                        .strip_prefix(occ.path.parent().unwrap_or(Path::new("")))
                        .map(|p| p.ends_with(rel))
                        .unwrap_or(false)
                    || occ
                        .path
                        .to_string_lossy()
                        .ends_with(rel.to_string_lossy().as_ref())
            })
        })
        .collect();
    // Exactly one occurrence attributable to the incoming ref is the only
    // non-guess outcome; zero attributable means the duplicate pre-exists the
    // range and the caller must refuse rather than pick a side.
    if introduced.len() == 1 {
        return Some(introduced[0]);
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::id::mint_node_id;
    use crate::org::OrgFile;
    use crate::NodeIdClass;
    use std::fs;

    fn write(path: &Path, contents: &str) {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).unwrap();
        }
        fs::write(path, contents).unwrap();
    }

    #[test]
    fn heading_id_token_lint_flags_task_mismatch_and_missing_token() {
        let source = "#+title: sprint\n\n\
            * BACKLOG TASK-WRONG Mismatch\n:PROPERTIES:\n:ID: TASK-RIGHT\n:END:\n\n\
            * BACKLOG Missing token only\n:PROPERTIES:\n:ID: TASK-MISS\n:END:\n\n\
            * BACKLOG TASK-OK Agrees\n:PROPERTIES:\n:ID: TASK-OK\n:END:\n";
        let file = OrgFile::parse(source, "inline.org").unwrap();
        let mismatch = file.find_by_id("TASK-RIGHT").unwrap();
        let missing = file.find_by_id("TASK-MISS").unwrap();
        let ok = file.find_by_id("TASK-OK").unwrap();
        assert!(lint_task_heading_id_token(mismatch).is_some());
        assert!(lint_task_heading_id_token(missing).is_some());
        assert!(lint_task_heading_id_token(ok).is_none());
    }

    #[test]
    fn heading_id_token_lint_flags_decision_and_arch_mismatch() {
        let decisions = "#+title: decisions\n\n\
            * dec_WRONG Decision\n:PROPERTIES:\n:ID: dec_RIGHT\n:END:\n";
        let file = OrgFile::parse(decisions, "decisions.org").unwrap();
        assert!(lint_decision_heading_id_token(&file.headings[0]).is_some());

        let arch = "#+title: architecture\n\n\
            * arch_WRONG Label\n:PROPERTIES:\n:ID: arch_RIGHT\n:END:\n\n\
            ** arch_CHILD.1 Child\n:PROPERTIES:\n:ID: arch_PARENT.1\n:END:\n";
        let file = OrgFile::parse(arch, "architecture.org").unwrap();
        assert!(lint_arch_heading_id_token(&file.headings[0]).is_some());
        let child = file.headings[0].sections.first().expect("child arch");
        assert!(lint_arch_heading_id_token(child).is_some());
    }

    #[test]
    fn glossary_slug_title_is_not_subject_to_heading_id_token_lint() {
        let source = "#+title: glossary\n\n\
            * human-readable slug\n:PROPERTIES:\n:ID: term_ABCDE\n:END:\n";
        let file = OrgFile::parse(source, "glossary.org").unwrap();
        let heading = &file.headings[0];
        assert_eq!(heading.title, "human-readable slug");
        assert!(lint_task_heading_id_token(heading).is_none());
        assert!(lint_decision_heading_id_token(heading).is_none());
        assert!(lint_arch_heading_id_token(heading).is_none());
    }

    #[test]
    fn malformed_only_when_project_has_minted_identities() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        write(
            &root.join(".orgasmic/tasks/backlog.org"),
            "#+title: sprint\n\n* BACKLOG TASK-001 Legacy-only\n:PROPERTIES:\n:ID: TASK-001\n:END:\n",
        );
        let findings = lint_project_identities(root, &BTreeMap::new());
        assert!(
            !findings
                .iter()
                .any(|f| matches!(f.kind, IdentityLintKind::MalformedIdentity)),
            "legacy-only fixtures must not trigger malformed lint: {findings:?}"
        );
    }

    #[test]
    fn duplicate_and_malformed_identities_are_flagged() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        let dup = mint_node_id(NodeIdClass::Task);
        write(
            &root.join(".orgasmic/tasks/done.org"),
            &format!("#+title: done\n\n* DONE {dup} One\n:PROPERTIES:\n:ID: {dup}\n:END:\n"),
        );
        write(
            &root.join(".orgasmic/tasks/backlog.org"),
            &format!(
                "#+title: backlog\n\n* BACKLOG {dup} Two\n:PROPERTIES:\n:ID: {dup}\n:END:\n\n* BACKLOG TASK-001 Legacy\n:PROPERTIES:\n:ID: TASK-001\n:END:\n"
            ),
        );
        let anchor = mint_node_id(NodeIdClass::Decision);
        write(
            &root.join(".orgasmic/decisions.org"),
            &format!(
                "#+title: decisions\n\n* {anchor} Anchor\n:PROPERTIES:\n:ID: {anchor}\n:END:\n"
            ),
        );
        let findings = lint_project_identities(root, &BTreeMap::new());
        assert!(
            findings.iter().any(
                |f| matches!(f.kind, IdentityLintKind::DuplicateId) && f.message.contains(&dup)
            ),
            "{findings:?}"
        );
        assert!(
            findings
                .iter()
                .any(|f| matches!(f.kind, IdentityLintKind::MalformedIdentity)
                    && f.message.contains("TASK-001")),
            "{findings:?}"
        );
    }

    #[test]
    fn dangling_reference_is_flagged() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        let id = mint_node_id(NodeIdClass::Term);
        write(
            &root.join(".orgasmic/glossary.org"),
            &format!(
                "#+title: glossary\n\n* {id} Example\n:PROPERTIES:\n:ID: {id}\n:RELATES_TO: missing-slug\n:END:\n"
            ),
        );
        let findings = lint_project_identities(root, &BTreeMap::new());
        assert!(
            findings
                .iter()
                .any(|f| matches!(f.kind, IdentityLintKind::DanglingReference)
                    && f.message.contains("missing-slug")),
            "{findings:?}"
        );
    }

    #[test]
    fn preexisting_duplicate_with_no_introduced_paths_is_not_attributed() {
        let occs = vec![
            IdentityOccurrence {
                id: "TASK-AAAAA".into(),
                path: PathBuf::from(".orgasmic/tasks/backlog.org"),
                line: Some(1),
                context: "a".into(),
            },
            IdentityOccurrence {
                id: "TASK-AAAAA".into(),
                path: PathBuf::from(".orgasmic/tasks/backlog.org"),
                line: Some(2),
                context: "b".into(),
            },
        ];
        assert!(select_incoming_occurrence(&occs, &HashSet::new()).is_none());
    }

    #[test]
    fn ambiguous_attribution_returns_none_for_multiple_introduced() {
        let occs = vec![
            IdentityOccurrence {
                id: "TASK-AAAAA".into(),
                path: PathBuf::from(".orgasmic/tasks/backlog.org"),
                line: Some(1),
                context: "a".into(),
            },
            IdentityOccurrence {
                id: "TASK-AAAAA".into(),
                path: PathBuf::from(".orgasmic/tasks/backlog.org"),
                line: Some(2),
                context: "b".into(),
            },
        ];
        let mut introduced = HashSet::new();
        introduced.insert(PathBuf::from(".orgasmic/tasks/backlog.org"));
        introduced.insert(PathBuf::from(".orgasmic/tasks/backlog.org"));
        assert!(select_incoming_occurrence(&occs, &introduced).is_none());
    }

    #[test]
    #[ignore = "manual probe against live checkout; run with --ignored --nocapture"]
    fn real_post_migration_repo_lint_probe() {
        let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
        let findings = lint_project_identities(&root, &BTreeMap::new());
        let malformed: Vec<_> = findings
            .iter()
            .filter(|f| matches!(f.kind, IdentityLintKind::MalformedIdentity))
            .collect();
        assert!(
            malformed.is_empty(),
            "expected zero malformed false positives on real repo, got:\n{}",
            malformed
                .iter()
                .map(|f| f.message.as_str())
                .collect::<Vec<_>>()
                .join("\n")
        );
        let dupes: Vec<_> = findings
            .iter()
            .filter(|f| matches!(f.kind, IdentityLintKind::DuplicateId))
            .collect();
        let dangles: Vec<_> = findings
            .iter()
            .filter(|f| matches!(f.kind, IdentityLintKind::DanglingReference))
            .collect();
        eprintln!("duplicate findings: {}", dupes.len());
        eprintln!("dangling reference findings: {}", dangles.len());
        for f in dupes.iter().take(5) {
            eprintln!("  duplicate: {}", f.message);
        }
        for f in dangles.iter().take(20) {
            eprintln!("  dangling: {}", f.message);
        }
    }
}
