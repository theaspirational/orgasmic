---
type: Operation
title: Forum commands
description: Ask, critique, review, and curate multi-model forums.
aliases:
- orgasmic forum
- orgasmic forum ask
- orgasmic forum critique
- orgasmic forum review
- orgasmic forum curate
sources:
- cli-help/forum.txt
- cli-help/forum/ask.txt
- cli-help/forum/critique.txt
- cli-help/forum/review.txt
- cli-help/forum/curate.txt
tags:
- cli-reference
---

# Forum commands

## Signature

`orgasmic <command-path> [OPTIONS]`

Canonical commands in this family:

- `orgasmic forum`
- `orgasmic forum ask`
- `orgasmic forum critique`
- `orgasmic forum review`
- `orgasmic forum curate`

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
orgasmic forum curate --help
```

For the interactive policy, see [Multi-model forum](../references/forum.md).
