## Changed
- `crates/orgasmic-cli/src/daemon_service.rs`
  - `systemd_unescape` now refuses `\x` escapes unless exactly two following bytes are valid hex digits; valid hex still accumulates bytes for one UTF-8 decode and doubled backslashes still stay literal through unit rendering.
  - `read_macos_plist_for_owner_scan` only falls back through `plutil -convert xml1` when the unreadable file starts with binary-plist magic `bplist00`; non-UTF-8 XML now errors before normalization can collapse duplicate keys.
  - Added focused assertions for malformed systemd hex and non-UTF-8 XML plist refusal; preserved the existing binary plist owner test.

## Verification Gates
- RED observed before fix: `CARGO_BUILD_JOBS=2 CARGO_TARGET_DIR=$PWD/target scripts/run-tests.sh -p orgasmic-cli --bin orgasmic systemd_owner_preserves_utf8_and_rejects_invalid_escapes` -> exit 1, log `/Users/aspirational/.codex/artifacts/orgasmic-recovery-20260906/p1-validation-red-bin-systemd.log`.
- GREEN after fix: same systemd filter -> exit 0, log `/Users/aspirational/.codex/artifacts/orgasmic-recovery-20260906/p1-validation-green-bin-systemd.log`.
- GREEN macOS plist focused filter: `CARGO_BUILD_JOBS=2 CARGO_TARGET_DIR=$PWD/target scripts/run-tests.sh -p orgasmic-cli --bin orgasmic macos_plist` -> exit 0, log `/Users/aspirational/.codex/artifacts/orgasmic-recovery-20260906/p1-validation-green-bin-macos-plist.log`; confirmed `non_utf8_xml_macos_plist_is_not_normalized_for_owner_scan`, `binary_macos_plist_owner_is_allowed`, and `macos_plist_owner_is_read_from_environment_home` ran ok.
- `cargo fmt --check` -> exit 0.
- `git diff --check` -> exit 0.

## Unmet Criteria
- None for the requested final validation fix.

## Residual Risk
- No broad suite by dispatch instruction.
- Wrapper reported owner lifecycle registry not checked because this retained worktree has no repo-local `.orgasmic/tasks`; test results still used the requested focused `scripts/run-tests.sh` path.
- Initial `--lib` probe was invalid for these tests and was replaced with the requested `--bin orgasmic` red/green evidence.
