---
type: Recipe
title: Diagnose an unresponsive daemon and restart explicitly
description: Distinguish a slow boot from an unresponsive local owner, preserve active runs, and use an operator-requested restart.
sources:
- cli-help/daemon/status.txt
- cli-help/daemon/start.txt
- cli-help/daemon/restart.txt
- crates/orgasmic-cli/src/daemon_lifecycle.rs
- crates/orgasmic-cli/src/daemon_service.rs
---

# Diagnose an unresponsive daemon and restart explicitly

## Goal

Restore responsiveness without treating a failed health probe as authorization to
replace a live daemon or disturb its active work.

## Steps

1. Read [daemon operations](/operations/daemon.md). Run `orgasmic daemon status`
   in the intended Orgasmic home. A local PID/lock with no usable boot progress
   is reported as **not responding**, rather than indefinitely **starting**.
2. Inspect that home's logs and reported PID/boot information. A timeout alone
   does not prove a process is dead. Ordinary status/start checks preserve the
   existing owner; repeatedly starting the daemon is not recovery.
3. When the operator has explicitly requested a restart, verify the same home
   and run `orgasmic daemon restart`. Restart is a separate action; diagnosis
   alone does not authorize it. Avoid killing processes or rewriting service
   definitions to bypass ownership refusal.
4. Verify responsive status and the resulting PID/boot identity, then inspect
   the affected run state through [run operations](/operations/runs.md). A new
   daemon boot does not establish that a worker completed or resumed successfully.

## Complete example

Read-only diagnosis:

```bash
orgasmic daemon status
orgasmic daemon restart --help
```

After an explicit operator restart request, run `orgasmic daemon restart` from
the same home and verify `orgasmic daemon status` again.

## Pitfalls

A per-user service may belong to another Orgasmic home. Foreign or unknown service
ownership is refused; retain that refusal instead of editing launchctl/systemd
configuration by hand. Startup failure diagnostics can capture owned process state,
but they do not prove that every intermittent stall has been fixed.
