---
type: Recipe
title: Retry an interrupted task write without duplicates
description: Reuse the original request id and payload after an uncertain response, then verify the recorded task state.
sources:
- cli-help/task/create.txt
- cli-help/task/update.txt
- cli-help/task/get.txt
- crates/orgasmic-daemon/src/writer.rs
- crates/orgasmic-daemon/src/api.rs
---

# Retry an interrupted task write without duplicates

## Goal

Recover the result of one intended task mutation after a timeout or daemon restart,
without turning a retry into a second create or a different edit.

## Steps

1. Read [task operations](/operations/task-graph.md). For a retryable mutation,
   assign a stable `--request-id` before the first call and retain its exact payload.
2. If the response is uncertain, inspect the known task id when available. Retry
   with the same request id, project, operation, and payload. A fresh request id is
   a new operation; do not mint one merely because the prior response was lost.
3. Durable replay can recover the original result across daemon restarts. If the
   daemon reports unreadable/conflicting replay evidence or uncertain sync, inspect
   the error and evidence. Do not hand-edit the ledger or assume rollback.
4. Verify the returned id/transaction and `orgasmic task get`. If a stale lifecycle
   state or removed required evidence causes a conflict, reread the task before
   deciding on a new operation. Reusing an id with changed content is a conflict.

## Complete example

Choose `REQUEST_ID` once for this intended create. On an uncertain response, rerun
this exact command with the same chosen values:

```bash
orgasmic task create --project PROJECT --title 'Investigate repeated reads' \
  --request-id REQUEST_ID
```

Read the task id returned by either the first successful response or its replay:
`orgasmic task get TASK_ID --project PROJECT`.

## Pitfalls

Concurrent lifecycle and property changes are serialized against fresh task bytes;
this preserves successful edits rather than overwriting them with a stale copy.
It does not remove lifecycle preconditions, required evidence, or real Git conflicts.
Not every command accepts `--request-id`; check the leaf command's help first.
