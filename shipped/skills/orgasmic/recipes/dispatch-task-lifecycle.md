---
type: Recipe
title: Dispatch and close a worker task
description: Run the dispatch, wait, inspect, merge or record the report, and close
  the exact generation with evidence.
sources:
- shipped/skills/orgasmic/references/dispatch.md
- shipped/skills/orgasmic/references/agent-selection.md
- cli-help/manager/dispatch.txt
- cli-help/manager/dispatch-wait.txt
- cli-help/manager/dispatch-status.txt
- cli-help/manager/dispatch-close.txt
- cli-help/dispatch/finalize.txt
- shipped/prompt-studio/prompt-specs/base_worker.org
- shipped/prompt-studio/prompt-specs/implementer.org
- shipped/prompt-studio/prompt-specs/reviewer.org
---

# Dispatch and close a worker task

## Goal

Dispatch an implementer or reviewer from committed history, wait for its report, inspect
the evidence, then close the same dispatch generation.

## Steps

1. Read [manager/dispatch operations](/operations/dispatch.md) and [dispatch policy](../references/dispatch.md).
2. Choose an installed mode/harness with `orgasmic manager drivers`; use a different harness family for review unless a reason is recorded.
3. Run `orgasmic manager dispatch ...`; retain its `started_tx`.
4. The worker's terminal action is `orgasmic dispatch finalize --task ... --summary-file ... [--commit]`.
5. Wait with `orgasmic manager dispatch-wait --started-tx ...`, inspect the report and diff, then merge or record report-only evidence.
6. Close by exact generation with `orgasmic manager dispatch-close --started-tx ...`; reviewed closes record `--verdict` and `--reviewed-diff`.

## Complete example

```bash
orgasmic manager dispatch --task TASK-XXXXX --kind implementer \
  --brief /tmp/brief.md --mode <mode> --harness <harness> \
  --model <model> --from <committed-ref>
orgasmic manager dispatch-wait --started-tx tx-...
orgasmic manager dispatch-close --task TASK-XXXXX --started-tx tx-... \
  --status done --worker-commit <sha> --merge-sha <sha>
```

## Pitfalls

Workers see committed refs only. A reported worker is not closed. Bind close to
`started_tx`; task-only closure can select a successor generation. Closing removes the
worktree and deletes a successful branch by default unless explicitly opted out.
