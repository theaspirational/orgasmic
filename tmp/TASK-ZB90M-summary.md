# TASK-ZB90M — AFE5Q fix round

## Changed

- **Cursor workspace trust (HIGH):** tmux/rmux spawn a startup-only task when `cursor-agent` uses argv delivery (`paste_prompt=None`). Detects `Workspace Trust Required`, sends `a`, exits without re-pasting the prompt.
- **Lossless prompt bytes (HIGH):** `build_spawn_plan` in tmux/rmux trims only for emptiness; argv/paste deliver the original bundle byte-for-byte. Tests cover leading/trailing whitespace/newlines.
- **Non-finalized releases (MEDIUM):** completion watcher and `write_dispatch_completion_artifacts` never synthesize `last.txt`/`stdout.log` unless `finalized_by_worker=true`; all other releases flag `manager.dispatch_orphaned`. Manual `"run released"` regression added.
- **Warnings/dead code (MEDIUM):** removed capture-delta helpers, unused EOT test helper, restored `#[tokio::test]` on rmux `inert_send_input_returns_unsupported`, dropped unread `RmuxSpawnPlan::persistent`.
- **Obsolete prose (LOW):** removed capture-model wording from Claude tmux/rmux worker personas; fixed dangling fallback reference in `manager-dispatch.org`.

## Verification Gates

- `cargo fmt --check` — pass
- `cargo test -p orgasmic-drivers --lib` — 146 passed
- `cargo test -p orgasmic-daemon --lib` — 377 passed
- `cargo clippy -p orgasmic-drivers --lib -- -D warnings` — AFE5Q delta clean; 2 pre-existing `transcript_finder.rs` nonminimal-bool lints remain (unchanged by this task)
- `git diff --check` — pass
- Targeted: `prompt_bytes_preserved`, `accept_cursor_workspace_trust`, `manual_release_without_worker_finalize`, `dispatch_completion_artifacts_never_synthesized_without_finalize`

## Unmet Criteria

None stated in assignment.

## Residual Risk

- Cursor trust detection is pane-text heuristic (`Workspace Trust Required` / `[a] Trust this workspace`); Cursor UI copy changes would need detector updates.
- No end-to-end live `cursor-agent` fresh-worktree tmux/rmux probe in CI (mock-capture regressions only).
- Pre-existing `transcript_finder.rs` clippy lints still fail `-D warnings` for the full drivers crate.
