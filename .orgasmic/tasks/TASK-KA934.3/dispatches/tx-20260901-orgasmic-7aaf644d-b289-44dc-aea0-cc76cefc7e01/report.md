# TASK-KA934.3 — task-comment routes member-reachable; `:ACTOR:` guarded

Commit: `3181651c` on branch `task-ka934.3-impl` (one commit, as ordered).

## Changed

- `crates/orgasmic-daemon/src/api.rs:905-907` — added the three task-comment
  routes (`POST /tasks/:id/comments`, `.../:entry_id/edit`, `.../:entry_id/delete`,
  exact router templates from api.rs:735-742) to `MEMBER_ALLOWED_ROUTES`, so
  `identity_middleware` no longer 403s `Identity::Member` before the handler;
  the per-project gate still runs through `authz::require(Action::TasksComment)`.
- `crates/orgasmic-daemon/src/api.rs:2334-2356` — `ensure_actor_namespace_free`:
  dec_Q78QN guard; refuses (403) an actor string that equals a `members.org`
  member name (`orgasmic_core::read_members`), message names the collision.
- `crates/orgasmic-daemon/src/api.rs:2377-2392` — `post_task_comment` admin
  branch guards the *effective* actor: the request-supplied `req.actor` when
  present, else the same `manager_actor`→`state.actor` fallback chain
  `choose_actor` applies at stamp time.
- `crates/orgasmic-daemon/src/api.rs:2432-2441`, `:2480-2489` —
  `post_task_comment_edit`/`post_task_comment_delete` admin branches guard
  `state.actor` (the string about to become `:EDITED_BY:`/`:DELETED_BY:`).
  Refusal chosen (not override) per brief; the message names the colliding
  member so the operator can rename or fix daemon config.
- `crates/orgasmic-daemon/src/writer.rs:1683-1690` — doc comment on
  `require_comment_body` (the KA934.1 authorship check): authorship is the raw
  stored `:ACTOR:` string; a renamed member immediately loses edit/delete
  rights on comments made under the old name; accepted, no migration.
- Tests (`crates/orgasmic-daemon/src/api.rs`, daemon lib):
  - `task_comments_use_member_session_attribution_and_refresh_activity` —
    rewritten to drive the REAL router + identity middleware over HTTP with a
    member session cookie (`POST /login` → cookie): member create (spoofed body
    actor ignored, `:ACTOR:` = session name, journal + activity verified),
    admin colliding `req.actor:"alice"` → 403 naming the collision, admin
    non-colliding actor create → 200, bob cross-author edit/delete → 403,
    member edit → 200 then OCC stale edit/delete → 409, seeded
    `reviewer.finding` row not editable (500), admin edit/delete of its own
    comment → 200 with `daemon-admin` stamps, member delete of own comment →
    200 with `alice` stamps, tombstone visible via `/tasks/:id/activity`.
    This in-test HTTP round-trip is the live member-session probe (the :4848
    daemon runs an old runtime and was not touched).
  - `admin_comment_actor_colliding_with_member_name_refused` — daemon actor
    configured as "alice" (a member): member session path still works; admin
    create with explicit colliding actor, admin create with actor omitted
    (fallback collision), and admin edit/delete (stamp collision) each refused
    403 with the collision message.

## Verification Gates

All logs under `/var/folders/9p/823z6j817xj9ts2xpvnx1q_40000gn/T/opencode/ka934.3-logs/`
(final post-fmt runs unless noted):

- `cargo test -p orgasmic-daemon --lib -- comment member identity authz allowed_routes`
  → exit 0 — `test result: ok. 58 passed; 0 failed; 0 ignored` —
  `lib-tests-final.log` (first run: `lib-tests.log`, same result).
  Both new tests listed `... ok`.
- `cargo clippy -p orgasmic-daemon --all-targets -- -D warnings` → exit 0 —
  `clippy-final.log` (first run `clippy.log`, exit 0).
- `cargo fmt --all --check` → exit 0 — `fmt-final.log` (first check found a
  2-line wrap in new test code; `cargo fmt --all` applied, then check green).

## Unmet Criteria

None. Each acceptance item:
- Member session POST comment/edit/delete over HTTP: proven in
  `task_comments_use_member_session_attribution_and_refresh_activity` (steps 1,
  6, 9). Admin `:ACTOR:` collision refused: same test step 3 (req.actor) and
  `admin_comment_actor_colliding_with_member_name_refused` (req.actor, omitted
  fallback, edit/delete stamp). Refusal variant chosen consistently; no
  override mechanism (brief allows either).
- Real-route attribution test: rewritten as above, no direct handler calls.
- Rename semantics: writer.rs doc comment; all three gates green.

## Residual Risk

- Guard scope is the three task-comment handlers only. The artifact-comment
  route (`POST /artifacts/:id/comments`) shares the attribution pattern and an
  admin `req.actor` naming a member is still stampable there (member-forgeable
  attribution, though artifact comments have no authorship-gated edit/delete).
  Out of scope per the brief; candidate follow-up.
- Admin comment create/edit/delete becomes a 403 in deployments where the
  daemon actor (or `manager_actor`) equals a member name. Intended namespace
  semantics; the error message names the collision and remediation is rename
  or config change. Live ledger members.org was not readable from this
  worktree, so whether the live daemon currently collides is unverified.
- members.org is read synchronously per admin comment mutation (same cost
  class as the `/login` handler's read); tiny file, no measurable latency.
- Mechanical side effects: none beyond the two source files (no lockfile,
  fixture, or generated changes; `cargo fmt` touched only the new test code).
