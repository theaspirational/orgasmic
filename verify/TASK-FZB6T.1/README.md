# verify/TASK-FZB6T.1 — a stale identity must never be mistaken for a live one

Replay: `orgasmic verify TASK-FZB6T --artifact verify/TASK-FZB6T.1`.
Sibling artifacts: `verify/TASK-FZB6T` (the catalog's cost claim),
`verify/TASK-FZB6T-redraw` (the storage lock), `verify/TASK-FZB6T-corruption`
(a derived index never outranks what it derives from).

## The defect this prevents

Two findings from the TASK-FZB6T review, and they are the same mistake made in
two places: **a name is treated as an identity**.

1. A run records the worktree it was dispatched into. When that worktree is
   pruned the run is tombstoned — permanently, because recovery under that
   identity is over. The first cut lifted the tombstone as soon as *something*
   existed at the recorded path again. Dispatch worktree paths are reused
   constantly; the next unrelated checkout at that path made a dead run an
   attach candidate again, and the daemon would offer to recover a run into a
   working tree that has nothing to do with it.

2. A snapshot entry was admitted into the catalog if its `session_path` merely
   started with the project root. That admits `<root>/.orgasmic/project.org`, a
   path that escapes through `..` and comes back, a nested file that is not a
   session, a symlink, a record for a file that no longer exists, and a record
   whose file has since been replaced. None of those are ever evicted either —
   eviction is scoped to direct children of the sessions directory — so a
   semantically corrupt entry survived every refresh and kept answering
   inventory queries with a run the session files never described.

Both let an old, dead, or forged record present itself as current state. That is
the shape of the bug worth a proof: nothing crashes, nothing is corrupt on disk,
and the daemon answers confidently with something that is not true.

## The rule

**Identity, not path.** A verified worktree pins the directory's device and
inode. A tombstone leaves that state only when a directory carrying exactly that
identity reappears at the recorded path — which is what an unmounted volume
returning looks like, and is not what a reused path looks like. A tombstone that
never had a verified identity (the path was already gone at first index) can
never be revived.

**Session-directory authority, not project containment.** A snapshot entry is
admitted only if it names a direct child of *this project's sessions
directory*, has no `..` component, ends in `.jsonl`, is a regular file today,
and its current device/inode/length/mtime is exactly the one the entry was
derived from. A refused entry costs one bounded re-scan of a file that is on
disk anyway.

## What the injection removes

Both rules, restored to what they were:

- `reverify_authority` lifts a tombstone on mere existence at the recorded path;
- `snapshot_entry_is_admissible` goes back to `starts_with(project_root)`.

The two tests then watch a reused path revive a dead run, and seven kinds of
semantically corrupt entry load into the catalog as if they were session
records.
