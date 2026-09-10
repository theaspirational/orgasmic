# Review brief: TASK-V2859 implementer generation tx-20260910-orgasmic-652035d8-4630-4356-a86b-c992e3bc54ac

Review worker commit `25ccfd31b9c0ed21f6d958c788f8190db98060af` (your worktree HEAD) against its base `6836bc8d`:
`git diff 6836bc8d..25ccfd31`. Author harness: codex (gpt-5.6-sol, high).

## What it claims
Implements dec_X2046: attachment blobs move from `~/.orgasmic/assets/<hash>/blobs/<sha256>` to `<node dir>/attachments/<sha256>` in the ledger, tracked by Git LFS via a daemon-written `.orgasmic/.gitattributes`. Boot migration links legacy blobs in. Quota scan counts the new location. Read `orgasmic task get TASK-V2859` acceptance criteria and `orgasmic decision get dec_X2046`.

## Look hard at
1. `ledger_sync.rs`: is `.orgasmic/.gitattributes` written on every path that stages a normal commit with a remote configured? (`stage_ledger` with `index=None` vs `Some`.) Could a first push ever carry a blob outside LFS?
2. `lib.rs::publish_blob`: atomicity and the EXDEV copy fallback; the temp-file name is `<target>.tmp.<uuid>` inside the node folder — does the LFS attribute or the ledger `git add --all` ever see that temp file? Any race with the sync loop staging a half-written copy?
3. `node_services.rs`: quota walk over `<ledger>/*/*/attachments/*` — cost and correctness (`valid_digest`). `attachment()` path resolution. `get_content` size check still holds.
4. Migration: idempotent? Runs before or after the ledger worktree is prepared at boot? Failure on one file cannot fail boot?
5. Tests: does `exercise()` actually prove the five claims (node-local blob, served from there, duplicate revision OK, one LFS rule, migrated legacy blob)? Anything asserted only on the idle (no-remote) sync path?
6. MSRV / clippy / fmt; commit hygiene.

## Gates you may run (own target dir; never start a daemon; never `cargo test --workspace`)
- `cargo test -p orgasmic-daemon --test node_services_routes --target-dir target`
- `cargo test -p orgasmic-daemon --lib ledger_sync --target-dir target`

## Output
Finish with `orgasmic dispatch finalize --task TASK-V2859 --summary-file <report>`. Report: findings (bugs first, each with file:line and a concrete failure), then a one-word verdict line `VERDICT: approve | approve-with-follow-ups | reject`.
