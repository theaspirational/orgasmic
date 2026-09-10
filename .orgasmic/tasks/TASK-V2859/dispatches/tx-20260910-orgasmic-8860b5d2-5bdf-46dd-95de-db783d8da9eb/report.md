# Review: TASK-V2859 — attachments publish into the ledger under Git LFS

Commit `25ccfd31` vs base `6836bc8d`. Author harness: codex (gpt-5.6-sol, high).

All eight acceptance criteria are met as literally written, and all three gates
are green (evidence in Verification Notes). The design is sound and I proved the
LFS round-trip works end to end. One HIGH remains: the commit ships a mechanism
that depends on a tool the daemon never checks for, and fails silently and
unrecoverably when it is absent.

## Findings

### P1 HIGH (bug / ops) — nothing configures the LFS filter, so blobs silently commit as raw git objects
`crates/orgasmic-daemon/src/ledger_sync.rs:251` writes the `filter=lfs` attribute,
but no code anywhere runs `git lfs install` or verifies `filter.lfs.clean` is set.
`grep -rni lfs` over the tree returns only the constant, its writer, the doc note,
and the test — no filter configuration.

A gitattributes rule naming an unconfigured filter is a silent no-op. Proved:

```
$ git init r && cd r
$ git config filter.lfs.clean ""   # simulate a machine without `git lfs install`
$ printf '*/*/attachments/** filter=lfs diff=lfs merge=lfs -text\n' > .orgasmic/.gitattributes
$ head -c 200000 /dev/urandom > .orgasmic/meetings/M1/attachments/abc
$ git add --all -- .orgasmic && git commit -qm x
$ git check-attr filter -- .orgasmic/meetings/M1/attachments/abc
.orgasmic/meetings/M1/attachments/abc: filter: lfs      <- attribute is applied
$ git cat-file -s $(git rev-parse HEAD:.orgasmic/meetings/M1/attachments/abc)
200000                                                   <- raw bytes, not a pointer
```
With the filter configured the same sequence stores a 131-byte pointer, and a peer
clone gets the real 300000 bytes back (both verified — see Verification Notes).
So the design works; only the prerequisite is unguarded.

Concrete failure, on any machine where git-lfs is not installed (it ships with
neither Xcode CLT git nor Homebrew git) or where `git lfs install` was never run:

1. Operator records a meeting. `FILE_LIMIT` is 8 GiB
   (`crates/orgasmic-daemon/src/node_services.rs:13`), so a 1.5 GB wav is accepted.
2. `finish_upload` publishes it to `.orgasmic/meetings/<id>/attachments/<sha256>`
   (`node_services.rs:858-868`).
3. The next sync tick runs `stage_ledger` then `commit_staged`
   (`ledger_sync.rs:198-199`) and commits **1.5 GB of raw bytes** into the
   `orgasmic` branch. No warning.
4. `git push origin HEAD:orgasmic` (`ledger_sync.rs:225`) is rejected — GitHub
   hard-caps a single non-LFS file at 100 MB, and the ledger remote here is
   `git@github.com:theaspirational/orgasmic.git`.
5. Nothing resets the local commit on push failure; `sync_once_with_park` only
   `bail!`s after `PUSH_ATTEMPTS` (`ledger_sync.rs:230-235`). Every subsequent
   2-second tick re-attempts the same push and fails, backing off to
   `MAX_BACKOFF` (5 min) forever.

Result: **the ledger stops syncing permanently and only a manual history rewrite
recovers it.** This is a new failure class — before this commit no attachment byte
ever entered git.

Blast radius is every registered project, not just the hidden ledger: `spawn`
iterates the whole board every tick (`ledger_sync.rs:976-983`).

Fix direction: in `ensure_attachment_lfs_attribute`, after writing the file, run
`git -C <ledger> lfs install --local` (idempotent; writes `filter.lfs.*` into the
repo's own config, so it does not depend on the user's global setup). If the
`git-lfs` binary is missing the command fails loudly, which is the signal you
want. Minimum viable alternative: refuse to publish and return 503 from
`finish_upload` when `git config --get filter.lfs.clean` is empty on a ledger
with a remote.

### P2 MEDIUM (test) — the `.gitattributes` assertion never exercises the path that matters
`crates/orgasmic-daemon/tests/node_services_routes.rs:373-380` asserts the file
exists after the daemon's sync loop ran. That test project has **no origin
remote**, so `sync_once_with_park` returns at `ledger_sync.rs:106-109`, which has
its own `ensure_attachment_lfs_attribute` call and never reaches `stage_ledger`.

The acceptance criterion is "written *before the first* `git add --all --
.orgasmic`" — that is the call at `ledger_sync.rs:278-280`. Delete those three
lines and the route test still passes. `cargo test -p orgasmic-daemon --lib
ledger_sync` (21 tests, all remote-backed via bare repos) contains zero
`.gitattributes` assertions: `grep -n gitattributes ledger_sync.rs` returns only
lines 255, 259, 264 — all inside the function itself.

So the one criterion that LFS actually depends on is asserted only on the branch
where no commit is ever made.

Fix direction: assert on a remote-backed test — e.g. in
`two_daemon_loops_converge_through_the_bare_remote`, check
`git ls-tree -r origin/orgasmic` contains `.orgasmic/.gitattributes` with the rule.

### P3 MEDIUM (bug) — the quota walk turns concurrent ledger writes into HTTP 500
`crates/orgasmic-daemon/src/node_services.rs:590-614`. The directory-level errors
are tolerated (`let Ok(nodes) = ... else { continue }`), but the per-entry results
are not: `collection.map_err(internal)?`, `node.map_err(internal)?`,
`attachment.metadata().map_err(internal)?`.

The old code walked one flat directory the daemon owned exclusively. This walks a
**live git worktree** that the sync loop rewrites every 2 seconds
(`git pull --rebase --autostash`, `ledger_sync.rs:216`). LFS smudge on checkout
unlinks and recreates an attachments file; if that lands between `read_dir` and
`metadata()`, an unrelated `POST /api/attachments/uploads` returns 500 instead of
starting an upload. Intermittent, and it will look like a daemon fault.

Fix direction: skip on error rather than propagate — `entry.ok()` /
`metadata().ok()` — the quota is an approximation already (see the `ponytail:`
comment at `node_services.rs:589`).

### P4 LOW (perf) — boot migration re-walks the whole ledger on every start
`crates/orgasmic-daemon/src/lib.rs:971-1000`. `legacy` is computed but never
checked for existence, so the walk proceeds unconditionally: every collection,
every node dir, one `read_to_string` of `attachments.org` each. There is no drain
condition (the legacy blobs are deliberately never deleted), so this runs at full
cost forever, synchronously on the boot critical path before the listener binds.

Measured against the real ledger `~/.orgasmic/ledgers/orgasmic/.orgasmic`:
1148 depth-2 node directories, **0** `attachments.org` files. So ~1150 wasted
opens per boot today. Milliseconds now; grows linearly with the ledger.

Fix direction: `if !legacy.is_dir() { continue; }` at the top of the project loop.

### P5 LOW (bug) — EXDEV temp files leak invisibly
`crates/orgasmic-daemon/src/lib.rs:952-966`. The `<sha256>.tmp.<uuid>` copy is
removed on both success and error, but a daemon SIGKILL mid-`io::copy` leaves it
in `<node>/attachments/` permanently. It is correctly excluded from staging
(`:(exclude,glob).orgasmic/**/*.tmp.*`, `ledger_sync.rs:288`) and skipped by the
quota scan (`valid_digest`), which is exactly why it is invisible: it consumes
disk and is never counted against the 64 GiB project quota.

Fix direction: sweep `attachments/*.tmp.*` older than an hour during the boot
migration you already walk.

### P6 LOW (design) — the LFS rule is written into projects the sync loop calls foreign
`ledger_sync.rs:106-109` fires on the **no-remote** branch, and `spawn`
(`ledger_sync.rs:976-983`) runs over every registered project every 2 seconds. So a
plain local project with no origin now gets a new `.orgasmic/.gitattributes` in its
working tree.

That is the branch the code takes pains not to touch: the dec_AF61D/dec_XH2XY
comment at `ledger_sync.rs:135-138` ("Foreign repos are never touched this way")
guards cleanup that is deliberately placed *after* the remote check. Cosmetic on
its own; combined with P1 it means a hand-managed repo can inherit an LFS rule it
cannot honour.

### P7 LOW (docs) — the operator note lands under the wrong heading and omits the local prerequisite
`shipped/skills/orgasmic/operations/artifacts.md:71-75`. The paragraph is appended
directly after the `## Example` code fence with no heading of its own, so it reads
as commentary on `orgasmic member list --help`.

It also says "the ledger remote must have LFS enabled" but never states the
prerequisite P1 hinges on: git-lfs must be installed on the machine and
`git lfs install` must have been run. Add its own `## Attachments` heading and that
sentence.

### P8 LOW (hygiene) — commit message
`25ccfd31` is `TASK-V2859: Changed`. The surrounding history is imperative,
descriptive, and unprefixed ("Let the meetings plugin create a meeting and import
notes", "Gate a stable publish on the certification profile it was certified
with"). Suggest: "Publish attachment blobs into the ledger under Git LFS".

## Open Questions

1. Is any target machine expected to run without git-lfs? If the answer is "no,
   it is an install prerequisite", P1 drops to a documentation fix plus a boot
   assertion. If "maybe", P1 needs the `lfs install --local` line before this
   reaches a second machine.
2. GitHub LFS on the free tier is 1 GB storage / 1 GB bandwidth per month, while
   `FILE_LIMIT` is 8 GiB per file and the project quota is 64 GiB. Is that
   mismatch understood and accepted, or does the quota want lowering?

## Verification Notes

Gates, run in this worktree with its own `--target-dir target` (no daemon started,
no `--workspace`):

- `cargo fmt --check` → clean, rc=0.
- `cargo clippy -p orgasmic-daemon --all-targets --target-dir target -- -D warnings`
  → `Finished`, 0 warnings, 0 errors (`/tmp/v2859-logs/clippy.log`, PID 80940).
- TEST_CMD `cargo test -p orgasmic-daemon --test node_services_routes` →
  `1 passed; 0 failed; 1 ignored` (`/tmp/v2859-logs/routes.log`, PID 73288). The
  ignored test is the pre-existing `#[ignore]`d 2 GiB gate
  (`node_services_routes.rs:692`); the new assertions all live in the
  `adversarial` branch, which **does** run — `node_services_authorize_stream_and_index`
  calls `exercise(8 MiB, true)` at `node_services_routes.rs:686-689`. Confirmed
  the new assertions executed.
- `cargo test -p orgasmic-daemon --lib ledger_sync` →
  `21 passed; 0 failed` (`/tmp/v2859-logs/ledger.log`, PID 85068). No regression.

Acceptance criteria checked individually against the code, not the claim:

1. Node-folder publication, atomic, no overwrite — `node_services.rs:858-868`
   resolves `owner.path.parent()`. Confirmed `node()` returns the **node.org**
   path (`node_services.rs:100`, via `org_node_path`), so `.parent()` is the node
   directory. `publish_blob` (`lib.rs:943-967`) hard-links and treats
   `AlreadyExists` as success. Staging still under `store_root` (`node_services.rs:488`). ✅
2. Reads from the node folder, 404 message fixed — `node_services.rs:977-983`
   and `:997`. ✅
3. `.gitattributes` idempotent, one rule, before `git add` — `ledger_sync.rs:251-275`
   and `:278-280`. Correct by inspection; see P2 for the test gap. ✅ (code) / ⚠️ (test)
4. Boot migration — `lib.rs:971-1000`, called at `lib.rs:1182`, after the project
   snapshot and before the writer and sync loop spawn. Idempotent (hard_link
   `AlreadyExists` → `Ok(false)`), per-file failures only `warn!`, legacy files
   left in place, unreferenced blobs untouched (it iterates records, not the dir). ✅
5. Quota counts node folders — `node_services.rs:590-614`; scope matches the old
   per-project semantics (`owner.ledger` = `<project>/.orgasmic`). ✅ (see P3)
6. Route tests prove the five claims — verified each:
   - blob under the node folder: `node_services_routes.rs:366-367` ✅
   - served from there: `:256-300` (200/206/HEAD) ✅
   - duplicate revision succeeds: `:381-411`, asserts the second finish returns
     the same `revision` ✅
   - exactly one gitattributes rule: `:373-380` ✅ but see P2
   - migrated legacy blob **served**: `:523-532` moves the node blob to
     `store/blobs/<rev>`, restarts the daemon on a renamed project root, then
     `:557-558` asserts the migrated file, and `:609-618` does a ranged GET that
     returns 206 — so the migrated blob really is served, not just stat'd. ✅
7. Gates green ✅ (above).
8. Operator doc note ✅ (see P7 for placement).

Cutover completeness: `grep -rn blobs crates/ ui/src` returns only the migration
(`lib.rs:971,978,1182`) and the test (`node_services_routes.rs:530`). No reader of
the old `store_root/blobs` path was left behind.

Positive LFS proof (that the design works when the prerequisite is met), run on
scratch repos in `/tmp`, never against the real ledger:
- `.gitattributes` written and staged in the *same* `git add --all` invocation
  still applies — git reads working-tree attributes, so the ordering at
  `ledger_sync.rs:278-280` is sufficient. Verified: 200000-byte file → 131-byte
  pointer.
- Pattern `*/*/attachments/**` relative to `.orgasmic/.gitattributes` matches
  `.orgasmic/<collection>/<node>/attachments/<sha>` and nothing shallower.
  Verified via `git check-attr`.
- Full round trip: bare remote, `git push origin HEAD:orgasmic` uploaded the LFS
  object ("Uploading LFS objects: 100% (1/1)") **without** a `pre-push` hook
  present, and a plain `git clone -b orgasmic` on a peer restored the real 300000
  bytes. I had hypothesised the missing hook would break uploads — I tested it
  and it does not. git-lfs 3.7.1.
- Singleton nodes cannot produce a depth-1 `attachments/` dir that the pattern
  would miss: `start_upload` requires `write=true`, which rejects singletons at
  `node_services.rs:112-115`.

Not verified: behaviour with real legacy blobs. The live ledger has 0
`attachments.org` files, so there is nothing to migrate in production today; the
migration path is covered only by the route test's synthetic move.

Environment: git-lfs 3.7.1 is installed and `filter.lfs.clean` is set globally on
this machine, and the live ledger has an origin remote — so P1 does not bite here
today. It bites on the next machine.

## Fix Directions

1. **P1** — one line in `ensure_attachment_lfs_attribute`: after writing the file,
   `git -C <ledger> lfs install --local`. Fails loudly if git-lfs is absent, which
   is the correct outcome.
2. **P2** — assert `.orgasmic/.gitattributes` in the pushed tree from an existing
   remote-backed `ledger_sync` test.
3. **P3** — `entry.ok()` / `metadata().ok()` in the quota walk instead of `?`.
4. **P4** — `if !legacy.is_dir() { continue; }` in the migration loop.
5. **P5/P6/P7/P8** — as described; all cosmetic or one-liners.

VERDICT: approve-with-follow-ups
