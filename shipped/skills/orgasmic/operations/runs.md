---
type: Operation
title: Run and utility commands
description: Inspect worker histories, recover runs, manage auth, answer questions,
  and mint ids.
aliases:
- orgasmic run
- orgasmic recovery
- orgasmic auth
- orgasmic question
- orgasmic id
- orgasmic run list
- orgasmic run show
- orgasmic run history
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

## Explicit native evidence and manual retrospectives

`orgasmic run evidence materialize --run <run-id>` converts verified Claude
native JSONL into a bounded, versioned diagnostic cache. It returns a summary
and file reference. Repeating it with unchanged source is a cache hit.

`orgasmic manager retro --project <project> --run <id> --run <id> --model <model-id>`
starts a foreground read-only Claude SDK retrospector. Repeat `--task <id>` to
include task-linked implementer, reviewer, and recovery runs, or use
`--task-sequence TASK-A,TASK-B` to preserve task order. Add `--question <text>`
for specific questions. `--prepare-only` pins the scope without a provider turn.
The SDK requires `ANTHROPIC_API_KEY` or `CLAUDE_CODE_OAUTH_TOKEN`; it isolates its
configuration and loads no ambient user/project tools, hooks, or plugins.

The immutable scope, SDK scratch files, and report stay under
`.orgasmic/tmp/retro/evidence`. The worker receives only catalog, materialize,
bounded read, and submit tools; it receives no daemon bearer token. Submission
is the retrospective's explicit terminal declaration. Provider exit without
submission is failure, with the scope retained for inspection. Changed sources
require a new manual scope. Evidence never authorizes source-run completion,
recovery, task transitions, or cleanup. Neither command restarts the daemon;
retrospectives have no automatic trigger.
