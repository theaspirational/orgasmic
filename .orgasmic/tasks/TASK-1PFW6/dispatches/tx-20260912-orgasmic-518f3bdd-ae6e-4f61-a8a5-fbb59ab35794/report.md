# TASK-1PFW6 round 2 re-review — marketplace registry fixes

## Verdict

**approve-with-follow-ups.**

All five gate commands pass. M1, M3, M4, M5 and M6 are genuinely fixed and
backed by tests that exercise the real HTTP routes against local bare git
repos. M2 is **partially** fixed: the guard ordering is correct and no clone or
copy runs under the `plugins.operations` write guard, but the "bound" only
covers the HTTP transport and cannot terminate a stalled child at all. Nothing
here blocks ship — the residue is a liveness gap and some resilience polish, so
it goes to follow-ups rather than a reject.

## Findings

### M-P2 `crates/orgasmic-daemon/src/marketplaces.rs:616` — the git bound is HTTP-only and cannot kill a stalled child (MEDIUM, bug/liveness)

`git()` sets exactly two things and nothing else (verified by grep: no
`timeout`, `kill`, or `Duration` appears anywhere in the file):

```rust
.env("GIT_HTTP_LOW_SPEED_LIMIT", "1")
.env("GIT_HTTP_LOW_SPEED_TIME", GIT_HTTP_LOW_SPEED_TIME_SECS)  // "120"
```

Three gaps, answering the brief's question directly:

1. **Transport coverage.** `GIT_HTTP_LOW_SPEED_*` are the env forms of
   `http.lowSpeedLimit` / `http.lowSpeedTime`, which git applies only to the
   curl/HTTP(S) transport. A `git@host:path` marketplace or plugin `:SOURCE:`
   goes over ssh and is **completely unbounded**. `git@` is an accepted
   marketplace URL (`marketplace_key`, `crates/orgasmic-core/src/marketplace.rs:144`)
   and an accepted plugin source (`is_git_source`, same file:139), so this is a
   supported path, not a hypothetical.
2. **No kill.** Even on HTTP, low-speed only aborts a transfer that has started
   and gone quiet. It does not cover an ssh host-key or passphrase prompt
   (`GIT_TERMINAL_PROMPT` is not set, and `SSH_ASKPASS`+`DISPLAY` in a GUI
   session can pop a dialog that waits forever).
3. **The wrapper cannot rescue it.** `tokio::task::spawn_blocking` tasks are
   not cancellable — dropping the `JoinHandle` neither stops the thread nor
   reaps the child. So wrapping the call in `tokio::time::timeout` would not
   help either.

**Failure scenario.** Admin runs `orgasmic marketplace add
git@unreachable.internal:plugins.git`. The ssh child hangs. `add()` holds
`self.operations` (`marketplaces.rs:111`) across the `.await`. The CLI gives up
after its new 300 s window (M1), but the daemon task keeps the lock. Every
later `marketplace add|refresh|remove|enable|disable` and every
`plugin add|update` blocks forever on that mutex, for the daemon's lifetime.
One bad URL wedges the whole marketplace subsystem until restart.

**Fix direction.** Spawn with `std::process::Command::spawn()` and poll/wait
with a deadline so you hold a `Child` you can `kill()`, or use
`tokio::process::Command` with `kill_on_drop(true)` under `tokio::time::timeout`.
Also set `GIT_TERMINAL_PROMPT=0` and
`GIT_SSH_COMMAND="ssh -oBatchMode=yes -oConnectTimeout=…"` so ssh fails fast
instead of prompting.

### M-P2 `crates/orgasmic-daemon/src/marketplaces.rs:225` — one bad plugin entry hides its siblings and records a sticky error (MEDIUM, correctness/stale-state)

The M3 fix wraps the per-marketplace body in a closure and calls
`record_error` on failure (`marketplaces.rs:225`, `:545`). That correctly stops
a bad *index* from killing the whole browse. But the `?` inside the closure
also aborts the **per-entry** loop, and `offered_manifest` now hard-errors
where it used to degrade:

- `plugin_source` (`:419`) bails with "marketplace source cache is missing"
  when `.orgasmic-sources/<id>` is absent.
- `offered_manifest` (`:346`) returns `Result<PluginManifest>`, not the old
  `Option`, so that bail propagates.

Two reachable symptoms:

1. **Partial cache hides the tail.** `refresh_sources` (`:443`) iterates
   `index.plugins` in order and returns on the first clone failure, so entries
   after the failing one never get cached. Browse then iterates the same order,
   hits the uncached entry, and drops every remaining plugin of that
   marketplace from `/marketplaces/plugins`.
2. **Race with refresh.** `plugins()` takes no lock. `refresh_sources` publishes
   by `rename(destination → backup)` then `rename(staged → destination)`
   (`:462`-`:474`). A browse landing between those two renames sees no
   `.orgasmic-sources/<id>`, drops the marketplace's plugins, and calls
   `record_error`. `clear_error` only runs on a successful `refresh`
   (`:167`), so a transient race leaves a **permanent-looking** error on the
   marketplace in `marketplace list` until someone refreshes again.

**Fix direction.** Make the entry loop collect per-entry errors instead of
`?`-ing out (surface them on the `MarketplacePluginStatus`), and do not
`record_error` from the read path — browse should not be able to write sticky
state that only a mutation can clear.

### L-P3 `crates/orgasmic-daemon/src/marketplaces.rs:444` — the source cache lives inside the marketplace's git working tree

`let sources = root.join(".orgasmic-sources")` puts the cache, plus the
`tempfile::tempdir_in(&sources)` staging dirs, inside the cloned repo. If an
upstream marketplace ever tracks a path under `.orgasmic-sources/`, the next
`git pull --ff-only` fails with "untracked working tree files would be
overwritten", and the marketplace is permanently un-refreshable — the documented
recovery is remove-and-re-add. A marketplace outside your control can do this.
**Fix direction:** keep the cache as a sibling of the clone
(`~/.orgasmic/user/marketplace-sources/<key>/`) so it is never in git's way.

### L-P3 `crates/orgasmic-core/src/marketplace.rs:190` — `.orgasmic-sources` is still addressable as a `:SOURCE:`

Answering the brief's M4 question: **the cache dir is not excluded.**
`validate_relative_source` only rejects empty, absolute, and `.`/`..`
components; `.orgasmic-sources` is a plain `Component::Normal`, so an index may
declare `:SOURCE: .orgasmic-sources/<other-id>` and `source_dir`
(`marketplace.rs:116`) will happily resolve it.

It is **not currently exploitable**: `source_dir` still runs the full
per-component symlink check and the canonicalized `starts_with(root)`
containment check, and both `refresh_sources` (`:467`) and `prepare_install`
(`:271`) assert `manifest.id == entry.id`, so pointing entry B at entry A's
cached tree fails the id check. The defense is incidental rather than
deliberate. **Fix direction:** one line in `validate_relative_source` rejecting
a leading `.orgasmic-sources` component, so the guarantee does not depend on
the id check staying in place.

Same-path-checks question: the cached tree does get the same treatment on the
copy path — `stage_plugin` routes through `copy_plugin_tree`
(`crates/orgasmic-core/src/plugin.rs:307`), which rejects a symlinked root and
`bail!`s on any non-file/non-dir entry, so an attacker-controlled git repo
cannot smuggle a symlink into `~/.orgasmic/user/plugins/`. The one asymmetry is
`offered_manifest` → `PluginManifest::read_dir`, which follows symlinks and has
no `symlink_metadata` guard; it only ever reads `plugin.org`, so impact is nil.

### L-P3 `crates/orgasmic-daemon/src/marketplaces.rs:406` — `add` now clones every git-source plugin in the index

Moving the source clone from install-time to refresh-time (M4) means
`clone_into` → `refresh_sources` clones *every* `https://`/`git@` entry before
`marketplace add` returns. A marketplace with 50 git-source plugins does 51
clones inside one HTTP request, all under `operations`. This is what makes the
M-P2 hang above so easy to trigger and what the 300 s CLI window is absorbing.
**Fix direction:** cache lazily on first install/browse, or cap concurrency and
report per-entry failures rather than all-or-nothing.

### L-P3 `crates/orgasmic-cli/src/manager.rs:11933` — duplicate `path_segment`

M6 correctly moved the CLI marketplace helper onto the strict percent-encoder
in `daemon_client.rs:336`. But `manager.rs:11933` still carries a
byte-identical private copy. No bug today, but it is the exact duplication that
produced M6 in the first place. **Fix direction:** delete it, use
`crate::daemon_client::path_segment` (already `pub(crate)`).

### L-P3 `crates/orgasmic-core/src/marketplace.rs:165` — `file://<host>/…` can key onto the official marketplace

`marketplace_key("file://github.com/theaspirational/orgasmic-plugins.git")`
takes the non-absolute `file://` branch and yields
`github.com/theaspirational/orgasmic-plugins` — the shipped official key.
`add()` (`marketplaces.rs:117`) permits an existing key when it is official and
then writes **no** user record, so the clone lands in the official
marketplace's root while still listing as official.

I probed what git does with that URL:

```
$ git clone --depth 1 -- "file://github.com/theaspirational/orgasmic-plugins.git" c2
fatal: '/theaspirational/orgasmic-plugins.git' does not appear to be a git repository
```

git discards the host and reads the local path `/theaspirational/…`, so
exploiting this needs write access to `/` (root, and SIP-blocked on macOS) plus
an admin token. Practically unreachable, hence P3. **Fix direction:** reject a
non-empty, non-`localhost` host for the `file://` scheme.

## Open Questions

1. I could not check the fix commit against the **literal** L1–L7 list — the
   first review's text is not in the branch or the task record I was given. I
   verified the L-shaped changes that are actually in the diff (see
   Verification Notes) and found them coherent, but "all seven L items closed"
   is the implementer's claim, not something I reproduced.
2. `remove` changed from quarantine-by-rename into `user/marketplaces-removed/`
   to an outright `std::fs::remove_dir_all` (`marketplaces.rs:189`). I assume
   that was a deliberate L-item (the quarantine grew unbounded and a clone is
   re-fetchable). Flagging in case the intent was the opposite.

## Verification Notes

Worktree `task-1pfw6-review` at `9506effe`, worktree-local
`CARGO_TARGET_DIR=$PWD/target-review`, no daemon started. Logs under
`/tmp/1pfw6-logs/`. All five assigned commands pass:

| command | result |
|---|---|
| `cargo fmt --all -- --check` | exit 0 |
| `cargo clippy --workspace --all-targets -- -D warnings` | exit 0, zero warnings (core/daemon/cli all present in the log) |
| `cargo test -p orgasmic-core` | exit 0 — 195 + 2 + 20 passed, 0 failed |
| `cargo test -p orgasmic-daemon --test plugin_registry_routes --test marketplace_routes` | exit 0 — 5 + 2 passed, 0 failed |
| `cargo test -p orgasmic-cli --test plugin_cli --test cli_parity -- --test-threads=1` | exit 0 — 9 + 2 passed, 0 failed |

Per-item, reading the source rather than trusting the claim:

- **M1 — fixed.** `MARKETPLACE_REQUEST_TIMEOUT_SECS = 300` and
  `post_marketplace_json` (`daemon_client.rs:37`, `:140`), wired into
  `marketplace add` and both refresh forms (`marketplace.rs:43`, `:60`) and
  into `plugin add <mkt>/<id>` and `plugin update` (`plugin.rs:146`, `:171`).
  Test `marketplace_requests_outlast_the_default_client_window`.
- **M2 — partially fixed.** Guard ordering read directly in `api.rs:15967-15980`
  and `:16000-16013`: `prepare_install`/`prepare_update` complete **before**
  `state.plugins.operations.write().await`. `publish_install`/`publish_update`
  (`marketplaces.rs:283`, `:322`) are rename-only. `prepare_install` resolves
  git sources from the cache (`plugin_source`, `:419`), never a clone — so
  confirmed: **no clone and no tree copy runs under the plugins write guard.**
  `git()` does run in `spawn_blocking`. The bound itself fails — see M-P2 above.
- **M3 — fixed.** Browse-past-a-bad-index via the closure + `record_error`
  (`:225`, `:545`); pre-pull HEAD reset on an invalid index in
  `pull_and_refresh_sources` (`:429-:441`, `git rev-parse HEAD` then
  `reset --hard`); refresh-all continues per key via the `refresh_all` flag
  (`:146`, `:167`). Covered by
  `bad_marketplace_indexes_do_not_break_browse_or_refresh_all`. Caveat in the
  second MEDIUM finding.
- **M4 — fixed.** Cache at `<marketplace>/.orgasmic-sources/<id>` populated by
  `refresh_sources` on both clone and pull (`:409`, `:441`). Browse reports the
  offered version and `update_available` from the cached manifest (`:230-:248`).
  Test `git_url_sources_are_cached_for_browse_install_and_update` is a good one:
  it uses a `GIT_CONFIG_GLOBAL` `insteadOf` rule to map `https://source.test/…`
  onto a local bare repo, so the `is_git_source` branch is genuinely exercised
  with **no network**, and it asserts 0.1.0 → install → 0.2.0 →
  `update_available: true` plus the on-disk cache file. Cache-dir addressability
  and the cached-tree path checks are answered in the L-P3 finding above.
- **M5 — fixed.** `marketplace_routes.rs:228-245` loops a member token over add,
  refresh-all, refresh-one, remove and plugin-update; `:257-267` covers
  activation; `:288-299` covers plugin-install. Seven mutations, all asserting
  403, with the member's read paths asserting 200 so the test cannot pass by
  the token simply being invalid.
- **M6 — fixed.** `path_segment` is now a strict unreserved-set percent-encoder
  (`daemon_client.rs:336`), replacing the `.replace('%').replace('/')` version;
  the CLI copy in `marketplace.rs` is deleted and imported instead. `?`/`#` are
  rejected in keys at all three URL branches
  (`core/marketplace.rs:148`, `:161`, `:170`), with negative unit tests. The
  `manager.rs` duplicate is the L-P3 note above.

First-review "holds up" items, re-checked after the refactor:

- **Source containment — holds.** `source_dir` (`core/marketplace.rs:116`)
  still canonicalizes and asserts `starts_with(root)`. I also confirmed
  `marketplace_key` ends in `validate_relative_source(key)` (`:187`), so an
  absolute `file:///abs` key is de-absolutized by `trim_matches('/')` and a
  `https://host/../../etc` key is rejected outright — `root()`'s bare
  `home.marketplaces().join(key)` (`marketplaces.rs:494`) cannot be made to
  escape.
- **Symlink refusal — holds, and is now the load-bearing guard for the newly
  attacker-controlled cached trees.** `copy_plugin_tree`
  (`core/plugin.rs:307-331`) rejects a symlinked source root and `bail!`s on
  any entry that is neither file nor dir; `MarketplaceIndex::read_dir`
  (`:92-101`) rejects a symlinked root and a symlinked `marketplace.org`.
- **`--` before git positionals — holds.** Present in `clone_into` (`:404`) and
  in the new `refresh_sources` clone (`:456`). The one new bare positional is
  `reset --hard <old_head>`, where `old_head` is `git rev-parse HEAD` output
  (a hex sha), not attacker input.
- **Officials disable-only — holds.** `remove` refuses officials (`:180`);
  `activate` (`:194`) only flips `enabled` and preserves `url`, `official` and
  `alias`; `add` on an official key writes no user record (`:120`). The narrow
  `file://` host-collision caveat is the last L-P3 finding.

Incidental fixes in the diff that I verified are sound: `file://` rejected as a
plugin `:SOURCE:` (`core/marketplace.rs:67`) while remaining legal as a
marketplace URL (documented in `PLUGINS-SCOPE.md:386`);
`copy_plugin_tree` now drops a forged `.orgasmic-install.org`
(`core/plugin.rs:315`, with a test); `ensure_installable_id` blocks shadowing a
core plugin id before publish (`plugins.rs:293`, test
`reserved_core_plugin_id_is_rejected_before_publish`); CLI `is_explicit_path`
stops `./foo` being parsed as `<marketplace>/<id>` (`plugin.rs:204`);
`remove` writes records before deleting the tree so a mid-operation crash
cannot leave an orphan record; duplicate plugin ids in an index are rejected
(`core/marketplace.rs:60`).

Acceptance criteria, checked against source not claims:

- *daemon route tests against local bare git repos, no network* — met.
  `bare_marketplace`/`bare_plugin` build `git init --bare` repos in a tempdir
  and address them over `file://`; the one `https://` case is redirected by
  `insteadOf`. Tests run green with no network dependency.
- *admin-only mutations* — met, see M5.
- *update keeps activation, grown capabilities need re-approval* — met, and the
  enforcement is real: `plugins.rs:325-333` disables an enabled plugin with
  "capabilities grew; enable again to approve them" when
  `!manifest.capabilities.is_subset(&activation.approved_capabilities)`, and
  `post_plugin_update` (`api.rs:16016`) calls `reconcile(true)` so the new
  manifest goes through that gate. Activation records themselves are untouched
  by update.

Probe run (not a unit test): I confirmed git's `file://<host>/<path>` handling
empirically in `/tmp/gitfiletest` — see the L-P3 finding. No failures were
observed in any suite, so there is nothing to classify as regression, flake, or
environment-blocked.

**Residual test gaps** (no test in the branch covers these):

1. No test drives a git child that stalls, so the M2 bound is asserted nowhere —
   which is precisely why the ssh gap survived the fix round. A test that points
   a marketplace at a TCP socket which accepts and never writes would pin it.
2. No concurrency test. The browse-vs-`refresh_sources`-rename race and the
   `prepare`/`publish` TOCTOU window (the marketplace `operations` lock is
   released between them; `publish_install`'s `!destination.exists()` re-check
   is what closes it) are both unexercised.
3. No test for a marketplace with a *partially* populated source cache, the
   condition behind the browse finding.

## Fix Directions

Ordered by what I would do first. None of these should block the merge.

1. Replace the env-var bound with a real deadline you can enforce: spawn the
   child, wait with a timeout, `kill()` on expiry. Add `GIT_TERMINAL_PROMPT=0`
   and a `GIT_SSH_COMMAND` with `BatchMode=yes` and a `ConnectTimeout`. Note
   that `spawn_blocking` cannot be cancelled, so the kill has to come from
   holding the `Child`, not from wrapping the future.
2. Make browse per-entry fault-tolerant, and stop it writing to `state.errors`
   from a read path.
3. Move `.orgasmic-sources` out of the cloned working tree.
4. Reject a leading `.orgasmic-sources` component in `validate_relative_source`,
   and reject a non-empty non-`localhost` host for `file://` in
   `marketplace_key`.
5. Delete the duplicate `path_segment` in `manager.rs`.
6. Reconsider cloning every git source at `marketplace add` time.
