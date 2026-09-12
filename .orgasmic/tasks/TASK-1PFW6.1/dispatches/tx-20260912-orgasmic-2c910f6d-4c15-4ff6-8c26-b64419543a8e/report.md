# Changed

1. Bounded every marketplace git child in `crates/orgasmic-daemon/src/marketplaces.rs` with `tokio::process::Command`, `kill_on_drop(true)`, a 120-second timeout, disabled terminal prompts, SSH batch/connect timeout options, and the retained HTTP low-speed environment. Covered by `marketplaces::tests::command_output_times_out_a_sleeping_child`.
2. Added per-entry `error` results with null versions, continued browse past bad siblings, and removed browse writes to marketplace-level errors in `crates/orgasmic-daemon/src/marketplaces.rs` and its async API callers in `crates/orgasmic-daemon/src/api.rs`. Covered by `git_url_sources_are_cached_for_browse_install_and_update`.
3. Added `Home::marketplace_sources()` in `crates/orgasmic-core/src/home.rs` and moved source caches to `user/marketplace-sources/<key>/<id>` in `crates/orgasmic-daemon/src/marketplaces.rs`. Covered by the sibling-cache assertions in `git_url_sources_are_cached_for_browse_install_and_update`.
4. Rejected relative plugin sources whose first component starts with `.orgasmic` in `crates/orgasmic-core/src/marketplace.rs`. Covered by `marketplace::tests::parses_index_and_rejects_escaping_sources`.
5. Made git source cloning lazy on browse/install and limited refresh to already-cached sources; refresh now continues after a source failure, rolls an invalid source back, and reports its entry error in `crates/orgasmic-daemon/src/marketplaces.rs`. Covered by `git_url_sources_are_cached_for_browse_install_and_update` using local repositories only.
6. Deleted the duplicate manager encoder and imported `crate::daemon_client::path_segment` in `crates/orgasmic-cli/src/manager.rs`. Covered by `daemon_client::tests::path_segments_encode_query_and_fragment_bytes`; the manager import is also compiled by the CLI integration gates.
7. Rejected `file://` marketplace URLs with non-empty hosts other than `localhost` in `crates/orgasmic-core/src/marketplace.rs`. Covered by `marketplace::tests::keys_follow_git_url_paths`.

# Verification Gates

- `cargo fmt --all -- --check` — pass.
- `cargo clippy --workspace --all-targets -- -D warnings` — pass; `/tmp/task-1pfw6-clippy-retry.log`.
- `cargo test -p orgasmic-core` — pass: 217 tests passed, 2 ignored; `/tmp/task-1pfw6-core-final.log`.
- `cargo test -p orgasmic-daemon --test plugin_registry_routes --test marketplace_routes` — pass: 7 tests passed; `/tmp/task-1pfw6-daemon-gates.log`.
- `cargo test -p orgasmic-cli --test plugin_cli --test cli_parity -- --test-threads=1` — pass: 11 tests passed; `/tmp/task-1pfw6-cli-gates.log`.
- `cargo test -p orgasmic-daemon --lib command_output_times_out_a_sleeping_child` — pass; `/tmp/task-1pfw6-timeout-final.log`.
- `cargo test -p orgasmic-cli --bin orgasmic path_segments_encode_query_and_fragment_bytes` — pass; `/tmp/task-1pfw6-path-segment.log`.
- `git diff --check` — pass.
- All Cargo commands used worktree-local `CARGO_TARGET_DIR=/Users/aspirational/.orgasmic/worktrees/orgasmic/task-1pfw6.1/target`. No production daemon was started and `ui/` was untouched.

# Unmet Criteria

None.

# Residual Risk

The timeout is covered at the shared child wrapper rather than through a PATH-injected fake git end-to-end test, as explicitly permitted by the task. The local repository route test covers lazy clone failure, sibling continuation, cached refresh failure, rollback, and recovery without network access.
