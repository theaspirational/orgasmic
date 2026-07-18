# TASK-R28CP fix round (TASK-QPKCD review)

## Changed
- **HIGH — early-exit orphan handling** (`crates/orgasmic-daemon/src/api.rs`): added `early-exit subprocess with no work envelopes` to `is_orphan_without_finalize_release_reason`, so the dispatch completion watcher emits exactly one `manager.dispatch_orphaned` tx and skips synthesized `last.txt`/`stdout.log` for that path (same as timeout / protocol-end-without-finalize).
- **MEDIUM — Failed tombstone recovery** (`crates/orgasmic-daemon/src/api.rs`): terminal `ReleaseOutcome::Failed` sessions now classify as `failed_recoverable` with read-only recovery actions (live-proven `reattach_tmux`, `resume_native_fork` when native metadata exists, always `start_recovery_run`). Original Failed tombstone stays immutable; `POST /runs/:id/recover` resolves `failed_recoverable_runs`. Exposed on `/api/recovery/status`, `/api/runs`, `GET /runs/:id`, and session path resolution.
- **Tests** (`crates/orgasmic-daemon/src/api.rs`, `crates/orgasmic-daemon/tests/dispatch_endpoint.rs`): hardened `dispatch_early_exit_auto_releases_stuck_lease` (orphan tx, single release, no artifacts, zero live runs, unchanged session count); added failed-recover POST regression, historical `phase: continuation` JSONL classification fixture, native `resume_native_fork` idle/no-ComposerSend regression; strengthened protocol-end orphan test with session-directory count.

## Verification Gates
- `cargo fmt --check` — pass
- Targeted orphan/recovery lib tests — pass (`early_exit_without_worker_finalize_flags_orphan_not_done`, `failed_terminal_release_is_recoverable_via_explicit_post`, `historical_continuation_lifecycle_deserializes_and_classifies_nonterminal`, `resume_native_fork_recovery_starts_idle_without_auto_prompt_send`)
- `cargo test -p orgasmic-daemon` — pass (full suite)
- `cargo test -p orgasmic-core` — pass
- `cargo test -p orgasmic-cli` — pass
- `cargo test -p orgasmic-drivers --lib` — `live_rmux_render_path_streams_screen_and_completes` failed (sentinel absent at `rmux.rs:2419`; environment-dependent, unchanged production render path per review); remaining 147 lib tests pass with that test skipped
- `git diff --check` — pass

## Unmet Criteria
- None stated in assignment acceptance block.

## Residual Risk
- Early-exit subprocess harnesses that reach `protocol_end_without_finalize` via stream-end synthesis share the orphan path but may not record the literal `early-exit subprocess with no work envelopes` reason; both reasons are now orphan-eligible.
- UI recovery surfaces still list `interrupted_runs` only in Run Dock; `failed_recoverable_runs` is available via API/CLI but not wired in UI (out of scope).
