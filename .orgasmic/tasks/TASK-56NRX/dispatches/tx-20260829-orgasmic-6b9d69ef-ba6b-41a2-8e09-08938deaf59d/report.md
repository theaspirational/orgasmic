## Changed

- `shipped/skills/orgasmic/scripts/multi-model-extract.py` now owns final artifact assembly: it validates curator JSON, renders the ART-MKRG1-style SVG deterministically for 2..N participants, injects the base64 `Image`, injects and verifies the first `Question` section verbatim, rejects model-authored SVG, and submits the artifact. `--artifact-id` supports deliberate version resubmission.
- `shipped/prompt-studio/prompt-specs/curator.org` now limits the curator to prose MDX with two orchestrator placeholders plus bounded extract/review/curator JSON fields; it no longer authors SVG or submits artifacts.
- `shipped/skills/orgasmic/references/extract.md` documents deterministic assembly and the existing-artifact option.
- Completed the required cheap production run: parent `TASK-DAACG`; extraction `TASK-DAACG.1` / `.2`; blind cross-review `TASK-DAACG.3` / `.4`; curation `TASK-DAACG.5`; resubmitted `ART-DSKQY` as version 3. Temporary installed-runtime prompt symlinks and curator `/tmp` outputs were removed.

## Verification Gates

- Load sequencing: `/tmp/task-56nrx-load-wait.log` records the smoke starting after 1-minute load reached `3.6416`; the focused cargo gate completed before the load wait and no cargo command overlapped the smoke.
- `python3 shipped/skills/orgasmic/scripts/multi-model-extract.py --self-test` — PASS. It renders 2- and 3-participant fixtures and asserts scaled width/viewBox/height, `2N+2` cards, four pills, deterministic text-node counts, `? / + / =` deltas, vendor colors, inline text styles, no `<style>`, safe verbatim Question injection, and rejection of model-authored SVG.
- `cargo test -p orgasmic-daemon prompt_compiler::tests::all_shipped_prompt_specs_compile_cleanly -- --exact` — PASS (`1 passed`); durable log `/tmp/task-56nrx-prompt-test.log`.
- Live `orgasmic prompt compile curator` against the temporarily staged runtime spec — zero error diagnostics, one Question placeholder, one diagram placeholder, no `artifact submit`, one finalization instruction. Compile capture: `/tmp/task-56nrx-curator-compile.json`.
- Production smoke — PASS, exit 0; durable log `/tmp/task-56nrx-smoke-r3.log`. All six tasks are `done`; every promoted report is non-empty via the task-dispatch API.
- Authenticated artifact API verification — PASS; `/tmp/task-56nrx-artifact-verify-v3.log` proves `ART-DSKQY` version 3 is submitted, Question is the first Section and matches the 159-character input verbatim, all five raw task ids are present, and the decoded SVG is 598x1000 with card counts `{prompt:1, extract:2, review:2, curator:1}`, 4 pills, 48 text nodes, two of each delta glyph, five existing record paths, all text styling inline, and no style block. The decoded production SVG was also rendered through `rsvg-convert` and visually checked at `/tmp/task-56nrx-artifact-v3.png`.
- Smoke hygiene: `/tmp/task-56nrx-smoke-hygiene-r3.log`; per-task `dispatch-status` has no open smoke generation (only the retained TASK-56NRX worktree notice), and the API returns exactly one promoted record for each smoke subtask.
- `python3 -m py_compile ...`, `git diff --check` — PASS.

## Unmet Criteria

- None.

## Residual Risk

- The full `scripts/run-tests.sh` suite was not rerun in round 3. Round 2 already baselined its 27 serial failures as identical on clean main and this branch at `0f46d34f`; this round used the named prompt-spec gate, extended self-test, and real production/API path instead of repeating that unrelated red suite.
- The existing report-only close workaround remains unchanged: successful report workers close `aborted` only to promote reports, then advance through normal task states.
