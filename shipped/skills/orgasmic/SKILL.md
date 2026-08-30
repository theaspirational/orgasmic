---
name: orgasmic
type: Topic
title: Orgasmic skill door
description: 'Orgasmic project management: forum ask/critique/review, self- or dispatched curation, task dispatch lifecycle, and runtime install/update.'
triggers:
- /orgasmic
- /orgasmic install
- /orgasmic update
- /orgasmic init
- /orgasmic recall
- /orgasmic resume
- /orgasmic forum
- /recall
- /resume
sources:
- shipped/skills/orgasmic/SKILL.md
---

# orgasmic

Orgasmic coordinates local project state, worker dispatch, and multi-model forums.

Start at the [bundle index][bundle-index]; choose the recipe matching the intent, and
follow its links to operation references and deeper policy. Raw Markdown traversal is
the complete fallback; OKFy is not required. Links starting with `/` (such as
`/operations/forum.md`) resolve from this bundle root — the directory holding this
SKILL.md — not the filesystem root.

[bundle-index]: index.md

When `okfy` is installed, search this directory with:

```bash
okfy query <directory-containing-this-SKILL.md> "<intent>"
```

For interactive forum behavior, start with
[Run a self-curated forum](recipes/self-curated-forum.md). All orgasmic state writes
go through the `orgasmic` CLI, never hand edits.
