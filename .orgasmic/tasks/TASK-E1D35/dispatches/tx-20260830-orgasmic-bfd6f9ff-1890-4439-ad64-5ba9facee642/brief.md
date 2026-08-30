# TASK-E1D35 — `forum review`: challenge existing reports with a chosen panel

## Goal

New verb, the detached stage-2 lego brick:

`orgasmic forum review --forum TASK-XXXXX --participant <spec> [--round N | --all-rounds] [--focus <one-line>]`

Each participant blind-reviews the promoted STAGE-1 reports of the named
earlier round (default: all rounds so far) — challenge/add/agree deltas like
today's cross-review — without writing a new answer. The reviewing panel is
chosen freely: one strong model reviewing ten cheap answers, or several;
reviewers need not have been stage-1 participants. Recorded in the manifest
as its own round kind (`review`), rendered in the final tree as a review row
attached to the rounds it reviewed, feeding the one curator card.

## Read first

- `crates/orgasmic-cli/src/forum.rs` at current main (`3aaca8d7`) — the
  multi-round + fast machinery you extend: `ForumKind`/`ForumInput`, round
  manifest shape (incl. the new `fast` field), `validate_join_request`,
  `validate_manifest`, diagram JSON `rounds` validation, `render_multi_round_svg`,
  `compile_self_contract`, `render_forum_about_run`, `forum curate` gates.
- `shipped/prompt-studio/prompt-specs/cross-reviewer.org` and
  `critique-cross-reviewer.org` — reuse these personas where possible; a new
  spec is allowed only if reuse genuinely does not fit, and say why.
- `shipped/skills/orgasmic/references/forum.md` — document the brick.
- History: merges `9b76db01` (multi-round self-curation) and `3aaca8d7`
  (fast rounds) — every invariant you touch was adversarially reviewed
  there; non-review behavior stays frozen.

## Binding rules

1. **Self-curated forums only.** `forum review` requires `--forum` naming an
   open self-curated forum (same refusal matrix as joins: unknown, curated,
   dispatched-curator, curation-task-reserved). It cannot open a fresh forum
   and takes no `--curator`.
2. **Scope selection:** `--round N` reviews round N's stage-1 reports;
   `--all-rounds` (the default when neither is given) reviews every existing
   round's stage-1 reports. Reviewing a round that does not exist, a review
   round itself (no reviews of reviews), or a forum with no rounds is refused
   by name. `--focus` is an optional one-line steer validated like critique's.
3. **Blindness and self-exclusion:** reviewers see stage-1 reports only —
   never other reviewers' outputs from this or any round, and never their own
   authored stage-1 report: if a reviewer's identity (harness+model) matches
   a stage-1 participant whose report is in scope, that report is excluded
   from THAT reviewer's manifest. A reviewer whose exclusions would leave
   zero reports in scope is refused at intake.
4. **Manifest:** a review round records kind `review`, the reviewed round
   numbers, its panel, one task + one promoted report path per reviewer, and
   no `cross_review_tasks` of its own. `validate_manifest` enforces reviewed
   rounds exist, precede it, and are not themselves review rounds. Old
   manifests keep loading (serde defaults).
5. **Curate integration:** review-round task ids join the raw-task
   requirement; the compiled contract and diagram JSON cover review rounds —
   in the diagram JSON a review round carries `reviews` entries (3 tagged
   bullets each, one per reviewer task) and no `extracts`. Gates reject an
   `extracts` entry for a review round and vice versa.
6. **Renderer:** review-round rows render distinctly (reuse the existing
   review-card look), visually attached beneath the rounds they reviewed,
   arrows feeding the curator card. The stored ask fixture stays
   byte-identical; existing multi-round/fast renderer tests keep passing.
7. **Panel:** 1+ reviewers allowed (a review round is inherently stage-1-only
   for itself). Distinctness rules as elsewhere.
8. **Skill docs:** when to use `forum review` (challenge what we have) vs
   `forum critique` (judge a document); one worked example with a single
   strong reviewer.

## Hard constraints

- Non-review behavior frozen: every current test passes unmodified except
  helper struct-field defaults.
- New tests: refusal matrix (nonexistent round, review-of-review, empty
  scope after self-exclusion, dispatched/curated forums), manifest
  round-trip + validation, self-exclusion manifest content, diagram JSON
  acceptance/rejection for review rounds, renderer structure with
  ask + fast + review rounds mixed, curate gates on a forum ending in a
  review round.
- `cargo fmt --all`; `cargo clippy -p orgasmic-cli --all-targets -- -D
  warnings`; full `cargo test -p orgasmic-cli --bin orgasmic` with the
  DEFAULT target dir (custom CARGO_TARGET_DIR breaks
  `empty_private_targets_never_run_another_worktrees_binary`).
- No live billed dispatches.

## Deliverables

Report to `/tmp/TASK-E1D35-report.md`: what changed, contract decisions,
tests run, and the exact command sequence for: fast ask (3 models) →
`forum review` with one strong model over it → `forum curate`.

## Completion

Write the report, then make your terminal action:
`orgasmic dispatch finalize --task TASK-E1D35 --summary-file /tmp/TASK-E1D35-report.md --commit`
Exiting without finalization is a failed run.
