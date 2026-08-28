# Review Brief: TASK-W97C8.1 — round 2 (fix round after FINDINGS)

Review branch `task-w97c8.1-impl-r2` (tip 0884b10c) against main (46b015a3).
Diff: `git diff 46b015a3..0884b10c`. You reviewed round 1 (d57d2824) and
returned F-1..F-5; round 2 claims all fixed. Your round-1 review:
`.orgasmic/tmp/dispatch/task-w97c8.1/review-round-1.md`. Round-2 report:
`.orgasmic/tmp/dispatch/task-w97c8.1-fix/` last.txt files.

Verify each fix actually closes its finding, with probes where cheap
(your round-1 probe crate pattern applies):

- F-1: missing brief / missing BRIEF_PATH / missing compiled prompt →
  close COMPLETES: worktree removed, remaining record promoted+committed,
  gap named in CLEANUP_ERROR. Exists-but-unsafe still hard-errors. Re-run
  your round-1 probe cases A/B/C against the new core — they must not block.
  Also check the upgrade scenario end-to-end: a record dir already holding
  start-written brief.md (pre-W97C8.1 daemon) closing under the new CLI.
- F-2: compiled prompt attempt-scoped via `-last.txt` suffix-replace; two
  attempts in one stem keep distinct bundles; daemon writer and close
  reader share one helper (no divergence).
- F-3: sidecar validator name grammar — your probe D (sibling last.txt as
  brief) must now be rejected; symlink rejection too; O_NOFOLLOW handle
  discipline unregressed.
- F-4: rollback prunes the attempt-scoped compiled prompt through the
  validated stem-dir handle, without touching sibling attempts.
- F-5: the one-commit property now asserted via single `git log --oneline
  -- <record_dir>` line on the production close path.

Cross-checks: partial-failure retention still holds (all tmp copies kept on
any failed copy); no daemon API shape change; evidence.json promotion
(TASK-W97C8) unaffected; shipped_conventions 5/5.

Pinned toolchain: `rustup run 1.97.1`. Do not edit code.
Verdict: APPROVE, APPROVE-WITH-FOLLOW-UPS (name them), or FINDINGS.
