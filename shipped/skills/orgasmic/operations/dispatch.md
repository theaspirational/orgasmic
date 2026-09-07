---
type: Operation
title: Manager and dispatch commands
description: Select drivers, dispatch workers, wait, close, finalize, record tx entries,
  run manual retrospectives, and run manager stages.
aliases:
- orgasmic manager
- orgasmic dispatch
- orgasmic tx
- orgasmic grill
- orgasmic plan
- orgasmic manager state
- orgasmic manager drivers
- orgasmic manager retro
- orgasmic manager dispatch
- orgasmic manager dispatch-close
- orgasmic manager dispatch-status
- orgasmic manager dispatch-wait
- orgasmic manager worktree-prune
- orgasmic manager lease-release
- orgasmic manager register
- orgasmic manager wake
- orgasmic manager release
- orgasmic manager tier
- orgasmic dispatch finalize
- orgasmic tx record
- orgasmic tx list
sources:
- cli-help/manager.txt
- cli-help/dispatch.txt
- cli-help/tx.txt
- cli-help/grill.txt
- cli-help/plan.txt
- cli-help/manager/state.txt
- cli-help/manager/drivers.txt
- cli-help/manager/retro.txt
- cli-help/manager/dispatch.txt
- cli-help/manager/dispatch-close.txt
- cli-help/manager/dispatch-status.txt
- cli-help/manager/dispatch-wait.txt
- cli-help/manager/worktree-prune.txt
- cli-help/manager/lease-release.txt
- cli-help/manager/register.txt
- cli-help/manager/wake.txt
- cli-help/manager/release.txt
- cli-help/manager/tier.txt
- cli-help/dispatch/finalize.txt
- cli-help/tx/record.txt
- cli-help/tx/list.txt
tags:
- cli-reference
---

# Manager and dispatch commands

## Signature

`orgasmic <command-path> [OPTIONS]`

Canonical commands in this family:

- `orgasmic manager`
- `orgasmic dispatch`
- `orgasmic tx`
- `orgasmic grill`
- `orgasmic plan`
- `orgasmic manager state`
- `orgasmic manager drivers`
- `orgasmic manager retro`
- `orgasmic manager dispatch`
- `orgasmic manager dispatch-close`
- `orgasmic manager dispatch-status`
- `orgasmic manager dispatch-wait`
- `orgasmic manager worktree-prune`
- `orgasmic manager lease-release`
- `orgasmic manager register`
- `orgasmic manager wake`
- `orgasmic manager release`
- `orgasmic manager tier`
- `orgasmic dispatch finalize`
- `orgasmic tx record`
- `orgasmic tx list`

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
orgasmic tx list --help
```

For lifecycle and visibility rules, see [Dispatch mechanics](../references/dispatch.md).

## Manual retrospective

`orgasmic manager retro --project PROJECT (--run RUN_ID | --task TASK_ID | --task-sequence TASK-A,TASK-B) [--question TEXT] [--model MODEL_ID | --prepare-only]`

Repeat `--run`, `--task`, or `--question`; run/task selectors may be combined.
`--task-sequence` preserves task order and is exclusive of those selectors.
`--model` is required unless `--prepare-only` is used. This is a foreground,
read-only diagnostic worker, with its own report submission contract; it does
not use task-dispatch leases or `dispatch finalize`. See the
[manual retrospective recipe](/recipes/manual-retrospective.md) for authentication,
source pinning, write boundaries, and failure handling.
