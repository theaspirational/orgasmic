# TASK-Z7VQK — one pre-warmed shared fake agent for `dispatch_endpoint.rs`

## Why the probe is not the reported symptom

The task was filed against a family of timeouts (`timed out waiting for session
ready`, `timed out waiting for synthetic run_complete`) whose trigger is load —
the 5FEN5 precedent already says such a trigger is not patch-expressible and
asks for pinned A/B counts instead.

Measuring it turned up something better and something worse.

**Better:** the cost this task names is deterministic once you stop measuring it
through a daemon. Measured on this machine, 2026-07-29, while three other
agents' workloads held the load average between 18 and 45:

| first exec of…                              | measured                        |
|---------------------------------------------|---------------------------------|
| a freshly written script                     | 157ms typical, **21,942ms peak** |
| a byte-identical *copy* of a warmed script   | 157ms                           |
| a *hard link* to a warmed script             | 5ms                             |

Spawned the way the daemon spawns a harness (stdin piped, timed to first stdout
line), ten installs each: fresh per-test fixture 153–1666ms, shared pre-warmed
fixture 7–17ms.

The cache key is the inode, not the path, and the evaluation is serialized
system-wide — so `every_fake_harness_is_the_same_executable` (inode identity)
and `the_shared_fake_agent_is_exec_d_before_any_test_waits_on_it` (the warm
actually happened) are the whole fix, stated where they can fail as themselves.
The injection restores the pre-fix world — `copy` instead of `hard_link`,
no warm-up, and a one-time per-inode sleep standing in for the evaluation — and
both go red.

**Worse:** the reported symptom has a second cause, in the product, that no
fixture change can fix. See below. That is why `cmd` does not run the family.

## The second cause (not fixed here — new-task candidate)

`dispatch_subprocess_exit_synthesizes_run_complete_from_system_tail` fails with
`system_chunks=0`: the driver's exit synthesis sees *no harness output at all*
and therefore synthesizes no `run_complete`. It is not a slow start. With the
fixture instrumented to record its own lifetime (breadcrumb probe, 2026-07-29):

```
1785291384.515414000 start 19568 /var/…/bin/cursor-agent
1785291384.524909000 end   19568
subprocess-stream-json exit synthesis decision binary="cursor-agent"
  exit_code=Some(0) distill_is_some=false assistant_len=0 system_chunks=0
```

The harness ran to completion in **9.5ms**, printed its 16 lines, and the driver
recorded zero. The output was produced and lost.

`run_subprocess_stream_json` (`crates/orgasmic-drivers/src/modes/subprocess_stream_json.rs`)
selects over stdout, stderr and a command channel; the command branch `break`s
the loop on release *or* on the channel closing, without draining stdout, and
`tokio::select!` picks randomly among ready branches. A harness that exits fast
enough loses its tail — and the fix here, which makes the fixture start ~20×
faster, makes that race fire *more* often, not less.

This reproduces on the **unmodified** file too (solo, 5 runs each: baseline
1 red / 4 green, fixed 2 red / 3 green — same `system_chunks=0` signature), so
it is pre-existing and is the reason the named family cannot be made green from
this file.

## A/B under the 5FEN5 recipe

Whole-file runs, both arms built from the same tree and run alternately inside
one load window (a looping `orgasmic-daemon --lib --test-threads=64` plus the
machine's ambient load from other agents). Counts are in `../../report.md`.
