# verify/TASK-FZB6T-redraw — the storage lock, proved at the choke point

Replay: `orgasmic verify TASK-FZB6T --artifact verify/TASK-FZB6T-redraw`.
Sibling artifacts: `verify/TASK-FZB6T` (the catalog), `verify/TASK-FZB6T-corruption`
(a corrupt catalog poisoning classification).

## The defect

dec_WDR5K item 7 and TASK-AFE5Q got rendered TUI output out of the JSONL by
changing the pane transports: rmux drops each pane chunk after measuring it and
publishes a coalesced `PaneActivity` byte *count*. That is the right fix and it
holds today.

It is also a fix in the drivers, and drivers are where the 2.239 GiB came from
in the first place. Nothing downstream refused the bytes. A driver that
synthesized a `text_chunk` from accumulated repaints, a new pane transport that
forwarded scrollback for a UI feature, a harness whose ACP `tool_result` carried
a whole `cargo test` log — all of it was persisted verbatim, because
`SessionWriter::append` wrote whatever it was handed.

## The fix

A per-payload cap at the writer, which is the single choke point every persisted
harness payload goes through (`orgasmic-core::session`):

- any individual string payload inside a `driver_event` longer than
  `DRIVER_EVENT_PAYLOAD_CAP_BYTES` (16 KiB) is replaced by its first 2 KiB plus a
  marker, with a sibling `<key>_bounded` object carrying the full byte count, the
  SHA-256 of the original, and the source reference;
- **structure is never payload**: `type`, `call_id`, `name`, `ok`, `seq`,
  `stream` and the envelope `time` are untouched, so a bounded stream still
  answers how many tool calls ran, which failed, which were retried, and how long
  each took;
- lifecycle, babysitter-summary, and note envelopes are supervisor-authored
  authority and are written verbatim — a digested `prompt_draft` would lose the
  operator the exact text that was staged;
- the digest names the harness-native transcript rather than a path orgasmic
  would have to keep valid. orgasmic never copies vendor-owned history.

The cap is idempotent, so re-bounding an already-bounded event is a no-op.

## Why the probe drives a regression rather than the current transport

The first half of `a_long_tui_session_persists_no_rendered_redraw_bytes` emits
only `PaneActivity` counts and asserts the file stays small — which is true, and
proves the *fixture*. So the test then does what a regression looks like: it
hands the writer a 4.6 MB `text_chunk` built from the same repaints, as a driver
that had stopped dropping them would. Under the injection the session file grows
by 5.8 MB. That is the assertion that makes this a lock instead of a description.
