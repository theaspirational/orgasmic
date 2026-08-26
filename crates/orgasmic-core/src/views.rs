use std::path::Path;
use std::str::FromStr;

use anyhow::{bail, Context, Result};

use crate::{collection_node_file_paths, read_claims, LifecycleStage, OrgFile, OrgRewriter};

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
    let claims = read_claims(project_root)?;
    let rendered = VIEWS
        .iter()
        .map(|(collection, file, header)| {
            Ok((
                project_root.join(".orgasmic/views").join(file),
                render_collection(project_root, collection, header, &claims)?,
            ))
        })
        .collect::<Result<Vec<_>>>()?;

    std::fs::create_dir_all(project_root.join(".orgasmic/views"))
        .context("create .orgasmic/views")?;
    rendered.into_iter().try_fold(0, |changed, (path, source)| {
        Ok(changed + usize::from(write_if_changed(&path, source.as_bytes())?))
    })
}

fn render_collection(
    project_root: &Path,
    collection: &str,
    header: &str,
    claims: &std::collections::BTreeMap<String, crate::TaskClaim>,
) -> Result<String> {
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
        let mut node = file.slice(heading.span.clone()).to_string();
        if collection == "tasks" {
            if let Some(id) = heading.property("ID") {
                if let Some(claim) = claims.get(id) {
                    let mut rewriter = OrgRewriter::new(&file, path.to_string_lossy());
                    rewriter.upsert_property(id, "CLAIM_HOLDER", &claim.holder)?;
                    if let Some(scope) = claim.write_scope.as_deref() {
                        rewriter.upsert_property(id, "CLAIM_WRITE_SCOPE", scope)?;
                    }
                    if claim.contenders.len() > 1 {
                        rewriter.upsert_property(
                            id,
                            "DOUBLE_CLAIM",
                            &claim.contenders.join(" "),
                        )?;
                    }
                    let rendered = rewriter.finish();
                    let rendered_file = OrgFile::parse(rendered, path.to_string_lossy())?;
                    node = rendered_file
                        .slice(rendered_file.headings[0].span.clone())
                        .to_string();
                }
            }
        }
        nodes.push((stage, node));
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{claims::CLAIMED, TxEntry, TxWriter};

    fn seed(root: &Path, machine_order: &[&str]) {
        let node = root.join(".orgasmic/tasks/TASK-CLAIM/node.org");
        std::fs::create_dir_all(node.parent().unwrap()).unwrap();
        std::fs::write(
            node,
            "#+title: orgasmic task TASK-CLAIM\n#+orgasmic_version: 2\n\n* BACKLOG Claimed task\n:PROPERTIES:\n:ID: TASK-CLAIM\n:END:\n",
        )
        .unwrap();
        for machine in machine_order {
            let path = root
                .join(".orgasmic/machines")
                .join(machine)
                .join("claims.org");
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            let mut event = TxEntry::new(
                format!("claim-{machine}"),
                CLAIMED,
                if *machine == "machine-a" {
                    "[2026-08-26 Wed 10:00:01]"
                } else {
                    "[2026-08-26 Wed 10:00:02]"
                },
                "test",
                *machine,
            );
            event.task = Some("TASK-CLAIM".into());
            event.extra.push(("WRITE_SCOPE".into(), "crates/**".into()));
            TxWriter::open(path).unwrap().append(&event).unwrap();
        }
    }

    #[test]
    fn multi_machine_views_are_ingest_order_independent_and_show_double_claims() {
        let first = tempfile::tempdir().unwrap();
        let second = tempfile::tempdir().unwrap();
        seed(first.path(), &["machine-a", "machine-b"]);
        seed(second.path(), &["machine-b", "machine-a"]);

        build_views(first.path()).unwrap();
        build_views(second.path()).unwrap();
        let first_board = std::fs::read(first.path().join(".orgasmic/views/board.org")).unwrap();
        let second_board = std::fs::read(second.path().join(".orgasmic/views/board.org")).unwrap();
        assert_eq!(first_board, second_board);
        let rendered = String::from_utf8(first_board).unwrap();
        assert!(rendered.contains(":CLAIM_HOLDER: machine-a"));
        let parsed = OrgFile::parse(rendered, "board.org").unwrap();
        assert_eq!(
            parsed.headings[0].property("DOUBLE_CLAIM"),
            Some("machine-a machine-b")
        );
    }
}
