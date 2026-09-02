# Review: TASK-KA934.3.2 — inverse `:ACTOR:` guard + narrowed forward guard

Scope: `git diff 7c85f177^1 7c85f177` (api.rs, member.rs, doctor.rs) and direct neighbours.
Read-only throughout; no edits, no git writes, no probe of the live `:4848` daemon.

## Verdict

**APPROVE WITH FOLLOW-UPS.** Three LOWs, no HIGH or MEDIUM. The central safety claim —
that `comment` is the only journal type whose stored `:ACTOR:` grants a member rights —
is confirmed at the authorization gate, so the narrowing does not reopen the forgery
the KA934.3.1 guard closed. Acceptance criteria are met.

## Findings

### LOW (design/test) — `crates/orgasmic-cli/src/doctor.rs:318` — doctor is silent about offline-knowable collisions
`push_member_actor_collision_findings` returns early on any `DaemonLiveness` other than
`Running`. But `member add` already checks two things that need no daemon: the `$USER`
boot default (`DaemonOptions::default`, `crates/orgasmic-daemon/src/lib.rs:267`) and
`manager.actor` from `home.config()`. Doctor checks neither.

Symptom: operator adds member `alice` on a host where `manager.actor: alice`, daemon is
stopped. `member add` would have refused — but a member added before this round exists
already. Run `orgasmic doctor` with the daemon down and it reports nothing; the collision
only surfaces after a boot, as a 403 on admin comment moderation. Doctor is the tool most
likely to be reached for while the daemon is *not* running.

Fix direction: in the non-`Running` arm, fall back to the same two static candidates
`refuse_daemon_actor_collision` uses (`$USER`, `DaemonConfig::load(home).manager_actor`)
and emit the same warn with a "daemon is down; checked config + `$USER`" qualifier.

### LOW (usability) — `crates/orgasmic-cli/src/member.rs:104` — `$USER` candidate is a guess with no override
The candidate is the *CLI process's* `USER`, not the daemon's. On macOS LaunchAgent
installs these match, so the guard is right in the common case. Where they diverge (daemon
under a service account, `sudo -u`, a container), `member add <my-own-username>` is
hard-refused for a name the running daemon will never stamp — and there is no `--force`.

The message compounds it: the remedy offered is "change the daemon actor (`manager.actor`
in config.yaml)", which is not the lever for the `$USER` branch — `manager.actor` does not
suppress `state.actor`, because `comment_mutation_actor` (`api.rs:2392`) stamps
`state.actor` directly, bypassing the `choose_actor` `manager_actor` preference. So setting
`manager.actor` will not make the refusal go away, and the operator is left with no
documented way forward.

Fix direction: when a live status *is* available and reports an actor, prefer it over the
`$USER` guess rather than checking both; and/or add `--force` (the daemon-side guard is
now narrow enough that the blast radius is admin comment moderation only).

### LOW (perf/usability) — `crates/orgasmic-cli/src/member.rs:111` — `member add` now blocks on HTTP
`live_daemon_status` → `daemon_status` builds a fresh `tokio::runtime::Runtime`
(`doctor.rs:452`) and issues a `GET /daemon/status` with a 10s client timeout
(`doctor.rs:466`). `member add` was a pure filesystem operation on a locked `members.org`;
it is now gated on a network round trip. A daemon that is listening but wedged stalls the
command for 10 seconds with no output. `main` is sync (`main.rs:1453`), so there is no
nested-runtime panic — this is latency only.

Fix direction: a short dedicated timeout (1-2s) for this call site, or accept it and say so.

## Verification Notes

**Confirmed: `comment` is the only journal type where the stored `:ACTOR:` grants rights.**
This is the load-bearing claim of the narrowing.
- `require_comment_body` (`writer.rs:1694`) is the sole authorship gate. It bails on
  `entry.ty != "comment"` (`writer.rs:1707`) *before* the actor comparison at
  `writer.rs:1710`.
- It has exactly two callers: `edit_journal_comment` (`writer.rs:1504`) and
  `tombstone_journal_comment` (`writer.rs:1531`) — both read at source, both gated.
- `grep` for every actor-equality comparison across `crates/orgasmic-daemon/src/`
  returns one authorization site: `writer.rs:1710`. All other `entry.actor` hits are
  projection/display (`index.rs:3718,4306`, `api.rs:4421,5478`, `writer.rs:775,2886`).
- Journal `:TYPE:` is the tx type verbatim (`writer.rs:journal_entry`, `writer.rs:2884`),
  so `worker.comment` renders as `:TYPE: worker.comment` and `require_comment_body`
  refuses it. `worker.comment` is therefore correctly outside the guard.
- UI does no client-side actor gating: no `canEdit`/`isAuthor`/actor-equality in `ui/src`;
  the edit/delete calls (`ui/src/lib/api.ts:185,197`) go straight to the server gate.

**Guard placement.** `journal_actor_guard_applies` (`api.rs:2377`) is used at both choke
points — `prepare_tx_append_request` (`api.rs:3089`) and `prepare_api_tx_as`
(`api.rs:8676`). `comment_mutation_actor` (`api.rs:2392`) is deliberately *not* narrowed:
it guards the admin edit/delete path, which does grant rights. Correct.

**`member add` reads the same config the daemon reads.** `DaemonConfig::load` resolves
`home.config()` (`config.rs:114`); the daemon wires `state.manager_actor = cfg.manager_actor`
at `lib.rs:1126`. Same path, same field. `DaemonOptions::default` actor is
`env::var("USER").unwrap_or("unknown")` (`lib.rs:267`) — the CLI mirrors this string
exactly, including the `"unknown"` fallback.

**Fail-closed on best-effort reads.** `DaemonConfig::load` only errors on a YAML parse
failure (unrecognized keys are collected into `unrecognized_keys`, not an error —
`config.rs:120,161`), so a config typo does not silently drop the `manager.actor` candidate.
On a parse failure or a down daemon the `$USER` candidate still applies. Matches the doc
comment.

**Single choke point for member creation.** `orgasmic_core::add_member` has exactly one
non-test caller: `cmd_add` (`member.rs:79`). `MemberCmd` is `Add`/`Revoke`/`List` — no
rename, no grant-update path that could reintroduce a colliding name. `revoke_member`
removes the heading outright (`members.rs:258`), so `read_members` never returns a revoked
name and doctor cannot false-positive on one.

**`push_daemon_findings` behaviour preserved.** Diffed old vs new: only the signature
changed (`&DaemonLiveness` parameter instead of an internal `daemon_status(home)` call) and
`&status` borrows. All three match arms — `Running` (staleness + ledger sync), `Unavailable`,
`Unauthorized` — carry byte-identical messages. The probe still happens at the same point in
`diagnose`, after `push_tracked_views_findings`, so finding order is unchanged.

**Old daemons.** `DaemonStatus.actor` / `.manager_actor` are `Option<String>` with
`#[serde(default)]`; a pre-field daemon deserializes to `None`, and the collision `find`
compares `*actor == Some(member.name)`, which `None` never matches. Covered by
`doctor_member_actor_no_collision_stays_silent`.

**`/status` exposure is admin-only.** `/daemon/status` is registered inside `protected`
(`api.rs:747`) and is absent from `MEMBER_ALLOWED_ROUTES` (`api.rs:897-922`), so a member
session is rejected by `identity_middleware` before the handler runs. Members cannot read
the actor names. Non-issue.

**Dead assertion.** The old `if journal.exists() { ... }` is replaced by
`assert!(!journal.exists())`, which is strictly stronger — the refusal is proven to have
happened before any write, not merely to have left no forged string behind.

**Deviation is accurate.** `grep` for an actor flag or `ORGASMIC_ACTOR` env override across
`crates/` returns nothing; `serve` has no `--actor`. The message's wording change from the
brief's suggestion is correct, not a shortcut.

**Nothing else moved.** Three files, 349 insertions / 24 deletions; every hunk maps to one
of the round's stated bullets. No scope drift.

## What I did NOT check

- **Did not re-run the gate suites.** The brief marks implementer gates (68 daemon / 40 cli
  / clippy / fmt) and the manager's re-run on merged `7c85f177` as already established.
  All behavioural claims above are from source reading, not from a fresh test run.
- **Did not run a live HTTP probe.** The `:4848` daemon runs an old runtime and the brief
  forbids probing it; `/status`'s new fields are unexercised against a real socket by me.
  They are covered by the existing `get_status` unit tests (`api.rs:22989`, `api.rs:30879`).
- **Did not audit body-injection into journals.** An admin `POST /tx` supplies a raw `BODY`
  that `journal_entry` unescapes to real newlines (`writer.rs:2876`) and
  `journal_entry_block` writes unescaped (`node_kernel.rs:185`). There is a column-0 `* `
  write guard (`crates/orgasmic-daemon/tests/body_write_guard.rs`, and the edit path refuses
  it — `writer.rs:4362`); I did not confirm the *append* path is equally covered. This is
  pre-existing, orthogonal to this diff, and admin-only (no privilege escalation, since an
  admin can already write `members.org`). Flagged here only so it is not mistaken for
  something this round introduced.
- **Artifact comments.** `post_artifact_add_comment` (`api.rs:19491`) lets an admin set
  `body.author` freely on a `ty: "comment"` entry in the artifact journal, unguarded before
  and after this round. No rights follow from it: the comment edit/delete routes resolve a
  *task* journal (`task_comment_journal`), and `post_artifact_comment_resolve` does not gate
  on author. Attribution spoofing by an admin only; not filed.

## Open Questions

1. Should doctor's collision check run offline against `$USER` + config `manager.actor`
   (finding 1)? It is a one-arm change and closes the asymmetry with `member add`.
2. Is a `--force` on `member add` wanted (finding 2)? The daemon-side guard is now narrow,
   so an intentional collision costs only admin comment moderation.

## Fix Directions

All three findings are LOW and independently shippable as a follow-up task. None block
this merge. If one is taken, finding 1 is the highest value: it is the only one with a
user-visible failure (silent doctor on a real, already-present collision) rather than a
latency or ergonomics cost.

**APPROVE WITH FOLLOW-UPS.**
