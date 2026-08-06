# verify/TASK-STWVB.1 — alone-red keeps exit 1 on a degraded host

`injection.patch` / `cmd` / `expect-red` are the replayable artifact:
`orgasmic verify TASK-STWVB.1`.

## Claim

A failure that fails in isolation is a code fact. It must return exit 1 with
its "this red means something" verdict regardless of host state. The host stamp
is reported *alongside* the verdict, never instead of it. Putting
`INCONCLUSIVE` ahead of `REAL` in the verdict ladder made the gate return no
verdict for the suite it was built for — including for alone-red failures.

## Injection

The verdict ladder restores the `HOST_DEGRADED` branch ahead of the
`REAL_COUNT` branch. Under that ordering, a degraded-host alone-red failure
returns exit 4. The FIRST failing selftest assertion is
`degraded host + fails alone too -> REAL exit 1, host stamp alongside`.

## Why this pins the production path

The command drives `scripts/run-tests-selftest.sh` with a stamped degraded host
and an alone-red failure. Under the fixed ladder that is REAL / exit 1; under
the injection it is INCONCLUSIVE / exit 4 and the selftest fails on exactly
that case. No cargo, no money — about a second.
