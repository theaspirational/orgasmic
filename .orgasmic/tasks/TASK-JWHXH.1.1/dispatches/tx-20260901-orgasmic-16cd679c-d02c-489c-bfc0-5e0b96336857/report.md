# Changed

- `crates/orgasmic-core/src/views.rs:9,27,122-129,199-249` — exposed the authoritative view collection table, made scratch paths unique with PID plus a process-local `AtomicU64`, retained rename as atomic publish, and added a 32-round/two-thread regression. Pre-fix probe failed with `No such file or directory` at rename; the fixed gate passes.
- `crates/orgasmic-daemon/src/index.rs:853-857,1181-1188` — skips a queued drain after `.orgasmic/` disappears and records why losing that rebuild is safe; schedules incremental rebuilds only for collections present in core `VIEWS`.
- `crates/orgasmic-daemon/src/ledger_sync.rs:40-96,369-399` — moved ignore, untrack, and staging under the pre-existing `.orgasmic` guard, removed directory fabrication, and added a synced-repo regression.
- Commit: `ccece697d2015899a665cf5c2cad67f71f379e35` (`TASK-JWHXH.1.1: fix(views): prevent concurrent scratch collisions`).

# Verification Gates

- PASS — `cargo test -p orgasmic-core --lib views`: `2 passed; 0 failed`; `/tmp/TASK-JWHXH.1.1/gate-core-views.log`.
- PASS — `cargo test -p orgasmic-daemon --lib -- ledger_sync views`: `9 passed; 0 failed`; `/tmp/TASK-JWHXH.1.1/gate-daemon-ledger-sync-views.log`.
- PASS — daemon required-tooling sentinel: `1 passed; 0 failed`; `/tmp/TASK-JWHXH.1.1/gate-daemon-required-tooling.log`.
- PASS — `cargo clippy -p orgasmic-core -p orgasmic-daemon --all-targets -- -D warnings`: finished successfully; `/tmp/TASK-JWHXH.1.1/gate-clippy.log`.
- PASS — `cargo fmt --all --check`: exit 0, no diagnostics; `/tmp/TASK-JWHXH.1.1/gate-fmt.log`.

# Unmet Criteria

None.

# Residual Risk

The chosen drain/teardown policy intentionally drops a queued rebuild when the project `.orgasmic/` directory has disappeared; node directories remain authoritative, and the next write or boot rebuilds views. No full-workspace suite was run, as explicitly prohibited. TASK-JWHXH.2 and the mixed-version fleet release note remain outside this task.
