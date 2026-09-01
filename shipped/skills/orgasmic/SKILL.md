---
name: orgasmic
type: Topic
title: Orgasmic skill door
description: 'Orgasmic project management: forum ask/critique/review, self- or dispatched curation, task dispatch lifecycle, and runtime install/update.'
triggers:
- /orgasmic
- /orgasmic install
- /orgasmic update
- /orgasmic init
- /orgasmic recall
- /orgasmic resume
- /orgasmic forum
- /recall
- /resume
sources:
- shipped/skills/orgasmic/SKILL.md
---

# orgasmic

Orgasmic coordinates local project state, worker dispatch, and multi-model
forums. It is opinionated but has no single mandatory workflow — the recipes
in this bundle are alternative doors, not steps of one process.

## Bare `/orgasmic` — orient first, load nothing

Invoked with no argument, do NOT read further bundle files yet. Run a cheap
read-only scan, then brief the user and ask what they want now.

1. `command -v orgasmic` — missing → offer
   [installing the runtime](recipes/install-update-runtime.md); stop.
2. `orgasmic entry` — prints `PROJECT -` when the cwd is not inside an
   orgasmic project → offer two doors and stop:
   [adopt this directory](references/init.md) (works without git), or move
   into one of the projects `orgasmic board` lists. Everything else below —
   tasks and forums alike — runs inside a project.
3. `orgasmic tasks list --stage in_review --stage in_progress --stage todo`
   — the open work, closest-to-done first.

Brief the user in a few lines — what orgasmic is, what the scan found — and
offer the fitting subset of:

- **Continue open work** — suggest the most advanced task
  (`in_review` beats `in_progress` beats `todo` beats `backlog`):
  [dispatch-task-lifecycle](recipes/dispatch-task-lifecycle.md) to move it,
  [inspect-work](recipes/inspect-work.md) to look first.
- **Shape something new** — pressure-test the idea in chat or via a
  [forum critique](recipes/judge-document.md), then record tasks and
  decisions ([task and graph verbs](operations/task-graph.md)).
- **Discuss an idea across models** — a
  [self-curated forum](recipes/self-curated-forum.md) or a
  [cheap wide round](recipes/cheap-wide-forum.md).
- **Deep manager bootstrap** —
  [recall and resume](references/recall-resume.md), the goal/handoff
  continuity workflow (what `/orgasmic recall` and `resume` run). One way of
  working, not the default.

Open a linked file only after the user picks.

## Any named intent

Start at the [bundle index][bundle-index]; choose the recipe matching the
intent, and follow its links to operation references and deeper policy. Raw
Markdown traversal is the complete fallback; OKFy is not required. Links
starting with `/` (such as `/operations/forum.md`) resolve from this bundle
root — the directory holding this SKILL.md — not the filesystem root.

Never invent a flag or signature: if no operation or recipe documents it, say
"not covered by this bundle" and check `--help` — the installed CLI is the
truth when they disagree.

[bundle-index]: index.md

When `okfy` is installed, search this directory with:

```bash
okfy query <directory-containing-this-SKILL.md> "<intent>"
```

All orgasmic state writes go through the `orgasmic` CLI, never hand edits.
