# Review TASK-1PFW6.1 — marketplace registry follow-ups

**Verdict: approve-with-follow-ups.**

All seven assigned follow-ups are implemented and the full assigned gate suite is
green. Nothing here is a ship blocker. Two MEDIUM items are worth a follow-up task
before this subsystem carries real traffic: a lazy clone that runs on the hot UI
read path (M1) and a source cache that can get permanently stuck (M2 + M3).

---

## Findings

### M1 — MEDIUM (bug/perf): `GET /plugins` can now block on network git clones, on every board refresh

Path: `crates/orgasmic-daemon/src/api.rs:16055` → `marketplaces.rs:372` `offering()`
→ `marketplaces.rs:227` `plugins()` (takes `operations`) → `marketplaces.rs:244`
→ `offered_manifest` → `marketplaces.rs:463` `plugin_source` → `marketplaces.rs:471`
lazy `clone_source`.

Before this commit `plugins()` never spawned git. It now can, and `GET /plugins?project=`
is the UI's plugin-status poll: `ui/src/lib/pluginRuntime.tsx:231`, re-fired on every
`board_refreshed` event (`pluginRuntime.tsx:216`).

Two amplifiers make this worse than "one slow first call":

1. `offering()` runs a **full browse per recommended plugin**. M recommended plugins in a
   project = M sequential `plugins()` passes per request, each holding `operations`.
2. A **failing** clone is never remembered. The sticky short-circuit at
   `marketplaces.rs:245` requires `self.source_cache(..).is_dir()`, which is false when
   the clone failed (the rename at `marketplaces.rs:539` never ran). So an unreachable
   git source is re-attempted on *every* board refresh, each attempt up to the 120 s
   `GIT_TIMEOUT`, while holding `operations` — which also queues `marketplace add`,
   `refresh`, `install` and `update` behind it.

Blast radius today depends on whether `github.com/theaspirational/orgasmic-plugins`
(`shipped/marketplaces.org:5`) uses `https://`/`git@` SOURCE values or relative dirs;
that repo is not in this tree, so I could not confirm it. With relative sources, zero
impact today and this is purely latent.

Judgement on the brief's question ("is a lazy clone on a GET acceptable?"): for
`/marketplaces/plugins` (an explicit user browse, currently only called by tests), yes.
For `/plugins`, no — that route is a background poll and should never make a network call.

Fix direction: keep the lazy clone in `stage_plugin`/install only, and have `plugins()`
report `error: "source not cached; run marketplace refresh"` with a null version for an
uncloned git source. Cheaper stopgap: negative-cache clone failures (drop the `is_dir()`
condition at `marketplaces.rs:245` and add a TTL), and make `offering()` call a
non-cloning variant.

### M2 — MEDIUM (correctness): a per-entry source error is permanently sticky, and hides a cached source that still works

`crates/orgasmic-daemon/src/marketplaces.rs:244-247` short-circuits to the recorded error
whenever the cache dir exists, and `marketplaces.rs:269` re-records the same error on the
way out. The `clear_source_error` call at `marketplaces.rs:255` is unreachable in that
state. Nothing else clears it except a **successful** `refresh_source`
(`marketplaces.rs:507`) or `remove` (`marketplaces.rs:198-203`) — and refresh is
manual-only (`api.rs:15892`, `api.rs:15902`; no timer or startup refresh exists).

Failure scenario: `marketplace refresh` runs while the plugin's upstream is unreachable →
`git pull` fails at `marketplaces.rs:551` → `record_source_error` at `marketplaces.rs:509`.
The cache on disk is untouched and perfectly valid, but browse now reports
`version: null` + the git error for that plugin forever, until the user happens to run a
refresh that succeeds.

Contract inconsistency this creates: in exactly that state, `install` of the same plugin
**succeeds** — `plugin_source` (`marketplaces.rs:463`) reads the same cache directly and
never consults `source_errors`. Browse says broken, install works.

`crates/orgasmic-daemon/tests/marketplace_routes.rs:685-690` locks this in as expected
behavior. That test's cache is at the rolled-back commit, so it is a valid `tasks-plus`
0.1.0 tree — the assertion is asserting that a good source reports no version.

Fix direction: on the sticky path, still read the cached manifest; report the version from
cache **and** the error, instead of suppressing the version. Or scope the sticky flag to
"cache content is known-bad" (the id-mismatch case) and never set it for a transport
failure.

### M3 — MEDIUM (bug): a timed-out `git pull` can permanently wedge the source cache

`refresh_source` (`marketplaces.rs:544-566`) now pulls **in place**. The previous code
staged a fresh clone in a temp dir and rename-swapped, so a killed git left only a temp
dir that got deleted. On timeout the child is SIGKILLed (`marketplaces.rs:730`
`kill_on_drop`), which can leave `.git/index.lock` behind mid-checkout.

Probe (`/tmp/lockprobe`): after `touch .git/index.lock`, `git pull --ff-only` fails with
`error: Unable to create '.../.git/index.lock': File exists.` and keeps failing; `git
rev-parse HEAD` still works. `git reset --hard` needs the same lock, so the rollback at
`marketplaces.rs:559` fails too. Nothing in the code ever removes a stale lock.

Combined with M2 the entry is stuck in browse with no self-healing; recovery requires
`marketplace remove` + re-add (which is the only thing that deletes
`marketplace-sources/<key>`, `marketplaces.rs:194-197`) — and for an official marketplace
`remove` is refused (`marketplaces.rs:188-191`), so there is no recovery path at all
short of deleting the directory by hand.

Note the brief's phrasing "the rename-swap is still atomic" is only half true: the *clone*
path still stages + renames (`marketplaces.rs:534-539`), the *refresh* path no longer does.

Fix direction: restore stage-and-swap in `refresh_source` (clone fresh, validate, rename
over the old cache), or fall back to a re-clone when a pull fails.

### L1 — LOW (bug): `kill_on_drop` kills git but not its transport helper

Probe (`/tmp/killprobe`, tokio 1.52.3): a direct child is killed and reaped on timeout
(`surviving_or_zombie=0`, so no zombie — the brief's "killed and reaped" question is a
confirmed yes). A **grandchild** survives, reparented to PID 1
(`surviving_grandchildren=1`, `ps` shows `PPID 1`).

`git clone` spawns `git-remote-https` (→ curl) or `ssh`. On timeout those leak.
`GIT_HTTP_LOW_SPEED_TIME=120` (`marketplaces.rs:717`) is inherited and bounds the https
helper; a wedged ssh transfer *after* connect is not bounded by `ConnectTimeout=20`
(`marketplaces.rs:715`), so it can hang indefinitely.

Fix direction: `.process_group(0)` on the `Command` and `killpg` the group on timeout.

### L2 — LOW (bug): source-cache path can collide with a marketplace key

`source_cache` is `marketplace-sources/<key>/<id>` (`marketplaces.rs:572-574`) and keys
contain `/` (`marketplace_key` returns `host/path`). The old layout's fixed
`.orgasmic-sources` segment made a collision impossible; it is now possible.

Failure scenario: marketplace `file:///srv/market` (key `srv/market`) offers plugin id
`meetings` with a git SOURCE → cache at `marketplace-sources/srv/market/meetings`. An
admin also adds marketplace `file:///srv/market/meetings` (key `srv/market/meetings`) →
its sources dir is the same path. `clone_source`'s `create_dir_all` + `tempdir_in`
(`marketplaces.rs:516-522`) then writes into the other marketplace's plugin clone, and
`remove` of either wipes the other's cache.

Requires an admin to add both, so severity is low. Fix: insert a fixed segment —
`marketplace-sources/<key>/.sources/<id>`.

### L3 — LOW (usability): a per-marketplace refresh reports success when every source pull failed

`refresh_sources` (`marketplaces.rs:498-511`) records per-source errors and always returns
`Ok(())`; `pull_and_refresh_sources` (`marketplaces.rs:495`) returns that, so
`refresh(Some(key))` returns 200 and `list()` shows `error: null` — asserted at
`marketplace_routes.rs:609-618`. `orgasmic marketplace refresh <key>` therefore prints a
clean success while every plugin source failed to update. The user only finds out via
browse. This looks like a deliberate consequence of follow-up #2 ("no sticky marketplace
error"), but the refresh **response** could still carry a per-source summary.

### L4 — LOW (design): stale caches are never pruned, and there is no migration

Nothing deletes `~/.orgasmic/user/marketplaces/<key>/.orgasmic-sources/` on an existing
install (grep for `orgasmic-sources` finds only `marketplace.rs:240` and
`marketplace_routes.rs:698`, both tests). Likewise `marketplace-sources/<key>/<id>` is
never removed when an entry leaves the index — `refresh_sources` only iterates the current
index. Harmless bytes, but worth a line in a cleanup pass.

### L5 — LOW (test): the timeout unit test does not prove the child died

`marketplaces.rs:800-812` asserts the error string and elapsed time only. It does spawn a
real sleeping child (the brief's question — confirmed, `Command::new("sleep").arg("60")`),
but a pass is consistent with a leaked process. I proved kill + reap externally (L1), not
in-repo. Suggest capturing the pid via `spawn` and asserting it is gone after the timeout.

### L6 — LOW (correctness): `file://localhost/x` and `file:///x` key differently

`marketplace.rs:170-178`: `file:///tmp/plugins` → key `tmp/plugins`;
`file://localhost/tmp/plugins` → key `localhost/tmp/plugins`. The same repo can be added
twice under two keys, each getting its own clone and source cache. Fix: normalize an empty
and a `localhost` host to the same thing.

### Latent, no finding: `offering()` re-entrancy

`offering()` (`marketplaces.rs:372`) calls `plugins()`, which takes `operations`
(`marketplaces.rs:227`). `tokio::sync::Mutex` is not reentrant, so any future caller that
invokes `offering()` while holding `operations` deadlocks the whole marketplace subsystem.
No such caller exists today (`api.rs:16055` is the only one and holds nothing). Worth a
comment on `plugins()` noting it takes the lock.

---

## The seven assigned follow-ups — verification

| # | Item | Status |
|---|---|---|
| 1 | Bounded git child | **Done.** `marketplaces.rs:711-733`: `tokio::process::Command`, `kill_on_drop(true)`, 120 s `tokio::time::timeout`, `GIT_TERMINAL_PROMPT=0`, `GIT_SSH_COMMAND="ssh -oBatchMode=yes -oConnectTimeout=20"`, HTTP low-speed envs kept. Child killed and reaped — probed (L1). Lock released on timeout: every `_guard` is a stack `MutexGuard`, dropped on the `?` return; no `std::mem::forget`, no panic path. Transport helper not killed (L1). |
| 2 | Per-entry browse error | **Done.** `marketplaces.rs:238-283`: `let Ok(index) … else { continue }`, per-entry `error` + null version, no `record_error` from the read path. `marketplace list` no longer shows a sticky marketplace error from a browse failure (`marketplace_routes.rs:609-618`). But see M2 for the new per-entry stickiness. |
| 3 | Cache out of the clone | **Done.** `home.rs:120-121`, `marketplaces.rs:572-574`; `marketplace_routes.rs:694-699` asserts nothing is written under the clone. Rename-swap: intact on the clone path (`marketplaces.rs:534-539`), **removed** on the refresh path — see M3. |
| 4 | Reject leading `.orgasmic*` | **Done.** `marketplace.rs:204-210`, tests `marketplace.rs:240-241`. |
| 5 | Lazy source clone | **Done.** `marketplaces.rs:463-478`. **No double-clone race**: every caller of `plugin_source` (`plugins()` :227, `install` :293, `update` :327) holds `operations`, so two concurrent browses serialize. Refresh only re-pulls existing caches (`marketplaces.rs:503-505`), continues past failures, and rolls back an invalid source (`marketplaces.rs:552-565`). Judgement on the GET side effect: acceptable for `/marketplaces/plugins`, not for `/plugins` — M1. |
| 6 | Duplicate `path_segment` deleted | **Done.** Removed from `manager.rs`, now imported from `daemon_client`. |
| 7 | `file://` remote host rejected | **Done.** `marketplace.rs:174-177`, test `marketplace.rs:273`. `file:///abs` and `file://localhost/abs` still accepted; see L6. |

## Trust boundary recheck

- **`--` before every user-controlled positional**: yes. `marketplaces.rs:436-443` (clone
  url) and `marketplaces.rs:524-533` (plugin source). Every other argument is internal:
  `-C <daemon-built path>`, `rev-parse HEAD`, `pull --ff-only`, and `reset --hard
  <sha from git's own rev-parse output>`.
- **Source containment**: improved. The cache moved outside the clone, so a hostile
  marketplace repo can no longer plant anything at the cache path — the containment that
  follow-up #4 was defending is now belt-and-braces.
- **Path-segment safety**: `key` passes `validate_relative_source` inside
  `marketplace_key` (`marketplace.rs:189`); `entry.id` passes `validate_id`
  (`marketplace.rs:59`, `plugin.rs:33-45`, lowercase/digits/hyphen only). No traversal
  into `marketplace-sources/`. The only path hazard is the key/id collision in L2.
- **Symlink refusal**: `marketplaces.rs:472-475` (cache), `:517-520` (sources root),
  `:546-548` (refresh target), plus core `MarketplaceIndex::read_dir` and
  `MarketplacePlugin::source_dir`. `exists()` follows symlinks, so a dangling symlink at
  the cache path takes the clone branch and `rename` replaces it — safe.
- **Officials disable-only**: unchanged, `marketplaces.rs:186-191`.
- **Admin-only mutations**: route authz untouched by this diff; the only `api.rs` changes
  are two added `.await`s.

## Verification notes

Worktree `/Users/aspirational/.orgasmic/worktrees/orgasmic/task-1pfw6.1-review` at
`b585c23d`, one commit over `main`. `CARGO_TARGET_DIR` set worktree-local to
`./target-review` (never shared). No daemon started.

All assigned gates run, all green (logs in `/tmp/1pfw6.1/`):

| Command | Result |
|---|---|
| `cargo fmt --all -- --check` | rc=0 |
| `cargo clippy --workspace --all-targets -- -D warnings` | rc=0, zero warnings |
| `cargo test -p orgasmic-core` | rc=0 — 195 + 2 + 20 passed, 0 failed |
| `cargo test -p orgasmic-daemon --test plugin_registry_routes --test marketplace_routes` | rc=0 — 5 + 2 passed |
| `cargo test -p orgasmic-daemon --lib marketplaces` | rc=0 — 1 passed |
| `cargo test -p orgasmic-cli --test plugin_cli --test cli_parity -- --test-threads=1` | rc=0 — 9 + 2 passed |

Targeted probes (both outside the repo, nothing mutated in the worktree):

- `/tmp/lockprobe` — shell probe for M3. Created two local repos, touched
  `.git/index.lock` in the clone, ran `git pull --ff-only`:
  `error: Unable to create '/tmp/lockprobe/down/.git/index.lock': File exists.`
  `git rev-parse HEAD` still succeeded. Confirms a stale lock wedges pull and reset
  permanently while leaving the dir looking healthy.
- `/tmp/killprobe` — throwaway tokio 1.52 binary for L1. `kill_on_drop` + 50 ms timeout
  against `sh -c "sleep 31337"`: `timed_out=true`, `surviving_or_zombie=0` (killed and
  reaped). Against `sh -c "sleep 31337 & wait"`: `surviving_grandchildren=1`, `PPID 1`.

Not verified, and the residual risk:

- **M1's blast radius.** The official marketplace repo
  (`github.com/theaspirational/orgasmic-plugins`) is not in this tree, so I could not read
  its `marketplace.org` to see whether its entries use `https://`/`git@` SOURCE values. If
  they are relative directories, M1 is latent rather than live today.
- **No end-to-end timeout test.** I did not build a hanging git remote, so "a timed-out
  clone leaves `operations` free and the next `marketplace add` succeeds" rests on the
  guard-drop code structure (every `_guard` is a plain stack binding) plus the unit test,
  not on an integration run.
- **M3's wedge is proven for git, not proven to be reachable from this exact SIGKILL.**
  The stale-lock behavior is confirmed; that a SIGKILL at the 120 s mark lands inside the
  lock window is inference from git's checkout sequence, not a captured repro.

## Open questions

1. Does `theaspirational/orgasmic-plugins` use git SOURCE values today? That decides
   whether M1 is a live problem or a latent one.
2. Is `marketplace_routes.rs:685-690` (browse reports null version for a rolled-back,
   valid cache) the behavior you want, or is it M2's bug frozen into a test? A follow-up
   fixing M2 has to change that assertion.
3. Should `refresh` ever clone a source that has never been cached? Today it deliberately
   does not (`marketplaces.rs:503-505`), which means `marketplace refresh` cannot be used
   to warm the cache — and under M1 that is the only thing that would stop the UI poll
   from doing it.

## Fix directions (suggested follow-up task)

1. **M1**: remove the lazy clone from `plugins()`. Clone only in `stage_plugin`. Report
   an uncloned git source as `version: null, error: "source not cached"`. Give `offering()`
   a cheap non-cloning lookup so `/plugins` never touches the network.
2. **M2**: on the sticky path, read the cached manifest and return its version alongside
   the error; only suppress the version when the cache itself is unreadable.
3. **M3**: restore stage-and-swap in `refresh_source`, or re-clone when a pull fails.
4. **L1**: `.process_group(0)` + kill the process group on timeout.
5. **L2**: `marketplace-sources/<key>/.sources/<id>`.
6. **L6**: normalize an empty `file://` host and `localhost` to the same key.
