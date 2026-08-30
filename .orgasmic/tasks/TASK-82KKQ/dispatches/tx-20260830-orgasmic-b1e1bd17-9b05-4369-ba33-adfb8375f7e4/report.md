# TASK-82KKQ Review — forum fast rounds (`--fast`)

## Verdict

**APPROVE.** Would I merge this onto main as-is? **Yes.** No defect found that
reproduces a wrong artifact, a relaxed normal-round gate, or a dishonest
compiled contract. All findings below are MEDIUM/LOW disclosure and test-gap
items; none blocks ship.

## Findings

- **MEDIUM correctness/test `crates/orgasmic-cli/src/forum.rs:1329`** —
  Undisclosed frozen-path behavior change that silently fixes a latent main
  defect. On main, `render_multi_round_svg` bails on `rounds.len() < 2`
  (main forum.rs:1296, introduced by ea57e7a7/TASK-9TGQS), and `run_curate`
  (main:3285) routes **every** self-curated forum through it — so
  `forum curate` on a single-round self-curated forum (the default flow) always
  failed with "multi-round diagram requires matching manifest and diagram
  rounds". This diff changes the guard to `rounds.is_empty()` and drops the
  `max_count < 2` bail, which fixes that path — necessary for fast single
  rounds, and incidentally for **normal** single rounds too. Impact: the
  "non-fast behavior frozen" rule is technically breached (error → success),
  in the correct direction, but the implementer report does not mention it and
  the normal (non-fast) single-round curate render still has no test. Fix
  direction: add one test rendering a normal 1-round manifest through
  `render_multi_round_svg`, and record the latent-defect fix in the task
  evidence so 9TGQS history is corrected.
- **LOW test `crates/orgasmic-cli/src/forum.rs:2268` and `:3102`** — The
  fast-contract surgery `contract.replace("- Cross-review tasks\n", "")`
  depends on the compiled spec emitting that exact byte sequence
  (curator.org:131, critique-curator.org:128). If the spec wording or compile
  formatting ever drifts, the replace silently no-ops and a fast curator's
  Output Contract again demands a "Cross-review tasks" line for tasks that do
  not exist. Compilation goes through the daemon (`compile_prompt`,
  forum.rs:2344), so no unit test covers the removal; this is the same
  untested-string-surgery idiom main already used at this site, so it is a
  carried-over gap, not a regression. Fix direction: a fixture/canary test on
  the spec source line, or assert the replace actually changed the string when
  `fast`.
- **LOW cosmetic `crates/orgasmic-cli/src/forum.rs:3066` and `:3201`** — For a
  dispatched fast round, `run_manifest` embeds an empty cross-review segment
  (yielding a `\n\n\n\n` run in the curator brief), and the parent-task
  evidence prints `- Cross-review tasks: ` with nothing after the colon. Both
  are honest (empty = none) but sloppy to read. Fix direction: skip the
  segment/line when the review list is empty. Not blocking.

No other findings. Specifically checked and clean:

- **Frozen-path drift:** `validate_participants` keeps the exact 2+ message for
  non-fast; `validate_manifest` keeps `count<2 / cross==count / promoted==2·count`
  for non-fast rounds (`fast` is `#[serde(default)]`, so old manifests load as
  normal and stay fully gated). `render_pipeline_svg` with `fast=false` is
  byte-identical (`.max(0)` no-op, `row_left = margin`); the stored ask fixture
  test passed unmodified. The multi-round `.max(544)` width floor can never
  bind for normal rounds (min panel 2 → width 598).
- **Invariant leaks:** a normal round with 1 participant, missing reviews, or
  count drift is still rejected at intake and at every manifest read/write
  (`read_manifest`/`write_manifest` both call `validate_manifest`). A fast
  round cannot smuggle a review through either diagram shape:
  `parse_diagram_fields` keys the review set off the round's (empty)
  `cross_review_tasks`, so both the legacy top-level shape and the `rounds`
  shape reject invented entries; `render_pipeline_svg(fast)` additionally
  bails on non-empty reviews. A review task added to a fast manifest round
  fails the `cross_review_tasks.len() != 0` gate even if a matching promoted
  path is added.
- **Ordinals and report pairing:** `next_task_ordinal` scans actual task
  ordinals, so mixed fast+normal joins number correctly (test asserts 6 for a
  1-task fast round + 4-task normal round). `round_reports` splits
  `promoted_report_paths[..count]`/`[count..]`, which is exact for fast
  (`len == count`); `self_curation_manifest`'s chain-zip pairs stage-1 tasks
  with their paths and its debug assert matches the new arity.
- **Renderer:** mixed forum test pins card/arrow counts (fast extract cards
  arrow straight to curator via `data-arrow="extract-curator"`, no review row
  or CROSS-REVIEW pill for fast rounds, `2 · CURATE` relabel only when fast);
  single fast round with 1 participant renders with the 464/544 width floors.
- **Prompt-spec honesty:** curator/critique-curator spec edits are confined to
  manifest-driven reading and "never invent a review" guidance — for normal
  rounds the manifest always names review tasks, so the instructions are
  equivalent to main's. Stage-1 specs (extractor.org:20, critic.org:23) never
  promise a cross-review will occur, so fast stage-1 briefs are honest.
  Dispatched fast About footer says `0 cross-reviews` (test-pinned);
  self-curated footers list per-round task ids (empty review list = nothing to
  misstate). `compile_self_contract` strips the Cross-review output bullet only
  when **all** rounds are fast — correct for mixed forums.
- **Skill docs:** `--fast` documented (panel-minimum line, when-to-use), and
  the required context-carrying `--file` line for later ask rounds is present.

## Open Questions

- None affecting the verdict. Whether single-round self-curated curation was
  ever exercised live between 9b76db01 and this branch may be worth a glance
  at the live ledger (if some live single-round forum "curated fine", my
  main-is-broken reading would need revisiting — but the code path on main is
  unambiguous).

## Verification Notes

All run in this review worktree (clean checkout of a05326dc), default target
dir:

- `cargo test -p orgasmic-cli --bin orgasmic` — **291 passed, 0 failed,
  1 ignored** (`/tmp/TASK-82KKQ-review-tests.log`).
- `cargo test -p orgasmic-cli --test cli_parity` — 7 passed.
- `cargo clippy -p orgasmic-cli --all-targets -- -D warnings` — clean.
- **Mutation probe A** (review-count gate): replaced
  `review_count = if round.fast { 0 } else { count }` with
  `round.cross_review_tasks.len()` →
  `fast_manifest_round_trip_accepts_stage_one_only_and_rejects_reviews`
  FAILED (its malformed-normal assert). Reverted.
- **Mutation probe B** (panel gate): replaced the `!fast && len < 2` check
  with `is_empty()` → `panel_of_one_requires_fast_for_ask_and_critique_rounds`
  FAILED. Reverted. `git status --porcelain` empty after reverts.
- Read the full diff (787 lines, 4 files) plus the untouched loaders/gates it
  leans on (`parse_diagram_fields`, `load_multi_round_diagram_fields`,
  `validate_manifest`, `validate_join_request`, `next_task_ordinal`,
  `round_reports`, `run_curate`, the dispatched-curator branch of `run_forum`)
  against main.
- **Not run:** live billed dispatch (forbidden by the brief). The dispatched
  fast path (acceptance criterion 2) is verified by code trace plus the
  renderer/footer/contract unit tests, not end to end — same residual risk the
  implementer declared.

## Fix Directions

1. Follow-up (non-blocking): test for normal single-round
   `render_multi_round_svg`, plus a note in task evidence that this branch
   fixes the latent single-round curate failure shipped in TASK-9TGQS.
2. Follow-up (non-blocking): canary-test or post-condition-assert the
   `- Cross-review tasks` contract surgery.
3. Optional polish: suppress the empty cross-review segment in the fast
   dispatched curator brief and the empty evidence list line.
