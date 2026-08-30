---
type: Topic
title: When to ask
description: Confirm only actions that are hard to reverse, external, or spend another
  party's resources.
sources:
- shipped/skills/orgasmic/references/asking.md
---

# Asking — when to confirm with the user

Proceed by default. Asking needs a reason naming which property triggers it.

**Confirm before** actions that are hard to reverse, leave this machine, or
spend someone else's resources: push to a remote, publish a release/artifact,
force-push or rewrite history, delete operator session/run history, touch the
LaunchAgent or a live production daemon, mutate any shared or external
service.

**Proceed without asking** on everything else, explicitly including: local
branch commits, creating/removing worktrees, running gates and tests, reading
anything, `.orgasmic/` writes through the CLI.

Unlisted action → classify by the same three properties (reversibility, reach,
whose resources), not by how large it feels.

- One extra question is allowed: the once-per-session agent-selection ask
  at first dispatch ([`agent-selection.md`](agent-selection.md)).
- An approved plan approves its obvious mechanical steps; re-asking inside it
  is an error. Standing authorization has a scope and an expiry.
- Unsure and in the proceed class → act, then report plainly enough to
  reverse.
