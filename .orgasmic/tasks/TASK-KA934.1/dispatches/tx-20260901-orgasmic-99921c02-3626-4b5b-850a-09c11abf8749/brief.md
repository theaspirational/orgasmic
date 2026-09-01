# TASK-KA934.1 — comment edit/delete need authorship; edit/tombstone must record who (M4)

Fix round for finding M4 of the whole-chain review (tx-1c6d2115, claude-opus-5 high).
Read the task first: `orgasmic task get --project orgasmic TASK-KA934.1`.

## What is actually true today (read it, do not take the finding verbatim)

- `post_task_comment` (`crates/orgasmic-daemon/src/api.rs:2319`) records the author: a member
  session's `identity.member_name()` wins; an admin/script may pass `req.actor`; else the
  daemon actor. That lands as `:ACTOR:` on the journal `comment` entry.
- `post_task_comment_edit` (`:2386`) and `post_task_comment_delete` (`:2421`) authorize via
  `task_comment_journal` → `Action::TasksComment`, which `authz.rs:83/92` grants to every
  role including viewer. Neither handler passes the caller's identity to the writer;
  `writer.edit_journal_comment(journal, entry_id, expected_body, body, edited_at)` and
  `writer.tombstone_journal_comment(journal, entry_id, expected_body)` know nothing about who.
- `node_kernel::comment_spans` ALREADY refuses any entry whose `TYPE` is not `comment`
  ("journal entry X is not an editable comment"), so `reviewer.finding`, `review.verdict`,
  `*.done` rows cannot be edited or tombstoned through these routes. Half of M4 is already
  closed at the kernel; confirm with a test rather than re-implementing it.
- `edit_comment_body` (`node_kernel.rs:259`) stamps only `:EDITED_AT:`;
  `tombstone_comment` (`:291`) rewrites TYPE to `comment.deleted`, drops the body, records
  nothing about who or when.

So the real gaps: (a) any authenticated caller can edit/delete ANY OTHER author's comment;
(b) edits and tombstones carry no actor.

## What to do — the minimum

1. **Authorship check inside the writer op**, where the file is already locked and read:
   extend `edit_journal_comment` / `tombstone_journal_comment` (writer.rs) with an
   `actor: Option<String>` + `admin: bool` (or one small enum) and have the transform compare
   the entry's `:ACTOR:` to the caller: match → proceed; admin → proceed; else refuse with a
   distinguishable error the handler maps to **403** (not 400, not 409). Handlers derive the
   pair the same way `post_task_comment` does (`identity.member_name()`; admin when there is
   no member name — check how `Identity` exposes that and reuse it, do not invent a role
   test). No new `Action` unless you find `TasksComment` is reused somewhere that makes the
   in-op check impossible; say so if you do.
2. **Stamps.** `edit_comment_body` gains `edited_by: &str` and writes `:EDITED_BY:` next to
   `:EDITED_AT:` (replace-in-place like the existing stamp). `tombstone_comment` gains
   `(deleted_by: &str, deleted_at: &str)` and writes `:DELETED_BY:` / `:DELETED_AT:` into
   the drawer while rewriting TYPE. Keep the body-drop and the one-line tombstone shape;
   `comment.deleted` stays.
3. **UI** (`ui/src/components/TaskDialog.tsx:885-940`): Edit/Delete are hidden for
   `automated` rows already. Additionally hide them when the row's actor is not the current
   member (admins keep them). Whatever field carries the viewer's identity in `Me` is the
   source; do not add a new endpoint. If the UI has no reliable way to know the current
   actor name, leave the UI as is and say so — the server rule is the deliverable.

## Tests
- Daemon api: member A edits/deletes own comment → 200; member B edits/deletes A's comment
  → 403 and the journal is unchanged; admin edits/deletes A's comment → 200; edit/delete on
  a `reviewer.finding` entry → refused (pin the existing kernel behaviour with an explicit
  status assertion). `task_comments_use_member_session_attribution_and_refresh_activity`
  (`api.rs:38121`) is the fixture to copy.
- Kernel: `edit_comment_body` output contains `:EDITED_BY:` and `:EDITED_AT:`;
  `tombstone_comment` output contains `:DELETED_BY:` and `:DELETED_AT:` and
  `:TYPE: comment.deleted`; both parse back with `parse_journal`.

## Gates (each to a log file, never pipe cargo output)
- `cargo test -p orgasmic-core --lib node_kernel`
- `cargo test -p orgasmic-daemon --lib -- comment`
- `cargo clippy -p orgasmic-core -p orgasmic-daemon --all-targets -- -D warnings`
- `cargo fmt --all --check`
- `cd ui && npm ci && npm run typecheck` (only if you touch `ui/`)

## Rules
- Work only in your worktree; commit as `TASK-KA934.1: fix(daemon): <one line>`.
- NEVER `cargo test --workspace`; NEVER the whole `orgasmic-cli` crate; NEVER set
  `ORGASMIC_HOME`; NEVER run `daemon start`; never touch the live ledger at
  `~/.orgasmic/ledgers/orgasmic`.
- Report: what changed (`file:line`), each gate with its pass/fail line and log path, unmet
  criteria, residual risk. Finish with `orgasmic dispatch finalize --summary-file <path>`
  (report only, no `--commit`).
