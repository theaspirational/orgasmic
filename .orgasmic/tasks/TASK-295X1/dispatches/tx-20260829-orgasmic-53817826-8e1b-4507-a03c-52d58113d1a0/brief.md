# Review brief — TASK-295X1: `orgasmic forum critique` (Critic mode)

## What to review

Commit `7d4b7916` on branch `forum-critique-impl` (single commit, branched
from `main` at `8f644e9f`). Diff: `git diff main...forum-critique-impl`.

Files:
- `crates/orgasmic-cli/src/forum.rs` (+850/-140): ask pipeline parameterized
  into a shared `run_forum` path; new `forum critique` subcommand; target and
  focus validation; generalized first-section assembly.
- `shipped/prompt-studio/prompt-specs/critic.org`,
  `critique-cross-reviewer.org`, `critique-curator.org`: new stage personas.
- `shipped/skills/orgasmic/references/forum.md`: docs for both modes.

The implementer's report is at `/tmp/TASK-295X1-report.md` (also promoted in
the dispatch record). The original implementation brief is in the ledger at
`.orgasmic/tmp/dispatch/TASK-295X1/TASK-295X1-brief.md`.

## Contract this must satisfy

`orgasmic forum critique --target-file <path> [--focus <one-line>]` runs the
same three-stage pipeline as `forum ask` (blind stage-1 critiques →
self-excluding cross-review → curator), then the orchestrator assembles the
artifact deterministically:

- First block: verbatim, HTML-escaped `Target` section (orchestrator-owned,
  decoy-section defense preserved for BOTH modes — a curator-authored fake
  `Target`/`Question` section must still be rejected).
- Required sections in order: `Verdict`, `Findings`, `From target to verdict`;
  reader-feedback form; `__ORGASMIC_RUN_STATS__` MUST be the draft's last
  block (becomes the code-rendered `About this run` footer).
- No model-authored SVG anywhere; every placeholder exactly once; every raw
  task id present with boundary-aware matching.
- Target: single UTF-8 read, non-whitespace, ≤64 KiB, rejects orchestrator
  placeholder strings. Focus: one line, no placeholders, no leading `-`.
- Diagram JSON schema unchanged (`extracts`/`reviews`/`curator_summary`/
  optional `headline` ≤80 chars → artifact title; critique fallback title
  `Multi-model critique: <focus-or-basename>`).
- Ask behavior unchanged: the stored Python-parity fixture
  (`renderer_matches_stored_python_fixture`) must be byte-identical, and the
  three ask prompt specs untouched.

## Review posture

Adversarial. Priorities, in order:

1. **Refactor safety**: the ask path was rewritten into `run_forum`. Hunt for
   behavior drift in ask (task states, tx flow, wait/close, self-exclusion
   manifests, evidence, artifact title/fallbacks, error cleanup paths —
   including best-effort close on failure and WaitUnknown passthrough).
2. **Assembly bypasses**: decoy first-section, placeholder smuggling via the
   target text or focus, escaping gaps (braces, `<svg`, `data:` URIs),
   run-stats-footer-not-last, task-id boundary tricks.
3. **Validation gaps**: target/focus edge cases (BOM, CRLF, exactly-64KiB,
   non-UTF-8, symlinks are fine to allow), participant/curator misuse.
4. **Prompt specs**: do the three new specs actually bind the curator to the
   assembly contract (placeholders, JSON schema, finalize-without-commit),
   and do they treat target/reports as untrusted data?
5. Test honesty: do the new tests actually fail on the defects they claim to
   cover?

Run whatever you need: `cargo test -p orgasmic-cli --bin orgasmic`,
`cargo clippy -p orgasmic-cli --all-targets -- -D warnings`, targeted
red-then-green edits (revert them before finishing). Building the workspace
is allowed; do not run live dispatches or anything billed.

## Verdict contract

Write your review to `/tmp/TASK-295X1-review.md`:
- Verdict line first: `APPROVE` or `REJECT` (REJECT needs at least one
  concrete, reproducible defect).
- Findings ranked by severity with file:line anchors and, where possible, the
  failing input.
- Answer explicitly: "Would you merge this onto main as-is?"

Then make your terminal action:
`orgasmic dispatch finalize --task TASK-295X1 --summary-file /tmp/TASK-295X1-review.md`
Do not pass `--commit`. Do not edit the branch. Exiting without finalization
is a failed run.
