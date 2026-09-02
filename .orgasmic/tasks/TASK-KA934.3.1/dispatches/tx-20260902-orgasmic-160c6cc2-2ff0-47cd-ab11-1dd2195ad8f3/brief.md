# Review: TASK-KA934.3.1 — `:ACTOR:` guard on the shared tx append paths (narrow)

Implementer: opencode / zai-coding-plan/glm-5.3-flash (variant max), one commit `14314e66`,
merged to main as `2be9f0a0`. Answers the MEDIUM + 2 LOWs of the KA934.3 review
(tx-bfe6e70a). Read `orgasmic task get --project orgasmic TASK-KA934.3.1` and `dec_Q78QN`.

    git diff 2be9f0a0^1 2be9f0a0     # api.rs (+~190/-~50), writer.rs (+5)

Keep this review to the diff and its direct neighbours.

## What this round claims
The four journal-comment producers share no single function, so the guard sits at three
choke points, all calling the one primitive `ensure_actor_namespace_free`:
1. `prepare_tx_append_request` (~:3076): guard on the `choose_actor` chain when
   `event_routes_to_journal(&type)`; NO identity parameter — claimed structurally admin-only
   (`POST /tx`, `/runs/:id/release` not in `MEMBER_ALLOWED_ROUTES`; `append_task_claim_event`
   uses non-journal types).
2. New `prepare_api_tx_as` (~:8602, guard ~:8661): gated on
   `identity.member_name().is_none() && event_routes_to_journal(&ty)`; old `prepare_api_tx`
   delegates as Admin so its 8 callers are unchanged; `post_task_comment` uses the new `_as`
   path.
3. New `comment_mutation_actor` (~:2372) for edit/delete `:EDITED_BY:`/`:DELETED_BY:`.
The three handler-level copies are deleted. `read_members` failure now `warn!`s and fails
open. writer.rs doc gains the inverse-rename sentence. New test
`admin_post_tx_journal_actor_colliding_with_member_name_refused` drives the real router.

## Attack these specifically
- **Is "structurally admin-only" true for choke point 1?** Enumerate every caller of
  `prepare_tx_append_request` and every route that reaches them; compare against
  `MEMBER_ALLOWED_ROUTES`. If any member-reachable path reaches it, a member's own-name tx
  would be refused (false positive) — that is a regression.
- **Widened scope.** Point 1 now guards EVERY journal-routed admin tx type (`task.created`
  etc.), not just comments. Is there an internal/daemon producer that passes a member-like
  actor legitimately (e.g. a manager acting on behalf of a member, `agent.*` actors, dispatch
  bookkeeping) and would now 403 or fail a background write? Grep the tx types that route to
  journals and their producers.
- **Choke point 2 exemption by identity.** A member identity is exempt regardless of the
  actor string; today only `post_task_comment` passes member identity and forces the session
  name — confirm, and confirm no `_as` caller lets a member supply a foreign actor.
- **Parity with the old guard.** Same effective actor (requested → manager_actor →
  state.actor), same trim, guard fires before any durable write, same 403 text. The old
  `post_task_comment` also guarded the FALLBACK actor when `req.actor` was omitted — does
  `prepare_api_tx_as` still cover that case?
- **Nothing else moved.** Two files; every hunk should be one of the bullets.

Classify precisely; if only LOWs remain, say so and APPROVE (with follow-ups if any).

Already established — do not re-spend: implementer gates (63 daemon lib, clippy, fmt);
manager re-ran on merged main `2be9f0a0` (task Evidence).

## Rules
- READ-ONLY. No edits, no git writes, no mutating `orgasmic` verbs, nothing against the live
  ledger at `~/.orgasmic/ledgers/orgasmic` beyond read-only git reads. The live daemon on
  :4848 runs an OLD runtime — do not probe it.
- File each finding as it appears:
  `orgasmic tx record --project orgasmic --type reviewer.finding --task TASK-KA934.3.1
  --reason "HIGH|MEDIUM|LOW <file:line> — <one sentence>"` (single line).
- Targeted tests only; NEVER the whole `orgasmic-cli` suite unfiltered; never the workspace;
  never `ORGASMIC_HOME`; never `daemon start`; do not read `verify/*/injection.patch`; never run
  `legacy_drivers_and_explicit_pairs_emit_equivalent_start_events`.
- Say what you did not check. Finish with `orgasmic dispatch finalize --summary-file <path>`
  (report only) and end with the explicit verdict sentence:
  APPROVE / APPROVE WITH FOLLOW-UPS / REJECT.
