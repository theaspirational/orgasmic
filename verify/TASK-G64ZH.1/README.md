# verify/TASK-G64ZH.1 — keep the durable path when the boot open fails

`injection.patch` / `cmd` / `expect-red` are the replayable artifact:
`orgasmic verify TASK-G64ZH.1`.

## Claim

A boot-time durable-open failure under a mirror-suppressed launch
(`LogMirror::None`) must **keep** `durable_path` so:

1. `record_drop()` fires per dropped line (`dropped_log_writes` moves), and
2. `maybe_reopen_durable`'s 1s→60s backoff can retry for the daemon's lifetime.

Pre-fix (`eaa88da`): `init_tracing_to` used `and_then` so a failed open
discarded the path with the handle; `resolve_mirror` handed back the caller's
`LogMirror::None`; zero tracing lines, zero counter movement, reopen unreachable.

## Injection

`resolve_durable_open` collapses with `and_then` again — path discarded on
failed open (adapted to the `Result<File, _>` open return from TASK-G64ZH.1.1;
the pinned assertion message is unchanged). The FIRST failing assertion is
`durable_path.is_some()`.

## Why this pins the production path

The test drives the same `resolve_durable_open` → `BestEffortMakeWriter::new`
construction `init_tracing_to` uses, with `LogMirror::None` as service defs
request via `ORGASMIC_LOG_MIRROR=off`. Removing the keep-the-path fix without
changing the assertion message fails the same way as the pre-fix silence.
