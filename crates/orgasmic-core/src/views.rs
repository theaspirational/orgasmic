use std::path::Path;
use std::str::FromStr;

use anyhow::{bail, Context, Result};

use crate::{collection_node_file_paths, LifecycleStage, OrgFile};

const VIEWS: [(&str, &str, &str); 3] = [
    (
        "tasks",
        "board.org",
        "#+title: orgasmic task board\n#+orgasmic_version: 2\n\n",
    ),
    (
        "glossary",
        "glossary.org",
        "#+title: orgasmic project glossary\n#+orgasmic_version: 2\n\n",
    ),
    (
        "decisions",
        "decisions.org",
        "#+title: orgasmic project decisions\n#+orgasmic_version: 2\n\n",
    ),
];

/// Rebuild the throwaway aggregate views from node directories.
/// Returns the number of files whose bytes changed.
pub fn build_views(project_root: &Path) -> Result<usize> {
    let rendered = VIEWS
        .iter()
        .map(|(collection, file, header)| {
            Ok((
                project_root.join(".orgasmic/views").join(file),
                render_collection(project_root, collection, header)?,
            ))
        })
        .collect::<Result<Vec<_>>>()?;

    std::fs::create_dir_all(project_root.join(".orgasmic/views"))
        .context("create .orgasmic/views")?;
    rendered.into_iter().try_fold(0, |changed, (path, source)| {
        Ok(changed + usize::from(write_if_changed(&path, source.as_bytes())?))
    })
}

fn render_collection(project_root: &Path, collection: &str, header: &str) -> Result<String> {
    let mut nodes = Vec::new();
    for path in collection_node_file_paths(project_root, collection)
        .with_context(|| format!("list .orgasmic/{collection}"))?
    {
        let source =
            std::fs::read_to_string(&path).with_context(|| format!("read {}", path.display()))?;
        let file = OrgFile::parse(source, path.to_string_lossy())
            .with_context(|| format!("parse {}", path.display()))?;
        if file.headings.len() != 1 {
            bail!("{} must contain exactly one heading", path.display());
        }
        let heading = &file.headings[0];
        let stage = if collection == "tasks" {
            Some(
                LifecycleStage::from_str(heading.todo.as_deref().unwrap_or_default())
                    .map_err(|_| anyhow::anyhow!("{} has invalid task state", path.display()))?,
            )
        } else {
            None
        };
        nodes.push((stage, file.slice(heading.span.clone()).to_string()));
    }
    nodes.sort_by_key(|(stage, _)| *stage);

    let mut out = header.to_string();
    for (_, node) in nodes {
        out.push_str(&node);
        if !node.ends_with('\n') {
            out.push('\n');
        }
        if !node.ends_with("\n\n") {
            out.push('\n');
        }
    }
    Ok(out)
}

fn write_if_changed(path: &Path, bytes: &[u8]) -> Result<bool> {
    if std::fs::read(path).is_ok_and(|current| current == bytes) {
        return Ok(false);
    }
    let mut tmp_name = path
        .file_name()
        .context("view path has no file name")?
        .to_os_string();
    tmp_name.push(format!(".{}.tmp", std::process::id()));
    let tmp = path.with_file_name(tmp_name);
    std::fs::write(&tmp, bytes).with_context(|| format!("write {}", tmp.display()))?;
    if let Err(error) = std::fs::rename(&tmp, path) {
        let _ = std::fs::remove_file(&tmp);
        return Err(error).with_context(|| format!("replace {}", path.display()));
    }
    Ok(true)
}
