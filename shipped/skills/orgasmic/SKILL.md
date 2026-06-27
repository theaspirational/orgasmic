---
name: orgasmic
description: 'Manage an orgasmic project. Use for /orgasmic, /orgasmic install, /orgasmic update, /orgasmic init, /orgasmic recall, /orgasmic resume, /recall, /resume, installing or updating orgasmic, scaffolding .orgasmic, or resuming project state.'
triggers: ["/orgasmic", "/orgasmic install", "/orgasmic update", "/orgasmic init", "/orgasmic recall", "/orgasmic resume", "/recall", "/resume"]
---

# orgasmic

Namespace skill for orgasmic — the only install surface; everything is a
subcommand.

Each subcommand below names a reference file bundled with this skill. **Read
that file in full before acting** — it carries the guard rails and exact steps;
do not run a subcommand from this summary alone.

## Subcommands

The first argument selects the subcommand. If empty, default to `recall`.

| arg | what it does | read first |
|-----|--------------|------------|
| `install` | interactive post-skill installer wizard for prebuilt CLI/runtime bundles, supported host apps, remote daemon, Android/mobile-browser access, and explicit contributor source setup — you are the installer | [`references/install.md`](references/install.md) |
| `update` | update the installed runtime bundle, or pull/rebuild the checkout only in contributor source mode | [`references/update.md`](references/update.md) |
| `init [name]` | scaffold via `orgasmic project init`, then ask whether to bootstrap now or defer to `/orgasmic resume` (runtime required) | [`references/init.md`](references/init.md) |
| `recall` (default) | restore manager state from `.orgasmic/`, print a briefing, then **stop** and wait | [`references/recall-resume.md`](references/recall-resume.md) |
| `resume` | same briefing, then **immediately** execute the next planned action | [`references/recall-resume.md`](references/recall-resume.md) |

`install` and `update` bootstrap from nothing — no runtime yet. `init` requires
the runtime (`orgasmic status` must succeed). `recall` and `resume` run inside
an existing orgasmic project (a `.orgasmic/` directory at the repo root) and
work from on-disk state alone in a fresh-context session.

## Roadmap (not yet implemented)

- `status` — terse one-paragraph checkin (goal + handoff + sprint + in-flight), no full briefing.
- `handoff` — refresh `tasks/handoff.org`; optionally export an `archive/<date>_manager-handoff/handoff.md` for human readers.
- `dispatch` — shorthand for the worktree + brief + codex + pre-flight-tx sequence.
- `audit` — conformance pass comparing `architecture.org` claims against shipped code.

When a not-yet-implemented subcommand is invoked, say it is on the roadmap and
offer the closest implemented alternative.

## Layout

```
orgasmic/
  SKILL.md
  references/
    install.md                  /orgasmic install
    update.md                   /orgasmic update
    init.md                     /orgasmic init
    recall-resume.md            /orgasmic recall and resume
  scaffold/                     project templates bundled for `init`
```
