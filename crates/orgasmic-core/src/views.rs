use std::path::Path;
use std::str::FromStr;

use anyhow::{bail, Context, Result};

use crate::{collection_node_file_paths, read_claims, LifecycleStage, OrgFile, OrgRewriter};

/// The on-demand aggregate views (dec_XH2XY): `(collection, file name, header)`.
/// Nothing writes these to disk anymore; `render_view` renders them on demand.
pub const VIEWS: [(&str, &str, &str); 3] = [
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

/// Render one aggregate view (`board.org`, `decisions.org`, `glossary.org`)
/// from the node directories. Pure: no `.orgasmic/views/` directory is
/// created and no file is written.
pub fn render_view(project_root: &Path, file: &str) -> Result<String> {
    let &(collection, _, header) = VIEWS
        .iter()
        .find(|(_, view_file, _)| *view_file == file)
        .with_context(|| format!("unknown view {file}"))?;
    let claims = read_claims(project_root)?;
    render_collection(project_root, collection, header, &claims)
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

        let first_board = render_view(first.path(), "board.org").unwrap();
        let second_board = render_view(second.path(), "board.org").unwrap();
        assert_eq!(first_board, second_board);
        assert!(first_board.contains(":CLAIM_HOLDER: machine-a"));
        let parsed = OrgFile::parse(first_board, "board.org").unwrap();
        assert_eq!(
            parsed.headings[0].property("DOUBLE_CLAIM"),
            Some("machine-a machine-b")
        );
        // Pure renderer: nothing lands on disk.
        assert!(!first.path().join(".orgasmic/views").exists());
    }

    #[test]
    fn render_view_covers_every_view_and_refuses_unknown_names() {
        let root = tempfile::tempdir().unwrap();
        seed(root.path(), &["machine-a"]);
        for (_, file, header) in VIEWS {
            let rendered = render_view(root.path(), file).unwrap();
            assert!(rendered.starts_with(header), "{file} must carry its header");
        }
        let error = render_view(root.path(), "other.org").unwrap_err();
        assert!(format!("{error:#}").contains("unknown view other.org"));
    }
}
