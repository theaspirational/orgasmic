# Review: TASK-JWHXH.3.2 — `project migrate` on a non-git root: cleanup only (narrow)

Implementer: opencode / zai-coding-plan/glm-5.3-flash (variant max), one commit `8c460413`,
merged to main as `82ed5a9d`. Answers the MEDIUM + 2 LOWs of the JWHXH.3.1 review
(tx-21182657). Read `orgasmic task get --project orgasmic TASK-JWHXH.3.2` and `dec_XH2XY`.

    git diff 82ed5a9d^1 82ed5a9d     # project_migrate.rs only, +135/-20

## What this round claims
- `run_at` probes the work tree once; `ViewsMigration::plan` takes the bool.
- Non-git root: views cleanup only (skipped on `--dry-run`), then `bail!` with a plain message
  when the plan has anything to rewrite or `--to-branch` was passed; v1 files untouched.
- `refuse_dirty_tree`'s non-git early return deleted (now unreachable).
- `print_summary` extracted so both paths print the same lines.
- Tests: non-git v1 fixture (views deleted, rewrite refused, v1 files byte-identical, summary
  line asserted at the call site); non-git `--to-branch` refused before any git call.

## Attack these specifically
- **Git-path parity.** The implementer claims git roots follow the byte-for-byte prior
  sequence. Verify the order (`refuse_dirty_tree` → `views.apply` → branch/rewrite →
  summary) and that the "already migrated" ledger-root early return still precedes it.
- **Non-git dry-run.** Now refuses instead of printing a plan (implementer-disclosed). Is
  that acceptable or does it hide the views plan an operator wanted to see? Size it.
- **`refuse_dirty_tree` precondition.** One caller, gated — confirm nothing else (tests
  included) calls it on a non-git root and would now get git's rc-128 bail.
- **Message honesty.** The bail text tells the operator to "init git or back up .orgasmic
  first" — is that the actual recovery for a non-git v1 project, and does the views cleanup
  that already ran get mentioned or is it silent?
- **Nothing else moved.** One file; every hunk should be one of the bullets.

Classify precisely; if only LOWs remain, say so and APPROVE (with follow-ups if any).

Already established — do not re-spend: implementer gates (cli migrate/doctor 33, clippy,
fmt); manager re-ran on merged main `82ed5a9d` (task Evidence).

## Rules
- READ-ONLY. No edits, no git writes, no mutating `orgasmic` verbs, nothing against the live
  ledger at `~/.orgasmic/ledgers/orgasmic` beyond read-only git reads. The live daemon on
  :4848 runs an OLD runtime — do not probe it.
- Never run `git reset --hard`, `git rebase`, `git pull`, `git stash drop`, `git rm` outside a
  throwaway temp repo you created.
- File each finding as it appears:
  `orgasmic tx record --project orgasmic --type reviewer.finding --task TASK-JWHXH.3.2
  --reason "HIGH|MEDIUM|LOW <file:line> — <one sentence>"` (single line).
- Targeted tests only; NEVER the whole `orgasmic-cli` suite unfiltered; never the workspace;
  never `ORGASMIC_HOME`; never `daemon start`; do not read `verify/*/injection.patch`; never run
  `legacy_drivers_and_explicit_pairs_emit_equivalent_start_events`.
- Say what you did not check. Finish with `orgasmic dispatch finalize --summary-file <path>`
  (report only) and end with the explicit verdict sentence:
  APPROVE / APPROVE WITH FOLLOW-UPS / REJECT.
