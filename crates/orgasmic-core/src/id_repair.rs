//! Mechanical re-mint repair for duplicate identity collisions.

use std::collections::{BTreeSet, HashMap, HashSet};
use std::path::{Path, PathBuf};

use crate::id::{mint_node_id, NodeIdClass};
use crate::identity_lint::{
    collect_identity_occurrences, duplicate_id_groups, paths_introduced_by_ref,
    select_incoming_occurrence, IdentityOccurrence,
};
use crate::marker::should_skip_marker_path;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IdRepairMapping {
    pub old_id: String,
    pub new_id: String,
    pub incoming_path: PathBuf,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum IdRepairError {
    NoDuplicates,
    AmbiguousAttribution { id: String, detail: String },
    Git(String),
    Io { path: PathBuf, detail: String },
}

impl std::fmt::Display for IdRepairError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NoDuplicates => write!(f, "no duplicate identity ids found"),
            Self::AmbiguousAttribution { id, detail } => {
                write!(f, "ambiguous attribution for duplicate `{id}`: {detail}")
            }
            Self::Git(msg) => write!(f, "git attribution failed: {msg}"),
            Self::Io { path, detail } => {
                write!(f, "{}: {detail}", path.display())
            }
        }
    }
}

impl std::error::Error for IdRepairError {}

fn node_class_for_id(id: &str) -> Option<NodeIdClass> {
    if id.starts_with("TASK-") {
        Some(NodeIdClass::Task)
    } else if id.starts_with("dec_") {
        Some(NodeIdClass::Decision)
    } else if id.starts_with("arch_") {
        Some(NodeIdClass::Architecture)
    } else if id.starts_with("term_") {
        Some(NodeIdClass::Term)
    } else {
        None
    }
}

fn mint_replacement(existing: &BTreeSet<String>, id: &str) -> String {
    let class = node_class_for_id(id).unwrap_or(NodeIdClass::Task);
    loop {
        let candidate = mint_node_id(class);
        if !existing.contains(&candidate) {
            return candidate;
        }
    }
}

fn is_boundary_char(c: char, before: bool) -> bool {
    if before {
        !c.is_ascii_alphanumeric() && c != '-' && c != '_'
    } else {
        !c.is_ascii_alphanumeric() && c != '-'
    }
}

fn replace_id_token(text: &str, old: &str, new: &str) -> (String, usize) {
    let mut out = String::new();
    let mut count = 0;
    let mut rest = text;
    while let Some(idx) = rest.find(old) {
        let (before, after) = rest.split_at(idx);
        let after_match = &after[old.len()..];
        let left_ok = before
            .chars()
            .last()
            .map(|c| is_boundary_char(c, true))
            .unwrap_or(true);
        // `.` is a boundary unless it opens a subtask/sub-node suffix
        // (`.<digit>`), so sentence-final refs like "blocks TASK-XXXXX." rewrite.
        let mut after_chars = after_match.chars();
        let right_ok = match after_chars.next() {
            None => true,
            Some('.') => !after_chars.next().is_some_and(|c| c.is_ascii_digit()),
            Some(c) => is_boundary_char(c, false),
        };
        out.push_str(before);
        if left_ok && right_ok {
            out.push_str(new);
            count += 1;
            rest = after_match;
        } else {
            out.push_str(&after[..old.len()]);
            rest = &after[old.len()..];
        }
    }
    out.push_str(rest);
    (out, count)
}

fn rewrite_text(text: &str, mappings: &[(String, String)]) -> (String, usize) {
    if mappings.is_empty() {
        return (text.to_string(), 0);
    }
    let mut order = mappings.to_vec();
    order.sort_by(|a, b| b.0.len().cmp(&a.0.len()));
    let mut out = text.to_string();
    let mut total = 0;
    for (old, new) in order {
        let (next, count) = replace_id_token(&out, &old, &new);
        out = next;
        total += count;
    }
    (out, total)
}

fn rewrite_marker_payload(payload: &str, mappings: &[(String, String)]) -> (String, usize) {
    let marker_mappings: Vec<(String, String)> = mappings
        .iter()
        .flat_map(|(old, new)| {
            let mut pairs = Vec::new();
            if old.starts_with("TASK-") {
                pairs.push((
                    format!("task_{}", old.strip_prefix("TASK-").unwrap()),
                    format!("task_{}", new.strip_prefix("TASK-").unwrap()),
                ));
            }
            pairs.push((old.clone(), new.clone()));
            pairs
        })
        .collect();
    rewrite_text(payload, &marker_mappings)
}

fn rewrite_file(path: &Path, mappings: &[(String, String)]) -> Result<bool, IdRepairError> {
    let original = std::fs::read_to_string(path).map_err(|e| IdRepairError::Io {
        path: path.to_path_buf(),
        detail: e.to_string(),
    })?;
    let mut changed = false;
    let mut out_lines = Vec::new();
    for line in original.lines() {
        let (mut line_out, n) = rewrite_text(line, mappings);
        if n > 0 {
            changed = true;
        }
        if line_out.contains("orgasmic:") {
            let mut rebuilt = String::new();
            let mut search = 0;
            while let Some(idx) = line_out[search..].find("orgasmic:") {
                let start = search + idx;
                rebuilt.push_str(&line_out[search..start]);
                let payload_start = start + "orgasmic:".len();
                let payload = &line_out[payload_start..];
                let (new_payload, pn) = rewrite_marker_payload(payload, mappings);
                if pn > 0 {
                    changed = true;
                }
                rebuilt.push_str("orgasmic:");
                rebuilt.push_str(&new_payload);
                search = payload_start + payload.len();
            }
            rebuilt.push_str(&line_out[search..]);
            line_out = rebuilt;
        }
        out_lines.push(line_out);
    }
    if changed {
        let mut joined = out_lines.join("\n");
        if original.ends_with('\n') {
            joined.push('\n');
        }
        std::fs::write(path, joined).map_err(|e| IdRepairError::Io {
            path: path.to_path_buf(),
            detail: e.to_string(),
        })?;
    }
    Ok(changed)
}

fn scan_code_marker_files(project_root: &Path) -> Vec<PathBuf> {
    let roots = [
        project_root.join(".orgasmic"),
        project_root.join("shipped"),
        project_root.join("crates"),
        project_root.join("ui/src"),
        project_root.join("scripts"),
    ];
    let mut files = Vec::new();
    for root in roots {
        if !root.is_dir() {
            continue;
        }
        let mut stack = vec![root];
        while let Some(dir) = stack.pop() {
            let Ok(read) = std::fs::read_dir(&dir) else {
                continue;
            };
            for entry in read.flatten() {
                let path = entry.path();
                if path.is_dir() {
                    stack.push(path);
                    continue;
                }
                let rel = path
                    .strip_prefix(project_root)
                    .unwrap_or(&path)
                    .to_string_lossy();
                if should_skip_marker_path(&rel) {
                    continue;
                }
                if matches!(
                    path.extension().and_then(|e| e.to_str()),
                    Some(
                        "rs" | "ts"
                            | "tsx"
                            | "js"
                            | "jsx"
                            | "py"
                            | "sh"
                            | "bash"
                            | "zsh"
                            | "yaml"
                            | "yml"
                            | "toml"
                    )
                ) {
                    files.push(path);
                }
            }
        }
    }
    files
}

/// Apply repair mappings on the incoming side and git-introduced paths only.
pub fn apply_id_collision_repairs(
    project_root: &Path,
    mappings: &[IdRepairMapping],
    extra_rewrite_paths: &HashSet<PathBuf>,
) -> Result<usize, IdRepairError> {
    if mappings.is_empty() {
        return Ok(0);
    }
    let mut changed_files = 0;
    for mapping in mappings {
        let pairs = [(mapping.old_id.clone(), mapping.new_id.clone())];
        let mut targets = HashSet::new();
        targets.insert(mapping.incoming_path.clone());
        for rel in extra_rewrite_paths {
            targets.insert(project_root.join(rel));
        }
        for path in targets {
            if !path.is_file() {
                continue;
            }
            if rewrite_file(&path, &pairs)? {
                changed_files += 1;
            }
        }
        for path in scan_code_marker_files(project_root) {
            let rel = path
                .strip_prefix(project_root)
                .unwrap_or(&path)
                .to_path_buf();
            if (extra_rewrite_paths.contains(&rel) || path == mapping.incoming_path)
                && rewrite_file(&path, &pairs)?
            {
                changed_files += 1;
            }
        }
    }
    Ok(changed_files)
}

/// Repair duplicate ids once; idempotent when no duplicates remain.
pub fn repair_id_collisions(
    project_root: &Path,
    base_ref: &str,
    incoming_ref: &str,
) -> Result<Vec<IdRepairMapping>, IdRepairError> {
    let occurrences = collect_identity_occurrences(project_root);
    let dupes = duplicate_id_groups(&occurrences);
    if dupes.is_empty() {
        return Err(IdRepairError::NoDuplicates);
    }
    let introduced = paths_introduced_by_ref(project_root, base_ref, incoming_ref)
        .map_err(IdRepairError::Git)?;
    let known: BTreeSet<String> = occurrences.iter().map(|occ| occ.id.clone()).collect();
    let mut mappings = Vec::new();
    for (id, occs) in dupes {
        let incoming = select_incoming_occurrence(&occs, &introduced).ok_or_else(|| {
            IdRepairError::AmbiguousAttribution {
                id: id.clone(),
                detail: format!(
                    "could not attribute incoming duplicate among {} occurrences using git diff {base_ref}..{incoming_ref}",
                    occs.len()
                ),
            }
        })?;
        mappings.push(IdRepairMapping {
            old_id: id.clone(),
            new_id: mint_replacement(&known, &id),
            incoming_path: incoming.path.clone(),
        });
    }
    apply_id_collision_repairs(project_root, &mappings, &introduced)?;
    Ok(mappings)
}

/// Test helper: repair with an explicit incoming occurrence per duplicate id.
pub fn repair_id_collisions_with_incoming(
    project_root: &Path,
    incoming_paths: &HashMap<String, PathBuf>,
) -> Result<Vec<IdRepairMapping>, IdRepairError> {
    let occurrences = collect_identity_occurrences(project_root);
    let dupes = duplicate_id_groups(&occurrences);
    if dupes.is_empty() {
        return Err(IdRepairError::NoDuplicates);
    }
    let known: BTreeSet<String> = occurrences.iter().map(|occ| occ.id.clone()).collect();
    let mut mappings = Vec::new();
    for (id, occs) in dupes {
        let incoming_path =
            incoming_paths
                .get(&id)
                .ok_or_else(|| IdRepairError::AmbiguousAttribution {
                    id: id.clone(),
                    detail: "no incoming path provided for duplicate".into(),
                })?;
        let matches: Vec<&IdentityOccurrence> = occs
            .iter()
            .filter(|occ| &occ.path == incoming_path)
            .collect();
        if matches.len() != 1 {
            return Err(IdRepairError::AmbiguousAttribution {
                id: id.clone(),
                detail: format!(
                    "expected exactly one incoming occurrence at {}, found {}",
                    incoming_path.display(),
                    matches.len()
                ),
            });
        }
        mappings.push(IdRepairMapping {
            old_id: id.clone(),
            new_id: mint_replacement(&known, &id),
            incoming_path: incoming_path.clone(),
        });
    }
    apply_id_collision_repairs(project_root, &mappings, &HashSet::new())?;
    Ok(mappings)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::id::mint_node_id;
    use crate::NodeIdClass;
    use std::collections::HashMap;
    use std::fs;

    fn write(path: &Path, contents: &str) {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).unwrap();
        }
        fs::write(path, contents).unwrap();
    }

    #[test]
    fn replace_id_token_boundaries() {
        // Sentence-final ref rewrites; subtask suffix stays protected.
        let (out, n) = replace_id_token("blocks TASK-AAAAA.", "TASK-AAAAA", "TASK-BBBBB");
        assert_eq!(out, "blocks TASK-BBBBB.");
        assert_eq!(n, 1);
        let (out, n) = replace_id_token("see TASK-AAAAA.1 here", "TASK-AAAAA", "TASK-BBBBB");
        assert_eq!(out, "see TASK-AAAAA.1 here");
        assert_eq!(n, 0);
    }

    #[test]
    fn repair_re_mints_incoming_side_and_is_idempotent() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        let dup = mint_node_id(NodeIdClass::Decision);
        let keep_path = root.join(".orgasmic/decisions.org");
        let incoming_path = root.join(".orgasmic/tasks/backlog.org");
        write(
            &keep_path,
            &format!(
                "#+title: decisions\n\n* {dup} Keep\n:PROPERTIES:\n:ID: {dup}\n:END:\n** Decision\nKeep side.\n"
            ),
        );
        write(
            &incoming_path,
            &format!(
                "#+title: backlog\n\n* BACKLOG {dup} Incoming\n:PROPERTIES:\n:ID: {dup}\n:DEPENDS_ON: {dup}\n:END:\n"
            ),
        );
        let mut incoming = HashMap::new();
        incoming.insert(dup.clone(), incoming_path.clone());
        let mappings = repair_id_collisions_with_incoming(root, &incoming).unwrap();
        assert_eq!(mappings.len(), 1);
        assert_ne!(mappings[0].old_id, mappings[0].new_id);
        let keep_text = fs::read_to_string(&keep_path).unwrap();
        let incoming_text = fs::read_to_string(&incoming_path).unwrap();
        assert!(keep_text.contains(&dup));
        assert!(!incoming_text.contains(&format!(":ID: {dup}")));
        assert!(incoming_text.contains(&mappings[0].new_id));
        assert!(repair_id_collisions_with_incoming(root, &incoming).is_err());
    }
}
