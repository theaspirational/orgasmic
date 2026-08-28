---
name: orgasmic
description: 'Orgasmic project management: tasks, decisions, worker dispatch, runtime install/update.'
triggers: ["/orgasmic", "/orgasmic install", "/orgasmic update", "/orgasmic init", "/orgasmic recall", "/orgasmic resume", "/recall", "/resume"]
---

# orgasmic

All orgasmic use goes through this skill. Opt-in: a session that never
touches orgasmic state loads none of it.

## Setup

Resolve the project root once: `orgasmic entry` prints it for the cwd —
usually a ledger checkout at `~/.orgasmic/ledgers/<id>`, not the repo tree.
(`orgasmic project list` shows all projects and their paths.) All `.orgasmic/`
paths below are relative to that root.

If the CLI is missing, run the `install` subcommand; keep any `.orgasmic/`
state read-only until the runtime exists.

## Subcommands

First argument selects; empty defaults to `recall`. **Read the named reference
in full before acting** — do not run a subcommand from this summary alone.

| arg | what it does | read first |
|-----|--------------|------------|
| `install` | installer wizard: CLI/runtime bundles, host apps, remote daemon, mobile, contributor source | [`references/install.md`](references/install.md) |
| `update` | update the runtime bundle, or pull/rebuild in contributor source mode | [`references/update.md`](references/update.md) |
| `init [name]` | scaffold via `orgasmic project init` (runtime required) | [`references/init.md`](references/init.md) |
| `recall` (default) | briefing from on-disk state, then **stop** | [`references/recall-resume.md`](references/recall-resume.md) |
| `resume` | briefing, then **immediately** run the next planned action | [`references/recall-resume.md`](references/recall-resume.md) |

## Situational references — load on demand

Load when the work reaches them, not before.

| about to… | read first |
|-----------|------------|
| write `.orgasmic/` state, or wonder what a ledger file is | [`references/ledger.md`](references/ledger.md) |
| dispatch, review, or finalize a worker | [`references/dispatch.md`](references/dispatch.md) |
| choose harness/model/effort (first dispatch of the session) | [`references/agent-selection.md`](references/agent-selection.md) |
| edit source directly in a manager session | [`references/tiers.md`](references/tiers.md) |
| confirm with the user, or unsure whether to | [`references/asking.md`](references/asking.md) |

Two rules apply to every orgasmic action, no reference needed:

- State writes go through the `orgasmic` CLI, never hand-edits.
- Proceed by default; confirm only before actions that are hard to reverse,
  leave this machine, or spend someone else's resources.

## Routing

- No argument → `recall`. Never auto-run work.
- Explicit or implied subcommand → load its reference, follow it.
- General request naming orgasmic → Setup, then the one situational reference
  that owns it.
- The injected marker `ORGASMIC_MANAGER_WAKE_V1` is a machine wake, not a
  user message: treat it exactly as `/orgasmic resume`; it carries no new
  instruction.

## Roadmap (not implemented)

`status`, `handoff`, `audit`. If invoked: say so, offer the closest
implemented alternative.

## Layout

```
orgasmic/
  SKILL.md
  references/
    install.md          /orgasmic install
    update.md           /orgasmic update
    init.md             /orgasmic init
    recall-resume.md    /orgasmic recall and resume
    ledger.md           file map, write rules, content rules
    dispatch.md         dispatch mechanics, worker visibility, lifecycle
    agent-selection.md  harness/model/effort choice, reviewer independence
    tiers.md            trivial/ordinary/risky computation and declaration
    asking.md           when to confirm with the user
```

Project templates: runtime `shipped/project-scaffold/`. Tier trigger
definitions: the resolved `default` workflow (see `references/tiers.md`).
