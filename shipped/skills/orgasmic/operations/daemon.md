---
type: Operation
title: Daemon commands
description: Inspect daemon responsiveness, preserve live owners, and restart explicitly.
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
- crates/orgasmic-cli/src/daemon_lifecycle.rs
- crates/orgasmic-cli/src/daemon_service.rs
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

## Responsiveness and service ownership

A present local owner without usable boot progress is reported as **not responding**.
Status/start checks leave it intact and point to logs plus an explicit restart.
A health timeout does not authorize replacing a live process. Service mutations
refuse foreign or unknown Orgasmic-home ownership.

Follow [unresponsive-daemon recovery](/recipes/recover-unresponsive-daemon.md) to
diagnose first and restart only when the operator requests it.
