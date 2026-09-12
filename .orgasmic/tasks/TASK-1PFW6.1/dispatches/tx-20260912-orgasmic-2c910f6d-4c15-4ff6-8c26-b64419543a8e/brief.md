# TASK-1PFW6.1: marketplace registry follow-ups from the round 2 review

The marketplace registry merged to `main` (see `crates/orgasmic-daemon/src/marketplaces.rs`, `crates/orgasmic-core/src/marketplace.rs`, `crates/orgasmic-cli/src/marketplace.rs`, tests in `crates/orgasmic-daemon/tests/marketplace_routes.rs`). The Opus reviewer approved with follow-ups. Fix all of them.

1. **Git child can hang forever and wedge the subsystem.** `fn git` in `marketplaces.rs` only sets `GIT_HTTP_LOW_SPEED_*`, which covers HTTP only; `git@` ssh URLs are unbounded, a prompt can block, and `spawn_blocking` cannot be cancelled. `add()` holds `self.operations` across it, so one hung URL blocks every later marketplace and plugin mutation until restart. Fix: use `tokio::process::Command` with `kill_on_drop(true)` under `tokio::time::timeout` (120 s), set `GIT_TERMINAL_PROMPT=0` and `GIT_SSH_COMMAND="ssh -oBatchMode=yes -oConnectTimeout=20"`. Keep the HTTP low-speed envs. Test: a marketplace URL pointing at a fake `git` on `PATH` that sleeps (or a `file://` repo behind a script) returns an error within the bound and the next `marketplace add` succeeds. If a clean test is impossible without network, a unit test of the timeout wrapper with a `sleep` child is enough.
2. **Browse: one bad entry hides its siblings and writes a sticky error.** In `plugins()` the per-entry `?` aborts the loop for that marketplace, and `record_error` is called from the read path, which only a later successful refresh clears. Fix: collect per-entry errors onto the `MarketplacePluginStatus` (an `error` field, version null), never abort the marketplace loop on an entry, and never write `record_error` from `plugins()`. `refresh_sources` must also continue past a failing entry and report per-entry errors instead of returning on the first.
3. **Source cache lives inside the clone.** `.orgasmic-sources` under the marketplace root collides with `git pull` if upstream ever tracks that path. Move the cache to a sibling: `~/.orgasmic/user/marketplace-sources/<key>/<id>/`. Add a `Home` accessor next to `marketplaces()`.
4. **`.orgasmic-sources` addressable as `:SOURCE:`.** After item 3 the folder is gone from the clone, but still add an explicit rejection in `validate_relative_source` for any leading component starting with `.orgasmic`.
5. **`marketplace add` clones every git-source plugin up front.** With item 2 done, refresh reports per-entry failures. Additionally make the source clone lazy: clone a git-URL source on first browse or install if the cache is missing, and on refresh only re-pull sources already cached. Browse must still show a null version with `error` when the lazy clone fails, not fail the request.
6. **Duplicate `path_segment` in `crates/orgasmic-cli/src/manager.rs`.** Delete it and use `crate::daemon_client::path_segment`.
7. **`file://<host>/…` keys onto the official marketplace.** In `marketplace_key`, reject `file://` with a non-empty host other than `localhost`.

Verify:

```sh
cargo fmt --all -- --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test -p orgasmic-core
cargo test -p orgasmic-daemon --test plugin_registry_routes --test marketplace_routes
cargo test -p orgasmic-cli --test plugin_cli --test cli_parity -- --test-threads=1
```

Worktree-local `CARGO_TARGET_DIR`. No daemon. Do not touch `ui/`. One commit with a real message (not "Changed"), then `orgasmic dispatch finalize`. Report each item 1 to 7 with the file and the test that covers it.
