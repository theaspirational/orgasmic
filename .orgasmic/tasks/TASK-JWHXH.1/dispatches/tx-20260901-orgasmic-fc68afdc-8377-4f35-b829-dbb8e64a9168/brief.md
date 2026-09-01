# TASK-JWHXH.1 — views/ must be ignored on EXISTING ledgers (H4) and regenerate after incremental node writes (M1)

Fix round for two findings of the whole-chain review (tx-1c6d2115, claude-opus-5 high).
Read the task first: `orgasmic task get --project orgasmic TASK-JWHXH.1`.

## The two defects, with the code that has them

**H4 — only NEW projects ignore `views/`.**
`shipped/project-scaffold/.gitignore` is `tmp/\nviews/\n`, so a scaffolded project is fine.
An existing ledger keeps whatever `.orgasmic/.gitignore` it had (this repo's live ledger had
just `tmp/`), so `views/{board,glossary,decisions}.org` stay tracked and the sync loop
(`crates/orgasmic-daemon/src/ledger_sync.rs:28 sync_once_inner`) re-commits them every tick.
The live ledger on this machine was hand-fixed on 2026-09-01; the CODE still does nothing for
any other ledger. `shipped/skills/orgasmic/references/ledger.md:23` promises "derived,
gitignored read views".

**M1 — `build_views` never runs after an incremental write.**
`orgasmic_core::build_views` (`crates/orgasmic-core/src/views.rs:28`, full re-render of every
node in tasks/glossary/decisions, `write_if_changed`) is called from exactly two places:
`index.rs:2970` (inside `load_project`, i.e. boot / full refresh) and `index.rs:920` (the
`machines/*/claims.org` arm of `apply_written_path`). `reload_node_dir` (`index.rs:976`) —
the path every node write takes via `apply_written_path` (`writer.rs:867,904` and the watcher
`watcher.rs:415,426`) — never rebuilds them. Views look fresh on this repo only because
dispatch claim churn rewrites `claims.org` constantly. A project without dispatches serves a
stale `views/board.org` forever after boot.

## What to do — the minimum

### H4: fix it in the sync loop, once per tick, idempotent
In `sync_once_inner`, after the `symbolic-ref == orgasmic && origin exists` early-return
(that is the scope: ledgers the daemon syncs; do NOT touch git state of projects that are not
synced ledgers) and before the existing `git add --all`:

1. If `.orgasmic/.gitignore` has no line equal to `views/`, append `views/\n` (create the
   file if missing; keep existing lines byte-for-byte).
2. `git rm -r -q --cached --ignore-unmatch -- .orgasmic/views` (no-op when untracked).

The existing `add --all` + commit then lands both in the same tick — that matters: the
loop's `pull --rebase --autostash` drops index-only changes, which is exactly how the first
hand-fix attempt on 2026-09-01 failed. Update the stale staging comment above `git add` that
still lists "the generated `views/`" among the staged singletons.

Test in `ledger_sync::tests`, reusing `seed_remote`/`run`: seed the remote with a tracked
`.orgasmic/views/board.org` and `.orgasmic/.gitignore` = `tmp/\n`; run `sync_once` on clone
`a`; assert `git ls-files .orgasmic/views` is empty, `.gitignore` contains `views/`, the file
is still on disk; run `sync_once` again and assert it produced no new commit (idempotent).

### M1: coalesced `build_views` at the tail of `reload_node_dir`
When `reload_node_dir` returns `Ok(true)` (bytes changed), mark that project root dirty and
schedule ONE rebuild per burst — a dispatch close writes N nodes back-to-back through
`writer.rs:867/904` with no debounce in between, so a synchronous call per node would be N
full renders. Minimum design that meets this: a `Mutex<HashSet<PathBuf>>` of dirty roots plus
an `AtomicBool` "drain scheduled" on `Index`; on mark, insert and, if not scheduled,
`tokio::spawn` a task that sleeps a short const (200 ms, same as the watcher default), takes
the set, runs `build_views` per root in `spawn_blocking`, and logs failures with
`tracing::warn!` (the boot path pushes a parse error instead — either is acceptable; say
which you chose). No new module, no trait, no config knob.

`views/` and `tmp/` writes are already dropped by `apply_written_path` (`index.rs:893`) and
by the watcher (`watcher.rs:351 dropped_views`), so the rebuild cannot re-trigger itself —
verify that claim, do not assume it.

Test next to `index::tests::refresh_rebuilds_byte_stable_derived_views` (`index.rs:5703`):
load a project, write a NEW task node dir through `apply_written_path` with no `claims.org`
write at all, wait past the debounce, assert `views/board.org` now contains the new task id.

### Docs
`shipped/skills/orgasmic/references/ledger.md:23` is true after H4 for synced ledgers; if you
change its wording keep it one line. The review cited `shipped/entry/router.org:84`; that
line no longer exists — do not add a claim there.

## Gates (run each to a log file, never pipe cargo output)
- `cargo test -p orgasmic-daemon --lib -- ledger_sync views` (must include your two new tests)
- `cargo clippy -p orgasmic-daemon --all-targets -- -D warnings`
- `cargo fmt --all --check`
No UI change expected; if you touch `ui/`, also `cd ui && npm ci && npm run typecheck`.

## Rules
- Work only in your worktree; commit as `TASK-JWHXH.1: fix(daemon): <one line>`; one commit
  preferred, two at most (H4, M1).
- NEVER `cargo test --workspace`; NEVER the whole `orgasmic-cli` crate; NEVER set
  `ORGASMIC_HOME`; NEVER run `daemon start`; never touch the live ledger at
  `~/.orgasmic/ledgers/orgasmic`.
- Report: what changed (`file:line`), each gate with its pass/fail line and log path, unmet
  criteria, residual risk. Finish with `orgasmic dispatch finalize --summary-file <path>`
  (report only, no `--commit`).
