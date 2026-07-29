# verify/TASK-JQ8AV — the clock that could not see a provider-bound turn

`injection.patch` / `cmd` / `expect-red` are the replayable artifact:
`orgasmic verify TASK-JQ8AV`.

## What the injection reintroduces

One hunk, in `ProcessSubtreeCpuProbe::observe`: the probe answers from the cpu
channel alone again, as it did when JK66P shipped. Everything else the fix
added — `rmux_pane_content`, `pane_open_turn_marker`, the `-S` plumbing —
stays exactly where it is, and becomes unreachable.

That is the defect stated precisely. A claude harness in a multi-minute
server-side think is a network wait: ~0% local cpu, no pane repaints, no
transport events. Under pane bytes and subtree cpu that is byte-identical to
VZMZE's wedge, and on 2026-07-29 the production clock released three healthy
workers (FZB6T, RB1ZN, SZJ2B) with `no work evidence for 600s; 1 process(es)
at 0.0-0.3% cpu` while one of their panes, captured live, read `Quantumizing…
(3m41s · ↓13.1k tokens · thinking with high effort)`. The truth was in the
pane's *content*; the clock only ever read byte *flow*.

## What the red proves

`a_network_waiting_provider_turn_survives_the_stall_window_and_a_wedge_dies`
(supervisor.rs) builds the state itself rather than simulating it: a real rmux
pane (on the test-owned server, TASK-SZJ2B's fixture) whose stub prints the
incident statusline verbatim and then blocks reading an ESTABLISHED connection
to a local listener — network-waiting, not sleeping, at ~0% cpu. The test
accepts the stub's connection before the first sweep, so "it was really
network-waiting" is proven, not assumed. Under the injection the first
compressed window kills it, and the pinned red carries today's tombstone
shape verbatim: `stall_timeout_exceeded: no work evidence for …; 1 process(es)
under pid … at 0.0% cpu (work threshold 5.0%)`.

## The green half, which the red does not cover

The same test's second half is the VZMZE guarantee: the same run, its pane
replaced by one with no statusline over a sleeping process with no connection
anywhere, dies on the next expired budget — and the reason names every channel
consulted (`% cpu (work threshold …)` and `no open-turn statusline in pane
capture`). Under the fix both halves pass; under the injection the run never
reaches the second half. Those assertions pin what the fix must NOT change:
Unknown and Idle still fail closed, and a probe that cannot read a pane can
name that fact but never rescue on it.

## Replay notes

- Requires a usable `rmux`; the test is counted by the binary's
  `required_test_tooling_is_present` sentinel. Without rmux the test skips,
  the red phase then mismatches this pin, and the verify fails loudly instead
  of green-washing.
- The red run takes ~3 minutes wall clock: the panic unwinds through the
  released run's real driver-drain path. The green run takes under a second.
