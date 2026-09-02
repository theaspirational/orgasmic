# Review: TASK-KA934.3.2 — inverse `:ACTOR:` guard + narrowed forward guard (narrow)

Implementer: opencode / zai-coding-plan/glm-5.3-flash (variant max), one commit `50fe2f8c`,
merged to main as `7c85f177`. Answers the MEDIUM + 1 LOW of the KA934.3.1 review
(tx-160c6cc2). Read `orgasmic task get --project orgasmic TASK-KA934.3.2` and `dec_Q78QN`.

    git diff 7c85f177^1 7c85f177     # api.rs (+~80), member.rs (+100), doctor.rs (+192)

Keep this review to the diff and its direct neighbours.

## What this round claims
- Forward guard narrowed: `journal_actor_guard_applies` = `ty == "comment"`, used at both
  choke points (`prepare_tx_append_request`, `prepare_api_tx_as`); `comment_mutation_actor`
  unchanged.
- `/status` now exposes `actor` and `manager_actor` (additive).
- `member add` (`member.rs` `refuse_daemon_actor_collision`): refuses a name equal to `$USER`
  (else `"unknown"`, mirroring the daemon default), `manager.actor` from the daemon config the
  CLI already loads, and the live daemon's status actor/manager_actor when reachable.
- Doctor: `push_member_actor_collision_findings` next to (not touching) the views fn; one
  shared status probe (`live_daemon_status`); `DaemonStatus` gains `#[serde(default)]` fields.
- Dead assertion fixed; the same test now also proves a non-comment journal tx with a member
  name is accepted (the narrowing).
- Deviation: the brief suggested "start the daemon with --actor" in the message; no such flag
  exists on `serve`, so the message says pick another name or change `manager.actor` in
  config.yaml and restart.

## Attack these specifically
- **Is `comment` really the only journal type where `:ACTOR:` grants rights?** Check
  `require_comment_body` (writer.rs) and anything else that keys authorization off a stored
  `:ACTOR:` (comment edit/delete, resolve, tombstones, artifact comments). If any other type
  grants rights, the narrowing re-opens the forgery.
- **`member add` guard correctness.** Does `$USER` here equal what the daemon actually uses
  at boot (`DaemonOptions::default`, lib.rs ~:268)? Is `manager.actor` read from the SAME
  config path the daemon reads? Best-effort reads: on config parse error or a down daemon,
  does the `$USER` default guard still apply (fail-closed on the one thing it can know)?
- **Doctor probe sharing.** `diagnose` now does one status probe shared by two findings
  fns — did `push_daemon_findings` keep its exact prior behaviour (same messages on down /
  unauthorized / stale daemons)? Old daemons without the new status fields must not produce
  a false "collision" or a parse error.
- **`/status` exposure.** Is exposing `actor`/`manager_actor` on `/status` a member-readable
  route? If members can read it, is that acceptable (it's a username)? Size it.
- **Nothing else moved.** Three files; every hunk should be one of the bullets.

Classify precisely; if only LOWs remain, say so and APPROVE (with follow-ups if any).

Already established — do not re-spend: implementer gates (68 daemon, 40 cli, clippy, fmt);
manager re-ran on merged main `7c85f177` (task Evidence).

## Rules
- READ-ONLY. No edits, no git writes, no mutating `orgasmic` verbs, nothing against the live
  ledger at `~/.orgasmic/ledgers/orgasmic` or the live `~/.orgasmic/user/auth/members.org`
  beyond reads. The live daemon on :4848 runs an OLD runtime — do not probe it.
- File each finding as it appears:
  `orgasmic tx record --project orgasmic --type reviewer.finding --task TASK-KA934.3.2
  --reason "HIGH|MEDIUM|LOW <file:line> — <one sentence>"` (single line).
- Targeted tests only; NEVER the whole `orgasmic-cli` suite unfiltered; never the workspace;
  never `ORGASMIC_HOME`; never `daemon start`; do not read `verify/*/injection.patch`; never run
  `legacy_drivers_and_explicit_pairs_emit_equivalent_start_events`.
- Say what you did not check. Finish with `orgasmic dispatch finalize --summary-file <path>`
  (report only) and end with the explicit verdict sentence:
  APPROVE / APPROVE WITH FOLLOW-UPS / REJECT.
