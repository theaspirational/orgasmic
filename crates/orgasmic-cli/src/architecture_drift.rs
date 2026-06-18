use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use orgasmic_core::{ArchitectureNode, Heading, OrgFile};
use serde::Serialize;

#[derive(Debug, Serialize)]
pub struct DriftReport {
    pub root: String,
    pub missing_source_paths: Vec<MissingSourcePath>,
    pub paths_missing_markers: Vec<PathMissingMarker>,
    pub markers_without_leaf_paths: Vec<MarkerWithoutLeafPath>,
    pub markers_with_unknown_leaf_ids: Vec<MarkerWithUnknownLeafId>,
}

impl DriftReport {
    pub fn has_drift(&self) -> bool {
        !self.missing_source_paths.is_empty()
            || !self.paths_missing_markers.is_empty()
            || !self.markers_without_leaf_paths.is_empty()
            || !self.markers_with_unknown_leaf_ids.is_empty()
    }

    pub fn drift_count(&self) -> usize {
        self.missing_source_paths.len()
            + self.paths_missing_markers.len()
            + self.markers_without_leaf_paths.len()
            + self.markers_with_unknown_leaf_ids.len()
    }
}

#[derive(Debug, Serialize)]
pub struct MissingSourcePath {
    pub leaf_id: String,
    pub path: String,
}

#[derive(Debug, Serialize)]
pub struct PathMissingMarker {
    pub leaf_id: String,
    pub path: String,
}

#[derive(Debug, Serialize)]
pub struct MarkerWithoutLeafPath {
    pub leaf_id: String,
    pub path: String,
}

#[derive(Debug, Serialize)]
pub struct MarkerWithUnknownLeafId {
    pub leaf_id: String,
    pub path: String,
}

#[derive(Debug)]
struct LeafSourcePaths {
    leaf_paths: BTreeMap<String, BTreeSet<String>>,
    path_leaves: BTreeMap<String, BTreeSet<String>>,
    all_leaf_ids: BTreeSet<String>,
}

pub fn repo_root_from(start: &Path) -> Result<PathBuf> {
    let mut cursor = start
        .canonicalize()
        .with_context(|| format!("canonicalize {}", start.display()))?;
    loop {
        if cursor.join(".orgasmic/architecture.org").is_file() {
            return Ok(cursor);
        }
        if !cursor.pop() {
            anyhow::bail!(
                "could not find .orgasmic/architecture.org from {}",
                start.display()
            );
        }
    }
}

pub fn run(root: &Path) -> Result<DriftReport> {
    let root = root
        .canonicalize()
        .with_context(|| format!("canonicalize {}", root.display()))?;
    let leaves = load_leaf_source_paths(&root)?;
    let markers = collect_arch_markers(&root)?;
    Ok(compare(root, leaves, markers))
}

fn load_leaf_source_paths(root: &Path) -> Result<LeafSourcePaths> {
    let arch_path = root.join(".orgasmic/architecture.org");
    let source =
        fs::read_to_string(&arch_path).with_context(|| format!("read {}", arch_path.display()))?;
    let file = OrgFile::parse(source, arch_path.to_string_lossy())?;
    let nodes = ArchitectureNode::from_org(&file, &arch_path.to_string_lossy())?;
    let mut leaf_paths: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
    let mut path_leaves: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
    let mut all_leaf_ids = BTreeSet::new();

    for node in nodes {
        if node.parent_id.is_none() {
            continue;
        }
        all_leaf_ids.insert(node.id.to_string());
        if node.source_paths.is_empty() || is_planned_or_target(&file, node.id) {
            continue;
        }
        for path in node.source_paths {
            let normalized = normalize_rel_path(path);
            leaf_paths
                .entry(node.id.to_string())
                .or_default()
                .insert(normalized.clone());
            path_leaves
                .entry(normalized)
                .or_default()
                .insert(node.id.to_string());
        }
    }

    Ok(LeafSourcePaths {
        leaf_paths,
        path_leaves,
        all_leaf_ids,
    })
}

fn is_planned_or_target(file: &OrgFile, id: &str) -> bool {
    let Some(heading) = find_heading_by_id(file, id) else {
        return false;
    };
    matches!(heading.property("STATUS"), Some("planned"))
        || matches!(heading.property("V0_0_1"), Some("target"))
}

fn find_heading_by_id<'a>(file: &'a OrgFile, id: &str) -> Option<&'a Heading> {
    file.headings
        .iter()
        .find_map(|heading| heading.find_by_id(id))
}

fn compare(
    root: PathBuf,
    leaves: LeafSourcePaths,
    markers: BTreeMap<String, BTreeSet<String>>,
) -> DriftReport {
    let mut missing_source_paths = Vec::new();
    let mut paths_missing_markers = Vec::new();
    let mut markers_without_leaf_paths = Vec::new();
    let mut markers_with_unknown_leaf_ids = Vec::new();

    for (leaf_id, paths) in &leaves.leaf_paths {
        for path in paths {
            let disk_path = root.join(path);
            if !disk_path.is_file() {
                missing_source_paths.push(MissingSourcePath {
                    leaf_id: leaf_id.clone(),
                    path: path.clone(),
                });
                continue;
            }
            if !markers
                .get(path)
                .map(|ids| ids.contains(leaf_id))
                .unwrap_or(false)
            {
                paths_missing_markers.push(PathMissingMarker {
                    leaf_id: leaf_id.clone(),
                    path: path.clone(),
                });
            }
        }
    }

    for (path, marker_ids) in &markers {
        for leaf_id in marker_ids {
            if !leaves.all_leaf_ids.contains(leaf_id) {
                markers_with_unknown_leaf_ids.push(MarkerWithUnknownLeafId {
                    leaf_id: leaf_id.clone(),
                    path: path.clone(),
                });
                continue;
            }
            if !leaves
                .path_leaves
                .get(path)
                .map(|ids| ids.contains(leaf_id))
                .unwrap_or(false)
            {
                markers_without_leaf_paths.push(MarkerWithoutLeafPath {
                    leaf_id: leaf_id.clone(),
                    path: path.clone(),
                });
            }
        }
    }

    DriftReport {
        root: root.to_string_lossy().into_owned(),
        missing_source_paths,
        paths_missing_markers,
        markers_without_leaf_paths,
        markers_with_unknown_leaf_ids,
    }
}

fn collect_arch_markers(root: &Path) -> Result<BTreeMap<String, BTreeSet<String>>> {
    let mut markers = BTreeMap::new();
    walk_dir(root, root, &mut |path| {
        if !is_source_file(path) {
            return Ok(());
        }
        let rel = rel_path(root, path)?;
        if should_skip_rel(&rel) {
            return Ok(());
        }
        let ids = arch_markers_at_file_top(path)?;
        if !ids.is_empty() {
            markers.insert(rel, ids);
        }
        Ok(())
    })?;
    Ok(markers)
}

fn walk_dir(root: &Path, dir: &Path, visit: &mut dyn FnMut(&Path) -> Result<()>) -> Result<()> {
    for entry in fs::read_dir(dir).with_context(|| format!("read dir {}", dir.display()))? {
        let entry = entry?;
        let path = entry.path();
        let rel = rel_path(root, &path)?;
        if should_skip_rel(&rel) {
            continue;
        }
        let file_type = entry.file_type()?;
        if file_type.is_dir() {
            walk_dir(root, &path, visit)?;
        } else if file_type.is_file() {
            visit(&path)?;
        }
    }
    Ok(())
}

fn arch_markers_at_file_top(path: &Path) -> Result<BTreeSet<String>> {
    let source = fs::read_to_string(path).with_context(|| format!("read {}", path.display()))?;
    let mut ids = BTreeSet::new();
    for (idx, line) in source.lines().take(8).enumerate() {
        let trimmed = line.trim_start();
        if idx == 0 && trimmed.starts_with("#!") {
            continue;
        }
        if trimmed.is_empty() {
            continue;
        }
        if let Some(rest) = trimmed.strip_prefix("// arch:") {
            insert_marker_ids(rest, &mut ids);
            continue;
        }
        if let Some(rest) = trimmed.strip_prefix("// @arch") {
            insert_marker_ids(rest, &mut ids);
            continue;
        }
        if trimmed.starts_with("//") {
            continue;
        }
        break;
    }
    Ok(ids)
}

fn insert_marker_ids(payload: &str, ids: &mut BTreeSet<String>) {
    for raw in payload.split(',') {
        let id = raw.trim();
        if is_arch_leaf_id(id) {
            ids.insert(id.to_string());
        }
    }
}

fn is_arch_leaf_id(id: &str) -> bool {
    let Some((parent, suffix)) = id.rsplit_once('.') else {
        return false;
    };
    parent.starts_with("arch_")
        && parent.len() > "arch_".len()
        && parent["arch_".len()..]
            .chars()
            .all(|c| c.is_ascii_alphanumeric())
        && !suffix.is_empty()
        && suffix.chars().all(|c| c.is_ascii_digit())
}

fn is_source_file(path: &Path) -> bool {
    matches!(
        path.extension().and_then(|ext| ext.to_str()),
        Some("rs" | "ts" | "tsx" | "js" | "jsx")
    )
}

fn should_skip_rel(rel: &str) -> bool {
    let skip_prefixes = [
        "archive/",
        "target/",
        ".git/",
        "node_modules/",
        "ui/dist/",
        ".orgasmic/tmp/",
    ];
    skip_prefixes.iter().any(|prefix| {
        rel.starts_with(prefix) || rel.trim_end_matches('/') == prefix.trim_end_matches('/')
    }) || rel.contains("/node_modules/")
}

fn rel_path(root: &Path, path: &Path) -> Result<String> {
    let rel = path
        .strip_prefix(root)
        .with_context(|| format!("strip {} from {}", root.display(), path.display()))?;
    Ok(normalize_rel_path(&rel.to_string_lossy()))
}

fn normalize_rel_path(path: &str) -> String {
    path.trim().trim_start_matches("./").replace('\\', "/")
}

pub fn print_human(report: &DriftReport) {
    if !report.has_drift() {
        println!("architecture drift: clean");
        return;
    }
    println!("architecture drift: {} finding(s)", report.drift_count());
    print_missing_source_paths(report);
    print_paths_missing_markers(report);
    print_markers_without_leaf_paths(report);
    print_markers_with_unknown_leaf_ids(report);
}

fn print_missing_source_paths(report: &DriftReport) {
    println!("\nmissing source paths:");
    if report.missing_source_paths.is_empty() {
        println!("  none");
        return;
    }
    for item in &report.missing_source_paths {
        println!("  {} -> {}", item.leaf_id, item.path);
    }
}

fn print_paths_missing_markers(report: &DriftReport) {
    println!("\npaths missing matching markers:");
    if report.paths_missing_markers.is_empty() {
        println!("  none");
        return;
    }
    for item in &report.paths_missing_markers {
        println!("  {} -> {}", item.leaf_id, item.path);
    }
}

fn print_markers_without_leaf_paths(report: &DriftReport) {
    println!("\nmarkers with no matching leaf source path:");
    if report.markers_without_leaf_paths.is_empty() {
        println!("  none");
        return;
    }
    for item in &report.markers_without_leaf_paths {
        println!("  {} -> {}", item.leaf_id, item.path);
    }
}

fn print_markers_with_unknown_leaf_ids(report: &DriftReport) {
    println!("\nmarkers referencing unknown leaf ids:");
    if report.markers_with_unknown_leaf_ids.is_empty() {
        println!("  none");
        return;
    }
    for item in &report.markers_with_unknown_leaf_ids {
        println!("  {} -> {}", item.leaf_id, item.path);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn marker_parser_accepts_comma_lists() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("lib.rs");
        fs::write(&path, "// arch: arch_ABC12.1, arch_ABC12.2\nfn main() {}\n").unwrap();
        let ids = arch_markers_at_file_top(&path).unwrap();
        assert!(ids.contains("arch_ABC12.1"));
        assert!(ids.contains("arch_ABC12.2"));
    }

    #[test]
    fn comparator_reports_all_four_categories_and_skips_planned() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        fs::create_dir_all(root.join(".orgasmic")).unwrap();
        fs::create_dir_all(root.join("crates/demo/src")).unwrap();
        fs::write(
            root.join(".orgasmic/architecture.org"),
            "* arch_ABCDE Demo\n:PROPERTIES:\n:ID: arch_ABCDE\n:END:\n** arch_ABCDE.1 One\n:PROPERTIES:\n:ID: arch_ABCDE.1\n:SOURCE_PATHS: crates/demo/src/one.rs crates/demo/src/missing.rs\n:END:\n** arch_ABCDE.2 Two\n:PROPERTIES:\n:ID: arch_ABCDE.2\n:STATUS: planned\n:SOURCE_PATHS: crates/demo/src/planned.rs\n:END:\n",
        )
        .unwrap();
        fs::write(root.join("crates/demo/src/one.rs"), "fn one() {}\n").unwrap();
        fs::write(
            root.join("crates/demo/src/extra.rs"),
            "// arch: arch_ABCDE.1, arch_ABCDE.9\nfn extra() {}\n",
        )
        .unwrap();
        fs::write(
            root.join("crates/demo/src/unlisted.rs"),
            "fn unlisted() {}\n",
        )
        .unwrap();
        let report = run(root).unwrap();
        assert_eq!(report.missing_source_paths.len(), 1);
        assert_eq!(report.paths_missing_markers.len(), 1);
        assert_eq!(report.markers_without_leaf_paths.len(), 1);
        assert_eq!(report.markers_with_unknown_leaf_ids.len(), 1);
        assert!(report
            .paths_missing_markers
            .iter()
            .all(|item| item.path != "crates/demo/src/unlisted.rs"));
        assert!(report
            .paths_missing_markers
            .iter()
            .all(|item| item.leaf_id != "arch_ABCDE.2"));
    }
}
