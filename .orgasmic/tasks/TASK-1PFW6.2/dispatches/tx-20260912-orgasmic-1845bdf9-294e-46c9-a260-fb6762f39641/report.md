# Review TASK-1PFW6.2 — marketplace source cache, round 3

**Verdict: approve-with-follow-ups.**

All eight brief items are implemented and independently verified against the
diff (`git diff main...HEAD`, commit `5671e4d2`, 4 files). Every assigned gate
passes. The follow-ups below are two MEDIUM resilience regressions and six LOW
items; none block ship.

## Findings

### MEDIUM

**M1 `crates/orgasmic-daemon/src/api.rs:16056` — `GET /plugins` now fails
whole-request where it used to degrade.**
The removed `offering()` was `self.plugins().await.ok()?` — a `plugins()`
failure silently yielded `marketplace: null`. The replacement propagates:
```rust
for plugin in state.marketplaces.plugins().await
    .map_err(|error| ApiError::bad_request(error.to_string()))?
```
`plugins()` returns `Err` only from `self.records()?` (marketplaces.rs:669),
which fails on a malformed `~/.orgasmic/user/marketplaces.org` (`parse_records`,
marketplaces.rs:798) or on M2. Symptom: one bad line in `marketplaces.org` turns
`GET /plugins` into a 400, and the entire Plugins page — including installed
plugins that have nothing to do with marketplaces — goes blank. Only reachable
when at least one plugin is `recommended` (the `any()` guard at api.rs:16055).
Fix direction: keep the old tolerance — `let offerings = state.marketplaces
.plugins().await.unwrap_or_default();` (or log-and-continue), so marketplace
attribution degrades to `null` instead of killing the page. This is the same
failure mode round 2 fixed for browse ("keep browse alive past bad entries");
the `/plugins` path did not get the same treatment.

**M2 `crates/orgasmic-daemon/src/marketplaces.rs:687` — best-effort legacy
cleanup gates every marketplace operation.**
```rust
self.cleanup_legacy_sources(&records)?;   // line 687
...
    remove_path(&legacy)?;                // line 706
```
`cleanup_legacy_sources` is a one-shot migration, but its error propagates out
of `records()`, and `records()` is on the path of `list`, `add`, `refresh`,
`remove`, `activate`, `plugins`, `resolve_key` and `resolve_plugin`. Failure
scenario: `<clone>/.orgasmic-sources` exists and cannot be removed (root-owned
file, immutable flag, EPERM on a mounted path). `remove_path` returns `Err`,
`legacy_sources_cleaned` is never set (line 709 is past the `?`), so every
marketplace call fails forever with the deletion error — including the refresh
that would be the operator's remedy. Combined with M1 this also 400s
`GET /plugins`. Fix direction: make it best-effort — ignore the `remove_path`
result and set `legacy_sources_cleaned = true` unconditionally, or move the
call out of `records()` into daemon startup.

### LOW

**L1 `crates/orgasmic-daemon/src/marketplaces.rs:784` — post-swap cleanup
failure is reported as a refresh failure.**
```rust
if backup.symlink_metadata().is_ok() {
    remove_path(&backup)?;      // swap already succeeded
}
```
At this point the new cache is already in place and correct. If removing the
old backup fails, `refresh_source` returns `Err`, so `refresh_sources`
(marketplaces.rs:551) records a sticky `source_error` and reports
`sources[].ok = false` for a source that in fact refreshed cleanly — and
`plugins()` then surfaces that stale error alongside the (correct) version.
Fix direction: `let _ = remove_path(&backup);` — the next refresh's
`prune_source_caches` reaps it anyway.

**L2 `crates/orgasmic-daemon/src/marketplaces.rs:779` — restore/cleanup use
different existence tests.**
Line 775 and 784 use `symlink_metadata().is_ok()`; line 779 uses
`backup.exists()`, which follows symlinks. If `destination` were a dangling
symlink it would be renamed to `backup` at 776, then on a staged-rename failure
`backup.exists()` is `false`, the restore is skipped and the backup is orphaned
silently. `refresh_source:575-580` rejects symlink destinations today, so this
is latent, not live. Fix direction: use `symlink_metadata().is_ok()` at 779 too.

**L3 `crates/orgasmic-daemon/src/marketplaces.rs:520` and `:467` — marketplace
roots have no orphan reaper.**
`prune_source_caches` (marketplaces.rs:639, called at 538) only scans `marketplace-sources/<key>/.sources`.
Nothing scans `marketplaces/`. If the daemon dies between the two renames in
`swap_directory` (pull-fallback path, line 520), a full clone is left as
`marketplaces/<key-parent>/.<leaf>-<uuid>` and is never reclaimed; likewise a
SIGKILL during `stage_marketplace` leaves a `tempfile` dir directly under
`marketplaces/` (line 467). Self-healing is correct — the next refresh sees no
`.git` and re-clones — but the disk leak is permanent. Fix direction: on
refresh, sweep `marketplaces/` (and the key's parent dir) for `.<name>-<uuid>`
and `.tmp*` entries, mirroring `prune_source_caches`.

**L4 `crates/orgasmic-daemon/src/marketplaces.rs:574` — refresh is now O(N full
clones) with no change detection.**
`refresh_source` unconditionally clones `--depth 1` and swaps; the previous
`rev-parse`/`pull` fast path is gone, and `refresh_sources:538-541` no longer
skips uncached entries. A marketplace with N git-URL plugins pays N clones on
every refresh, each with a 120s ceiling. Harmless today (the official
marketplace uses relative `SOURCE:` paths, so N=0), but it is the cost model a
third-party marketplace will hit. Fix direction: `git ls-remote <source> HEAD`
and skip the clone when it matches the cached `HEAD` — cheap, and keeps the
"never pull in place" property.

**L5 `crates/orgasmic-daemon/src/marketplaces.rs:996` — the process-group test
races its own fixture.**
The shell has 100ms to fork `sleep 60`, write `$$ $!` to the pid file and reach
`wait`. If it loses that race, `std::fs::read_to_string(pids).unwrap()` panics
with `NotFound` — or parses a torn line — instead of failing the actual
assertion, which reads as a mystery panic rather than "the fixture was slow". I
ran it 25×, 0 failures (see Verification), so this is a load-sensitivity note,
not an observed flake. Fix direction: poll for the pid file before asserting, or
raise the timeout to ~500ms.

**L6 `ui/src/lib/api.ts:682` — the new `sources` field has no UI consumer.**
`MarketplaceStatus` in the UI type does not declare `sources`, and
`PluginsView.tsx:294` discards the refresh response (it only re-triggers
`marketplaces.refresh` / `catalog.refresh`). A refresh in which every git source
failed renders a healthy marketplace card with a fresh "Last refreshed"
timestamp. Nothing is actually hidden — the per-plugin errors appear on the
Browse tab and the CLI prints the raw JSON (`crates/orgasmic-cli/src/
marketplace.rs:81`) — so this is unmet *potential*, not a contract break. Brief
item 6 only required the response to carry the results, which it does. Fix
direction: if the refresh result should be visible, add `sources` to the TS type
and show a "n of m sources failed" line on the card.

## Brief items — verified

1. **No git on the read path.** `offered_manifest` (marketplaces.rs:397) is now
   sync and resolves via `cached_source` / `entry.source_dir`; neither
   `orgasmic-core/src/plugin.rs` nor `orgasmic-core/src/marketplace.rs` contains
   a `Command::new` (grepped). `offering()` is deleted with no remaining callers
   (grepped across `crates/` and `ui/src`). `get_plugins` makes exactly one
   `plugins()` call behind a `BTreeMap`. Uncached git source →
   `"source not cached; run marketplace refresh"` with `version: null`, asserted
   at `marketplace_routes.rs:611-620`. The `plugins()` doc comment at
   marketplaces.rs:251 names the `operations` acquisition, and no caller holds
   `marketplaces.operations` across the call (`prepare_install`/`prepare_update`
   drop their guard before `plugins.operations.write()` is taken —
   api.rs:15967-15972, 16001-16006). ✔
2. **Valid cache survives a recorded transport error.** marketplaces.rs:284
   returns `self.source_error(...)` in the `Ok(offered)` arm, so version and
   capabilities ship alongside the error; asserted at
   `marketplace_routes.rs:766-772` (`version == "0.1.0"` *and*
   `error` contains "does not match"). ✔
3. **Clone-fresh-and-swap.** `refresh_source:574` clones into
   `tempfile::tempdir_in(<parent>)` — a sibling of the destination under the
   same directory, so the `rename` in `swap_directory` is same-filesystem and
   atomic. Validation (`PluginManifest::read_dir` + id check, lines 606-610)
   runs *before* `swap_directory`, so a failed validation returns early and the
   old cache is untouched. The old cache is renamed to a backup and removed
   *after* the new one lands (lines 775-786). The marketplace `git pull`
   failure path falls back to `stage_marketplace` + `swap_directory`
   (lines 516-521); note `stage_marketplace` validates the index itself
   (line 478), so the `reset --hard old_head` recovery at line 526 is
   unreachable after a fallback and `old_head` staleness is not a live bug. The
   fallback preserves the user record and `official` flag because both live in
   `marketplaces.org`, not in the clone (`parse_records` re-derives the key from
   `:URL:`, marketplaces.rs:821), and it runs under `operations` (held by
   `refresh`, marketplaces.rs:158). Stale `index.lock` covered at
   `marketplace_routes.rs:736-740` and `:794-797`. ✔
4. **Process group.** `command.process_group(0)` (marketplaces.rs:885) makes the
   child its own group leader, so `pgid == child.id()`; `spawn()` is `?`-checked
   *before* the pid is read (lines 886-888), so a failed spawn never reaches
   `killpg` and the pid can never be 0 or the daemon's own group. `ESRCH` is
   tolerated, the group is reaped via `output.await`. The unit test asserts both
   the shell pid and the `sleep` pid are gone. ✔
5. **Fixed segment.** `source_cache` = `marketplace-sources/<key>/.sources/<id>`
   (marketplaces.rs:618-624); `remove()` and `prune_source_caches` use the same
   path. The segment does disambiguate the `key="a/b"` vs `key="a", id="b"`
   collision, and plugin ids cannot start with `.` (`validate_id`). ✔
6. **Per-source refresh results.** `MarketplaceSourceStatus` (marketplaces.rs:48)
   is filled by `refresh()` at line 196; `list()` returns `sources: []`.
   Asserted at `marketplace_routes.rs:632-647`. See L6 for the UI gap. ✔
7. **Pruning + legacy cleanup.** `prune_source_caches` (line 639) removes
   `.sources` entries absent from the index *and* retains only current
   `source_errors`; asserted at `marketplace_routes.rs:815-820`.
   `cleanup_legacy_sources` cannot escape `<clone>/.orgasmic-sources`: `key` is
   `validate_relative_source`-checked (no `..`, not absolute,
   `orgasmic-core/src/marketplace.rs:193`), a symlinked `root` is skipped
   (`symlink_metadata().is_ok_and(is_dir)`, line 700), and `remove_path`
   branches on `symlink_metadata` so a symlinked `.orgasmic-sources` is unlinked
   rather than followed. Asserted at `marketplace_routes.rs:865-868, 897`. ✔
   One residual: the cleanup is unconditional rather than scoped to a directory
   we created, so a marketplace repo that legitimately tracks `.orgasmic-sources`
   would be dirtied on every daemon start (→ `pull --ff-only` fails → fallback
   re-clone → dirtied again). Speculative; folded into M2's fix direction.
8. **`file://localhost` normalisation.** `orgasmic-core/src/marketplace.rs:178`
   now emits `/{path}`, so `file://LOCALHOST/tmp/plugins.git` and
   `file:///tmp/plugins.git` both key to `tmp/plugins`; asserted at
   `marketplace.rs:266-269`. No migration concern — the whole marketplaces
   feature landed today (`5c7a8ad0`, 2026-09-12) and is unreleased. ✔

## Open Questions

- M1: is a 400 on `GET /plugins` the intended behaviour when `marketplaces.org`
  is unreadable, or should marketplace attribution degrade to `null`? I read the
  round-2 "keep browse alive" follow-up as implying the latter.
- L6: was `sources` meant to reach the UI in this task, or is it CLI/API-only
  until a later ticket?

## Verification Notes

Worktree `/Users/aspirational/.orgasmic/worktrees/orgasmic/task-1pfw6.2-review`
at `5671e4d2`, worktree-local `CARGO_TARGET_DIR=$PWD/target-review`, no daemon.
Logs under `/tmp/rev1pfw6/`.

| Command | Result |
| --- | --- |
| `cargo fmt --all -- --check` | rc=0 (`fmt.log`) |
| `cargo clippy --workspace --all-targets -- -D warnings` | rc=0 (`clippy.log`) |
| `cargo test -p orgasmic-core` | rc=0, 195+2+20 passed, 2 ignored (`core.log`) |
| `cargo test -p orgasmic-daemon --test plugin_registry_routes --test marketplace_routes` | rc=0, 5 + 2 passed (`daemon.log`) |
| `cargo test -p orgasmic-daemon --lib marketplaces` | rc=0, 1 passed (`lib.log`) |
| `cargo test -p orgasmic-cli --test plugin_cli --test cli_parity -- --test-threads=1` | rc=0, 9 + 2 passed (`cli.log`) |

Targeted probes:
- Flake probe for L5: `command_output_times_out_a_sleeping_process_group` run
  25× consecutively — 0 failures. The finding is a code-reading risk under load,
  not an observed flake.
- Zero-git-on-read proof: grepped `Command::new|process::Command` across
  `orgasmic-core/src/plugin.rs` and `orgasmic-core/src/marketplace.rs` (no hits)
  and read the full `plugins()` call graph; the only `git()` call sites left are
  `clone_into`/`stage_marketplace`/`pull_and_refresh_sources`/`refresh_source`,
  all under `add`/`refresh`/`prepare_install`/`prepare_update`.
- Lock-ordering probe: grepped every `plugins.operations` and
  `marketplaces.*` call site in `api.rs`; no handler holds one across the other,
  so the new `plugins()` call in `get_plugins` cannot deadlock.
- No production-path probe was run against a live daemon (brief says "No
  daemon"). Residual risk: M1's 400 is argued from the diff and the `records()`
  error surface rather than observed against a running daemon; no test covers a
  malformed `marketplaces.org` reaching `GET /plugins`.

Failure classification: none. No test failed in any suite.

## Fix Directions

Ordered by value:
1. M2 — make `cleanup_legacy_sources` best-effort; set the flag unconditionally
   and drop the `?` at line 706. One-line change, removes a brick-the-subsystem
   path.
2. M1 — `unwrap_or_default()` the `plugins()` result in `get_plugins`
   (api.rs:16058), restoring the pre-change degradation. Add a route test with a
   corrupt `marketplaces.org` asserting `GET /plugins` still returns 200.
3. L1/L2 — `let _ = remove_path(&backup)` at 785; `symlink_metadata().is_ok()`
   at 779.
4. L3 — extend the prune sweep to `marketplaces/` orphans.
5. L4 — `git ls-remote HEAD` short-circuit in `refresh_source`, if third-party
   marketplaces with git-URL sources are expected.
6. L5/L6 — test fixture timing; `sources` in the UI type if it should be shown.
