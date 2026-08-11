# TASK-K9WWM write-refresh verification

This runbook compares logical refresh requests with physical scans and repeats
the dispatch-close regressions without adding them to the flake registry.

## Root-cause finding

The generic 10-second and run-release 30-second client budgets remain unchanged.
The measured fault was the release/write acknowledgement path, not an
intrinsically short budget: each mutation amplified to approximately three full
refreshes (the awaited refresh, the handler's explicit project refresh, and the
`TxAppended` listener), and every refresh repeated Git work and built the whole
project while holding the index write lock. Suite concurrency supplied the
scheduler pressure that made that redundant work exceed 10 seconds; it was the
trigger, not the source of the work.

The fix removes that approximately 3N amplification, removes Git lookup and
multi-project clone work from mutation acknowledgement, builds one bounded
target projection off the index lock, and publishes through one authoritative
coordinator generation. Coalescing is an additional reduction, not the primary
root-cause claim. Five production HTTP repetitions each landed 16 concurrent
writes exactly once in 132-182 ms with 16 logical refresh requests, one physical
scan, and 15 coalesced requests. An intentionally staggered 30-request stream
measured four scans and 766 ms (`coalesced_total=27`, one stale generation
discarded). Production does not coalesce every arrival shape into one scan; the
bounded 200 ms maximum wait prevents a steady stream from deferring the first
scan indefinitely. The 50 ms trailing window is also the uncontended
acknowledgement floor: an idle target waits for that quiet window before its
first scan so concurrent mutations can join the same authoritative batch.

Five consecutive projections made stale by same-target mutations bound one
captured batch's retry window. At that bound its covered committed callers get
the existing structured committed-but-refresh-failed 503, while newer arrivals
remain queued and the detached coordinator continues to convergence.

## Timeout-classification decision

This task does not add a `daemon request timed out` arm to `run-tests.sh`. The
accepted implementation plan superseded classifier-only mitigation with a fix
to the production acknowledgement path, and the task's declared write scope
does not include the test wrapper. A future timeout therefore remains a real
failure requiring state reconciliation rather than being excused as a flake;
the CLI's timeout text already identifies the self-describing timeout condition
and warns that the server-side write may still have landed. This is an explicit
decision not to derive a second host-attributability word from absolute load.

## Metrics

`GET /api/daemon/status` (also printed by `orgasmic status`) now includes:

```text
.index_refresh.requests_total
.index_refresh.scans_total
.index_refresh.coalesced_total
.index_refresh.discarded_total
.writer.queue_depth
.writer.in_flight_operation
```

On an installed candidate, capture boot-local counters immediately before and
after a mutation batch:

```bash
orgasmic status > /tmp/task-k9wwm-before.json
# Run the supported mutation batch for the project under test.
orgasmic status > /tmp/task-k9wwm-after.json
jq '{requests: .index_refresh.requests_total,
     scans: .index_refresh.scans_total,
     coalesced: .index_refresh.coalesced_total,
     discarded: .index_refresh.discarded_total}' \
  /tmp/task-k9wwm-before.json /tmp/task-k9wwm-after.json
```

Compare deltas from the same `boot_id`; counters reset at boot. A same-project
batch should increase `requests_total` once per logical request but
`scans_total` by no more than two. It must not resemble the former approximately
three full refreshes per mutation. Git metadata probes are intentionally absent
from these counters.

The deterministic production-HTTP batch is:

```bash
cargo test -p orgasmic-daemon \
  sixteen_concurrent_api_writes_finish_within_budget_without_refresh_amplification \
  -- --nocapture --test-threads=1
```

It prints the captured `IndexRefreshStatus` and asserts sixteen concurrent
production API requests complete inside the unchanged 10-second request budget,
land exactly once, and require at most two physical scans. The staggered shape
and companion coordinator cases are selected with:

```bash
cargo test -p orgasmic-daemon \
  staggered_arrivals_cannot_extend_coalescing_past_the_absolute_bound \
  -- --nocapture --test-threads=1
cargo test -p orgasmic-daemon index::tests:: -- --test-threads=1
```

## Required parallel regression repetitions

Run the three originally failing cases together five times with the supported
parallel test setting. Preserve each log and exit code; do not register a red as
a flake.

```bash
mkdir -p /tmp/task-k9wwm-repetitions
for run in 1 2 3 4 5; do
  env -u ORGASMIC_RUN_ID -u ORGASMIC_HOME \
    cargo test -p orgasmic-cli --test dispatch reviewer_close_ \
      -- --test-threads=4 \
      > "/tmp/task-k9wwm-repetitions/reviewer-close-${run}.log" 2>&1 || exit $?
done
```

Confirm every log contains green results for:

- `reviewer_close_non_clean_verdict_flags_stay_in_progress`
- `reviewer_close_refuses_verdict_flag_alongside_property_verdict`
- `reviewer_close_verdict_has_issues_stays_in_progress`

Then repeat the production HTTP mutation batch five times under parallel libtest
scheduling:

```bash
for run in 1 2 3 4 5; do
  env -u ORGASMIC_RUN_ID -u ORGASMIC_HOME \
    cargo test -p orgasmic-daemon \
      sixteen_concurrent_api_writes_finish_within_budget_without_refresh_amplification \
      -- --nocapture --test-threads=4 \
      > "/tmp/task-k9wwm-repetitions/api-concurrent-${run}.log" 2>&1 || exit $?
done
```

Finally run the normal classified crate gate on a calm host:

```bash
ORGASMIC_ALLOW_MISSING_TOOLS=tmux scripts/run-tests.sh -p orgasmic-cli
```

## Worker evidence

The fix-round worker executed the deterministic coordinator, bounded-arrival,
Git publication/probe, home-permit, cached-retry, read-after-write,
committed-release-503, last-good, and writer-accounting regressions. It ran five
repetitions of all five `reviewer_close_*` tests at `--test-threads=4` (25/25
green; the test body completed in 6.18-7.04 seconds) and five repetitions of the
16-write production HTTP case (5/5 green; 132-182 ms acknowledgement, one scan,
15 coalesced requests). `required_test_tooling_is_present` passed. The
full classified `scripts/run-tests.sh -p orgasmic-cli` gate also passed with
zero failures, no tooling waiver, and `host: calm` (`syspolicyd_rate=0.0688`;
load 7.51 before / 15.01 after). The installed-daemon counter comparison remains
a manager/independent-verifier checkpoint; this worker did not install or
restart the live daemon.
