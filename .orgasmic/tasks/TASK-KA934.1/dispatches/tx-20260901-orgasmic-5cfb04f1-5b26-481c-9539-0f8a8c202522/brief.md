# Review: TASK-KA934.1 — comment edit/delete authorship + audit stamps (M4)

Fix round for chain-review finding M4 (whole-chain review tx-1c6d2115). Implementer: codex
gpt-5.6-sol, one commit `71ecc0dc`, merged to main as `cffb986b`.

## What to review

    git diff cffb986b^1 cffb986b

Four files, +316/-45: `crates/orgasmic-core/src/node_kernel.rs`,
`crates/orgasmic-daemon/src/writer.rs`, `crates/orgasmic-daemon/src/api.rs`,
`ui/src/components/TaskDialog.tsx`.

## The finding this must close (M4)
Comment edit/delete gated only on `Action::TasksComment` (granted to viewer) with no
authorship check, so any caller could rewrite or tombstone anyone's comment; tombstone and
edit recorded no actor. (Half of the original finding was already false: `comment_spans`
refuses non-`comment` entry types, so `reviewer.finding`/`*.done` rows were never editable
through these routes.)

## What the fix claims
1. `writer::CommentMutationActor::{Member(name), Admin(name)}`; handlers derive it from
   `identity.member_name()` (member) else `Admin(state.actor)`. `require_comment_body`
   (runs inside the locked `mutate_file` transform) refuses a `Member` whose name != the
   entry's `:ACTOR:` with typed `CommentAuthorshipForbidden`; `writer_comment_error` maps it
   to 403; everything else still maps through `writer_append_error`.
2. `node_kernel::upsert_comment_property` (insert-or-replace one drawer property before
   `:END:`); `edit_comment_body(+edited_by)` stamps `EDITED_BY`+`EDITED_AT`;
   `tombstone_comment(+deleted_by, deleted_at)` stamps `DELETED_BY`+`DELETED_AT` and still
   rewrites TYPE to `comment.deleted` and drops the body.
3. UI `ActivityRow`: `canMutate = !automated && (identity === 'admin' || me?.name === entry.actor)`.
4. Tests: api — member `bob` (viewer) editing/deleting `alice`'s comment → 403 and the journal
   bytes are unchanged; `Identity::Admin` edits/deletes alice's comment → ok with admin
   stamps; edit/delete on a `reviewer.finding` → refused (pinned as **500**, pre-existing
   mapping); kernel + writer tests assert both stamp pairs parse back.

## Attack these specifically
- **Actor identity semantics.** `:ACTOR:` on a comment is whatever `post_task_comment`
  wrote: `identity.member_name()` for members, but for an ADMIN it is `req.actor` (free
  text) or the daemon actor. Can a member choose a display name that equals another
  member's `:ACTOR:` (rename, case, unicode normalisation, trailing space) and thereby pass
  the equality check? Where do member names come from (`members.org`?) and are they unique
  and immutable? If names are mutable, authorship should key on a stable id — say whether
  one exists.
- **Admin scope.** `Admin(state.actor)` bypasses authorship entirely. Is every non-member
  `Identity` variant really an operator (e.g. a worker token, an agent session, a
  local-only unauthenticated request)? Enumerate the `Identity` variants and say which reach
  these handlers as `Admin`.
- **Ordering inside `require_comment_body`.** It now checks: exists → type == comment →
  authorship → OCC (`expected_body`). A non-author therefore learns nothing about the body
  (good) — but confirm the 403 path does not leak the current body in its message, and that
  a missing entry still 404/400s rather than 403s.
- **`upsert_comment_property` correctness.** It searches props by key and `replace_range`s
  the VALUE span, else inserts `:KEY: value\n` before `:END:`. Verify against a drawer whose
  last property has no trailing newline, a value that already contains `:`, and a repeated
  edit (second edit must replace, not duplicate). Does the inserted key order break
  `JournalEntry::validate` (REQUIRED keys, duplicate detection) or any byte-stable
  round-trip test elsewhere?
- **The 500 for automated rows.** The test pins `INTERNAL_SERVER_ERROR` for editing a
  `reviewer.finding`. That is an honest pin of pre-existing behaviour, but is it the right
  status? A LOW if you agree it should be 400/409; a MEDIUM if the 500 path also skips
  something (e.g. leaves a partial write or a poisoned OCC).
- **Middleware reality.** The implementer notes `MEMBER_ALLOWED_ROUTES` (api.rs ~896) does
  not include the task-comment routes, so real member sessions are 403'd before these
  handlers. Confirm that from the table. If true, the new check is defense in depth today
  and the direct-handler tests are the only exercise it gets — say so plainly; do not
  invent a route change.
- **UI honesty.** `me?.name === entry.actor` — is `me.name` the same string the daemon
  wrote as `:ACTOR:` (same source, same normalisation)? For admins `identity === 'admin'`
  — is that the field the `/me` route actually returns (`ui/src/lib/types.ts MeIdentity`)?
- **Test honesty.** Does the 403 test's "journal unchanged" assertion run AFTER both
  refusals against the same bytes captured BEFORE them? Does any test prove a member can
  still edit/delete their OWN comment after the change (the positive path for a member,
  not the admin)?

Already established — do not re-spend: on the merged tree the manager ran
`cargo test -p orgasmic-core --lib node_kernel` (4 passed), `cargo test -p orgasmic-daemon
--lib -- comment` (18 passed), `cargo clippy -p orgasmic-core -p orgasmic-daemon
--all-targets -- -D warnings` clean, `cargo fmt --all --check` clean,
`cd ui && npm run typecheck` clean (see `orgasmic task get --project orgasmic TASK-KA934.1`).

## Rules
- READ-ONLY. No edits, no git writes, no mutating `orgasmic` verbs, nothing against the live
  ledger at `~/.orgasmic/ledgers/orgasmic`.
- File each finding as it appears:
  `orgasmic tx record --project orgasmic --type reviewer.finding --task TASK-KA934.1
  --reason "HIGH|MEDIUM|LOW <file:line> — <one sentence>"` (single line).
- Targeted tests only (`cargo test -p orgasmic-daemon --lib <name>`,
  `cargo test -p orgasmic-core --lib <name>`); never the workspace; never `ORGASMIC_HOME`;
  do not read `verify/*/injection.patch`.
- Say what you did not check. Finish with `orgasmic dispatch finalize --summary-file <path>`
  (report only) and end with the explicit verdict sentence:
  APPROVE / APPROVE WITH FOLLOW-UPS / REJECT.
