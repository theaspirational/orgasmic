# TASK-DTCBP item 2 — the `:VERIFY_ARTIFACT:` drawer property

Items 1 and 3 are DONE. Read the task's Evidence section before you start.

- Item 1 shipped today: `orgasmic verify --all [--check] [--json]`
  (commit af0d15a5, merged as ad8cf634).
- Item 3 was DECIDED by the manager on 2026-09-02 and is recorded in Evidence:
  do NOT gate `dispatch-close --status done` on a passing replay by default.
  Measured reason: 39 of 109 artifacts are currently stale, so a default gate
  would block most closes on artifacts the closing task never touched.

## What to build (item 2 only)

The task body's stated preference, deferred earlier for write scope:

1. Add a `verify_artifact` schema field in `orgasmic-core` — a node drawer
   property, `:VERIFY_ARTIFACT:`.
2. Let implementers set it through the existing node property write surface.
3. Have `orgasmic verify` PREFER it over the current path convention, falling
   back to the convention when the property is absent.
4. At `dispatch-close`, validate that a task claiming an artifact has one that
   LOADS. This is the narrow validation item 3's decision points at: a close
   validates only its OWN artifact, never the whole corpus.

## Why this matters

Item 3's decision explicitly depends on this: once a close can validate only
its own artifact, the default-gate question can be revisited. That is the
point of the item.

## Guardrails

- Do not turn this into the global gate item 3 refused.
- A missing property must stay a fallback, not an error — most existing tasks
  do not have one.
- `verify --all --check` currently exits 2 on this tree with 39/109 stale
  artifacts. That is BY DESIGN. Do not "fix" it by weakening the sweep.

## Acceptance

- A task with `:VERIFY_ARTIFACT:` set to a real artifact resolves through the
  property, not the path convention.
- A task with the property pointing at a MISSING or unloadable artifact is
  refused at `dispatch-close` with a message naming the artifact.
- A task with no property behaves exactly as today.
- clippy `-D warnings` and `cargo fmt --all --check` clean.
