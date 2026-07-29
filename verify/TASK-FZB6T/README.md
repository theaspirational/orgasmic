# verify/TASK-FZB6T — run enumeration was a function of transcript bytes, on every call

`injection.patch` / `cmd` / `expect-red` are the replayable artifact:
`orgasmic verify TASK-FZB6T`.

Two sibling artifacts cover the task's other defect modes:

| artifact | defect |
| --- | --- |
| `verify/TASK-FZB6T-redraw` | rendered TUI / oversized harness payloads persisting to orgasmic JSONL |
| `verify/TASK-FZB6T-corruption` | a corrupt or foreign-version catalog poisoning classification |

## The defect

TASK-KWSTJ and TASK-7QM8M fixed the *reader*: `scan_session_lifecycle` answers
lifecycle questions from a bounded prefix/tail window instead of reading the
whole session file, which took boot reattach from 14.00 s to 0.145 s on a
2.41 GiB board.

What they did not fix is that the answer was thrown away. Every `GET /api/runs`
and every daemon boot re-derived every record on the board from disk — opening
197 files, reading two windows of each, re-parsing their lifecycle envelopes,
re-canonicalizing their worktrees, and re-deciding verdicts that had not changed
and could not change, because the runs they describe ended.

So enumeration cost stayed a function of what the session files *contain*, paid
again on every poll:

| | files opened per pass | session bytes read per pass |
|---|---|---|
| before (bounded scan, no catalog) | 197 | 2.4 MiB (1x board) / 27 MiB (16x board) |
| after (catalog, steady state) | 197 stat calls | **0** |

The 16x column is the point. Both boards hold the same 197 records and the same
classification-deciding lines; only the payload differs. A cost that follows
payload has no ceiling, and a board is only ever going to get bigger.

## The fix

`crates/orgasmic-daemon/src/run_catalog.rs`: one compact record per run, keyed
by session path and validated by **file identity** — device, inode, length,
mtime. A file that has not been written since it was indexed is answered from
memory. A terminal run is never written again, so after the one-time legacy
index the steady-state inventory reads only the files that are actually live.

Three design choices are what make this safe rather than merely fast:

1. **The entry retains the compact lifecycle envelope set it was derived from.**
   Serving classification from the catalog is therefore the same computation on
   the same input, not a second classifier free to drift. Driver events are
   reduced to their `type` (plus `ready`'s `protocol_version`, the only
   driver-event body any consumer reads) before an entry is built, so an 18 KiB
   capabilities frame never enters the catalog or its durable snapshot.
2. **Reattach and the inventory disagree about an `Interrupted` release, and
   always have** — the classifier stops there and calls the run recoverable,
   reattach falls through to the terminal driver events. The catalog stores the
   two raw facts (`final_release_outcome`, `driver_terminal_event`) separately so
   that asymmetry stays visible instead of being silently normalized away by the
   shared record.
3. **`final_line_run_id` is carried on the entry.** TASK-7QM8M's truncation guard
   — the prefix segment must own the end of the file — is the one thing standing
   between a bounded read and reattaching a run the file itself says is over. A
   guard that only ran on a fresh scan would be skipped by every cache hit.

The durable snapshot (`.orgasmic/tmp/run-catalog.json`, written through the
existing writer boundary) carries the index across a restart. It is derived
state and never authority: absent, corrupt, and foreign-version are all handled
the same way — discard and re-index — which is what `verify/TASK-FZB6T-corruption`
pins.

## What the injection removes

The file-identity cache in `RunCatalog::refresh_dir`. Everything else stays,
including the snapshot, which is then loaded and immediately ignored. That is
precisely the pre-catalog behaviour: a bounded read, re-derived from scratch, on
every call.

## Why the probe is stated as a byte count and not a wall time

The TASK-7QM8M precedent. A timing assertion on a loaded CI box is a flake
generator and says nothing about the shape of the cost. `bytes_inspected` says
the shape directly: zero after the index, and — for the one-time index itself —
bounded by `records × (prefix + tail)` at any payload size, which is a ceiling a
whole-file read does not have.
