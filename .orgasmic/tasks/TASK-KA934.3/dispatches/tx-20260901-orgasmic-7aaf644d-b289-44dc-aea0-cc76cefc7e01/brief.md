# TASK-KA934.3 — open the task-comment routes to members, with the `:ACTOR:` collision guard

Read the task first: `orgasmic task get --project orgasmic TASK-KA934.3`, then `dec_Q78QN`.
Line numbers are approximate; read the current files.

## The gap
`crates/orgasmic-daemon/src/api.rs` `MEMBER_ALLOWED_ROUTES` (~:897) omits the task-comment
routes, so `identity_middleware` (~:933) 403s every `Identity::Member` before the handler,
while the viewer capability list (authz.rs ~:83/~:92 `TasksComment`), `/me`, and the UI
composer all promise members can comment. The member-attribution branch in
`post_task_comment` (~:2333) and the test
`task_comments_use_member_session_attribution_and_refresh_activity` (~:38396) describe a
path that never runs.

## The change
1. Add the three comment routes (create, edit, delete — copy the EXACT templates from the
   router, `.route(...)` around ~:762-800) to `MEMBER_ALLOWED_ROUTES`. Keep the template
   form the existing entries use.
2. `:ACTOR:` becomes a guarded identity namespace: in `post_task_comment`,
   `post_task_comment_edit` (~:2400) and the delete handler, when the identity is an admin
   (bare API key / owner) and the request's `actor` equals a `members.org` member name
   (`orgasmic_core::members`), REFUSE with 403 and a message naming the collision. Members
   keep their own name from the session (authz.rs ~:108 `Identity::Member`). Rationale:
   `require_comment_body` (writer.rs ~:1683) and the KA934.1 authorship checks compare raw
   `:ACTOR:` strings.
3. Rename semantics: state in one doc comment next to the authorship check that authorship
   is the stored `:ACTOR:` string — a renamed member loses edit/delete rights on comments
   made under the old name (accepted; no migration).
4. Tests (daemon lib, through the real router + middleware with a member session, not by
   calling the handler directly): member create/edit/delete succeed and carry the member
   name as `:ACTOR:`; admin request with a colliding `actor` is refused; the existing
   attribution test exercises the real route. The live daemon on :4848 runs an old runtime
   — do NOT probe it; the in-test HTTP round-trip is the probe.

OFF LIMITS (TASK-JWHXH.3 territory, running in parallel): `orgasmic-core/src/views.rs`,
`projects.rs`, daemon `index.rs`, `get_org_file`/`post_org_file`, CLI `doctor.rs`,
`project_migrate.rs`, `shipped/prompt-studio`, `shipped/skills`. `TaskDialog.tsx` needs no
change (the composer is already enabled).

## Gates (each to a log file, never pipe cargo output)
- `cargo test -p orgasmic-daemon --lib -- comment member identity authz allowed_routes`
- `cargo clippy -p orgasmic-daemon --all-targets -- -D warnings`
- `cargo fmt --all --check`

## Rules
- Work only in your worktree; one commit `TASK-KA934.3: fix(api): <one line>`.
- NEVER `cargo test --workspace`; NEVER the whole `orgasmic-cli` crate; NEVER set
  `ORGASMIC_HOME`; NEVER run `daemon start`; never touch the live ledger at
  `~/.orgasmic/ledgers/orgasmic`; never run
  `legacy_drivers_and_explicit_pairs_emit_equivalent_start_events`.
- Fix pre-existing clippy/lint diagnostics in files you touch.
- Report: what changed (`file:line`), each gate with its pass/fail line and log path, unmet
  criteria, residual risk. Finish with `orgasmic dispatch finalize --summary-file <path>`
  (report only, no `--commit`).
