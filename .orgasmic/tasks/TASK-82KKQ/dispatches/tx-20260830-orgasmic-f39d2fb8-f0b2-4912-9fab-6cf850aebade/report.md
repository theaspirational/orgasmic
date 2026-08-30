# TASK-82KKQ Report

## Changed

- Added per-round `--fast` handling shared by `forum ask` and `forum critique`, including fresh forums, `--forum` joins, and dispatched-curator runs.
- Fast rounds accept one or more distinct participants, dispatch only stage 1, record `fast: true`, empty `cross_review_tasks`, and one promoted report path per participant. Normal rounds retain the two-participant minimum and full stage-1/cross-review count invariants.
- Manifest, diagram JSON, single- and multi-round rendering, self-curation, raw-task gates, and curator contracts now accept fast rounds while rejecting invented review provenance. Mixed fast and normal diagrams route fast extract cards directly to the curator and retain normal review rows.
- Dispatched fast footers explicitly say `0 cross-reviews`. Self-curated forum footers keep their existing per-round task lists and omit a separate cross-review count clause.
- Curator specs instruct reading every report named in the manifest and permit `reviews` to be absent or empty only when no review tasks are named. Fast-only compiled contracts omit the `Cross-review tasks` output line.
- Updated forum skill documentation for `--fast` use cases and context-carrying ask rounds via `--file`.
- Added focused regressions for one-participant intake in ask and critique, fast manifest round-trip/rejection, optional fast review JSON, mixed renderer cards/arrows, fast-only curate gates, dispatched fast rendering/footer honesty, and the unchanged stored normal ask SVG fixture.

## Contract Decisions

- `fast` is a boolean field on each manifest round with `serde(default)`, so older manifests load as normal rounds.
- A fast round's diagram must omit `reviews` or use `[]`; any review entry is invalid provenance.
- The dispatched fast About footer keeps the existing sentence shape and reports `0 cross-reviews`; self-curated footers omit report-count clauses as before.
- No live billed dispatch was run, per the brief.

## Verification Gates

- `cargo fmt --all` — pass (`/tmp/TASK-82KKQ-fmt-final.log`).
- `cargo clippy -p orgasmic-cli --all-targets -- -D warnings` — pass (`/tmp/TASK-82KKQ-clippy-final.log`).
- `cargo test -p orgasmic-cli --bin orgasmic` with the default target directory — pass: 291 passed, 0 failed, 1 ignored (`/tmp/TASK-82KKQ-bin-tests-final.log`).
- `cargo test -p orgasmic-core --test fixtures` — pass: 19 passed (`/tmp/TASK-82KKQ-shipped-gates-final2.log`).
- `cargo test -p orgasmic-cli --test cli_parity` — pass: 7 passed (`/tmp/TASK-82KKQ-shipped-gates-final2.log`).
- `cargo run -q -p orgasmic-cli -- forum ask --help` and `forum critique --help` exposed `--fast` on both modes (`/tmp/TASK-82KKQ-{ask,critique}-help.txt`).

## Exact Example Commands

### Three-participant fast ask opening a self-curated forum

```bash
orgasmic forum ask --fast \
  --file /tmp/question.txt \
  --participant 'stdio,hermes,openai/gpt-5.6-luna,low' \
  --participant 'stdio,hermes,google/gemini-3.7-flash,low' \
  --participant 'stdio,claude,claude-haiku-4-5-20251001,low'
```

Use the returned `forum` id in place of `TASK-XXXXX` below.

### Single-model fast critique joining it

```bash
orgasmic forum critique --fast \
  --forum TASK-XXXXX \
  --file /tmp/critique-target.md \
  --participant 'stdio,hermes,openai/gpt-5.6-luna,low'
```

## Unmet Criteria

- None.

## Residual Risk

- The production dispatch path was not exercised end to end because the brief forbids live billed dispatches. Its stage/manifest/contract/render/curate branches are covered by focused unit tests and the full CLI binary suite.
