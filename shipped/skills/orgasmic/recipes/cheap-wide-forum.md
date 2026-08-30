---
type: Recipe
title: Run a cheap wide forum round
description: Use --fast with one or more participants to skip cross-review, including
  a cheap 10-model first pass.
sources:
- shipped/skills/orgasmic/references/forum.md
- cli-help/forum/ask.txt
- cli-help/forum/critique.txt
- git/forum-merges.txt
---

# Run a cheap wide forum round

## Goal

Get independent first-pass answers cheaply. A ten-model round is ten repeated
`--participant` flags plus `--fast`; OKFy is not needed.

## Steps

1. Read [forum operations](/operations/forum.md).
2. Put the question in a UTF-8 file (or use `--question` for ask).
3. Choose one or more `mode,harness,model,effort` participant specs; use ten for a 10-model round.
4. Run `orgasmic forum ask --fast`; it skips blind cross-review.
5. Inspect every promoted stage-1 report and curate in the current chat; finish via [Run a self-curated forum](self-curated-forum.md).

## Complete example

```bash
orgasmic forum ask --file /tmp/question.txt --fast \
  --participant '<mode>,<harness>,<model-1>,<effort>' \
  --participant '<mode>,<harness>,<model-2>,<effort>' \
  --participant '<mode>,<harness>,<model-3>,<effort>' \
  --participant '<mode>,<harness>,<model-4>,<effort>' \
  --participant '<mode>,<harness>,<model-5>,<effort>' \
  --participant '<mode>,<harness>,<model-6>,<effort>' \
  --participant '<mode>,<harness>,<model-7>,<effort>' \
  --participant '<mode>,<harness>,<model-8>,<effort>' \
  --participant '<mode>,<harness>,<model-9>,<effort>' \
  --participant '<mode>,<harness>,<model-10>,<effort>'
```

## Pitfalls

`--fast` is per round, accepts a panel of one, and means no cross-review. It does
not choose cheap models for you; select supported identities deliberately.
