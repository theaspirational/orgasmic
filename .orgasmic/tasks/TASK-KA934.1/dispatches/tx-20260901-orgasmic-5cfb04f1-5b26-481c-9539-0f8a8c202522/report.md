# Review: TASK-KA934.1 — comment edit/delete authorship + audit stamps (M4)

Commit reviewed: `71ecc0dc` (merged as `cffb986b`), 4 files, +316/-45.

## Verdict

**APPROVE WITH FOLLOW-UPS.**

Both acceptance criteria are met and honestly tested. The fix is correct in the kernel,
correctly ordered in the writer, and the error mapping does not leak. Every finding below
is a follow-up, not a blocker; the two MEDIUMs are pre-existing conditions that determine
how much the new check is actually worth today, and the manager should see them before
closing M4 as "exploitable hole shut".

## Findings

Filed as `reviewer.finding` tx `-6884` … `-6889`.

### MEDIUM — `crates/orgasmic-daemon/src/api.rs:897` (security scope; pre-existing)
`MEMBER_ALLOWED_ROUTES` does not list `POST /tasks/:id/comments`,
`POST /tasks/:id/comments/:entry_id/edit`, or `.../delete`. `identity_middleware`
(api.rs:960-979) rejects any `Identity::Member` on an unlisted route with 403 *before the
handler runs*, and there are exactly two `Identity` variants (authz.rs:108-115), so every
request that reaches `post_task_comment_edit` / `post_task_comment_delete` in production
today is `Identity::Admin` and therefore takes the `CommentMutationActor::Admin` branch,
which bypasses authorship by design.

Consequence, stated plainly: **the M4 premise was not reachable over HTTP.** A viewer
holds `Action::TasksComment` (authz.rs:80) but cannot reach the route to use it. The
implementer's note in the brief is correct, and I confirmed it from the table rather than
taking it on faith. The new authorship check is defense in depth against a future
allow-list change, and the direct-handler unit tests are the only thing that exercises it.
That is a legitimate thing to build — it just should not be recorded as "closed an
exploitable LAN-exposed hole".

### MEDIUM — `ui/src/components/TaskDialog.tsx:179` (bug; pre-existing, not caused here)
The same allow-list gap is user-visible. `role_capabilities("viewer")` includes
`TasksComment`, `/me` (api.rs:1049-1060) reports it, and `canComment = can(projectId,
'tasks.comment')` enables the composer. The resulting POST is 403'd by the middleware with
`"forbidden for this member role"`. So a member sees a comment box that can never work.

Either the three comment routes belong in `MEMBER_ALLOWED_ROUTES`, or the member-attribution
branch in `post_task_comment` (api.rs:2348-2351) and the test
`task_comments_use_member_session_attribution_and_refresh_activity` are describing a path
that does not exist. Someone should decide which. Note that adding the routes to the
allow-list is exactly the change that makes this commit's authorship check load-bearing —
they should land together.

### LOW — `crates/orgasmic-daemon/src/index.rs:4321` (observability)
`activity_entry_from_tx` returns `None` for `TYPE: comment.deleted`, and `ActivityEntry`
has no field for `EDITED_BY` / `EDITED_AT` / `DELETED_BY` / `DELETED_AT`. Two effects:

1. The new audit stamps have **no reader**. Acceptance criterion 2 says "record … in the
   journal", and it is met literally — but the only way to see who deleted a comment is to
   open `journal.org` by hand. Nothing in the API or UI surfaces it.
2. A tombstoned row disappears from `GET /tasks/:id/activity` entirely, so replies whose
   `:IN_REPLY_TO:` points at it dangle — the opposite of the intent documented at
   `node_kernel.rs:311` ("leave the one-line tombstone so reply chains never dangle").

Both are pre-existing (`tombstone_comment` behaved this way before), but this commit is the
one that added audit data with nowhere to go.

### LOW — `crates/orgasmic-daemon/src/api.rs:1777` (API contract)
`writer_comment_error` maps only `CommentAuthorshipForbidden` to 403 and sends everything
else through `writer_append_error` → generic 500 `"failed to record transaction"`. That
swallows three distinguishable client errors:

| condition | source | current status | reasonable status |
|---|---|---|---|
| entry_id not in journal | `writer.rs:1653` `bail!("… not found")` | 500 | 404 |
| entry is not `TYPE: comment` | `writer.rs:1655` | 500 (pinned by the new test at api.rs:38293) | 400 or 409 |
| OCC lost update | `writer.rs:1663` `"changed since it was read"` | 500 | 409 |

The 500 for a `reviewer.finding` edit is an honest pin of pre-existing behaviour and it does
not leave a partial write — I confirmed the transform runs inside `mutate_file` and
`checked_journal_bytes` (writer.rs:1669) re-parses before any bytes are committed, and the
test's `assert_eq!(std::fs::read(&journal), before_refusals)` after the loop proves the
journal is byte-identical. So: status wart only, no state damage.

The OCC case is the one worth fixing. The rationale for the blanket 500 (api.rs:1759-1762 —
"its text can carry filesystem paths") does not apply to `"journal comment {id} changed
since it was read"`, which carries only the entry id. As it stands the UI cannot tell a
concurrent edit from a server fault, and this commit just demonstrated the cheap pattern
(typed error → mapped status) that would fix it.

### LOW — `crates/orgasmic-daemon/src/api.rs:2403` (audit fidelity)
The two audit fields are drawn from different namespaces. `post_task_comment` stamps
`:ACTOR:` from `identity.member_name()` for a member but from caller-supplied `req.actor`
free text for an admin (api.rs:2348-2351); edit/delete stamp `EDITED_BY`/`DELETED_BY` from
`state.actor`. An admin script that posts as `ci-bot` and later edits records
`:ACTOR: ci-bot` / `:EDITED_BY: <daemon actor>`. Neither request type carries an actor field
(`TaskCommentEditRequest` / `TaskCommentDeleteRequest`, api.rs:2307-2315), so there is no
drift between the wire contract and the handler — the asymmetry is in the stamp source.

The security consequence follows from the same fact: **`:ACTOR:` is not a member-identity
namespace.** `require_comment_body` (writer.rs:1657) compares `entry.actor != *name` as raw
strings, so a member whose `members.org` name collides with an admin-chosen free-text actor
string would pass the authorship check on someone else's comment. There is no stable member
id to key on — `Identity::Member` carries only `name: String` (authz.rs:110). Unreachable
today purely because of the route allow-list (finding 1). If the allow-list is opened, this
needs a decision first: either forbid admins from writing an actor that collides with a
member name, or key authorship on something stable.

I checked the narrower spoofs the brief asked about and found nothing: member names come
from `members.org` via the session cookie, not the request body; the comparison is exact
`String` equality with no case folding, trimming, or unicode normalisation on either side,
so a case or whitespace variant *fails* the check (fails closed, which is the safe
direction). The residual risk is the exact-collision case above.

### LOW — `crates/orgasmic-daemon/src/api.rs:38268` (test)
Two gaps in otherwise good tests:

- The bob refusals assert only `error.status == FORBIDDEN`, not the message. Since 403 is
  also what `authz::require` returns, the test would stay green if the authorship check were
  removed and the 403 arrived from a role change instead. It genuinely exercises the new
  check today — I verified `viewer` holds `TasksComment` (authz.rs:80) and bob's grant is
  `("proj-a","viewer")` with a matching `expected_body`, so authz and OCC both pass and the
  403 can only come from `require_comment_body`. Asserting on the message would pin that.
- No test performs two sequential edits, i.e. the `upsert_comment_property` *replace*
  branch. See Verification Notes — I proved it correct out of tree, but it is unpinned.

## Open Questions

1. Are the three task-comment routes *meant* to be member-reachable? The viewer capability,
   the member-attribution code, and the UI composer all say yes; `MEMBER_ALLOWED_ROUTES`
   says no. This is the one answer that decides whether M4 is really closed.
2. If yes — is there any stable member identifier, or is `name` the identity? If `name` is
   mutable in `members.org`, renaming a member silently transfers or revokes edit rights on
   their existing comments.

## Verification Notes

Everything below I ran myself on the merged tree at `cffb986b`.

- `cargo test -p orgasmic-core --lib node_kernel` → **4 passed**.
- `cargo test -p orgasmic-daemon --lib -- comment` → **18 passed** (log:
  `/tmp/ka934-daemon-comment.log`, exit 0). Reproduces the manager's numbers exactly.
- I did **not** re-run clippy, `fmt --check`, or `ui typecheck` — already established, and
  the brief said not to re-spend on them.

**Out-of-tree probe for the untested `upsert_comment_property` paths.** A unit test could not
be added (read-only review), so I built a throwaway crate at `/tmp/ka934probe` depending on
`crates/orgasmic-core` by path and drove `edit_comment_body` / `tombstone_comment` directly.
Results, all confirmed by re-parsing with `parse_journal`:

- **Two sequential edits** (`alice` then `bob`): `EDITED_BY` count 1, `EDITED_AT` count 1 —
  the replace branch fires, no duplicate. Parses clean; `validate` (node_kernel.rs:109) does
  not trip on the inserted keys since they are outside `REQUIRED`.
- **Legacy entry** with `:EDITED_AT:` but no `:EDITED_BY:` (a pre-KA934 edit): `EDITED_BY` is
  inserted and `EDITED_AT` is replaced, both correct. Key order comes out `EDITED_AT` then
  `EDITED_BY` rather than the fresh-insert order — cosmetic only, nothing depends on drawer
  order.
- **A pre-existing extra** (`:IN_REPLY_TO: tx-0`) survives both edits and the tombstone
  intact; the insert lands immediately before `:END:` at column 0, so no drawer is torn.
- **Tombstone after two edits** keeps `EDITED_BY`/`EDITED_AT`, adds `DELETED_BY`/`DELETED_AT`,
  rewrites `:TYPE:` to `comment.deleted`, and empties the body. Correct.
- A value containing `:` (the org timestamp `[2026-08-22 Sat 13:02:00]`) round-trips through
  both the insert and the replace branch.

**Reviewed by reading, not executed:**

- `writer_comment_error` 403 path does not leak the body: `CommentAuthorshipForbidden`'s
  `Display` (writer.rs:1785) emits only the entry id.
- Check ordering in `require_comment_body` is exists → `ty == "comment"` → authorship → OCC,
  so a non-author learns nothing about the current body. A missing entry 500s rather than
  403s (finding 4) — it does not misreport as forbidden.
- A malformed stamp value cannot corrupt the file: `checked_journal_bytes` (writer.rs:1669)
  re-parses before returning bytes, and `validate_property_value` (tx.rs:538) rejects
  newlines and control characters.
- `edit_journal_comment` / `tombstone_journal_comment` have exactly two callers, both in
  api.rs (2409, 2449) — no sibling caller was left unguarded.
- Artifact comments are not a parallel hole: `/artifacts/:id/comments` supports add and
  resolve only, no edit or delete route, and resolve is deliberately any-member per
  dec_V44E4/dec_KF2MR.
- The member positive path *is* covered: the pre-existing part of the test has member
  `alice` edit then delete her own `tx-comment-1`, and the final assertions check
  `EDITED_BY == "alice"` and `DELETED_BY == "alice"`. The admin path is covered separately on
  `tx-admin-1` against `state.actor`.
- The 403 test's "journal unchanged" assertion is honest: `before_refusals` is captured
  before both refusals and compared after both, then compared again after the automated-row
  loop.
- UI contract: `me.name` comes from `/me` `name` = `identity.member_name()` (api.rs:1073),
  the same string `post_task_comment` writes as `:ACTOR:` for a member — same source, same
  normalisation. One nuance: `useMe`'s `identity` is derived client-side as
  `isMember ? 'member' : 'admin'` (useMe.tsx:155), **not** read from the server's
  `MeIdentity` field. It happens to agree, because no member session means an admin bearer.
  Not a defect, but the field name invites the assumption that it is the server's answer.

**What I did not check:**

- No live-daemon or HTTP-level probe. Every daemon assertion here is from the route table,
  the middleware source, and direct-handler tests. I did not stand up a daemon and issue a
  real member-session request to confirm the 403 ordering end to end — that is the one gap I
  would close if finding 1 is acted on.
- No UI runtime check. I read `TaskDialog.tsx` and `useMe.tsx`; I did not render the dialog
  as a member or as an admin.
- I did not run clippy, fmt, or the ui typecheck (see above), and I ran no workspace-wide
  test.
- I did not read `verify/*/injection.patch`, per the brief.
- I did not audit whether `machines/<id>/tx/` retains the original `:BODY:` after a
  tombstone. It does — `journal_entry` (writer.rs:2783) projects `BODY` into the journal body
  while the tx file keeps the escaped copy — but that is the append-only ledger working as
  designed and is well outside M4.

## Fix Directions

1. Decide finding 1 first; it gates the rest. If the comment routes should be
   member-reachable, add `("POST", "/tasks/:id/comments")`, `.../edit`, `.../delete` to
   `MEMBER_ALLOWED_ROUTES` **in the same change** that resolves the `:ACTOR:` namespace
   question (finding 5) — the authorship check is only sound once actor strings and member
   names cannot collide. If they should not be reachable, drop `TasksComment` from the
   viewer/editor capability lists or stop advertising it in `/me`, so the UI stops offering
   a composer that always fails.
2. Add one arm to `writer_comment_error` for the OCC conflict → 409, mirroring
   `claim_conflict`. Cheapest real improvement in this list, and the pattern is already here.
3. Surface the stamps: add `edited_by` / `edited_at` to `ActivityEntry` and stop dropping
   `comment.deleted` in `activity_entry_from_tx`, so the tombstone is visible as a tombstone
   and the audit trail has a reader.
4. Pin the two test gaps: assert the 403 body text on the bob refusals, and add a
   two-sequential-edits case to the `node_kernel` test (my probe is the shape — the
   assertions that matter are `EDITED_BY` count == 1 and the value being the *second*
   editor).

**APPROVE WITH FOLLOW-UPS.**
