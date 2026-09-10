# Changed

- Published completed attachment bytes atomically at each node's `attachments/<sha256>` path, preserving home-based upload staging and accepting an existing immutable revision.
- Added a shared hard-link publisher with EXDEV copy-to-destination-volume fallback and atomic no-overwrite publication.
- Served attachment content from the node folder and replaced the false backup recovery message with `attachment payload missing`.
- Counted stored quota bytes from `.orgasmic/*/*/attachments/<sha256>` instead of the legacy home blob directory.
- Added idempotent `.orgasmic/.gitattributes` preparation with `*/*/attachments/** filter=lfs diff=lfs merge=lfs -text` before normal ledger staging.
- Migrated referenced legacy home blobs into node attachment folders at boot, leaving legacy and unreferenced files in place and logging per-file failures plus the migrated count.
- Extended the route test through the real HTTP/boot path for node-local publication and serving, duplicate revision publication, one LFS rule, and legacy migration.
- Documented ledger/LFS attachment storage and the remote LFS requirement.

# Verification Gates

- `cargo fmt --all -- --check`: PASS (exit 0; `/tmp/task-v2859-fmt-final.log`).
- `cargo clippy -p orgasmic-daemon --all-targets -- -D warnings`: PASS (exit 0; `/tmp/task-v2859-clippy-final.log`).
- `cargo test -p orgasmic-daemon --test node_services_routes`: PASS (1 passed, 0 failed, 1 explicitly ignored 2 GiB smoke; `/tmp/task-v2859-node-services-routes-final.log`).
- `cargo test -p orgasmic-daemon --lib ledger_sync`: PASS (21 passed, 0 failed, 897 filtered; `/tmp/task-v2859-ledger-sync-final.log`).
- `git diff --check`: PASS.

# Unmet Criteria

None.

# Residual Risk

- The EXDEV fallback is compiled and follows the same atomic publication helper as the exercised hard-link path, but the focused tests run on one filesystem and do not force a cross-device copy.
- The explicit 2 GiB streaming smoke remained ignored as designed; no workspace-wide suite was run because the task specified focused daemon gates and the project gotcha forbids `cargo test --workspace` on this laptop.
- No deviations from the dispatch brief or write scope.
