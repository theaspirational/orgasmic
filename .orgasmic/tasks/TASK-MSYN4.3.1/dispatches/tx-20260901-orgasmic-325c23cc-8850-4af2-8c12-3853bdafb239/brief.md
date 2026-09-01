# TASK-MSYN4.3.1 — test-only: pin the fold contract for one tx id shared by two machines

Read the task first: `orgasmic task get --project orgasmic TASK-MSYN4.3.1` — it is the whole
spec. One test in `crates/orgasmic-core/src/tx.rs` next to
`dispatch_fold_keeps_two_machine_generations_distinct_by_uuid_tx_id` (~:1170): two
`manager.dispatch_started` entries on two machines sharing ONE `TX_ID` (the pre-fix numeric
shape `tx-2026…-orgasmic-0007`), then one `CLOSED_TX` naming it. Read the fold, state in a
comment what the documented behaviour is for that shape (both generations close, or the
ambiguity is detected), and assert exactly that. No production code. Skip the optional
`ProjectSequence` rename unless it is a mechanical few-minute change.

## Gates (each to a log file, never pipe cargo output)
- `cargo test -p orgasmic-core --lib tx`
- `cargo clippy -p orgasmic-core --all-targets -- -D warnings`
- `cargo fmt --all --check`

## Rules
- Work only in your worktree; one commit `TASK-MSYN4.3.1: test(tx): <one line>`.
- NEVER `cargo test --workspace`; NEVER the whole `orgasmic-cli` crate; NEVER set
  `ORGASMIC_HOME`; NEVER run `daemon start`; never touch `~/.orgasmic/ledgers/orgasmic`.
- Report: what changed (`file:line`), each gate with its pass/fail line and log path, the
  behaviour you pinned and why. Finish with `orgasmic dispatch finalize --summary-file <path>`
  (report only, no `--commit`).
