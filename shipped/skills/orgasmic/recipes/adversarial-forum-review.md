---
type: Recipe
title: Challenge forum answers with reviewers
description: Use forum review to send existing stage-1 reports to a fresh panel, including
  one strong model.
sources:
- shipped/skills/orgasmic/references/forum.md
- cli-help/forum/review.txt
- shipped/prompt-studio/prompt-specs/forum-reviewer.org
- git/forum-merges.txt
---

# Challenge forum answers with reviewers

## Goal

Have a fresh reviewer panel challenge, add to, and explicitly confirm existing
stage-1 reports without producing another answer.

## Steps

1. Read [forum operations](/operations/forum.md).
2. Keep the self-curated forum open.
3. Run `orgasmic forum review --forum TASK-XXXXX --all-rounds` with one strong participant, or repeat `--participant` for a panel.
4. Read the new promoted delta reports and fold them into the later curation.

## Complete example

```bash
orgasmic forum review --forum TASK-XXXXX --all-rounds \
  --participant 'stdio,claude,claude-fable-5,high'
```

## Pitfalls

Use `--round N` instead of `--all-rounds` for one earlier answer round. Reviewers
never see their own stage-1 report, review outputs, or other review rounds.
