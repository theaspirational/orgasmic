# TASK-CS2TM item 4 — `orgasmic manager dispatch-status --json`

This is the ONE remaining item on a task whose other three items shipped today
(commits 0e1a8558, 8cd761f5, 0ca77380, merged as ecdcb78a). Read the task's
Evidence section first: it records why item 4 was skipped.

## Why it was skipped, and what that means for you

`dispatch-status` prints from SEVERAL independent branches — cleanup-failed,
open dispatches with health and claim annotations, torn-close reconcile, and
the managed-worktree report — each with its own derived fields. The previous
implementer judged that `--json` needs a struct per branch and declined to
rush it. That judgement was accepted. Do it properly now.

## What to build

Add `--json` to `orgasmic manager dispatch-status` in
`crates/orgasmic-cli/src/manager.rs`.

- One serde struct per output branch, composed into a single top-level object
  so a consumer can tell WHICH branch produced what. Do not flatten the
  branches into an untagged blob.
- Every field the human output shows must appear, including the tokens the
  2026-09-02 sprint added: MODEL, EFFORT, PREFLIGHT, CLAIM_HOLDER,
  DOUBLE_CLAIM, PARKED lines, AWAITING_MERGE disposition, the exit reason and
  evidence path for gone runs, and main_checkout_dirty.
- An optional value the human line prints as `-` must be `null` in JSON, not
  the string "-".
- `--json` must not change the human path at all.

## Guardrails

- The human output is parsed by tests and by shipped docs. Do not reword it.
- Prefer reusing the existing types that already hold this data over inventing
  parallel ones. Look before you add.
- No new dependency.

## Acceptance

- A test that asserts the JSON round-trips for at least: an open dispatch with
  model/effort/preflight set, a gone run with an exit reason and evidence
  path, a PARKED task, and a cleanup-failed record.
- A test that the human output is byte-identical with and without the flag
  absent (i.e. `--json` is purely additive).
- `cargo clippy -p orgasmic-cli --all-targets -- -D warnings` and
  `cargo fmt --all --check` clean.
