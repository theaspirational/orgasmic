---
type: Recipe
title: Investigate repeated work with a manual retrospective
description: Select task or run history, prepare an immutable scope, and request an evidence-linked read-only retrospective.
sources:
- cli-help/manager/retro.txt
- cli-help/run/evidence/materialize.txt
- crates/orgasmic-daemon/src/retro.rs
- crates/orgasmic-daemon/src/native_evidence.rs
- provider-host/src/retro.ts
- shipped/prompt-studio/prompt-specs/retro.org
---

# Investigate repeated work with a manual retrospective

## Goal

Explain where selected work spent time or tokens: repeated tool calls and reads,
retries, failures, tool latency, idle/auth/permission waits, context resets, or
circular reasoning. Findings cite evidence; unavailable metrics remain unknown.

## Steps

1. Check installed help for [manager operations](/operations/dispatch.md) and
   [evidence operations](/operations/runs.md). If `manager retro` is absent, the
   installed runtime is older; report the mismatch instead of running a raw provider.
2. Select `--project` and repeated `--run` and/or `--task` values. Task selection
   includes linked implementer, reviewer, and recovery runs. For task order, use
   `--task-sequence TASK-A,TASK-B` alone; it cannot be mixed with run/task selectors.
   Repeat `--question` to focus the investigation.
3. Use `--prepare-only` when the desired outcome is a scope preview without a
   provider turn. It returns scope id, path, digest, and run count. The manifest
   pins ordering, catalog summaries, task/dispatch links, history, source digests,
   evidence gaps, and the compiled Prompt Studio prompt.
4. To perform the requested analysis, use the same selectors with `--model` and
   omit `--prepare-only`. This creates a new scope for that invocation, not a
   continuation of the preview. Claude SDK authentication requires an existing
   `ANTHROPIC_API_KEY` or `CLAUDE_CODE_OAUTH_TOKEN`; ambient credentials/config are
   not loaded. Do not print credentials or put them in a report.
5. Wait for `report_submitted`, then read the returned report. The worker starts
   from catalog summaries, materializes only needed runs, and reads bounded pages.
   Check citations, coverage, omissions, and limitations before using conclusions.

## Complete example

Prepare an ordered scope without starting a provider turn. Substitute the project
and task ids with the selected values:

```bash
orgasmic manager retro --project PROJECT \
  --task-sequence TASK-A,TASK-B \
  --question 'Where did implementer and recovery runs repeat work?' \
  --prepare-only
```

For an authorized analysis, replace `--prepare-only` with `--model MODEL_ID`, using
the chosen Claude model id unchanged. For one run's evidence without an analyst,
use `orgasmic run evidence materialize --run RUN_ID`.

## Pitfalls

The worker has only catalog, materialize, read, and submit tools. Its output is
restricted to derived evidence, SDK scratch files, and the report under
`.orgasmic/tmp/retro/evidence`. It cannot change source tasks, close dispatches,
recover runs, edit code, or alter vendor transcripts. It receives no daemon token.
There is no automatic trigger or schedule, and this command never starts or
restarts the daemon.

Source changes during analysis require a new manual scope. Missing or ambiguous
native correlation is an evidence gap, not permission to guess a transcript.
Provider exit without report submission is failure; the scope remains inspectable.
The retrospective's submit action does not finalize any source worker task.
