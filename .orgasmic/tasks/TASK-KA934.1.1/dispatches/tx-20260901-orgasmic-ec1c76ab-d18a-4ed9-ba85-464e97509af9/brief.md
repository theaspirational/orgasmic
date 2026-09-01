# TASK-KA934.1.1 — residuals of the KA934.1 review (OCC → 409, stamps have a reader, test pins)

Fix round 2 for TASK-KA934.1 (merged `cffb986b`). The review (claude-opus-5 high,
tx-5cfb04f1) approved with follow-ups. Read the task first:
`orgasmic task get --project orgasmic TASK-KA934.1.1` — exact `file:line` and acceptance.
Everything below is the minimum. Do NOT touch `MEMBER_ALLOWED_ROUTES` or member
capabilities — that is TASK-KA934.2, an open decision.

## 1. LOW — OCC lost-update must be a 409
`writer_comment_error` (`crates/orgasmic-daemon/src/api.rs:~1777`) maps only
`CommentAuthorshipForbidden` to 403; the OCC failure in `require_comment_body`
(`writer.rs:~1663`, "journal comment {id} changed since it was read") falls through to a
generic 500. Add a typed error (same shape as `CommentAuthorshipForbidden`, `Display` emits
only the entry id) and one arm → `StatusCode::CONFLICT`, mirroring `claim_conflict`. If it
stays a few lines, map the "not found" bail (`writer.rs:~1653`) to 404 the same way. One api
test: edit with a stale `expected_body` → 409 and the journal bytes are unchanged.

## 2. LOW — the audit stamps need a reader; tombstones must not vanish
`activity_entry_from_tx` (`crates/orgasmic-daemon/src/index.rs:~4321`) returns `None` for
`TYPE: comment.deleted`, and `ActivityEntry` has no `EDITED_BY/EDITED_AT/DELETED_BY/
DELETED_AT`. Effects: nothing outside `journal.org` shows who edited/deleted, and a
tombstoned row disappears from `GET /tasks/:id/activity`, so replies whose `IN_REPLY_TO`
points at it dangle (contradicts the intent stated at `node_kernel.rs:~311`).
Fix: add `Option<String>` fields `edited_by, edited_at, deleted_by, deleted_at` to
`ActivityEntry` (serde skip-if-none), return `comment.deleted` rows with an empty body, and
in `ui/src/components/TaskDialog.tsx` `ActivityRow` render a tombstone row
("comment deleted by <who>") with no Edit/Delete/Reply actions and an "edited" marker when
`edited_by` is set. Update `ui/src/lib/types.ts`. One daemon test: after
`tombstone_comment`, activity lists the row as `comment.deleted` with `deleted_by`.

## 3. LOW — pin the two test gaps
- `api.rs:~38268`: the bob refusals assert only `status == 403`. Also assert the authorship
  message (the `CommentAuthorshipForbidden` `Display` text), so the test goes red if the
  check is removed and the 403 arrives from authz instead.
- `node_kernel` tests: two sequential `edit_comment_body` calls by different editors on one
  comment → `EDITED_BY` appears exactly once and holds the SECOND editor; `EDITED_AT` once.

## Gates (each to a log file, never pipe cargo output)
- `cargo test -p orgasmic-daemon --lib -- comment activity`
- `cargo test -p orgasmic-core --lib node_kernel`
- `cargo clippy -p orgasmic-core -p orgasmic-daemon --all-targets -- -D warnings`
- `cargo fmt --all --check`
- `cd ui && npm run typecheck`

## Rules
- Work only in your worktree; one commit `TASK-KA934.1.1: fix(comments): <one line>`.
- NEVER `cargo test --workspace`; NEVER the whole `orgasmic-cli` crate in one command; NEVER
  set `ORGASMIC_HOME`; NEVER run `daemon start`; never touch the live ledger at
  `~/.orgasmic/ledgers/orgasmic`; never run
  `legacy_drivers_and_explicit_pairs_emit_equivalent_start_events`.
- Report: what changed (`file:line`), each gate with its pass/fail line and log path, unmet
  criteria, residual risk. Finish with `orgasmic dispatch finalize --summary-file <path>`
  (report only, no `--commit`).
