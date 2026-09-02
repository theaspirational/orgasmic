## Changed

- Added `TaskHeading.verify_artifact` for `:VERIFY_ARTIFACT:` and projected it through task GET JSON.
- Kept the existing generic task/node property writer; `VERIFY_ARTIFACT` is now a recognized task schema key and documented on create/update.
- Made single-task `orgasmic verify` resolve `:VERIFY_ARTIFACT:` before `verify/<TASK-ID>`, while explicit `--artifact` still wins and absent properties keep the convention.
- Made successful `manager dispatch-close --status done` load each closing task's claimed artifact before merge evidence or cleanup; missing and malformed artifacts are refused with the task and artifact path in the error.

## Verification Gates

- `cargo test -p orgasmic-core --test fixtures task_heading_parses_verify_artifact` — 1 passed.
- `cargo test -p orgasmic-cli --bin orgasmic verify::tests::` — 21 passed, including property preference, convention fallback, and missing/unloadable artifact refusals.
- `env -u ORGASMIC_RUN_ID -u ORGASMIC_HOME cargo test -p orgasmic-daemon --lib task_create_accepts_resolvable_reference_and_non_reference_property` — 1 passed; POST create then GET round-tripped `verify_artifact`.
- `cargo build -p orgasmic-cli` — passed.
- `cargo clippy --workspace --all-targets -- -D warnings` — passed.
- `cargo fmt --all --check` — passed.
- `git diff --check` — passed.

Durable logs: `/tmp/task-dtcbp-core-test-1788362148.log`, `/tmp/task-dtcbp-cli-verify-tests-final-1788362430.log`, `/tmp/task-dtcbp-daemon-api-test-1788362274.log`, `/tmp/task-dtcbp-build-1788362369.log`, `/tmp/task-dtcbp-clippy-final-1788362447.log`, `/tmp/task-dtcbp-fmt-check-final.log`.

## Unmet Criteria

- None.

## Residual Risk

- The focused checks exercise the production parser/loader and task HTTP round-trip, but do not run the full dispatch-close integration binary. The repository gotcha forbids the full workspace/test sweep on this laptop; no CI wiring was added per the task non-goal.
