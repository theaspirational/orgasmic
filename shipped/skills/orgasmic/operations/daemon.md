---
type: Operation
title: Daemon commands
description: Run and manage the local daemon lifecycle.
aliases:
- orgasmic serve
- orgasmic daemon
- orgasmic daemon status
- orgasmic daemon start
- orgasmic daemon stop
- orgasmic daemon restart
sources:
- cli-help/serve.txt
- cli-help/daemon.txt
- cli-help/daemon/status.txt
- cli-help/daemon/start.txt
- cli-help/daemon/stop.txt
- cli-help/daemon/restart.txt
tags:
- cli-reference
---

# Daemon commands

## Signature

`orgasmic <command-path> [OPTIONS]`

Canonical commands in this family:

- `orgasmic serve`
- `orgasmic daemon`
- `orgasmic daemon status`
- `orgasmic daemon start`
- `orgasmic daemon stop`
- `orgasmic daemon restart`

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
orgasmic daemon restart --help
```
