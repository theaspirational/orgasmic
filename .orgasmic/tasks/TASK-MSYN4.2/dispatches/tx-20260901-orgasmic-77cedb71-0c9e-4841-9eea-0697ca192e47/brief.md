# TASK-MSYN4.2 — sync loop commits writer sidecars (H2) and wedges silently on a failed rebase (H5)

Fix round for findings H2 + H5 of the whole-chain review (tx-1c6d2115, claude-opus-5 high).
Read the task first: `orgasmic task get --project orgasmic TASK-MSYN4.2`.

## The defects

**H2 — `sync_once` sweeps the tree while the writer is mid-transaction.**
`crates/orgasmic-daemon/src/ledger_sync.rs:56` stages with
`git add --all -- .orgasmic ':(exclude).orgasmic/machines'` and takes no writer lease. The
writer's transaction sidecars live next to their targets: `<file>.tmp` (`writer.rs:3202,3234`),
`<file>.tmp.req-rollback…` (`writer.rs:4492`) and `<file>.bak.<request_id>`
(`transaction_sidecar_path`, `writer.rs:3544`). The live ledger already has commits carrying
`node.org.bak.53cd3fda` (`cd544977`) and `.bak.fd0f75a5` (`8f937138`). The same window spans
`transaction_multi_locked_inner`'s rename loop (`writer.rs:3478-3500`: renames first,
`append_txs_inner` after), so a tick can commit and push node files rewritten by a dispatch
close whose close tx has not been appended yet.

**H5 — a failed `pull --rebase` is invisible and retried identically forever.**
`ledger_sync.rs:96`: on failure `rebase --abort` + `bail!`; the loop (`:119 spawn`) logs
`tracing::warn!` and ticks again in 2 s with the same result. Local commits pile up unpushed;
nothing in `/status` or `orgasmic daemon status` says so.

**Line numbers above are from before two merges that touched this file today** (JWHXH.1
`c3d779af`, JWHXH.1.1 `22b9e615`). Current shape of `sync_once_inner`: early return (branch
+ origin) → `if dotorg.exists() { ensure .gitignore has views/; git rm --cached views;
git add --all -- .orgasmic :(exclude).orgasmic/machines }` → `git add` of
`machines/<id>` → commit → pull/push loop. Keep the views ignore/untrack step exactly as
it is; your excludes go on the two `git add` calls. Find code by name, not by line.

## What to do — the minimum

### H2: exclude sidecars at staging; document the torn window as a ceiling
The lease shape (`WriterHandle::with_detached_session_lease`, `writer.rs:1358`) is per-path
and refuses paths another lease holds; it does not fit a whole-tree sweep. Take the
acceptance's second option:

- Add pathspec excludes to BOTH `git add` calls (the `.orgasmic` sweep and the
  `machines/<id>` one): `*.tmp`, `*.tmp.*`, `*.bak.*`. Git's default pathspec `*` does not
  stop at `/`, but write them with explicit `:(exclude,glob)…/**/…` magic so the intent is
  readable, and PROVE nesting with the test below rather than trusting the docs.
- Put a `// ponytail:` comment on the staging block naming the ceiling that remains: a tick
  can commit node rewrites before their close tx lands (rename loop → `append_txs_inner`);
  the tx lands next tick; a peer may observe the node ahead of its tx for ≤ one interval.
  Name the upgrade path (a writer-published quiescence barrier or one lease over the ledger).
  No code for the upgrade.

### H5: record the last outcome per ledger, surface it, back off on repeat failure
- A small shared status: `Arc<std::sync::Mutex<BTreeMap<PathBuf, LedgerSyncStatus>>>` (or
  `RwLock`) created in `lib.rs` next to `ledger_sync::spawn(index, machine_id, shutdown)`
  (`lib.rs:1104`), passed into `spawn`, and stored on `ApiState` (`api.rs:185`).
  `LedgerSyncStatus { outcome: "idle"|"synced"|"failed"|"backed_off", error: Option<String>,
  consecutive_failures: u32, last_attempt_at, last_success_at, next_attempt_at }` — a plain
  `#[derive(Serialize, Clone)]` struct; no trait, no new module.
- Backoff in the LOOP, not in `sync_once`: after a failure, skip that ledger until
  `next_attempt_at = now + min(SYNC_INTERVAL * 2^consecutive_failures, 5 min)`; a success
  resets the counter. Keep `sync_once` pure so its tests stay as they are. The
  `tracing::warn!` stays but must not repeat every 2 s for the same wedge — log on a change
  of state (first failure, error text changed, recovered), not per tick.
- `/status` (`StatusResponse`, `api.rs:8907`; `get_status`, `:8940`) gains
  `ledger_sync: BTreeMap<String, LedgerSyncStatus>` keyed by ledger path.
- `orgasmic daemon status` (CLI `DaemonStatus`, `crates/orgasmic-cli/src/daemon_lifecycle.rs:88`,
  a `#[serde(default)]` slice of `/status`) gains the same field and prints ONE line per
  ledger that is `failed`/`backed_off` — path, consecutive failures, first line of the error.
  Healthy ledgers print nothing; quiet by default.

### Tests (reuse `ledger_sync::tests::seed_remote` / `run` / `local_commit`)
1. Sidecar never staged: in clone `a` write `.orgasmic/tasks/T1/node.org` PLUS
   `.orgasmic/tasks/T1/node.org.tmp`, `.orgasmic/tasks/T1/node.org.tmp.req-rollback-x`,
   `.orgasmic/tasks/T1/node.org.bak.abc`, and one `machines/<uuid>/tx/2026-09.org.bak.zzz`;
   run `sync_once`; `git ls-files` in `a` lists `node.org` and the tx file and NONE of the
   sidecars.
2. Failed pull surfaces: make `a` and `b` commit conflicting content to the same tracked
   file, push from `b`, then drive the loop body (factor the per-ledger tick into a function
   you can call directly with a fake `now`) for `a`: status becomes `failed` with the
   `git pull --rebase failed` text and `consecutive_failures == 1`; a second call before
   `next_attempt_at` records `backed_off` and does not run git (assert via the existing
   `before_push`-style hook or by checking reflog/commit count unchanged).
3. `get_status` includes the map (one small api test is enough; there are many `get_status`
   tests to copy from).

## Gates (each to a log file, never pipe cargo output)
- `cargo test -p orgasmic-daemon --lib -- ledger_sync status` (add your test names if they do
  not match these substrings)
- `cargo test -p orgasmic-cli --lib -- daemon_lifecycle` (targeted; NEVER the whole crate)
- `cargo clippy -p orgasmic-daemon -p orgasmic-cli --all-targets -- -D warnings`
- `cargo fmt --all --check`

## Rules
- Work only in your worktree; commit as `TASK-MSYN4.2: fix(daemon): <one line>`; two commits
  (H2, H5) are fine.
- NEVER `cargo test --workspace`; NEVER the whole `orgasmic-cli` crate in one command (this
  laptop reboots); NEVER set `ORGASMIC_HOME`; NEVER run `daemon start`; never touch the live
  ledger at `~/.orgasmic/ledgers/orgasmic`; never run
  `legacy_drivers_and_explicit_pairs_emit_equivalent_start_events`.
- Report: what changed (`file:line`), each gate with its pass/fail line and log path, unmet
  criteria, residual risk. Finish with `orgasmic dispatch finalize --summary-file <path>`
  (report only, no `--commit`).
