## Changed

- `crates/orgasmic-daemon/src/writer.rs:1651-1679,1807-1838` adds path-free typed comment OCC/not-found errors; `crates/orgasmic-daemon/src/api.rs:1777-1788` maps them to HTTP 409/404 while retaining 403 authorship handling.
- `crates/orgasmic-daemon/src/index.rs:147-163,4351-4387` projects edit/delete audit stamps and retains `comment.deleted` journal entries as empty-body comment activity rows, preserving reply anchors.
- `ui/src/lib/types.ts:160-172` carries the four optional audit stamps; `ui/src/components/TaskDialog.tsx:887-905,1042-1044` renders one-line tombstones without Reply/Edit/Delete and marks edited comments.
- Tests pin stale edit/delete 409 responses and unchanged journal bytes, exact authorship refusal messages, activity tombstone stamps, sequential edit stamp replacement, and tombstone UI actions (`crates/orgasmic-daemon/src/api.rs:38424-38612`, `crates/orgasmic-core/src/node_kernel.rs:381-419`, `ui/src/components/__tests__/TaskActivityRail.test.tsx:121-146`).
- Commit: `8967bd4e` (`TASK-KA934.1.1: fix(comments): return conflicts and tombstones`). Worktree clean. `npm ci` populated ignored `ui/node_modules`; no lockfile or generated source changed.

## Verification Gates

- PASS — `cargo test -p orgasmic-daemon --lib -- comment activity`: `test result: ok. 22 passed; 0 failed; 0 ignored; 0 measured; 810 filtered out`. Log `/tmp/TASK-KA934.1.1-daemon-comment-activity-final.log`; PID record `/tmp/TASK-KA934.1.1-daemon-comment-activity-final.pid`.
- PASS — `cargo test -p orgasmic-core --lib node_kernel`: `test result: ok. 3 passed; 0 failed; 0 ignored; 0 measured; 178 filtered out`. Log `/tmp/TASK-KA934.1.1-core-node-kernel.log`; PID record `/tmp/TASK-KA934.1.1-core-node-kernel.pid`.
- PASS — `cargo clippy -p orgasmic-core -p orgasmic-daemon --all-targets -- -D warnings`: `Finished dev profile ... in 7.60s`. Log `/tmp/TASK-KA934.1.1-clippy-final.log`; PID record `/tmp/TASK-KA934.1.1-clippy-final.pid`.
- PASS — `cargo fmt --all --check`: `exit=0`. Log `/tmp/TASK-KA934.1.1-fmt-final.log`; PID record `/tmp/TASK-KA934.1.1-fmt-final.pid`.
- PASS — `cd ui && npm run typecheck`: `tsc --noEmit`, exit 0. Final log `/tmp/TASK-KA934.1.1-ui-typecheck-final.log`; PID record `/tmp/TASK-KA934.1.1-ui-typecheck-final.pid`. The first attempt used global TypeScript 6.0.2 because dependencies were absent and failed on the pre-existing `baseUrl` deprecation; `npm ci` restored the locked local toolchain, after which both reruns passed. Initial evidence: `/tmp/TASK-KA934.1.1-ui-typecheck.log`; install log: `/tmp/TASK-KA934.1.1-ui-npm-ci.log`.
- PASS supplemental — `cd ui && npm test -- --run src/components/__tests__/TaskActivityRail.test.tsx`: `1 passed`, `6 passed`. Log `/tmp/TASK-KA934.1.1-ui-activity-test.log`; PID record `/tmp/TASK-KA934.1.1-ui-activity-test.pid`.

## Unmet Criteria

- None.

## Residual Risk

- No live browser clickthrough was required or run; tombstone rendering and action suppression were checked in jsdom plus TypeScript typecheck.
