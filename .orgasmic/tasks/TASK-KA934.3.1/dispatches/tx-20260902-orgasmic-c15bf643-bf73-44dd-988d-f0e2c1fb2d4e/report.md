# TASK-KA934.3.1 — Hoist the `:ACTOR:` namespace guard into the shared tx append path

Commit `14314e66` on `task-ka934.3.1-impl` (`TASK-KA934.3.1: fix(api): guard the :ACTOR: namespace on the shared tx append paths`).

## Changed

All in `crates/orgasmic-daemon/src/api.rs` unless noted.

**MEDIUM — guard on the shared tx append path.** The four journal-`comment` producers do NOT all pass through `prepare_tx_append_request` (see "Shared-function finding" below), so the guard now lives at three choke points, all delegating to the single `ensure_actor_namespace_free` primitive:

1. `prepare_tx_append_request` (api.rs:3076): guard on the effective `choose_actor` chain, fired when `event_routes_to_journal(&req.r#type)`. No identity parameter needed: every path into this function is admin-only (`POST /tx`, `/runs/:id/release` are absent from `MEMBER_ALLOWED_ROUTES`, enforced by `identity_middleware`) or daemon-internal (`append_task_claim_event` uses `CLAIMED`/`RELEASED` types, which are not journal-routed). Documented at the call site. Covers `POST /tx` — the review's MEDIUM.
2. `prepare_api_tx_as` (api.rs:8602, guard at :8661): same guard on the same `choose_actor` chain, gated on `identity.member_name().is_none() && event_routes_to_journal(&req.ty)`. Member sessions are exempt (their handler forces the session name — the one legitimate producer). Old `prepare_api_tx` (:8594) delegates with `&Identity::Admin`, so its 8 other callers are untouched. Reached via new `record_api_tx_as` (:8544) → `record_api_tx_after_project_mutation_as` (:8560); the non-`_as` wrappers keep their signatures, so the 9 `record_api_tx*` callers and `post_org_file` (OFF LIMITS, parallel TASK-JWHXH.3.1) are untouched. Covers the admin create-comment path.
3. New `comment_mutation_actor` (api.rs:2372): shared constructor for the `:EDITED_BY:`/`:DELETED_BY:` actor used by `post_task_comment_edit` and `post_task_comment_delete` — one guard instead of a copy per handler (those writes are direct writer mutations, not tx appends; there is no tx-append function downstream of them).

**Deleted (the three handler-level guard copies):**
- `post_task_comment`: the effective-actor guard computation deleted; admin branch is now just `req.actor.filter(|v| !v.trim().is_empty())` (that value was already the request payload; only the guard needed the fallback chain). Handler switched to `record_api_tx_as(&state, &identity, …)`.
- `post_task_comment_edit` / `post_task_comment_delete`: the `ensure_actor_namespace_free(&state, &state.actor)?` match arms replaced by `comment_mutation_actor(&state, &identity)?`.

**LOW — warn on members.org read failure** (api.rs:2347-2358): `read_members(...).unwrap_or(false)` → explicit match that `tracing::warn!`s the error and still fails open, with a comment on why.

**LOW — writer.rs:1690 doc**: `require_comment_body` doc gains the inverse-rename sentence: a member re-added or renamed INTO a retired member's name inherits edit/delete on that member's old comments (raw `:ACTOR:` equality; accepted).

**New test** `admin_post_tx_journal_actor_colliding_with_member_name_refused` (api.rs:39040): drives the real router with the admin bearer token — admin `POST /tx` `type=comment`, `task=TASK-001`, `actor=alice` → 403 naming the collision; journal checked for no forged `:ACTOR: alice`; same tx with `actor=daemon-admin` → 200 (guard does not overfire); member-session comment via cookie → 200.

### Shared-function finding (required by the brief)

No single function is shared by all four producers: create → `prepare_api_tx`, `POST /tx` → `prepare_tx_append_request`, edit/delete → `writer.edit_journal_comment`/`tombstone_journal_comment` (no tx append at all). The brief's fallback was applied as: guard in each producer family's smallest shared choke point (the three listed above), all delegating to the one `ensure_actor_namespace_free` primitive.

### Behaviour parity

Effective actor checked is unchanged (requested actor → `manager_actor` → `state.actor` via `choose_actor`); the guard still fires before any durable write (in prepare, before `writer.append_tx`); 403 body/message unchanged (`actor \`X\` collides with a members.org member name…`). Non-comment journal-routed admin txs (`task.created` etc.) are now also guarded — the intended tightening; internal daemon types (`manager.dispatch_started`, claims) are not journal-routed and are unaffected.

## Verification Gates

| Gate | Command | Result | Log |
|---|---|---|---|
| Test | `cargo test -p orgasmic-daemon --lib -- comment member identity authz tx_append post_tx` | PASS — `test result: ok. 63 passed; 0 failed; … 787 filtered out` | `/var/folders/9p/823z6j817xj9ts2xpvnx1q_40000gn/T/opencode/ka9343/test-gate-final.log` |
| Clippy | `cargo clippy -p orgasmic-daemon --all-targets -- -D warnings` | PASS — `Finished 'dev' profile`, exit 0 | `/var/folders/9p/823z6j817xj9ts2xpvnx1q_40000gn/T/opencode/ka9343/clippy-gate-2.log` |
| Fmt | `cargo fmt --all --check` | PASS — exit 0 (one arm-width fix applied after first run) | `/var/folders/9p/823z6j817xj9ts2xpvnx1q_40000gn/T/opencode/ka9343/fmt-gate-2.log` |

Key tests confirmed green in the log: `admin_comment_actor_colliding_with_member_name_refused` (existing, unchanged), `admin_post_tx_journal_actor_colliding_with_member_name_refused` (new), `task_comments_use_member_session_attribution_and_refresh_activity` (member session path).

Off-limits respected: no changes under `crates/orgasmic-cli/**`, `shipped/**`; `post_org_file`/`reject_ledger_rewrite` untouched (signature-stable wrappers kept them so); artifact comment routes untouched.

## Unmet Criteria

None.

## Residual Risk

- `prepare_tx_append_request` guards without an identity parameter (structurally admin-only today). A future member-reachable route that reaches it would bypass the member exemption and could get a member's own-name tx refused — the call-site comment names this invariant.
- `prepare_api_tx_as` exempts by identity, not by "actor == session name": a future member-reachable producer that lets members supply arbitrary actors could write a DIFFERENT member's name unguarded. Only `post_task_comment` (which forces the session name) passes member identity today.
- Edit/delete `:EDITED_BY:`/`:DELETED_BY:` stamps remain guarded via `comment_mutation_actor` rather than in the writer itself; a new writer-level caller of `CommentMutationActor::Admin` would not inherit the guard.
- Read-failure of `members.org` still fails open (per brief), now with a `warn!`.
