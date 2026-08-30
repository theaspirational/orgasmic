---
type: Recipe
title: Run a self-curated forum
description: Run one or more ask/critique/review rounds, curate in the current chat,
  then submit once.
sources:
- shipped/skills/orgasmic/references/forum.md
- cli-help/forum/ask.txt
- cli-help/forum/critique.txt
- cli-help/forum/review.txt
- cli-help/forum/curate.txt
- git/forum-merges.txt
---

# Run a self-curated forum

## Goal

Collect independent model reports, keep curation in the current chat, optionally add
rounds or reviewers, and submit one final artifact.

## Steps

1. Read [forum operations](/operations/forum.md) and the deeper [forum policy](../references/forum.md).
2. Start `orgasmic forum ask` or `orgasmic forum critique` with repeated `--participant`; omit `--curator`.
3. Read the returned manifest, compiled contract, and every promoted report.
4. Add answer rounds with `--forum TASK-XXXXX`, or a challenge round with `orgasmic forum review`.
5. When the operator is done, write the contract-shaped draft and diagram and run `orgasmic forum curate` with this session's real identity.

## Complete example

```bash
orgasmic forum ask --file /tmp/question.txt \
  --participant 'stdio,hermes,openai/gpt-5.6-luna,low' \
  --participant 'stdio,hermes,google/gemini-3.7-flash,low'
# inspect returned files; optionally add rounds/review; then:
orgasmic forum curate --forum TASK-XXXXX \
  --draft /tmp/TASK-XXXXX-curation.mdx \
  --diagram /tmp/TASK-XXXXX-diagram.json \
  --identity '<mode>,<harness>,<actual-model>,<effort>'
```

## Pitfalls

Do not pass `--curator` on a self-curated forum. Use `--from` and `--artifact-id`
only on the first round. Never guess the invoking session's identity.
