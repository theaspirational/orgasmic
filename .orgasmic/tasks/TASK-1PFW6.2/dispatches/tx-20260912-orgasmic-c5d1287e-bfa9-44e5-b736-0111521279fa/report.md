# Make marketplace cache refreshes safe and read-only

## Changed

1. `plugins()` is read-only for git sources, reports the required uncached error, documents its `operations` lock, and `GET /plugins` now performs one marketplace browse per request. Covered by `marketplace_routes_cover_refresh_install_update_auth_and_recommendation` and `git_url_sources_are_cached_for_browse_install_and_update`.
2. Browse reads a valid cached manifest even when the last refresh recorded an error, preserving its version and capabilities alongside that error. Covered by `git_url_sources_are_cached_for_browse_install_and_update`.
3. Git-source refresh clones into a sibling temp directory and swaps only after validation; a marketplace pull failure also fresh-clones and swaps. Stale source and marketplace `index.lock` cases are covered by `git_url_sources_are_cached_for_browse_install_and_update`.
4. Git commands run in their own process group and timeout kills and reaps the entire group. Covered by `marketplaces::tests::command_output_times_out_a_sleeping_process_group`, which verifies both shell and child PIDs are gone.
5. Source caches now use `marketplace-sources/<key>/.sources/<id>`; removal targets only that fixed cache segment. Covered by `git_url_sources_are_cached_for_browse_install_and_update`.
6. Refresh responses include `sources: [{id, ok, error}]`, including HTTP success with every source reported failed. The existing CLI JSON renderer prints this response without another output path. Covered by `git_url_sources_are_cached_for_browse_install_and_update`; CLI regression coverage is `plugin_cli` and `cli_parity`.
7. Refresh prunes source IDs absent from the current index, and first marketplace access removes legacy `<clone>/.orgasmic-sources/`. Covered by `git_url_sources_are_cached_for_browse_install_and_update` and `bad_marketplace_indexes_do_not_break_browse_or_refresh_all`.
8. `file://localhost/x` and `file:///x` now produce the same marketplace key. Covered by `marketplace::tests::keys_follow_git_url_paths` in `orgasmic-core`.

Files changed: `crates/orgasmic-core/src/marketplace.rs`, `crates/orgasmic-daemon/src/api.rs`, `crates/orgasmic-daemon/src/marketplaces.rs`, and `crates/orgasmic-daemon/tests/marketplace_routes.rs`. No UI or ledger files changed.

## Verification Gates

- `cargo fmt --all -- --check` — passed.
- `cargo clippy --workspace --all-targets -- -D warnings` — passed; log `/tmp/task-1pfw6.2-clippy-final.log`.
- `cargo test -p orgasmic-core` — passed; log `/tmp/task-1pfw6.2-core-final.log`.
- `cargo test -p orgasmic-daemon --test plugin_registry_routes --test marketplace_routes` — 7 passed; log `/tmp/task-1pfw6.2-daemon-tests-final.log`.
- `cargo test -p orgasmic-daemon --lib marketplaces` — 1 passed; log `/tmp/task-1pfw6.2-daemon-lib-gate.log`.
- `cargo test -p orgasmic-cli --test plugin_cli --test cli_parity -- --test-threads=1` — 11 passed; log `/tmp/task-1pfw6.2-cli-final.log`.
- All cargo gates used the worktree-local `CARGO_TARGET_DIR=/Users/aspirational/.orgasmic/worktrees/orgasmic/task-1pfw6.2/target` and recorded owning shell PIDs in matching `/tmp/task-1pfw6.2-*.pid` files.

## Unmet Criteria

None.

## Residual Risk

None identified within the requested Rust-only scope. The existing CLI pretty-prints the additive refresh response; no separate human renderer or UI change was introduced.
