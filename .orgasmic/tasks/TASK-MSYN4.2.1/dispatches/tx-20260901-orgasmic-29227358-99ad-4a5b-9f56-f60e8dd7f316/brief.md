# TASK-MSYN4.2.1 — residuals of the MSYN4.2 review (tracked sidecars, ceiling comment, status hygiene)

Fix round 2 for TASK-MSYN4.2 (merged `d75dee5a`). The review (claude-opus-5 high,
tx-bfdb698d) approved with follow-ups. Read the task first:
`orgasmic task get --project orgasmic TASK-MSYN4.2.1` — exact `file:line` and acceptance.
TASK-8DWJP (the conflict path) has ALREADY merged into `ledger_sync.rs` by the time you
start — read the current file; do not assume the line numbers in the task. Everything
below is the minimum.

## 1. MEDIUM — a tracked sidecar must be able to leave the index
`git add --all -- .orgasmic :(exclude,glob)…` never stages the deletion of an excluded
path, so a sidecar that is in `HEAD` stays a permanent ` D` and the autostash churns it
every tick. Fix: once per tick, right beside the existing
`git rm -r -q --cached --ignore-unmatch -- .orgasmic/views`, run the same for the three
sidecar globs (`:(glob).orgasmic/**/*.tmp`, `…/**/*.tmp.*`, `…/**/*.bak.*`). Test in
`ledger_sync::tests`: `git add -f` + commit a sidecar, delete it from the worktree, run
`sync_once`, assert `git ls-files` no longer lists it and `git status --porcelain` is empty.

## 2. MEDIUM — the ceiling comment
The `ponytail:` comment claims one sync interval. Rewrite it (comment only): the torn
state lasts until the next SUCCESSFUL sync — backoff can stretch that to `MAX_BACKOFF`, a
wedged ledger indefinitely — and both orders exist: node rewrite without its close tx
(add#1 after rename, add#2 before append) and close tx WITHOUT the node rewrite (add#1
before rename, add#2 after append). Keep the upgrade path sentence.

## 3. LOW — status hygiene (three one-liners)
- `SyncOutcome::Idle` must not touch `last_success_at` (keep the previous value; `None` if it
  never synced). Only `Synced`/`Conflict` count as success.
- After building the `ledgers` set in `spawn`, `retain` the status map to those paths.
- `crates/orgasmic-cli/src/doctor.rs` (~:334, where `/daemon/status` is read): print one
  warning line per ledger whose outcome is `failed`, `backed_off`, or `conflict` (path,
  failures, first error line — same shape as `daemon status`). Reuse the
  `LedgerSyncStatus` type already in `daemon_lifecycle.rs`; do not add a second struct.

## Gates (each to a log file, never pipe cargo output)
- `cargo test -p orgasmic-daemon --lib -- ledger_sync status`
- `cargo test -p orgasmic-cli --bin orgasmic -- doctor daemon_lifecycle` (targeted; NEVER unfiltered)
- `cargo clippy -p orgasmic-daemon -p orgasmic-cli --all-targets -- -D warnings`
- `cargo fmt --all --check`

## Rules
- Work only in your worktree; one commit `TASK-MSYN4.2.1: fix(ledger-sync): <one line>`.
- NEVER `cargo test --workspace`; NEVER the whole `orgasmic-cli` crate in one command; NEVER
  set `ORGASMIC_HOME`; NEVER run `daemon start`; never touch the live ledger at
  `~/.orgasmic/ledgers/orgasmic`.
- Report: what changed (`file:line`), each gate with its pass/fail line and log path, unmet
  criteria, residual risk. Finish with `orgasmic dispatch finalize --summary-file <path>`
  (report only, no `--commit`).
