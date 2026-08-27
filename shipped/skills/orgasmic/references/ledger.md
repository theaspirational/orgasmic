# Ledger — file map, write rules, content rules

Paths relative to the project root printed by `orgasmic entry` (usually
`~/.orgasmic/ledgers/<id>/.orgasmic/`).

## Map

- `project.org` — baseline, mission, operating constraints.
- `tasks/<ID>/node.org` — one directory-backed task per id.
- `tasks/goal.org` — manager focus. `tasks/handoff.org` — manager continuity.
- `decisions/<ID>/node.org` — durable rationale.
- `glossary/<ID>/node.org` — domain language.
- `views/*.org` — derived, gitignored read views; never edit.
- `gotchas.org` — repeated traps; read before source edits.
- `conventions/` — repo-local working agreements, one file each.
- `tx/` — agent activity log.
- `tmp/local_instructions.org` — gitignored, machine-specific notes.

## Write rules

- Precheck: `command -v orgasmic >/dev/null && orgasmic status >/dev/null`.
- Success → write through `orgasmic ...` verbs only. Failure → stop; install
  or start the runtime. Never hand-edit.
- Tasks/decisions/glossary: create via their verbs; revise via
  `orgasmic node body set|append` / `orgasmic node prop set`.

## Content rules

- `tx/`, decisions, task evidence may be committed or published: no personal
  emails, use stable handles, prefer repo-relative paths.
- Rationale goes through `orgasmic decision` — no stray decision docs.
- Hit a trap or unexpected behavior? Add a `gotchas.org` entry.
- Scope changed? Update `tasks/` through the CLI.

Something under `~/.orgasmic/` looks configured but stale? Run
`orgasmic doctor` — it names every retired path and its deciding decision.
Do not conclude a capability is lost before reading that decision.
