# TASK-82KKQ — forum fast rounds: `--fast` skips cross-review, panel of 1 allowed

## Goal

Add a `--fast` flag to `orgasmic forum ask` and `orgasmic forum critique`:
the round runs stage 1 only — no cross-review dispatches. Works on fresh
forums and on `--forum` joins, mixes freely with normal rounds under one
forum, and works with a dispatched `--curator` on a single-round forum (the
curator then reads stage-1 reports only). Panel minimum drops from 2 to 1
for fast rounds ONLY (self-excluding cross-review needs 2+; a stage-1-only
round does not). The point: a cheap wide first pass — e.g. 10 participants,
10 dispatches instead of 20 — and "one chosen model critiques this document"
as a later round.

## Read first

- `crates/orgasmic-cli/src/forum.rs` — the whole file; you are relaxing its
  invariants, so know every place that assumes reviews exist:
  `validate_participants` (min 2), manifest round shape + `validate_manifest`
  (`cross_review_tasks.len() == count`, `promoted_report_paths == count*2`),
  diagram JSON validation (reviews required per round, exactly 3 tagged
  bullets each), `render_pipeline_svg` and `render_multi_round_svg` (review
  rows and their arrows), `render_about_run`/`render_forum_about_run`
  (counts), `compile_self_contract` + `self_curation_manifest`, the
  cross-review launch loop and its self-exclusion manifests, wait barriers,
  and every test that hardcodes `count*2`.
- `shipped/prompt-studio/prompt-specs/curator.org` / `critique-curator.org` —
  the curator contract mentions cross-review reports; a fast single-round
  dispatched curation must not instruct reading reports that don't exist.
- `shipped/skills/orgasmic/references/forum.md` — document the flag and when
  to use it.
- Recent history: merges `c74eb263` (critique), `9b76db01` (self-curation +
  multi-round) — the invariants you touch were reviewed hard there; keep the
  strictness for NON-fast rounds byte-identical.

## Binding rules

1. `--fast` is per-round: recorded in the manifest round (new field), so one
   forum can hold fast and normal rounds. Manifest for a fast round: empty
   `cross_review_tasks`, `promoted_report_paths.len() == count`.
2. Panel of 1 is accepted ONLY with `--fast`; without it, the existing 2+
   rule and its message stay exactly as they are. `--fast` with 2+ panels is
   fine too (it just skips reviews).
3. Diagram JSON: for a fast round, `reviews` (legacy) / that round's
   `reviews` array must be ABSENT or empty — a review entry for a fast round
   is a validation error (invented provenance). Non-fast rounds keep the
   exact current requirements.
4. Renderer: a fast round draws its extract cards with arrows straight to
   the next consumer (curator card) — no review row. The stored ask fixture
   `renderer_matches_stored_python_fixture` must remain byte-identical
   (normal rounds render exactly as today).
5. Dispatched-curator fast run: curator brief/manifest lists only stage-1
   reports; the compiled prompt must not reference cross-review reports for
   rounds that have none. About-run footer says `0 cross-reviews` honestly
   (or omits the clause for fast rounds — your call, state it in the report).
6. Self-curated flow unchanged otherwise: fast rounds join, get manifest
   entries, compiled contract, and are covered by `forum curate` gates
   (raw-task presence = stage-1 tasks only for fast rounds).
7. Skill docs (`references/forum.md`): document `--fast` (when: cheap wide
   first pass; single-model critique), and add one line noting a new ask
   round's `--file` may carry the shared understanding so far plus the new
   question (context-carrying rounds — docs only, no code).

## Hard constraints

- Existing non-fast behavior is frozen: every current test keeps passing
  unmodified except where a test helper needs a new struct field default.
- New tests: panel-of-1 accepted only with fast (both modes, both error
  paths); fast manifest round-trip + validate_manifest acceptance and its
  rejection of a review entry for a fast round; multi-round tree with a
  mixed fast+normal forum (card/arrow counts); curate gates on a fast-only
  forum; about-run honesty.
- `cargo fmt --all`; `cargo clippy -p orgasmic-cli --all-targets -- -D
  warnings`; full `cargo test -p orgasmic-cli --bin orgasmic` with the
  DEFAULT target dir (a custom CARGO_TARGET_DIR breaks
  `empty_private_targets_never_run_another_worktrees_binary`).
- No live billed dispatches.

## Deliverables

Report to `/tmp/TASK-82KKQ-report.md`: what changed, contract decisions
(esp. rule 5's footer wording), tests run, and the exact commands for
(a) a 3-participant fast ask opening a self-curated forum and (b) a
single-model fast critique joining it.

## Completion

Write the report, then make your terminal action:
`orgasmic dispatch finalize --task TASK-82KKQ --summary-file /tmp/TASK-82KKQ-report.md --commit`
Exiting without finalization is a failed run.
