# TASK-8DWJP.1 — fix round after the 8DWJP review REJECT (conflict path)

Read the task first: `orgasmic task get --project orgasmic TASK-8DWJP.1` — every finding with
`file:line`, fix direction and acceptance. The decision is still the spec:
`orgasmic decision get --project orgasmic dec_EWY0K`. TASK-MSYN4.2.1 (tracked-sidecar
untracking, ceiling comment, status hygiene) has merged into `ledger_sync.rs` before you
start — read the CURRENT file; line numbers in the task are approximate.

## The four things that must change (minimum)
1. **Routing (HIGH) — ALREADY FIXED by TASK-SRBGS.1 (merged `c56b0bbe`,
   `ledger_sync.rs:~403-410` now joins `tx/`).** Verify it on the current file. What remains:
   the test must assert the literal relative path `machines/<id>/tx/<YYYY-MM>.org` instead of
   re-deriving the same expression as production. If SRBGS.1 already did that too, say so and
   move on.
2. **Detection (HIGH).** After `git pull --rebase --autostash`, regardless of exit code, run
   `git diff --name-only --diff-filter=U`. Non-empty → conflict path. Two sub-cases:
   - rebase in progress (today's case): read the unmerged paths, `rebase --abort`, salvage,
     park HEAD, fetch, reset — as now.
   - NO rebase in progress (autostash pop conflicted, exit 0): the local pre-pull worktree is
     the retained stash commit. Park THAT commit (`git rev-parse stash@{0}` → `update-ref
     refs/orgasmic/conflicts/<machine>/<ts> <sha>`), `git stash drop`, then `git fetch origin
     orgasmic` + `git reset --hard origin/orgasmic`. Never `git add` a `UU` path.
   Test vector: `a` has a tracked file under `machines/<other-machine>/…` modified locally
   (uncommitted; `stage_ledger` never stages foreign machine dirs) while the remote changed
   the same file. Assert: outcome `conflict`, parked ref's tree holds a's bytes, working file
   == remote bytes, and a SECOND tick pushes NO `<<<<<<<` markers to the bare remote.
3. **Paths (MEDIUM).** Delete the `" in "` prose scrape in `conflict_paths`; use the same
   `--diff-filter=U` helper as (2), read BEFORE `rebase --abort`. Test with a modify/delete
   conflict (remote deletes, local modifies) → `PATHS` is the real path, not `tree.`.
4. **Barrier (MEDIUM).** `crates/orgasmic-daemon/src/writer.rs` (~343, the
   `WriterCommand` enum; `LeaseSessions`/`ReleaseSessions` ~378 is the shape to copy): add
   `Barrier { run: Box<dyn FnOnce() + Send>, reply: oneshot::Sender<()> }`, one match arm
   that runs it inline, and `WriterHandle::run_barrier(f)`. Run `park_conflict` inside it.
   The `ledger.sync_conflict` append stays AFTER the barrier returns. Test: an append issued
   while the barrier runs is applied afterwards and is not lost.

Optional (≤ 5 lines, else skip and say so): `daemon status` prints the count of
`refs/orgasmic/conflicts/*` on a conflict ledger's line.

## Gates (each to a log file, never pipe cargo output)
- `cargo test -p orgasmic-daemon --lib -- ledger_sync status sync_conflict barrier`
- `cargo test -p orgasmic-cli --bin orgasmic -- daemon_lifecycle` (targeted; NEVER unfiltered)
- `cargo clippy -p orgasmic-daemon -p orgasmic-cli --all-targets -- -D warnings`
- `cargo fmt --all --check`

## Rules
- Work only in your worktree; one commit `TASK-8DWJP.1: fix(ledger-sync): <one line>`.
- `git reset --hard` / `git stash drop` appear ONLY inside the conflict path against the
  ledger worktree the daemon owns, after the parked ref exists. Never run them anywhere else.
- NEVER `cargo test --workspace`; NEVER the whole `orgasmic-cli` crate in one command; NEVER
  set `ORGASMIC_HOME`; NEVER run `daemon start`; never touch the live ledger at
  `~/.orgasmic/ledgers/orgasmic`; never run
  `legacy_drivers_and_explicit_pairs_emit_equivalent_start_events`.
- Report: what changed (`file:line`), each gate with its pass/fail line and log path, unmet
  criteria, residual risk. Finish with `orgasmic dispatch finalize --summary-file <path>`
  (report only, no `--commit`).
