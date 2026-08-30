# TASK-E1D35 review — `forum review` (detached stage-2 rounds)

## Verdict

**APPROVE.** Would I merge this onto main as-is? **Yes.** All acceptance
criteria are met, the refusal matrix fires before any task creation, the
blindness machinery is real (mutation-probed red), the full CLI suite is green
(299 passed / 0 failed / 1 ignored), and the ask SVG fixture test passes
unmodified. The findings below are non-blocking; the top one is a deliberate
contract choice worth revisiting, not a defect against the binding contract.

## Findings

1. **MEDIUM · correctness/blindness · `crates/orgasmic-cli/src/forum.rs:3665`** —
   Self-exclusion requires harness equality, so the same underlying model
   reviewing via a different transport receives its own stage-1 report.
   Failing input: stage 1 `--participant 'stdio,codex,gpt-5.6-luna,low'`,
   then `forum review --participant 'stdio,hermes,openai/gpt-5.6-luna,high'`.
   `same_reviewer_identity` compares `harness + vendor + model`; harness
   `codex` ≠ `hermes`, so the reviewer's own report stays in scope. This
   *is* what the dispatch brief specified ("harness + normalized
   vendor/model"), so it is contract-conformant and not grounds for REJECT —
   but it is inconsistent with the codebase's own identity notion: panel
   uniqueness (`forum.rs:2158–2163`) treats vendor+model alone as "a model
   identity". For blindness purposes the author is the model, not the
   transport. Fix direction: drop `harness` from `same_reviewer_identity`
   (vendor+model only). Effort/mode are correctly ignored already.

2. **LOW · correctness/blindness · `crates/orgasmic-cli/src/forum.rs:2129`** —
   Vendor/model comparison is case- and inner-whitespace-sensitive.
   `parse_participant` trims whole fields and maps `claude→anthropic` /
   `codex→openai`, but never casefolds and never trims around the `/` split:
   reviewer spec `stdio,claude,Anthropic/claude-fable-5,high` yields vendor
   `Anthropic` ≠ stage-1 `anthropic`, bypassing self-exclusion (and the
   panel-uniqueness gate) while the dispatch may still launch fine. Operator
   typo territory, not an attack surface. Fix direction: lowercase vendor and
   model (and trim the split halves) in `parse_participant`.

3. **LOW · design · `crates/orgasmic-cli/src/forum.rs:1476`** — A review
   round occupies a full 650px `round_height` slot but only draws ~254px of
   content (header + 190px cards), leaving a large blank band before the next
   row/curator in mixed trees. Purely visual; structure and arrows are
   correct. Fix direction: per-kind row height if the whitespace bothers
   anyone.

No HIGH findings. No unmet acceptance criteria.

## Contract check (brief items, in order)

1. **CLI surface / refusal matrix** — `run_review` (forum.rs:3728) validates
   forum id, participants (panel 1+ via `validate_participants(_, true)`),
   focus, and project, then `validate_join_request` (forum.rs:2424: unknown
   forum, wrong project, dispatched curator, already curated,
   curation-task-reserved, parent not open), then `select_review_rounds`
   (forum.rs:3642: zero rounds, missing round incl. `--round 0` via
   `checked_sub`, explicit review-of-review; default scope filters review
   rounds out), then empty-scope-after-exclusion — **all before the first
   `api.create_task`**. No half-created rounds on refusal.
2. **Blindness** — per-reviewer brief carries only `source_context`
   (reviewed rounds' questions/targets — stage-1 inputs, never outputs),
   the focus, and a per-reviewer report manifest built from stage-1 reports
   only (`round_reports` first tuple element; review rounds excluded from
   sources by `select_review_rounds`). No other reviewer's output and no
   earlier review-round report can enter scope. Self-exclusion holes: see
   findings 1–2.
3. **Manifest** — `review_tasks` is `#[serde(default)]`; the round-trip test
   also strips the field from a serialized old manifest and re-validates it
   (old manifests load). Review rounds: empty stage-1/cross lists, counts
   tied to panel, reviewed rounds must exist, precede, and be non-review
   (validated both at intake and in `validate_manifest`, so a hand-edited
   manifest can't smuggle a round-1 review into the `unreachable!` artifact
   arms). The new "repeats a model identity" manifest check mirrors the
   intake rule that already existed on main (`git show
   main:...forum.rs:1930`), so no existing on-disk manifest can newly fail.
4. **Curate gates** — `report_tasks()` folds `review_tasks` into raw-task
   coverage, ordinals (`next_task_ordinal` test expects 7), the self-curation
   manifest, and the contract text; diagram review rounds require `reviews`,
   forbid `extracts`, and reuse the existing 3-tag `?`/`+`/`=` bullet gate.
5. **Renderer** — distinct tinted review row with `data-round-kind`,
   `round-review` arrows from each reviewed round, `review-curator` arrows to
   the single curator card; the byte-identity fixture covers
   `render_pipeline_svg` (single-round dispatched ask renderer,
   forum.rs:5240), which this diff does not touch — and the test passed in
   the suite run. Existing multi-round/fast test functions are unmodified;
   only shared helpers gained the new field/shape.
6. **Non-review behavior** — ask/critique validation arms preserve main's
   exact count rules plus `review_tasks.is_empty()`; `run_forum` paths mark
   Review arms `unreachable!` and are never fed Review inputs (round 1 can
   never be a review round, enforced by validation).

## Prompt spec judgment

Reusing `cross-reviewer.org` is genuinely unfit: it hardcodes "Do not seek,
infer, or read your own stage-1 report. The manifest deliberately excludes
it" (assumes the reviewer authored one), has no focus slot, and frames a
single question. `forum-reviewer.org` keeps the identical delta contract
(`?`/`+`/`=`, no anonymous labels, untrusted-data framing, same
completion/finalize block), adds the focus as untrusted data, forbids
writing a new answer or verdict, and never assumes stage-1 authorship. The
fork is justified and faithful.

## Open Questions

- Should self-exclusion ignore harness (finding 1)? The brief said include
  it; the panel-uniqueness rule says vendor+model is the identity. Manager
  call.
- Concurrent `forum review` / round-join invocations can race on ordinals
  and the manifest file. Pre-existing pattern for all multi-round joins, not
  introduced here.

## Verification Notes

- Read the full diff (`git diff main...HEAD`, 3 files, +984/−59) and the
  surrounding unchanged machinery (`parse_participant`,
  `validate_participants`, `validate_join_request`, `round_reports`,
  `read/write_manifest`, renderer geometry, fixture test).
- `cargo test -p orgasmic-cli --bin orgasmic` (default target dir): **299
  passed, 0 failed, 1 ignored**, exit 0. Log `/tmp/E1D35-review-tests.log`.
- `cargo clippy -p orgasmic-cli --all-targets -- -D warnings`: exit 0. Log
  `/tmp/E1D35-review-clippy.log`.
- Mutation probe 1: `same_reviewer_identity → false` made
  `review_scope_excludes_the_reviewers_own_stage_one_reports` FAIL; reverted.
- Mutation probe 2: disabling the review-round `extracts` bail made
  `review_round_diagram_accepts_reviews_only_and_rejects_wrong_shape` FAIL;
  reverted. Worktree clean afterward (`git status --porcelain` empty);
  focused `forum::tests` re-run green (30 passed).
- Production-path probe: `cargo run -- forum review --forum TASK-ZZZZZ
  --participant 'stdio,claude,claude-fable-5,high'` → `Error: unknown forum
  TASK-ZZZZZ`, refused before any task creation (`Api::new` only opens a
  client). No live dispatches were run.
- Residual gaps: no automated test wires `run_review` to
  `validate_join_request` end-to-end (covered here by the production probe
  plus the shared-function tests); no test covers the case/harness identity
  gaps in findings 1–2; the ordinal race under concurrent joins is untested
  (pre-existing).

## Fix Directions

- Finding 1: compare vendor+model only in `same_reviewer_identity`
  (one-line change; add a cross-harness case to the exclusion test).
- Finding 2: casefold and trim vendor/model halves in `parse_participant`.
- Finding 3: optional per-kind row height in `render_multi_round_svg`.
