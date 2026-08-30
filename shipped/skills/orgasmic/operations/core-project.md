---
type: Operation
title: Core and project commands
description: Install, enter, diagnose, update, and inspect projects and the UI.
aliases:
- orgasmic init
- orgasmic entry
- orgasmic integrate
- orgasmic doctor
- orgasmic path
- orgasmic update
- orgasmic project
- orgasmic views
- orgasmic board
- orgasmic status
- orgasmic reindex
- orgasmic restart
- orgasmic ui
- orgasmic path ensure
- orgasmic path print
- orgasmic project init
- orgasmic project add
- orgasmic project list
- orgasmic project migrate
- orgasmic views build
sources:
- cli-help/init.txt
- cli-help/entry.txt
- cli-help/integrate.txt
- cli-help/doctor.txt
- cli-help/path.txt
- cli-help/update.txt
- cli-help/project.txt
- cli-help/views.txt
- cli-help/board.txt
- cli-help/status.txt
- cli-help/reindex.txt
- cli-help/restart.txt
- cli-help/ui.txt
- cli-help/path/ensure.txt
- cli-help/path/print.txt
- cli-help/project/init.txt
- cli-help/project/add.txt
- cli-help/project/list.txt
- cli-help/project/migrate.txt
- cli-help/views/build.txt
tags:
- cli-reference
---

# Core and project commands

## Signature

`orgasmic <command-path> [OPTIONS]`

Canonical commands in this family:

- `orgasmic init`
- `orgasmic entry`
- `orgasmic integrate`
- `orgasmic doctor`
- `orgasmic path`
- `orgasmic update`
- `orgasmic project`
- `orgasmic views`
- `orgasmic board`
- `orgasmic status`
- `orgasmic reindex`
- `orgasmic restart`
- `orgasmic ui`
- `orgasmic path ensure`
- `orgasmic path print`
- `orgasmic project init`
- `orgasmic project add`
- `orgasmic project list`
- `orgasmic project migrate`
- `orgasmic views build`

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
orgasmic views build --help
```

For installation and updating depth, see [Install](../references/install.md) and [Update](../references/update.md).
