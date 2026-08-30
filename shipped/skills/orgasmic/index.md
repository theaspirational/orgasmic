# Orgasmic workflow discovery

## Operation

- [Artifact, verification, and member commands](operations/artifacts.md) — Replay verification proofs, submit or inspect artifacts, and manage local members.
- [Prompt and shipped-content commands](operations/content.md) — Inspect prompts and skills and manage optional or hub content.
- [Core and project commands](operations/core-project.md) — Install, enter, diagnose, update, and inspect projects and the UI.
- [Daemon commands](operations/daemon.md) — Run and manage the local daemon lifecycle.
- [Manager and dispatch commands](operations/dispatch.md) — Select drivers, dispatch workers, wait, close, finalize, record tx entries, and run manager stages.
- [Forum commands](operations/forum.md) — Ask, critique, review, and curate multi-model forums.
- [Run and utility commands](operations/runs.md) — Inspect worker histories, recover runs, manage auth, answer questions, and mint ids.
- [Task and graph commands](operations/task-graph.md) — List and mutate tasks, goals, glossary, decisions, edges, and node bodies/properties.

## Recipe

- [Challenge forum answers with reviewers](recipes/adversarial-forum-review.md) — Use forum review to send existing stage-1 reports to a fresh panel, including one strong model.
- [Run a cheap wide forum round](recipes/cheap-wide-forum.md) — Use --fast with one or more participants to skip cross-review, including a cheap 10-model first pass.
- [Dispatch and close a worker task](recipes/dispatch-task-lifecycle.md) — Run the dispatch, wait, inspect, merge or record the report, and close the exact generation with evidence.
- [Dispatch a fire-and-forget forum curator](recipes/dispatched-curator.md) — Run the single-round forum path with an explicit fresh curator and automatic artifact submission.
- [Inspect tasks, runs, and artifacts](recipes/inspect-work.md) — Use read surfaces to locate task state, run history, artifact content, and feedback before mutating anything.
- [Install or update the runtime](recipes/install-update-runtime.md) — Install a prebuilt runtime by default, or update according to install.json without touching project state.
- [Judge a document with a forum](recipes/judge-document.md) — Critique a UTF-8 document with an optional focus and either self or dispatched curation.
- [Run a self-curated forum](recipes/self-curated-forum.md) — Run one or more ask/critique/review rounds, curate in the current chat, then submit once.

## Topic

- [Orgasmic skill door](SKILL.md) — Orgasmic project management: forum ask/critique/review, self- or dispatched curation, task dispatch lifecycle, and runtime install/update.
- [Agent selection](references/agent-selection.md) — Choose dispatch harness, model, and effort while preserving reviewer independence.
- [When to ask](references/asking.md) — Confirm only actions that are hard to reverse, external, or spend another party's resources.
- [Dispatch mechanics and lifecycle](references/dispatch.md) — Worker visibility, retained worktrees, dispatch lifecycle, and finalization ownership.
- [Interactive multi-model forum](references/forum.md) — Full self-curated and dispatched-curator forum behavior for ask, critique, review, rounds, and curate.
- [Adopt a repository](references/init.md) — Initialize project state safely and choose whether to run the interactive bootstrap.
- [Install orgasmic](references/install.md) — Install the runtime, optional host app, remote access, or contributor source mode.
- [Ledger map and write rules](references/ledger.md) — Map project state and enforce daemon-owned writes.
- [Recall and resume](references/recall-resume.md) — Bootstrap a manager session from the thin goal and handoff state.
- [Manager edit tiers](references/tiers.md) — Classify manager-direct source edits and route ordinary or risky work to dispatch.
- [Update orgasmic](references/update.md) — Update a bundle runtime or contributor source checkout without touching project state.
