# Review TASK-KA934.3 — task-comment routes member-reachable; `:ACTOR:` guarded

## Verdict

**APPROVE WITH FOLLOW-UPS.** The three acceptance criteria are met on the routes the task
names. No HIGH. One MEDIUM completeness gap (the guard is one route deep) and three LOWs,
all follow-up material rather than blockers.

## Findings

### MEDIUM — `crates/orgasmic-daemon/src/api.rs:2991` (`post_tx`) — the namespace guard is one route deep
`ensure_actor_namespace_free` is called only from the three comment handlers. `POST /tx`
reaches the identical journal write with no guard:

- `event_routes_to_journal("comment") == true` (api.rs:8824), so `tx_destination` →
  `node_journal_path` (api.rs:8846) points the append at the task's `journal.org`
  (api.rs:8925).
- `choose_actor` (api.rs:8960) honours `req.actor` verbatim (trimmed), same as the
  comment route.
- `LedgerWriter::append` (writer.rs:2784) converts the `TxEntry` via `journal_entry`
  (writer.rs:2860) and writes `:ACTOR: <requested>` into the drawer.
- The resulting entry has `ty == "comment"`, so `require_comment_body` (writer.rs:1691)
  treats it as an editable comment and hands edit/delete rights to the named member's
  session — exactly the forgery `dec_Q78QN`'s doc comment (api.rs:2333) says the guard
  prevents.

Reproducer shape (admin credential, not run — see Verification Notes):
`orgasmic tx record --type comment --task TASK-001 --project <p> --actor <member-name>
--extra BODY=forged` (`--actor` is a first-class CLI flag, cli/src/main.rs:915).

Impact is bounded by trust: `POST /tx` is admin-only (absent from `MEMBER_ALLOWED_ROUTES`)
and an admin already holds `OrgWrite`, so this is not privilege escalation — it is an
accident/consistency gap that makes the guard's stated invariant false as written. Fix
direction: call `ensure_actor_namespace_free` once in `prepare_tx_append_request`
(api.rs:3023) — or in `choose_actor`'s caller — gated on `event_routes_to_journal(&ty)`,
so every producer of a journal `:ACTOR:` routes through one check instead of three
copies. That also subsumes the artifact-comment LOW below.

### LOW — `crates/orgasmic-daemon/src/writer.rs:1683` — rename doc covers only one direction
The new doc comment states that a renamed member loses rights on comments under the old
name. The inverse is undocumented and unguarded: `add_member` (core/src/members.rs:198)
rejects a duplicate name but not a *retired* one, so re-adding or renaming a member into a
name a previous member used hands the newcomer edit/delete on every comment the old holder
left — raw `:ACTOR:` equality is the only authorship test. Fix direction: one more
sentence on the doc comment, or refuse to mint a name that appears as an `:ACTOR:` in any
live journal (the latter is almost certainly not worth it).

### LOW — `crates/orgasmic-daemon/src/api.rs:2349` — guard fails open, silently
`read_members(&state.home).map(..).unwrap_or(false)`: an unparseable or unreadable
`members.org` disables the guard with no log line. `members.org` is admin-owned and a
parse failure would already break member login, so severity is genuinely LOW — but the
`unwrap_or(false)` deserves at least a `warn!` so the disabled guard is observable.

### LOW — `crates/orgasmic-daemon/src/api.rs:847` — artifact comments unguarded
`POST /artifacts/:id/comments` shares the member-attribution pattern and is not
namespace-guarded. Implementer-disclosed, out of scope for this task, same class as the
MEDIUM. Folding it into a single `prepare_tx_append_request` guard closes both at once.

## What I verified

**Allow-list templates — exact match, confirmed.** Router (api.rs:735, 737, 741) declares
`/tasks/:id/comments`, `/tasks/:id/comments/:entry_id/edit`,
`/tasks/:id/comments/:entry_id/delete`; `MEMBER_ALLOWED_ROUTES` (api.rs:905-907) lists the
same three strings byte for byte, in the app-relative form `identity_middleware` compares
after stripping `/api` (api.rs:963-975). No near-miss.

**The test really goes through the router.** Both tests boot a real daemon
(`crate::Daemon::run`), acquire a session by `POST /api/login` with a minted member token
(`member_session_cookie`, api.rs:39034), and drive every assertion with `reqwest` against
`http://{addr}/api/...` carrying that cookie. This is a genuine rewrite of the old
handler-level test, not a bypass — the old version called `post_task_comment(...)` with a
hand-built `Extension(Identity::Member{..})`, which is exactly what could not have caught
a missing allow-list entry.

**Guard string == stamped string.** `post_task_comment` (api.rs:2383-2389) computes
`requested → state.manager_actor → state.actor`; `choose_actor` (api.rs:8960) applies the
identical chain, and `non_empty` (api.rs:8971) trims — matching the guard's own
`actor.trim()` (api.rs:2343). Edit/delete stamp `state.actor` directly (no `manager_actor`
fallback) and the guard checks `state.actor` directly. No trim/case/fallback divergence.

**No newline-injection bypass.** I chased whether an admin `actor` of `"alice\n:FOO: x"`
could parse back to `:ACTOR: alice` and slip past the whole-string comparison. It cannot:
`journal_entry_block` interpolates `actor` unescaped (core/src/node_kernel.rs:187), but
`JournalEntry::validate` (node_kernel.rs:109) round-trips through
`TxEntry::validate` → `validate_property_value` (core/src/tx.rs:544), which rejects `\n`,
`\r`, and control characters in any property value before a byte is written. Hypothesis
disproved, not a finding.

**Member path never hits the guard.** All three handlers match
`identity.member_name()` first and only enter the guarded branch on `None`
(api.rs:2383, 2447, 2492). Test 1 exercises this: alice creates, edits and deletes her own
comment while `add_member` has her in `members.org`.

**Member authorization is not over-opened.** `viewer` carries `TasksComment`
(authz.rs:83), so a viewer commenting is the intended table entry, not a regression; the
`artifacts` role does not (authz.rs:93) and is still refused inside the handler by
`resolve_authorized_task(.., Action::TasksComment)`. The allow-list is coarse by design
(api.rs:884-890) and the per-project gate still runs.

**Operational blast radius, confirmed and tested.** With `state.actor` equal to a member
name, admin create-with-omitted-actor, edit, and delete all 403 — test 2 asserts exactly
these three. Message names the offending actor and the namespace
(`actor \`alice\` collides with a members.org member name; member names are reserved for
member sessions`), which is actionable. Task Notes record the live check: one member
(`Victor`) vs daemon actor `aspirational` — no collision, so the upgrade does not break
admin comments in production.

**Nothing else moved.** `git diff 9f6874f0^1 9f6874f0 --stat` = 2 files, +445/-237. The
writer.rs delta is 8 lines, all doc comment. The api.rs non-test delta is 4 hunks
(allow-list, the new fn, and the three handler branches); the rest is the rewritten test
plus the new one. UI is untouched and already calls all three routes
(ui/src/lib/api.ts:171, 185, 197), so there is no frontend/backend contract drift.

## What I did NOT check

- **I did not re-run the gate suite.** The brief marks the 58 daemon lib tests, clippy
  `-D warnings` and `cargo fmt --check` as already established by both implementer and
  manager on merged `9f6874f0`. I took those as given and spent the budget on the diff.
- **The MEDIUM has no executed probe.** Proving it needs either a new test (I am
  read-only) or a live `POST /tx` against a daemon — and the brief forbids probing the
  :4848 daemon and forbids `ORGASMIC_HOME`. The finding rests on a complete static trace
  through `post_tx` → `prepare_tx_append_request` → `tx_destination` → `node_journal_path`
  → `LedgerWriter::Journal` → `journal_entry`, with every hop cited above. Residual risk:
  some validation I did not find rejects `type=comment` on the raw `/tx` route. I looked
  for one (`validate_property_value`, `event_routes_to_journal`, `JournalEntry::validate`)
  and found none.
- **Session/cookie security itself** (`create_member_session`, cookie flags, expiry) —
  pre-existing, untouched by this diff.
- **Concurrency between the guard read and the write.** `read_members` is unlocked and a
  member could be added between the check and the append. The window is microseconds and
  the outcome is one unguarded comment; not worth a finding, noted for completeness.
- **The `%2F`-in-task-id path-matching question** visible in
  `ui/src/lib/__tests__/taskCommentsApi.test.ts:21` — pre-existing, unrelated to this
  change.

## Open Questions

1. Should the `:ACTOR:` guard move to `prepare_tx_append_request` so `POST /tx` and the
   artifact comment route inherit it (closing the MEDIUM and one LOW together), or is
   `dec_Q78QN` deliberately scoped to the task-comment surface only? The decision's
   framing ("`:ACTOR:` is a guarded identity namespace") reads as the former.
2. Is member-name reuse after revocation something the product wants to allow at all? If
   not, a duplicate check against retired names in `add_member` is cheaper than any
   journal-side migration.

## Fix Directions

- **MEDIUM:** hoist `ensure_actor_namespace_free` into `prepare_tx_append_request`
  (api.rs:3023), called when `event_routes_to_journal(&req.r#type)` — one guard, all
  producers, and the three handler-level copies can then be deleted.
- **LOW (rename):** extend the writer.rs:1683 doc comment with the name-reuse direction.
- **LOW (fail-open):** replace `.unwrap_or(false)` with a match that `warn!`s the read
  error before allowing the write.
- **LOW (artifacts):** subsumed by the MEDIUM fix; otherwise a separate follow-up task.

APPROVE WITH FOLLOW-UPS.
