# TASK-JWHXH.3.1 — doctor sees stale views on non-git projects; stale strings (narrow fix round)

Read `orgasmic task get --project orgasmic TASK-JWHXH.3.1` and `dec_XH2XY`. Line numbers are
approximate; read the current files.

## The move (MEDIUM)
`crates/orgasmic-cli/src/doctor.rs` ~:254 `push_tracked_views_findings`:
`if !is_git_work_tree(&root) { continue; }` runs before `dir_present` is checked, so a
registered NON-git project keeps a stale, never-refreshed `.orgasmic/views/` with no warning.
Fix: compute/report `dir_present` for every registered project; run the `git ls-files`
probe only inside a work tree. The existing "still present" message already fits.

## LOWs (same round)
- `crates/orgasmic-daemon/src/api.rs` ~:14705 (`reject_ledger_rewrite`, used by
  `post_org_file` for `.orgasmic/views/*`): the message still says "regenerate it through
  the view refresh operation" — that operation no longer exists. Say the views are rendered
  on demand from tasks/decisions/glossary and are read-only. Update the test that asserts
  the text.
- `crates/orgasmic-cli/src/project_migrate.rs` ~:155/~:170: the `else if views_applied` arm
  is unreachable (an earlier arm always fires) — delete it; and the summary prints the
  PRE-apply `tracked.len()` on a real apply — print what is true after apply.
- `shipped/schema/tx.org` ~:294: drop the "derived aggregate read views under
  `.orgasmic/views/`" line.

## Tests
- Non-git registered project with `.orgasmic/views/board.org` present: doctor warns; after
  `project migrate` (dir deleted) doctor is quiet. Reuse the fixture shape from
  `plain_branch_views_doctor_warns_migrate_untracks_then_doctor_quiet`.

OFF LIMITS (TASK-KA934.3.1 runs in parallel): `api.rs` `MEMBER_ALLOWED_ROUTES`,
`ensure_actor_namespace_free`, `post_task_comment*`, `prepare_tx_append_request`, `post_tx`;
`writer.rs`; `authz.rs`. Touch `api.rs` ONLY at the refusal string + its test.

## Gates (each to a log file, never pipe cargo output)
- `cargo test -p orgasmic-cli --bin orgasmic -- doctor migrate` (targeted; NEVER unfiltered)
- `cargo test -p orgasmic-daemon --lib -- org_file`
- `cargo clippy -p orgasmic-daemon -p orgasmic-cli --all-targets -- -D warnings`
- `cargo fmt --all --check`

## Rules
- Work only in your worktree; one commit `TASK-JWHXH.3.1: fix(cli): <one line>`.
- NEVER `cargo test --workspace`; NEVER the whole `orgasmic-cli` crate; NEVER set
  `ORGASMIC_HOME`; NEVER run `daemon start`; never touch the live ledger at
  `~/.orgasmic/ledgers/orgasmic`; `git rm`/`remove_dir_all` only inside temp fixtures; never
  run `legacy_drivers_and_explicit_pairs_emit_equivalent_start_events`.
- Report: what changed (`file:line`), each gate with its pass/fail line and log path, unmet
  criteria, residual risk. Finish with `orgasmic dispatch finalize --summary-file <path>`
  (report only, no `--commit`).
