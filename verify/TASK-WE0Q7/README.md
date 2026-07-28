# verify/TASK-WE0Q7 — proofs that the soak watches a class, not a code path

`injection.patch` / `cmd` / `expect-red` are the replayable artifact:
`orgasmic verify TASK-WE0Q7`. The injection is TASK-Q07Y5's defect — the
connection-drain budget applied to the serve task's whole life instead of to
the drain — which is what shipped fully green in 2026 and took the operator's
daemon out of service 10s after every boot.

## The second exemplar

`exemplar-timer-fd-leak.patch` is not replayed by `orgasmic verify` (an
artifact directory holds exactly one injection). It exists because one proof
against one historical defect cannot show a gate is more than a re-run of the
bug it was written for. This one is a *different* uptime defect class, caught
by a *different* assertion:

- **Injection** — a short-interval watcher (`spawn_run_timeout_monitor`, 50ms)
  acquires a file handle per tick and never releases it. Artificial; nothing
  like it ever shipped. It is the fd/session-accumulation shape.
- **What catches it** — the fd-growth alarm, not the pid/boot_id identity
  assertion that catches Q07Y5. The daemon stays up, keeps answering, keeps its
  pid and boot_id, and reports zero parse errors the whole time.
- **Pinned red** (2026-07-28, `--duration-seconds 60 --probe-interval-seconds 5`):

  ```
    [t+0004s] probe 1   pid=60155 ... parse_errors=0 rss=29152KB fds=52
    [t+0009s] probe 2   pid=60155 ... parse_errors=0 rss=29200KB fds=156
  SOAK FAIL: open file descriptors grew from 52 to 260 by t+14s (limit
         +128). Something is accumulating handles with uptime.
  ```

To replay by hand:

```sh
git apply verify/TASK-WE0Q7/exemplar-timer-fd-leak.patch
bash scripts/soak.sh --duration-seconds 60 --probe-interval-seconds 5   # expect nonzero
git apply --reverse verify/TASK-WE0Q7/exemplar-timer-fd-leak.patch
```
