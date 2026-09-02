---
type: Topic
title: Ledger map and write rules
description: Map project state and enforce daemon-owned writes.
sources:
- shipped/skills/orgasmic/references/ledger.md
---

# Ledger — file map, write rules, content rules

Paths relative to the project root printed by `orgasmic entry` (usually
`~/.orgasmic/ledgers/<id>/.orgasmic/`).

## Map

- `project.org` — baseline, mission, operating constraints.
- `tasks/<ID>/node.org` — one directory-backed task per id.
- `tasks/<ID>/journal.org` — that node's event log (post tx-split).
- `tasks/<ID>/dispatches/<tx-id>/` — dispatch brief, report, evidence.
- `tasks/goal.org` — manager focus. `tasks/handoff.org` — manager continuity.
- `decisions/<ID>/node.org` — durable rationale.
- `glossary/<ID>/node.org` — domain language.
- `gotchas.org` — repeated traps; read before source edits.
- `conventions/` — repo-local working agreements, one file each.
- `tx/` — frozen pre-cutover activity log; node events land in `journal.org`.
- `machines/<machine-id>/tx/` — machine-routed events, including parked ledger
  sync conflicts.
- `tmp/local_instructions.org` — gitignored, machine-specific notes.

## Write rules

- Precheck: `command -v orgasmic >/dev/null && orgasmic status >/dev/null`.
- Success → write through `orgasmic ...` verbs only. Failure → stop; install
  or start the runtime. Never hand-edit.
- Tasks/decisions/glossary: create via their verbs; revise via
  `orgasmic node body set|append` / `orgasmic node prop set`; retitle via
  `orgasmic node title set` (goal and project titles excepted; goal: TASK-V460X).
- Claims prevent overlapping dispatch writes while held, not free writes between
  dispatches. A cross-machine Git conflict parks the local side before following
  the remote; reconcile from the parked ref reported by daemon status.

## Content rules

- `tx/`, decisions, task evidence may be committed or published: no personal
  emails, use stable handles, prefer repo-relative paths.
- Rationale goes through `orgasmic decision` — no stray decision docs.
- Hit a trap or unexpected behavior? Add a `gotchas.org` entry.
- Scope changed? Update `tasks/` through the CLI.

Something under `~/.orgasmic/` looks configured but stale? Run
`orgasmic doctor` — it names every retired path and its deciding decision.
Do not conclude a capability is lost before reading that decision.
