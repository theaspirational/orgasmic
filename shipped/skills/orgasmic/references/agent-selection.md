---
type: Topic
title: Agent selection
description: Choose dispatch harness, model, and effort while preserving reviewer
  independence.
sources:
- shipped/skills/orgasmic/references/agent-selection.md
---

# Agent selection — harness, model, effort

Kind, mode, harness + the harness's own model/effort decide who runs a
dispatch. `orgasmic manager drivers` lists installed pairs. Model and effort
are harness vocabulary: per-dispatch flags, passed through unvalidated. The
dispatch run record stores what was requested (`model`, `reasoning_effort` in
the run's driver config).

## The one allowed question

At the **first dispatch of the session** (not session start), ask the operator
once, in one turn:

1. Which harness family to implement with.
2. What effort level review and risky stages carry.

Carry both answers for the rest of the session; stop asking. Session-scoped,
not stored. No operator present → dispatch anyway with the selection rationale
in `--reason` (it lands on the dispatch tx). Never select by silent default;
an unasked dispatch with a stated reason is fine.

## Rules

- **Reviewer independence** (runtime policy): the reviewer runs on a different
  harness family than the run under review. Same-family review requires a
  stated reason on the dispatch. Author's harness unknown → ask which family
  wrote it; defaulting to your own family risks same-family review.
- **Review and risky stages carry an explicit `--effort`.** Unset effort means
  the harness chose silently; the run record then shows only that nothing was
  requested, not what actually ran. Unset is allowed as a stated choice, never
  as an oversight.
- Ordinary/trivial work may run at harness default effort.
