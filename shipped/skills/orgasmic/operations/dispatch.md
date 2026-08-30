---
type: Operation
title: Manager and dispatch commands
description: Select drivers, dispatch workers, wait, close, finalize, record tx entries,
  and run manager stages.
aliases:
- orgasmic manager
- orgasmic dispatch
- orgasmic tx
- orgasmic grill
- orgasmic plan
- orgasmic manager state
- orgasmic manager drivers
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
