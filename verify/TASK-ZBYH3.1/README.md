# verify/TASK-ZBYH3.1 — stdout mirror double-write after a plist rewrite

`injection.patch` / `cmd` / `expect-red` are the replayable artifact:
`orgasmic verify TASK-ZBYH3.1`.

## Claim

Under a launchd-shaped launch (stdout redirected to a file — any file, here a
distinct `daemon.stdout.log`), a line logged once via the **production**
discriminator (`LogMirror::Stdout`) must appear **once** across
`daemon.out.log` and `daemon.stdout.log` combined.

Pre-fix (after TASK-ZBYH3 shipped rulings A+B): B suppressed the mirror only
on inode equality; A moved `StandardOutPath` to a different inode. Combined
count was 2. Total write volume unchanged from the original defect — only the
distribution moved into an unrotated file.

## Injection

`resolve_mirror`'s tty gate is forced open (`true || is_terminal()`), so
`LogMirror::Stdout` survives a non-tty redirect and every line is written to
both files — the defect stated precisely.

## Why not the TASK-ZBYH3 artifact

That round pinned `LogMirror::Writer` → `same_file_as_path`, which is not what
runs in production. A fix that relocated the defect still passed that gate.
This artifact pins `LogMirror::Stdout` and the combined two-file count.
