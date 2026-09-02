# Review: TASK-JWHXH.3.1 — doctor sees stale views on non-git projects; stale strings

Scope: `git diff e290d7fb^1 e290d7fb` (4 files: `doctor.rs`, `project_migrate.rs`,
`api.rs`, `shipped/schema/tx.org`) and its direct neighbours. Read-only throughout.

## Verdict

**APPROVE WITH FOLLOW-UPS.** All four assigned items are correctly implemented and the
acceptance criteria hold. The one MEDIUM is a blast-radius question about the
`refuse_dirty_tree` early return, not a defect in the shipped behaviour; it does not
block.

## Findings

### MEDIUM (correctness / usability) — `crates/orgasmic-cli/src/project_migrate.rs:219`

The early return

```rust
if !git_ok(root, &["rev-parse", "--is-inside-work-tree"]) {
    return Ok(());
}
```

is more than "move the dirty check". Before this commit `refuse_dirty_tree` was the first
thing `run_at` did after planning, and on a non-git root `git status` exits 128 (verified:
`git status --porcelain=v1 -z --untracked-files=all -- ':(exclude).orgasmic/views'` in a
plain directory → `fatal: not a git repository`, rc 128), so it `bail!`ed and **the entire
migrate verb refused on non-git roots**. That refusal was also, incidentally, the only guard
on every later step.

Now the whole of `run_at` runs on a non-git root. The acceptance scenario (v2 project, only
a stale `views/` dir) is safe — `plan()` returns early with `old_files` empty, and
`ViewsMigration::apply` never shells out to git when `tracked` is empty. The newly reachable
hazard is a **non-git v1 project**: `apply_with_recovery` (`project_migrate.rs:497`) now runs
the destructive aggregate→node rewrite there. `apply` writes the new node dirs, then
`remove_file`s every old source, then rewrites `project.org`. A failure between those steps
leaves the project wedged (`plan` will then bail with either `migration source is missing`
or `migration target already exists`), and the context line it prints is:

```
migration partially applied; recover <root> with:
  git -C <root> checkout -- .orgasmic
  git -C <root> clean -fd -- .orgasmic
```

Both commands are inert in a non-git directory. Non-git registrations are real:
`projects::register_project` (`orgasmic-core/src/projects.rs:280`) has no git requirement,
and the new test in this very diff registers one.

Fix direction: keep the early return, but gate the git-dependent work in `run_at` instead of
letting it fall through — e.g. refuse `--to-branch` on a non-git root up front, and make
`apply_with_recovery`'s context git-aware (or state plainly that there is no VCS to recover
from). The narrow alternative is to run the views cleanup and skip `apply_with_recovery`
when the root is not a work tree.

### LOW (usability) — `crates/orgasmic-cli/src/project_migrate.rs:481`

`orgasmic project migrate --to-branch` on a non-git root now reaches `migrate_to_branch`.
Traced: `create_orphan_branch` → `git_ok(root, ["rev-parse","--verify","refs/heads/orgasmic"])`
is false → `git_env(root, …, ["read-tree", "--empty"])` → verified rc 128,
`fatal: not a git repository` → `bail!`. Because `branch_created`/`worktree_added`/
`source_removal_started` are all still false, the context reads
`branch cutover failed before this run changed the repository` — but `views.apply(root)`
already deleted `.orgasmic/views/` earlier in `run_at` (`project_migrate.rs:161`), so the
run did change the tree. Cosmetic only (the deletion is the desired end state and is
idempotent), and the pre-fix message (`git status failed: not a git repository`) was no
clearer. Folds into the MEDIUM's fix.

### LOW (test) — `crates/orgasmic-cli/src/project_migrate.rs:836`

Acceptance says "migrate summary correct on a real apply". The new
`views_summary_reports_post_apply_state_not_pre_apply_counts` is a pure unit test of the
extracted helper; `non_git_project_views_doctor_warns_migrate_deletes_then_doctor_quiet`
drives a real `run_at` but asserts only on the filesystem, never on stdout. So the real
`println!` path is not covered by any test. Low value to fix — the helper is total and the
call site is a bare `for` loop — but the acceptance box is checked by inference, not by a
production-path assertion.

## What I checked and confirmed correct

- **`doctor.rs:252-268` — the MEDIUM this round answers.** `dir_present` is now computed
  unconditionally for every board entry; `git ls-files` runs only under `is_git_work_tree`.
  The `tracked.is_empty() && !dir_present` continue is the same predicate as before, so no
  git-repo behaviour changed. Nothing in the tree writes `.orgasmic/views/` any more
  (grepped all `*.rs`: the only writers left are the two *cleaners*, `ledger_sync.rs:139-158`
  and `ViewsMigration::apply`; `index.rs:5731` and `views.rs:160` assert non-materialization).
  So there is no rebuild→delete window that could make doctor cry wolf on the synced ledger,
  and `is_git_work_tree` is called with the board entry's canonicalized `path`, which for the
  worktree-ledger deployment shape is a real work tree → the tracked probe still runs there.

- **Summary truthfulness — `views_summary_lines` is *not* the same bug in a nicer hat.**
  `views_applied` comes from `views.apply(root)?`, which returns `Ok(true)` only after both
  the `git rm --cached` and the `remove_dir_all` succeeded; any partial failure propagates
  `Err` through `run_at` and no summary is printed at all. Each `(tracked_empty, dir_present)`
  pre-state therefore maps 1:1 to a determined post-state:
  `(false,true)`→untracked+removed, `(false,false)`→untracked, `(true,true)`→removed,
  `(true,false)`→`is_clean`, so `apply` returned false and the arm is dead. Inference from
  pre-plan booleans is sound here. Non-dry output when nothing changed is empty, matching
  the old code exactly.

- **`api.rs:14705`** — new string is live on the real refusal path
  (`org_file_rewrite_refuses_ledger_paths` asserts the exact new text); the old phrase
  "view refresh operation" has zero remaining occurrences anywhere in the repo, including
  `ui/`.

- **`shipped/schema/tx.org:293`** — line dropped; `parses_shipped_schema_files` still green.

- **Scope.** Exactly four files, and every hunk maps to one of the four assigned bullets.
  No drift.

## Verification notes

Independently re-run in this worktree at `e290d7fb`:

```
cargo test -p orgasmic-cli --bin orgasmic -- project_migrate doctor   → 31 passed, 0 failed
cargo test -p orgasmic-daemon --lib -- org_file                       → 8 passed, 0 failed
cargo test -p orgasmic-core --test fixtures                           → 19 passed, 0 failed
```

Matches the implementer's and manager's counts exactly.

Git primitives probed directly in a throwaway `/tmp` non-repo to ground the MEDIUM/LOW:
`rev-parse --is-inside-work-tree`, `status --porcelain=v1 … ':(exclude).orgasmic/views'`,
`rev-parse --verify refs/heads/orgasmic`, and
`GIT_WORK_TREE=… GIT_INDEX_FILE=… read-tree --empty` — all rc 128,
`fatal: not a git repository`.

**Not checked:**
- `clippy -D` and `cargo fmt` — not re-run (already gated twice; the brief says do not
  re-spend). No new lint surface is obvious in the diff.
- **No production-path probe of the non-git `--to-branch` failure.** Running the built
  binary would need an isolated `ORGASMIC_HOME`, which this dispatch forbids, and the live
  home/ledger is off-limits. The MEDIUM and the first LOW rest on a full code trace plus the
  verified git exit codes above, not on an end-to-end run. Residual risk: the exact wording
  and stop point of the `--to-branch` non-git failure could differ from my trace; the fact
  that migrate no longer refuses on non-git roots is certain (it follows directly from the
  new early return plus rc 128).
- The daemon on :4848 — untouched, per the rules.

## Open questions

1. Is `orgasmic project migrate` *intended* to work on non-git projects at all, or only far
   enough to clear a stale `views/` dir? The answer decides whether the MEDIUM's fix is
   "gate the git-dependent path" or "document non-git as fully supported and fix the
   recovery text". dec_XH2XY does not say.

## Fix directions (follow-ups, not blockers)

1. In `run_at`, compute `is_work_tree` once. Bail early with a clear message if
   `to_branch && !is_work_tree`. Skip `apply_with_recovery`'s git-flavoured context (or
   swap it for a non-git variant) when `!is_work_tree`. That closes both the MEDIUM and the
   first LOW with one branch.
2. Optional: have `run_at` return the summary lines instead of printing them, so a test can
   assert the real path.

## Out of scope, noticed in passing

`shipped/skills/orgasmic/meta/corpus-manifest.json:135-136` still lists `cli-help/views.txt`
and `cli-help/views/build.txt`, but the `views` CLI subcommand was removed in JWHXH.3
(no `Views` variant remains in `crates/orgasmic-cli/src/main.rs`). Nothing consumes the
manifest in-tree, so no gate catches it. A JWHXH.3 leftover, not this round's.

---

**APPROVE WITH FOLLOW-UPS.**
