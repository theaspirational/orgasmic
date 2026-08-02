# verify/TASK-4CSMY — the tmux transport had no pane evidence channel at all

`injection.patch` / `cmd` / `expect-red` are the replayable artifact:
`orgasmic verify TASK-4CSMY`.

## What the injection reintroduces

Two hunks, one per blind channel — and the point is that on tmux there were
never any others:

1. **`crates/orgasmic-drivers/src/modes/tmux.rs`** — `pane_activity_watch`
   returns before it opens anything, so the tmux transport publishes no
   continuous pane event of any kind. `pane_activity` was emitted by
   `modes/rmux.rs` and by nothing else; `modes/tmux.rs` sent `Ready`,
   `TransitionState`, `ToolCall` and the fatal paths, so between tool calls the
   stall clock saw nothing from a tmux pane no matter how hard it was
   repainting.
2. **`crates/orgasmic-daemon/src/supervisor.rs`** — the open-turn statusline
   consult and the pane-pid resolution go back to `transport == "rmux"`, where
   TASK-JQ8AV left them. `PaneMux`, `pane_content`, `pane_pid` and
   `pane_open_turn_marker` all stay exactly where the fix put them and keep
   working for rmux; a tmux run reaches none of it.

That is the defect stated precisely. A claude-opus-5 high-effort turn thinking
server-side is a network wait: ~0 % local cpu, no tool calls, and — because the
TUI can go minutes without repainting — no pane bytes either. On rmux, JQ8AV's
consult still sees the open-turn statusline burned on screen. On tmux the
consult never ran, the pane pid never resolved so the cpu channel had nothing
to walk down from, and the probe answered `Unknown`: every channel blind, and a
healthy worker released at 600 s. tmux is the shipped default driver, so that
is the path a first-time user is on.

TASK-RWCRN's history is why this is not merely "JQ8AV's refinement is missing":
rmux only became safe when it grew `pane_activity`. tmux never had it, so it
sat where rmux was *before* that fix.

## What the red proves

Both halves, in one run (`--no-fail-fast`, so a red reports both targets rather
than stopping at the first):

- `tmux_pane_activity_publishes_raw_byte_counts_from_a_real_pane`
  (orgasmic-drivers) drives a REAL tmux pane on the test-owned server
  (`own_tmux_server_for_tests`, TASK-0RCRY's fixture) and asserts the driver
  publishes a coalesced `pane_activity`. It runs the fixture twice, and the
  second is the ship blocker TASK-RWCRN.1 measured: a pane that repaints with
  CR and ANSI and *never emits an LF*. The unit is raw output bytes, so that
  pane is as visible as a chatty one. Under the injection the watcher's channel
  closes with nothing on it.
- `a_network_waiting_tmux_turn_outlives_the_stall_window_and_a_wedge_dies`
  (orgasmic-daemon) is the tmux arm of JQ8AV's acceptance, built rather than
  simulated: a real tmux pane whose stub prints the incident statusline
  verbatim and then blocks reading an ESTABLISHED connection to a local
  listener — network-waiting, not sleeping, at ~0 % cpu. The test accepts the
  stub's connection before the first sweep, so "it was really network-waiting"
  is proven, not assumed. Under the injection the first compressed window kills
  it with `stall_timeout_exceeded`.

## The green half, which the red does not cover

The daemon test's second half is the VZMZE guarantee on tmux: the same run, its
pane replaced by one with no statusline over a sleeping process with no
connection anywhere, dies on the next expired budget — and the reason names
every channel consulted (`% cpu (work threshold …)` and `no open-turn
statusline in pane capture`). Those assertions pin what the fix must NOT
change: `Unknown` and `Idle` still fail closed, the channel may only flip
`Idle -> Working`, and a probe that cannot read a pane can name that fact but
never rescue on it.

`verify/TASK-JQ8AV` remains the rmux arm of the same acceptance and still
replays red-then-green; its `injection.patch` was re-cut against this change
because the fix rewrites the exact hunk it patches, and it reintroduces the
same cpu-only clock it always did.

## Replay notes

- Requires a real `tmux` binary resolved ahead of the rmux PATH shim, and
  `bash` (the network-waiting stub uses `/dev/tcp`). Both tests are counted by
  their binaries' `required_test_tooling_is_present` sentinels. Without tmux
  they skip, the red phase then mismatches this pin, and the verify fails
  loudly instead of green-washing.
- Every tmux call in both tests goes through `tmux_command()`, which carries
  the `-L` the gate pinned. An unpinned call takes its socket from `$TMUX` and
  would land on the server hosting live dispatch panes — see the tmux cluster
  in `.orgasmic/gotchas.org`.
- Both phases are fast: seconds, not minutes. The stall windows are compressed
  by `age_run`, and the pane watcher runs at a compressed cadence. The
  production 30 s cadence is covered by the two `#[ignore]`d live smokes
  (`live_tmux_pane_publishes_*`), which go through `acquire` and are not part
  of this replay.
