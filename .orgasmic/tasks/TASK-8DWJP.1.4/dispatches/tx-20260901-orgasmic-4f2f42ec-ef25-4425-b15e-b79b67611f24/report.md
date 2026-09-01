# TASK-8DWJP.1.4 — rebase abort under the writer barrier; pending salvage; SALVAGE_REF pin

Commit: `641ec6c8 TASK-8DWJP.1.4: fix(ledger-sync): run the rebase abort under the writer barrier; carry pending salvage; restore SALVAGE_REF event pin` (one file, +469/−15)

## Changed

All in `crates/orgasmic-daemon/src/ledger_sync.rs`:

- **MEDIUM — abort under the barrier.** `sync_once_with_park` gained an `abort: impl FnMut(&Path, &str) -> Result<String>` closure parameter; both call sites of `abort_rebase_with_salvage` (entry path, ledger_sync.rs:109; in-tick path, ledger_sync.rs:219) now go through it. The daemon loop (`spawn`, ledger_sync.rs:970-989) passes a second `run_barrier`-wrapping closure mirroring the existing park one, so salvage+`git rebase --abort` runs inside the writer barrier at both sites. Test helpers (`sync_once_inner`, `sync_ledger_at`) and direct test callers pass the identity fn.
- **LOW (a) — pending salvage carry.** Entry abort with an empty unmerged set keeps its ref in a `pending_salvage` local (ledger_sync.rs:104-125). It rides `SyncOutcome::Synced { pending_salvage }` / `SyncOutcome::Conflict { pending_salvage }` (attached at the in-tick recover, ledger_sync.rs:222-230). Status: synced outcome gets an `error` note naming the ref, conflict outcome appends `"; pending salvage from aborted rebase at {ref}"` (ledger_sync.rs:716-763); `tracing::warn!` in both branches (ledger_sync.rs:791-808). Event: `record_sync_conflict` emits `PENDING_SALVAGE_REF` extra (ledger_sync.rs:879-884).
- **LOW (b) — salvage-failure trade.** `abort_rebase_with_salvage` (ledger_sync.rs:384-409) degrades on salvage failure (incl. `rebase_orig_head` failure): `tracing::warn!`, empty `salvage_ref` (the established "no snapshot" signal), still aborts — keeping 1.2's unwedge guarantee. Pinned with a `ponytail:` comment naming the choice.
- **LOW (c) — SALVAGE_REF event assertion.** See Unmet Criteria: the brief's premise is false for `conflicting_two_writer_tick`; the positive assertion is restored in `mid_rebase_tick_aborts_and_recovers_instead_of_idling` (ledger_sync.rs:1713-1740, now `#[tokio::test]`, asserts `SALVAGE_REF` extra == outcome's non-empty ref), and `conflicting_two_writer_tick` pins the truthful negative (no `SALVAGE_REF` extra) with an explanatory comment (ledger_sync.rs:1507-1516).
- **Tests.** New `prepare_resolved_stopped_rebase` fixture (stopped rebase, conflict resolved, unmerged empty, tracked outage writes). New `writer_append_during_the_abort_barrier_lands_after_it_and_survives` (MEDIUM + LOW-a conflict branch: append queued during the abort barrier lands after it, survives in the parked ref **and on the remote**; pending ref carries the outage writes; status/event name it). New `empty_unmerged_entry_abort_names_its_salvage_ref_when_the_tick_syncs` (LOW-a synced branch: status note names the sole `-salvage` ref, outage writes only there, worktree reset, local edit pushed). New concurrency test ran 10/10 green.

## Verification Gates

| Gate | Result | Log |
|---|---|---|
| `cargo test -p orgasmic-daemon --lib -- ledger_sync status sync_conflict barrier` | `test result: ok. 32 passed; 0 failed` (baseline 30) | `/tmp/orgasmic-8dwjp14-logs/gate-daemon-ledger-sync.log` |
| `cargo test -p orgasmic-cli --bin orgasmic -- daemon_lifecycle` | `test result: ok. 22 passed; 0 failed` | `/tmp/orgasmic-8dwjp14-logs/gate-cli-daemon-lifecycle.log` |
| `cargo clippy -p orgasmic-daemon -p orgasmic-cli --all-targets -- -D warnings` | exit 0, no warnings | `/tmp/orgasmic-8dwjp14-logs/gate-clippy.log` |
| `cargo fmt --all --check` | clean | `/tmp/orgasmic-8dwjp14-logs/gate-fmt.log` |

`two_daemon_loops_converge_through_the_bare_remote` passed inside the daemon gate (no timeout).

## Unmet Criteria

- **LOW (c) as literally specified** — "restore the two-line SALVAGE_REF event assertion in `conflicting_two_writer_tick`; the test still produces a non-empty salvage_ref (source Autostash)". Premise false, two ways (verified, not inferred):
  1. Shell probe replicating the test's exact git sequence (`/tmp/orgasmic-probe-path`, script output in session): the pull stops **mid-rebase** (`rebase-merge`, head-name `refs/heads/orgasmic`, unmerged `[T1/node.org]`) → source is `Worktree`, not `Autostash`; and the abort-time salvage tree is **identical** to orig-head's tree → `salvage_ref` is empty (the abort snapshots the pre-rebase HEAD which already contains everything this tick staged).
  2. In-repo proof: temporarily restoring the verbatim assertion fails — `assertion failed: events[0].extra.iter().any(... key == "SALVAGE_REF" ...)` (`/tmp/orgasmic-8dwjp14-logs/probe-lowc-verbatim.log`).
  The reviewer's underlying intent (event must name its salvage ref when one exists) is now pinned by the same two-line assertion relocated to `mid_rebase_tick_aborts_and_recovers_instead_of_idling`, which genuinely produces a non-empty salvage (entry abort snapshots real outage writes); `conflicting_two_writer_tick` asserts the honest inverse. Manager may re-derive the verbatim form only if the salvage design changes.

## Residual Risk

- The pending-salvage note rides `LedgerSyncStatus.error` with outcome `"synced"`; CLI `status`/`doctor` only render conflict/failed outcomes (main.rs:2790-2813, doctor.rs:302-311), so the note surfaces via the daemon log warn and the API status payload — no doctor noise, but also no CLI line for it yet.
- A degraded salvage (LOW b) intentionally proceeds without any snapshot; the outage writes are then unrecoverable — accepted trade, documented at ledger_sync.rs:386-392.
- In the parked path, a writer append that lands between the abort barrier and the park barrier ends up preserved in the **parked ref** (pushed to origin), not merged into `orgasmic` — same treatment as any parked conflict write; the new barrier test asserts exactly this outcome.
- LOW (d) (salvage-skip noise / conflict-ref retention) left out of scope per brief.
