# Review: TASK-KA934.3.1 — `:ACTOR:` namespace guard on the shared tx append paths

Scope: `git diff 2be9f0a0^1 2be9f0a0` (api.rs +189/-49, writer.rs +5/-2) and its direct
neighbours. Read-only; no edits, no live-daemon probes.

## Verdict

**APPROVE WITH FOLLOW-UPS.** No HIGH. One MEDIUM (widened blast radius with no inverse
guard), two LOWs. The three claimed choke points are correct and necessary, the
"structurally admin-only" claim for choke point 1 holds under enumeration, and the
fallback-actor parity the brief asked about is genuinely preserved and proven by an
existing test.

## Findings

### MEDIUM — widened guard now brickable by one `member add`; no inverse guard
`crates/orgasmic-daemon/src/api.rs:3077` and `:8662`
(filed: tx-20260902-orgasmic-7060)

The guard moved from "comments only" to "every `event_routes_to_journal` type"
(api.rs:8867 — `task.created`, `task.state_transitioned`, `task.property_updated`,
`task.edited`, all `graph.*.created|edited`, `reviewer.finding`, `review.verdict`,
`artifact.*`). Every one of the 16 `record_api_tx`/`prepare_api_tx` call sites passes
`actor: None`, so `choose_actor` (api.rs:9009) falls through to `state.manager_actor`
then `state.actor`. `state.actor` defaults to `$USER` (`lib.rs:268`).

Failure scenario, fully deterministic from the code:
1. Operator runs the daemon as `$USER = aspirational` (no `--actor`, no `manager_actor`).
2. Operator adds a member for their own phone: `orgasmic member add aspirational ...`.
   `add_member_locked` (`orgasmic-core/src/members.rs:186`) validates only the character
   class and duplicate names — nothing compares against the daemon actor.
3. `POST /projects/:id/tasks` → `prepare_api_tx` at api.rs:17684 with
   `ty: "task.created"`, `actor: None` → `choose_actor` yields `"aspirational"` →
   `event_routes_to_journal("task.created") == true` and `identity == Admin` →
   `ensure_actor_namespace_free` → **403** on every task create, every state transition,
   every property update, and every graph node write.

Before this diff the same collision cost only comments. Now it takes the daemon's whole
write surface down. The refusal is clean (fires before any durable write, no corruption)
and the message names the collision, but nothing warns at the moment the operator creates
the collision, and the CLI that creates it (`orgasmic-cli/src/member.rs:78`) is the only
place a human is present.

Not a HIGH: admin-triggered, fail-closed, no data loss, and it is the literal semantic
dec_Q78QN asks for. It needs the inverse guard, not a revert.

### LOW (test) — the new test's anti-forgery assertion is dead code
`crates/orgasmic-daemon/src/api.rs:39085` (filed: tx-20260902-orgasmic-7061)

```rust
let journal = root_a.join(".orgasmic/tasks/TASK-001/journal.org");
if journal.exists() { ... assert!(!contents.contains(":ACTOR: alice")) }
```
`seed_two_projects` (api.rs:38498) writes `project.org`, the task node file and the board
— never `journal.org` — and the refused `POST /tx` is the first journal-routed request in
the test. So `journal.exists()` is false and the assertion never runs. The
`StatusCode::FORBIDDEN` assertion above it carries the whole test. Fix: drop the `if` and
assert `!journal.exists()`, which is the actual claim ("nothing was written").

### LOW (consistency) — ten `artifact.*` journal producers bypass all three choke points
`crates/orgasmic-daemon/src/api.rs:19525` (filed: tx-20260902-orgasmic-7062)

`artifact.*` is journal-routed (api.rs:8892), but ten producers construct `TxEntry::new`
directly and go straight to `writer.transaction`: api.rs:19129, 19162, 19251, 19525,
19646, 20111, 20268, 20363, 20545, 20775. I checked the actor argument of every one —
all pass `&state.actor`, never a client-supplied value, so there is **no forgery hole**.
The only observable effect is asymmetry: with a colliding daemon actor, artifact journals
still stamp `:ACTOR: alice` while task journals refuse. `:ACTOR:` grants no rights in the
artifact path (comment edit/resolve keys off CID + `AUTHOR`), so impact is cosmetic.

## Attack points from the brief — results

**1. Is "structurally admin-only" true for choke point 1?** Yes. Three callers of
`prepare_tx_append_request`:
- `append_tx_request` (api.rs:3014) — `POST /tx` and `/runs/:id/release`. Neither appears
  in `MEMBER_ALLOWED_ROUTES` (api.rs:897-921; I read all 24 entries).
- `append_task_claim_event` (api.rs:7519) — types are `orgasmic_core::claims::CLAIMED` /
  `RELEASED` = `"task.claimed"` / `"task.claim_released"` (`claims.rs:8-9`), neither in
  the `event_routes_to_journal` match, so the guard never fires there.
- dispatch close (api.rs:18427) — admin-only route.
No member-reachable path reaches it. No false-positive regression for members.

**2. Widened scope — is there a legitimate producer passing a member-like actor?** No.
I read the `ty`/`actor` of all 16 `record_api_tx`/`prepare_api_tx` call sites (2660, 4248,
4553, 5157, 7490, 7661, 7915, 8176, 8492, 14683, 16739, 16842, 17684, 17945, 18269,
18758): every single one is `actor: None`. Nothing in the daemon deliberately stamps a
member name on an admin tx. The only way the widened guard fires is the daemon/manager
actor itself colliding — which is the MEDIUM above.

**3. Choke point 2 exemption by identity.** Confirmed. `record_api_tx_as` /
`prepare_api_tx_as` have exactly one non-Admin caller: `post_task_comment` (api.rs:2426),
and its actor is `match identity.member_name() { Some(name) => Some(name.to_string()),
None => req.actor... }` — a member cannot supply a foreign actor, the session name is
forced. All 8 pre-existing `prepare_api_tx` callers and `record_api_tx_after_project_mutation`
delegate with `&Identity::Admin`, so they are unchanged.

**4. Parity with the old guard.** Holds, and slightly improves.
- Same fallback chain: old `post_task_comment` did `requested → manager_actor → state.actor`;
  `choose_actor` (api.rs:9014-9018) does the same.
- **Fallback case still covered.** The pre-existing test
  `admin_comment_actor_colliding_with_member_name_refused` (api.rs:38946) boots with
  `options.actor = "alice"` and asserts 403 for the actor-omitted admin comment, plus
  edit and delete, plus a passing member session. It survived the refactor unchanged and
  is in the gate filter — that is the parity proof, not an inference.
- Trim: the old handler guarded the **untrimmed** `req.actor` while `choose_actor` trimmed
  before stamping, so `" alice "` slipped past the old guard and landed as `:ACTOR: alice`.
  The new guard runs on the `choose_actor` output, closing that hole. Improvement, not a
  regression.
- Guard fires before any durable write in both `prepare_*` functions; same
  `ApiError::forbidden` text (single `ensure_actor_namespace_free`, api.rs:2361).

**5. Nothing else moved.** Confirmed. Every api.rs hunk is one of: the `warn!` on
`read_members` failure, `comment_mutation_actor`, the three handler-copy deletions, the
two `_as` variants, the two guard insertions, the new test. writer.rs is the doc sentence
only.

## Acceptance criteria

- [x] Admin `POST /tx` type=comment with actor == member name refused 403 — new test
      `admin_post_tx_journal_actor_colliding_with_member_name_refused` (api.rs:39048)
      drives the real router through `post_tx` → `append_tx_request` →
      `prepare_tx_append_request`, i.e. choke point 1, not a unit shim.
- [x] Member session comments still work — asserted in both the new and the pre-existing test.
- [x] No local guard copy in the three comment handlers — `ensure_actor_namespace_free`
      now has exactly 3 call sites (api.rs:2381 inside the shared `comment_mutation_actor`,
      3077, 8662); zero inside a handler body.
- [x] `warn!` on members.org read failure (api.rs:2350-2357), fail-open preserved and commented.
- [x] writer.rs doc sentence on the inverse rename case (writer.rs:1690-1693).

Note on scope: the assignment said "move the guard to `prepare_tx_append_request`" alone.
That is not sufficient — `post_task_comment` routes through `prepare_api_tx`, and
edit/delete call `writer.edit_journal_comment` / `tombstone_journal_comment` directly
without any tx prepare (api.rs:2468-2479). Three choke points is the correct reading of
the intent, not scope drift.

## Open questions

1. Should `orgasmic member add` refuse (or at least warn on) a name equal to `$USER` /
   configured `manager_actor`? Refusing is safer but can't see the running daemon's
   `--actor` override; warning is always possible. Operator call.
2. `manager_actor` comes from config (`config.rs:154`). If an operator sets it to a human
   name that is also a member, same lockout with a different trigger. Same fix covers both.

## Verification notes

What I actually did:
- Read the full diff for both files.
- Read `event_routes_to_journal` (api.rs:8867), `choose_actor` (api.rs:9009),
  `MEMBER_ALLOWED_ROUTES` (api.rs:897), `ensure_actor_namespace_free` (api.rs:2342),
  `claims.rs:8-9`, `members.rs:44-143,165-215`, `lib.rs:268`.
- Enumerated all 3 `prepare_tx_append_request` callers and all 16
  `record_api_tx`/`prepare_api_tx` callers, checking `ty` and `actor` at each.
- Enumerated all 20 `TxEntry::new` sites in the daemon and checked the actor argument of
  every non-test one.
- Read both guard tests in full and traced `seed_two_projects`.

What I did **not** check (per brief, already established — implementer 63 daemon lib tests
+ clippy `-D` + fmt; manager re-ran on merged main `2be9f0a0`, 70 passed / 0 failed):
- I did not re-run the gate suite or any test. All behavioral claims above are code-trace
  derivations plus the two existing tests' assertions, not fresh execution.
- The MEDIUM is derived from code, not executed. It is a straight-line derivation
  (api.rs:17684 `ty:"task.created"` → api.rs:8662 guard → api.rs:8875 match arm) with no
  branch I could not read, but a test proving it does not exist and I could not add one
  under the read-only rule. Residual risk: low, but non-zero.
- I did not probe the live daemon on :4848 (old runtime, forbidden by the brief).
- I did not review the KA934.3 parent work, the writer.rs comment-mutation internals
  beyond the doc hunk, or the UI side.

## Fix directions

1. **MEDIUM** — add the inverse guard where the collision is created:
   `orgasmic-cli/src/member.rs:78`, warn (or refuse with `--force`) when `name` equals
   `std::env::var("USER")` or the configured `manager_actor`. One `eprintln!` + one
   comparison. Cheaper and clearer than any daemon-side mitigation, because it catches the
   operator at the moment they can still pick a different name.
2. **LOW/test** — api.rs:39085: replace the `if journal.exists()` block with
   `assert!(!journal.exists(), "refusal wrote a journal")`.
3. **LOW/consistency** — no code change recommended. If it is ever worth unifying, the
   move is a small `journal_tx_entry(state, ty, time)` helper wrapping
   `ensure_actor_namespace_free` + `TxEntry::new` for the ten artifact producers; not
   worth the diff today since none of them takes a client actor.

**APPROVE WITH FOLLOW-UPS.**
