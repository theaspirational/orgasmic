# TASK-JWHXH.3.2 — `project migrate` on a non-git root: cleanup only (narrow fix round)

Read `orgasmic task get --project orgasmic TASK-JWHXH.3.2` and `dec_XH2XY`. Line numbers are
approximate; read the current `crates/orgasmic-cli/src/project_migrate.rs`.

## The problem
`run_at` (~:154): `plan` → `ViewsMigration::plan` → `refuse_dirty_tree` (~:219, now returns
`Ok(())` on a non-git root) → `views.apply` → `migrate_to_branch` or `apply_with_recovery`.
On a NON-GIT root the destructive v1→v2 rewrite (`apply_with_recovery` ~:490) now runs with no
VCS, and its partial-apply context prints inert `git checkout`/`git clean` commands; `--to-branch`
reaches `create_orphan_branch` and dies with "failed before this run changed the repository"
although views were already deleted.

## The move
Detect the work tree ONCE at the top of `run_at` (`git_ok(root, ["rev-parse",
"--is-inside-work-tree"])`, same probe `ViewsMigration::plan` uses — share it). On a non-git
root: run the views cleanup (plan/apply/summary as today), then if the migration plan has
anything to rewrite or `--to-branch` was passed, `bail!` with a plain message ("<root> is not a
git work tree; the v1→v2 rewrite and --to-branch need a repository to recover from — init git
or back up .orgasmic first"), leaving the v1 files untouched. `refuse_dirty_tree`'s early
return can then go back to being unreachable or stay — prefer deleting it if the new gate
makes it dead. Git-repo behaviour must not change.

## Tests
- Non-git v1 fixture (reuse the v1 fixture the existing `apply_with_recovery` tests build):
  migrate deletes `.orgasmic/views/`, refuses the rewrite with the message, v1 files intact.
- Non-git `--to-branch`: refused before any git call.
- Existing git fixtures unchanged. If cheap, assert one real-apply summary line by capturing
  the lines from `views_summary_lines` at the call site rather than stdout.

OFF LIMITS (TASK-KA934.3.2 runs in parallel): `crates/orgasmic-cli/src/member.rs`,
`crates/orgasmic-daemon/**`, `crates/orgasmic-core/src/members.rs`; in `doctor.rs` touch nothing.

## Gates (each to a log file, never pipe cargo output)
- `cargo test -p orgasmic-cli --bin orgasmic -- migrate doctor` (targeted; NEVER unfiltered)
- `cargo clippy -p orgasmic-cli --all-targets -- -D warnings`
- `cargo fmt --all --check`

## Rules
- Work only in your worktree; one commit `TASK-JWHXH.3.2: fix(cli): <one line>`.
- NEVER `cargo test --workspace`; NEVER the whole `orgasmic-cli` crate; NEVER set
  `ORGASMIC_HOME`; NEVER run `daemon start`; never touch the live ledger at
  `~/.orgasmic/ledgers/orgasmic`; `git rm`/`remove_dir_all` only inside temp fixtures; never
  run `legacy_drivers_and_explicit_pairs_emit_equivalent_start_events`.
- Report: what changed (`file:line`), each gate with its pass/fail line and log path, unmet
  criteria, residual risk. Finish with `orgasmic dispatch finalize --summary-file <path>`
  (report only, no `--commit`).
