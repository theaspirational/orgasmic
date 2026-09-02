# TASK-JWHXH.3.1 — doctor sees stale views on non-git projects; stale strings (narrow fix round)

Commit: c60bb97b `TASK-JWHXH.3.1: fix(cli): doctor warns on stale views in non-git projects; fix stale refusal string and migrate summary`

## Changed

- `crates/orgasmic-cli/src/doctor.rs:258` (`push_tracked_views_findings`, MEDIUM): `dir_present` is now computed for every registered project before any git probe; the `git ls-files` probe runs only when `is_git_work_tree` is true. A stale `.orgasmic/views/` on a non-git project now warns with the existing "still present … run: orgasmic project migrate" message. Doc comment updated (no longer says "git-repo project").
- `crates/orgasmic-cli/src/project_migrate.rs:219` (`refuse_dirty_tree`): early-returns when the root is not a git work tree. Necessary for acceptance: previously `git status` exited 128 on a non-git root and `project migrate` bailed "git status failed", so "after project migrate -> quiet" could never hold there (premise verified by live probe: `git status` in a plain dir → exit 128). Mechanical enablement of the stated criterion, not a behavior change for git repos.
- `crates/orgasmic-cli/src/project_migrate.rs:115` (`views_summary_lines`, new) + `:187` (call site): deleted the unreachable `else if views_applied` arm and stopped printing the PRE-apply `tracked.len()` on a real apply. Real apply now prints what is true afterwards: "views untracked and directory removed" (tracked+dir), "views untracked" (tracked only), "views directory removed" (dir only); dry run keeps the plan wording.
- `crates/orgasmic-daemon/src/api.rs:14705` (`reject_ledger_rewrite`, used by `post_org_file`): refusal for `.orgasmic/views/*` now says "views render on demand from tasks/decisions/glossary and are read-only" (the "view refresh operation" no longer exists).
- `shipped/schema/tx.org:292`: dropped the "with derived aggregate read views under `.orgasmic/views/`" clause.
- Tests: `crates/orgasmic-cli/src/project_migrate.rs:803` `non_git_project_views_doctor_warns_migrate_deletes_then_doctor_quiet` (fixture shape reused from `plain_branch_views_…`: non-git registered project with `.orgasmic/views/board.org` → doctor warns "still present" → `run_at` real apply deletes the dir → doctor quiet); `:831` `views_summary_reports_post_apply_state_not_pre_apply_counts` (asserts post-apply/dry-run summary lines for all three pre-states); `crates/orgasmic-daemon/src/api.rs:21412` asserts the new refusal text in `org_file_rewrite_refuses_ledger_paths` (previously only `is_err` was asserted; no test asserted the old text).

Off-limits surfaces untouched: api.rs touched only at the refusal string + its test; writer.rs, authz.rs, MEMBER_ALLOWED_ROUTES, ensure_actor_namespace_free, post_task_comment*, prepare_tx_append_request, post_tx all unchanged.

## Verification Gates

All logs in `/var/folders/9p/823z6j817xj9ts2xpvnx1q_40000gn/T/opencode/task-jwhxh.3.1-logs/`; no cargo output piped.

- PASS `cargo test -p orgasmic-cli --bin orgasmic -- doctor migrate` → exit 0, "31 passed; 0 failed" (includes both new tests) — `test-cli-doctor-migrate.log`
- PASS `cargo test -p orgasmic-daemon --lib -- org_file` → exit 0, "8 passed; 0 failed" (includes the refusal-text assertion) — `test-daemon-org_file.log`
- PASS `cargo clippy -p orgasmic-daemon -p orgasmic-cli --all-targets -- -D warnings` → exit 0 — `clippy.log`
- PASS `cargo fmt --all --check` → exit 0 — `fmt.log`
- Extra: PASS `cargo test -p orgasmic-core --test fixtures` → exit 0, "19 passed" incl. `parses_shipped_schema_files`, confirming the tx.org edit still parses — `test-core-fixtures.log`

## Unmet Criteria

None. Acceptance 1 covered by the new non-git fixture test; acceptance 2 by the api.rs text assertion + `views_summary_reports_post_apply_state_not_pre_apply_counts`; acceptance 3 by the four gates above.

## Residual Risk

- The migrate summary line content on a real apply is asserted via the extracted `views_summary_lines` unit test; the `println!` loop itself is exercised but its stdout is not captured (no in-process stdout-capture precedent in this crate, and staging ORGASMIC_HOME to run the real binary is banned by gotcha). Wiring is a two-line loop.
- `refuse_dirty_tree` on a non-git `to_branch` run now skips the dirty check and fails later inside `migrate_to_branch` with its own git error — that path requires git by definition; untested here (out of scope).
