# TASK-4CSMY — tmux liveness evidence

This verifier locks both work-evidence channels required by a long-running tmux
pane:

1. The driver emits coalesced `pane_activity` from raw pane output bytes,
   including newline-free TUI redraws.
2. At the stall deadline, the daemon resolves the pane process tree and checks
   a quiet pane for a provider open-turn status line before declaring it idle.

The injection disables both channels. The red phase must show that a healthy,
network-waiting turn is released while a genuinely wedged pane still dies on
schedule. The green phase proves the waiting turn survives and the wedge does
not.

Run from the repository root:

```sh
orgasmic verify TASK-4CSMY
```

This verifier requires a real tmux binary and uses an isolated test server.
