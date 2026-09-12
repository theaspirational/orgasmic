# TASK-1PFW6.2: marketplace source cache, round 3 follow-ups

`crates/orgasmic-daemon/src/marketplaces.rs` on `main` (after TASK-1PFW6.1). The Opus reviewer approved with follow-ups. Fix all of them. Rust only.

1. **`GET /plugins` must never spawn git.** `offering()` → `plugins()` → `plugin_source` lazily clones git-URL sources, and `GET /plugins?project=` is the UI's background poll on every board refresh. A failing clone is retried on every poll for up to 120 s each while holding `operations`. Fix: remove the lazy clone from the read path entirely. `plugins()` reports `version: null` and `error: "source not cached; run marketplace refresh"` for an uncloned git source. Only `refresh` (all sources, not just cached ones) and `install`/`update` clone. Give `offering()` a non-locking or single-pass variant so M recommended plugins do not run M full browses; one browse per request. Add a doc comment on `plugins()` stating it takes `operations` and must not be called while holding it.
2. **Sticky source error hides a valid cache.** When a `refresh` pull fails for transport reasons the cache on disk is still valid, but browse reports null version plus the error forever, while install from the same cache works. Fix: on the error path still read the cached manifest and report its version alongside the error. Only an id mismatch or unparseable manifest suppresses the version. Update the test at `marketplace_routes.rs` that currently asserts a good cache reports no version.
3. **Timed-out pull wedges the cache.** `refresh_source` pulls in place; a killed git leaves `.git/index.lock` and both pull and the `reset --hard` rollback fail forever, with no recovery for an official marketplace. Fix: restore stage-and-swap for refresh: clone fresh into a temp dir under the sources root, validate, rename over the old cache. Same for the marketplace clone itself if `git pull --ff-only` fails with a lock or non-ff error: fall back to a fresh clone and swap.
4. **Kill the whole process group.** `kill_on_drop` kills git but not `git-remote-https`/`ssh` grandchildren. Set `.process_group(0)` on the command and `killpg` the group on timeout (libc is already a dependency). Extend the timeout unit test to capture the pid and assert it is gone after the timeout.
5. **Cache path collision.** `marketplace-sources/<key>/<id>` can collide with another marketplace's key. Use `marketplace-sources/<key>/.sources/<id>`.
6. **Per-marketplace refresh reports success when every source failed.** Return a per-source summary in the refresh response (`sources: [{id, ok, error}]`) and have the CLI print failures.
7. **Prune.** On refresh, delete cached sources for ids no longer in the index. On daemon start or first marketplace access, delete any legacy `<clone>/.orgasmic-sources/` directory.
8. **`file://localhost/x` and `file:///x` key differently.** Normalise both to the same key in `marketplace_key`.

Verify:

```sh
cargo fmt --all -- --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test -p orgasmic-core
cargo test -p orgasmic-daemon --test plugin_registry_routes --test marketplace_routes
cargo test -p orgasmic-daemon --lib marketplaces
cargo test -p orgasmic-cli --test plugin_cli --test cli_parity -- --test-threads=1
```

Worktree-local `CARGO_TARGET_DIR`. No daemon. Do not touch `ui/`. One commit with a real message, then `orgasmic dispatch finalize`. Report each item 1 to 8 with the covering test.
