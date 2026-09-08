pub use orgasmic_core::node_registry::*;

pub fn load(home: &orgasmic_core::Home) -> anyhow::Result<NodeTypeRegistry> {
    let mut registry = NodeTypeRegistry::for_home(home)?;
    registry.register_hooks(
        "tasks",
        WriteHooks {
            validate_write: Some(|file, heading| {
                orgasmic_core::TaskHeading::from_heading(file, heading, "node.org")?;
                Ok(())
            }),
            validate_transition: Some(|file, heading, _| {
                if heading.todo.as_deref() == Some("DONE")
                    && crate::index::parse_task_body(file, heading)
                        .evidence
                        .is_empty()
                {
                    let id = heading.property("ID").unwrap_or("task");
                    anyhow::bail!("closing {id} requires recorded evidence and its Evidence section is empty. Record proof (a run id, review verdict, test output, commit) with `orgasmic node body set {id} --section Evidence --create --body \"…\"`, then close again.");
                }
                Ok(())
            }),
        },
    )?;
    registry.register_hooks(
        "artifacts",
        WriteHooks {
            validate_write: Some(|_, heading| {
                crate::artifacts::validate_art_id_readable(heading.property("ID").unwrap_or_default())
                    .map_err(anyhow::Error::msg)?;
                anyhow::ensure!(heading.property("VERSION").and_then(|value| value.parse::<u32>().ok()).is_some_and(|version| version > 0), "artifact VERSION must be a positive integer");
                anyhow::ensure!(heading.todo.is_none(), "artifacts use their compiled STATE lifecycle, not heading states");
                Ok(())
            }),
            validate_transition: Some(|_, heading, before| {
                if let Some(before) = before {
                    for key in ["VERSION", "STATE"] {
                        anyhow::ensure!(heading.property(key) == before.property(key), "artifact {key} is owned by submit/regenerate; use the artifact command");
                    }
                } else {
                    anyhow::ensure!(heading.property("VERSION") == Some("1") && heading.property("STATE") == Some("submitted"), "new artifacts start at VERSION 1, STATE submitted");
                }
                Ok(())
            }),
        },
    )?;
    Ok(registry)
}

#[cfg(test)]
mod tests {
    #[test]
    fn descriptor_states_match_compiled_task_behavior() {
        use orgasmic_core::LifecycleStage::*;
        let registry = orgasmic_core::NodeTypeRegistry::embedded().unwrap();
        let actual: std::collections::BTreeSet<_> = registry
            .descriptor("tasks")
            .unwrap()
            .states
            .iter()
            .map(String::as_str)
            .collect();
        let expected = [Backlog, Todo, InProgress, InReview, Done, Cancelled]
            .map(|stage| stage.as_str())
            .into_iter()
            .collect();
        assert_eq!(actual, expected);
    }
}
