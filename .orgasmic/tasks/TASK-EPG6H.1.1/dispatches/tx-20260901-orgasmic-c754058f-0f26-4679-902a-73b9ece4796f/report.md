# Changed

- `crates/orgasmic-core/src/tx.rs:88-90,750-753` adds the shared strict full-width Org TIME parser and its date-only rejection test.
- `crates/orgasmic-cli/src/manager.rs:9802-9832,12337-12446` parses both close and relevant journal times, clears candidates on invalid stamps or per-candidate journal read/parse errors, and covers earlier, later, date-only, and malformed-journal fixtures.
- `crates/orgasmic-daemon/src/api.rs:18418-18425,18499-18563,22462-22579` documents the evidence remedy, refuses invalid times, maps journal read/parse failures to path-naming 400 responses, reads `JournalEntry` fields directly, and covers earlier/later/invalid/error cases.
- `crates/orgasmic-daemon/src/index.rs:3741` restores `journal_tx_entry` to module-private visibility after removing the API round-trip.
- Commit: `a2a05e1c TASK-EPG6H.1.1: fix(dispatch): fail closed on invalid repair journals`.

# Verification Gates

- PASS — `cargo test -p orgasmic-core --lib full_org_timestamp_parser_rejects_date_only_stamps`: `1 passed; 0 failed`; `/tmp/TASK-EPG6H.1.1-core-test-20260901T175128.log` (`GATE_EXIT=0`).
- PASS — `cargo test -p orgasmic-cli --bin orgasmic -- torn_close`: `1 passed; 0 failed`; `/tmp/TASK-EPG6H.1.1-cli-torn-close-20260901T175249.log` (`GATE_EXIT=0`).
- PASS — `cargo test -p orgasmic-daemon --lib -- repair evidence`: `11 passed; 0 failed`; `/tmp/TASK-EPG6H.1.1-daemon-repair-evidence-20260901T175304.log` (`GATE_EXIT=0`).
- PASS — `cargo clippy -p orgasmic-core -p orgasmic-cli -p orgasmic-daemon --all-targets -- -D warnings`: finished dev profile; `/tmp/TASK-EPG6H.1.1-clippy-20260901T175436.log` (`GATE_EXIT=0`).
- PASS — `cargo fmt --all --check`: `/tmp/TASK-EPG6H.1.1-fmt-20260901T175543.log` (`GATE_EXIT=0`).
- PASS — `git diff --check`; committed worktree is clean.
- Corrected implementation-test failure — the first CLI attempt found one missing fixture format placeholder; fixed before the passing rerun. Evidence: `/tmp/TASK-EPG6H.1.1-cli-torn-close-20260901T175150.log`.

# Unmet Criteria

- None.

# Residual Risk

- Verification was intentionally limited to the required targeted gates; the prohibited workspace-wide and unfiltered CLI suites were not run.
