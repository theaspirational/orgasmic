# Review: TASK-MSYN4.2 — sync sidecar excludes (H2) + per-ledger sync status with backoff (H5)

Reviewed `git diff d75dee5a^1 d75dee5a` (5 files, +295/-7) plus `sync_once_inner`,
`transaction_multi_locked_inner` (`writer.rs:3430-3560`), `views.rs:117`, `cmd_status`,
`doctor.rs`, and `ui/src/lib/types.ts`. Read-only throughout; no daemon started, nothing
mutated in `~/.orgasmic/ledgers/orgasmic`.

## Verdict

**APPROVE WITH FOLLOW-UPS.** Both acceptance criteria are met. The excludes are load-bearing
and correct at every depth; the status surface reaches `/status`, `orgasmic status`, and
`orgasmic daemon status`; both new tests genuinely exercise what they claim. Two MEDIUM
follow-ups: one latent dirty-tree regression, one ceiling comment that its own sibling change
falsified.

## Findings

### MEDIUM — `crates/orgasmic-daemon/src/ledger_sync.rs:129-131` — a tracked sidecar can never be un-tracked

`git add --all -- .orgasmic :(exclude,glob)…*.bak.*` does not stage the **deletion** of a file
matching an exclude. If a sidecar is ever in `HEAD`, its removal from the worktree is a
permanent unstaged change: `git status` never clears, and every 2 s tick's
`pull --rebase --autostash` stashes and pops it forever. That is precisely the
"permanent uncommitted changes for `--autostash` to churn on every tick" class the comment at
`ledger_sync.rs:100-108` says was fixed.

Reproduced (plain git 2.52, `/tmp/pstest`, no daemon):

```
$ git add -f .orgasmic/a/stale.bak.old && git commit -m "old runtime committed a sidecar"
$ rm .orgasmic/a/stale.bak.old
$ git add --all -- .orgasmic ':(exclude).orgasmic/machines' \
    ':(exclude,glob).orgasmic/**/*.tmp' ':(exclude,glob).orgasmic/**/*.tmp.*' \
    ':(exclude,glob).orgasmic/**/*.bak.*'
$ git diff --cached --name-status      # empty
$ git status --porcelain
 D .orgasmic/a/stale.bak.old           # survives every future tick
```

Not currently triggered: `git ls-files | grep -E '\.bak\.|\.tmp'` on the live ledger returns
nothing (the two commits named in the assignment, `cd544977` and `8f937138`, have since been
cleaned). Reachable two ways: (a) a mixed-version fleet where a pre-fix peer (gigabyte) commits
a sidecar that this machine then pulls — the file becomes tracked garbage no fixed machine can
ever remove; (b) all machines upgrade while one of their own sidecars sits in `HEAD` — the
writer deletes the file, the deletion is never stageable, and that ledger's tree is dirty
forever.

**Fix direction:** one-shot reconciliation before the add —
`git rm --cached --ignore-unmatch -- :(glob).orgasmic/**/*.bak.* …` (mirroring the existing
`.orgasmic/views` `rm --cached`), or drop the exclude for paths that are already tracked.

### MEDIUM — `crates/orgasmic-daemon/src/ledger_sync.rs:117-120` — the ceiling comment is falsified by its own sibling change, and names only one of the two torn orders

The comment claims a peer "can see that torn state for one sync interval". The backoff added in
the same commit removes that bound: a torn commit is already **pushed** when the next tick
fails, and `next_attempt_at` then holds the completing commit for up to `MAX_BACKOFF` (5 min).
A wedged ledger holds it indefinitely. The interval bound is only true on the success path.

It also names one direction. Walking `transaction_multi_locked_inner` (`writer.rs:3480-3500`:
rename loop → `append_txs_inner`) against `sync_once_inner`'s two adds (`:126` nodes,
`:141` `machines/<id>`), both orders are reachable:

- add#1 after rename, add#2 before append → node rewrite, no close tx. *(the comment's case)*
- add#1 before rename, add#2 after append → **close tx with the node rewrite missing.**

The second is the worse one: a peer sees a `dispatch.close`/state-transition tx while `node.org`
still reads the old state, which is what the Done evidence gate reads.

**Fix direction:** comment-only. Say "until the next successful sync (backoff can extend this to
`MAX_BACKOFF`)" and name both orders.

### LOW — `crates/orgasmic-daemon/src/ledger_sync.rs:230-238` — `idle` claims a success that never happened

`sync_once` returns `Idle` **only** when the path is not a synced ledger (wrong branch or no
`origin`); a healthy ledger with nothing to push returns `Synced { push_retries: 0 }`. So the
outcome is not conflated — but the success branch still writes `last_success_at: Some(now)`,
refreshed every 2 s, for a plain project checkout that has never synced anything. `orgasmic
status` prints the raw JSON, so a user debugging "why isn't my ledger syncing" sees a
freshly-timestamped success. Leave `last_success_at: None` on `SyncOutcome::Idle`.

### LOW — `crates/orgasmic-daemon/src/ledger_sync.rs:289-295` — the status map is never pruned

Keyed by board path, entries are only ever inserted. A project removed from the board keeps its
last status in `/status` for the daemon's lifetime. Bounded by paths-ever-seen, so not a leak
that matters — but it is stale state on a diagnostic surface. One `retain(|k, _| ledgers.contains(k))`
after the `BTreeSet` is built.

### LOW — `crates/orgasmic-cli/src/doctor.rs:334` — `doctor` does not read the new field

`doctor` fetches `/daemon/status` and has `check_daemon_for_status_with_status`, but ignores
`ledger_sync`. The health verb still reports a healthy daemon while a ledger is wedged. Natural
follow-up alongside dec_EWY0K.

## What I checked and found clean

- **Pathspec depth.** Empirically, on git 2.52: `:(exclude,glob).orgasmic/**/*.tmp` excludes at
  depth 1 (`.orgasmic/top.tmp`) and depth 4 (`.orgasmic/a/b/c/deep.tmp`). `*.tmp.*` covers
  `.tmp.req-rollback-x`. `glob` magic on some elements and not others is accepted. All legitimate
  files staged.
- **Sidecar shape coverage is complete.** Four shapes exist: `<name>.tmp`
  (`writer.rs:3202`, `:3234`), `<name>.tmp.<req>` and `<name>.bak.<req>`
  (`transaction_sidecar_path`, `writer.rs:3540-3556`), and `<name>.<pid>.<n>.tmp`
  (`views.rs:125-129`, and `views/` is gitignored anyway). No `.new` / `.lock` / `.part` /
  `.swp` writers exist under `.orgasmic`. All four match the three excludes.
- **No collateral damage.** `.orgasmic` filenames are all machine-generated (`node.org`,
  `journal.org`, `artifact.mdx`, `versions/vN.mdx`, `tx/YYYY-MM.org`); nothing in `paths.rs`
  derives a ledger filename from user input. `git ls-files | grep -Ei '\.tmp$|\.tmp\.|\.bak\.'`
  over the live ledger's 2482 tracked files: zero hits. `.orgasmic/tmp/` is gitignored, so the
  excludes change nothing there.
- **The concurrency attack does not land.** `ledgers` is a `BTreeSet` (deduped), the `for` loop
  `await`s each `spawn_blocking`, and the outer loop `await`s the tick — two ticks for the same
  ledger cannot overlap, so the "clone previous outside the lock, insert fresh later" pattern has
  no competing writer.
- **Backoff arithmetic.** `1_u32 << n.min(8)` maxes at 256; `Duration::saturating_mul(256)` =
  512 s, capped to 300 s. No overflow path. Reaching the cap takes 2+4+…+256 ≈ 8.5 min of
  continuous failure, so a 30 s network blip costs ≤ 32 s of extra staleness.
- **`last_success_at` survives backoff.** The skip branch mutates `status.outcome` in place
  (`:215`) rather than inserting; `error`, `consecutive_failures`, `last_success_at`, and
  `next_attempt_at` are all preserved.
- **Both tests are honest.** `writer_sidecars_are_never_staged` asserts the machine tx file **is**
  tracked and then asserts globally that no tracked path matches any sidecar shape — that covers
  the machine-dir add, not just the node add. `failed_pull_is_reported_and_backed_off`'s reflog
  assertion is a real proof: I confirmed independently that a repeated
  `pull --rebase --autostash` + `rebase --abort` against a conflicting remote **does** change
  `git reflog` (two runs → two different digests), so an un-skipped second tick would fail the
  assertion.
- **No contract drift.** `ui/src/lib/types.ts:305` and `StatusView.tsx` read only optional
  fields; an added `ledger_sync` key cannot break them. `cmd_status` (`main.rs:2556`) prints the
  raw `/daemon/status` JSON, so `orgasmic status` picks the field up for free — the acceptance
  criterion is met on both status verbs.

## Failure classification (answering the brief)

No failure class needs a *different* backoff today. The one worth splitting later is
**remote-unreachable** (`ls-remote`/fetch failure) versus **rebase conflict**: the first is
self-healing and deserves a flat cheap retry, the second is operator-blocking and should
stop-and-flag rather than keep re-attempting at 5 min. Push-after-successful-rebase does not
need special handling — `PUSH_ATTEMPTS = 5` already absorbs the push race, and reaching the
`bail!` means something structural. Not filed as a finding: the current cost is bounded and
dec_EWY0K's conflict path is the right place for the split.

## Fight with dec_EWY0K?

Nothing structural. `LedgerSyncStatus.outcome` is a `&'static str`, so TASK-8DWJP can add a
`"conflict"` variant without touching the wire type, and `next_attempt_at: None` already encodes
"do not retry". Two things 8DWJP should inherit rather than rebuild: the map needs pruning
(finding 4) and `doctor` needs to read it (finding 5).

## Open Questions

1. Is the fleet ever mixed-version in practice? If gigabyte is always upgraded in lockstep,
   finding 1 stays latent and can wait; if not, the reconciling `rm --cached` should land before
   the next runtime publish.
2. Should a wedged ledger be clearable without a daemon restart? The status map is in-memory
   only, so `daemon restart` resets the backoff — that is the current (undocumented) escape hatch.

## Verification Notes

- Diff, `sync_once_inner`, `sync_ledger_at`, `spawn`, `transaction_multi_locked_inner`,
  `transaction_sidecar_path`, `views::write_if_changed`, `cmd_status`, `cmd_daemon_status`,
  `doctor.rs`, `ui/src/lib/types.ts` read directly.
- Two throwaway git repos under `/tmp` (`pstest`, `rtest`) for the pathspec-depth, tracked-sidecar-
  deletion, and reflog probes. Nothing under `~/.orgasmic` was written; the only live-ledger
  commands were `git ls-files`, `git status --porcelain`, and `cat .orgasmic/.gitignore`.
- **Not checked, deliberately:** I did not re-run `cargo test`, `clippy`, or `fmt` — the brief
  records them green on the merged tree and instructs not to re-spend. Residual risk: I am
  trusting that record for compile-and-pass; every behavioral claim above is instead proven by a
  direct git probe or by reading the code.
- **Not checked:** the live daemon on :4848 (pre-fix runtime, per the brief), and any
  multi-machine behavior against gigabyte — finding 1's mixed-version path is reasoned from the
  code, not observed on a real second machine.

## Fix Directions (ranked)

1. `git rm --cached --ignore-unmatch` the three sidecar globs once per tick, right beside the
   existing `.orgasmic/views` call, so an already-tracked sidecar can leave the index.
2. Correct the `ponytail:` comment: the window is "until the next successful sync, which backoff
   can stretch to `MAX_BACKOFF`", and both torn orders exist.
3. `last_success_at: None` when `SyncOutcome::Idle`.
4. `retain` the status map against the live ledger set each tick; teach `doctor` to read it.

APPROVE WITH FOLLOW-UPS
