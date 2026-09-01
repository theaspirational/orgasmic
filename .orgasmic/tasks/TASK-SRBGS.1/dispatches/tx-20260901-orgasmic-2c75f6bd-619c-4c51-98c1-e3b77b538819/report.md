# Changed

- **L1** — `crates/orgasmic-core/src/identity_lint.rs:220-263,357-364,613-628` now returns and propagates contextual collection scan errors; `crates/orgasmic-core/src/id_repair.rs:187-191,225-229` and `crates/orgasmic-daemon/src/index.rs:3482-3494,3507-3519,3839-3851` surface those failures instead of reporting a clean lint. Added the Unix unreadable-directory regression.
- **L2** — `crates/orgasmic-cli/src/project_migrate.rs:78,379-389,650-676` wraps non-branch migration apply failures with exact `git -C <tree> checkout -- .orgasmic` and `git -C <tree> clean -fd -- .orgasmic` recovery commands. The regression forces a partial apply and verifies both commands and the already-written first node.
- **L3** — `crates/orgasmic-daemon/src/writer.rs:537,857-926` keys apply failures by the owning tx, and successful repair drops repaired foreign failures; `crates/orgasmic-daemon/src/api.rs:8592,8637` takes only the current tx failure. `crates/orgasmic-daemon/src/api.rs:32874-32911` proves a later request repairs rather than inheriting the earlier 503.
- **L4** — `shipped/schema/tx.org:113-126` adds `implementer.commit_pending` and `fixer.done`; `crates/orgasmic-daemon/src/api.rs:8766-8784,24272-24299` owns the Rust dispatch route set and parses the shipped bullet block to require exact equality. Inspection also confirmed `ledger.sync_conflict` was documented but bypassed API routing, so `crates/orgasmic-daemon/src/ledger_sync.rs:403-410` now writes it under `machines/<id>/tx/`; its integration regression was updated.
- **L5** — `crates/orgasmic-cli/src/project_migrate.rs:51,89,204` counts byte-unstable heading round trips in `Migration::anomalies` and prints that field instead of a literal.
- Commit: `79caf3356d2041ac9e418005b9eebd23a58e1091` (`TASK-SRBGS.1: fix(follow-ups): fail closed and align routes`). Worktree is clean.

# Verification Gates

- PASS — `cargo test -p orgasmic-core --lib identity_lint`: `11 passed; 0 failed; 1 ignored`; `/tmp/TASK-SRBGS.1-core-identity_lint.log`.
- PASS — `cargo test -p orgasmic-cli --bin orgasmic -- project_migrate`: `3 passed; 0 failed`; `/tmp/TASK-SRBGS.1-cli-project_migrate.log`.
- PASS — `cargo test -p orgasmic-daemon --lib -- apply_failure`: `1 passed; 0 failed`; `/tmp/TASK-SRBGS.1-daemon-apply_failure.log`.
- PASS — `cargo test -p orgasmic-daemon --lib -- shipped_tx_types`: `1 passed; 0 failed`; `/tmp/TASK-SRBGS.1-daemon-shipped_tx_types.log`.
- PASS — `cargo test -p orgasmic-daemon --lib -- ap971_every_known_event_type_has_a_pinned_ledger_route`: `1 passed; 0 failed`; `/tmp/TASK-SRBGS.1-daemon-route-pin.log`.
- PASS — `cargo test -p orgasmic-daemon --lib -- conflicting_two_writer_tick_parks_recovers_and_records_event`: `1 passed; 0 failed`; `/tmp/TASK-SRBGS.1-daemon-ledger_sync_conflict.log`.
- PASS — daemon tooling sentinel `required_test_tooling_is_present`: `1 passed; 0 failed`; `/tmp/TASK-SRBGS.1-daemon-tooling-sentinel.log`.
- PASS — `cargo clippy -p orgasmic-core -p orgasmic-cli -p orgasmic-daemon --all-targets -- -D warnings`; `/tmp/TASK-SRBGS.1-clippy.log`.
- PASS — `cargo fmt --all --check`; zero-output log `/tmp/TASK-SRBGS.1-fmt.log`.

# Unmet Criteria

None.

# Residual Risk

Only the brief's focused tests were run; unfiltered crate/workspace suites were intentionally not run under the dispatch restrictions.
