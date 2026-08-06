# verify/TASK-G64ZH — bound the durable reopen retry

`injection.patch` / `cmd` / `expect-red` are the replayable artifact:
`orgasmic verify TASK-G64ZH`.

## Claim

A persistent durable-open failure costs a **bounded** number of reopen
attempts (and stderr notes) per unit time — not one per tracing line. With
rotation armed and the handle missing, `maybe_rotate` must not re-enter; the
reopen path owns the retry behind a 1s → 60s exponential backoff.

Pre-fix (TASK-ZBYH3.1 F3 retry): every line paid `try_open_durable` plus a
`maybe_rotate` re-entry that parked `bytes_written` at `max_bytes + 1`, so
`dropped_log_writes` inflated ~4x and `daemon.err.log` grew unbounded.

## Injection

`maybe_reopen_durable`'s backoff gate is forced open (`false && Instant::now()
< next_reopen_attempt`), so every line retries open — the unbounded per-line
cost stated precisely. The FIRST failing assertion is
`open_attempts == 1` (got 40).

## Why this pins the production path

The test drives the same missing-handle + rotation-armed state the heading
reproduced from project stderr, not a test-only discriminator. Removing the
bound without changing the assertion message fails the same way as the
pre-fix stuck state.
