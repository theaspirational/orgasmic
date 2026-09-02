# TASK-KA934.3.2 — inverse `:ACTOR:` guard + narrow the forward guard (narrow fix round)

Read `orgasmic task get --project orgasmic TASK-KA934.3.2` and `dec_Q78QN`. Line numbers are
approximate; read the current files.

## The problem
After TASK-KA934.3.1 the `:ACTOR:` guard (`ensure_actor_namespace_free`, api.rs ~:2340) fires
in `prepare_tx_append_request` (~:3077) and `prepare_api_tx_as` (~:8662) for EVERY
`event_routes_to_journal` type. All 16 admin producers pass `actor: None`, so `choose_actor`
falls to `manager_actor` → `state.actor` (default `$USER`, daemon lib.rs ~:268). One
`orgasmic member add <that name>` therefore 403s every task create / transition / property
update. `member add` (`crates/orgasmic-cli/src/member.rs` ~:78 → `orgasmic_core::add_member`,
`members.rs` ~:165/~:186) writes `$ORGASMIC_HOME/user/auth/members.org` directly and never
talks to the daemon.

## Three moves
1. **Narrow the forward guard** to journal writes where `:ACTOR:` grants rights: fire only
   when the type is `comment` (keep the `event_routes_to_journal` gate AND add
   `ty == "comment"`, or whatever single predicate the code already has for
   "editable comment" — see `require_comment_body`, writer.rs ~:1691). Wider scope bought
   nothing (every producer passes `actor: None`).
2. **Inverse guard in `member add`**: refuse a name equal to the daemon actor. The CLI
   cannot ask a running daemon at add-time, so: refuse `name == $USER` (the daemon default)
   and `name == manager_actor` when readable from the daemon config the CLI already loads
   (look at how the CLI resolves config; do not add a new config file). If a daemon IS
   reachable (the CLI has a status client — see doctor's `check_daemon_for_status`), also
   compare against its reported actor when the status payload exposes it; if it does not,
   expose `actor` and `manager_actor` on `/status` (small, additive). Message names the
   collision and says to pick another member name or start the daemon with `--actor`.
3. **Doctor**: warn when any `members.org` name equals the live daemon actor or
   manager_actor (reuse the status client + `read_members`). Shape: like
   `push_tracked_views_findings` (doctor.rs ~:253).
4. **Dead assertion**: in `admin_post_tx_journal_actor_colliding_with_member_name_refused`
   (api.rs ~:39085) replace the `if journal.exists() { … }` block with
   `assert!(!journal.exists())`.

OFF LIMITS (TASK-JWHXH.3.2 runs in parallel): `crates/orgasmic-cli/src/project_migrate.rs`,
and `doctor.rs` `push_tracked_views_findings` (add a NEW findings fn next to it, do not edit
that one).

## Gates (each to a log file, never pipe cargo output)
- `cargo test -p orgasmic-daemon --lib -- comment member identity authz post_tx status`
- `cargo test -p orgasmic-cli --bin orgasmic -- member doctor` (targeted; NEVER unfiltered)
- `cargo clippy -p orgasmic-daemon -p orgasmic-cli -p orgasmic-core --all-targets -- -D warnings`
- `cargo fmt --all --check`

## Rules
- Work only in your worktree; one commit `TASK-KA934.3.2: fix(api,cli): <one line>`.
- NEVER `cargo test --workspace`; NEVER the whole `orgasmic-cli` crate; NEVER set
  `ORGASMIC_HOME`; NEVER run `daemon start`; never touch the live ledger at
  `~/.orgasmic/ledgers/orgasmic` or the live `~/.orgasmic/user/auth/members.org`; never run
  `legacy_drivers_and_explicit_pairs_emit_equivalent_start_events`.
- Report: what changed (`file:line`), each gate with its pass/fail line and log path, unmet
  criteria, residual risk. Finish with `orgasmic dispatch finalize --summary-file <path>`
  (report only, no `--commit`).
