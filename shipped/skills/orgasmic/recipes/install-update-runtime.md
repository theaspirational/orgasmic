---
type: Recipe
title: Install or update the runtime
description: Install a prebuilt runtime by default, or update according to install.json
  without touching project state.
sources:
- shipped/skills/orgasmic/references/install.md
- shipped/skills/orgasmic/references/update.md
- cli-help/update.txt
- cli-help/doctor.txt
- cli-help/status.txt
---

# Install or update the runtime

## Goal

Install the supported prebuilt CLI/daemon runtime and later update the same install mode
without changing user overrides or project ledger state.

## Steps

1. Read [core/project operations](/operations/core-project.md), [Install orgasmic](../references/install.md) for the wizard, or [Update orgasmic](../references/update.md) for an existing install.
2. Install from a checkout with `bash scripts/install.sh`, or use the documented curl path.
3. Verify with `orgasmic doctor`, `orgasmic status`, and `orgasmic ui --print-url`.
4. Later run `~/.orgasmic/bin/orgasmic update`; it follows `$ORGASMIC_HOME/install.json`.

## Complete example

```bash
bash scripts/install.sh --channel stable
~/.orgasmic/bin/orgasmic doctor
~/.orgasmic/bin/orgasmic status
~/.orgasmic/bin/orgasmic update
```

## Pitfalls

Do not ask regular users to install developer toolchains. Bundle mode updates shipped
runtime content and preserves `$ORGASMIC_HOME/user`, state, secrets, logs, auth, and
registered project `.orgasmic/` files.
