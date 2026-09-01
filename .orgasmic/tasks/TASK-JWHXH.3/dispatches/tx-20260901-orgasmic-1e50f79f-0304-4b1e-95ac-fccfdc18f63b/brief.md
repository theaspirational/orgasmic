# Review: TASK-JWHXH.3 — on-disk views deleted; render on demand; migrate + doctor

Implementer: opencode / zai-coding-plan/glm-5.3 (variant max), one commit `9af6548d`, merged to
main as `ad642ca7` on top of TASK-KA934.3's `9f6874f0` (both touch `api.rs`, different
regions). Implements `dec_XH2XY` (+ `dec_AF61D`). Read
`orgasmic task get --project orgasmic TASK-JWHXH.3` and both decisions.

    git diff ad642ca7^1 ad642ca7     # 19 files, +404/-306

## What this round claims
- core `views.rs`: `build_views`/`write_if_changed`/scratch-write machinery deleted; new pure
  `render_view(root, "board.org"|"decisions.org"|"glossary.org")`.
- daemon `index.rs`: all three rebuild call sites + debounce machinery deleted.
- daemon `api.rs get_org_file` (~:14512-14570): `.orgasmic/views/<name>.org` rendered on demand;
  unknown names fall through to the disk read (404). `post_org_file` refusal untouched.
- daemon `ledger_sync.rs` (~:136-158): synced-ledger loop keeps `git rm -r --cached` of
  `.orgasmic/views`, now also `remove_dir_all` it; the `views/`-in-.gitignore ensure removed.
- CLI: `orgasmic views build` deleted; `project migrate` gains `ViewsMigration` (detect via
  `git ls-files`, untrack, delete dir, idempotent; `refuse_dirty_tree` now excludes
  `.orgasmic/views` paths); `doctor` warns while tracked/present.
- Scaffold `.gitignore` drops `views/`; context packs deleted; prompt prose + skill docs
  repointed to the CLI.

## Attack these specifically
- **Data safety of the deletes.** `remove_dir_all(.orgasmic/views)` in the sync loop and in
  `project migrate`: can either ever run against a path that is NOT the derived views dir
  (symlink, case-folded `Views/`, a project root resolved wrongly, `.orgasmic` being a
  submodule)? Is the sync-loop delete inside the writer barrier or otherwise safe against a
  concurrent renderer? (There should be no renderer left — confirm nothing writes there.)
- **`refuse_dirty_tree` exclusion.** Excluding `.orgasmic/views` from the dirty check must not
  let `migrate_to_branch` proceed over OTHER dirty paths. Read the filter.
- **On-demand render cost.** `get_org_file` for `board.org` renders every task node on each
  request (3 MB, 40k lines here). Is it on the async executor thread (blocking) or
  `spawn_blocking`? Size it: LOW vs a real stall of the daemon under UI polling.
- **Behavioural equivalence.** Does `render_view` produce byte-identical output to the old
  `build_views` for the same tree (ordering, `#+title`, version header)? The UI viewer and
  any org-mode user relied on the old shape.
- **Idle-ledger regression.** With the boot-time board-entry rebuild gone, is anything else
  that used to be triggered by that code path (claims.org reload hook at the old ~:972) still
  triggered? Read what surrounded the deleted calls.
- **Peer on old runtime.** Synced ledger with a peer still writing views files: each tick
  untracks+deletes → does that create a commit per tick (churn) or a conflict loop with the
  peer's re-adds? Size it against the conflict path (dec_EWY0K).
- **Docs honesty.** Prompt prose now says `orgasmic glossary list --project <id>` etc. — do
  those verbs exist with those flags (`crates/orgasmic-cli/src/main.rs`)?
- **Nothing else moved.** 19 files; every hunk should be one of the bullets above.

Classify precisely; if only LOWs remain, say so and APPROVE (with follow-ups if any).

Already established — do not re-spend: implementer gates (core, daemon lib 111, integration
scaffold, cli 34, clippy, fmt); manager re-ran the combined set on merged main `ad642ca7`
(see task Evidence). Targeted re-runs are fine; never the workspace.

## Rules
- READ-ONLY. No edits, no git writes, no mutating `orgasmic` verbs, nothing against the live
  ledger at `~/.orgasmic/ledgers/orgasmic` beyond read-only git reads. The live daemon on
  :4848 runs an OLD runtime — do not probe it; not a defect.
- Never run `git reset --hard`, `git rebase`, `git pull`, `git stash drop`, `git rm` outside a
  throwaway temp repo you created.
- File each finding as it appears:
  `orgasmic tx record --project orgasmic --type reviewer.finding --task TASK-JWHXH.3
  --reason "HIGH|MEDIUM|LOW <file:line> — <one sentence>"` (single line).
- Targeted tests only; NEVER the whole `orgasmic-cli` suite unfiltered; never the workspace;
  never `ORGASMIC_HOME`; never `daemon start`; do not read `verify/*/injection.patch`; never run
  `legacy_drivers_and_explicit_pairs_emit_equivalent_start_events`.
- Say what you did not check. Finish with `orgasmic dispatch finalize --summary-file <path>`
  (report only) and end with the explicit verdict sentence:
  APPROVE / APPROVE WITH FOLLOW-UPS / REJECT.
