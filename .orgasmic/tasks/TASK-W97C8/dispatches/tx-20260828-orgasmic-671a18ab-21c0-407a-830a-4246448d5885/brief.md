# Fix Brief: TASK-W97C8 — round 2, address review findings

Your round-1 implementation (commit bc4ee26d on `task-w97c8-impl`) was
reviewed. Verdict: FINDINGS — blocks ship. The full review is at
`.orgasmic/tmp/dispatch/task-w97c8/review-round-1.md` relative to the project
root — READ IT FIRST; it contains measured probes against the live ledger and
real session JSONLs, plus exact file:line for every finding.

You are in the same chain worktree; continue on `task-w97c8-impl`.

Fix, in this order (review's Fix Directions, condensed):

1. **F1 (HIGH)** `manager.rs:10531` — `session_path` reads `extra["TARGET"]`,
   but `parse_tx_file` lifts TARGET into the first-class `TxEntry.target`
   field and filters it out of `extra` (`tx.rs:713,718-731`). Result: always
   `None`, every production close writes zeroed evidence. Read the field:
   `run.and_then(|e| e.target.as_deref()).map(PathBuf::from)`. Add the test
   the review names: fold a real-shaped `run.created` tx and assert
   `session_path.is_some()`.
2. **F2 (HIGH)** `manager.rs:8244-8312` — strict `DriverEvent` typing aborts
   on real files (4/5 sampled abort by line 60) because the session writer
   elides oversized subtrees into `orgasmic_bounded` stubs
   (`session.rs:590-657`). Make parsing lossy-tolerant: skip-and-tally
   unparseable lines into `unparsed_events` + `bounded_events` fields in
   evidence.json (surfacing the elision), keep going, count a
   `provider_runtime` stub as one event. Fixture: copy a real
   `orgasmic_bounded` line (review names one) + a truncated final line.
3. **F4 (MED)** `manager.rs:8289-8294` — tool-call count whitelists 3
   literals, but the codex adapter puts the TOOL NAME in `item_type`
   (`codex.rs:642-662`): `exec`, `file_change`, etc. go uncounted (measured
   2-3x undercount). Count every `ItemStarted` minus a known non-tool set.
4. **F3 (MED)** — codex-chat sessions carry NO assistant/reasoning
   `content.delta` at all, so narrative is empty for codex runs. Decide and
   write it down: project codex agent messages from item lifecycle payloads,
   OR document (task journal + convention) that narrative is claude-only
   today. Do not leave it silently empty.
5. **F5 (MED)** `tests/shipped_conventions.rs:405-408,428-431` — gate test
   FAILS on your commit: the convention rewrite dropped the year-one growth
   sentence and the stdout.log.bytes prose the guards assert. Update both
   guards with the convention; replace the sidecar assertion with an
   evidence.json contract guard.
6. **F7 (LOW)** — after F1: recovery generations pair the addressed
   (replacement) run id with the initial run's file; replacement
   `run.created` has `target: None` (`api.rs:8364`) → silent zero. Take
   session_path from the addressed run when present, fall back to the
   initial run's target otherwise.
7. **F8 (LOW)** `paths.rs:433-438` — the empty-evidence guard is
   byte-length, unreachable by construction. Make the floor semantic:
   refuse when counts are 0 AND no failure reason (missing/unparsed) is
   named in the file.
8. **F9 (LOW)** — cap the narrative (pick a bound, mirror the
   STDOUT_PROMOTE_MAX_BYTES pattern) with a `narrative_truncated` flag, and
   state the evidence.json retention in the convention.

Also answer review Open Question 2 in the task journal: codex `System`
stream maps to "reasoning_text" (`codex.rs:626`) but carries harness
notices, not model thinking — exclude it from reasoning narrative or
justify keeping it.

Constraints unchanged: focused tests only; no daemon API changes; payload
exclusion (no ToolCall args / ToolResult outputs) must keep holding — the
review verified it holds, do not regress it.

Report: per-finding disposition (fixed/how or deferred/why), test names +
pass counts, and rerun `cargo test -p orgasmic-cli --test shipped_conventions`
green with the pinned toolchain (`rustup run 1.97.1 cargo ...` — plain cargo
on this machine is 1.94.1).
