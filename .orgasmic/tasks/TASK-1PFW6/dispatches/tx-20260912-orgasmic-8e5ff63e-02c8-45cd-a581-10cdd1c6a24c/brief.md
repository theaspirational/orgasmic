# TASK-1PFW6 fix round 2: address the review of the marketplace registry

Your previous round landed as commit `5c7a8ad0` on branch `task-1pfw6-impl` (this worktree). The Opus reviewer requested changes. Fix every item below in this worktree, one new commit on top, then `orgasmic dispatch finalize`. Do not rewrite the first commit.

## Must fix

**M1 CLI timeout.** `crates/orgasmic-cli/src/daemon_client.rs` applies a 10 s client-wide timeout; `marketplace add/refresh` and `plugin add <marketplace>/<id>` / `plugin update` inherit it, so a real clone times out client-side while the daemon finishes, and the retry says "already registered". Add a longer per-request timeout for those calls the way `post_full_board_json` and `post_dispatch` already do in that file (a `MARKETPLACE_REQUEST_TIMEOUT_SECS`, 300 s is fine).

**M2 blocking git in the daemon.** `fn git` in `crates/orgasmic-daemon/src/marketplaces.rs` runs `std::process::Command::output()` inside async handlers with no timeout, and `post_plugin_install` in `api.rs` takes `state.plugins.operations.write()` before the clone, so a slow or hung remote stalls plugin UI asset serving and the reconcile loop daemon-wide. Fix: run git through `tokio::task::spawn_blocking`; bound it (kill the child after a timeout, 120 s, or set `GIT_HTTP_LOW_SPEED_LIMIT`/`GIT_HTTP_LOW_SPEED_TIME` on the child, either is fine, pick one); and stage the clone or copy *before* taking the `plugins.operations` write guard, holding the guard only for the final rename and reconcile. Same for update.

**M3 one bad index breaks browse.** `plugins()` in `marketplaces.rs` uses `?` on `MarketplaceIndex::read_dir` inside the loop, so one unparseable `marketplace.org` 400s `GET /marketplaces/plugins` for every marketplace. Collect per-marketplace errors like `list()` does and keep going. Also in `refresh`: capture `HEAD` before `git pull`, and if the post-pull index fails validation, `git reset --hard <old head>` so a bad publish does not stick; record the error on the entry.

**M4 git-URL sources report wrong version.** For `:SOURCE:` git URLs, `offered_manifest` returns the installed manifest, so `version` echoes the installed one, `update_available` is always false, and capabilities are empty before install. Fix: on refresh, shallow-clone each git-URL source into a cache under the marketplace root (for example `<root>/.orgasmic-sources/<id>/`), read `plugin.org` from there, and use that for browse, install and update. Add a route test with a git-URL `:SOURCE:` pointing at a second local bare repo: browse shows the offered version, install works, bumping the source repo and refreshing shows `update_available`.

**M5 authz tests.** Only 2 of 7 mutations are tested with a member token. Add one test that loops a member token over `POST /marketplaces`, `POST /marketplaces/refresh`, `POST /marketplaces/:key/refresh`, `POST /marketplaces/:key/remove`, `POST /plugins/:id/update` and asserts 403 for each.

**M6 CLI path encoding.** `crates/orgasmic-cli/src/marketplace.rs` has its own encoder that misses `?` and `#`. Delete it and use `crate::daemon_client::path_segment`. Also make `marketplace_key` in core reject a URL whose path contains `?` or `#`.

## Also fix (small)

- **L1** Removed marketplace clones go to `marketplaces-removed/<uuid>` and are never reaped. Just `remove_dir_all` after the record write succeeds.
- **L2** `refresh` with no key aborts on the first failure. Record per-key errors and return the list.
- **L3** `plugin add ./typo` is routed to the marketplace resolver. Treat a source starting with `./`, `../`, `~` or `/` as a path and report the missing directory.
- **L4** Reject `file://` for plugin `:SOURCE:` entries (keep it for marketplace URLs, tests need it). One line in `PLUGINS-SCOPE.md` section 12 saying `file://` marketplace URLs are a test affordance.
- **L5** `copy_plugin_tree` in core must skip `.orgasmic-install.org` like it skips `.git`, so a repo added directly by URL cannot forge marketplace provenance.
- **L6** `install` must pre-check the core-reserved id (`base.descriptor(&id).is_none()`) before the rename, so a reserved id fails the request instead of landing a permanently broken folder.
- **L7** Lowercase the host in `marketplace_key`.

## Verify

```sh
cargo fmt --all -- --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test -p orgasmic-core
cargo test -p orgasmic-daemon --test plugin_registry_routes --test marketplace_routes
cargo test -p orgasmic-cli -- --test-threads=1
```

Worktree-local `CARGO_TARGET_DIR` (`$PWD/target` here). No daemon. Report each item M1 to L7 with the file and test that covers it.
