# Review: TASK-MSYN4.2 — sync loop sidecar excludes (H2) + per-ledger sync status with backoff (H5)

Fix round for chain-review findings H2 + H5 (whole-chain review tx-1c6d2115). Implementer:
codex gpt-5.6-sol, one commit `51af1f08`, merged to main as `d75dee5a`.

## What to review

    git diff d75dee5a^1 d75dee5a

Five files, +295/-7: `crates/orgasmic-daemon/src/ledger_sync.rs` (the substance),
`lib.rs`, `api.rs` (status plumbing), `crates/orgasmic-cli/src/daemon_lifecycle.rs`,
`crates/orgasmic-cli/src/main.rs`.

## The findings this must close
- **H2.** `sync_once` swept the tree with `git add --all -- .orgasmic` while the writer was
  mid-transaction: it committed `<file>.bak.<req>` and `<file>.tmp` sidecars (two real
  commits on the live ledger) and can publish node rewrites before their close tx lands.
- **H5.** A failed `pull --rebase` was `rebase --abort` + `bail!`, retried identically every
  2 s, visible only as `tracing::warn!`.

## What the fix claims
1. Both `git add --all` calls carry `:(exclude,glob).orgasmic/**/*.tmp`, `…/**/*.tmp.*`,
   `…/**/*.bak.*` (the machine-dir call builds the same three from `machine_rel`). A
   `ponytail:` comment names the remaining one-interval torn window and the upgrade path
   (writer quiescence barrier / ledger-wide lease). Test `writer_sidecars_are_never_staged`
   plants three sidecars next to a node and one next to a machine tx file.
2. `LedgerSyncStatus { outcome: &'static str ("idle"|"synced"|"failed"|"backed_off"),
   error, consecutive_failures, last_attempt_at, last_success_at, next_attempt_at }` in
   `Arc<Mutex<BTreeMap<PathBuf, _>>>`, created in `lib.rs`, shared with `ApiState`.
   `sync_ledger_at(ledger, machine_id, statuses, now)` is the per-ledger tick: skips (and
   marks `backed_off`) while `now < next_attempt_at`; on failure backoff =
   `SYNC_INTERVAL * 2^min(n,8)` capped at 5 min; logs only on first/changed failure and on
   recovery. `sync_once` itself is unchanged in signature.
3. `/status` gains `ledger_sync` (path → status); `orgasmic daemon status` prints one line
   per `failed`/`backed_off` ledger (first error line only). Tests:
   `failed_pull_is_reported_and_backed_off` (conflict → `failed` + reason; second tick 1 s
   later → `backed_off` with reflog unchanged), `daemon_status_decodes_ledger_sync_failures`,
   and the `get_status` test asserts the map.

## Attack these specifically
- **Pathspec correctness.** Confirm on git ≥ 2.40 that `:(exclude,glob)` with `**` excludes
  a sidecar at ANY depth under `.orgasmic` and under `machines/<id>`, and that the
  positive pathspec `.orgasmic` plus these excludes cannot exclude a legitimate file: what
  about a node dir or artifact whose NAME legitimately ends in `.tmp` / contains `.bak.`?
  (`rg -n '\.bak\.|\.tmp' crates/orgasmic-core/src/paths.rs crates/orgasmic-daemon/src/writer.rs`
  for every sidecar shape; is `.tmp.req-rollback` covered by `*.tmp.*`? Is there a fourth
  shape — e.g. `.new`, `.lock`, `.swp` from the writer or from `write_if_changed` in
  `views.rs` (`<file>.<pid>.<n>.tmp`) — that is NOT covered?) Also: `.orgasmic/tmp/` is
  gitignored; do the excludes change anything there?
- **The torn window ceiling.** The comment claims ≤ one interval. Is that true when the
  rename loop's target and the close tx live in different `git add` calls (node dirs in the
  first, `machines/<id>/tx` in the second) and a tick lands BETWEEN them? Walk
  `transaction_multi_locked_inner` (`writer.rs` ~3478) against `sync_once_inner`'s two adds.
- **Backoff arithmetic and races.** `1_u32 << consecutive_failures.min(8)` then
  `saturating_mul` then `.min(MAX_BACKOFF)`: any overflow path? `sync_ledger_at` clones
  `previous` outside the lock, runs git for seconds, then INSERTS a fresh status — if the
  same ledger appears twice in the board (two projects, one root) or a tick overlaps a slow
  previous tick (interval `MissedTickBehavior::Skip` — can two `spawn_blocking` ticks for the
  same ledger run concurrently?), what does the map end up saying? Is `last_success_at`
  preserved through the backed_off branch (it only sets `outcome`)?
- **Idle semantics.** A plain project checkout (not a synced ledger) now gets an `idle`
  entry with `last_success_at = now` every tick — is that misleading in `/status`, and does
  the map grow for every board path forever (removed projects)?
- **Failure classification.** Every `Err` from `sync_once` — including a push that fails 5
  times, a missing git binary, a non-UTF-8 path — takes the same backoff. Is there a failure
  class that should NOT back off (e.g. push race after a successful rebase)? Say which, if any.
- **Surface honesty.** `orgasmic daemon status` prints failing ledgers; does `orgasmic
  status` (the other status verb, if any) or the UI read `/status` and now break on the new
  field (typescript `Status` type)? `rg -n 'index_refresh|fd_limit' ui/src | head`.
- **Test honesty.** Does `writer_sidecars_are_never_staged` also prove the machine-dir add
  excludes (it plants `tx/2026-09.org.bak.zzz` — assert it is absent, not just that node
  sidecars are)? Does `failed_pull_is_reported_and_backed_off` prove "no git ran" via reflog
  robustly (would a failed pull even write reflog entries)?

Already established — do not re-spend: on the merged tree the manager ran
`cargo test -p orgasmic-daemon --lib -- ledger_sync` (8 passed), `-- status` (7 passed),
`cargo test -p orgasmic-cli --bin orgasmic -- daemon_lifecycle` (22 passed),
`cargo clippy -p orgasmic-daemon -p orgasmic-cli --all-targets -- -D warnings` clean,
`cargo fmt --all --check` clean (see `orgasmic task get --project orgasmic TASK-MSYN4.2`).

Context: dec_EWY0K (decided today) makes the NEXT round (TASK-8DWJP) add a conflict path on
top of this status surface — if you see something here that will fight that design, say so
as a finding rather than reviewing 8DWJP in advance.

## Rules
- READ-ONLY. No edits, no git writes, no mutating `orgasmic` verbs, nothing against the live
  ledger at `~/.orgasmic/ledgers/orgasmic` (reading `git log`/`ls-files` there is fine; the
  live daemon on :4848 still runs the PRE-fix runtime, so `/status` there will not show the
  new field — do not report that as a defect).
- File each finding as it appears:
  `orgasmic tx record --project orgasmic --type reviewer.finding --task TASK-MSYN4.2
  --reason "HIGH|MEDIUM|LOW <file:line> — <one sentence>"` (single line).
- Targeted tests only; NEVER the whole `orgasmic-cli` suite unfiltered; never the workspace;
  never `ORGASMIC_HOME`; do not read `verify/*/injection.patch`.
- Say what you did not check. Finish with `orgasmic dispatch finalize --summary-file <path>`
  (report only) and end with the explicit verdict sentence:
  APPROVE / APPROVE WITH FOLLOW-UPS / REJECT.
