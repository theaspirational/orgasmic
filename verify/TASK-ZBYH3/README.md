# verify/TASK-ZBYH3 — durable log double-write under a same-inode mirror

`injection.patch` / `cmd` / `expect-red` are the replayable artifact:
`orgasmic verify TASK-ZBYH3`.

## Claim

When the tracing mirror resolves to the same device+inode as
`$ORGASMIC_HOME/logs/daemon.out.log` (launchd `StandardOutPath` pointing at
that file), a line logged once must appear **once** in the durable file.

Pre-fix (operator daemon, 2026-08-06): fd 1 and the durable sink shared inode
439742072; last 400 lines → 201 unique; ~half of a 13 MB log was exact
duplication.

## Injection

`resolve_mirror` is forced never to suppress (`if false && …`), so a
`LogMirror::Writer` opened on the durable path writes every line twice — the
defect stated precisely.

## Related fix (not pinned here)

Size-triggered rotation of the durable sink, and pointing generated LaunchAgent
`StandardOutPath` at `daemon.stdout.log`, are covered by unit/plist tests in
tree. This artifact pins the cleanly measurable 2× write.
