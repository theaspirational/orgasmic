# Review: TASK-JWHXH.3.1 — doctor sees stale views on non-git projects; stale strings (narrow)

Implementer: opencode / zai-coding-plan/glm-5.3-flash (variant max), one commit `c60bb97b`,
merged to main as `e290d7fb`. Answers the MEDIUM + 3 LOWs of the JWHXH.3 review
(tx-1e50f79f). Read `orgasmic task get --project orgasmic TASK-JWHXH.3.1` and `dec_XH2XY`.

    git diff e290d7fb^1 e290d7fb     # doctor.rs, project_migrate.rs, api.rs (string+test), tx.org

Keep this review to the diff and its direct neighbours.

## What this round claims
- `doctor.rs` `push_tracked_views_findings`: `dir_present` computed for every registered
  project; `git ls-files` only inside a work tree; non-git stale dir now warns.
- `project_migrate.rs` `refuse_dirty_tree`: early-returns for a non-git root (git status exits
  128 there; the implementer says migrate previously bailed on non-git projects entirely).
- `views_summary_lines` (new): unreachable arm deleted; real apply prints post-apply state.
- `api.rs` `reject_ledger_rewrite` string for `.orgasmic/views/*` updated; test asserts it.
- `shipped/schema/tx.org` line dropped; `cargo test -p orgasmic-core --test fixtures` still parses it.

## Attack these specifically
- **`refuse_dirty_tree` early return.** New behaviour beyond "move the check": a non-git root
  skips the dirty check. What else in `run_at` runs on a non-git root after that? Can
  `migrate_to_branch` or any git op now run there and fail half-way (partial apply on a
  non-repo)? Is the early return the smallest correct enablement, or should `run_at` gate
  the whole git-dependent path instead?
- **Doctor on non-git projects.** Does `is_git_work_tree` get called with the right root for
  a registered project whose `.orgasmic` is a worktree ledger (the real deployment shape)?
  Any false "still present" for the synced ledger itself (its dir is deleted by the sync loop
  each tick — but is there a window where doctor runs between rebuild and delete? there
  should be no rebuild anymore).
- **Summary truthfulness.** `views_summary_lines` on a real apply: is it derived from state
  observed AFTER apply, or inferred from the pre-apply plan booleans? The finding was
  "prints pre-apply counts"; inferring post-state from pre-plan is the same bug in a nicer hat
  if `apply` can partially fail.
- **Nothing else moved.** Four files; every hunk should be one of the bullets.

Classify precisely; if only LOWs remain, say so and APPROVE (with follow-ups if any).

Already established — do not re-spend: implementer gates (cli doctor/migrate 31, daemon
org_file 8, clippy, fmt, core fixtures 19); manager re-ran on merged main `e290d7fb` (task
Evidence). Targeted re-runs are fine; never the workspace.

## Rules
- READ-ONLY. No edits, no git writes, no mutating `orgasmic` verbs, nothing against the live
  ledger at `~/.orgasmic/ledgers/orgasmic` beyond read-only git reads. The live daemon on
  :4848 runs an OLD runtime — do not probe it.
- Never run `git reset --hard`, `git rebase`, `git pull`, `git stash drop`, `git rm` outside a
  throwaway temp repo you created.
- File each finding as it appears:
  `orgasmic tx record --project orgasmic --type reviewer.finding --task TASK-JWHXH.3.1
  --reason "HIGH|MEDIUM|LOW <file:line> — <one sentence>"` (single line).
- Targeted tests only; NEVER the whole `orgasmic-cli` suite unfiltered; never the workspace;
  never `ORGASMIC_HOME`; never `daemon start`; do not read `verify/*/injection.patch`; never run
  `legacy_drivers_and_explicit_pairs_emit_equivalent_start_events`.
- Say what you did not check. Finish with `orgasmic dispatch finalize --summary-file <path>`
  (report only) and end with the explicit verdict sentence:
  APPROVE / APPROVE WITH FOLLOW-UPS / REJECT.
