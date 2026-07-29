# TASK-JQ8AV — a third evidence channel: seeing the provider-bound turn

## Premise re-check

The incident is real and reproduced from the session store, not taken on faith:

- `dispatch-TASK-{FZB6T,RB1ZN,SZJ2B}-implementer-20260729T060*.jsonl` all show the
  same shape: `pane_activity` every ~30 s (2–59 KB per window) from launch, then a
  final smaller event after an anomalous 70–86 s gap, then **zero driver events for
  exactly 600.1 s**, then `run_complete` with
  `stall_timeout_exceeded: no work evidence for 600s; 1 process(es) under pid NNN at
  0.0–0.3% cpu (work threshold 5.0%)`. All three were `claude --model claude-opus-5
  --effort high` dispatches.
- So the panes really were byte-silent for the whole window — the kills were not a
  crediting bug in the clock. One premise sub-claim is corrected by measurement: the
  claude TUI *does* normally repaint continuously (my own session emitted 7–44 KB of
  pane bytes every 30 s through thinks and quiet tool windows alike). The silent
  state is specific to long opus-5 high-effort thinks where no tokens stream down;
  it is real, but not the TUI's universal thinking behavior.

## Measurement table (all measured 2026-07-29, from inside this worker: harness pid
63820, `claude` 2.1.220 as a direct child of rmux-daemon; sampler logs in the
dispatch scratchpad `net.log`, `conn_quiet_t{0,90}.csv`)

| # | Candidate signal | Measurement | Verdict |
|---|---|---|---|
| a1 | Established TCP to provider (existence) | 14–15 ESTABLISHED :443 sockets while a tool runs (no request in flight); statsig socket (35.190.46.17) persisted >25 min idle; api-host pool survived a 120 s no-request window intact | **Rejected.** Telemetry/bridge sockets are long-lived, so "a :443 connection exists" is ≈always-true for any live harness — the network-channel analog of VZMZE's heartbeat trap. |
| a2 | Traffic on provider sockets (rate/delta) | Thinking: ~1.1 KB/s inbound, but with zero-delta gaps up to ~6 s; no-request ambient: ~68–106 B/s **including 5 KB/90 s inbound + 184 KB/90 s outbound on a socket to the same api host** (bridge session streaming — `CLAUDE_CODE_BRIDGE_SESSION_ID` is set on this fleet's dispatches); nettop per-pid counters are non-monotonic (drop when pool sockets close) and macOS-only | **Rejected.** Bridge streaming moves bytes on api-host sockets with no turn in flight; separation is ~11–20×, not the order-of-magnitude-both-sides bar JK66P set for `MIN_WORK_CPU_PERCENT`, and it shrinks further for slow opaque thinks. |
| b | `~/.claude` native session/log mtime | Zero files under `~/.claude` modified across >10 min of active turns. The pane itself names the cause: "⚠ Transcript saving is off — inherited CLAUDE_CODE_CHILD_SESSION marker". No open regular-file fds on the harness. | **Rejected.** Dispatched workers are child sessions; there is no local native-file progress to observe. |
| c1 | Pane byte cadence during thinking | My session: 30 s cadence, never a gap. Killed workers: same cadence, then a dead stop for 600.1 s | **Rejected as a new signal** — it is already the stall clock's input, and it demonstrably goes to zero in the incident state. |
| c2 | Pane **content** (open-turn statusline) | In-turn (my live pane): `● Moonwalking… (20m 4s · ↓ 46.3k tokens)`. In-turn (incident, RB1ZN retry capture): `Quantumizing… (3m41s · ↓13.1k tokens · thinking with high effort)`. At-rest (throwaway claude TUI on a private rmux server): prompt box + status bar only — no line matching glyph-anchored `… (` + elapsed/`↓ … tokens` | **Chosen.** The harness's own TUI writes a turn-open marker into the pane and removes it at rest; the marker persists on screen precisely when repaints stop. `rmux capture-pane -p` reads it in ~ms at deadline. |
| d | Driver-level turn-open (acp) | Not measured | **Rejected for this task.** The incident fleet is rmux (no protocol); acp transports already stream turn events into the stall clock. Noted as future work if an acp incident is ever measured. |

## Chosen design

Third channel inside `ProcessSubtreeCpuProbe` (the JK66P `WorkEvidenceProbe`), consulted
only at the stall deadline, only when subtree CPU is below the work threshold, and only
for `rmux` transports: capture the run's pane and look for the TUI's open-turn
statusline. Marker found → `Working` ("provider-bound turn open", reason names the
statusline and the cpu that was also consulted). Pane readable but no marker →
`Idle`, reason now also says `no open-turn statusline in pane capture`. Pane
unreadable → reason says `pane capture unavailable`; the channel can only rescue,
never save a run it cannot see (Unknown/Idle fail closed, JK66P's rule kept).

A harness frozen mid-turn with the statusline burned on screen is rescued until
`max_run_duration` (14 400 s default) — the brief's bounded "turn open" class.
VZMZE's wedge (alive-idle process, at-rest pane, no marker) still dies on schedule.

(work in progress — implementation, tests, verify artifact, gates to follow)
