# verify/TASK-FZB6T-corruption — a derived index must never outrank the thing it derives from

Replay: `orgasmic verify TASK-FZB6T --artifact verify/TASK-FZB6T-corruption`.
Sibling artifacts: `verify/TASK-FZB6T` (the catalog), `verify/TASK-FZB6T-redraw`
(the storage lock).

## The defect this prevents

The run catalog ships a durable snapshot so a restart does not re-index the whole
board. That snapshot is a file on disk in `.orgasmic/tmp/`, and every file on
disk eventually shows up torn (a kill during the write), empty, half-written by a
full volume, or written by a different build of the daemon than the one reading
it.

A catalog is not a cache miss when it is wrong — it is an *answer*. An index that
is trusted unconditionally can tell the inventory that a live run released, that
a failed run is terminal-noop, or that a run exists at all, none of which the
session files say. That is worse than having no catalog, because the durable
record that would have corrected it is the one thing the catalog exists to avoid
reading.

## The rule

The catalog is derived state and never authority. Session JSONL decides; the
catalog only remembers what a bounded read of it already said. So every way the
snapshot can be wrong has the same answer — **discard and re-index** — and the
tests pin that the re-indexed board classifies *exactly* as a board with no
snapshot at all:

- **truncated mid-object** → `Corrupt`, rebuild
- **not JSON** → `Corrupt`, rebuild
- **a `catalog_version` this build cannot vouch for** → `VersionMismatch`, rebuild
- **an entry naming a session file outside the project it is loaded for** →
  refused entry-by-entry, so a copied snapshot cannot inject foreign runs
- **an entry whose file-identity fingerprint no longer matches the file on disk**
  → re-derived by `refresh_dir` before anyone reads it

The version check is deliberately an equality test, not a floor. Rollback is a
real operation: an operator installs an older runtime and it must refuse a
catalog shape it does not know rather than read the fields it happens to
recognize. Both directions are the same problem and take the same answer.

## What the injection removes

The `catalog_version` check in `load_snapshot`. Any catalog file on disk is then
trusted, whatever wrote it — and the acceptance test watches 197 unverifiable
entries become the inventory's answer.
