# TASK-8DWJP — implement dec_EWY0K: the ledger sync conflict path (H6, folds TASK-AS0FS)

The DECISION IS MADE — read it first and treat it as the spec:
`orgasmic decision get --project orgasmic dec_EWY0K`. Then the task:
`orgasmic task get --project orgasmic TASK-8DWJP` (Notes section has the scope).
Do not re-open pen-on-write vs conflict path.

## Where the code is (after TASK-MSYN4.2, merged today)
`crates/orgasmic-daemon/src/ledger_sync.rs`:
- `sync_once_inner`: early return (branch `orgasmic` + origin) → views ignore/untrack →
  `git add --all` with sidecar excludes → commit → loop: `pull --rebase --autostash`; on
  failure `rebase --abort` + `bail!("git pull --rebase failed: …")` → push with retries.
- `sync_ledger_at(ledger, machine_id, statuses, now)`: wraps `sync_once`, records
  `LedgerSyncStatus { outcome: "idle"|"synced"|"failed"|"backed_off", error,
  consecutive_failures, last_attempt_at, last_success_at, next_attempt_at }` in the shared
  `LedgerSyncStatuses` map, exponential backoff on failure, change-only logging.
- `spawn(index, machine_id, statuses, shutdown)` from `lib.rs` (~:1104), where the
  `WriterHandle` is also in scope.
- `/status` exposes the map (`api.rs get_status`); `orgasmic daemon status` prints
  failed/backed_off ledgers (`crates/orgasmic-cli/src/main.rs cmd_daemon_status`).

## What to build — the minimum that satisfies dec_EWY0K
1. **Distinguish a CONFLICT from other pull failures.** Parse the failed pull's output for
   `CONFLICT (…): … in <path>` lines. Only a conflict takes the new path; network/auth
   failures keep today's `failed` + backoff behaviour.
2. **Conflict path**, inside `sync_once_inner` right where it bails today:
   a. `git rebase --abort` (already there).
   b. Salvage anything written since the tick's commit: same `git add --all` (same
      excludes) + commit `ledger: conflict salvage <machine-id>` if anything is staged.
   c. Park: `git update-ref refs/orgasmic/conflicts/<machine-id>/<UTC yyyymmddThhmmssZ> HEAD`.
      Best-effort `git push origin <that ref>:<that ref>` so the other machine can see it;
      a push failure here is logged, not fatal.
   d. Follow the remote: `git reset --hard origin/orgasmic` (the failed pull already
      fetched; run `git fetch origin orgasmic` first anyway so this never resets to a stale
      ref). Then `return Ok(SyncOutcome::Conflict { parked_ref, paths })`.
   e. Add the `Conflict` variant to `SyncOutcome`; `sync_ledger_at` records
      `outcome: "conflict"`, `error: Some("<n> conflicting paths parked at <ref>: <paths>")`,
      resets `consecutive_failures` to 0 and `next_attempt_at` to `None` (the conflict is
      resolved by parking; the next tick must sync normally), and `tracing::warn!`s once.
3. **Ledger event.** Thread the `WriterHandle` into `spawn` (it is built in `lib.rs` before
   `ApiState`). After a conflict, append ONE tx to this machine's
   `machines/<machine-id>/tx/YYYY-MM.org` through the writer (the same append API
   `record_api_tx`/`ApiTxRequest` use — find the writer-level call it bottoms out in and
   call that; do not write the file yourself): `TYPE ledger.sync_conflict`, extras
   `PARKED_REF`, `PATHS` (space-separated), `LOCAL_HEAD`, `REMOTE_HEAD`, `MACHINE`. It
   syncs on the next tick like any other event. Add the type to `shipped/schema/tx.org`'s
   routed list (tx/, not journal) — and if TASK-SRBGS.1's list-vs-code test has landed by
   the time you merge, keep it green.
4. **Premise rewrite.** The staging comment in `ledger_sync.rs` ("A foreign node dir can
   only appear modified here if something wrote outside its pen, which the claim gate
   refuses") is false — replace it with the dec_EWY0K premise (writes are free; conflicts
   park). Same for any sentence in `crates/orgasmic-daemon/src/writer.rs` near
   `guard_node_write` and in `shipped/skills/orgasmic/references/ledger.md` that calls the
   claim gate a cross-machine write barrier. One or two sentences each; no essays.
5. **`orgasmic daemon status`** prints a `conflict` ledger like a failed one, plus the
   parked ref (one line).

## Tests (reuse `ledger_sync::tests::seed_remote`, `run`, `local_commit`;
`failed_pull_is_reported_and_backed_off` is the template)
- Two-writer: `a` and `b` write different content to the same `tasks/T1/node.org`; `b`
  pushes; tick `a` → status `conflict`; `refs/orgasmic/conflicts/<machine>/…` exists in `a`
  and its tree holds a's content; `a`'s HEAD == `origin/orgasmic` and the working file holds
  b's content; `consecutive_failures == 0`; a SECOND tick with a fresh local write in `a`
  syncs (`synced`) and reaches the remote. That is "both writes survive (one live, one
  parked), the loop recovers, status shows it".
- Non-conflict failure (e.g. remote URL pointing at a missing dir) still yields `failed` +
  backoff — pin that the two paths stay distinct.
- The event: one test at the level where a real `WriterHandle` exists (the daemon api test
  harness builds one — `direct_stage_test_state` in `api.rs` tests) asserting a
  `ledger.sync_conflict` entry with `PARKED_REF` lands in the machine tx file after a
  conflict tick. If wiring that harness costs more than ~40 lines, make the event append a
  small `FnMut`/closure parameter of the tick function, unit-test it with a recording
  closure, and say so.
- Existing `ledger_sync` tests stay green.

## Gates (each to a log file, never pipe cargo output)
- `cargo test -p orgasmic-daemon --lib -- ledger_sync status sync_conflict`
- `cargo test -p orgasmic-cli --bin orgasmic -- daemon_lifecycle` (targeted; NEVER unfiltered)
- `cargo clippy -p orgasmic-daemon -p orgasmic-cli --all-targets -- -D warnings`
- `cargo fmt --all --check`

## Rules
- Work only in your worktree; commits `TASK-8DWJP: feat(ledger-sync): <one line>`.
- NEVER `cargo test --workspace`; NEVER the whole `orgasmic-cli` crate in one command; NEVER
  set `ORGASMIC_HOME`; NEVER run `daemon start`; never touch the live ledger at
  `~/.orgasmic/ledgers/orgasmic`; never run
  `legacy_drivers_and_explicit_pairs_emit_equivalent_start_events`.
- `git reset --hard` appears in this task ONLY inside `sync_once_inner` against the ledger
  worktree the daemon owns, guarded by the salvage commit + parked ref that precede it.
  Never run it anywhere else.
- Report: what changed (`file:line`), each gate with its pass/fail line and log path, unmet
  criteria, residual risk (name the write-loss window between salvage commit and reset).
  Finish with `orgasmic dispatch finalize --summary-file <path>` (report only, no `--commit`).
