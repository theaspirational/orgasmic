# Review: TASK-KA934.3 — task-comment routes member-reachable; `:ACTOR:` guarded

Implementer: opencode / zai-coding-plan/glm-5.3 (variant max), one commit `3181651c`, merged to
main as `9f6874f0`. Implements `dec_Q78QN`. Read `orgasmic task get --project orgasmic
TASK-KA934.3` and the decision.

    git diff 9f6874f0^1 9f6874f0     # api.rs (+445/-237, mostly one rewritten test), writer.rs (+8)

## What this round claims
- Three task-comment routes added to `MEMBER_ALLOWED_ROUTES` (api.rs ~:905), templates copied
  from the router (~:735-742).
- `ensure_actor_namespace_free` (~:2340): 403 when an admin-effective actor equals a
  `members.org` member name (`orgasmic_core::read_members`, `$ORGASMIC_HOME/user/auth/members.org`).
  Applied in `post_task_comment` on the EFFECTIVE actor (req.actor → manager_actor → state.actor,
  same chain `choose_actor` uses at stamp time), and on `state.actor` in edit/delete.
- Rename semantics pinned as a doc comment on `require_comment_body` (writer.rs ~:1683).
- `task_comments_use_member_session_attribution_and_refresh_activity` rewritten to drive the real
  router + identity middleware over HTTP with a member session cookie; new
  `admin_comment_actor_colliding_with_member_name_refused`.

## Attack these specifically
- **Allow-list templates.** Do the three strings match the router's templates EXACTLY (param
  names, trailing segments, the app-relative form `identity_middleware` compares after stripping
  the prefix at ~:963)? A near-miss keeps the 403 and the new test would only pass if it
  bypasses the middleware — confirm the test really goes through the router with a member cookie.
- **Guard placement vs stamp.** Is the string the guard checks the same string that gets
  stamped as `:ACTOR:` / `:EDITED_BY:` / `:DELETED_BY:`? Trace `choose_actor` and the writer
  side; any divergence (trim, case, fallback order) is a hole.
- **Member path untouched.** A member session must never hit the guard (their own name IS a
  member name). Confirm `identity.member_name()` short-circuits before it in all three handlers.
- **Fail-open on read error.** `read_members(...).unwrap_or(false)`: a corrupt members.org
  disables the guard silently. Size it (LOW vs more) — members.org is admin-owned.
- **Operational blast radius.** With a daemon actor equal to a member name, every admin comment
  mutation becomes 403. Is the message actionable? Manager note: the live members.org
  (`~/.orgasmic/user/auth/members.org`) and the daemon actor were checked by the manager — see
  the task Notes for whether they collide today.
- **Artifact comments.** `POST /artifacts/:id/comments` shares the attribution pattern and is
  NOT guarded (implementer disclosed). Out of scope; size it as a follow-up.
- **Nothing else moved.** Two files; the large api.rs delta should be the rewritten test.

Classify precisely; if only LOWs remain, say so and APPROVE (with follow-ups if any).

Already established — do not re-spend: implementer gates (58 daemon lib tests, clippy, fmt);
manager re-ran the same on merged main `9f6874f0` — see the task Evidence.

## Rules
- READ-ONLY. No edits, no git writes, no mutating `orgasmic` verbs, nothing against the live
  ledger at `~/.orgasmic/ledgers/orgasmic` beyond read-only git reads. The live daemon on
  :4848 runs an OLD runtime — do not probe it; not a defect.
- File each finding as it appears:
  `orgasmic tx record --project orgasmic --type reviewer.finding --task TASK-KA934.3
  --reason "HIGH|MEDIUM|LOW <file:line> — <one sentence>"` (single line).
- Targeted tests only; NEVER the whole `orgasmic-cli` suite unfiltered; never the workspace;
  never `ORGASMIC_HOME`; never `daemon start`; do not read `verify/*/injection.patch`; never run
  `legacy_drivers_and_explicit_pairs_emit_equivalent_start_events`.
- Say what you did not check. Finish with `orgasmic dispatch finalize --summary-file <path>`
  (report only) and end with the explicit verdict sentence:
  APPROVE / APPROVE WITH FOLLOW-UPS / REJECT.
