# Review: TASK-W97C8 — typed `evidence.json` in dispatch records

Diff reviewed: `bb0645fb..bc4ee26d` (branch `task-w97c8-impl`), 4 files.

## Verdict

**FINDINGS — blocks ship.**

The design is right and the promote/partial-failure plumbing is correct. But the
two wires that carry real data are both cut. Measured against the live ledger and
real session JSONL, **every closed dispatch would promote an `evidence.json` of
`event_count: 0`, `tool_call_count: 0`, `narrative: []`** — the same "no work
happened" lie as the 0-byte `stdout.log` this task exists to remove, now dressed
as a typed, authoritative record. Neither failure is visible to the new tests,
because they bypass the production path. A gate test also fails outright.

## Findings

### F1 — HIGH (bug, unmet acceptance): `session_path` is always `None`; every real close writes empty evidence

`crates/orgasmic-cli/src/manager.rs:10531`

```rust
session_path: run_extra("TARGET").map(PathBuf::from),
```

`run_extra` reads `TxEntry.extra` (`manager.rs:10515`, `manager.rs:10625`).
`TARGET` is never in `extra`: `parse_tx_file` lifts it into the first-class
`TxEntry.target` field and *explicitly filters it out* of `extra`
(`crates/orgasmic-core/src/tx.rs:713`, and the exclusion list at
`crates/orgasmic-core/src/tx.rs:718-731`). The daemon writes it as `target:`,
not as an extra (`crates/orgasmic-daemon/src/api.rs:7533`).

Compiled probe against the live ledger (`~/.orgasmic/ledgers/orgasmic/.orgasmic/tx/2026-08.org`,
225 dispatch folds), using the real `orgasmic-core` `parse_tx_file`/`fold_dispatches`:

```
started=tx-20260826-orgasmic-6456 run_tx=tx-20260826-orgasmic-6457
  TxEntry.target = Some(".orgasmic/tmp/sessions/dispatch-TASK-KA934-implementer-20260826T195627.jsonl")
  extra["TARGET"] (what manager.rs:10531 reads) = None
```

`None` for every fold, every month. So `promote_and_persist_dispatch_record`
always calls `build_dispatch_evidence(None, ...)` (`manager.rs:8153-8175`) and
promotes:

```json
{"event_count":0,"tool_call_count":0,
 "session":{"status":"missing","reason":"dispatch record has no session JSONL target"},
 "native_transcript":{"status":"not_found",...},"narrative":[]}
```

Acceptance "closed dispatch record contains evidence.json with counts, pointers,
and assistant/reasoning text" and "a run that did work can never produce an empty
evidence file" are both unmet on the production path.

Fix: read the field, not the extras — `run.and_then(|e| e.target.as_deref())` —
and keep the relative→absolute join at `manager.rs:10498-10506`.

### F2 — HIGH (bug, unmet acceptance): strict `DriverEvent` parsing aborts on real session files ~40 lines in

`crates/orgasmic-cli/src/manager.rs:8264-8267` (and the envelope parse at `:8258`)

```rust
let event: DriverEvent = serde_json::from_value(envelope.event.clone())
    .map_err(|err| format!("parse driver event on line {}: {err}", line_index + 1))?;
```

Any parse error propagates out of `parse_dispatch_evidence` and lands in
`unreadable_dispatch_evidence` (`manager.rs:8231`) — counts 0, narrative `[]`,
whole file discarded. That is not a rare case: the session writer **deliberately
elides oversized subtrees**, replacing them with a digest stub
(`crates/orgasmic-core/src/session.rs:590-657`, `digest_subtree` →
`{"orgasmic_bounded":{bytes,sha256,retained_bytes,reason,source}}`). A bounded
event is no longer a valid `DriverEvent` — required fields (`eventId`,
`itemType`) are gone, and neither `DriverEvent` nor `ProviderRuntimeEventKind`
has a `#[serde(other)]` fallback (`session.rs:937`, `session.rs:762`).

Probe using the real `orgasmic-core` types over 5 real session JSONL files:

```
dispatch-TASK-W97C8-implementer-20260828T075551.jsonl
  parse_err=Some("driver event line 30: missing field `itemType`")
dispatch-TASK-W97C8-reviewer-20260828T082546.jsonl
  parse_err=Some("driver event line 38: missing field `eventId`")
dispatch-TASK-JHWNP.1-implementer-20260827T231023.jsonl
  parse_err=Some("driver event line 59: missing field `eventId`")
dispatch-TASK-EHE15-implementer-20260827T175026.jsonl
  parse_err=Some("driver event line 51: missing field `eventId`")
dispatch-TASK-EMY0M-implementer-20260719T123637.jsonl   parse_err=None
```

The offending line in this task's own reviewer session:

```json
{"seq":37,...,"kind":"driver_event","event":{"event":{"orgasmic_bounded":{
 "bytes":23652,"reason":"subtree-size","retained_bytes":0,"sha256":"5bdb8e74...",
 "source":"harness-native session transcript (vendor-owned; never copied by orgasmic)"}},
 "type":"provider_runtime"}}
```

4 of 5 real sessions abort within the first 60 lines. Core's own reader is
lenient about this by construction — `read_session_file`
(`session.rs:1241-1251`) keeps `event` as a raw `Value` and never types it. The
new code introduces the strictness.

Fix: skip and *count* unparseable/bounded events rather than failing the file
(`bounded_events` / `unparsed_events` fields make the elision visible), and
count a `type: "provider_runtime"` stub as one event.

### F3 — MEDIUM (bug, unmet acceptance): narrative is always empty for `codex-chat`

`crates/orgasmic-cli/src/manager.rs:8316-8324` (`evidence_provider_stream`)

Independent of F2 — a whole-file scan (no strict typing, so the bounded lines do
not stop it) shows codex-chat runs carry **no assistant or reasoning text at
all** in the session JSONL:

| session | harness | `text_chunk` streams | `content.delta` streamKinds | narrative bytes |
|---|---|---|---|---|
| `dispatch-TASK-W97C8-implementer` | codex-chat | `stderr`×20, `user`×1 | `command_output`×13 | **0** |
| `dispatch-TASK-JHWNP.1-implementer` | codex-chat | 2 (non-assistant) | `command_output`×12 | **0** |
| `dispatch-TASK-W97C8-reviewer` | claude | 1 | `assistant_text`×3 | 96 |
| `dispatch-TASK-EHE15-implementer` | claude | 1 | `assistant_text`×28 | 1301 |

The codex adapter maps `TextStream::Assistant → "assistant_text"` and
`System → "reasoning_text"` (`crates/orgasmic-drivers/src/adapters/codex.rs:620-641`),
so the filter itself is right — codex-chat simply never emits those. Its agent
messages land in `item.completed` with `itemType: "tool_call"` only. The
implementer's *own* run therefore produces zero narrative.

Either project narrative from the item lifecycle payloads for codex-chat, or
state in the task/convention that narrative is claude-only today. Silently empty
is the failure mode this task was opened to kill.

### F4 — MEDIUM (correctness): `tool_call_count` undercounts by 2–3x on codex-chat

`crates/orgasmic-cli/src/manager.rs:8289-8294`

```rust
ProviderRuntimeEventKind::ItemStarted(item)
    if matches!(item.item_type.as_str(),
        "command_execution" | "tool_call" | "mcp_tool_call") => { tool_call_count += 1; }
```

That is a whitelist of three literals, but the codex adapter sets `item_type` to
the **tool name**, not a category
(`crates/orgasmic-drivers/src/adapters/codex.rs:642-662` sets
`item_type: name.clone()`; names come from `codex.rs:968-992` — `file_change`,
`web_search`, MCP/dynamic tool names, plus `function_call` / `custom_tool_call`
names at `codex.rs:1024`, `codex.rs:1050`).

Measured `item.started` itemTypes vs what gets counted:

| session | counted | actual `item.started` | uncounted types |
|---|---|---|---|
| `W97C8-implementer` | 99 | 262 | `exec`×138, `file_change`×25 |
| `JHWNP.1-implementer` | 75 | 183 | `exec`×95, `file_change`×11, `wait`×2 |
| `EHE15-implementer` | 30 | 39 | `file_read`×3, `file_change`×6 |

A run that only edited files reports `tool_call_count: 0`. Count `ItemStarted`
generically (or key off the presence of `data.toolName`) instead of a name
whitelist.

### F5 — MEDIUM (test): a gate test fails on this commit

```
$ rustup run 1.97.1 cargo test -p orgasmic-cli --test shipped_conventions
test manager_convention_names_post_close_report_path ... FAILED
panicked at crates/orgasmic-cli/tests/shipped_conventions.rs:406:5:
  year-one growth for last.txt and the stdout.log bound must be stated
test result: FAILED. 4 passed; 1 failed
```

The convention rewrite dropped `24–30 MB/yr`
(`shipped/prompt-studio/conventions/manager-dispatch.org:391-392` now only keeps
the stdout number) but `shipped_conventions.rs:405-408` still asserts it. The
next assertion, `shipped_conventions.rs:428-431` ("the prose must say what a
non-zero stdout.log.bytes with no stdout.log means"), is now also unsatisfiable —
that sentence was correctly deleted with the sidecar. Both guards need updating
in the same change; the second should be replaced by an `evidence.json` guard,
otherwise nothing protects the new prose.

### F6 — MEDIUM (test): the new tests structurally cannot catch F1 or F2

- `manager.rs:12920-12941` — `dispatch_close_clean_worktree_has_no_salvage_side_effects`
  hand-assigns `open.session_path = Some(session_path)` and
  `open.run_id = Some("run-evidence")`, so it never exercises
  `dispatch_record_from_fold` (`manager.rs:10531`), which is exactly where F1
  lives. No test anywhere builds a `DispatchRecord` from a `run.created` tx and
  asserts `session_path.is_some()`.
- `manager.rs:10879`, `:11049` — the two "non-empty" tests assert
  `!json.is_empty()`, which is true of the all-zeros file. They prove
  serialization, not evidence.
- `manager.rs:10897` (`dispatch_evidence_projects_work_without_tool_payloads`)
  synthesizes clean, fully-typed envelopes. No fixture contains an
  `orgasmic_bounded` stub, an unknown `type`, or a truncated final line — so F2
  is invisible.

The payload-exclusion assertions (`!serialized.contains("must-not-leak")`) are
good and I found no leak: only `item_type` and `content.delta.delta` are read;
`ToolCall.args`, `ToolResult.output` and `ProviderItemLifecyclePayload.data`
(which *does* carry `{"toolName","input"}` at `codex.rs:658`) are never
serialized. Claim 2's exclusion half holds.

### F7 — LOW (correctness, latent): recovered generations pair the wrong run id with the wrong session file

`session_path` is read from `dispatch.run`, which `fold_dispatches` pins to the
**initial** `run.created` (`crates/orgasmic-core/src/tx.rs:190`), while `run_id`
is `addressed_run_id`, which recovery advances to the **replacement**
(`tx.rs:143-149`). Recovery's `run.created` writes `target: None`
(`crates/orgasmic-daemon/src/api.rs:8364`). So after a recovery, the filter at
`manager.rs:8261` matches zero envelopes in the original file: counts 0,
narrative empty, `session.status: "found"` — a silent, confident zero.

Not observed live: `grep -c "ORIGIN:       recovery"` is 0 across
`2026-06.org`, `2026-07.org`, `2026-08.org`. Latent, but it is the same class of
bug as F1 and will surface the moment F1 is fixed.

### F8 — LOW (design): the "refuses empty evidence" guard is unreachable

`crates/orgasmic-core/src/paths.rs:433-438` rejects `bytes.is_empty()`, but
`build_dispatch_evidence` (`manager.rs:8153-8175`) always returns pretty-printed
JSON plus a newline — it cannot return zero bytes. The guard is true by
construction and enforces byte-length, not meaning. If the acceptance criterion
("a run that did work can never produce an empty evidence file") is to bite,
the floor needs to be semantic (e.g. refuse when `session.status == "missing"`
while a `TARGET` was recorded, or when counts are 0 and no failure reason is
named).

### F9 — LOW (design): narrative has no size bound, unlike the thing it replaces

`push_dispatch_narrative` (`manager.rs:8326-8342`) concatenates without a cap,
into a file committed forever, replacing an artifact whose whole point was the
explicit `STDOUT_PROMOTE_MAX_BYTES` bound. Measured narrative on real sessions is
tiny (max 1.3 KB over the files sampled), so this is not urgent — but the
convention now states a retention number for `stdout.log` and none for
`evidence.json` (`manager-dispatch.org:391-395`). A cap plus a `narrative_truncated`
flag keeps the guarantee the old design had.

## Open Questions

1. Should `evidence.json` name how much was elided (`bounded_events`,
   `bytes_elided`)? The session writer already computes `BoundStats`
   (`session.rs:648-655`); surfacing it turns F2's failure mode into a fact.
2. Is `TextStream::System → "reasoning_text"` (`codex.rs:626`) meant to be read
   back as *reasoning* narrative? On codex-chat, `System` carries harness
   notices, not model thinking — that would put harness chatter in the record
   under a "reasoning" label.
3. `transcript_finder.rs:204` (`"codex-chat" => "codex"`) is a real fix and is
   tested, but it is not named in the task. Intentional in-scope, or drive-by?

## Verification Notes

All commands run from `/Users/aspirational/.orgasmic/worktrees/orgasmic/task-w97c8-review`
with the pinned toolchain (`rust-toolchain.toml` = 1.97.1; note plain `cargo` on
this machine resolves to Homebrew rustc 1.94.1, so `rustup run 1.97.1` was used
throughout). No repo file was modified; probes live in `/tmp/w97c8probe` and
`/tmp/w97c8_probe.py`.

| check | result |
|---|---|
| `cargo test -p orgasmic-core --lib paths::` | **ok, 13 passed** — promote/partial-failure suite is green |
| `cargo test -p orgasmic-cli --bin orgasmic -- dispatch_evidence dispatch_close_clean_worktree` | **ok, 4 passed** — the new tests pass (see F6 for why that is not reassuring) |
| `cargo test -p orgasmic-cli --test shipped_conventions` | **FAILED, 4 passed / 1 failed** — F5, a regression introduced by this commit |
| `rustup run 1.97.1 cargo fmt --all -- --check` | 8 diffs, **all pre-existing** — `main.rs:1863`, `manager.rs:5989`, `manager.rs:7545`, `core/lib.rs:37`, `projects.rs:212`, `api.rs:31241`, `prompt_compiler.rs:1454/1464`; none is a line this commit touched. Classified pre-existing, not a finding. |
| Rust probe: `extra["TARGET"]` over the live ledger | `None` for all 225 dispatch folds → F1 |
| Rust probe: strict `DriverEvent` parse over 5 real session JSONL | 4/5 abort at line 30–59 → F2 |
| Python whole-file scan of the same 5 sessions | narrative 0 bytes on both codex-chat runs → F3; counted-vs-actual `item.started` → F4 |
| `grep -c "ORIGIN:       recovery"` over `2026-0[678].org` | 0 → F7 is latent, not live |

Claims from the brief, adjudicated:

1. **Run-id filtering** — the filter itself is correct (`manager.rs:8261`), but it
   is never reached, because `session_path` is `None` (F1); and it pairs a
   replacement run id with the initial run's file after a recovery (F7).
2. **Counts / pointers / narrative, no payloads** — payload exclusion **holds**
   (verified: no `ToolCall.args`, no `ToolResult.output`, no
   `ProviderItemLifecyclePayload.data` reaches the JSON). Counts and narrative
   are wrong (F2, F3, F4).
3. **"Never empty" / missing-JSONL edge** — a missing JSONL yields a typed
   `status: "missing"` record; close does **not** fail loudly, it promotes a
   zeroed file and reports no `CLEANUP_ERROR`. Because of F1 that is the *only*
   outcome in production. The `refuses empty` guard is unreachable (F8).
4. **Partial-failure discipline** — **holds.** `unlink_validated_attempt_pair` runs
   only in the all-succeeded branch (`paths.rs:349-356`); the evidence-failure
   path returns early with `report_path: Some(..)` and no unlink
   (`paths.rs:335-341`), and `write_promoted_bytes_to` scrubs its own
   `.promoting` temp (`paths.rs:446-449`). The QGWK7 rule is preserved.
5. **`stdout.log.bytes` removed** — **holds** in code
   (`paths.rs:470-486` no longer writes the sidecar; empty stdout now unlinks any
   stale `dest`). One stale reader remains, and it is a test:
   `crates/orgasmic-cli/tests/shipped_conventions.rs:428-431` (F5).
6. **Heartbeat/pane excluded, provider command starts counted** — exclusion holds
   (`manager.rs:8271-8276`); the counting classification is wrong (F4).
7. **No new `TextStream` variant** — **holds.** `TextStream`
   (`crates/orgasmic-core/src/session.rs:1027-1035`) is untouched; reasoning is
   projected from `ProviderRuntimeEventKind::ContentDelta`. No dead schema added.

Not run (not assigned, and not needed to reach the verdict): the full gate suite.
Residual risk: `orgasmic-daemon` and the `verify/` gates were not exercised, so a
non-fmt regression outside `orgasmic-cli`/`orgasmic-core` would not have been seen
here.

## Fix Directions

1. `manager.rs:10531` — read the tx field, not the extras map:
   `session_path: run.and_then(|e| e.target.as_deref()).map(PathBuf::from)`.
   Add a test that folds a real-shaped `run.created` tx and asserts
   `session_path.is_some()` — that single test is what F1 slipped past.
2. `manager.rs:8244-8312` — make the parser lossy-tolerant. Skip a line that
   fails to type, tally it (`unparsed_events`, `bounded_events`), and keep
   going. Add a fixture containing a real `orgasmic_bounded` stub (copy one out
   of `dispatch-TASK-W97C8-reviewer-20260828T082546.jsonl:38`) and a truncated
   final line.
3. `manager.rs:8289-8294` — stop whitelisting tool names. Count every
   `ItemStarted`, or exclude a known non-tool set, so `file_change` / MCP /
   dynamic tools land in the count.
4. F3 — decide and write it down: project codex-chat agent messages from the
   item lifecycle, or state in the task node and the convention that narrative
   is claude-only for now.
5. `shipped_conventions.rs:405-408` and `:428-431` — update both guards with the
   convention. Replace the sidecar assertion with one that pins the
   `evidence.json` contract, so the new prose is protected the way the old prose
   was.
6. After (1): fix F7 by taking `session_path` from the addressed run when it has
   one, and falling back to the initial run's `target` only when the addressed
   run is a recovery replacement (which always carries `target: None`).
