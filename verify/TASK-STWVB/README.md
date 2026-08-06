# verify/TASK-STWVB — load-sensitivity requires a measured degraded host

`injection.patch` / `cmd` / `expect-red` are the replayable artifact:
`orgasmic verify TASK-STWVB`.

## Claim

An unregistered failure that is green in isolation is **LOAD-SENSITIVE** only
when this run's measured host state was degraded. On a calm host the same shape
stays **REAL**. That interlock is what keeps the flake registry from becoming a
graveyard of environment excuses: the excuse is bounded by a measurement taken
during the run, not by a name on a list forever.

## Injection

`classify_one`'s unregistered / isolation-green branch drops the
`HOST_DEGRADED` gate, so load-sensitivity applies on a calm host too. The
FIRST failing selftest assertion is
`unregistered failure -> REAL, exit 1, even though it is green in isolation`
(the same interlock the later calm-host case names explicitly).

## Why this pins the production path

The command drives `scripts/run-tests-selftest.sh`, which injects a calm host
sample (`load=0.5,syspolicyd_cpu=1.0`) and an unregistered isolation-green
failure. Under the fixed classifier that is REAL / exit 1; under the injection
it is mis-labeled LOAD-SENSITIVE and the selftest fails on exactly that case.
No cargo, no load, no money — about a second.
