# Changed

- Added strict `MarketplaceIndex::parse` / `read_dir`, URL-to-key derivation, source containment checks, and the shared no-symlink/no-`.git` plugin copier in core.
- Added the shipped official marketplace and user `marketplaces.org` override storage, shallow clone / fast-forward refresh, official disable-only behavior, marketplace browsing, install provenance, install, and update in the daemon.
- Added authenticated marketplace and plugin install/update routes. All mutations remain admin-only; list/browse are available to signed-in members.
- Extended plugin status with project-specific recommendations for uninstalled owners whose collection data exists. Marketplace updates leave activation records intact, while the existing approved-capability subset gate disables grown capability sets until re-approval.
- Added `orgasmic marketplace add|list|remove|refresh|enable|disable`, marketplace-aware `orgasmic plugin add <marketplace>/<id>`, `orgasmic plugin update <id>`, and the matching shipped skill reference.
- Added local-bare-git route coverage for add/list/refresh/browse/install/update, duplicate install refusal, member mutation refusal, official removal refusal, activation retention, capability-growth re-approval, recommendation, and escaping `:SOURCE:` rejection. No network repository is used by these tests.

# Verification Gates

- `cargo fmt --all -- --check` — passed.
- `cargo clippy --target-dir "$PWD/target" --workspace --all-targets -- -D warnings` — passed.
- `cargo test --target-dir "$PWD/target" -p orgasmic-core` — passed: 194 unit tests, 2 descriptor-state tests, 20 fixture tests; 2 ignored; doc tests passed.
- `cargo test --target-dir "$PWD/target" -p orgasmic-daemon --test plugin_registry_routes --test marketplace_routes` — passed: 4 tests.
- `cargo test --target-dir "$PWD/target" -p orgasmic-cli -- --test-threads=1` — passed: full CLI unit and integration suite, including 9 CLI parity tests; 1 existing ignored test.
- Default-parallel `cargo test --target-dir "$PWD/target" -p orgasmic-cli` attempts each reached unrelated pre-existing concurrency-sensitive failures (`repair_task_ids_refuses_dirty_lock_symlink_claim_descendant_and_edits`, then `dispatch_finalize_from_subprocess_stream_json_mode`); each failed test passed immediately in isolation, and the complete serialized suite passed.
- `git diff --check` — passed.

# Unmet Criteria

- None.

# Residual Risk

- Git credential and remote-host behavior is delegated to the host `git` process as specified; automated coverage intentionally uses only local `file://` bare repositories.
