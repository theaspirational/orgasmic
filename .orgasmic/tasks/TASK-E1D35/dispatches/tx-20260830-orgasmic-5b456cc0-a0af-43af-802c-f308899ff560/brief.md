# Review brief — TASK-E1D35: `forum review` (detached stage-2 rounds)

## What to review

The single implementer commit on branch `forum-review-rounds-impl`
(branched from `main` at `3aaca8d7`). Diff: `git diff main...HEAD`. Files:
`crates/orgasmic-cli/src/forum.rs` (+~900 net),
`shipped/prompt-studio/prompt-specs/forum-reviewer.org` (new),
`shipped/skills/orgasmic/references/forum.md`.
Implementer report: `/tmp/TASK-E1D35-report.md`. Implementation brief:
`.orgasmic/tmp/dispatch/TASK-E1D35/TASK-E1D35-brief.md` (ledger).

## Contract (binding)

1. `forum review --forum <id> --participant <spec>... [--round N |
   --all-rounds] [--focus <one-line>]`: reviews promoted STAGE-1 reports of
   existing non-review rounds; self-curated open forums only (full refusal
   matrix incl. curation-task-reserved); no reviews of reviews; panel 1+.
2. Blindness: reviewers see selected stage-1 reports only — never other
   reviewers' outputs; a reviewer matching a stage-1 participant
   (harness + normalized vendor/model) never receives its own report;
   empty-scope-after-exclusion refused at intake.
3. Manifest: review rounds use a serde-defaulted `review_tasks` list (one
   task + one promoted path per reviewer), empty `first_stage_tasks` and
   `cross_review_tasks`; validation requires reviewed rounds to exist,
   precede, and not be review rounds; old manifests still load.
4. Curate: review tasks join the raw-task requirement, contracts, diagram
   JSON (review rounds carry `reviews` with the 3-tag `?`/`+`/`=` gate,
   forbid `extracts`), footer, ordinals — each exactly once.
5. Renderer: distinct review row linked from reviewed rounds, feeding the
   one curator card; ask fixture byte-identical; existing multi-round/fast
   renderer tests unmodified.
6. All non-review behavior frozen.

## Review posture — adversarial, priorities in order

1. **Blindness holes.** Per-reviewer manifest content: can a reviewer
   receive its own report through identity-normalization gaps
   (`provider/model` vs bare model, case, whitespace, hermes-prefixed
   models)? Can review-round briefs leak other reviewers' outputs or
   earlier review-round reports through `--all-rounds` scope or contract
   compilation?
2. **Invariant leaks in the shared machinery.** The diff rewrites manifest
   validation, diagram validation, ordinals, and the renderer AGAIN (third
   rewrite in two days). Trace normal + fast + dispatched paths against
   main for drift. Check `review_tasks` interactions: ordinal collisions,
   report-path pairing, raw-task boundary matching, curate gates on forums
   ending (or starting-scope-wise) with review rounds.
3. **Refusal matrix honesty:** nonexistent round, review-of-review (explicit
   and via default scope), forums with zero rounds, dispatched/curated/
   reserved forums, empty scope after exclusion — each refused by name
   BEFORE any task creation or dispatch (no half-created rounds).
4. **Diagram/renderer:** review rounds accept `reviews` only; mixed
   ask+fast+review tree structure (cards, arrows, one curator); fixture
   byte-identity.
5. **New spec `forum-reviewer.org`:** does it hold reviewers to the delta
   contract, forbid rewriting answers, treat reports/focus as untrusted,
   and avoid assuming the reviewer authored a stage-1 answer? Is reuse of
   the existing cross-reviewer specs genuinely unfit (the implementer
   claims yes — judge it)?
6. **Test honesty:** mutation-probe the self-exclusion match and the
   review-round `extracts` rejection (revert probes before finishing).

Run what you need: full `cargo test -p orgasmic-cli --bin orgasmic`
(DEFAULT target dir — custom CARGO_TARGET_DIR breaks
`empty_private_targets_never_run_another_worktrees_binary`), clippy
`-D warnings`, prompt-spec compile test, red-then-green edits. No live
dispatches.

## Verdict contract

Write `/tmp/TASK-E1D35-review.md`: verdict first (`APPROVE`/`REJECT`,
REJECT needs a concrete reproducible defect), findings ranked with
file:line anchors and failing inputs, and answer explicitly: "Would you
merge this onto main as-is?"

Terminal action:
`orgasmic dispatch finalize --task TASK-E1D35 --summary-file /tmp/TASK-E1D35-review.md`
No `--commit`. Do not edit the branch. Exiting without finalization is a
failed run.
