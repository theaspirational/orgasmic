# Review: TASK-JWHXH.3.2 — `project migrate` on a non-git root

Commit `8c460413`, merged `82ed5a9d`. One file, `crates/orgasmic-cli/src/project_migrate.rs`, +135/-20.

## Verdict

**APPROVE WITH FOLLOW-UPS.** Both acceptance criteria are met, the MEDIUM from the
JWHXH.3.1 review is genuinely closed (the destructive v1→v2 rewrite and `--to-branch`
can no longer be reached on a non-git root), and the git path is byte-for-byte
unchanged. Four LOWs remain, all in the operator-facing message/output surface and one
in test quality. None blocks ship.

## Findings

### LOW (bug/usability) — `project_migrate.rs:169-176`: the refusal is silent about the deletion it just did
On a non-git v1 root the real run calls `views.apply(root)?` (deleting `.orgasmic/views/`),
then `bail!`s. `print_summary` at :181 is never reached, so the `views directory removed`
line is never printed, and the error text says nothing about it.

Symptom: `orgasmic project migrate` on a non-git v1 project exits non-zero with only
`... is not a git work tree; ...`. The operator reasonably reads a failed command as "nothing
happened", but `.orgasmic/views/` is gone. The new test at :862 proves this — it asserts
`unwrap_err()` and `!dotorg.join("views").exists()` in the same test.

Not data loss: per `dec_XH2XY` views are derived and their removal is the goal. It is
purely under-reporting. Fix direction: append the applied views lines to the bail context,
or print the summary before bailing.

### LOW (usability) — `project_migrate.rs:170`: "init git" is not actually sufficient recovery
The bail text tells the operator to "init git or back up `.orgasmic` first". `refuse_dirty_tree`
(:243) runs `git status --porcelain=v1 -z --untracked-files=all -- ':(exclude).orgasmic/views'`
and bails on any output. After a bare `git init` every file is untracked, so the next run
fails again with `refusing to migrate a dirty git tree`.

Probe (throwaway temp repo, `/tmp`):

    T=$(mktemp -d); mkdir -p "$T/.orgasmic"; echo x > "$T/.orgasmic/project.org"
    cd "$T" && git init -q . && git status --porcelain=v1 --untracked-files=all -- ':(exclude).orgasmic/views'
    # => ?? .orgasmic/project.org      (non-empty => migrate bails "dirty git tree")

Fix direction: say `git init && git add -A && git commit`, or drop the git suggestion and
keep only "back up `.orgasmic` first".

### LOW (usability) — `project_migrate.rs:168-176`: `--dry-run` on a non-git v1 root lost its output
Old flow: non-git + `--dry-run` + v1 files printed the `DRY RUN` summary and the views plan,
exit 0 (`refuse_dirty_tree` early-returned, `apply_with_recovery` was `dry_run`-guarded).
New flow: `views_applied = false` (correctly no mutation), then `!migration.old_files.is_empty()`
→ `bail!`. The read-only inspection path now exits non-zero and prints no plan at all.

Correct in that it does not mutate, and the refusal is honest, but it hides the views plan the
operator ran `--dry-run` to see. Implementer-disclosed. Sizing it: LOW — `--dry-run` on a
non-git v1 root is a narrow case, and the refusal is the more actionable message. Fix
direction (optional): on `dry_run`, print the summary and the refusal as a warning line rather
than an `Err`.

### LOW (test) — `project_migrate.rs:906-910`: the "summary line" assertion does not test the call site
The dispatch brief claims the new test asserts the "summary line at the call site". It does not:

    let views_plan = ViewsMigration::plan(&root, false).unwrap();     // :890, before run_at
    ...
    assert_eq!(views_summary_lines(&views_plan, true, false),
               vec!["  views directory removed".to_string()]);        // :906-910

That is a pure-helper assertion on a hand-built plan, duplicating what
`views_summary_reports_post_apply_state_not_pre_apply_counts` (:951) already covers. Because
`run_at` bails before `print_summary` on this path, the assertion passes whether or not the
line is ever printed — which is exactly the LOW above. The assignment's "cover the println
path once in the real-apply test if cheap" is only incidentally satisfied, by the pre-existing
`non_git_project_views_doctor_warns_migrate_deletes_then_doctor_quiet` (:830, v2 + no
`--to-branch`), which executes `print_summary` but asserts nothing about its output.

Minor: `non_git_to_branch_refused_before_any_repo_change` (:923) is misnamed — a repo change
(the views deletion) does happen before the refusal; its own `assert!(!dotorg.join("views").exists())`
says so. "before any git call" would be accurate.

## What I verified (and how)

- **Git-path parity — CONFIRMED, byte-for-byte.** Diffed `run_at` against
  `git show 82ed5a9d^1:crates/orgasmic-cli/src/project_migrate.rs`. Order is unchanged:
  `plan` → work-tree probe → `ViewsMigration::plan` → ledger-root `already migrated` early
  return → `refuse_dirty_tree` → `views.apply` → branch/rewrite → summary → `--to-branch` tail.
  The extracted `print_summary` body is character-identical to the block it replaced,
  including the `views_summary_lines` loop. The ledger-root early return (:158-165) still
  precedes everything, including the new non-git branch.
- **Single probe — CONFIRMED.** `git_ok(root, &["rev-parse", "--is-inside-work-tree"])` now
  appears once at :156, threaded into `ViewsMigration::plan(root, is_git_work_tree)`. Previously
  the probe lived inside `plan`; same call point, same result.
- **`refuse_dirty_tree` precondition — CONFIRMED.** `grep -n refuse_dirty_tree` over the file:
  definition at :243, exactly one call at :184, inside the git branch. No test calls it. The
  deleted early return is genuinely unreachable, and nothing can now hit git's rc-128 path.
- **No git call on the non-git branch — CONFIRMED by reading.** `ViewsMigration::plan(root, false)`
  forces `tracked = Vec::new()`, so `apply` (:87) skips the `git rm --cached` arm and only does
  `remove_dir_all`. `is_ledger_root` (:532) is pure `std::fs::canonicalize`, no git.
- **Nothing else moved — CONFIRMED.** One file; every hunk maps to one of the claimed bullets
  (`plan` signature, the non-git branch, `print_summary` extraction, the `refuse_dirty_tree`
  early return + comment, two new tests).
- **Acceptance criterion 1 — met.** `non_git_v1_root_deletes_views_then_refuses_rewrite_keeping_v1_files`
  (:862) asserts views deleted, the plain message (`is not a git work tree`,
  `init git or back up .orgasmic first`, and negatively `!contains("git checkout")` — the inert
  recovery hint the MEDIUM was about), and byte-identical `project.org` / `tasks/todo.org`,
  plus `!tasks/TASK-A` and `decisions.org` still present.
- **Acceptance criterion 1b — met.** `non_git_to_branch_refused_before_any_repo_change` (:923)
  asserts the refusal, negatively `!contains("branch cutover")` and `!contains("not a git repository")`
  (git's own rc-128 wording), i.e. it never reached `create_orphan_branch`.
- **Message honesty — see LOW #1 and #2.** The text is plain and mentions no git commands, so
  the original MEDIUM (inert git hints) is fixed. The recovery advice is incomplete and the
  views deletion is unmentioned.

## What I did NOT check

- **`clippy -D warnings` and `cargo fmt`** — not re-run. The brief marks them established
  (implementer + manager on merged `82ed5a9d`); the diff introduces no new lint-prone
  construct beyond one extracted 4-arg function.
- **The live daemon on `:4848`** — not probed, per the brief (old runtime).
- **A real-binary end-to-end probe** of `orgasmic project migrate` against a hand-built non-git
  v1 fixture. `run_at` is the whole production path below `run` (which only adds
  `find_project_root`), and the unit tests call `run_at` directly, so the control flow above is
  read off the same code the binary executes. The one thing a binary probe would add over
  reading is exit-code confirmation for the `--dry-run` LOW; `bail!` → `Err` → non-zero is not
  in doubt.
- **`verify/*/injection.patch`** — not read, per the brief.
- **The daemon-side views cleanup** (`dec_XH2XY`'s "synced ledgers clean their own dir") — out
  of scope for this diff, untouched by it.

## Test evidence

`cargo test -p orgasmic-cli --bin orgasmic -- migrate doctor` — see the "Gates" section below.

## Open questions

1. Is the `--dry-run` refusal (LOW #3) the intent, or should a non-git dry run still print the
   plan it would have refused? The assignment says "refuse ... up front", which the implementer
   read as covering dry runs too. Defensible; worth an explicit operator call if `--dry-run` is
   documented as always-informative.
2. Should the views cleanup run at all on a non-git root when the command is about to fail
   (`--to-branch`)? Currently it does. It is the desired end state per `dec_XH2XY` and the
   acceptance criterion says "deletes views", so I read this as intended — but the pairing of
   "mutation happened" with "exit non-zero, silent" is the substance of LOW #1.

## Fix directions (follow-ups, none blocking)

- Print the views summary (or fold the applied lines into the bail context) before the non-git
  `bail!` at :170, so the deletion is reported. Smallest form: move `print_summary(...)` above
  the `if to_branch || !migration.old_files.is_empty()` block, guarded on `views_applied`.
- Correct the recovery advice at :170 to `git init && git add -A && git commit`, or drop the
  git half.
- Replace the helper-level assertion at :906-910 with a real assertion on the printed output,
  or delete it as redundant with :951 and note the print path is covered by :830.
- Rename `non_git_to_branch_refused_before_any_repo_change` → `..._before_any_git_call`.

## Gates

Independently re-run by this review, on the review worktree at merged main `82ed5a9d`:

    cargo test -p orgasmic-cli --bin orgasmic -- migrate doctor
    # test result: ok. 33 passed; 0 failed; 0 ignored; 0 measured; 277 filtered out; finished in 41.22s
    # exit code 0   (log: /tmp/jwhxh32-review-tests.log)

Matches the implementer's and manager's reported count (33). No regressions in the git
fixtures.

## Findings recorded

Four `reviewer.finding` tx entries on TASK-JWHXH.3.2 (first: `tx-20260902-orgasmic-7072`),
all LOW, matching the four findings above.

---

**APPROVE WITH FOLLOW-UPS.**
