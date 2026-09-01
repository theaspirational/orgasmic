# Review: TASK-JWHXH.1 — views/ ignored on existing ledgers (H4) + coalesced view rebuild (M1)

Commit reviewed: `49de897f` (merged as `c3d779af`), 2 files, +149/-3.

## Verdict

**APPROVE WITH FOLLOW-UPS.** Both acceptance criteria are met on the path the fix
targets. The coalescer is race-correct as written and the two new tests are honest.
Six follow-ups below; none is a ship blocker, but #1 and #3 should be scheduled
before the next multi-machine release.

## Findings

### 1. MEDIUM (correctness, latent-but-now-routine) — `crates/orgasmic-core/src/views.rs:122`

`write_if_changed` names its scratch file with the PID only:

    tmp_name.push(format!(".{}.tmp", std::process::id()));

There are now two `build_views` call sites inside one daemon process that can run
concurrently for the same project root:

- `index.rs:854` — the new drain, on a `spawn_blocking` thread;
- `index.rs:959` — the `machines/*/claims.org` arm of `apply_written_path`,
  synchronous on a runtime worker thread.

Nothing serialises them (the claims arm calls `build_views` after dropping the
index write lock; the drain holds no index lock at all).

Failure scenario: a dispatch writes `tasks/TASK-X/node.org` at T0 → drain armed for
T0+200 ms. The manager writes `machines/<id>/claims.org` at T0+195 ms → the claims
arm starts writing `.orgasmic/views/board.org.<pid>.tmp`. On the live ledger
`board.org` is **3,070,784 bytes** (`ls -la ~/.orgasmic/ledgers/orgasmic/.orgasmic/views/`),
rendered from 801 task + 116 decision + 60 glossary nodes, so that write is not
instantaneous. At T0+200 ms the drain calls `std::fs::write` on the *same* tmp path,
truncating it mid-write. Whichever `rename` lands first publishes a truncated or
interleaved `board.org`. Consumers are the prompt-studio context packs
(`shipped/prompt-studio/context-packs/sprint_tasks.org` → `.orgasmic/views/board.org`)
and `prompt-specs/manager.org`.

Not data loss — node dirs are the source of truth and the next rebuild self-heals
within 200 ms–2 s — but an agent can read a garbage board in that window.

This race pre-existed (`load_project` at `index.rs:3011` vs. the claims arm), but it
was narrow. This change makes the two writers fire on the same project inside the
same debounce window during ordinary dispatch churn.

**Fix direction:** make the tmp name unique per write (append a process-local
`AtomicU64` counter or `thread::current().id()` to the PID). One line in
`views.rs:122`; `rename` is already atomic, so last-writer-wins is then correct.

### 2. MEDIUM (unmet acceptance, partial) — `crates/orgasmic-daemon/src/ledger_sync.rs:33-36`

The remediation sits behind the early return:

    if git_optional(ledger, &["symbolic-ref", "--short", "HEAD"])?.as_deref() != Some("orgasmic")
        || git_optional(ledger, &["remote", "get-url", "origin"])?.is_none()
    { return Ok(SyncOutcome::Idle); }

`sync_once` is called for **every board project path** every 2 s
(`ledger_sync.rs:168-178`), but only repos on branch `orgasmic` *with* an `origin`
get past this. So two populations are never fixed:

- an existing ordinary project checkout that carries `.orgasmic/` committed in its
  own tree (branch `main` etc.) — the majority of orgasmic-using repos;
- an `orgasmic`-branch ledger with no remote configured.

Nothing else closes the gap: `init_project` skips any file that already exists
(`crates/orgasmic-core/src/projects.rs:188` — `if dest.exists() { continue; }`), so the
scaffold's `tmp/\nviews/\n` never reaches a project whose `.gitignore` already says
`tmp/`; and `project_migrate.rs` never touches `.gitignore` (only its test fixture at
line 648 writes one).

H4 said "the CODE still leaves every other existing ledger tracked". That is closed
for synced ledgers and still open everywhere else — those repos keep committing a
3 MB derived `board.org` into user history and will keep conflicting on it.

**Fix direction:** hoist the ignore+untrack out of `sync_once_inner` into something
that runs for every registered project (daemon boot, or `project migrate`), gated on
"is a git repo" rather than "has an origin".

### 3. MEDIUM (transition hazard, not verified on hardware) — `crates/orgasmic-daemon/src/ledger_sync.rs:129`

Mixed-version fleet, reasoned from the loop, **not reproduced**:

- Machine A (fixed) untracks `views/*.org`, commits the deletion, pushes.
- Machine B still on the pre-fix daemon: its `views/*.org` are tracked and rewritten
  constantly by `build_views`, so `git add --all -- .orgasmic` stages *modifications*
  and commits them. Then `git pull --rebase --autostash origin orgasmic` rebases a
  modify onto a delete → **modify/delete conflict** → `rebase --abort` →
  `bail!("git pull --rebase failed: …")` at line 129.

B's ledger sync then fails on every 2 s tick until B is upgraded. Once both machines
run the fixed code the untrack happens *before* `git add --all`, so both sides commit
a deletion and delete/delete rebases cleanly — that direction I believe is safe, but
I could not exercise a real two-machine setup, only the single-worktree tests.

Practically: the live ledger's cutover already happened by hand on 2026-09-01
(`.orgasmic/.gitignore` is `tmp/\nviews/\n`, `git ls-files .orgasmic/views` is empty),
so the exposure is any *other* ledger, plus the gigabyte machine if it runs an older
runtime.

**Fix direction:** none in code required; note the upgrade order in the release notes
(upgrade every machine before the first fixed daemon pushes the untrack), or make the
old-version failure loud rather than a `warn!`-and-retry.

### 4. LOW (dead guard / scope) — `crates/orgasmic-daemon/src/ledger_sync.rs:41`

`std::fs::create_dir_all(&dotorg)` now runs unconditionally, which makes the
pre-existing guard at line 86 (`if ledger.join(".orgasmic").exists()`) permanently
true. A repo that is on branch `orgasmic` with an origin but has no `.orgasmic/` at
all now gets one created, a `.gitignore` written into it, staged by `git add --all`,
committed and pushed. Narrow, but the daemon fabricating and publishing a file in a
repo it was only supposed to observe is a behaviour change nothing tests.

**Fix direction:** only `create_dir_all` when `.orgasmic` already exists, or move the
whole block under the existing `exists()` guard.

### 5. LOW (perf/scope) — `crates/orgasmic-daemon/src/index.rs:1177`

`schedule_view_rebuild` sits at the common tail of `reload_node_dir`, so it fires for
**every** collection — including `artifacts`. `build_views` renders only
`tasks`/`glossary`/`decisions` (`views.rs:8-24`), so an artifact node write costs a
full 977-node re-read and a 3.0 MB re-render that provably cannot change any view.

I did **not** measure `build_views` wall-clock on the live corpus (that would have
required running a mutating CLI verb against a copy of the ledger). The frequency
amplification is proven from code; the per-rebuild cost is not.

**Fix direction:** `if matches!(collection, "tasks" | "glossary" | "decisions")` around
the call.

### 6. LOW (test hygiene) — `crates/orgasmic-daemon/src/index.rs:848`

The drain is `tokio::spawn`'d detached, holding only the two `Arc`s — no handle on the
`Index` or on the test's `TempDir`. Every *existing* test that calls
`apply_written_path` on a node dir now arms a rebuild that fires 200 ms later, often
after the test has returned and `TempDir::drop` has started deleting the tree.
`build_views` does `create_dir_all(project_root.join(".orgasmic/views"))`, so it can
resurrect directories mid-teardown. `TempDir`'s drop ignores errors, so this is stray
`/tmp` dirs plus `warn!` noise rather than a failure today — but it is a new flake
surface, and it is exactly the shape that bites once a test asserts on log output or
on an empty temp root.

**Fix direction:** none needed if accepted knowingly; otherwise gate the spawn behind a
shutdown watch, or keep the `JoinHandle` on `Index` and abort it on drop.

## What I checked and found clean

- **Coalescer race analysis** (`index.rs:841-873`). Walked all three interleavings the
  brief named. A mark can never be dropped: the marker `insert`s under the set lock
  *before* `swap`ping the flag, and the drain's `is_empty` check and
  `scheduled.store(false)` both happen while holding that same lock. If the marker wins
  the lock, the drain sees a non-empty set and loops; if the drain wins, it clears the
  flag and the marker's `swap` returns `false` and spawns a fresh drain. Two drains
  cannot overlap — the only path that clears the flag immediately `return`s.
  `mem::take`'s temporary guard is dropped at end of statement, and the loop-body guard
  is dropped before the next `sleep`, so no lock is held across an `await`.
- **Runtime shutdown.** `schedule_view_rebuild` is only reached from `reload_node_dir`,
  which is always inside the runtime, so `tokio::spawn` cannot panic for want of a
  runtime; a shutdown mid-burst drops the task (rebuild skipped, no panic). The
  `.lock().unwrap()`s could wedge the drain permanently on poisoning, but only `HashSet`
  insert/take/is_empty run under those locks, so poisoning is not reachable in practice.
- **No self-trigger / rebuild storm.** Confirmed from source, not the brief:
  `apply_written_path` drops `.orgasmic/views` writes at `index.rs:933`
  (`matches!(parts.get(1).copied(), Some("tmp" | "views"))`), and the watcher classifies
  them `dropped_views` at `watcher.rs:351-354` before they reach `project_paths`. The
  `board.org.<pid>.tmp` scratch file lands under the same prefix, so it is dropped too.
  No other consumer of fs events under `.orgasmic/views` exists.
- **Test honesty, `index::tests::incremental_node_write_rebuilds_views_without_claim_churn`.**
  `Index::rebuild()` eagerly `load_project`s every board entry (`index.rs:1302-1341`), so
  `snap.projects` is populated and `reload_node_dir` takes the incremental branch, not the
  `refresh_project` fallback at `index.rs:1133-1137`. `TASK-NEW` is written *after*
  `rebuild()`, so it cannot have reached `views/board.org` via `load_project`. The only
  writer that can satisfy the poll is the coalescer. The test is honest.
- **Test honesty, `ledger_sync::tests::existing_ledger_views_are_ignored_untracked_and_idempotent`.**
  `sync_once` runs the full body including `pull --rebase` and `push`; the second call
  does not short-circuit before them, it reaches `diff --cached --quiet`, finds nothing
  staged, skips the commit, and still pulls and pushes. Asserting HEAD is unchanged
  therefore does exercise the pull/push path.
- **`.gitignore` matching.** `views/` is matched as a whole line with `\r` stripped, so a
  commented `#views/` correctly does not match. `views` (no slash) and `/views/` also do
  not match and would produce a second, harmless `views/` line — idempotent from the next
  tick onward. Byte-preserving append with a newline fixup for a file lacking a trailing
  newline is correct.
- **`git rm --cached` semantics.** `rm --cached` does not consult `.gitignore` for tracked
  paths, and `--ignore-unmatch` makes the steady-state call a no-op. Verified by the
  passing idempotency test rather than by version-specific reasoning.
- **Live ledger state.** `~/.orgasmic/ledgers/orgasmic/.orgasmic/.gitignore` is
  `tmp/\nviews/\n`; `git ls-files .orgasmic/views` is empty. The hand cutover holds and the
  new code is a no-op there, as intended.
- **Targeted tests, rerun independently on the merged tree:**

      cargo test -p orgasmic-daemon --lib -- \
        existing_ledger_views_are_ignored_untracked_and_idempotent \
        incremental_node_write_rebuilds_views_without_claim_churn
      # 2 passed; 0 failed

## Open questions

1. Is any second machine (gigabyte) currently running a pre-fix daemon against a ledger
   whose `views/*.org` are still tracked? That decides whether finding #3 is live or
   already moot.
2. Was limiting H4 to synced ledgers a deliberate scope call, or an oversight? The
   acceptance line says "existing ledger", which the fix satisfies; finding #2 is about
   ordinary project checkouts, which the original H4 text arguably also covered.
3. The acceptance item "router.org claim is true after the change" appears misattributed:
   `shipped/entry/router.org` is 38 lines and contains no mention of `views` (there is no
   line 84). The claim that views are gitignored lives at
   `shipped/skills/orgasmic/references/ledger.md:23`, and it is now true for synced
   ledgers and still false for the repos in finding #2.

## What I did not check

- No wall-clock measurement of `build_views` on the 977-node live corpus (would have
  required running a mutating verb against a ledger copy). Cost claims are structural
  only.
- No real two-machine reproduction of finding #3; reasoned from `sync_once_inner` alone.
- Did not rerun clippy, fmt, or the broader `ledger_sync`/`views` suite — the brief
  records those as already green and I had no reason to doubt them.
- Did not read `verify/*/injection.patch`, per the brief.
- Did not exercise the `refresh_project` fallback branch of `reload_node_dir`
  (`index.rs:1133`) against the new scheduler; it reaches `build_views` via
  `load_project` synchronously, which is the pre-existing path.

## Fix directions, ranked

1. `views.rs:122` — unique tmp suffix (PID + atomic counter). Smallest diff, kills #1
   and the pre-existing latent race with it.
2. `index.rs:1177` — gate `schedule_view_rebuild` on the three collections that views
   actually render. One `matches!`.
3. `ledger_sync.rs:41` — put `create_dir_all` behind the existing `.orgasmic` exists
   check so the guard at line 86 stops being dead.
4. #2 (non-synced repos) is a follow-up task, not a patch to this commit — it needs a
   decision about where migration for ordinary checkouts belongs.
5. #3 is a release note, not code.

**APPROVE WITH FOLLOW-UPS.**
