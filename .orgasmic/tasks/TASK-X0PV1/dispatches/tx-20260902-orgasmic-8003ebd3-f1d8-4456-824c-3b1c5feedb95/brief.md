# TASK-X0PV1 — retire or re-evidence the last two flake-registry entries

Read the task body FIRST. It exists to OWN registry entries, not to speculate
about them, and it explains why deleting an entry is worse than keeping it.

## State

Four entries were owned. Today (2026-09-02) two were DELETED on evidence and
one was fixed in-test; that work is recorded in Evidence. TWO remain, both in
`crates/orgasmic-cli/tests/dispatch.rs`:

- `dispatch_close_records_cleanup_failure_and_status_filter_lists_it`
  signature "cleanup failure close should still append tx" (dispatch.rs:3644)
- `dispatch_timeout_requests_daemon_cleanup`
  signature "daemon cleanup should remove branch after CLI timeout"
  (dispatch.rs:4759)

Both were measured red only under FULL-WORKSPACE parallelism and green on an
isolated rerun, on 2026-08-30 at b00b48bd and earlier at 9413059a.

## What to do

MEASURE FIRST, then decide per entry. Do not guess.

1. Reproduce under load: run these two tests as concurrent copies of the same
   `orgasmic-cli` test binary under CPU pressure, enough times to be a real
   sample (the earlier round used 20x each, two concurrent copies). Record the
   exact command, the count, and the load.
2. For each entry, land ONE of:
   - a FIX in the test or product code, and delete the entry; or
   - fresh evidence at today's HEAD, and update the entry's `evidence` field
     with the date, the sample, and what still trips it.
3. If both entries end up deleted, say so — the task can then close. If either
   stays, the task STAYS OPEN and you must say that in your report.

## Hard constraint

The registry guard refuses an entry whose owner is done. Do NOT delete an
entry just to make `run-tests.sh --check` green, and do not touch the `owner`
field. `--check` currently reports "registry: OK, every owner open".

## Guardrails

- Never `cargo test --workspace`.
- Never run the whole `orgasmic-cli` bin crate unfiltered.
- Use a PRIVATE cargo target dir passed as a FLAG (`--target-dir <path>`),
  never as an exported env var — exporting it makes an unrelated test fail.

## Acceptance

- Each remaining entry is either deleted with a landed fix, or carries fresh
  dated evidence from a measurement you actually ran.
- `bash scripts/run-tests.sh --check` still reports the registry OK.
- Your report states plainly whether this task can now close.
