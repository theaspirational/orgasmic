# Changed

- `crates/orgasmic-daemon/src/writer.rs:40-52,1450-1504,1643-1665,1776-1793` — introduced a member/admin mutation actor, enforced authorship inside the locked writer transform, and exposed a typed authorship refusal for HTTP 403 mapping.
- `crates/orgasmic-daemon/src/api.rs:1776-1781,2393-2469` — derived the mutation actor from the authenticated identity, stamped admin mutations with the daemon actor, and mapped non-author mutations to 403.
- `crates/orgasmic-core/src/node_kernel.rs:256-317,378-410` — edits now upsert `EDITED_BY`/`EDITED_AT`; tombstones now upsert `DELETED_BY`/`DELETED_AT`, retain `comment.deleted`, drop prose, and parse back through the journal parser.
- `crates/orgasmic-daemon/src/api.rs:38183-38373` — covered member-own edit/delete, non-author edit/delete 403 with byte-identical journal, admin edit/delete, automated `reviewer.finding` refusal, and both actor/time stamp pairs.
- `ui/src/components/TaskDialog.tsx:883-887,921-935` — retained Reply for commenters while hiding Edit/Delete unless the current identity is admin or the row actor matches the current member.
- Commit: `71ecc0dc689d93de3a24999b6f9842bc835e434f` (`TASK-KA934.1: fix(daemon): enforce comment authorship and audit mutations`).

# Verification Gates

- PASS — `cargo test -p orgasmic-core --lib node_kernel`: `4 passed; 0 failed`; log `/tmp/TASK-KA934.1/core-test-final.log` (PID record `/tmp/TASK-KA934.1/core-test-final.pid`).
- PASS — `cargo test -p orgasmic-daemon --lib -- comment`: `18 passed; 0 failed`; log `/tmp/TASK-KA934.1/daemon-comment-test-final2.log` (PID record `/tmp/TASK-KA934.1/daemon-comment-test-final2.pid`).
- PASS — `cargo clippy -p orgasmic-core -p orgasmic-daemon --all-targets -- -D warnings`: finished successfully; log `/tmp/TASK-KA934.1/clippy-final.log` (PID record `/tmp/TASK-KA934.1/clippy-final.pid`).
- PASS — `cargo fmt --all --check`: exit 0; log `/tmp/TASK-KA934.1/fmt-check.log` (PID record `/tmp/TASK-KA934.1/fmt-check.pid`).
- PASS — `cd ui && npm ci && npm run typecheck`: `tsc --noEmit` exit 0; log `/tmp/TASK-KA934.1/ui-typecheck.log` (PID record `/tmp/TASK-KA934.1/ui-typecheck.pid`).
- PASS (focused UI check) — `cd ui && npm test -- src/components/__tests__/TaskActivityRail.test.tsx`: `5 passed`; log `/tmp/TASK-KA934.1/ui-task-activity-test.log` (PID record `/tmp/TASK-KA934.1/ui-task-activity-test.pid`).
- PASS — `git diff --check`; worktree clean after commit.

# Unmet Criteria

- None within the dispatched writer/handler/kernel/UI scope.

# Residual Risk

- The existing `MEMBER_ALLOWED_ROUTES` allowlist does not include task-comment POST/edit/delete routes, so a real member HTTP request is currently rejected by coarse middleware before these handlers. The specified direct-handler fixture proves the new writer authorization and stamps, but this dispatch did not widen the unrelated member route allowlist.
- Automated journal entry mutation remains a no-write refusal through the existing kernel type check and generic writer-error mapping (HTTP 500); this task preserved that behavior rather than broadening error taxonomy.
- No live browser/daemon clickthrough was run; focused UI tests and typecheck cover the touched UI path.
