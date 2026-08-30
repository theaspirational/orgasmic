# Review brief — TASK-9TGQS: forum self-curation + multi-round forums

## What to review

Commit `ea57e7a7` on branch `forum-self-curation-impl` (single commit,
branched from `main` at `97eaf308`). Diff: `git diff main...HEAD`.
Files: `crates/orgasmic-cli/src/forum.rs` (+~1550 net),
`shipped/skills/orgasmic/SKILL.md`, `shipped/skills/orgasmic/references/forum.md`.

Implementer report: `/tmp/TASK-9TGQS-report.md`. Original brief:
`.orgasmic/tmp/dispatch/TASK-9TGQS/TASK-9TGQS-brief.md` (ledger).

## Contract (binding)

1. Omitted `--curator` = self-curation: stages 1-2 dispatch and promote as
   before, then the command persists a forum manifest under the ledger's
   `.orgasmic/tmp/forum/`, prints forum id + manifest path + promoted report
   paths + a compiled in-session curation contract, and exits WITHOUT a
   curator dispatch and WITHOUT submitting an artifact.
2. `--forum TASK-XXXXX` adds a round (ask or critique, any panel) to an OPEN
   self-curated forum; refusal matrix: unknown forum, already-curated forum,
   dispatched-curator forum, `--forum` combined with `--curator`. Subtask
   numbering continues under the one parent. Round 1 fixes `--from`/
   `--artifact-id`; later rounds may only omit or repeat them.
3. `forum curate --forum --draft --diagram --identity [--project]` runs the
   FULL existing gate set (model-SVG rejection, each placeholder exactly
   once, verbatim first section from ROUND 1 with decoy defense, required
   section order, boundary-aware raw-task-id presence for EVERY round,
   run-stats placeholder last, headline ≤80 → title with fallback), renders
   ONE deterministic SVG tree containing ALL rounds converging on a single
   curator card, submits one artifact, writes evidence, finishes the parent,
   marks the manifest curated (second curate refused).
4. Explicit `--curator <index|spec>` keeps the pre-change single-round
   dispatched-curator behavior byte-for-byte.
5. `renderer_matches_stored_python_fixture` byte-identity must hold on the
   untouched fixture. The three ask prompt specs and the two curator specs
   must be unchanged unless the diff explains why.

## Review posture — adversarial, priorities in order

1. **Refactor safety on the money paths.** The single-commit diff rewrites
   ~1700 lines of forum.rs. Trace explicit-curator ask AND critique end to
   end against main for drift (task states, tx request-ids, wait barriers,
   close/cleanup on failure, WaitUnknown passthrough, evidence, titles,
   About footer). Anything that changes the dispatched-curator path's
   behavior is at least MEDIUM.
2. **Manifest trust boundary.** The manifest lives on disk between CLI
   invocations and `forum curate` consumes it. What happens if it is edited,
   truncated, or swapped between rounds — can a tampered manifest smuggle a
   placeholder, fake report paths outside the ledger, or task ids from a
   different forum/project into the artifact? Path traversal in
   manifest-recorded paths? (The operator owns the machine, so this is
   robustness, not hard security — but silent nonsense in a submitted
   artifact is a real defect.)
3. **Multi-round assembly gaps.** Mixed ask+critique: which contract wins,
   is the round-1 verbatim check actually enforced when round 1 is critique
   (Target) vs ask (Question), do later-round prompts appear anywhere
   verbatim-unchecked, does the About footer Rounds list clip hostile input,
   are round task ids from EVERY round required in the draft?
4. **Diagram JSON `rounds` validation.** Coverage exactly-once per round,
   caps enforced per entry, legacy shape still accepted only where allowed
   (single-round), model-SVG rejection on the whole file, curator card
   identity from `--identity` not from JSON.
5. **State machine honesty.** The curation subtask minted at curate time:
   is its lifecycle legal (no in_review→todo style violations), is nothing
   closed as a fake dispatch, does a failed curate leave the forum re-curable
   rather than wedged, do abandoned forums leave tasks in a state the
   operator can close?
6. **Skill instructions.** Do SKILL.md + forum.md actually walk a session
   through: run → read manifest/contract/reports → curate in chat → optional
   `--forum` rounds → write draft+diagram → `forum curate` with REAL model
   identity (placeholders forbidden)? Would a fresh session following them
   succeed?
7. Test honesty: do the new tests fail on the defects they claim to cover?
   Mutation-probe anything suspicious (revert probes before finishing).

Run what you need: full `cargo test -p orgasmic-cli --bin orgasmic` (use the
default target dir — a custom CARGO_TARGET_DIR breaks
`empty_private_targets_never_run_another_worktrees_binary`), clippy
`-D warnings`, cli_parity, red-then-green edits. No live dispatches, nothing
billed.

## Verdict contract

Write `/tmp/TASK-9TGQS-review.md`:
- Verdict first: `APPROVE` or `REJECT` (REJECT needs a concrete reproducible
  defect).
- Findings ranked by severity with file:line anchors and failing inputs.
- Answer explicitly: "Would you merge this onto main as-is?"

Terminal action:
`orgasmic dispatch finalize --task TASK-9TGQS --summary-file /tmp/TASK-9TGQS-review.md`
No `--commit`. Do not edit the branch. Exiting without finalization is a
failed run.
