# Changed

- M1 — `crates/orgasmic-cli/src/daemon_client.rs`, `marketplace.rs`, and `plugin.rs`: marketplace add/refresh, marketplace plugin install, and plugin update use a shared 300-second per-request timeout. Covered by `daemon_client::tests::marketplace_requests_outlast_the_default_client_window` and compilation of both CLI call sites.
- M2 — `crates/orgasmic-daemon/src/marketplaces.rs`, `api.rs`, and `plugins.rs`: all marketplace Git commands run in `spawn_blocking` with Git HTTP low-speed bounds; install/update prepare the tree before the plugin write guard, then reserve-check, rename, and reconcile under the guard. Covered functionally by `marketplace_routes_cover_refresh_install_update_auth_and_recommendation` plus the plugin UI route suite; guard ordering is structural and clippy-checked.
- M3 — `crates/orgasmic-daemon/src/marketplaces.rs`: browse records a bad marketplace error and continues; refresh captures HEAD and resets after an invalid pulled index; refresh-all records per-key failures and continues. Covered by `bad_marketplace_indexes_do_not_break_browse_or_refresh_all`.
- M4 — `crates/orgasmic-daemon/src/marketplaces.rs`: refreshed Git-URL plugin sources are shallow-cloned into `<marketplace>/.orgasmic-sources/<id>`, validated, atomically replaced, and used for browse/install/update. Covered without network by `git_url_sources_are_cached_for_browse_install_and_update`, using a second local bare repo through a test-local Git URL rewrite.
- M5 — `crates/orgasmic-daemon/tests/marketplace_routes.rs`: the member token now loops over add, refresh-all, refresh-one, remove, and plugin update and receives 403 for each; the existing activation/install checks complete all seven mutations.
- M6 — `crates/orgasmic-cli/src/marketplace.rs` now reuses `daemon_client::path_segment`; `crates/orgasmic-core/src/marketplace.rs` rejects `?`/`#` in URL paths. Covered by `path_segments_encode_query_and_fragment_bytes` and `keys_follow_git_url_paths`.
- L1 — marketplace removal deletes the clone after the registry record write. Covered by `marketplace_routes_cover_refresh_install_update_auth_and_recommendation`.
- L2 — refresh-all returns statuses with per-key errors instead of aborting. Covered by `bad_marketplace_indexes_do_not_break_browse_or_refresh_all`.
- L3 — explicit `./`, `../`, `~`, and `/` plugin sources remain paths and report a missing directory. Covered by `plugin::tests::explicit_relative_and_home_paths_are_not_marketplace_ids`.
- L4 — plugin index entries reject `file://`; `PLUGINS-SCOPE.md` documents file marketplace URLs as a test affordance. Covered by `marketplace::tests::parses_index_and_rejects_escaping_sources`.
- L5 — `copy_plugin_tree` drops `.orgasmic-install.org`. Covered by `plugin::tests::copying_a_plugin_drops_repository_and_install_metadata`.
- L6 — the daemon rejects core-reserved plugin IDs before publish. Covered by `reserved_core_plugin_id_is_rejected_before_publish`.
- L7 — marketplace URL hosts are normalized to lowercase, including hosted file URLs. Covered by `marketplace::tests::keys_follow_git_url_paths`.

# Verification Gates

- `cargo fmt --all -- --check` — green (`/tmp/task-1pfw6-fmt-final.log`).
- `cargo clippy --workspace --all-targets -- -D warnings` — green (`/tmp/task-1pfw6-clippy-final.log`).
- `cargo test -p orgasmic-core` — green: 217 passed, 2 ignored across targets (`/tmp/task-1pfw6-core.log`).
- `cargo test -p orgasmic-daemon --test plugin_registry_routes --test marketplace_routes` — green: 7 passed (`/tmp/task-1pfw6-daemon-routes.log`).
- `cargo test -p orgasmic-cli -- --test-threads=1` — the unit phase and preceding integration binaries were green, but `dispatch_finalize_from_subprocess_stream_json_mode` failed inside the 98-test dispatch batch with `no live run found`; it passed immediately in exact serial isolation (`/tmp/task-1pfw6-cli-r3.log`, `/tmp/task-1pfw6-cli-dispatch-rerun.log`). The first attempt with an exported absolute `CARGO_TARGET_DIR` also exposed the pre-existing nested-worktree target-path incompatibility; unsetting only the exported variable leaves Cargo's outer target at this worktree's default `$PWD/target` and made that exact test green.
- `cargo test -p orgasmic-cli --test plugin_cli -- --test-threads=1` — green: 2 passed (`/tmp/task-1pfw6-cli-plugin.log`).
- `git diff --check` — green.

# Unmet Criteria

- No requested marketplace behavior is unmet.
- The aggregate CLI command did not produce one all-green invocation because of the unrelated dispatch finalize batch-only failure above; the exact failing test is green in isolation and its source/runtime paths are unchanged from `5c7a8ad0`.

# Residual Risk

- There is no timing fault-injection test for a hung Git child or concurrent plugin UI read. The implementation uses the explicitly permitted Git HTTP low-speed bound and structurally acquires `plugins.operations` only after staging.
- Non-HTTP Git transports rely on their own transport behavior; the requested bound specifically permitted the Git HTTP low-speed settings used here.
