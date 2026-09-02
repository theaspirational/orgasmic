# TASK-KA934.3.1 — one `:ACTOR:` guard for every journal write (narrow fix round)

Read `orgasmic task get --project orgasmic TASK-KA934.3.1` and `dec_Q78QN`. Line numbers are
approximate; read the current `crates/orgasmic-daemon/src/api.rs`.

## The move
`ensure_actor_namespace_free` (api.rs ~:2340) is called from `post_task_comment`,
`post_task_comment_edit`, `post_task_comment_delete` only. `POST /tx` (`post_tx` ~:2991 →
`prepare_tx_append_request` ~:3023 → `choose_actor` ~:8960) writes the same journal `:ACTOR:`
unguarded when `event_routes_to_journal(&type)` is true. Call the guard ONCE in
`prepare_tx_append_request` on the effective actor (the same `choose_actor` chain), only when
the identity is not a member session and the event routes to a journal; then DELETE the three
handler-level calls (the create handler's effective-actor computation can go with it if it
only existed for the guard — keep behaviour identical otherwise). If the comment handlers do
not pass through `prepare_tx_append_request`, put the guard in the smallest function all
four producers share; say which in the report.

## LOWs (same round)
- `read_members(...).unwrap_or(false)` fails open silently: `tracing::warn!` the error, keep
  failing open (members.org is admin-owned and a parse error already breaks login).
- `writer.rs` ~:1683 doc on `require_comment_body`: add one sentence — a member re-added or
  renamed INTO a retired member's name inherits edit/delete on that member's old comments
  (raw `:ACTOR:` equality; accepted).

## Tests
- Admin `POST /tx` with `type=comment`, a task, and `actor == <member name>` → 403 naming the
  collision (drive the real router with the admin credential as the existing tests do).
- Existing member-session test and `admin_comment_actor_colliding_with_member_name_refused`
  stay green with the handler copies deleted.

OFF LIMITS (TASK-JWHXH.3.1 runs in parallel): `post_org_file` / `reject_ledger_rewrite`
(~:14700), `crates/orgasmic-cli/**`, `shipped/**`. Do not touch artifact comment routes
except through the shared guard.

## Gates (each to a log file, never pipe cargo output)
- `cargo test -p orgasmic-daemon --lib -- comment member identity authz tx_append post_tx`
- `cargo clippy -p orgasmic-daemon --all-targets -- -D warnings`
- `cargo fmt --all --check`

## Rules
- Work only in your worktree; one commit `TASK-KA934.3.1: fix(api): <one line>`.
- NEVER `cargo test --workspace`; NEVER the whole `orgasmic-cli` crate; NEVER set
  `ORGASMIC_HOME`; NEVER run `daemon start`; never touch the live ledger at
  `~/.orgasmic/ledgers/orgasmic`; never run
  `legacy_drivers_and_explicit_pairs_emit_equivalent_start_events`.
- Report: what changed (`file:line`), what you deleted, each gate with its pass/fail line
  and log path, unmet criteria, residual risk. Finish with
  `orgasmic dispatch finalize --summary-file <path>` (report only, no `--commit`).
