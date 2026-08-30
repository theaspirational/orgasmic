---
type: Recipe
title: Dispatch a fire-and-forget forum curator
description: Run the single-round forum path with an explicit fresh curator and automatic
  artifact submission.
sources:
- shipped/skills/orgasmic/references/forum.md
- cli-help/forum/ask.txt
- cli-help/forum/critique.txt
- shipped/prompt-studio/prompt-specs/curator.org
---

# Dispatch a fire-and-forget forum curator

## Goal

Run the original non-interactive single-round workflow: participants report, a fresh
curator synthesizes, and the orchestrator submits the artifact.

## Steps

1. Read [forum operations](/operations/forum.md).
2. Run `orgasmic forum ask` or `orgasmic forum critique` with an explicit `--curator`.
3. Use a 1-based participant index or a full `mode,harness,model,effort` curator spec.
4. Keep the printed parent/forum id if any stage fails; inspect completed report tasks.

## Complete example

```bash
orgasmic forum ask --file /tmp/question.txt \
  --participant 'stdio,hermes,openai/gpt-5.6-luna,low' \
  --participant 'stdio,hermes,google/gemini-3.7-flash,low' \
  --curator 'stdio,claude,claude-fable-5,low'
```

## Pitfalls

A dispatched curator cannot join an existing forum, and `--forum` conflicts with
`--curator`. The curator always runs in a fresh dispatch.
