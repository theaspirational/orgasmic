## Changed

- **F-1 (HIGH):** Missing `brief.md`, a missing `BRIEF_PATH`, or missing attempt-scoped `compiled-prompt.md` now yields an optional validated sidecar. Close still removes the worktree, promotes/commits the remaining record, and reports `brief.md missing from tmp` or `compiled-prompt.md missing from tmp` through `CLEANUP_ERROR`. Existing-but-unsafe sidecars still hard-error.
- **F-2 (MED):** `dispatch_compiled_prompt_path` now suffix-replaces the selected attempt's `-last.txt` filename with `-compiled-prompt.md`; the daemon writer and close reader continue sharing that helper. `attempt_scoped_paths_isolate_consecutive_dispatch_bundles` proves two attempts in one stem retain distinct bundle bytes. Journal note: `tx-20260828-orgasmic-6501`; no brief-basename uniqueness rule added.
- **F-3 (MED):** Brief validation now requires the filename to derive the selected stem, compiled-prompt validation requires the exact derived filename, and both retain the existing `O_NOFOLLOW` handle discipline. `validate_dispatch_record_rejects_wrong_or_symlinked_brief_sidecar` covers a sibling `last.txt` and symlink.
- **F-4 (LOW):** Rollback validation now retains the attempt-scoped compiled-prompt handle and unlinks it only through the validated stem-directory handle. Both the core prune test and daemon-timeout integration test assert it is removed without touching sibling attempts.
- **F-5 (LOW):** The close integration test now asserts one `git log --oneline -- <record_dir>` line instead of four `cat-file -e` probes.
- Round-2 files: `crates/orgasmic-core/src/paths.rs`, `crates/orgasmic-cli/src/manager.rs`, `crates/orgasmic-cli/tests/dispatch.rs`.

## Verification Gates

Pinned toolchain: `rustup run 1.97.1`.

- `cargo test -p orgasmic-core --lib paths::` — **15 passed, 0 failed**. Log: `/tmp/TASK-W97C8.1-final-core-paths-20260828-130808.log`.
- `cargo test -p orgasmic-cli --bins dispatch_close` — **13 passed, 0 failed** (includes both missing-sidecar close tests). Log: `/tmp/TASK-W97C8.1-final-dispatch-close-20260828-130819.log`.
- `cargo test -p orgasmic-cli --bins dispatch_evidence` — **5 passed, 0 failed**. Log: `/tmp/TASK-W97C8.1-final-dispatch-evidence-20260828-130829.log`.
- `cargo test -p orgasmic-cli --bins attempt_scoped_paths_isolate_consecutive_dispatch_bundles` — **1 passed, 0 failed**. Log: `/tmp/TASK-W97C8.1-final-attempt-bundles-20260828-130840.log`.
- `cargo test -p orgasmic-cli --test dispatch dispatch_close_promotes_complete_record_only_at_close` — **1 passed, 0 failed**. Log: `/tmp/TASK-W97C8.1-final-integration-close-20260828-130854.log`.
- `cargo test -p orgasmic-cli --test dispatch dispatch_timeout_requests_daemon_cleanup` — **1 passed, 0 failed**. Log: `/tmp/TASK-W97C8.1-final-integration-timeout-20260828-130904.log`.
- `cargo test -p orgasmic-cli --test dispatch required_test_tooling_is_present` — **1 passed, 0 failed**; no tooling waiver. Log: `/tmp/TASK-W97C8.1-final-tooling-sentinel-20260828-130959.log`.
- `cargo test -p orgasmic-cli --test shipped_conventions` — **5 passed, 0 failed**. Log: `/tmp/TASK-W97C8.1-final-shipped-conventions-20260828-130913.log`.
- `git diff --check` — clean.

## Unmet Criteria

None.

## Residual Risk

Focused gates only, as required by the round-2 brief; the full workspace suite was not run.
