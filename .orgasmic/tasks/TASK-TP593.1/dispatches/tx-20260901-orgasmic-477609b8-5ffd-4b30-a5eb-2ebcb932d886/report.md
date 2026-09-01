# Changed

- `crates/orgasmic-core/src/node_kernel.rs:449` (file end): deleted the `real_data::every_migrated_node_parses` test and its unset `ORGASMIC_MIGRATED_DIR` escape path.
- `crates/orgasmic-core/tests/fixtures.rs:1-47`: corrected the fixture description, added `fixture_ledger_root()`, and routed `.orgasmic/` reads to the committed fixture ledger.
- `crates/orgasmic-core/tests/fixtures.rs:59-130,239-303,459-491`: removed all seven early-return skip branches and pointed task/tx corpus discovery at the fixture root. A missing fixture now reaches `unwrap`/`expect` and fails.
- `crates/orgasmic-core/tests/fixtures/ledger/.orgasmic/`: copied 15 files (46,682 bytes / 45.6 KiB logical size) from the read-only live ledger, including `project.org`, `.gitignore`, `dec_R75SW`, `term_YC32J`, and the first 40 complete entries of `tx/2026-08.org`.
- Copied task nodes:
  - `TASK-VWBDJ` (DONE): required by the existing field, acceptance, and rewrite assertions.
  - `TASK-8H8A2` (DONE): small task carrying `TEST_CMD` and `WRITE_SCOPE` properties.
  - `TASK-2DFTX.4` (DONE): child task carrying `DEPENDS_ON`, tags, body prose, and an Org link.
  - `TASK-QK8PT` (CANCELLED): cancelled lifecycle shape with extra benchmark properties.
- Commit: `09d87cf88ac61b716ecfe1c6c8f243b5b23cea33` (`TASK-TP593.1: test(core): wire corpus tests to committed ledger`). Worktree is clean.

# Verification Gates

- PASS — `cargo test -p orgasmic-core --test fixtures`: `19 passed; 0 failed; 0 ignored`; zero `skipping` lines. Log: `/tmp/TASK-TP593.1-fixtures.log` (PID sidecar `/tmp/TASK-TP593.1-fixtures.pid`).
- PASS — `cargo test -p orgasmic-core --lib node_kernel`: `3 passed; 0 failed; 175 filtered out`. Log: `/tmp/TASK-TP593.1-node-kernel.log` (PID sidecar `/tmp/TASK-TP593.1-node-kernel.pid`).
- PASS — `cargo clippy -p orgasmic-core --all-targets -- -D warnings`: `Finished dev profile`. Log: `/tmp/TASK-TP593.1-clippy.log` (PID sidecar `/tmp/TASK-TP593.1-clippy.pid`).
- PASS — `cargo fmt --all --check`: exit 0, empty diagnostic log. Log: `/tmp/TASK-TP593.1-fmt.log` (PID sidecar `/tmp/TASK-TP593.1-fmt.pid`).
- PASS — acceptance cross-check `cargo test -p orgasmic-core`: lib `176 passed; 2 ignored`, fixtures `19 passed`, docs `0 passed`; zero `skipping: no live` markers. Log: `/tmp/TASK-TP593.1-core.log` (PID sidecar `/tmp/TASK-TP593.1-core.pid`).

# Unmet Criteria

None.

# Residual Risk

The committed corpus is intentionally a 46,682-byte sample, not the full live ledger. It covers the manager-selected task, decision, glossary, project, and tx shapes, but future live-ledger schema shapes require deliberate fixture refreshes.
