# Review: TASK-8DWJP.1.1 — re-entrant conflict recovery (merged 59c351dc)

**Verdict: REJECT** — one new HIGH (a silent permanent wedge on the exact
"interrupted attempt" the task exists to close) plus one HIGH data-loss gap in the
recovery path itself. The six assigned items are otherwise correctly implemented and
the four listed gates reproduce green.

## What the round got right (verified, not taken on trust)

- **Entry guard order.** `unmerged_paths` is read at `ledger_sync.rs:104`, before the
  `.gitignore`/`git rm --cached` block and before `stage_ledger` (`:177`). No path
  stages over a UU index. The HIGH 1+2 marker-push and foreign-machine wedge from the
  8DWJP.1 REJECT are closed.
- **`Created autostash:` parse.** Probed on git 2.52: the autostash-pop conflict exits
  **0** and prints `Created autostash: <short>` on **stdout** (stderr only carries
  "Applying autostash resulted in conflicts"). `created_autostash` (`:311-318`) is
  reading the right stream.
- **Stage 3 is the local side** in the pop shape (probed: `:2:` = "remote bytes",
  `:3:` = "local bytes"). The rebase shape never reaches
  `commit_matches_conflict_side` — `conflict_source_on_entry` aborts the rebase first
  (`:332-335`) and returns `Worktree` — so the "are stages swapped in the other shape"
  hazard does not apply.
- **Stash identity.** `park_conflict_inner:451-458` re-reads `stash@{0}` immediately
  before the drop and bails on mismatch. `git stash list -1 --format=%gs` yields the
  literal subject `autostash` (probed), so the entry-guard candidate check is sound.
  There is still a positional `git stash drop` after the compare — the window is one
  process spawn, which is the right trade; say so rather than chase it.
- **Barrier contents.** Read the closure at `:374-403` and `:694-711`: `git fetch
  origin orgasmic` runs *before* `run_barrier`, the best-effort parked-ref push *after*
  it, and the fenced body (`park_conflict`) is local-only — `stage_ledger`,
  `commit_staged`, `update-ref`, `rev-parse`, `stash drop`, `reset --hard`. No
  `push`/`fetch`/`ls-remote` inside. `remote_head` is read inside the fence from the
  pre-fence fetch, and nothing assumes it is the latest (the next tick pulls again).
- **`catch_unwind`.** `writer.rs:2428`. On panic `result_tx` is dropped, so
  `run_barrier` returns `Err("writer barrier result dropped")` and the ledger tick
  fails into backoff — not a silent `Ok`. The panic message still reaches stderr via
  the default hook (not `tracing`). `park_conflict` has no `expect`/`unwrap`, so the
  half-done-git-sequence case is theoretical; the next tick's guard would handle it.
- **Parked-ref stability.** Re-entry reuses the matching ref (`ConflictSource::Parked`,
  `:423`); a new timestamped ref is minted only when none matches. No litter per retry.
- **LOW 6.** `PATHS` is tab-joined (`:613`).
- **No regression from earlier rounds:** the literal `machines/<id>/tx/<month>.org`
  route, the modify/delete conflict, and `PARKED_REF` are still asserted in
  `conflicting_two_writer_tick_parks_recovers_and_records_event`;
  `barrier_runs_before_an_append_queued_during_it` still passes.

## Findings

### HIGH 1 — `ledger_sync.rs:98-104`: an interrupted rebase bypasses the new guard entirely; the ledger wedges silently and forever

The Idle gate runs **before** the unmerged-paths guard:

```rust
98:  if git_optional(ledger, &["symbolic-ref", "--short", "HEAD"])?.as_deref() != Some("orgasmic")
99:     || git_optional(ledger, &["remote", "get-url", "origin"])?.is_none()
       { return Ok(SyncOutcome::Idle); }
104: let paths = unmerged_paths(ledger)?;
```

During an in-progress rebase HEAD is detached. Probed on git 2.52 in a throwaway repo
(`/tmp/rbprobe.jt2V`), after a conflicting `git pull --rebase --autostash`:

```
pull exit=1
--- unmerged ---            .orgasmic/n.org
--- symbolic-ref --short HEAD ---
fatal: ref HEAD is not a symbolic ref      (exit 128)
--- remote get-url ---      /tmp/rbprobe.jt2V/remote.git
```

So `git_optional` returns `None`, `None != Some("orgasmic")`, and `sync_once_with_park`
returns `Ok(SyncOutcome::Idle)` — line 104 is never reached.

**Failure scenario.** The daemon is killed between the conflicting `git pull --rebase`
(`:186`) and the `git rebase --abort` (`:192`) — a launchd restart, an in-place binary
swap (the SIGKILL trap already recorded for local update), machine sleep. On restart the
ledger sits mid-rebase. Every subsequent tick returns `Idle`, status is `"idle"` with
`consecutive_failures: 0`, `next_attempt_at: None`, `last_success_at` carried forward
(`:517`, `:539`). No conflict, no failure, no backoff, no publish — this machine's
ledger writes silently stop reaching every other machine, forever, and the outcome the
operator sees is the one the code comment (`:65-67`) documents as *"the normal
single-machine cases"*. There is no self-heal and `doctor` names no recovery (that text
was the item explicitly skipped).

This is the acceptance criterion "the conflict path must be re-entrant across a crashed
or interrupted attempt", unmet for the rebase-conflict shape. The new guard covers only
the autostash-pop shape, where HEAD stays attached.

**Fix direction.** Check the remote first, then `rebase_in_progress` (it already exists,
`:288-303`) before the symbolic-ref gate: if a rebase is in progress and
`rebase-merge/head-name` reads `refs/heads/orgasmic`, `git rebase --abort` and fall
through to the normal tick. Test: leave a mid-rebase repo on disk (no injection needed —
run the conflicting pull, do not abort) and assert the next `sync_once` recovers instead
of returning `Idle`.

### HIGH 2 — `ledger_sync.rs:424-462`: recovery resets away every tracked-file write made after the conflicting pull

`park_conflict_inner` stages the worktree only in the `Worktree` branch (`:428-437`).
The `Parked`, `Autostash` and `Unrecoverable` branches go straight to
`git reset --hard origin/orgasmic` (`:462`). Any daemon write that landed in the ledger
worktree since the conflicting pull, to a file git already tracks, is discarded.

Probed with the production command sequence verbatim (`/tmp/ledgerprobe.2guK`): after the
conflicting pull I wrote a fresh line into a tracked, non-conflicted ledger file, then
ran `fetch` → `update-ref` → `stash drop` → `reset --hard origin/orgasmic`:

```
=== worktree other.org after recovery ===   other base
=== parked ref other.org ===                other base
=== is the fresh line anywhere? ===         0
```

Zero surviving copies — not in the worktree, not in the parked ref, not in any commit.

**The window is not small, and this round widened it.** Three sources:
1. `recover_conflict:375` now runs a **network** `git fetch` before `run_barrier` (the
   correct fix for MEDIUM 4, but it is unfenced time).
2. Writes already queued ahead of the barrier drain into the worktree *before* the
   fenced body runs. The barrier stops writes *during* `park_conflict`; it does not
   capture the ones that just landed.
3. The re-entry path spans a **whole backoff cycle** (2 s → up to `MAX_BACKOFF` 5 min):
   the recovery tick fails (the pre-barrier `fetch` `?`-propagates on a network blip),
   the daemon keeps appending tx entries to the tracked
   `machines/<id>/tx/<month>.org`, and the next successful recovery tick deletes all of
   them.

`.orgasmic/machines/<id>/tx/<month>.org` is tracked and committed, so what is lost is
ledger tx entries — claims, transitions, dispatch closes. Under dec_EWY0K point 2
("nothing this machine wrote is ever lost") this is an invariant violation, and it is
silent: the status string is `"N conflicting paths parked at <ref>: …"` (`:527-531`) and
says nothing was discarded.

To be fair on classification: the same `reset --hard` existed pre-fix, so the *class* is
pre-existing rather than introduced. What this round changes is that the re-entry path
is now the *designed* recovery route and the unfenced pre-barrier fetch is new — the
window went from incidental to structural.

**Fix direction (cheap).** Before the reset, snapshot the worktree into a second parked
commit through a scratch index, which sidesteps the UU-blocks-commit problem entirely:
`GIT_INDEX_FILE=$tmp git read-tree origin/orgasmic`, `git add -A` with the existing
`stage_ledger` pathspecs, `write-tree`, `commit-tree`, `update-ref`. Five git calls, all
local, fits inside the fence. Minimum acceptable alternative: name the discard in the
conflict status/error string so it is not silent.

### MEDIUM 3 — `ledger_sync.rs:320-329`: `None == None` lets a stale parked ref pass as the local side

```rust
323: if git_optional(ledger, &["rev-parse", &format!("{commit}:{path}")])?
324:    != git_optional(ledger, &["rev-parse", &format!(":3:{path}")])?
```

A delete/modify conflict has no stage 3. Probed (`/tmp/dmprobe.*`): local deletes a file
the remote modified, the pop conflict leaves stages 1 and 2 only, and
`git rev-parse :3:<path>` exits non-zero — `git_optional` yields `None`. Any candidate
commit that also lacks the path yields `None`, and `None == None` counts as a match.

Parked refs are scanned before the stash (`:344-357`), so a stale
`refs/orgasmic/conflicts/<machine>/…` from an earlier conflict whose tree also lacks the
path is accepted as this conflict's local side. Consequence: `ConflictSource::Parked` →
`retained_stash = false` → the real autostash holding the *current* local bytes is never
dropped and never parked, `reset --hard` discards the worktree, and the conflict event
points the operator at a ref with stale content. Reachability is narrow (needs a prior
parked ref shaped just so) but the failure is silent misdirection plus loss.

**Fix direction.** Treat an absent `:3:` as non-matchable for parked-ref candidates —
require at least one path with `Some` on both sides, or restrict the absent-stage-3
all-paths case to the retained autostash (identity), which is the candidate that is
genuinely known to be this tick's local side.

### LOW 4 — `ledger_sync.rs:451-458`: a mismatched identity check orphans the autostash permanently

`update-ref` runs before the identity check, so when the check bails the parked ref
already exists. The next tick's `conflict_source_on_entry` matches that ref, takes
`ConflictSource::Parked`, and `retained_stash` is `false` — the drop never happens on any
later tick. The bytes are safe (they are in the parked ref) but one entry accumulates on
the shared `refs/stash` per collision, on a stack the operator also uses.
`foreign_stash_on_top_is_not_dropped` (`:1322`) leaves exactly this state and stops
without running the next tick, so nothing pins the behaviour either way.

### LOW 5 — `ledger_sync.rs:389-400`: a failed parked-ref push is only a `tracing::warn`

Listed in the assignment as optional; the implementer's note says it was skipped, which
matches the code. Worth stating that this is the case where the local side exists *only*
in a ref that never reached the remote — precisely the outage the HIGH was about — and
the operator-facing status does not mention it.

### LOW 6 — test gap around the silent-loss branches

`leftover_foreign_machine_conflict_does_not_wedge_the_next_tick` (`:1294-1319`) destructures
`SyncOutcome::Conflict { paths, .. }` and never asserts `parked_ref` is non-empty, so a
regression into the `Unrecoverable` branch (which resets with no ref and no salvage)
passes green. The post-conflict write in
`conflicting_two_writer_tick_parks_recovers_and_records_event` (`fresh = ".orgasmic/tasks/T2/node.org"`)
is a *new untracked* file, which `reset --hard` preserves — so no existing test would
catch HIGH 2. One assertion on a *tracked, modified* file would.

## Open Questions

1. HIGH 2 under dec_EWY0K: is silently discarding backoff-window tx appends an accepted
   cost of the conflict path, or does "nothing this machine wrote is ever lost" bind
   here? If accepted, the status string should say it; if not, the scratch-index salvage
   is the small fix.
2. `record_sync_conflict` failure (`:713-720`) is only a `tracing::warn` and the tick is
   still counted a success — dec_EWY0K point 3's ledger event can be absent while the
   status says `conflict`. Pre-existing, out of this round's scope; flagging for the
   parent.
3. Parked refs are never pruned. Unbounded growth on a machine that conflicts often.
   Pre-existing.

## Verification Notes

Checked:
- `git diff 59c351dc^1 59c351dc` in full, plus `59c351dc^1:ledger_sync.rs` for the
  regression-vs-pre-existing classification of HIGH 2.
- `cargo test -p orgasmic-daemon --lib -- ledger_sync status sync_conflict barrier` →
  **26 passed, 0 failed** (6.16 s), log `/tmp/gates-daemon-review.log`. Reproduces the
  manager's count on merged main.
- Four throwaway-repo probes on git 2.52 (`/tmp/ledgerprobe.2guK`, `/tmp/dmprobe.*`,
  `/tmp/rbprobe.jt2V`), all outside any real checkout: autostash-pop stdout/exit-code,
  conflict stage 2/3 identity, `stash list -1 --format=%gs` subject, delete/modify
  stage-3 absence, detached HEAD during a rebase, and the verbatim recovery command
  sequence for the HIGH 2 loss.

Not checked (say so plainly):
- I did not re-run `cargo test -p orgasmic-cli --bin orgasmic -- daemon_lifecycle`,
  clippy, or fmt — the manager re-ran all four on merged main and the daemon suite I did
  re-run agrees with their count.
- No live-daemon probe. The daemon on :4848 runs the pre-fix runtime, so it could not
  confirm or refute anything here.
- HIGH 1 and HIGH 2 are proved at the git level with the production command sequences,
  not through a Rust test exercising `sync_once` — the review is read-only and both
  would need a new test in the crate. The code path to each is a direct read of
  `:98-104` and `:424-462` respectively.
- I did not audit the `two_daemon_loops_converge_through_the_bare_remote` timing
  behaviour; it passed in the run above.

Test honesty, per test:
- `conflict_reenters_after_failure_between_stash_drop_and_reset` (`:1215`) — hand-crafted
  entry state (the test runs the conflicting pull itself) but the failure is injected
  through a **real production seam**, `park_conflict_inner`'s `before_reset`. It asserts
  on the **bare remote** (`git -C remote show orgasmic:<path>` has no `<<<<<<<`, and the
  parked ref reached the remote). This is the test that matters and it is honest.
- `leftover_foreign_machine_conflict_does_not_wedge_the_next_tick` (`:1295`) —
  hand-crafted state, real `sync_once`; proves no wedge, but under-asserts (LOW 6).
- `foreign_stash_on_top_is_not_dropped` (`:1322`) — real seam (a second worktree pushes a
  genuine stash from inside the park closure), asserts `failed` + the exact error + both
  stash entries still present. Honest.
- `panicking_barrier_does_not_stop_the_writer` (`writer.rs:3612`) — real path, asserts the
  caller gets `Err` and the next barrier still runs.

## Fix Directions

1. Move the rebase-in-progress check ahead of the `symbolic-ref` gate (abort and fall
   through); regression test = a mid-rebase repo must not report `Idle`. **Blocking.**
2. Salvage the worktree into a scratch-index commit before `reset --hard`, or at minimum
   name the discard in the conflict status string. **Blocking as one or the other.**
3. Make absent stage 3 non-matchable for parked-ref candidates.
4. Drop the retained autostash on the re-entry path too, or park-then-drop in one step.
5. Assert `parked_ref` non-empty in the foreign-machine test; add a tracked-modified file
   to the post-conflict assertions.

REJECT
