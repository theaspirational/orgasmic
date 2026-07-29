# TASK-D1Z87 — un-recorded warm-up for the remaining stub-then-probe families

Branch `task-d1z87-impl`, fix commit `0f8b258`.

## Premise corrections (read first)

The brief's premise holds, but the *exposure* is narrower and sharper than the
task body describes, and the difference decided the design.

1. **TASK-GEZHQ-retry already immunised the stateless half of the family.**
   `read_status_output` now makes `STATUS_ATTEMPTS = 2` and sets
   `kill_on_drop(true)` (`crates/orgasmic-drivers/src/preflight.rs:49,90`). A
   first exec that is merely late is therefore killed at `STATUS_TIMEOUT`
   leaving *no trace*, and the second attempt asks a now-warm file. For a stub
   that answers the same way every time — `make_auth_status_stub`, which backs
   the three claude.rs tests listed as "same shape, not yet observed red" — the
   retry fully absorbs it. Those three were listed as equally exposed; they are
   not. Evidence: read from `preflight.rs` (the retry and `kill_on_drop` are
   both in the shipped code), and corroborated by the pre-fix loaded runs below
   — 0/5 red where TASK-GEZHQ saw 2/5 before the retry landed. I did not build a
   separate delay-only injection for those three tests; the claim rests on the
   code path and the null result, not on a fourth artifact.

2. **What stayed exposed is exactly the stubs that *remember*.** A killed
   attempt cannot un-append its ledger line or un-advance its scripted-answer
   index. So when the first child is late enough that the bound expires while it
   is mid-flight — after it reached its own first lines, before it printed an
   answer — the retry reaches a stub whose answers have moved on. That is why
   the member that actually fired in TASK-GEZHQ's gates is the one on
   `make_recording_stub`, and why the daemon test fails on its *count*.
   The recording constraint is not merely why the warm-up could not be
   transplanted; it is also why the defect survived the retry.

3. Consequence for the injection: a deterministic reproduction cannot put the
   delay at the top of the script. It has to sit where the child actually died.
   Documented at `make_recording_stub_that_starts_late`.

## The warm-up design

A third argv, `__orgasmic_warm_up` (`WARM_UP_ARGV`), answered by the stub
*above* the recording line, exec'd once by the stub factory itself,
synchronously and unbounded, before any test body runs.

Why not TASK-GEZHQ's warm-up verbatim: it asks the harness its real question one
extra time. Against `make_recording_stub` / `write_recording_claude_stub` that
appends to the ledger the tests count and advances the scripted-answer index —
so the count assertion would be measuring the test's own warm-up, and the probe
would get answer #2. `--version` is worse: that arm sleeps 60 s on purpose, to
catch a composition that spawns the harness.

Where it *does* fit, it is reused unchanged: `make_auth_status_stub` is
stateless, so `warm_up_auth_status_stub` is TASK-GEZHQ's `warm_up_stub` with the
expected payload parameterised. Two patterns total, not three, and the second
exists only because of the recording constraint.

### Why it cannot mask the one-probe-per-dispatch property

Four independent reasons, three of them asserted in code rather than argued:

1. **The exemption is keyed to an argv production cannot mint.**
   `__orgasmic_warm_up` is not a `claude` subcommand and not a flag the adapter
   or daemon composes; it appears in no non-test string in either crate. Every
   argv production *can* produce falls through to the `printf … >> log` line
   untouched — including a second `auth status`, which is the regression the
   count exists to catch.
2. **Nothing is actually un-recorded.** Warm-ups are appended to their own
   ledger (`warmups.log`). The mechanism is a routing decision, not a
   suppression.
3. **The warm-up asserts its own containment before the test starts**: exactly
   one line in the warm-up ledger, and the counted ledger still byte-empty. A
   warm-up that leaked into the counted ledger fails there, loudly, rather than
   silently paying for one of the invocations a test is about to count.
4. **The scripted-answer counter is untouched by the warm-up arm**, so the probe
   still gets the answer the test scripted for it.

Empirical confirmation, not just argument: with the warm-up removed, the
daemon-side test fails on the *count* assertion itself —
`observed ["auth status", "auth status", …], left: 2, right: 1`. The property
the brief feared a warm-up might hide is the property that catches the warm-up's
absence.

### TASK-GEZHQ's rule kept

Both warm-ups assert the stub's own answer (`WARM_UP_ACK` / the `loggedIn`
payload) and exit status. A stub that cannot exec, cannot run its own script, or
answers wrongly fails as a stub failure at the top of the test, not as a verdict
mystery two hundred lines down.

## Changed

- `crates/orgasmic-drivers/src/adapters/claude.rs`
  - `make_auth_status_stub` now warms itself via `warm_up_auth_status_stub`
    (TASK-GEZHQ's `warm_up_stub`, payload parameterised). Covers all three
    listed tests — `acp_stdio_rejects_a_dispatch_for_a_logged_out_claude`,
    `an_empty_endpoint_still_gets_a_verdict_because_it_still_spawns_claude`,
    `acp_stdio_accepts_a_dispatch_for_a_logged_in_claude`.
  - `make_recording_stub` → thin wrapper over `recording_stub`, which adds the
    `WARM_UP_ARGV` arm, the `warmups.log` ledger, an optional late-first-exec
    marker, and a self-warm through `warm_up_recording_stub`.
  - `make_recording_stub_that_starts_late` — new; used only by
    `the_launch_uses_the_credential_the_preflight_admitted`.
  - `the_launch_uses_the_credential_the_preflight_admitted` now uses the late
    variant and documents the mask it wore.
- `crates/orgasmic-daemon/tests/dispatch_credential_plan.rs`
  - `write_recording_claude_stub` gains the same warm-up arm, its own ledger and
    an always-armed late first exec; warms itself through `warm_up_stub`.
- `verify/TASK-D1Z87/{injection.patch,cmd,expect-red}` — new.

No production code changed. `composition_asks_a_wedged_claude_nothing_and_stays_bounded`
asserts the *whole* invocation log is empty; keeping warm-ups in a separate
ledger is what leaves that assertion intact and unweakened.

## Verification gates

### Verify artifact (self-tested)

`orgasmic verify TASK-D1Z87 --artifact verify/TASK-D1Z87` → **PASS**:

```
  [tree]    clean
  [inject]  injection.patch applied
  [red]     as pinned — exit 101, signature matched
  [revert]  reverted; tree byte-identical
  [green]   passes without the injection — exit 0
verify TASK-D1Z87: PASS — red-then-green replay reproduced
```

The injection removes the three warm-up calls and nothing else — the literal
reverse of the fix. Injected red, verbatim:

```
panicked at crates/orgasmic-drivers/src/adapters/claude.rs:3116:9:
assertion `left == right` failed
  left: "native_login"
 right: "bare_api_key"
test result: FAILED. 0 passed; 1 failed
```

That is TASK-GEZHQ's gate-run-2-of-5 failure, character for character, now
deterministic (5.02 s, the killed first attempt) instead of two runs in five.

Same injection against the daemon test, run separately (not part of the
artifact — one artifact, one command):

```
panicked at crates/orgasmic-daemon/tests/dispatch_credential_plan.rs:342:5:
assertion `left == right` failed: one `auth status` at preflight and nothing
else; observed ["auth status", "auth status", "--safe-mode …"]
  left: 2
 right: 1
```

### Loaded runs (TASK-GEZHQ's harness: 19 sibling test binaries looping at `--test-threads 16`)

Post-fix, **6 consecutive loaded runs** (≥5 required), all green:

| test | runs green |
|---|---|
| `acp_stdio_rejects_a_dispatch_for_a_logged_out_claude` | 6/6 |
| `an_empty_endpoint_still_gets_a_verdict_because_it_still_spawns_claude` | 6/6 |
| `acp_stdio_accepts_a_dispatch_for_a_logged_in_claude` | 6/6 |
| `the_launch_uses_the_credential_the_preflight_admitted` | 6/6 |
| `the_daemon_pins_the_admitted_credential_plan_into_the_launch` | 6/6 |

Pre-fix red counts, same harness, same machine, `f4a9b91` versions of both
files: **0/5 red — not reproducible here.** Recorded as a null result rather
than omitted, because it is the measurement behind premise correction 1: after
TASK-GEZHQ-retry the window needs a heavier machine than this harness produces.
The deterministic injection is the reproduction; the load harness is no longer
one.

### Suites (`scripts/run-tests.sh`, per crate)

- `-p orgasmic-drivers` → **GREEN**, 0 failures (billed test NOT RUN, `--skip`
  applied by the wrapper; `ORGASMIC_ALLOW_BILLED_TESTS` never set).
- `-p orgasmic-daemon` → RED, 4 failures, none reachable from this diff:
  - `supervisor::tests::dead_pid_aborts_joins_hung_producer_then_receiver_releases`
    — green in isolation. TASK-J1XCB's ground (worker live in `supervisor.rs`).
    Reported, not registered.
  - `dispatch_endpoint.rs::dispatch_subprocess_exit_synthesizes_run_complete_from_system_tail`
    — green in isolation. TASK-Z7VQK's family. Reported, not registered.
  - `supervisor::tests::required_test_tooling_is_present` and
    `recovery_fault_restart::required_test_tooling_is_present` —
    environment-blocked, not flaky: both fail in isolation with `required test
    tooling is missing: tmux`. This dispatch runs inside an rmux worker whose
    `tmux` on `PATH` is a symlink to `rmux`
    (`/var/folders/…/rmux-shim-…/tmux -> ~/.local/bin/rmux`), the documented
    worker artifact in `.orgasmic/gotchas.org`. `crates/orgasmic-daemon/tests/dispatch_credential_plan.rs`
    passed in the same suite run.

`orgasmic-cli` / `orgasmic-core` not run: the diff touches neither, and both
were exercised continuously as load-generator binaries.

## Unmet criteria

None. Each acceptance item is answered above:

- ≥5 loaded runs — 6/6 on all five named tests.
- Pre-fix red pinned "where reproducible" — measured, and it is *not*
  reproducible under this harness; the deterministic injection replaces it and
  is stronger (100% vs TASK-GEZHQ's 40%).
- One-auth-status-per-dispatch still proven — unchanged assertions, and now
  actively demonstrated to still bite (it is what catches the missing warm-up on
  the daemon side).
- Injection per TCTTD, self-tested with `orgasmic verify --artifact` before
  reporting.

## Residual risk / new-task candidates

1. **The retry can legitimately produce two `auth status` invocations in
   production.** `STATUS_ATTEMPTS = 2` means a genuinely late harness is asked
   twice, and both invocations are real. The count assertions say "exactly one".
   In the tests the warm-up removes the lateness so the number is honestly 1,
   but the *property as worded* ("one dispatch, one question to the harness") is
   now narrower than production: it holds for re-probing after admission, which
   is what it was written for, and not for retrying before it. Candidate task:
   restate the property as "one probe *decision* per dispatch, before ownership"
   and let the count admit `STATUS_ATTEMPTS`. Not fixed here — it is a
   product-semantics decision, outside this task.
2. `the_launch_uses_the_credential_the_preflight_admitted` and the daemon test
   each cost ~6 s of deliberate sleep. That is the price of making the load
   condition deterministic; it is paid once per test, not per assertion.
3. The three stateless `make_auth_status_stub` tests are warmed but have no
   deterministic proof of their own — after the retry, no injection makes them
   red (see premise correction 1). Their warm-up is insurance against the
   both-attempts-miss case and a loud stub-failure channel, not a defect fix.
4. The warm-up argv exemption rests on `__orgasmic_warm_up` never becoming a
   real `claude` subcommand. If it ever did, the containment assertions in the
   warm-up would not catch it — the ledger split would. Cheap to notice, worth
   naming.
