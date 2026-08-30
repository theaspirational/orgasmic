---
type: Recipe
title: Judge a document with a forum
description: Critique a UTF-8 document with an optional focus and either self or dispatched
  curation.
sources:
- shipped/skills/orgasmic/references/forum.md
- cli-help/forum/critique.txt
- shipped/prompt-studio/prompt-specs/critique-curator.org
---

# Judge a document with a forum

## Goal

Collect independent critiques of one supplied document and synthesize a prioritized
verdict without rewriting the target.

## Steps

1. Read [forum operations](/operations/forum.md).
2. Run `orgasmic forum critique --file ...` with at least two participants, or one with `--fast`.
3. Add `--focus` for a one-line steer.
4. Omit `--curator` to curate in chat, or pass it for the dispatched path.

## Complete example

```bash
orgasmic forum critique --file /tmp/design.md --focus 'security posture' \
  --participant 'stdio,hermes,openai/gpt-5.6-luna,low' \
  --participant 'stdio,hermes,google/gemini-3.7-flash,low'
```

## Pitfalls

The file is required, non-empty UTF-8, and at most 64 KiB. Use `forum review`, not
critique, when the object is the forum's existing reports rather than a document.
