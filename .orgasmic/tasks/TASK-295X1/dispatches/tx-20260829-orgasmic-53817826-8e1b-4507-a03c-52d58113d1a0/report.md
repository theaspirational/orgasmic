# Review — TASK-295X1: `orgasmic forum critique` (Critic mode)

APPROVE

Would I merge this onto main as-is? **Yes.** No blocking or medium defects found. The refactor of ask into `run_forum` is behavior-preserving on every path I traced, the assembly defenses hold against the attacks in the brief, and all claimed gates reproduce green. Findings below are LOW.

## Findings

- **LOW test forum.rs:1090** — the `draft.contains(other_placeholder)` guard has no covering test. Mutation probe: replacing the condition with `|| false` leaves all 9 forum tests green (`cargo test -p orgasmic-cli --bin orgasmic forum` → 9 passed). The guard is defense-in-depth (an unreplaced cross-mode placeholder would render as literal text, not be substituted), so no exploit — but if it regresses, nothing catches it. Fix direction: add one assembly case per mode where the draft contains the *other* mode's placeholder and assert the "each orchestrator placeholder once" rejection. Mutation reverted; worktree clean (`git status --short` empty).
- **LOW correctness (deliberate tightening, flagging for the record) forum.rs:1149-1160** — required-section *order* enforcement is new and now applies to **ask** as well; on main, an ask draft with `Knowledge map` before `Final answer` assembled fine, now it fails. The shipped ask curator spec (curator.org:58-77) already mandates exactly this order, so compliant curators are unaffected. Not a defect; strictly narrows accepted curator drafts.
- **LOW correctness (same category) forum.rs:1237-1246** — `validate_question` now rejects `__ORGASMIC_TARGET_SECTION__`/`__ORGASMIC_RUN_STATS__` in ask questions (main only rejected QUESTION/DIAGRAM). RUN_STATS in a question previously failed late with a confusing verbatim-mismatch error; now it fails up front. TARGET previously passed harmlessly. Tightening on a reserved namespace; fine.
- **LOW perf forum.rs (`read_target`)** — the 64 KiB cap is checked after `read_to_string` reads the whole file, so a multi-GB `--target-file` is fully buffered before rejection. Operator-local CLI, memory-only cost. Fix direction if desired: `fs::metadata` length check before reading.
- **LOW test/docs** — the reader-feedback `QuestionForm` is part of the brief's artifact contract but is enforced only by the prompt spec (critique-curator.org:79-80), not by `assemble_artifact`. Exact parity with ask (never assembly-enforced there either), so not a regression — naming it as the residual enforcement gap.
- **Pre-existing, not new** — `task_is_present` (forum.rs:1012) rejects a task id whose only occurrence is followed by a sentence period (`TASK-X.1.`), because `.` could begin a deeper subtask suffix. Affects ask identically on main; noting only so it isn't attributed to this change.

## Refactor-safety audit (priority 1)

Traced old `run_ask` (main) against new `run_forum` line by line via `git diff main...HEAD`:

- Parent task title/description/acceptance, `short_question` derivation (`split_whitespace` join + `chars().take(100)`), artifact title fallback, `submit_artifact` args (`input.content()` = question), evidence text, `finish_task`, `eprintln!("parent_task=…")` — all byte-equivalent for ask.
- Task-state flow, tx request-ids (`forum-ask-*` strings preserved via `kind.slug()`), wait barriers, `close_and_finish`, `mark_closed`, self-excluding cross-review manifests (`other_index != index`), tempdir prefix — unchanged.
- Error cleanup tail (forum.rs:2050-2057): best-effort close on failure with `WaitUnknown` passthrough (skip close) — identical to main.
- External JSON: `AskResult` keeps `extraction_tasks`; `CritiqueResult` uses sibling `critique_tasks`. Ask prompt specs (`curator.org`, `extractor.org`, `cross-reviewer.org`) have zero diff vs main.

## Assembly-bypass audit (priority 2)

- Decoy first section: `section_titles` (forum.rs:990) matches any `<Section` prefix, including `<Section  title=` and `<SectionX title=`, so a decoy titled `Target`/`Question` becomes the *first* matching offset and fails the `starts_with(&first_section)` verbatim check. Covered by tests (two-space decoy, `Preface` decoy, ask decoy). A decoy can only pass by being byte-identical to the genuine escaped target — i.e., not a decoy.
- Placeholder smuggling: all four placeholders rejected in target, focus, and question (`contains_orchestrator_placeholder`); `escape_rich_text` (`& < > { }`) cannot reconstruct one. Cross-mode placeholder in the draft rejected at forum.rs:1090.
- `<svg`/`data:` URIs: `contains_model_svg` runs on the draft and diagram JSON before substitution; escaped target/focus cannot introduce `<svg` afterward (`<` → `&lt;`).
- Run-stats-last: draft-level `trim_end().ends_with(RUN_STATS_PLACEHOLDER)` preserved; tested.
- Task-id boundaries: boundary-aware `task_is_present` unchanged; suffix-trick test present.

## Validation audit (priority 3) — production-path probes

Real CLI runs (no daemon contact — validation precedes `Api::new`):

- whitespace-only target → `target file must not be empty`
- target with `__ORGASMIC_RUN_STATS__` → `target file must not contain orchestrator placeholders`
- `--focus $'two\nlines'` → `focus must be one line`
- `--focus --` → `focus must not start with '-'`

Unit level: exactly-64 KiB accepted, 64 KiB+1 rejected, non-UTF-8 file rejected with `as UTF-8` context, all four placeholders rejected. BOM/CRLF pass through byte-preserved, which matches the verbatim contract; symlinks allowed (plain `read_to_string`). Target deliberately not trimmed (byte-preserving), question still trimmed — matches contract.

## Prompt specs (priority 4)

`critique-curator.org` binds the full contract: TARGET placeholder as first line with "never write your own Target section", Verdict/Findings/From-target-to-verdict order, QuestionForm, RUN_STATS as last line, no `<svg`/`data:image/svg+xml`, exact diagram JSON schema with headline ≤80, finalize without `--commit`, and a Security section marking target/focus/manifest/reports untrusted. `critic.org` and `critique-cross-reviewer.org` are report-only, self-exclusion stated, untrusted-data language present. `node.extra_prompt` is a first-class compiler variable (crates/orgasmic-daemon/src/prompt_compiler.rs:508, default "not set"), and the unfocused value `(none)` matches the specs' "`(none)` means no steer".

## Test honesty (priority 5)

The new tests fail on the defects they claim to cover: decoy, misplaced-first-section, out-of-order sections, and title-precedence assertions all target the exact bail strings, and the hostile-target case asserts the escaped bytes land in the output. The one exception is the untested `other_placeholder` guard (finding 1, proven by mutation).

## Verification Notes

All run by me in the review worktree (`forum-critique-review` @ 7d4b7916):

- `cargo test -p orgasmic-cli --bin orgasmic` — 278 passed, 1 ignored.
- `cargo test -p orgasmic-cli --bin orgasmic renderer_matches_stored_python_fixture` — 1 passed (ask fixture byte-identity).
- `cargo clippy -p orgasmic-cli --all-targets -- -D warnings` — clean.
- `cargo test -p orgasmic-daemon --lib prompt_compiler::tests::all_shipped_prompt_specs_compile_cleanly -- --exact` — 1 passed.
- Four production CLI validation probes (above).
- One mutation probe (guard at forum.rs:1090 → `|| false`), reverted; `git status --short` clean.
- Not run (per brief): live billed dispatches; slow `tests/dispatch.rs` integration binary; full workspace suite. Residual risk stays where the implementer put it: no end-to-end multi-model smoke has exercised the critique curator against the assembly gate with real model output.

## Open Questions

- None blocking. Whether ask's newly-enforced section order should be called out in a changelog note is a manager taste call; the shipped curator spec already required it.

## Fix Directions

1. Add two assembly test cases asserting cross-mode placeholder rejection (finding 1) — a few lines each in the existing tests.
2. Optionally stat the target file before reading to enforce the 64 KiB cap pre-read.
