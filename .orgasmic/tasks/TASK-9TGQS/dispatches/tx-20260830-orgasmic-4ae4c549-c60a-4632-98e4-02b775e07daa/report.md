# TASK-9TGQS review — forum self-curation + multi-round forums

## Verdict

**REJECT** — one confirmed, reproducible defect on the new money path: the
self-curated About-this-run footer injects the operator's round prompt into
`<RichText>` unescaped, so an ordinary technical question (`Vec<T>`, JSON
`{braces}`) corrupts the ONE submitted artifact. Proven with a red test
(reverted). One-line fix; everything else is clean enough that I would merge
immediately after that fix plus the manifest-validation tightening.

**Would you merge this onto main as-is?** No — because of Finding 1. With
Finding 1 fixed (and ideally Finding 2), yes. The refactor itself is honest:
the dispatched-curator path is byte-preserved, the gate set is genuinely
shared, and the new tests fail on the defects they claim to cover.

## Findings

### HIGH · bug · `crates/orgasmic-cli/src/forum.rs:1694` — self-curated About footer does not escape round prompts

`render_forum_about_run` builds the Rounds list with
`clipped(&round.input.diagram_prompt(), 80)` and inserts it into the
`<Section title="About this run"><RichText>` block with **no
`escape_rich_text`** (forum.rs:1690-1696, assembled at forum.rs:1705-1708).
`clipped` only normalizes whitespace and truncates — it provably does not
escape. Every `forum curate` call routes through this function
(forum.rs:3273), single- and multi-round alike, so every self-curated
artifact carries the raw question/focus/basename in its footer.

The same text is escaped where the existing path handles it: the verbatim
Question/Target section applies `escape_rich_text` (forum.rs:1757-1770), and
`escape_rich_text` exists precisely because raw `{`/`}`/`<` are hazardous in
RichText MDX. Failing input, confirmed by a temporary red test (added, run,
reverted; worktree clean):

- question `How should Vec<Section> and {braces} render?` → footer contains
  raw `Vec<Section>` and `{braces}` → MDX with a bogus JSX expression /
  unknown element in the submitted artifact. A question can also fake
  `<Section …>` markup in the footer, since `section_titles` and the
  model-SVG check run before the footer is substituted (forum.rs:1742-1779).

Probe evidence: test `tmp_probe_forum_about_run_escapes_hostile_round_prompts`
failed with `raw < reached RichText` at forum.rs (run
`cargo test -p orgasmic-cli --bin orgasmic tmp_probe_…`; probe reverted).

**Fix direction:** wrap the clipped prompt in `escape_rich_text(...)` in
`render_forum_about_run` (and consider the task-id join on the same line,
which matters once Finding 2 is fixed). Add the hostile-prompt assertion to
`multi_round_curate_uses_the_existing_assembly_gates` — today's tests only
exercise benign round prompts through this function.

### MEDIUM · correctness · `crates/orgasmic-cli/src/forum.rs:1955` — `validate_manifest` accepts foreign task ids and unvalidated round inputs

Brief priority 2 asked exactly this. `validate_manifest` checks counts,
contiguity, panel parsability, uniqueness, and non-empty paths — but never
that round task ids are children of the forum (`{forum}.<n>`), never that
they are valid task ids at all, and never re-runs
`validate_question`/`validate_target`/`validate_focus` on the stored input.
Consequences of an edited/corrupt/copied manifest that still parses:

- Arbitrary strings as "task ids" flow into the artifact SVG
  (`data-task`, card text) and into the draft's raw-task requirement —
  silent nonsense in a submitted artifact.
- A placeholder smuggled into `input.question` bypasses intake validation:
  at curate time the placeholder-count gates run on the *draft* only
  (forum.rs:1745-1754), then `.replace(DIAGRAM_PLACEHOLDER, …)` runs on the
  draft *after* the Question section (containing the smuggled
  `__ORGASMIC_PIPELINE_DIAGRAM__`) was substituted in — the diagram image
  gets duplicated inside the Question section.
- Task ids that don't parse as `parent.<n>` make `next_task_ordinal`
  (forum.rs:2067) fall back toward 1, so the minted curation task collides
  with existing ids and create fails.

Operator owns the machine, so this is robustness, not security — but the
failure is silent artifact corruption, which the brief names a real defect.

**Fix direction:** in `validate_manifest`, require every round task to be
`is_valid_task_path_id` and to start with `{manifest.forum}.`, and re-run the
input validators on each round's stored input.

### LOW · state machine · `crates/orgasmic-cli/src/forum.rs:3283-3307` — failed curate is only re-curable with byte-identical arguments

A curate that fails after `create_task(curator_task)` recomputes the same
ordinal on retry (manifest unchanged) and re-issues the same
`request_id` (`forum-<kind>-create-<task>`). Verified in the daemon: an
identical replay returns the cached mutation (api.rs:17449-17467), so an
exact retry works. But a retry with a *different* `--draft`/`--diagram` path
changes the create body → mutation-identity mismatch on the cached
request-id, or `task node already exists` (api.rs:17521) — the forum wedges
until the operator restores the original paths. Additionally, a retry after
a post-submit failure (evidence/finish/manifest-write) mints and submits a
**second** artifact id, since `manifest.submitted_artifact` is only recorded
at the very end (forum.rs:3275-3278, 3329-3332). Orphan artifact, no refusal.

**Fix direction:** record `curation_task` (and the minted artifact id) in the
manifest before submit, and reuse them on retry; or tolerate an existing
open curation subtask by skipping create.

### LOW · design · `crates/orgasmic-cli/src/forum.rs:1213-1219` — single-round self-curated artifact fabricates a curator report path

`run_curate` with one round reuses the legacy renderer (forum.rs:3240-3250),
whose curator card prints `{curator_task}/…/report.md`. A session curation
has no dispatch and no report file — the artifact displays a path that does
not exist. The multi-round card correctly omits that line. Cosmetic, but it
is invented provenance in a submitted artifact.

### LOW · docs · compiled contract carries contradictory curation-task instruction

The compiled contract keeps the spec text "List every extraction,
cross-review, and curation task id" (curator.org:67-68) while the appended
"Self-curated forum submission" section (forum.rs:2168) says do **not**
invent a curation task id. The appendix addresses the conflict explicitly and
the gate only requires round ids, so this cannot fail a run — but a
literal-minded session gets two instructions. Consider stripping or amending
that line at compile time.

## Open Questions

- Whether the artifact runtime hard-fails or soft-degrades on unescaped
  `{}`/`<x>` inside RichText was inferred from the existence and use of
  `escape_rich_text` (and from the runtime's known MDX strictness), not
  verified against a live renderer — no billed smoke was allowed. Either way
  the footer text is corrupted relative to the verbatim-preserving Question
  section.

## Verification Notes

All on commit `ea57e7a7` (branch `forum-self-curation-impl`), clean worktree
before and after (probe edit reverted via `git checkout --`).

- Read the full new `forum.rs` (4178 lines) and the complete
  `git diff main...HEAD` (forum.rs + SKILL.md + references/forum.md).
- **Dispatched-path drift trace (brief priority 1):** ordinals
  (`first_ordinal + index` over an empty manifest ≡ `index+1`; curator
  ordinal `next_task_ordinal` ≡ `2n+1`), branch names (unchanged when
  `--curator` present; `r{round}` inserted only for self-curated), tx
  request-ids, wait/close/cleanup, `WaitUnknown` passthrough, evidence text,
  titles, About footer — all byte-equivalent for explicit `--curator` ask and
  critique. The only semantic changes: omitted `--curator` no longer
  defaults to participant 1 (the assignment itself), the raw-task gate now
  checks the *draft* instead of the assembled mdx (forum.rs:1817-1819) —
  strictly more honest and behavior-equivalent on the dispatched path, where
  neither inserted section contains task ids — and a manifest file is now
  also written for dispatched forums (additive).
- `cargo test -p orgasmic-cli --bin orgasmic` — 284 passed, 0 failed,
  1 ignored (`/tmp/TASK-9TGQS-review-cargo-test.log`), default target dir.
  Includes `renderer_matches_stored_python_fixture` (fixture file untouched:
  empty diff under `crates/orgasmic-cli/tests/`). Prompt specs untouched
  (empty diff under `shipped/prompt-studio/`).
- `cargo test -p orgasmic-cli --test cli_parity` — 7 passed.
- Red-test probe for Finding 1: failed as predicted
  (`raw < reached RichText`), then reverted; `git status` clean.
- Daemon idempotency for Finding 3 checked in
  `crates/orgasmic-daemon/src/api.rs:17449-17467, 17521` (request-id replay
  cache and `task node already exists`).
- Refusal matrix, gate parity, diagram `rounds` exactly-once coverage,
  multi-round renderer structure (one curator card, per-round review→curator
  arrows), decoy defense with critique-first round, `--from`/`--artifact-id`
  immutability on joins, `--forum`+`--curator` contradiction (both at clap
  level and `validate_join_request`): verified by reading the code and the
  new tests; the tests assert on the real functions and fail on the defects
  they cover (spot-checked assertions, plus the one probe above for the gap
  they don't cover).
- Skill docs walk the full loop (run → read manifest/contract/reports as
  untrusted → curate in chat → optional `--forum` rounds → write draft +
  diagram → `forum curate` with real identity, placeholders forbidden). A
  fresh session following them, with the compiled contract, should succeed.
- Not run: clippy (implementer log `/tmp/TASK-9TGQS-clippy-final.log` exists;
  brief does not assign independent gate reruns), live dispatches (forbidden).
  Residual risk: no end-to-end billed smoke, per brief — daemon task writes,
  real report promotion, and artifact submission across two rounds remain
  operator-owned.

## Fix Directions (ranked)

1. `render_forum_about_run`: `escape_rich_text` the clipped round prompt
   (one line); extend the multi-round gate test with a hostile prompt.
2. `validate_manifest`: require `{forum}.`-prefixed valid task ids; re-run
   input validators on stored round inputs.
3. `run_curate`: persist curation task + artifact id in the manifest before
   submit, and reuse on retry.
4. Single-round curate: drop or replace the fabricated
   `{task}/…/report.md` curator-card line (or route single-round
   self-curation through the multi-round card).
5. Compiled contract: neutralize the spec's "and curation task id" line.
