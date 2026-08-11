# TASK-K9WWM refresh-coalescing verification

This runbook compares logical refresh requests with physical scans and repeats
the dispatch-close regressions without adding them to the flake registry.

## Root-cause finding

The generic 10-second and run-release 30-second client budgets were left
unchanged. With only refresh-path changes, the worker measured the three
reported reviewer-close cases green in five consecutive parallel repetitions
(the enclosing five-test filter completed in 4.64-7.16 seconds), while the
generic same-project batch measured 16 logical refresh requests to one physical
scan in all five repetitions. Together with the dated pre-fix timeouts that
passed in isolation, this identifies concurrency as the trigger and redundant
project-refresh/Git/index-lock work on the acknowledgement path as the fault;
the budget was not intrinsically too short for one serialized write plus one
coalesced authoritative refresh.

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

The deterministic source-level batch is:

```bash
cargo test -p orgasmic-daemon \
  index::tests::sixteen_same_project_mutations_coalesce_without_concurrent_scans \
  -- --nocapture --test-threads=1
```

It prints the captured `IndexRefreshStatus` and asserts sixteen concurrent
same-project requests complete with at most two physical scans. The companion
coordinator cases are selected with:

```bash
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

Then repeat the deterministic generic mutation batch five times under parallel
libtest scheduling:

```bash
for run in 1 2 3 4 5; do
  env -u ORGASMIC_RUN_ID -u ORGASMIC_HOME \
    cargo test -p orgasmic-daemon \
      index::tests::sixteen_same_project_mutations_coalesce_without_concurrent_scans \
      -- --test-threads=4 \
      > "/tmp/task-k9wwm-repetitions/mutation-batch-${run}.log" 2>&1 || exit $?
done
```

Finally run the normal classified crate gate on a calm host:

```bash
ORGASMIC_ALLOW_MISSING_TOOLS=tmux scripts/run-tests.sh -p orgasmic-cli
```

## Worker evidence

The implementation worker executed the six deterministic coordinator/Git tests,
the committed-503 daemon test, and the CLI reconciliation-message test. It also
ran five repetitions of the five `reviewer_close_*` tests at
`--test-threads=4` (25/25 green, 4.64-7.16 seconds per repetition) and five
repetitions of the sixteen-request mutation batch (5/5 green). Every mutation
batch measured `requests_total=16`, `scans_total=1`, and
`coalesced_total=15`; scan duration was 0-4 ms. The installed-daemon counter
comparison and full classified crate gate remain manager/independent-verifier
checkpoints; this file does not claim they ran.
