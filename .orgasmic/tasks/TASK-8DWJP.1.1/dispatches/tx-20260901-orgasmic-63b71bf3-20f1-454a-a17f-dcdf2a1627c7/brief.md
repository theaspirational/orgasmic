# TASK-8DWJP.1.1 — make the conflict path re-entrant; verified stash drop; network git outside the barrier

Read the task first: `orgasmic task get --project orgasmic TASK-8DWJP.1.1` — every finding with
`file:line`, fix direction and acceptance. The previous round (`a64d5cf8`) fixed detection,
paths, routing and the barrier; the review confirmed those and rejected on what follows.
Line numbers are approximate; read the current `crates/orgasmic-daemon/src/ledger_sync.rs`.

## The one change that closes both HIGHs
`unmerged_paths()` must be read at the TOP of the tick, before `stage_ledger`. Non-empty on
entry → go straight into the conflict path; never stage or commit over a UU index.
`park_conflict` is a multi-step, non-transactional git sequence (update-ref, push, stash
drop, fetch, reset) and any step can fail — so the path must be safe to re-enter:
- rebase in progress → `rebase --abort`, then park as now;
- retained autostash present (verified, see below) → park it as now;
- neither, but a parked ref for this machine already exists from a crashed attempt → reuse
  it (do not mint a second), then `fetch` + `reset --hard origin/orgasmic`;
- nothing recoverable → still `fetch` + `reset --hard` (the remote is the source of truth) and
  say so in the status error.
Tests: (1) an injectable failure seam (mirror the existing `before_push` seam) that fails the
tick between `stash drop` and `reset --hard`; the NEXT tick recovers (worktree == remote,
parked ref still holds the local bytes) and the bare remote never receives `<<<<<<<`.
(2) The leftover UU path under `machines/<other>/`: no permanent wedge — the next tick
recovers instead of failing at `commit_staged` forever.

## MEDIUM 3 — drop the stash by verified identity
`refs/stash` is shared between the ledger worktree and the operator's source checkout. Parse
`Created autostash: <sha>` from the pull stdout; immediately before `git stash drop`, require
`git rev-parse stash@{0}` == that sha; on mismatch return `failed` (+ backoff), drop nothing.
Test: plant a foreign stash on top before the drop → no drop, status `failed`.

## MEDIUM 4 — only local git inside the writer barrier
`writer_loop` is a plain tokio task and the `Barrier` arm runs `run()` inline. Move
`git fetch origin orgasmic` BEFORE `run_barrier` and the best-effort parked-ref push AFTER it.
Inside the fence only: salvage commit, `update-ref`, verified `stash drop`, `reset --hard`.

## LOWs
- `writer.rs` Barrier arm: `std::panic::catch_unwind(AssertUnwindSafe(run))`, always send
  `reply` (~4 lines).
- `PATHS` is space-joined: use a tab separator or repeat the extra, if it stays a one-liner.
- Optional one-liners (skip and say so if not one line): put "parked-ref push failed" into the
  conflict status error string; `doctor` names the manual recovery for a UU ledger index.

## Gates (each to a log file, never pipe cargo output)
- `cargo test -p orgasmic-daemon --lib -- ledger_sync status sync_conflict barrier`
- `cargo test -p orgasmic-cli --bin orgasmic -- daemon_lifecycle` (targeted; NEVER unfiltered)
- `cargo clippy -p orgasmic-daemon -p orgasmic-cli --all-targets -- -D warnings`
- `cargo fmt --all --check`
(`two_daemon_loops_converge_through_the_bare_remote` has a 10 s deadline and is load-sensitive;
if it times out under parallel cargo, rerun it alone before calling it a failure.)

## Rules
- Work only in your worktree; one commit `TASK-8DWJP.1.1: fix(ledger-sync): <one line>`.
- `git reset --hard` / `git stash drop` appear ONLY inside the conflict path against the ledger
  worktree the daemon owns, after the parked ref exists. Never run them anywhere else.
- NEVER `cargo test --workspace`; NEVER the whole `orgasmic-cli` crate in one command; NEVER
  set `ORGASMIC_HOME`; NEVER run `daemon start`; never touch the live ledger at
  `~/.orgasmic/ledgers/orgasmic`; never run
  `legacy_drivers_and_explicit_pairs_emit_equivalent_start_events`.
- Report: what changed (`file:line`), each gate with its pass/fail line and log path, unmet
  criteria, residual risk. Finish with `orgasmic dispatch finalize --summary-file <path>`
  (report only, no `--commit`).
