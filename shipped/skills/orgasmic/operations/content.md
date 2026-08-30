---
type: Operation
title: Prompt and shipped-content commands
description: Inspect prompts and skills and manage optional or hub content.
aliases:
- orgasmic prompt
- orgasmic skills
- orgasmic optional
- orgasmic hub
- orgasmic prompt list
- orgasmic prompt show
- orgasmic prompt compile
- orgasmic prompt lint
- orgasmic prompt fork
- orgasmic skills list
- orgasmic skills show
- orgasmic optional list
- orgasmic optional enable
- orgasmic optional disable
- orgasmic hub install
- orgasmic hub list
- orgasmic hub remove
sources:
- cli-help/prompt.txt
- cli-help/skills.txt
- cli-help/optional.txt
- cli-help/hub.txt
- cli-help/prompt/list.txt
- cli-help/prompt/show.txt
- cli-help/prompt/compile.txt
- cli-help/prompt/lint.txt
- cli-help/prompt/fork.txt
- cli-help/skills/list.txt
- cli-help/skills/show.txt
- cli-help/optional/list.txt
- cli-help/optional/enable.txt
- cli-help/optional/disable.txt
- cli-help/hub/install.txt
- cli-help/hub/list.txt
- cli-help/hub/remove.txt
tags:
- cli-reference
---

# Prompt and shipped-content commands

## Signature

`orgasmic <command-path> [OPTIONS]`

Canonical commands in this family:

- `orgasmic prompt`
- `orgasmic skills`
- `orgasmic optional`
- `orgasmic hub`
- `orgasmic prompt list`
- `orgasmic prompt show`
- `orgasmic prompt compile`
- `orgasmic prompt lint`
- `orgasmic prompt fork`
- `orgasmic skills list`
- `orgasmic skills show`
- `orgasmic optional list`
- `orgasmic optional enable`
- `orgasmic optional disable`
- `orgasmic hub install`
- `orgasmic hub list`
- `orgasmic hub remove`

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
orgasmic hub remove --help
```
