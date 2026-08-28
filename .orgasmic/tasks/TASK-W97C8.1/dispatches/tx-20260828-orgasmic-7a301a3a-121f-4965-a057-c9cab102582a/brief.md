# Fix Brief: TASK-W97C8.1 — round 2, address review findings

Round 1 (d57d2824 on `task-w97c8.1-impl`) was reviewed: FINDINGS, blocks
ship on F-1. Full review with measured probes:
`.orgasmic/tmp/dispatch/task-w97c8.1/review-round-1.md` (project-root
relative) — READ IT FIRST. Continue from the round-1 tip.

Fix in this order:

1. **F-1 (HIGH, gate)** `paths.rs:250-284,390-398` + `manager.rs:7686-7700` —
   missing tmp sidecars currently hard-error BEFORE `git worktree remove`,
   so a dispatch whose tmp brief/compiled-prompt is gone (binary upgraded
   mid-dispatch, tmp swept) can NEVER close. Sidecars are evidence, not
   preconditions: on `ErrorKind::NotFound` return `None` for that sidecar,
   promote what exists, and record the gap loudly (CLEANUP_ERROR naming the
   missing file, or a stub in the record naming the absence). Keep `Err`
   only for exists-but-unsafe. Tests: close with brief deleted; close with
   compiled prompt deleted (both must complete and promote the rest).
2. **F-2 (MED)** `paths.rs:288-299` — compiled prompt is stem-scoped
   (`<stem>-compiled-prompt.md`), so two dispatches whose brief files share
   a basename overwrite each other's bundle. Make it attempt-scoped: derive
   from the `last.txt` FILENAME by suffix-replace (`-last.txt` →
   `-compiled-prompt.md`), pattern:
   `dispatch_sibling_artifact_paths_from_last` (`manager.rs:10405`). One
   helper body; daemon writer and close reader share it. Test: two starts
   in one stem dir keep distinct bundles.
3. **F-3 (MED)** `paths.rs:824-850` — `validate_dispatch_sidecar_file`
   accepts ANY regular file in the stem dir; a wrong BRIEF_PATH consumed a
   sibling attempt's retained last.txt as "the brief" and unlinked it
   (measured, review probe D). Require the filename to equal the expected
   sidecar name (two string compares), matching the strictness of
   `validate_dispatch_artifact_file` beside it. Test: sidecar rejection
   (sibling last.txt, symlink).
4. **F-5 (LOW)** — also swap the four `cat-file -e` assertions in
   `tests/dispatch.rs:4869` for one `git log --oneline -- <record_dir>`
   single-line assertion (proves ONE record commit, which is the stated
   property).
5. **F-4 (LOW)** — rollback leaves `<stem>-compiled-prompt.md` orphaned in
   tmp. After F-2's attempt-scoping, thread the sidecars into the rollback
   prune (or best-effort drop the well-known names in
   `prune_dispatch_stem_after_worktree`). Keep it small.

Also address review Open Question 2 pragmatically: do not enforce brief-
basename uniqueness; F-2's attempt-scoping removes the collision, note that
in the task journal.

Constraints unchanged: focused tests only, pinned toolchain
(`rustup run 1.97.1`), no API shape changes, keep the O_NOFOLLOW handle
discipline exactly as round 1 has it (review verified it sound — do not
regress it). Rerun green: `cargo test -p orgasmic-core --lib paths::`,
orgasmic-cli `dispatch_close`/`dispatch_evidence` bins,
`--test dispatch dispatch_close_promotes_complete_record_only_at_close`,
`--test dispatch dispatch_timeout_requests_daemon_cleanup`,
`--test shipped_conventions`.

Report: per-finding disposition, test names + pass counts.
