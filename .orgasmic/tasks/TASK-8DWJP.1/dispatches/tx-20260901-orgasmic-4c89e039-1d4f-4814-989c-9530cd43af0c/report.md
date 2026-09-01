# Review — TASK-8DWJP.1 (conflict-path fix round) + the TASK-MSYN4.2.1 hunk

## Verdict

**REJECT** for TASK-8DWJP.1.

The four assigned findings are all genuinely fixed and the new tests are honest (they
really produce the exit-0 autostash-pop shape and really assert on the bare remote).
But the fix opens a new door onto the same failure the round was dispatched to close:
`park_conflict` is not atomic, and there is no unmerged-index guard on the next tick, so
any failure inside the conflict path leaves the daemon either pushing conflict markers to
every machine or permanently wedged. I reproduced both in throwaway repos.

TASK-MSYN4.2.1 hunk (`9909a41e`): **APPROVE** — one LOW filed.

## Findings

### HIGH 1 — `crates/orgasmic-daemon/src/ledger_sync.rs:164` — `park_conflict` failure re-opens HIGH 2 through a side door

`unmerged_paths()` is called exactly once, at `:177`, *after* the pull (verified: the only
non-test call site). `stage_ledger` (`:164`) and `commit_staged` (`:211`) run *before* it with
no unmerged-index check.

`park_conflict` (`:229-266`) is a sequence of `?`-propagating git calls with the unmerged
index live throughout:

    :232  stage_ledger / commit_staged         (rebase branch only)
    :246  git update-ref <parked_ref> <sha>
    :247  git push origin <ref>:<ref>          (best-effort, warns only — OK)
    :256  git stash drop                       (retained-stash branch)
    :258  git fetch origin orgasmic            <-- network, no timeout
    :259  git rev-parse origin/orgasmic
    :261  git reset --hard origin/orgasmic

If any of these returns `Err` — `git fetch` on a network blip is the reachable one — the tick
returns `Err`, the status goes `failed` + backoff, and the working tree is left with conflict
markers and a UU index. Nothing ever revisits it.

Next tick, `stage_ledger` runs first. `git add --all -- .orgasmic ...` *resolves* the conflict by
staging the marker text. `commit_staged` commits it. The loop pushes it. Reproduced in
`/tmp/orgq.9BJ9`:

    === NEXT TICK ===
    staged? exit=1
    COMMIT OK
    PUSH OK
    === bare remote content ===
    <<<<<<< Updated upstream
    remote-line
    =======
    local-line
    >>>>>>> Stashed changes

That is the original HIGH 2 verbatim: conflict markers committed and propagated to every
machine. Worse than before, because in the retained-stash branch `git stash drop` (`:256`)
already ran before the failing `fetch` — the pre-pull bytes now exist only in the local parked
ref, whose push (`:247`) failed in the same outage.

### HIGH 2 — `ledger_sync.rs:207-232` — the other half: permanent sync wedge

Same root cause, different outcome depending on *where* the leftover conflict is. When the
unmerged path sits under `.orgasmic/machines/<other>/` — the exact vector the new
`autostash_pop_conflict_parks_stash_and_cleans_the_next_tick` test uses — both `stage_ledger`
pathspecs exclude it (`:216 :(exclude).orgasmic/machines`, and `:222` only covers *this*
machine). `git add --all` cannot resolve it, so `commit_staged` fails forever. Reproduced in
`/tmp/orgp.2Sdq/b`:

    add exit=0
    diff --cached --quiet exit=1
    error: Committing is not possible because you have unmerged files.
    commit exit=128
    == after ==
    UU m/other/x.org

Every subsequent tick dies at the same line, backs off to `MAX_BACKOFF` (5 min) and retries
forever. The ledger stops syncing across machines with no self-heal; only a manual
`git checkout --merge` / `reset` in the ledger worktree recovers it.

**Fix direction for both (one guard):** hoist the unmerged read to the top of
`sync_once_with_park`, before `stage_ledger`. Non-empty on entry → go straight into the
conflict path (park + reset) instead of staging. That also makes `park_conflict` idempotent
across a crashed or interrupted attempt, which is what it needs to be given it is a
multi-step non-transactional git sequence.

### MEDIUM 3 — `ledger_sync.rs:256` — `git stash drop` is positional on a *shared* stash stack

Verified, git 2.52, in `/tmp/orgprobe.IyNg`: `refs/stash` is **not** per-worktree. A worktree
and its source checkout see the same stack:

    == main checkout stash list ==
    stash@{0}: On main: OPERATOR-PRECIOUS
    == worktree stash list (shared?) ==
    stash@{0}: On main: OPERATOR-PRECIOUS

The ledger *is* a worktree of the operator's checkout, so this is the live shape.

On the manager's pre-check question — "can this branch be entered for a reason other than a
fresh autostash-pop?" — I traced it and the answer is **no, in practice**. To reach `:184`
you need a non-empty unmerged index *at pull time*, and `commit_staged` (`:211`) refuses to
commit with unmerged entries (proved above), so a leftover conflict wedges the tick before
the pull. The entry path is sound. Also confirmed the intended shape does put the autostash
on `refs/stash`:

    Created autostash: c7d22fb
    Applying autostash resulted in conflicts.
    == stash list ==
    stash@{0}: autostash

What *is* exposed is a TOCTOU on the shared stack. The sha is captured at `:184`; the drop at
`:256` runs after `update-ref` **and a network `git push origin`** — seconds to minutes later,
and now also after the writer-barrier queue wait. An operator `git stash push` in the source
checkout inside that window makes the daemon drop the operator's stash and leave the
autostash on top (so the *next* tick's `stash@{0}` is wrong too). `Created autostash:` is
parsed nowhere.

Recoverability: the parked ref holds the autostash tree, so the daemon's data is safe; the
operator's stash is gone from `git stash list` and recoverable only via reflog. MEDIUM.

**Fix:** drop by verified identity — re-run `rev-parse stash@{0}` immediately before the drop
and require it still equals `local_head`; otherwise `failed` + backoff, no drop. Parsing
`Created autostash: <sha>` from the pull stdout and requiring the match is the stronger form
and closes the entry-path question permanently.

### MEDIUM 4 — `writer.rs:2421` — the barrier runs unbounded network git on a runtime worker

`writer_loop` is a plain `tokio::spawn` task (`writer.rs:1765`), not a dedicated thread. The
`Barrier` arm calls `run()` inline — no `spawn_blocking`, no `block_in_place`. The barrier body
is `park_conflict`, which contains `git push origin` (`:247`) and `git fetch origin orgasmic`
(`:258`) with no timeout configured anywhere (`git_output`, `:611`, sets only `LC_ALL=C`).

A blackholed remote (VPN drop, hung TLS handshake) therefore blocks the writer actor
*indefinitely* — every node write, tx append and API mutation queues behind it — and pins one
tokio worker thread for the duration. The fencing requirement is only the
salvage-commit → `update-ref` → `reset --hard` sequence; the network calls do not need it.

**Fix:** hoist `git fetch` to before `run_barrier` and move the best-effort parked-ref push to
after it, leaving only local git inside the fence. If the network must stay inside, at minimum
use `block_in_place`.

No deadlock: `park_conflict` never calls back into the writer, and the
`runtime.block_on(...)` at `:552` sits inside `spawn_blocking`, which is legal.

### LOW 5 — `writer.rs:2421` — a panicking barrier body wedges the writer forever

`run()` is called with no `catch_unwind`, and `reply` is sent only after it returns. A panic
inside any barrier body aborts `writer_loop`; every later write then fails
`"writer task is gone"` for the daemon's lifetime. `park_conflict` is `Result`-based so this is
not live today, but `run_barrier` is a generic `FnOnce() -> T` API and any future caller
inherits it. `catch_unwind` + always replying is ~4 lines.

### LOW 6 — `ledger_sync.rs:495` — `PATHS` is space-joined

`("PATHS".into(), paths.join(" "))`. The `-z` read at `:271` handles spaces correctly; only the
event serialization loses them. Ledger paths are daemon-generated today, so it is latent.

### LOW 7 (TASK-MSYN4.2.1) — `ledger_sync.rs:123-133` — the sidecar untrack glob has no ownership check

`git rm -r --cached --ignore-unmatch -- :(glob).orgasmic/**/*.tmp *.tmp.* *.bak.*` runs every
tick against the whole index. Any legitimately tracked ledger file whose *basename* ends in
`.tmp` or contains `.bak.` is silently untracked and its deletion pushed to every machine.
Today only writer sidecars match (artifacts are `.orgasmic/artifacts/ART-<slug>/` with
fixed filenames, and `*` does not cross `/` so a directory named `...bak...` is safe), so this
is a naming-collision hazard rather than a live bug.

## What the round got right

All four assigned findings are genuinely closed, and the tests are not theatre:

- **HIGH 1 (tx route).** `:499-503` now `.join("tx")`. The test asserts the literal
  `".orgasmic/machines/{machine_id}/tx/{}.org"` string rather than re-deriving the
  expression (`:1006`), so it would actually fail if the segment regressed.
- **HIGH 2 (autostash-pop detection).** I reproduced the exact shape the predecessor
  described — `PULL EXIT=0`, no `CONFLICT(` line, `UU`, no rebase dir, `stash@{0}: autostash`
  (git 2.52.0, `/tmp/orgp.2Sdq`). The new `unmerged_paths` check at `:177` fires regardless of
  exit code, and `rebase_in_progress` (`:281`) correctly distinguishes the two branches via
  `rev-parse --git-path rebase-merge|rebase-apply` (worktree-correct, unlike a hardcoded
  `.git/`).
  The test vector is honest: a foreign `machines/<other>/state.org` dirty into the pull plus a
  remote edit to the same file, i.e. the real exit-0 path — not a simulated one. It asserts the
  parked ref holds `"local bytes"` (not the remote's), the worktree holds `"remote bytes"`,
  `unmerged_paths` is empty, `status --porcelain` is clean, the second tick is `Synced`, and the
  **bare remote** contains `"remote bytes"` with no `<<<<<<<`.
- **MEDIUM 3 (prose scrape).** `conflict_paths` and its `rsplit_once(" in ")` are gone — no
  callers, no dead residue (`grep` for `conflict_paths` returns nothing). Paths come from
  `git diff --name-only --diff-filter=U -z` read *before* `rebase --abort`, correctly ordered.
  The modify/delete case is covered by a real test (remote deletes, local modifies;
  `assert_eq!(paths, &[relative])`, and `assert!(!a.join(relative).exists())` after the reset).
  `-z` splitting on NUL handles paths with spaces.
- **MEDIUM 4 (barrier).** `WriterCommand::Barrier` + `run_barrier` exist and `park_conflict`
  runs inside (`:549-560`); the `record_sync_conflict` append happens after the barrier
  returns, in the async loop (`:568`). `barrier_runs_before_an_append_queued_during_it`
  waits for `queue_depth == 1` before releasing, so it genuinely proves ordering rather than
  racing.

Also checked and clean:

- **Cached tx handles after `reset --hard`.** `tx_handles_detached_from_paths` (`writer.rs:2895`)
  runs before every append batch and compares a `FileIdentity` of `(dev, ino)` on unix
  (`writer.rs:2702`). `git reset --hard` replaces the file rather than truncating in place, so
  the inode changes and the handle is dropped. The manager's "same inode, shorter file"
  worry does not arise on unix; on the `not(unix)` arm the identity is `(len, mtime)`, which
  *would* catch a length change anyway.
- **Stale `ponytail:` comment (`:156-162`).** It describes the *staging* window (`add` #1/#2 vs.
  the writer's rename+append), which the new barrier does not fence — it only fences
  `park_conflict`. The comment is still accurate. Not a finding.
- **`retain_live_statuses`** (`:518`) compares `entry.path` against the same `entry.path` used for
  the insert key. Same source, no drift.
- **`Idle` preserving `last_success_at`** (`:336-339`) is correct and tested
  (`idle_sync_does_not_record_a_success`).
- **`doctor.rs` vs `daemon status`.** Different commands — `main.rs:2790` prints `ledger_sync`
  for `daemon status`, `push_ledger_sync_findings` only runs from `doctor`. No double-print.
  `backed_off` keeps the previous error and failure count (`:308-310`), so the warning text is
  populated.
- **Sidecar untrack ping-pong.** Cannot happen: both machines' `stage_ledger` `add` pathspecs
  carry the same `*.tmp` / `*.bak.*` excludes, so a machine never re-adds a sidecar another
  machine untracked.
- **LOW 5 (conflict-ref count in `daemon status`)** was skipped by design, as declared.

## Open Questions

1. Is the manual recovery for HIGH 2 (a wedged UU index in the ledger worktree) documented
   anywhere an operator would find it, or does `doctor` need to name the fix? Right now
   `doctor` says `ledger sync: <path> (N failures): Committing is not possible...` and stops.
2. Should the parked-ref push failure be surfaced (status/doctor) rather than only
   `tracing::warn!`? Under HIGH 1 it is exactly the case where the local bytes are at risk.

## Verification Notes

Commands run (all read-only against the repo; all mutating git confined to throwaway
`/tmp` repos I created):

- `cargo test -p orgasmic-daemon --lib -- ledger_sync barrier` → **16 passed, 0 failed** (5.03 s).
  Log: `/tmp/gate-8dwjp-review.log`. Includes both new tests and
  `two_daemon_loops_converge_through_the_bare_remote` (passed, no flake).
- `git --version` → **2.52.0** (same version the predecessor measured on).
- Probe A `/tmp/orgprobe.IyNg` — worktree + source checkout share `refs/stash`. Evidence quoted
  in MEDIUM 3.
- Probe B `/tmp/orgp.2Sdq` — reproduced the exit-0 autostash-pop shape: `PULL EXIT=0`,
  `Created autostash: c7d22fb`, `UU`, no rebase dir, `stash@{0}: autostash`, markers in the file.
- Probe C `/tmp/orgp.2Sdq/b` — leftover UU under a foreign machine dir → `git add --all` with
  the real exclude pathspecs leaves it unresolved → `commit` exits 128. Evidence in HIGH 2.
- Probe D `/tmp/orgq.9BJ9` — leftover UU on a shared path (`.orgasmic/project.org`) →
  `add --all` resolves it, `COMMIT OK`, `PUSH OK`, markers land on the bare remote. Evidence
  in HIGH 1.
- Static: `grep -n "diff-filter=U|unmerged"` (only call site is `:177`), `grep conflict_paths`
  (gone), `writer.rs:1765` (`tokio::spawn(writer_loop(...))`), `writer.rs:2418-2422`
  (Barrier arm), `writer.rs:2702-2732` (`FileIdentity`).

### What I did **not** check

- Did not re-run the four gates (`clippy`, `fmt`, `orgasmic-cli daemon_lifecycle`) — the
  implementer and the manager both ran them on merged `a64d5cf8`, and the brief marked that
  established. I ran only the daemon-side targeted subset.
- Did not exercise the conflict path against a **real** remote (network failure injection); the
  HIGH 1/HIGH 2 reproductions simulate the post-failure state directly rather than making
  `git fetch` fail. The state I start from is exactly what an `Err` at `:258` leaves behind, but
  I did not drive the daemon end-to-end through it.
- Did not test the barrier under a hung network (MEDIUM 4) — that finding rests on reading
  `writer_loop`'s `tokio::spawn` and the absence of any timeout, not on a measurement.
- Did not measure the per-tick cost of the MSYN4.2.1 `git rm --cached` globs on a large
  ledger. Two extra index walks per 2 s tick; I expect this is immaterial below ~100k tracked
  files but have no number.
- Did not exercise the TOCTOU race in MEDIUM 3 (concurrent operator `git stash push` during a
  park); the shared-`refs/stash` half is measured, the race half is by inspection.
- Did not look at `verify/*/injection.patch` (prohibited) and did not touch the live ledger
  beyond read-only `git`/`orgasmic task get`/`orgasmic tx record`.

## Fix Directions

1. **HIGH 1 + HIGH 2 (one change).** Read `unmerged_paths` at the *top* of
   `sync_once_with_park`, before `stage_ledger`. Non-empty → enter the conflict path
   immediately (park what is recoverable, then `fetch` + `reset --hard`) instead of staging.
   This makes the conflict path re-entrant, which it must be: `park_conflict` is a multi-step
   non-transactional git sequence with a network call in the middle, so "it failed halfway"
   is a state the next tick has to be able to recognise, not one it should paper over. Add a
   test that drops the tick between `stash drop` and `reset --hard` (an injectable failure
   point, mirroring the existing `before_push` seam) and asserts the next tick pushes no
   `<<<<<<<` to the bare remote.
2. **MEDIUM 3.** Re-verify `rev-parse stash@{0} == local_head` immediately before
   `git stash drop`; on mismatch return `failed` + backoff without dropping. Optionally parse
   `Created autostash: <sha>` from the pull stdout and require the match.
3. **MEDIUM 4.** Move `git fetch origin orgasmic` before `run_barrier` and the best-effort
   parked-ref `push` after it, so only local git runs inside the fence.
4. **LOW 5.** Wrap `run()` in `catch_unwind` and always send `reply`.
5. **LOW 6 / LOW 7** are latent; fold into a residual note rather than a round of their own.

**Verdict for TASK-8DWJP.1: REJECT.**
