---
type: Operation
title: Task and graph commands
description: List and mutate tasks, goals, glossary, decisions, edges, and node bodies/properties.
aliases:
- orgasmic tasks
- orgasmic task
- orgasmic goal
- orgasmic glossary
- orgasmic decision
- orgasmic graph
- orgasmic node
- orgasmic tasks list
- orgasmic tasks count
- orgasmic task create
- orgasmic task get
- orgasmic task update
- orgasmic task comment
- orgasmic goal set
- orgasmic goal clear
- orgasmic goal supersede
- orgasmic glossary list
- orgasmic glossary get
- orgasmic glossary create
- orgasmic glossary schema
- orgasmic decision list
- orgasmic decision get
- orgasmic decision create
- orgasmic decision schema
- orgasmic graph edges
- orgasmic node body
- orgasmic node prop
- orgasmic node submit
- orgasmic node regenerate
- orgasmic node delete
- orgasmic task comment edit
- orgasmic task comment delete
- orgasmic node body set
- orgasmic node body append
- orgasmic node body unset
- orgasmic node prop set
- orgasmic node prop unset
sources:
- cli-help/tasks.txt
- cli-help/task.txt
- cli-help/goal.txt
- cli-help/glossary.txt
- cli-help/decision.txt
- cli-help/graph.txt
- cli-help/node.txt
- cli-help/tasks/list.txt
- cli-help/tasks/count.txt
- cli-help/task/create.txt
- cli-help/task/get.txt
- cli-help/task/update.txt
- cli-help/task/comment.txt
- cli-help/goal/set.txt
- cli-help/goal/clear.txt
- cli-help/goal/supersede.txt
- cli-help/glossary/list.txt
- cli-help/glossary/get.txt
- cli-help/glossary/create.txt
- cli-help/glossary/schema.txt
- cli-help/decision/list.txt
- cli-help/decision/get.txt
- cli-help/decision/create.txt
- cli-help/decision/schema.txt
- cli-help/graph/edges.txt
- cli-help/node/body.txt
- cli-help/node/prop.txt
- cli-help/node/submit.txt
- cli-help/node/regenerate.txt
- cli-help/node/delete.txt
- cli-help/task/comment/edit.txt
- cli-help/task/comment/delete.txt
- cli-help/node/body/set.txt
- cli-help/node/body/append.txt
- cli-help/node/body/unset.txt
- cli-help/node/prop/set.txt
- cli-help/node/prop/unset.txt
tags:
- cli-reference
---

# Task and graph commands

## Signature

`orgasmic <command-path> [OPTIONS]`

Canonical commands in this family:

- `orgasmic tasks`
- `orgasmic task`
- `orgasmic goal`
- `orgasmic glossary`
- `orgasmic decision`
- `orgasmic graph`
- `orgasmic node`
- `orgasmic tasks list`
- `orgasmic tasks count`
- `orgasmic task create`
- `orgasmic task get`
- `orgasmic task update`
- `orgasmic task comment`
- `orgasmic goal set`
- `orgasmic goal clear`
- `orgasmic goal supersede`
- `orgasmic glossary list`
- `orgasmic glossary get`
- `orgasmic glossary create`
- `orgasmic glossary schema`
- `orgasmic decision list`
- `orgasmic decision get`
- `orgasmic decision create`
- `orgasmic decision schema`
- `orgasmic graph edges`
- `orgasmic node body`
- `orgasmic node prop`
- `orgasmic node submit`
- `orgasmic node regenerate`
- `orgasmic node delete`
- `orgasmic task comment edit`
- `orgasmic task comment delete`
- `orgasmic node body set`
- `orgasmic node body append`
- `orgasmic node body unset`
- `orgasmic node prop set`
- `orgasmic node prop unset`

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
orgasmic node prop unset --help
```

For storage and write authority, see [Ledger map](../references/ledger.md).
