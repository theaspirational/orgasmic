## Changed

- `crates/orgasmic-daemon/src/ledger_sync.rs:20-42,113-153` adds serializable per-ledger sync status and excludes nested `*.tmp`, `*.tmp.*`, and `*.bak.*` writer sidecars from both staging paths. The required `ponytail:` comment records the remaining one-interval torn-close ceiling and upgrade path.
- `crates/orgasmic-daemon/src/ledger_sync.rs:201-299` factors one ledger tick, records failed/recovered outcomes, and exponentially backs off repeated failures up to five minutes while logging only first/changed failures and recovery.
- `crates/orgasmic-daemon/src/lib.rs:1104-1112,1148` shares the status map between the sync loop and API state.
- `crates/orgasmic-daemon/src/api.rs:233,8936-8955,9008` exposes `ledger_sync` from `/status`; `api.rs:30485-30517` verifies the map and failure reason.
- `crates/orgasmic-cli/src/daemon_lifecycle.rs:99-111` decodes the backward-compatible status slice; `crates/orgasmic-cli/src/main.rs:2790-2804` prints one line for each failed/backed-off ledger, with the first error line only.
- `crates/orgasmic-daemon/src/ledger_sync.rs:492-586` proves nested sidecars are untracked and a conflicting rebase is surfaced/backed off without another git invocation.
- Commit: `51af1f08 TASK-MSYN4.2: fix(daemon): exclude sync sidecars and surface failures`.

## Verification Gates

- PASS — `cargo test -p orgasmic-daemon --lib -- ledger_sync`: `8 passed; 0 failed`; `/tmp/TASK-MSYN4.2-daemon-ledger_sync-retry.log`.
- PASS — `cargo test -p orgasmic-daemon --lib -- status`: `7 passed; 0 failed`; `/tmp/TASK-MSYN4.2-daemon-status.log`.
- PASS — corrected bin-target form `cargo test -p orgasmic-cli --bin orgasmic -- daemon_lifecycle`: `22 passed; 0 failed`; `/tmp/TASK-MSYN4.2-cli-daemon_lifecycle-bin.log`. The brief's literal `--lib` form is invalid because `orgasmic-cli` has no library target; evidence: `/tmp/TASK-MSYN4.2-cli-daemon_lifecycle.log`.
- PASS — `cargo clippy -p orgasmic-daemon -p orgasmic-cli --all-targets -- -D warnings`: `Finished dev profile`; `/tmp/TASK-MSYN4.2-clippy.log`.
- PASS — `cargo fmt --all --check`; `/tmp/TASK-MSYN4.2-fmt.log`.
- PASS — `git diff --check`; worktree clean after commit.

## Unmet Criteria

- None.

## Residual Risk

- By the accepted staging-only design, a sync tick can still publish rewritten node files before the close tx append; a peer may observe that state for at most one sync interval. A writer-published quiescence barrier or ledger-wide lease is the documented upgrade.
- `project.org` and `gotchas.org` were absent from the dispatched worktree root; implementation used the live task record and the assignment-referenced source paths.
