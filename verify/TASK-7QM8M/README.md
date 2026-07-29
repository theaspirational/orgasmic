# verify/TASK-7QM8M — boot auto-reattach read every session file whole

`injection.patch` / `cmd` / `expect-red` are the replayable artifact:
`orgasmic verify TASK-7QM8M`.

## The defect

`collect_boot_reattach_candidates` called `read_session_file`
(`std::fs::read_to_string`, then a parse of every line) on every session JSONL in
every project, at every daemon start. Lifecycle facts — did this run release, is
it still live, what does reattaching it need — live in the first and last few
kilobytes of a session file; everything between is transcript. A single live TUI
run can persist hundreds of megabytes of `text_chunk` driver events, and the boot
pass paid all of it to answer a question none of those bytes speak to.

This is the same unbounded read TASK-KWSTJ removed from `GET /api/runs`, left
behind on the boot path. TASK-KKGKM had already moved this scan after bind and
onto a blocking thread, so it can no longer stop the daemon serving — what
remained is cost, and the latency of the operator's live manager terminal coming
back.

Measured here on a synthetic production-shaped board (194 files, 2.41 GiB, four
huge live TUI transcripts), by the ignored
`measure_boot_reattach_on_a_production_shaped_board`:

| | wall time | bytes read |
|---|---|---|
| before (`read_session_file`) | 14.00 s | 2.41 GiB |
| after (`scan_session_lifecycle`) | 0.145 s | 7.16 MiB |

Same five candidates either way.

## The subtlety the substitution had to handle

`boot_reattach_candidate` calls `latest_run_segment`, which picks the newest
contiguous run segment by `run_id`. A bounded scan reads a prefix window and a
tail window with an **unread gap** between them, and `latest_run_segment` can
only segment what was retained. So the newest RETAINED segment is not provably
the newest segment on disk: the gap can hold this run's `Release` and a later
run's `Acquire`. Pairing the prefix run's metadata with the file's end reattaches
a run the file itself says is over, under a stale identity, while the run that
actually owns the end of the file is not reattached at all — which, from the
operator's side, looks like the run vanished.

`classify_session_files` guards its own version of this with
`scan.final_envelope_retained` before trusting `release_outcome`. Reattach needs
that rule too, and one more, because it does something classify does not: it
pairs an `Acquire`/`RunMeta` from the head of a file with a liveness judgement
about its end.

The extra fact that makes it provable is `SessionLifecycleScan::final_line_run_id`
(added in `orgasmic-core`): the `run_id` of the file's **final line**, read from
that line's bounded envelope header **even when the line was dropped as
transcript** — which is the normal shape for a run that is still writing. One
probe, no parse, no extra bytes. Boot reattach then requires:

1. on a truncated scan, the chosen segment's `run_id` must equal
   `final_line_run_id`. A live run whose acquire is in the prefix and whose
   transcript owns the tail passes this; a stale prefix segment behind a later
   run's file end does not;
2. on a truncated scan, one `run_id` spanning two `runtime_id`s is two runs, not
   one — the residue shape `latest_run_segment` exists for, invisible once the
   gap hides the boundary;
3. `release_outcome` is trusted only when the file's genuine final envelope was
   retained, the same rule `classify_session_dir` applies.

All three are no-ops on a whole-file scan, so untruncated behaviour is
bit-identical to before.

## Why this injection and not a byte counter

The brief offered two natural reds: bytes-inspected exceeding the bound, or the
truncation guard absent. Bytes-inspected is not directly observable from inside
the boot path — the count lives in the scan the injected code no longer performs,
so a test asserting it would go on passing while the boot path read gigabytes,
and asserting on wall time instead would pin a number that changes with the
machine.

So the bound is pinned by consequence rather than by count: the fixture's live
run carries a **torn line deep in its transcript** — a partial append, what a
`kill -9` mid-write leaves behind. Any implementation that reads the file whole
parses that line, fails, and drops the run; an implementation that reads bounded
windows never touches it. The same test also asserts
`bytes_inspected <= 192 KiB` against a >2 MiB file, so the claim is stated as a
number as well — but the assertion that makes it a gate is the candidate.

That torn-middle tolerance is a real consequence of the change, not a
contrivance: before this task, a live run whose transcript had one damaged line
anywhere in it could never be reattached, and nothing said so.

## What the command proves

1. **The bound.** `collect_boot_reattach_candidates` — the function the boot pass
   calls — still rehydrates a live run whose transcript the whole-file read
   cannot parse.
2. **The guard.** A two-segment file, each segment larger than the budget, must
   not yield the first (released) run as a candidate. The test first asserts that
   the whole-file read answers `run-second`, so the fixture is pinned as a real
   hazard rather than an artifact of the fixture.
3. **The guard's other half.** One `run_id`, two `runtime_id`s, an unread gap
   between them: not one segment. Constructed as a `SessionLifecycleScan` rather
   than a file because the shape is legacy residue no current build can write —
   the file-naming fix in `manager_launch_ids` is what stopped writing it.

## Scope note

`scan_session_lifecycle` retains every non-`driver_event` line plus the four
lifecycle driver event types, which covers everything this function consumes:
`Acquire`, `RunMeta`, `StageMeta` (TASK-KPMFK), `Release`, the terminal driver
events, and the external-registration marker. The retained set needed no
extension. Its one documented edge — a lifecycle-bearing `driver_event` line over
64 KiB reads as non-terminal — makes a run stay visible as recoverable rather
than silently dropped, which is the safe direction here too: a non-terminal
verdict sends the run to `Supervisor::reattach`, whose driver `attach()` proves
liveness before anything is rehydrated.
