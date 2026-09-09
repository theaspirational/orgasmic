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
    registry.register_hooks(
        "conversations",
        WriteHooks {
            validate_write: None,
            validate_transition: Some(|_, heading, before| {
                let Some(before) = before else { return Ok(()) };
                for key in ["PURPOSE", "OWNER"] {
                    anyhow::ensure!(heading.property(key) == before.property(key), "conversation {key} is immutable");
                }
                for key in ["RUNS", "MACHINE", "MODE", "PROVIDER", "CREATED_AT"] {
                    anyhow::ensure!(heading.property(key) == before.property(key), "conversation {key} is owned by the daemon; use POST /conversations and POST /conversations/:id/input");
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
    fn conversation_hooks_pin_owner_purpose_and_daemon_fields() {
        let tmp = tempfile::tempdir().unwrap();
        let home = orgasmic_core::Home::at(tmp.path().join("home"));
        home.ensure().unwrap();
        let registry = super::load(&home).unwrap();
        let render = |title: &str, purpose: &str, owner: &str, runs: &str| {
            format!(
                "#+todo: OPEN | ARCHIVED\n\n* OPEN {title}\n:PROPERTIES:\n:ID: CONV-AB12C\n:PURPOSE: {purpose}\n:OWNER: {owner}\n:PROVIDER: codex\n:MODE: chat\n:MACHINE: m1\n:RUNS: {runs}\n:CREATED_AT: 2026-09-09T00:00:00Z\n:END:\n"
            )
        };
        let parse = |text: String| orgasmic_core::OrgFile::parse(text, "node.org").unwrap();
        let before = parse(render("Chat", "discuss", "admin", "run-a"));
        let check = |after: orgasmic_core::OrgFile| {
            registry.validate_write("conversations", &after, &after.headings[0])?;
            registry.validate_transition(
                "conversations",
                Some(&before.headings[0]),
                &after,
                &after.headings[0],
            )
        };
        // Title and state edits are ordinary edits.
        check(parse(render("Renamed", "discuss", "admin", "run-a"))).unwrap();
        check(parse(
            render("Chat", "discuss", "admin", "run-a").replace("* OPEN", "* ARCHIVED"),
        ))
        .unwrap();
        let err = check(parse(render("Chat", "review", "admin", "run-a"))).unwrap_err();
        assert!(err.to_string().contains("PURPOSE is immutable"), "{err}");
        let err = check(parse(render(
            "Chat",
            "discuss",
            "[\"member\",\"anna\"]",
            "run-a",
        )))
        .unwrap_err();
        assert!(err.to_string().contains("OWNER is immutable"), "{err}");
        let err = check(parse(render("Chat", "discuss", "admin", "run-a run-b"))).unwrap_err();
        assert!(
            err.to_string().contains("RUNS is owned by the daemon"),
            "{err}"
        );
        assert!(
            err.to_string().contains("POST /conversations/:id/input"),
            "{err}"
        );
        // The daemon's own RUNS append validates the write shape only.
        let appended = parse(render("Chat", "discuss", "admin", "run-a run-b:cold"));
        registry
            .validate_write("conversations", &appended, &appended.headings[0])
            .unwrap();
        // Creation (no `before`) is not pinned by the hook; the daemon route
        // is the only creator.
        registry
            .validate_transition("conversations", None, &appended, &appended.headings[0])
            .unwrap();
        let err = registry
            .validate_write(
                "conversations",
                &parse(render("Chat", "", "admin", "")),
                &parse(render("Chat", "", "admin", "")).headings[0],
            )
            .unwrap_err();
        assert!(err.to_string().contains("PURPOSE"), "{err}");
    }

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
