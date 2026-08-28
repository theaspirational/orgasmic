# Brief: TASK-W97C8 — typed evidence.json in dispatch records

The task body carries the full design and acceptance criteria — read it first
(`orgasmic task show TASK-W97C8` or the tracker node). Summary: replace the
always-empty `stdout.log` / `stdout.log.bytes` pair in promoted dispatch
records with a typed `evidence.json` built from the run's session JSONL.

Anchors:
- `crates/orgasmic-core/src/paths.rs` — `promote_validated_dispatch_attempt`
  (~line 318) is the close-time promote; extend it (or a sibling) to emit
  `evidence.json` into `dispatches/<tx>/`. Keep the partial-failure rule:
  unlink tmp only after every intended copy succeeded.
- `crates/orgasmic-core/src/session.rs` — `DriverEvent` (line 938):
  `TextChunk { stream: TextStream }`, `ToolCall`, `ToolResult`;
  `TextStream` (line 1029) has NO Reasoning variant — decide: add
  `TextStream::Reasoning` or project thinking from `ProviderRuntimeEvent`.
  Record the choice in the task journal.
- Session JSONL lives at `.orgasmic/tmp/sessions/dispatch-TASK-X-<role>-<ts>.jsonl`;
  every driver mode feeds it.
- `crates/orgasmic-drivers/src/transcript_finder.rs` — native transcript
  path + confidence (dec_WDR5K); include its result in evidence.json.
- `crates/orgasmic-cli/src/manager.rs` — close path calls the promote and has
  the existing stdout expectations/tests (~8419, 10031-10055, 12447).

evidence.json content (from the task):
- counts: events, tool calls (proof of work as counts, never byte sizes)
- pointers: session JSONL filename + size; native transcript path + confidence
- narrative: Assistant (and reasoning) TextChunk text, in order — NO ToolCall
  args, NO ToolResult outputs
- stdout excerpt promoted ONLY when non-empty; `stdout.log.bytes` removed

Constraints:
- Do not widen the task. No daemon API changes unless the promote genuinely
  requires one.
- Focused tests only (never a whole-crate run): unit tests around the
  evidence builder (empty session file, session with work, missing JSONL —
  a run that did work must never yield empty evidence) and the promote path
  (partial-failure keeps tmp intact).
- Update the manager-dispatch convention text if it names stdout.log
  retention.

Report: files changed, the Reasoning-variant decision and why, test names +
pass counts.
