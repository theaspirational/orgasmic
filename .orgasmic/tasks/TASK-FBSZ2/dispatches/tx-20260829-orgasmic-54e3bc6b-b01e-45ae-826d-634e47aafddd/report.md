## Changed

- Added native `orgasmic extract` in `crates/orgasmic-cli/src/extract.rs`, wired through `main.rs`, with the Python orchestrator's flags, validation, three-stage dispatch flow, self-excluding blind reviews, report promotion, deterministic SVG rendering, guarded MDX assembly, artifact submission, and JSON result.
- Added truthful `manager dispatch-close --status done --report-only`: it records `REPORT_ONLY=true`, requires no merge SHA, reuses ordinary promotion/cleanup/lifecycle machinery, and reserves the property to the typed flag.
- Ported the Python self-tests to Rust and added the stored Python renderer fixture `crates/orgasmic-cli/tests/fixtures/TASK-FBSZ2-pipeline.svg` (SHA-256 `f68c49371c30017077dd23800bf0bff8eae491d35c888eb0ddc1f095af38314a`).
- Deleted `shipped/skills/orgasmic/scripts/multi-model-extract.py`; updated the skill routing, native extract reference, and tx schema.

## Verification Gates

- `cargo build -p orgasmic-cli` — PASS (`/tmp/TASK-FBSZ2-cli-build.log`).
- `cargo test -p orgasmic-cli --bin orgasmic` — PASS: 274 passed, 1 ignored (`/tmp/TASK-FBSZ2-cli-unit-test.log`); includes all five ported extract tests and exact Python SVG fixture parity.
- `cargo test -p orgasmic-cli --test dispatch manager_dispatch_status_close_done_with_stub_codex -- --exact` — PASS: report-only `implementer.done` closes without merge evidence and uses normal cleanup/promotion (`/tmp/TASK-FBSZ2-report-only-test-2.log`).
- `cargo test -p orgasmic-cli --test cli_parity` — PASS: 7 passed (`/tmp/TASK-FBSZ2-cli-parity.log`).
- `cargo test -p orgasmic-core --test fixtures` — PASS: 19 passed, including shipped schema parsing (`/tmp/TASK-FBSZ2-core-fixtures.log`).
- `cargo clippy -p orgasmic-cli --all-targets -- -D warnings` — PASS (`/tmp/TASK-FBSZ2-cli-clippy-2.log`).
- `target/debug/orgasmic extract --help` — PASS; documents the native verb and participant/question/artifact flags.
- Script-retirement grep and filesystem probe — PASS: script absent, no Python cache remains, no extract workaround language remains in the three prompt specs.

## Unmet Criteria

- The required cheap two-participant live smoke was not launched. The brief forbids dispatching smoke workers until the host's 1-minute load is below 4. Three durable probe runs waited about 35 minutes and never observed the threshold; samples ranged from 4.66 to 38.37 and ended at 9.24 (`/tmp/TASK-FBSZ2-load-wait.log`, `/tmp/TASK-FBSZ2-load-wait-2.log`, `/tmp/TASK-FBSZ2-load-wait-3.log`). Therefore there is no smoke parent task, subtask list, artifact id, or live `/api/tasks/:id/dispatches` readability evidence to report.

## Residual Risk

- Unit and daemon-backed integration coverage pass, but the full native extraction path against live model workers and artifact submission remains unverified until the load precondition is met.
- The full 97-test `dispatch` integration target was not run; the exact modified report-only test passed.
