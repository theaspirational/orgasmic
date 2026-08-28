# Review: TASK-W97C8 round 2 — typed `evidence.json` in dispatch records

Diff reviewed: `bb0645fb..dbf66f4e` (branch `task-w97c8-impl-r2`), 5 files,
+982/−113. Round-1 review: `.orgasmic/tmp/dispatch/task-w97c8/review-round-1.md`.

## Verdict

**APPROVE-WITH-FOLLOW-UPS.**

All nine round-1 findings are closed, and I verified each against real ledger
and session data rather than against the tests. The two wires that were cut in
round 1 now carry data: measured end-to-end, the same production-shaped
projection that produced `{0, 0, []}` for every close in round 1 now produces
non-zero counts on **452 of 452** historical runs whose session JSONL still
exists (the other 150 are age-deleted files, which correctly yield
`session.status: "missing"` with a named reason).

Two follow-ups worth opening; neither blocks ship, neither is a regression of
this diff, and neither reintroduces the "no work happened" lie the task exists
to kill.

## Findings

### N1 — MEDIUM (correctness): `tool_call_count` now double-counts codex-chat by ~1.9×

`crates/orgasmic-cli/src/manager.rs:8336-8343`

Round-1 F4 was a 2–3× *under*count; the fix ("count every `ItemStarted` minus a
non-tool list") overshot in the other direction. On codex-chat the provider
emits **two `item.started` events per model tool call** — the model-issued call
and the runtime execution it spawns — and the new code counts both.

Measured on `dispatch-TASK-W97C8-implementer-20260828T075551.jsonl`, grouping
`item.started` by `itemType` and `itemId` prefix:

| itemType | itemId prefix | count | layer |
|---|---|---|---|
| `exec` | `call_` | 138 | model-issued tool call |
| `command_execution` | `exec-` | 99 | runtime execution of that call |
| `file_change` | `exec-` | 25 | runtime side-effect of that call |

They interleave 1:1 in file order, same shell command in both:

```
exec             | call_SkjdzBH0wVvE7ri4pP6er6AG | {"input":"const r = await tools.exec_command({cmd:\"cat …SKILL.md && …\"})"}
command_execution| exec-43c31d36-b38a-484c-bee5-6e0efa81df02 | {"input":{"command":"/bin/zsh -lc \"cat …SKILL.md && …\""}}
```

`dispatch-TASK-JHWNP.1-implementer` is the cleanest proof, because there the
model layer is closed explicitly: `item.started exec (call_) = 95` and
`item.completed tool_call (call_) = 95`, against `item.started
command_execution (exec-) = 75` + `file_change (exec-) = 11`. Reported
`tool_call_count` = 183 for 97 actual model tool calls.

Claude sessions have **one** layer (every `item.started` carries a `toolu_`
itemId), so they are counted correctly. Net effect: the field is not comparable
across drivers — the round-2 live probe's `codex 262` vs `claude 71` reads as
3.7× the work when roughly half the codex number is the same calls counted
twice.

Not a ship blocker: the count is non-zero, directionally right, and no consumer
reads it yet. But the convention sells it as "tool-call counts"
(`manager-dispatch.org:380`), and it is not that on codex.

Fix direction: the discriminator is already in the data. Count only the
model-issued layer — on codex that is `itemId` starting `call_` (equivalently
`item.completed` with `itemType: "tool_call"`); on claude every item is that
layer already. Alternatively keep the current semantics and rename the field
`tool_item_count`, saying in the convention that it counts runtime tool items
across layers, not model tool calls.

### N2 — MEDIUM (docs): the convention promises narrative that the JSONL does not contain, for Claude too

`shipped/prompt-studio/conventions/manager-dispatch.org:380-386`

The prose now says evidence carries "assistant/reasoning narrative in event
order", and names exactly one exception: "Codex-chat does not emit
assistant/reasoning deltas today, so its narrative is empty." A reader
reasonably concludes the Claude case is whole. It is not — it is ~1%.

Measured, counting **every** `content.delta` in the file regardless of filter:

| session | harness | assistant deltas | narrative bytes | assistant blocks in the run |
|---|---|---|---|---|
| `W97C8-reviewer-20260828T082546` | claude | 4 | **141** | 42 (`item.completed`/`assistant_message`) |
| `W97C8-reviewer-20260828T085711` | claude | 2 | **95** | — |
| `EHE15-implementer` | claude | 28 | **1301** | — |
| `W97C8-implementer` (both) | codex-chat | 0 | **0** | 0 |

141 bytes is the opening sentence of a full review. The other 41 assistant
blocks are present as `item.completed` / `itemType: "assistant_message"`, but
their `payload.data` is `null` (verified: 42 items, 168 total bytes of `data`,
all nulls) and their `raw` is a bare `content_block_stop` — the text genuinely
is not in the session JSONL.

This is **not** a defect in this diff — the projection is faithful to its input,
and the emitter is the external local-first runtime, not this repo (grep for
`ContentDelta`/`streamKind` producers in `crates/` finds only
`adapters/codex.rs`). But the convention is what a manager will read, and it
currently overstates. Fix direction: state the measured reality in the same
sentence that already names the codex gap — narrative is a partial excerpt of
assistant text on every driver today, and `event_count`/`tool_call_count` plus
the native transcript pointer are the load-bearing proof of work. Open a
follow-up against the runtime for the missing deltas if the full prose is
wanted in the record.

### N3 — LOW (design): the semantic floor suppresses the stdout crash log in exactly the case stdout exists for

`crates/orgasmic-core/src/paths.rs:433-451` and `paths.rs:335-341`

`write_promoted_bytes_to` refuses zero-count evidence, and its failure
early-returns from `promote_validated_dispatch_attempt` **before** the
`stdout.log` promotion at `paths.rs:344`. So a run that produced no driver
events promotes `report.md` and nothing else — no `evidence.json`, and no
`stdout.log` excerpt — even though a harness that died before emitting a single
driver event is the one case where the stdout crash insurance is the only
evidence there is.

Latent, not live: I replayed the round-2 projection over every `run.created`
with a `TARGET` in `2026-06.org`, `2026-07.org`, `2026-08.org` — **452 pass the
floor, 0 refused, 150 have an age-deleted session file** (which takes the
`missing` + reason branch and passes). So the floor does not false-positive on
any run this project has ever produced, including runs that only ran a handful
of commands. The brief's specific worry — a legitimately idle run producing only
lifecycle events — does not occur, because `DriverEvent::Ready` alone counts.

Fix direction: promote `stdout.log` before the floor, or on the floor's error
path, so a refused evidence file never costs the crash log too.

## Answers to the brief's explicit questions

- **Is the F4 exclusion list right? Does `wait` count now, and is that
  acceptable?** `wait` counts (2–4 per codex session). It is acceptable and in
  fact correct: `wait` carries a `call_` itemId, i.e. it is a model-issued tool
  call, not a runtime artifact. The exclusion list itself
  (`agent_message`/`agentMessage`/`reasoning`) never fires on any real session I
  sampled — codex emits no such items, and claude emits `assistant_message`
  (different spelling) only on `item.completed`, which is not counted. It is
  harmless but unexercised precaution; the live over-count is N1's layering, not
  the list.
- **Can the F8 floor false-positive on a legitimately idle run?** No, measured:
  0 refusals over 452 real runs (above). Residual edge is N3's ordering, not the
  predicate.
- **Are the new `shipped_conventions` guards vacuous?** No. The replacement
  guard (`shipped_conventions.rs:427-433`) requires `=evidence.json=`,
  `=unparsed_events=` and `=narrative_truncated=` in the prose; deleting any of
  the three new convention sentences fails it. The restored
  `24–30 MB/yr` + `64 KB` guard (`:405-408`) matches
  `manager-dispatch.org:397-399`.
- **Would the new fold test have caught round 1?** Yes. It parses a real-shaped
  `run.created` through `parse_tx_file` and asserts
  `entries[1].extra` contains no `TARGET` **before** asserting
  `record.session_path.is_some()` (`manager.rs:12536-12551`) — round-1's
  `extra["TARGET"]` read fails it on the second assertion.

## Per-finding verification of round 2

| # | claim | verdict | evidence |
|---|---|---|---|
| F1 | fold reads first-class `TxEntry.target` | **closed** | `manager.rs:10597-10609` reads `entry.target`; `dispatch_fold_reads_run_created_target_field` passes and is non-vacuous (asserts the `extra` filter first) |
| F2 | lossy parser, continues past bounded + truncated | **closed** | `manager.rs:8286-8316`; independent replay over 9 real sessions: every file parses to completion, e.g. `W97C8-implementer` 436 events / 262 tools / 1 bounded, `W97C8-reviewer` 775 / 71 / 5 bounded. Round 1 aborted 4/5 at line 30–59. Fixture at `manager.rs:11185` is the verbatim `orgasmic_bounded` line from the round-1 session plus a truncated `{"seq":39` tail |
| F3 | claude-only narrative documented; codex `System` excluded | **closed** | `node.org:30` (Decisions) + `manager-dispatch.org:380-386`; `evidence_provider_stream("reasoning_text") == None` (`manager.rs:11229`). Verified the exclusion is right: codex `System` is harness notices. See N2 for what the documentation still understates |
| F4 | generic `ItemStarted` counting | **closed, overshot** | `exec`/`file_change`/MCP names now count (round-1's undercount is gone) — but see **N1** |
| F5 | `shipped_conventions` 5/5 | **closed** | `cargo test -p orgasmic-cli --test shipped_conventions` → **5 passed, 0 failed** |
| F6 | tests exercise the paths round 1 bypassed | **closed** | fold test + lossy fixture test both fail against round-1 code by construction. The close-path test still hand-assigns `session_path` (`manager.rs:13195`), but the fold test now covers that seam separately |
| F7 | recovery pairing never mixes path and run id | **closed** | `manager.rs:10597-10609` takes `(target, RUN_ID)` from the same entry in both branches; `dispatch_fold_prefers_addressed_session_then_falls_back_to_initial` asserts `replacement.jsonl`+`run-b`, then with `target: None` asserts `initial.jsonl`+`run-a`. Independently confirmed run-id pairing is real: 6/6 sampled live `run.created` TARGETs have 100% of their session envelopes carrying the paired `RUN_ID` |
| F8 | semantic floor | **closed** | `paths.rs:444-451`; `promoted_evidence_requires_work_or_a_named_failure` passes. See **N3** for the ordering edge |
| F9 | 64 KiB UTF-8 cap + `narrative_truncated` | **closed** | `manager.rs:8380-8410`, `DISPATCH_NARRATIVE_MAX_BYTES = 64 * 1024`; `dispatch_evidence_caps_narrative_on_a_utf8_boundary` asserts exactly 65536 bytes retained and the flag set |

Brief's cross-cutting checks:

- **No payload leakage.** Holds. Only `item_type`, `content.delta.delta`,
  `TextChunk.chunk` are read; `ToolCall.args`, `ToolResult.output` and
  `ProviderItemLifecyclePayload.data` never reach the JSON
  (`manager.rs:8326-8351`), and
  `dispatch_evidence_projects_work_without_tool_payloads` asserts
  `!serialized.contains("must-not-leak")` for both args and output. Confirmed by
  reading the serializer, not only the test.
- **Partial-failure discipline.** Intact. `unlink_validated_attempt_pair` runs
  only in the all-succeeded branch (`paths.rs:350`); both early returns keep
  `report_path: Some(..)` and leave tmp alone; `.promoting` temps are scrubbed
  on error (`paths.rs:454-457`, `paths.rs:520-522`).
- **No daemon API changes.** Confirmed: the diff touches 5 files, none under
  `crates/orgasmic-daemon/`.
- **`stdout.log.bytes` removed.** Confirmed: repo-wide grep finds only four
  negative assertions and the convention sentence "There is no
  `=stdout.log.bytes=` sidecar".
- **`transcript_finder.rs` `codex-chat -> codex`.** Kept, with the justification
  the brief asked for. It is a behaviour change beyond `evidence.json` —
  `normalize_harness` also feeds the live transcript lookup — but it turns
  `Unsupported` into a real adapter for a driver mode this project runs, and it
  is guarded by `codex_chat_uses_the_codex_transcript_adapter`.

## Open Questions

1. Should `bounded_events` and `unparsed_events` be disjoint? Today a bounded
   line increments **both** (`manager.rs:8306` then `:8313`). Measured overlap
   is partial, not total — `W97C8-reviewer` is 5 bounded / 5 unparsed
   (identical set), `JHWNP-reviewer` is 13 bounded / 10 unparsed (3 bounded
   lines still typed). A reader summing the two fields over-reports the damage.
   One sentence in the convention would settle it either way.
2. `write_promoted_bytes_to` (`paths.rs:433-451`, orgasmic-core) validates the
   evidence contract by string key — `event_count`, `tool_call_count`,
   `unparsed_events`, `/session/reason` — against JSON built in
   `orgasmic-cli`. There is no shared type and no test pinning the two together.
   Drift fails loud (a missing key reads as 0 and the floor refuses), so this is
   a maintainability note, not a bug. Worth a shared const or a serde struct if
   the schema grows.

## Verification Notes

All commands from
`/Users/aspirational/.orgasmic/worktrees/orgasmic/task-w97c8-review` with the
pinned toolchain (`rustup run 1.97.1`; plain `cargo` here is Homebrew 1.94.1).
No repo file was modified. Probe scripts live in `/tmp/w97c8_r2_probe.py` and
inline heredocs; they print counts only and never emit tool arguments or
outputs.

| check | result |
|---|---|
| `cargo test -p orgasmic-cli --test shipped_conventions` | **5 passed, 0 failed** — F5 regression gone |
| `cargo test -p orgasmic-cli --bin orgasmic -- dispatch_evidence dispatch_fold_ codex_system dispatch_close_clean_worktree` | **9 passed, 0 failed** |
| `cargo test -p orgasmic-core --lib paths::` | **14 passed, 0 failed** (was 13; +semantic floor) |
| `cargo test -p orgasmic-drivers --lib -- transcript_finder` | **23 passed, 0 failed** |
| `cargo fmt --all -- --check` | 8 diffs, **the same 8 pre-existing regions** as round 1 (`main.rs:1863`, `manager.rs:5992`/`7548`, `core/lib.rs:37`, `projects.rs:212`, `api.rs:31241`, `prompt_compiler.rs:1454`/`1464`); line numbers shifted, regions unchanged, none touched by this diff. Classified pre-existing |
| Independent replay of the round-2 projection over 9 real session JSONLs | reproduces the implementer's live-probe numbers exactly (codex impl `436/262`, claude reviewer `775/71`, narrative 141 B) — I re-derived them from the raw files, not from the report |
| Same projection over **every** `run.created` with a `TARGET` in `2026-06/07/08.org` | 452 pass the floor, 0 refused, 150 session files age-deleted (→ `missing` + reason, passes). N3 is latent |
| Live `RUN_ID` ↔ session `run_id` pairing, 6 sampled dispatches | 6/6 with 100% envelope match — the F7 filter addresses real files |
| `item.started` itemId-prefix grouping across codex and claude sessions | two layers on codex (`call_` vs `exec-`), one on claude (`toolu_`) → **N1** |
| All `content.delta` in the claude reviewer session, unfiltered | 4 deltas / 141 bytes total; 42 `assistant_message` items with `data: null` → **N2** |
| Task node Decisions + `manager-dispatch.org` prose | F3 documented in both, as claimed |

Not run (not assigned): the full gate suite, `orgasmic-daemon` tests, `verify/`.
Residual risk: a non-fmt regression outside `orgasmic-cli`/`orgasmic-core`/
`orgasmic-drivers` would not have been seen here; the diff touches nothing else,
so the exposure is low.

Classification of every failure seen: none. The 8 `cargo fmt` diffs are
pre-existing (verified by region against round 1, and none is a line this diff
touches).

## Fix Directions

1. **N1** `manager.rs:8336-8343` — count only the model-issued tool layer.
   Cheapest correct rule: on `ProviderRuntimeEventKind::ItemStarted`, count when
   the event's `itemId` does not carry the runtime prefix, or equivalently count
   `item.completed` with `itemType: "tool_call"` on codex and `item.started` on
   claude. If the layered count is wanted instead, rename the field to
   `tool_item_count` and say so in `manager-dispatch.org`. Either way add a
   fixture with the real `call_`/`exec-` pair so the layering is pinned.
2. **N2** `manager-dispatch.org:380-386` — one sentence: narrative is a partial
   excerpt on every driver today (measured 141 B for a full Claude review, 0 for
   codex-chat); counts, session pointer and native transcript path are the
   load-bearing proof. Optionally open a runtime-side task for the missing
   `content.delta` events.
3. **N3** `paths.rs:335-344` — promote `stdout.log` before the evidence floor,
   or on its error path, so a refused evidence file does not also cost the crash
   log.
