- Changed
  - `crates/orgasmic-cli/src/daemon_service.rs`
  - F1: `LoadState=not-found` systemd recovery now reads the on-disk unit owner; matching owner allowed, foreign/malformed/unreadable stays refused/unknown.
  - F2: macOS launchctl owner parsing now reads only the actual `environment = {}` block, ignoring inherited/default blocks.
  - F3: macOS plist owner scan can convert valid binary plists via `/usr/bin/plutil` while preserving malformed and duplicate-key refusal.
  - Systemd owner decoder now preserves UTF-8 bytes, avoids double-decoding already-tokenized homes, and refuses unterminated quoted tokens.
  - Existing daemon lifecycle dispatch refusal was not relaxed.
  - Commits: `04bf2189e3fa` and `101f7e185528` (HEAD).

- Verification Gates
  - Behavioral RED, after temporarily restoring only the prior reader bodies under safe test seams: `CARGO_BUILD_JOBS=2 scripts/run-tests.sh --work-dir /Users/aspirational/.codex/artifacts/orgasmic-recovery-20260906/p1-fix-behavior-red-work-2 -p orgasmic-cli --bin orgasmic daemon_service::tests` -> exit 1, 33 passed / 4 failed. Failures observed: `binary_macos_plist_owner_is_allowed`, `loaded_macos_owner_ignores_inherited_environment_noise`, `systemd_not_found_uses_on_disk_unit_owner`, `systemd_owner_preserves_utf8_and_rejects_invalid_escapes`. Log: `p1-fix-behavior-red-daemon-service-tests.log`.
  - GREEN after restoring final F1-F3/Unicode implementation: `CARGO_BUILD_JOBS=2 scripts/run-tests.sh --work-dir /Users/aspirational/.codex/artifacts/orgasmic-recovery-20260906/p1-fix-green-work-2 -p orgasmic-cli --bin orgasmic daemon_service::tests` -> exit 0, 39 passed. Log: `p1-fix-green-daemon-service-tests-2.log`.
  - GREEN lifecycle regression coverage unchanged: `CARGO_BUILD_JOBS=2 scripts/run-tests.sh --work-dir /Users/aspirational/.codex/artifacts/orgasmic-recovery-20260906/p1-fix-green-lifecycle-work-2 -p orgasmic-cli --bin orgasmic daemon_lifecycle::tests` -> exit 0, 28 passed. Log: `p1-fix-green-lifecycle-focused-2.log`.
  - GREEN integration refusal check: `CARGO_BUILD_JOBS=2 scripts/run-tests.sh --work-dir /Users/aspirational/.codex/artifacts/orgasmic-recovery-20260906/p1-fix-green-integration-work -p orgasmic-cli --test daemon_lifecycle worker_restart_refuses_before_runtime_override_preparation` -> exit 0, 1 passed. Log: `p1-fix-green-integration-worker-restart.log`.
  - Final parser follow-up GREEN: `CARGO_BUILD_JOBS=2 scripts/run-tests.sh --work-dir /Users/aspirational/.codex/artifacts/orgasmic-recovery-20260906/p1-fix-unterminated-systemd-work -p orgasmic-cli --bin orgasmic systemd_` -> exit 0, 6 passed. Log: `p1-fix-unterminated-systemd-tests.log`.
  - `cargo fmt --check` -> exit 0. Log: `p1-fix-fmt-check.log`.
  - `git diff --check` -> exit 0. Log: `p1-fix-diff-check.log`.
  - Not counted as evidence: earlier `p1-fix-red-daemon-service-tests.log` was missing-helper/partial-implementation red; earlier `p1-fix-green-lifecycle-focused.log` was Cargo filter misuse and ran no tests.

- Unmet Criteria
  - None for the scoped follow-up.

- Residual Risk
  - No live `launchctl`, `systemctl`, or `schtasks` services were mutated or restarted; coverage is parse/adapter-seam based.
  - Host state for short focused runs remained `unknown (window too short to judge)` in the wrapper output.
  - No broad crate/workspace suite, push, deploy, provider/quota probe, or P0 production FD recovery work was run by scope.
