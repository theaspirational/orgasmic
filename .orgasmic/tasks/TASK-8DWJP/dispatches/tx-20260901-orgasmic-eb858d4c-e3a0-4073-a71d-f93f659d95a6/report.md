# Review: TASK-8DWJP — ledger sync conflict path (dec_EWY0K)

Reviewed `200892f2` (implementer commit `fa8ef1f9`), 7 files, +370/-63.

## Verdict

**REJECT.** Two HIGH findings. One is a routing bug that makes the new
`ledger.sync_conflict` event unreadable by every consumer in the tree and flips
its tx-id policy to the one machine-routed events deliberately avoid — the
acceptance criterion "records the event" is not met on the real path. The other
is a conflict shape the new detector misses while reporting `synced`, after
which the next tick commits and pushes conflict markers into the shared ledger.

The rest of the design is sound: the salvage → park → follow-remote sequence is
correct, `rebase --abort` restores the autostash cleanly, the `reset --hard`
target cannot go stale, and the parked-ref naming is collision-safe.

---

## Findings

### HIGH 1 — the conflict event lands one directory too high; nothing can read it
`crates/orgasmic-daemon/src/ledger_sync.rs:405-407`

```rust
tx_path: ledger
    .join(".orgasmic/machines")
    .join(machine_id)
    .join(format!("{}.org", now.format("%Y-%m"))),
```

The `tx/` segment is missing. Every reader in the tree requires it:

- `index.rs:948` — `Some("machines") => ((parts.len() == 5 && parts.get(3) == Some(&"tx")) || …)`.
  A 4-component `machines/<id>/2026-09.org` yields `tx_file = None`, falls to
  `refresh_project`, which re-derives dirs from…
- `index.rs:3790` `project_tx_dirs` — `machines.map(|m| m.path().join("tx")).filter(is_dir)`.
- `api.rs:3801` — same shape.
- `writer.rs:3035` (`scan_project_tx_max_seq`) — same shape.

So the file is written, staged (`stage_ledger`'s second `git add` covers
`machines/<self>` wholesale) and pushed, but it is **never indexed**: it does not
appear in `orgasmic tx list`, the API tx feed, or any view. The one durable
operator-facing record of a parked write is invisible.

Second consequence, worse. `writer.rs:2884` `is_machine_tx_path` requires
`parts[0] == "machines" && parts[2] == "tx"` — false here. `prepare_tx_entry`
(`writer.rs:2837-2846`) therefore takes the **project-sequence** branch:

```rust
entry.tx_id = if is_machine_tx_path(&req.tx_path) {
    format!("tx-{date}-{}-{}", project_tx_slug(project_id), Uuid::new_v4())
} else {
    next_project_tx_id(seq_cache, project_id, &project_tx_dir(&req.tx_path)?, date)?
};
```

Machine-routed events get UUID ids precisely because machines cannot coordinate
a counter. A conflict is by definition a two-machine event, and
`scan_project_tx_max_seq` cannot see the misplaced files, so two machines
conflicting in the same month mint the **same** `tx-<date>-orgasmic-NNNN`.

It also contradicts both docs shipped in this same commit:
`shipped/schema/tx.org:75` ("machine-routed `tx/YYYY-MM.org`") and
`shipped/skills/orgasmic/references/ledger.md:27-28`
("`machines/<machine-id>/tx/`").

**Fix:** `.join(machine_id).join("tx").join(format!("{}.org", …))`.
`ledger_sync.rs:585-588` (the test) needs the same fix — see LOW 6.

---

### HIGH 2 — an autostash-pop conflict reports `synced`, then the next tick pushes conflict markers
`crates/orgasmic-daemon/src/ledger_sync.rs:131-141`

The detector only fires when `!pull.status.success()`. There is a conflict shape
where `git pull --rebase --autostash` **exits 0 and prints no `CONFLICT (` line
at all**: the rebase fast-forwards, then the autostash pop conflicts.

Probe, git 2.52.0, throwaway repos in `/tmp/pk8` (a has an uncommitted change to
a tracked file the remote also changed; a has no local commit to rebase):

```
$ LC_ALL=C git pull --rebase --autostash origin orgasmic ; echo exit=$?
exit=0
--- STDOUT ---
Updating 8eb912f..716e104
Created autostash: d80ab76
Fast-forward
 n/g.org | 2 +-
--- STDERR ---
Applying autostash resulted in conflicts.
Your changes are safe in the stash.
--- git status --short ---
UU n/g.org
--- git rebase --abort ---
fatal: no rebase in progress
```

`pull.status.success()` is true, so `sync_once_inner` skips the whole conflict
branch, runs `git push origin HEAD:orgasmic`, and returns
`SyncOutcome::Synced` — `daemon status` says the ledger is healthy while
`n/g.org` on disk contains `<<<<<<< Updated upstream`.

The next tick then destroys it. Continuing in the same repo:

```
$ git add --all -- .                       # stage_ledger
$ git commit -m "ledger: sync machine-a"   # commit_staged  -> COMMIT SUCCEEDED
$ git show HEAD:n/g.org
<<<<<<< Updated upstream
remote-side
=======
local-dirty
>>>>>>> Stashed changes
$ git push origin HEAD:orgasmic
$ git -C remote.git show orgasmic:n/g.org
<<<<<<< Updated upstream
…
```

Conflict markers are committed and pushed into the shared ledger, and every
other machine then pulls the corrupted org file. `git add` on a `UU` path marks
it resolved with the marker text, and no rebase is in progress, so nothing
blocks the commit.

Reachability: the pull only sees dirty tracked files if something is dirty
*after* `stage_ledger`+`commit_staged`. Two live sources — (a) the unfenced
window in MEDIUM 4, where the writer lands a tracked-file rewrite between the
commit and the pull; (b) paths `stage_ledger` deliberately excludes
(`:(exclude,glob).orgasmic/**/*.tmp`, `*.tmp.*`, `*.bak.*`) that are nonetheless
tracked — the sidecars TASK-MSYN4.2.1 is untracking, which today are the
"permanent uncommitted changes for `--autostash` to churn on every tick" that
`ledger_sync.rs:112-118` describes in its own comment.

This hole predates the diff (the `pull --rebase --autostash` line is moved, not
changed), so it is not a regression — but it is exactly the detection gap this
task existed to close, and the new conflict path walks straight past it.

**Fix direction:** do not trust the exit code. After the pull, check
`git ls-files -u` (or `git diff --name-only --diff-filter=U`) non-empty, or match
`Applying autostash resulted in conflicts` on stderr; take the salvage path and
recover the stash from `git stash list` rather than `rebase --abort`. A regression
test is cheap — the /tmp/pk8 setup is four commands.

---

### MEDIUM 3 — `conflict_paths` mis-parses every conflict shape but "Merge conflict in"
`crates/orgasmic-daemon/src/ledger_sync.rs:230-249`

```rust
if let Some((_, path)) = line.rsplit_once(" in ") {
```

That fits `CONFLICT (content): Merge conflict in <path>`. It does not fit the
other shapes. Measured, same probe repos:

```
CONFLICT (modify/delete): n/f.org deleted in c24ae70 (a deletes f) and modified in HEAD.  Version HEAD of n/f.org left in tree.
```

`rsplit_once(" in ")` returns **`tree.`**. Same class for `rename/delete`
(`… renamed to … in <commit> but deleted in <branch>.`).

Recovery still runs (the list is non-empty, so the conflict branch is taken and
the parked ref is correct), but `PATHS` in the event, the `daemon status` line,
and the operator's only pointer at *what* conflicted are garbage. For a feature
whose entire product is "tell the operator what got parked", that is the payload.

Reachable here: a node file one machine deletes (task/dispatch cleanup, the
`views/` untracking) while another modifies it is a modify/delete.

**Fix direction:** after the failed pull, read the conflicted set from
`git diff --name-only --diff-filter=U` (before `rebase --abort`) instead of
scraping prose. That also fixes any future shape for free.

---

### MEDIUM 4 — the write-loss window is still unfenced, and the barrier is now cheap
`crates/orgasmic-daemon/src/ledger_sync.rs:252-284`

`park_conflict` runs on `spawn_blocking` while the writer keeps serving. Between
`commit_staged` (salvage) and `reset --hard origin/orgasmic`, a writer rewrite of
a tracked node file is **discarded with no record** — not in the parked ref (it
landed after the salvage commit), not in the event. An untracked new file
survives, which is right; a modified tracked tx/node file does not.

The brief asks whether the now-threaded `WriterHandle` can fence this in ≲15
lines. It can. The writer is a single-threaded actor draining
`mpsc::Receiver<WriterCommand>` (`writer.rs:343`, handle at `writer.rs:535`), so a
new `WriterCommand::Barrier { run: Box<dyn FnOnce() + Send>, reply }` variant plus
one match arm plus one `WriterHandle::run_barrier` method makes the whole
`park_conflict` run with every other write queued behind it. Three small edits,
no new locking discipline. The existing `LeaseSessions`/`ReleaseSessions` pair
(`writer.rs:378-383`) is the same shape and shows the deferral works.

Not a HIGH on its own — the window is milliseconds and the dropped write is
recoverable from the node's journal — but it is the direct cause of HIGH 2's
reachability, and it is no longer a residual worth accepting.

---

### LOW 5 — a parked conflict's only durable pointer is the ref name
`crates/orgasmic-daemon/src/ledger_sync.rs:272-284`

`LedgerSyncStatus` is in-memory only, and `record_sync_conflict` runs *after*
`park_conflict` returns. A daemon crash in between leaves the salvage commit
reachable by nothing but a `tracing::warn!` line. Separately,
`refs/orgasmic/conflicts/<machine>/<ts>` is never pruned and never listed by any
verb; since the ledger is a git worktree sharing the source checkout's ref store,
those commits are permanently un-GC-able. No expiry, no `daemon status` roll-up.

### LOW 6 — the two-writer test cannot catch HIGH 1
`crates/orgasmic-daemon/src/ledger_sync.rs:768-790`

The test is good where it counts — it asserts the parked *tree* holds `a`'s
bytes (`git show {parked_ref}:{relative}` == `"a"`), that the working file holds
`b\n` after the reset, that `HEAD == origin/orgasmic == remote_head`, that
`parked_ref == local_head`, and that the second tick's write reaches the **bare
remote** (`git -C remote show orgasmic:{fresh}`). The non-conflict test proves the
paths stay distinct (`failed`, backoff set, error does not contain `parked at`).

But it calls `record_sync_conflict` directly with a literal `"project-a"`, never
through `spawn`'s board loop, and it re-derives the very same `tx_path`
expression it is asserting against:

```rust
let tx_path = a.join(".orgasmic/machines").join(&machine_id)
    .join(format!("{}.org", now.format("%Y-%m")));
```

So it covers neither the routing nor the `project_id` resolution, which is why a
misrouted event passes green. Asserting the event is visible through the index
(or at minimum hard-coding `machines/<id>/tx/<month>.org`) would have caught it.

---

## What I checked and found clean

- **`reset --hard` target.** `git()` (`ledger_sync.rs:504`) bails on a non-zero
  exit, so a failed `fetch origin orgasmic` can never fall through to a stale
  `origin/orgasmic`. The live ledger has the refspec:
  `git -C ~/.orgasmic/ledgers/orgasmic config --get-all remote.origin.fetch` →
  `+refs/heads/*:refs/remotes/origin/*`, `symbolic-ref --short HEAD` → `orgasmic`.
  Leaving untracked files alone (no `clean`) is right: untracked = writes after
  salvage, keep them.
- **`rebase --abort` re-applies the autostash.** Measured in `/tmp/pk9`: dirty
  file restored to `DIRTY-UNCOMMITTED`, `git stash list` empty, "Applied
  autostash." So the pre-pull dirty state *is* inside the salvage commit. I also
  forced the autostash to overlap the conflicting file (`/tmp/pk10`): abort still
  exits 0 and applies cleanly, because abort restores the original HEAD first.
- **Parked-ref collision safety.** The `show-ref --verify --quiet` + `-{suffix}`
  loop (`ledger_sync.rs:257-268`) covers two conflicts in the same second, and
  the ref is machine-scoped so two machines cannot collide. `update-ref` never
  sees an existing name.
- **`LC_ALL=C` on `git_output`** (`ledger_sync.rs:516`) — correct and necessary,
  since `conflict_paths` scrapes English prose. Concatenating stderr+stdout is
  also right: the `CONFLICT (` lines are on **stdout**, the rebase progress on
  stderr (measured).
- **In-memory state after the reset.** `watcher.rs` handles
  `EventKind::Create|Modify|Remove` (`watcher.rs:282-286`) with a 200 ms debounce
  (`watcher.rs:36`), so the files `reset --hard` rewrites drive
  `Index::reload_*` (`index.rs:905-975`) and converge. Cached tx append handles
  are protected: `tx_handles_detached_from_paths` (`writer.rs:2830-2850`) compares
  a `FileIdentity` (inode/len/mtime) before each append and reopens on mismatch,
  so a reset-rewritten tx file does not get appended at a stale offset.
- **Hot-loop risk.** After the reset local == remote, and the only new content is
  the event file under `machines/<self>/`, which no other machine writes — so the
  next tick commits, pushes and reports `synced` (the test proves exactly this).
  `consecutive_failures = 0` on conflict does not create a hot loop; a
  permanently conflicting remote parks once and then follows the remote.
- **Premise rewrites are complete.**
  `rg -n "claim gate|pen\b" crates/orgasmic-daemon/src/writer.rs shipped/skills/orgasmic/references/ledger.md crates/orgasmic-daemon/src/ledger_sync.rs`
  returns no stale cross-machine-barrier language. `writer.rs:1752-1754` and
  `ledger_sync.rs:121-122` both now say claims are per-dispatch, and
  `ledger.md:38-40` states the conflict-park rule. This satisfies the
  "ledger_sync.rs:52 comment matches reality" acceptance line.
- **Status surface.** `rg -rn 'ledger_sync' ui/` → no hits, so the new `conflict`
  outcome cannot break a UI contract; the `/status` JSON is additive (a new
  `outcome` string value, no field changes) and `daemon_lifecycle.rs` decodes it.
  `main.rs:2791-2799` prints one line and falls back when `error` is `None`.
- **Schema string match.** `shipped/schema/tx.org:75` `ledger.sync_conflict` ==
  `ledger_sync.rs:381`. The *extras* match too (`PARKED_REF PATHS LOCAL_HEAD
  REMOTE_HEAD`, plus `:PROJECT:` via `entry.project`). Only the *directory* in the
  doc disagrees with the code — that is HIGH 1, not a separate LOW.
- **Targeted tests re-run.** `cargo test -p orgasmic-daemon --lib -- ledger_sync`
  → 9 passed, 0 failed, 4.62s. Green, including both new tests.

## Open Questions

1. HIGH 2 is pre-existing (MSYN4.2). Fix it under this task, or split it out?
   It is the same code path and the same 5-line detection change, so folding it
   in is cheaper — but it is genuinely a second defect.
2. Does the sidecar-untracking half of TASK-MSYN4.2.1 remove source (b) of
   HIGH 2's reachability? If so, only source (a) (the MEDIUM 4 window) remains,
   which lowers HIGH 2's *frequency* but not its severity.
3. `refs/orgasmic/conflicts/` has no reconciliation verb. Is manual
   `git show <ref>:<path>` the intended operator workflow, or is a
   `orgasmic ledger conflicts` verb planned?

## What I did NOT check

- Did not run a daemon, did not touch `~/.orgasmic/ledgers/orgasmic` beyond the
  two read-only `git config` / `symbolic-ref` reads quoted above. The live daemon
  on :4848 runs the pre-fix runtime; its `/status` was not consulted.
- Did not run `clippy`/`fmt` or the CLI suite — the brief records the implementer
  and manager each ran all four gates on `200892f2`.
- Did not exercise `record_sync_conflict` through the real `spawn` loop (that
  needs a live board), so HIGH 1's index-invisibility is established from the
  three reader call sites, not from a running daemon. The tx-id-policy half of
  HIGH 1 is established from `writer.rs:2837-2846` by reading, not by execution.
- Did not test add/add-on-binary or file/directory conflict shapes; MEDIUM 3's
  fix direction (`--diff-filter=U`) makes them moot, so I stopped at two proven
  mis-parses.
- Did not re-file TASK-MSYN4.2.1's tracked-sidecar / ceiling-comment / status
  hygiene items (out of scope), though the sidecar issue is cited above only as
  reachability evidence for HIGH 2.

## Fix Directions (ordered)

1. `ledger_sync.rs:405` — insert `.join("tx")`. Fix the mirrored expression in the
   test at `:585`, and assert the event through the index rather than by
   re-deriving the path.
2. `ledger_sync.rs:131` — stop trusting the pull exit code. After the pull, treat
   a non-empty `git diff --name-only --diff-filter=U` as a conflict regardless of
   status; recover the retained autostash from `git stash list` when there is no
   rebase to abort. Add the `/tmp/pk8` scenario as a test.
3. `ledger_sync.rs:239` — replace the `" in "` scrape with
   `git diff --name-only --diff-filter=U`, read before `rebase --abort`.
4. `writer.rs:343` — add `WriterCommand::Barrier` and run `park_conflict` inside
   it, closing the salvage→reset window.
5. Give parked refs a durable index (append the event *before* the reset, or
   surface `refs/orgasmic/conflicts/` in `daemon status`) and an expiry.

**REJECT.**
