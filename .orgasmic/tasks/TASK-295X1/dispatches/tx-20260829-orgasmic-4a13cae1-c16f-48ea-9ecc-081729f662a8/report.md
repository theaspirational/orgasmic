# TASK-295X1 implementation report

## Changed

- Added `orgasmic forum critique` with required `--target-file`, optional one-line `--focus`, and the same participant, curator, source-ref, timeout, artifact, and project controls as `forum ask`.
- Parameterized the existing forum orchestration into one shared `run_forum` path. Critique uses the same task/dispatch/wait/close/promotion/evidence/finish pipeline, self-excludes each participant's stage-1 report from its cross-review manifest, and reuses `load_diagram_fields`, `render_pipeline_svg`, `render_about_run`, and artifact submission.
- Added target validation: one UTF-8 read, non-whitespace content, 64 KiB byte limit, and rejection of every orchestrator placeholder. Focus rejects empty, multiline, placeholder-bearing, and leading-`-` values.
- Generalized artifact assembly for orchestrator-owned first sections. Critique emits the escaped byte-preserving `Target` section plus labeled focus, requires `Verdict`, `Findings`, and `From target to verdict` in order, rejects decoy Target sections, rejects model SVG, requires raw task ids, and keeps the run footer last.
- Added `critic`, `critique-cross-reviewer`, and `critique-curator` shipped prompt specs, reusing `output_style_plain_english` and the existing MDX/diagram/finalize contracts.
- Updated the shipped orgasmic forum reference to document both modes.

## Contract decisions

- Kept the renderer itself unchanged so `renderer_matches_stored_python_fixture` remains byte-identical. Critique passes the focus line to it, or `critique of <basename>, N bytes` when unfocused.
- Kept the existing diagram JSON schema (`extracts`, `reviews`, `curator_summary`, optional `headline`) for both modes. Critique titles prefer the curated headline and fall back to `Multi-model critique: <focus-or-basename>`.
- Kept Ask's external JSON field `extraction_tasks`; Critique reports the sibling field `critique_tasks`. Both are projections of the same shared stage-one task vector.
- Did not add a second SVG renderer, duplicate the dispatch pipeline, or alter the three shipped Ask prompt specs.

## Verification Gates

- `cargo fmt --all -- --check` — green (`/tmp/TASK-295X1-fmt-final.log`).
- `cargo build -p orgasmic-cli` — green (`/tmp/TASK-295X1-cli-build.log`).
- `cargo clippy -p orgasmic-cli --all-targets -- -D warnings` — green (`/tmp/TASK-295X1-cli-clippy-final.log`).
- `cargo test -p orgasmic-cli --bin orgasmic` — green: 278 passed, 1 ignored (`/tmp/TASK-295X1-cli-bin-tests.log`). This includes all 9 forum tests and the byte-identical stored renderer fixture.
- `cargo test -p orgasmic-daemon --lib prompt_compiler::tests::all_shipped_prompt_specs_compile_cleanly -- --exact` — green (`/tmp/TASK-295X1-prompt-specs.log`).
- `cargo test -p orgasmic-core --test fixtures` — green: 19 passed (`/tmp/TASK-295X1-core-fixtures.log`).
- `cargo test -p orgasmic-cli --test cli_parity` — green: 7 passed (`/tmp/TASK-295X1-cli-parity.log`).
- `cargo run -p orgasmic-cli -- forum critique --help` — green; required/mirrored flags visible (`/tmp/TASK-295X1-critique-help.log`).
- Production CLI pre-dispatch probe rejected both an empty target and a target containing `__ORGASMIC_TARGET_SECTION__` with named errors (`/tmp/TASK-295X1-target-cli-validation.log`).
- `git diff --check` — green (`/tmp/TASK-295X1-diff-check.log`).

## Unmet Criteria

- None.

## Residual Risk

- Per the brief, no live billed multi-model smoke was run; the operator owns that end-to-end provider/daemon check.
- The slow `crates/orgasmic-cli/tests/dispatch.rs` integration binary and repo-wide suite were not run. The shared forum unit suite, full CLI bin tests, prompt compilation, CLI parity, build, clippy, and production pre-dispatch validation path are green.
