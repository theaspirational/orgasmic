# TASK-KA934.3.2 — inverse `:ACTOR:` guard + narrow the forward guard

Commit: `50fe2f8c TASK-KA934.3.2: fix(api,cli): narrow :ACTOR: guard to comment writes; member add refuses the daemon actor; doctor warns on collisions` (worktree `task-ka934.3.2-impl`, on top of merged 2be9f0a0).

## Changed

1. **Forward guard narrowed to comment writes** — `crates/orgasmic-daemon/src/api.rs`
   - `journal_actor_guard_applies` (`api.rs:2376`): new predicate, `ty == "comment"` — the only journal type whose rows grant the named member rights (`require_comment_body`, `writer.rs:1707`, refuses anything that is not an editable comment; `worker.comment` and the other 15 journal producers grant nothing).
   - `prepare_tx_append_request` (`api.rs:3090`) and `prepare_api_tx_as` (`api.rs:8676`) now gate on the narrow predicate instead of `event_routes_to_journal`; comments updated.
   - The admin comment-mutation guard (`comment_mutation_actor`, `api.rs:2395`) is unchanged.
2. **`/status` exposes the daemon identity** — `StatusResponse.actor` / `.manager_actor` (`api.rs:9060-9064`, populated `api.rs:9138-9139`); small and additive; CLI `orgasmic status` parses the payload as raw JSON (`main.rs:2539`), so no consumer breaks.
3. **Inverse guard in `member add`** — `crates/orgasmic-cli/src/member.rs`
   - `refuse_daemon_actor_collision` (`member.rs:104`), called from `cmd_add` (`member.rs:79`) before minting. Refuses when the name equals: the daemon boot default (`$USER`, else `"unknown"` — mirrors `DaemonOptions::default`, `daemon/lib.rs:268`), `manager.actor` from the daemon config the CLI already loads (`orgasmic_daemon::config::DaemonConfig::load`), and — when a daemon is reachable — the `actor`/`manager_actor` its live status reports (`crate::doctor::live_daemon_status`). Message names the collision and offers both remedies. Config/status reads are best-effort (unreadable config or down daemon → the `$USER` default guard still applies).
4. **Doctor collision warning** — `crates/orgasmic-cli/src/doctor.rs`
   - `push_member_actor_collision_findings` (`doctor.rs:317`), new fn placed next to (not touching) `push_tracked_views_findings`; warns when any `members.org` name equals the live daemon `actor` or `manager_actor`; skipped for down/unauthorized/old daemons (no live actor to compare). `diagnose` now performs ONE status probe shared by `push_daemon_findings` (signature now takes `&DaemonLiveness`) and the new fn.
   - `live_daemon_status` helper (`doctor.rs:452`); CLI `DaemonStatus` gains `#[serde(default)] actor/manager_actor` (`doctor.rs:159-164`) so old-daemon payloads still parse.
5. **Dead assertion fixed** — `admin_post_tx_journal_actor_colliding_with_member_name_refused`: the `if journal.exists() { … }` block is now `assert!(!journal.exists(), …)` (`api.rs:39112-39116`); the refusal is the first attempted write, so the journal cannot exist. The same test additionally covers the narrowing: a `task.property_updated` tx with `actor: "alice"` now returns OK (`api.rs:39123-39139`).
6. **Tests**: daemon — narrowed-scope assertions in `admin_post_tx_journal_actor_colliding_with_member_name_refused`; CLI — `member_add_refuses_name_matching_configured_manager_actor`, `member_add_refuses_daemon_default_actor_name`, `member_add_allows_name_disjoint_from_daemon_actors` (`member.rs` tests), `doctor_warns_when_member_name_equals_live_daemon_actor`, `doctor_warns_when_member_name_equals_manager_actor`, `doctor_member_actor_no_collision_stays_silent` (`doctor.rs` tests).

Mechanical side effects: none (no lockfiles, fixtures, or generated files).

## Verification Gates

Logs in `/var/folders/9p/823z6j817xj9ts2xpvnx1q_40000gn/T/opencode/ka934-3-2/`:

| Gate | Result | Log |
|---|---|---|
| `cargo test -p orgasmic-daemon --lib -- comment member identity authz post_tx status` | PASS — `68 passed; 0 failed … finished in 26.04s`, exit=0 (includes `admin_post_tx_journal_actor_colliding_with_member_name_refused … ok`) | `daemon-tests.log` |
| `cargo test -p orgasmic-cli --bin orgasmic -- member doctor` | PASS — `40 passed; 0 failed … finished in 1.70s`, exit=0 | `cli-tests.log` |
| `cargo clippy -p orgasmic-daemon -p orgasmic-cli -p orgasmic-core --all-targets -- -D warnings` | PASS — exit=0, `Finished … in 2.86s` | `clippy.log` |
| `cargo fmt --all --check` | PASS — exit=0 (one `cargo fmt --all` applied to new test code first) | `fmt.log` |

One intermediate failure: my first doctor test asserted the message contained backticked `` `actor` `` while backticks wrap the actor value; fixed the assertion (not production code) and re-ran the gate green.

Production-path note: a binary-level `orgasmic member add` probe would require setting `ORGASMIC_HOME`, which the dispatch forbids; the CLI gate drives `cmd_add` — the exact function the verb dispatches to — including a real `DaemonConfig::load` parse, and the daemon gate drives real HTTP against a real daemon boot.

## Unmet Criteria

None of the stated acceptance criteria. One deliberate wording deviation, with evidence: the brief's suggested remedy text "start the daemon with `--actor`" references a flag that does not exist on `serve` (the only `--actor` in the CLI is `tx record --actor`, `main.rs:904`; `Cmd::Serve` has only `bind/port/no_log_mirror`, `main.rs:208-223`). The refusal and doctor messages instead name the real mechanism: pick another member name, or change `manager.actor` in config.yaml and restart. Acceptance only requires "a message naming the collision" — satisfied.

## Residual Risk

- The `member add` guard cannot see an actor a future daemon boot will pick beyond `$USER` + config `manager.actor`; an operator who changes the daemon's environment (`$USER`) between add-time and boot can still create a collision. The doctor warning covers exactly this gap at diagnosis time.
- Members named `unknown` are refused because that is the daemon's fallback actor when `$USER` is unset; intended, but worth knowing.
- `prepare_api_tx_as` narrowing is covered indirectly (the comment handlers and `POST /tx` test exercise the same `ensure_actor_namespace_free` chain); no dedicated second HTTP test boots a separate daemon for that path alone.
