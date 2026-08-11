# TASK-K9WWM final hardening evidence

Date: 2026-08-11

## Behavior locked in

- Project and home refresh builders seed `tx` and `parse_errors` empty. The
  project builder retains the board entry, prior project projection, and
  `rebuilt_at`, preserving last-good behavior while avoiding clones that were
  cleared before the scan.
- A failed non-forced Git probe removes only its own dedup key. A later
  registration/watch-path request can retry, while requests concurrent with an
  in-flight probe remain deduplicated.
- Five consecutive stale projections bound one captured mutation batch. The
  covered older waiter receives the normal refresh error (and therefore the
  existing committed 503 mapping), later waiters remain queued, and the
  detached coordinator continues until it publishes the newest projection.
- The trailing 50 ms coalescing window remains the uncontended acknowledgement
  floor.
- A writer-level cached `Transaction` replay is followed by another writer
  command, proving replay does not terminate the serialized writer loop.

## Measurements

The production HTTP concurrency case completed 16 logical requests in 147 ms:

```text
requests_total=16
scans_total=1
coalesced_total=15
discarded_total=0
last_scan_duration_ms=4
```

The deterministic sustained-arrival case completed its bounded sequence with:

```text
older_error="index refresh remained stale for 5 consecutive scans"
requests_total=6
scans_total=6
coalesced_total=5
discarded_total=5
pending_targets=0
```

The first five scans were deliberately made stale by a new committed waiter.
No waiter was acknowledged from those projections. The sixth scan converged
all five newer waiters and exposed the newest on-disk task.

## Gates run

- `cargo test -p orgasmic-daemon --lib index::tests:: -- --test-threads=4` —
  53 passed. Covers last-good/parse-error behavior, concurrent Git dedup, live
  URL publication merge, failure-then-retry, ordinary coalescing, stale
  generation handling, failed-scan survival, and watcher convergence.
- `cargo test -p orgasmic-daemon --lib index::tests::sustained_stale_generations_fail_older_batch_but_newer_waiters_converge -- --exact --nocapture`
  — 1 passed; the bounded metrics above were printed.
- `cargo test -p orgasmic-daemon --lib writer::tests::writer_accepts_a_command_after_cached_transaction_replay -- --exact --nocapture`
  — 1 passed.
- `cargo test -p orgasmic-daemon --lib committed_refresh_failure_returns_structured_503 -- --nocapture`
  — 1 passed.
- `cargo test -p orgasmic-daemon --lib project_task_mutation_is_immediately_visible -- --nocapture`
  — 2 passed (project tx and home tx).
- `cargo test -p orgasmic-daemon --lib cached_task_retry_after_committed_503_repairs_projection -- --nocapture`
  — 1 passed.
- `cargo test -p orgasmic-daemon --lib cached_graph_retry_after_committed_503_repairs_projection -- --nocapture`
  — 1 passed.
- `cargo test -p orgasmic-daemon --lib release_terminal_tx_committed_refresh_failure_is_truthful_and_durable -- --nocapture`
  — 1 passed.
- `cargo test -p orgasmic-daemon --lib sixteen_concurrent_api_writes_finish_within_budget_without_refresh_amplification -- --nocapture --test-threads=1`
  — 1 passed; the 16-to-1 measurement above was printed.
- `cargo fmt --all --check` — passed.
- `cargo clippy --workspace --all-targets -- -D warnings` — passed with zero
  warnings. The first detached wrapper was interrupted before producing output
  or status; the single allowed foreground retry passed.
- `cargo build` — passed.
- Three repetitions of
  `env -u ORGASMIC_RUN_ID -u ORGASMIC_HOME cargo test -p orgasmic-cli --test dispatch reviewer_close_ -- --test-threads=4`
  — 5 passed in every repetition (15/15 total, 0 failed).

The full `scripts/run-tests.sh` gate was deliberately not run; the dispatch
brief reserves it for the manager after independent review. No live daemon was
installed, replaced, restarted, or contacted.
