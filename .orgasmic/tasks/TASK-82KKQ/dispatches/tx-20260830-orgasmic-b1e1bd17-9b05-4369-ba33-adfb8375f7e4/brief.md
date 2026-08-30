# Review brief — TASK-82KKQ: forum fast rounds

## What to review

Commit `a05326dc` on branch `forum-fast-rounds-impl` (single commit, branched
from `main` at `9b76db01`). Diff: `git diff main...HEAD`. Files:
`crates/orgasmic-cli/src/forum.rs` (+~360 net), `curator.org`,
`critique-curator.org`, `shipped/skills/orgasmic/references/forum.md`.
Implementer report: `/tmp/TASK-82KKQ-report.md`. Implementation brief:
`.orgasmic/tmp/dispatch/TASK-82KKQ/TASK-82KKQ-brief.md` (ledger).

## Contract (binding)

1. `--fast` on ask/critique = stage 1 only, per round: fresh forums,
   `--forum` joins, dispatched-curator single rounds. Manifest round records
   `fast: true` (serde default false so old manifests still load), empty
   `cross_review_tasks`, one promoted report path per participant.
2. Panel of 1 is legal ONLY with `--fast`; without it the 2+ rule and its
   message are unchanged. `--fast` with 2+ participants also legal.
3. Diagram JSON: fast round `reviews` must be absent or `[]`; any review
   entry for a fast round is rejected (invented provenance). Normal rounds
   keep exact current requirements.
4. Renderer: fast extract cards arrow straight to the curator; mixed
   fast+normal forums render both shapes; the stored ask fixture
   (`renderer_matches_stored_python_fixture`) stays byte-identical.
5. Dispatched fast footer says `0 cross-reviews` honestly; fast-only
   compiled contracts/curator prompts must not instruct reading cross-review
   reports that don't exist.
6. ALL non-fast behavior frozen: every pre-existing test passes unmodified
   (helper struct-field defaults excepted).

## Review posture — adversarial, priorities in order

1. **Frozen-path drift.** The diff touches shared validation, manifest,
   renderer, contract-compile, and BOTH curator specs (+27/-? lines each —
   the brief only authorized loosening the cross-review reading
   instruction; scrutinize every other spec change). Trace normal ask,
   normal critique, dispatched and self-curated, against main.
2. **Invariant relaxation leaks.** Can a NORMAL round now slip through with
   1 participant, missing reviews, or count mismatches (`count*2` checks
   loosened too far)? Can a fast round smuggle a review entry through the
   legacy top-level diagram shape, or through `rounds` with a kind/fast
   mismatch? Is `fast` join + normal join under one forum counted right in
   `next_task_ordinal` and report-path zip logic (`promoted_report_paths`
   pairing assumptions — the old code zipped tasks×2)?
3. **Renderer correctness.** Mixed forums: arrow/card counts, no review row
   for fast rounds, single fast round with 1 participant renders sanely;
   fixture byte-identity.
4. **Prompt-spec honesty.** Compiled fast contracts: no dangling references
   to cross-review reports, `reviews` guidance matches validation, About
   footer wording truthful for fast, normal, and mixed.
5. **Test honesty.** Do the new tests fail on the defects they claim?
   Mutation-probe at least the fast/normal review-count gate and the
   panel-of-1 gate (revert probes before finishing).

Run what you need: full `cargo test -p orgasmic-cli --bin orgasmic` (DEFAULT
target dir — custom CARGO_TARGET_DIR breaks
`empty_private_targets_never_run_another_worktrees_binary`), clippy
`-D warnings`, cli_parity, red-then-green edits. No live dispatches.

## Verdict contract

Write `/tmp/TASK-82KKQ-review.md`: verdict first (`APPROVE`/`REJECT`, REJECT
needs a concrete reproducible defect), findings ranked with file:line and
failing inputs, and answer explicitly: "Would you merge this onto main
as-is?"

Terminal action:
`orgasmic dispatch finalize --task TASK-82KKQ --summary-file /tmp/TASK-82KKQ-review.md`
No `--commit`. Do not edit the branch. Exiting without finalization is a
failed run.
