# Review Brief: TASK-W97C8 — typed evidence.json in dispatch records

Review the implementer's branch `task-w97c8-impl` (commit bc4ee26d) against
main (bb0645fb). Diff scope: `git diff bb0645fb..bc4ee26d`.

Task: replace the always-empty `stdout.log`/`stdout.log.bytes` in promoted
dispatch records with a typed `evidence.json` built from the run's session
JSONL. Read TASK-W97C8's node for design + acceptance criteria.

Implementer's claims to verify (report in the dispatch record):
1. Close reads the session JSONL named by `run.created` and streams ONLY the
   matching run's events — check run-id filtering; a stem shared across
   attempts must not leak another attempt's events into evidence.
2. evidence.json: event/tool-call counts, session filename+size, transcript-
   finder result, ordered assistant/reasoning narrative. NO ToolCall args,
   NO ToolResult outputs — grep the builder for any raw payload leakage
   (including inside ProviderRuntimeEvent projection).
3. A run that did work can never yield empty evidence; promote REFUSES an
   empty evidence.json. Verify the empty-session edge: what happens when the
   session JSONL is missing entirely — does close fail loudly or silently
   promote nothing?
4. Partial-failure discipline preserved: tmp artifacts kept on ANY failed
   copy (the QGWK7 rule: unlink only after every intended copy succeeded).
5. stdout.log promoted only when non-empty; `stdout.log.bytes` fully removed
   — check no stale readers of the byte sidecar remain (manager.rs, tests,
   conventions text).
6. Heartbeat/pane events excluded from counts; provider-runtime command
   starts counted as tool calls — sanity-check that classification.
7. Reasoning comes from `ProviderRuntimeEvent::ContentDelta` projection, no
   new `TextStream` variant — confirm no dead schema was added.

Also check: focused tests actually cover the claims (empty / work-bearing /
missing JSONL; payload exclusion; partial failure), and the convention text
(`shipped/prompt-studio/conventions/manager-dispatch.org`) matches the new
behavior.

Verdict: APPROVE or FINDINGS (numbered, each with file:line and why it's
wrong, severity-ordered). Do not edit code.
