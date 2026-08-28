# Review Brief: TASK-W97C8.1 — brief.md + compiled-prompt.md move to close-time promote

Review branch `task-w97c8.1-impl` (tip d57d2824) against main (46b015a3).
Diff: `git diff 46b015a3..d57d2824`. Read TASK-W97C8.1's node for the design
and acceptance criteria.

Change under review: dispatch START no longer writes anything durable —
the daemon writes the compiled bundle to the gitignored tmp stem
(`.orgasmic/tmp/dispatch/<stem>/<stem>-compiled-prompt.md`); the close-time
promote copies brief + compiled-prompt into `dispatches/<tx>/` as
`brief.md`/`compiled-prompt.md` alongside report/evidence/stdout, in the one
record commit.

Verify with measured rigor (probe real ledger state where cheap):

1. Complete-or-absent: nothing under `dispatches/<tx>/` before close;
   after close, all files land in ONE path-scoped record commit. Check the
   daemon start path really creates no durable dir on ANY branch (including
   error paths after `record_dispatch_started`).
2. Rollback: failed dispatch leaves NO `dispatches/<tx>/` folder and prunes
   tmp only. Check the timeout/cleanup path too
   (`dispatch_timeout_requests_daemon_cleanup` claims it).
3. Handle discipline: the new brief/compiled-prompt promotion uses no-follow
   validated handles like last.txt/stdout (symlink games from a worker
   cannot redirect the copy); unlink happens only after EVERY copy
   succeeded; partial failure keeps all tmp copies.
4. The close needs the brief path at close time — where does it come from
   (recorded where?), and what happens when the manager passed a brief
   outside the stem dir, or the tmp brief was deleted before close (crashed
   machine, prune)? A missing brief must not silently produce a record
   without one — check it fails loud or records the absence.
5. Evidence.json interplay (merged in TASK-W97C8): the promote order and
   the semantic evidence floor — does a refused evidence file still promote
   brief/compiled-prompt, and is whatever behavior chosen coherent with the
   complete-or-absent promise?
6. Compiled prompt at close: unchanged bundle? It is written at start into
   tmp — confirm the close promotes that exact file (no recompilation, no
   divergence between what the worker saw and what the record keeps).
7. Tests: would they catch the regressions — absent-before-close asserted on
   the production close path (not hand-assigned state), rollback orphan
   check, partial-failure retention? Conventions gate 5/5.
8. No behavior widened: daemon change confined to relocating the writes; no
   API shape changes; payload/report promotion behavior for existing files
   unchanged.

Pinned toolchain: `rustup run 1.97.1 cargo ...` (plain cargo is 1.94.1).
Do not edit code.

Verdict: APPROVE, APPROVE-WITH-FOLLOW-UPS (name them), or FINDINGS
(numbered, file:line, severity-ordered).
