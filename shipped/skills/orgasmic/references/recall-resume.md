# /orgasmic recall & resume — manager session bootstrap

Both run inside an existing orgasmic project (a `.orgasmic/` directory at the
repo root) from on-disk state alone. They share one bootstrap and differ only
after the briefing: `recall` stops and waits; `resume` executes the next
planned action.

Speed is the contract: the core bootstrap is three file reads and three
commands, then the briefing. `tasks/handoff.org` is the digest of everything
else — trust it. Read any other file only when a trigger below names it.

## Core bootstrap (always, in order)

1. `.orgasmic/tasks/goal.org` — the `* GOAL` heading with `:STATUS: active`:
   statement plus optional reached-when criteria. goal.org is thin (operator
   intent only); continuity lives in handoff.org.
2. `.orgasmic/tasks/handoff.org` — the current `* HANDOFF` heading:
   `:GOAL_ID:` (must match the active goal), `:LIVENESS:` SHA, `** Done so
   far`, `** Next likely actions` (first entry is `resume`'s default action),
   `** In flight`, `** Divergence / Risks`.
3. `.orgasmic/project.org` — skim `:ID:` and operating constraints once; note
   `config.org` holds verification commands for later.
4. Commands:
   - `git log --oneline <LIVENESS>..HEAD` and `git status --short` — drift
     check. After a clean session HEAD is at most one commit past
     `:LIVENESS:`.
   - `git worktree list` — extra worktrees imply in-flight implementers.
   - `orgasmic manager dispatch-status` — open and orphan dispatches (skip if
     the CLI is absent).
5. Produce the briefing.

The bootstrap is read-only and writes nothing. No tx-log reading, no
decisions.org or architecture.org reread, no full task-file scan — those are
trigger-gated below.

## Triggers for deeper reads

Open these only when the condition actually holds:

- **Liveness drift** (HEAD more than one commit past `:LIVENESS:` on
  scope-relevant files): read `git log <LIVENESS>..HEAD` and
  `git diff <LIVENESS> HEAD`. Either a prior manager died mid-sequence, or the
  operator (or a second manager) committed to main while you were live — read
  `conventions/concurrent-writer.org` and reconcile in place (fold into Done so
  far, bump `:LIVENESS:`, never replay an operator commit) before proposing
  actions.
- **Open or orphan dispatch** (from `dispatch-status` or handoff
  `** In flight`): inspect `.orgasmic/tmp/dispatch/<task-stem>/` (brief, last
  message, stdout log) and `git -C <worktree> status`; decide
  resume-integration vs abort.
- **No CLI but handoff names in-flight work**: scan the most recent
  `.orgasmic/tx/*.org` for `manager.dispatch_started` entries without a
  matching close (`implementer.done` / `reviewer.done` /
  `manager.dispatch_aborted`).
- **Next action targets a specific task**: read that task's heading in
  the correct state file (`tasks/backlog.org`, `tasks/in_progress.org`, …) by
  searching its ID. Never read every state file wholesale.
- **Next action dispatches a worker**: read the `manager-dispatch` convention
  and `.orgasmic/gotchas.org` first.
- **Next action edits source in-session**: read
  `conventions/manager-implementer.org` and `.orgasmic/gotchas.org` first.
- **goal.org missing or no `:STATUS: active` heading**: reconstruct intent
  from the most recent `manager.set_goal` tx, then recent commits; if still
  unclear, ask. Under `resume` this always forces a wait.

Manager operating rules live in `shipped/prompt-studio/prompt-specs/manager.org`
and its conventions (`manager-dispatch`, `manager-handoff`,
`manager-contribution`, `tx-scannable-bodies`). They load by situation as
above — do not pre-read them during bootstrap. If repo docs contradict them,
the prompt spec and conventions win.

## Briefing

One short paragraph, no file dumps:

> *Goal* <one sentence, with goal `:ID:`>. *Done so far* <latest landed item +
> SHA>. *In flight* <workers/worktrees/orphans, or "nothing">. *Next planned
> action* <first handoff entry, or the in-flight task that pre-empts it>.
> *Divergence* <unrecorded commits, orphan dispatches — or "none">.

## recall — brief, then wait (default)

`recall` is what bare `/orgasmic` runs: the user wants to see where things
stand, not to set the manager moving. Print the briefing, ask "Resume from
here, or change direction?", and stop. No dispatch, no edits, no proactive
next-step inference.

## resume — brief, then go

Print the briefing, then immediately execute the next planned action — no
"should I proceed?" handshake; invoking `resume` was the yes. Follow the
manager spec and the `manager-dispatch` convention (in practice the action is
usually a worker dispatch with pre-flight tx, or closing an in-flight task).

`resume` stops and waits only when:

- bootstrap surfaced unreconciled liveness drift;
- an orphan dispatch's worktree/process state is ambiguous;
- goal.org is missing or has no active heading;
- the next action is itself a user decision ("set a new goal", "pick a
  direction") or needs authorization (destructive op, force-push);
- no next action exists (handoff empty and nothing in flight).

If none apply, go. One line after the briefing — "Proceeding: <action>." — is
the right amount of preamble.

## When the bootstrap writes (rare; both subcommands)

Requires `orgasmic status` to succeed; if not, route to `/orgasmic install` or
`orgasmic daemon start` — do not hand-edit.

1. **User sets a new goal**: `orgasmic goal set` (supersedes prior goal, updates
   handoff `:GOAL_ID:` and next actions); `orgasmic tx record` with
   `manager.set_goal`.
2. **Liveness drift reconciliation**: under `recall`, surface it and wait. Under
   `resume`, reconcile in place — update the handoff sections from the diff and
   bump `:LIVENESS:` to HEAD with `orgasmic node body set|append --section`
   and `orgasmic node prop set`, then proceed. Never change goal.org unless the
   goal itself changed.
