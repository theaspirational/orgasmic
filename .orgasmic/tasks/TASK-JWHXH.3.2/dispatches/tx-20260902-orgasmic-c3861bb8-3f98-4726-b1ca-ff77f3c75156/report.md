# TASK-JWHXH.3.2 — `project migrate` on a non-git root: views cleanup only

## Changed
All in `crates/orgasmic-cli/src/project_migrate.rs` (single file, one commit `8c460413`):

- `run_at` (project_migrate.rs:154): detects the work tree ONCE at the top
  (`git_ok(root, ["rev-parse", "--is-inside-work-tree"])`) and shares the probe
  with `ViewsMigration::plan` (project_migrate.rs:157). The ledger-root
  "already migrated" early return is untouched and still runs first, so a
  non-git ledger root with `--to-branch` and nothing to do keeps answering
  "already migrated".
- Non-git gate (project_migrate.rs:166-183): on a non-git root the run does
  only the dec_XH2XY views cleanup (`views.apply`, skipped on `--dry-run`),
  then `bail!`s with the plain message "`<root>` is not a git work tree; the
  v1→v2 rewrite and --to-branch need a repository to recover from — init git
  or back up .orgasmic first" whenever the migration plan has anything to
  rewrite (`!old_files.is_empty()`) or `--to-branch` was passed. The v1 files
  are untouched by the refusal. Views-only runs keep their exact prior
  behavior (cleanup → "already migrated" or summary).
- `ViewsMigration::plan` (project_migrate.rs:66): now takes the shared
  `is_git_work_tree` bool instead of re-probing.
- `refuse_dirty_tree` (project_migrate.rs:237): the non-git early return is
  DELETED (brief preferred deletion; the new gate makes it dead). The call
  site is reached only for roots already probed to be git work trees.
- `print_summary` helper (project_migrate.rs:222): extracted from the inline
  summary block so both the non-git and git paths print identically;
  `views_summary_lines` kept as-is. The `--to-branch` tail stays inline in the
  git path (unreachable on non-git, which bails first).
- Git-repo behavior: unchanged — on git roots the flow is byte-for-byte the
  prior sequence (refuse_dirty_tree → views.apply → branch/rewrite/summary).

## Tests added
- `non_git_v1_root_deletes_views_then_refuses_rewrite_keeping_v1_files`
  (project_migrate.rs:861): non-git v1 fixture — migrate deletes
  `.orgasmic/views/`, refuses the rewrite with the plain message (no inert
  `git checkout` hints), and leaves `project.org` (version 1), all six
  `tasks/<state>.org` (byte-identical), `decisions.org` intact; no
  `tasks/TASK-A/` node dirs created. Also asserts one real-apply summary line
  via `views_summary_lines(&views_plan, true, false)` == `["  views directory
  removed"]` at the call site (the brief's cheap println-path coverage).
- `non_git_to_branch_refused_before_any_repo_change`
  (project_migrate.rs:921): non-git `--to-branch` — refused with the plain
  gate message, not `branch cutover …` and not git's own `fatal: not a git
  repository`, i.e. before any mutating git call; views cleanup still ran.

## Verification Gates
(each to a log file; cargo output never piped)
- `env -u ORGASMIC_RUN_ID -u ORGASMIC_HOME cargo test -p orgasmic-cli --bin
  orgasmic -- migrate doctor` → exit 0, `33 passed; 0 failed; 0 ignored;
  277 filtered out` — log: /tmp/task-jwhxh.3.2-test.log. Both new tests
  confirmed present and ok in the run (module path `project_migrate::` matches
  the `migrate` filter). Existing git fixtures all green, unchanged.
- `cargo clippy -p orgasmic-cli --all-targets -- -D warnings` → exit 0 — log:
  /tmp/task-jwhxh.3.2-clippy.log
- `cargo fmt --all --check` → exit 0 (after one `cargo fmt` application; the
  test+clippy gates were re-run on the post-fmt tree and stayed green) — log:
  /tmp/task-jwhxh.3.2-fmt.log

## Unmet Criteria
None. Both acceptance boxes are covered by the tests above plus the gates.

## Residual Risk
- Non-git v1 `--dry-run` now refuses instead of printing an unrunnable plan —
  a deliberate reading of the brief's unconditional gate wording ("if the
  migration plan has anything to rewrite or --to-branch was passed, bail!");
  dry runs change nothing, so this only sharpens the message an operator sees.
- `refuse_dirty_tree` is now correct only when called on a probed git root;
  it has exactly one production caller (`run_at`, gated) and the doc comment
  states the precondition.
- Mechanically side-effect-free outside the one file: no lockfiles, fixtures,
  or generated files touched.
