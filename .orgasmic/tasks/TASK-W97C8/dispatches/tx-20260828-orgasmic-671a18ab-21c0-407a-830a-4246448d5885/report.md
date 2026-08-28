## Changed

- **F1 fixed:** `dispatch_record_from_fold` reads first-class `TxEntry.target`, not filtered `extra["TARGET"]`. `dispatch_fold_reads_run_created_target_field` folds a real-shaped `run.created` tx and proves `session_path` plus its evidence run id survive.
- **F2 fixed:** evidence parsing now skips and tallies malformed/untypeable lines as `unparsed_events`, tallies `orgasmic_bounded` lines as `bounded_events`, continues after both, and counts an untypeable `provider_runtime` stub as one event. The regression fixture copies the review's real bounded line and appends a truncated final line.
- **F3 fixed by the brief's documentation option:** task Worklog tx `tx-20260828-orgasmic-6494` and `manager-dispatch.org` state that narrative is Claude-only today; codex-chat counts/pointers remain proof of work. Codex `reasoning_text` is excluded because `TextStream::System` carries harness notices, not model thinking.
- **F4 fixed:** every provider `item.started` counts except the known non-tool item types (`agent_message` / `agentMessage` / `reasoning`), so `exec`, `file_change`, dynamic, and MCP tool names count.
- **F5 fixed:** the convention restores `last.txt` year-one growth, documents the evidence contract, and replaces the deleted sidecar guard with `evidence.json` / lossy-count / truncation guards.
- **F6 fixed:** production-fold and lossy-parser regressions now exercise the two paths round-1 tests bypassed; payload-exclusion assertions remain green.
- **F7 fixed:** evidence now carries a run id paired with its session path. It prefers the addressed recovery run's target and id; when recovery has no target, it falls back to both the initial path and initial run id, avoiding the prior replacement-id/original-file zero.
- **F8 fixed:** promotion rejects valid JSON whose event/tool counts are both zero unless the file names missing/unreadable session evidence or tallies unparsed events.
- **F9 fixed:** narrative text is capped at 64 KiB on a UTF-8 boundary with `narrative_truncated`; the convention states permanent retention and the cap.
- Round-1 `codex-chat -> codex` transcript-finder normalization is intentionally retained: it is required to populate native transcript evidence for the codex-chat driver mode, not a drive-by daemon API change.

Changed source files in round 2:
- `crates/orgasmic-cli/src/manager.rs`
- `crates/orgasmic-core/src/paths.rs`
- `crates/orgasmic-cli/tests/shipped_conventions.rs`
- `shipped/prompt-studio/conventions/manager-dispatch.org`

No daemon API, dependency, lockfile, glossary, or decision file changes.

## Verification Gates

Pinned toolchain: `rustup run 1.97.1`.

- Pre-probe: `cargo test -p orgasmic-cli --test shipped_conventions` reproduced **4 passed / 1 failed** at `/tmp/TASK-W97C8-r2-pre-shipped-conventions.log`.
- `cargo test -p orgasmic-cli --bin orgasmic -- dispatch_evidence` — **5 passed** (`/tmp/TASK-W97C8-r2-final-cli-evidence.log`). Covers typed counts/pointers/narrative, payload exclusion, bounded+truncated lossy parsing, and UTF-8 narrative cap.
- `cargo test -p orgasmic-cli --bin orgasmic -- dispatch_fold_` — **2 passed** (`/tmp/TASK-W97C8-r2-final-cli-fold.log`). Covers first-class TARGET plus recovery addressed/fallback path+run-id pairing.
- `cargo test -p orgasmic-cli --bin orgasmic -- codex_system_notices_are_not_reasoning_evidence` — **1 passed** (`/tmp/TASK-W97C8-r2-final-cli-system.log`).
- `cargo test -p orgasmic-cli --bin orgasmic -- dispatch_close_clean_worktree_has_no_salvage_side_effects` — **1 passed** (`/tmp/TASK-W97C8-r2-final-cli-close.log`). Closed-record production path writes nonzero evidence and omits `stdout.log.bytes`.
- `cargo test -p orgasmic-core --lib paths::` — **14 passed** (`/tmp/TASK-W97C8-r2-final-core-paths.log`). Includes semantic evidence floor, partial-failure discipline, empty-stdout omission, and bounded non-empty stdout promotion.
- `cargo test -p orgasmic-cli --test shipped_conventions` — **5 passed** (`/tmp/TASK-W97C8-r2-final-conventions.log`).
- `cargo test -p orgasmic-drivers --lib -- codex_chat_uses_the_codex_transcript_adapter` — **1 passed** (`/tmp/TASK-W97C8-r2-final-transcript.log`).
- Drivers tooling sentinel — **1 passed** with worker-safe `ORGASMIC_ALLOW_MISSING_TOOLS=tmux` invocation (`/tmp/TASK-W97C8-r2-final-drivers-sentinel.log`).
- Production-shaped, count-only probe over the two live W97C8 JSONLs (`/tmp/TASK-W97C8-r2-live-session-probe.log`): codex implementer `events=436 tools=262 bounded=1 narrative_bytes=0`; Claude reviewer `events=775 tools=71 bounded=5 narrative_bytes=141`. No tool arguments/results were printed or retained by the probe.
- `git diff --check` — green.
- `cargo fmt --all -- --check` remains red only at the eight pre-existing regions already identified in round-1 review; `/tmp/TASK-W97C8-r2-fmt-check-3.log` contains no diff in round-2 lines.

## Unmet Criteria

None.

## Residual Risk

- Per the brief's focused-tests-only constraint, no full crate/workspace suite was run.
- Codex-chat narrative remains empty until that adapter emits actual assistant/reasoning deltas; this limitation is now explicit in both task journal and shipped convention, while counts/pointers remain non-empty proof of work.
- Repository-wide format check still has unrelated pre-existing diffs; round-2 touched lines are formatter-clean.
