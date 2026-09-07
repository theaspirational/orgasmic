---
type: Operation
title: Run and utility commands
description: Inspect worker histories, recover runs, manage auth, answer questions,
  materialize explicit native evidence, and mint ids.
aliases:
- orgasmic run
- orgasmic recovery
- orgasmic auth
- orgasmic question
- orgasmic id
- orgasmic run list
- orgasmic run show
- orgasmic run history
- orgasmic run evidence
- orgasmic run evidence materialize
- orgasmic run native-transcript
- orgasmic run recover
- orgasmic recovery status
- orgasmic auth show
- orgasmic question ask
- orgasmic question answer
- orgasmic id mint
- orgasmic run history inspect
- orgasmic run history compact
- orgasmic run history rollback
sources:
- cli-help/run.txt
- cli-help/recovery.txt
- cli-help/auth.txt
- cli-help/question.txt
- cli-help/id.txt
- cli-help/run/list.txt
- cli-help/run/show.txt
- cli-help/run/history.txt
- cli-help/run/evidence.txt
- cli-help/run/evidence/materialize.txt
- crates/orgasmic-daemon/src/native_evidence.rs
- cli-help/run/native-transcript.txt
- cli-help/run/recover.txt
- cli-help/recovery/status.txt
- cli-help/auth/show.txt
- cli-help/question/ask.txt
- cli-help/question/answer.txt
- cli-help/id/mint.txt
- cli-help/run/history/inspect.txt
- cli-help/run/history/compact.txt
- cli-help/run/history/rollback.txt
tags:
- cli-reference
---

# Run and utility commands

## Signature

`orgasmic <command-path> [OPTIONS]`

Canonical commands in this family:

- `orgasmic run`
- `orgasmic recovery`
- `orgasmic auth`
- `orgasmic question`
- `orgasmic id`
- `orgasmic run list`
- `orgasmic run show`
- `orgasmic run history`
- `orgasmic run evidence`
- `orgasmic run evidence materialize`
- `orgasmic run native-transcript`
- `orgasmic run recover`
- `orgasmic recovery status`
- `orgasmic auth show`
- `orgasmic question ask`
- `orgasmic question answer`
- `orgasmic id mint`
- `orgasmic run history inspect`
- `orgasmic run history compact`
- `orgasmic run history rollback`

## Parameters

Read `orgasmic <command-path> --help` immediately before use. Flags are scoped to
the leaf verb; do not infer a flag from a sibling command.

## Returns

Read-only verbs print text or JSON. Mutating verbs report their identifiers or tx
evidence; dispatch verbs additionally identify the dispatch generation.

## Errors

Unknown verbs and invalid lifecycle transitions are refused by name. Treat a timeout
or a reported worker result as evidence to inspect, not as lifecycle closure.

## Example

```bash
orgasmic run history rollback --help
```

## Explicit native evidence

`orgasmic run evidence materialize --run RUN_ID` converts verified Claude native
JSONL into a versioned `derived_native` cache. It returns the run id, source digest,
converter version, cache-hit flag, summary, and file reference; never a full
transcript in the response. Unchanged source reuses the cache; changed source gets
a new digest entry. Missing/ambiguous correlation, malformed or incomplete input,
unsafe cache paths, and corrupt caches are refused.

Limits are 64 MiB source, 2 MiB source line, 16 KiB converted event, 10,000 retained
events, and 8 MiB cache per run. Truncation and omissions are reported. Native files
stay vendor-owned; converted terminal signals never establish run completion,
liveness, task state, or recovery authority.

For analysis across runs, follow the [manual retrospective recipe](/recipes/manual-retrospective.md).
