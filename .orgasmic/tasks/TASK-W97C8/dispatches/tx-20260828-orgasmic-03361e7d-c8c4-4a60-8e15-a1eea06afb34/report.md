## Changed
- `crates/orgasmic-cli/src/manager.rs`: close now reads the exact session JSONL `TARGET` from `run.created`, streams the matching run's typed events, writes event/tool-call counts, session filename/size, transcript-finder result, and ordered assistant/reasoning narrative without retaining tool arguments or results. Provider-runtime command starts count as tool calls; heartbeat/pane activity do not count as substantive evidence.
- `crates/orgasmic-core/src/paths.rs`: promotion requires non-empty `evidence.json`, keeps tmp artifacts on any partial failure, promotes `stdout.log` only when non-empty, and no longer writes `stdout.log.bytes`.
- `crates/orgasmic-drivers/src/transcript_finder.rs`: `codex-chat` uses the existing Codex native transcript adapter.
- `shipped/prompt-studio/conventions/manager-dispatch.org`: retention and recovery text now describes typed evidence and optional stdout crash excerpts.
- Reasoning decision recorded in TASK-W97C8 Worklog via tx `tx-20260828-orgasmic-6491`: project typed `ProviderRuntimeEvent::ContentDelta` reasoning/thinking instead of adding an unused `TextStream::Reasoning` schema variant.

## Verification Gates
- `cargo test -p orgasmic-cli --bin orgasmic dispatch_evidence_` — 3 passed, 0 failed (`/tmp/TASK-W97C8-cli-5.log`). Covers empty, work-bearing, and missing session JSONL; native path/confidence; assistant/reasoning order; and tool payload exclusion.
- `cargo test -p orgasmic-cli --bin orgasmic manager::tests::dispatch_close_clean_worktree_has_no_salvage_side_effects -- --exact` — 1 passed, 0 failed (`/tmp/TASK-W97C8-close-2.log`). Production close path wrote and committed `evidence.json`, retained non-empty stdout only, and emitted no byte sidecar.
- `cargo test -p orgasmic-core --lib promote_` — 4 passed, 0 failed (`/tmp/TASK-W97C8-core.log`). Includes empty/bounded stdout and evidence partial-failure tmp retention.
- `cargo test -p orgasmic-drivers --lib codex_chat_uses_the_codex_transcript_adapter` — 1 passed, 0 failed (`/tmp/TASK-W97C8-drivers.log`).
- `cargo test -p orgasmic-drivers --lib modes::tmux::tests::required_test_tooling_is_present -- --exact` — 1 passed, 0 failed (`/tmp/TASK-W97C8-driver-sentinel.log`).
- `git diff --check` — passed.
- Two implementation-time focused-test reds were regressions and were fixed before the final green run: a test compared JSON directly to `PathBuf` (`/tmp/TASK-W97C8-cli-2.log`), then its expected macOS temp path lacked canonical `/private` (`/tmp/TASK-W97C8-cli-3.log`).

## Unmet Criteria
- None.

## Residual Risk
- Per brief, verification stayed focused; no whole-crate or workspace suite/clippy run was performed.
