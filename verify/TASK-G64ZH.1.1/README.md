# verify/TASK-G64ZH.1.1 — decide the stdout mirror from the live handle

`injection.patch` / `cmd` / `expect-red` are the replayable artifact:
`orgasmic verify TASK-G64ZH.1.1`.

## Claim

A failed boot open with **no** suppression (`LogMirror::Stdout`) and a non-tty
stdout must keep a write-time fallback (`MirrorState::StdoutWhenNoDurable`),
not resolve to permanent silence. Whether there is anything to double-write is
a property of the live durable handle at write time — not of path presence
keyed once at construction.

Pre-fix (TASK-G64ZH.1): `Option<(PathBuf, Option<File>)>` made
`durable_path.is_some()` true on a failed boot open, and `resolve_mirror` still
keyed on the path, so every non-terminal unsuppressed launch went silent.

## Injection

`resolve_mirror`'s non-tty Stdout branch returns `MirrorState::None` again —
the path-keyed gate. The FIRST failing assertion is the R-1
`StdoutWhenNoDurable` keep message.

## Why this pins the production path

The test drives `resolve_durable_open` → `new_with_terminal_gate(..., false)`
with `LogMirror::Stdout` after an EISDIR boot open — the hand-rolled
systemd/Docker/`nohup`/`serve >> capture.log` shape that was never in F-A's
scope and was never silent before round 2.
