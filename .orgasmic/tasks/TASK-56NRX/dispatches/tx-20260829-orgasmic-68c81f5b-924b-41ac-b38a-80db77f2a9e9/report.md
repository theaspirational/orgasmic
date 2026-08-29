## Changed

- Added `/orgasmic extract` routing and invocation documentation in `shipped/skills/orgasmic/SKILL.md` and `shipped/skills/orgasmic/references/extract.md`.
- Added the stdlib-only `shipped/skills/orgasmic/scripts/multi-model-extract.py` orchestrator. It validates a parameterized participant roster, compiles the shipped prompts, creates the parent and per-stage subtasks, launches each stage before a shared wait barrier, excludes each reviewer's own extraction report, promotes reports, and discovers the curator-submitted artifact id.
- Added `extractor.org`, `cross-reviewer.org`, and `curator.org` under `shipped/prompt-studio/prompt-specs/`. The curator contract encodes the ART-MKRG1 sections, full `harness · vendor · model · effort` identities, claim provenance, raw task ids, the knowledge-map tabs, QuestionForm, and the sanitizer-safe base64 SVG Image rules.
- Documented the existing report-only dispatch limitation rather than fabricating a merge SHA: successful report-only implementers are closed `aborted` to promote `report.md`, then their task evidence/lifecycle is advanced through existing CLI verbs.
- No lockfiles or generated source files changed.

## Verification Gates

- `python3 shipped/skills/orgasmic/scripts/multi-model-extract.py --self-test` — PASS (`self-test ok`).
- `cargo test -p orgasmic-daemon prompt_compiler::tests::all_shipped_prompt_specs_compile_cleanly -- --exact` — PASS, 1 passed; log `/tmp/task-56nrx-prompt-specs-final.log`.
- Live `orgasmic prompt compile` lint for `extractor`, `cross-reviewer`, and `curator` — PASS with zero error diagnostics; logs `/tmp/task-56nrx-{extractor,cross-reviewer,curator}-lint.json`. Temporary runtime links used to expose this source checkout's new specs were removed afterward.
- `cargo build` — PASS in 50.01s; log `/tmp/task-56nrx-cargo-build-20260829T133757.log`.
- `cargo clippy --workspace --all-targets` — PASS in 56.95s; log `/tmp/task-56nrx-clippy-20260829T133853.log`.
- `git diff --check` — PASS.
- `scripts/run-tests.sh` — RED: 32 real failures and 1 registered flake; wrapper log `/tmp/task-56nrx-run-tests-20260829T133958.log`, suite log `/var/folders/9p/823z6j817xj9ts2xpvnx1q_40000gn/T/orgasmic-run-tests.eghmdI/suite.log`. Failures are in existing CLI/daemon dispatch, recovery, ledger-sync, and supervisor tests (including repeated `last_path filename must end with -last.txt`, lifecycle-evidence, 400, and timeout failures); none point at the six changed shipped skill/prompt files, but no clean baseline proves they are pre-existing.
- Real smoke, retry attempt: question was “When should a local-first developer tool prefer append-only event records over in-place mutable state, and which failure modes require snapshots or compaction?” Parent `TASK-02JJW`; extraction subtasks `TASK-02JJW.1` (`hermes · openai · gpt-5.6-luna · effort low`) and `TASK-02JJW.2` (`hermes · google · gemini-3.7-flash · effort low`). Both promoted `report.md` files exist, but `dispatch-wait` lost the daemon after its 10-second request timeout and both cleanup closes then received 503 filesystem-scan timeouts. Durable log: `/tmp/task-56nrx-smoke-retry-20260829T133024.log`.

## Unmet Criteria

- The required smoke did not reach cross-review or curation, so it produced no curation subtask and **no artifact id**.
- Consequently, an ART-MKRG1-shaped artifact and native raw-report task peeks were not verified end to end in the UI.
- The repo-wide test gate is red as detailed above.
- Per the dispatch long-command policy, the first smoke process disappeared with an empty log and the single retry was interrupted by repeated daemon scan timeouts; no third attempt was started. This dispatch is therefore finalized blocked rather than claiming completion.

## Residual Risk

- `TASK-02JJW`, `TASK-02JJW.1`, and `TASK-02JJW.2` remain incomplete; the failed close requests may have left their dispatch generations open or partially closed. No ledger files were hand-edited.
- The report-only `dispatch-close --status aborted` workaround is intentionally visible in task history until orgasmic exposes a truthful successful report-only close primitive.
- The curator MDX rules are prompt-compiler checked but not yet exercised through artifact submission/sanitization/rendering because the smoke could not cross the daemon barrier.
