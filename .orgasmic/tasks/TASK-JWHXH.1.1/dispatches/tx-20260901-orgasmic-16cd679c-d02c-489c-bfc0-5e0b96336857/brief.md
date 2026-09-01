# TASK-JWHXH.1.1 — residuals of the JWHXH.1 review (views tmp race, rebuild scope, exists guard, drain teardown)

Fix round 2 for TASK-JWHXH.1 (merged `c3d779af`). The review (claude-opus-5 high,
tx-a4acf9f4) approved with follow-ups; this round takes the four mechanical ones. Read the
task first: `orgasmic task get --project orgasmic TASK-JWHXH.1.1` — it has the exact
`file:line` for each item and the acceptance list. Everything below is the minimum.

## 1. MEDIUM — `crates/orgasmic-core/src/views.rs:122` unique scratch name
`write_if_changed` uses `<file>.<pid>.tmp`. Two `build_views` callers now run concurrently
inside one daemon for the same root: the debounced drain (`index.rs:854`, `spawn_blocking`)
and the synchronous `machines/*/claims.org` arm (`index.rs:959`). Same tmp path → one
truncates the other mid-write → a torn `board.org` (3.0 MB on the live ledger) can be
renamed into place. Fix: append a process-local `static COUNTER: AtomicU64` value after
the PID (`.{pid}.{n}.tmp`). `rename` is already the atomic publish; last-writer-wins is then
correct. Test in `views.rs`: spawn two threads calling `build_views` on one seeded root a few
dozen times each; afterwards every `views/*.org` parses (`OrgFile::parse`) and no `*.tmp`
is left behind.

## 2. LOW — `crates/orgasmic-daemon/src/index.rs:1177` rebuild only for view collections
`schedule_view_rebuild` fires at the tail of `reload_node_dir` for every collection,
including `artifacts`; `build_views` renders only `tasks`/`glossary`/`decisions`
(`views.rs:8-24`). Wrap the call: `if matches!(collection, "tasks" | "glossary" | "decisions")`.
Prefer reusing the collection list from `views.rs` (`VIEWS` is private today — a tiny
`pub fn view_collections() -> [&'static str; 3]` or making the const `pub` is fine; do not
duplicate the three strings in index.rs).

## 3. LOW — `crates/orgasmic-daemon/src/ledger_sync.rs:41` no fabrication
`create_dir_all(.orgasmic)` runs unconditionally after the early return, making the
`if ledger.join(".orgasmic").exists()` guard at `:86` dead and letting the daemon create and
commit a `.orgasmic/.gitignore` in a synced repo that had no `.orgasmic/`. Move the whole
ignore+untrack block under that existing guard (one `if`, no new helper) and drop the
`create_dir_all`.

## 4. LOW — `crates/orgasmic-daemon/src/index.rs:848` drain vs teardown
The detached drain holds only two `Arc`s; every existing test that calls
`apply_written_path` on a node dir now arms a rebuild that can fire during `TempDir`
teardown (`build_views` does `create_dir_all(.orgasmic/views)`). Cheapest honest fix: in the
drain, skip a root whose `.orgasmic` directory no longer exists (one `is_dir()` check before
`build_views`), and say in a comment that a rebuild lost to shutdown/teardown is accepted
because node dirs are the source of truth and the next write or boot rebuilds. Do not
thread a shutdown watch through `Index` for this.

## Gates (each to a log file, never pipe cargo output)
- `cargo test -p orgasmic-core --lib views`
- `cargo test -p orgasmic-daemon --lib -- ledger_sync views`
- `cargo clippy -p orgasmic-core -p orgasmic-daemon --all-targets -- -D warnings`
- `cargo fmt --all --check`

## Rules
- Work only in your worktree; one commit `TASK-JWHXH.1.1: fix(views): <one line>`.
- NEVER `cargo test --workspace`; NEVER the whole `orgasmic-cli` crate; NEVER set
  `ORGASMIC_HOME`; NEVER run `daemon start`; never touch the live ledger at
  `~/.orgasmic/ledgers/orgasmic`.
- Report: what changed (`file:line`), each gate with its pass/fail line and log path, unmet
  criteria, residual risk. Finish with `orgasmic dispatch finalize --summary-file <path>`
  (report only, no `--commit`).
