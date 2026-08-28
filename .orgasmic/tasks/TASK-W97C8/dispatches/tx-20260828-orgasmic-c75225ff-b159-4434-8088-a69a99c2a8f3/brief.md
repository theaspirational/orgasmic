# Review Brief: TASK-W97C8 — round 2 (fix round after FINDINGS)

Review branch `task-w97c8-impl-r2` (tip dbf66f4e) against main (bb0645fb).
Diff: `git diff bb0645fb..dbf66f4e`. You reviewed round 1 (bc4ee26d) and
returned 9 findings; round 2 claims all fixed. Your round-1 review:
`.orgasmic/tmp/dispatch/task-w97c8/review-round-1.md` (project-root
relative). Round-2 report: `.orgasmic/tmp/dispatch/task-w97c8/` —
`task-w97c8-*-last.txt` files, or ask git log.

Verify each fix ACTUALLY closes its finding — with the same measured rigor
as round 1 (probe against real ledger/session files where cheap):

- F1: `dispatch_record_from_fold` reads `TxEntry.target` (first-class), and
  the new `dispatch_fold_reads_run_created_target_field` test folds a
  REAL-shaped tx (would it have caught round 1?).
- F2: lossy parser — `unparsed_events` + `bounded_events` tallied, parsing
  continues past bounded stubs AND a truncated final line; the fixture is a
  real `orgasmic_bounded` line, not a sanitized one.
- F3: claude-only narrative documented in convention + task journal; codex
  `System` stream excluded from reasoning (harness notices).
- F4: generic ItemStarted counting with non-tool exclusion list
  (`agent_message`/`agentMessage`/`reasoning`) — is the exclusion list
  right? Would `wait` count as a tool now, and is that acceptable?
- F5: shipped_conventions 5/5 green; the new guards actually pin the
  evidence.json contract (not vacuous).
- F7: recovery pairing — addressed run's target+id preferred, fallback pairs
  initial path WITH initial run id (never mixed).
- F8: semantic floor — zero-counts evidence refused unless missing/unread/
  unparsed is named. Check it can't false-positive on a legitimately idle
  run that only produced lifecycle events.
- F9: 64 KiB UTF-8-boundary cap + `narrative_truncated`.

Also: no payload leakage regression (ToolCall args / ToolResult outputs /
ProviderItemLifecyclePayload.data), partial-failure discipline still
intact, and no daemon API changes.

Verdict: APPROVE, APPROVE-WITH-FOLLOW-UPS (name them), or FINDINGS.
Pinned toolchain: `rustup run 1.97.1 cargo ...`. Do not edit code.
