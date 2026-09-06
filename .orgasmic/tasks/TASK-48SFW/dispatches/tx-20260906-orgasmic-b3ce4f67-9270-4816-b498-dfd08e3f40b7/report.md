# TASK-48SFW — final four-line confirmation (stdio/claude, opus, high)

## Verdict
APPROVE. The delta closes the exact prior LOW finding and disturbs no valid decoding.

## Findings
None at HIGH/MEDIUM/LOW in the reviewed range.

Informational only, explicitly out of the assigned scope and NOT a blocker:
`crates/orgasmic-daemon/src/supervisor.rs:14034` (`decode_js_escape`) has the
same `from_str_radix` leading-sign looseness. It is a spinner-glyph reader over
a bundle string the daemon itself ships, not an ownership/trust boundary, so it
carries no security consequence. Recorded so the class is not forgotten; no
action requested here.

## Verification Notes
Reviewed exactly `72004345..14561065` (`git diff`): two hunks, four added lines,
one file — `crates/orgasmic-cli/src/daemon_service.rs`.

1. Semantics of the prior finding, reproduced independently (standalone `rustc`
   probe, /tmp/hexprobe.rs, not repo source):
   - `u8::from_str_radix("+4", 16)` => `Ok(4)`. The prior LOW finding was real:
     before this commit `\x+4` decoded silently to byte `0x04`.
   - `"+4".bytes().all(u8::is_ascii_hexdigit)` => `false`. The new guard rejects it.
   - `"4a"` and `"4A"` both pass the guard and both parse to `0x4A`. Lower- and
     upper-case hex, i.e. every legitimate two-digit form, is unaffected.
   - `" 4"`, `"\t4"`, `"0x"`, `"-0"`, `"4\0"` all fail both the guard and
     `from_str_radix`; the guard adds no new rejection of anything that was
     previously accepted AND valid. No regression surface.
2. Bounds: `daemon_service.rs:1405` bails when `i + 3 >= bytes.len()`, so the
   guard's `bytes[i + 2..i + 4]` slice at :1407 is reached only when
   `i + 4 <= bytes.len()`. The added indexing cannot panic.
3. Non-ASCII: `is_ascii_hexdigit` is false for any byte >= 0x80, so a `\x`
   escape whose two following bytes straddle a UTF-8 char is now rejected before
   the `from_utf8` call rather than by it. Same outcome, earlier.
4. Test reaches production code: `parse_systemd_owner_home` (:1271) ->
   `take_systemd_token` (:1292, unquoted arm at :1308-1310) -> `systemd_unescape`,
   and the `Result` propagates. The new assertion at :2184
   (`ORGASMIC_HOME=/tmp/\x+4` is_err) therefore exercises the added branch on the
   real path, not a stub.
5. Manager evidence checked rather than assumed:
   `strict-hex-work/suite.log:6` — `test daemon_service::tests::
   systemd_owner_preserves_utf8_and_rejects_invalid_escapes ... ok`, and :8
   `test result: ok. 1 passed; 0 failed`. `strict-hex-test.log` verdict GREEN,
   0 failures, billed test skipped.
6. Formatting re-verified in this worktree: `cargo fmt -p orgasmic-cli -- --check`
   exited 0.
7. Fail-closed ownership policy untouched: the delta only adds refusals inside
   `systemd_unescape`; no policy, adapter, or lifecycle code is in the range.

No daemon start/stop/restart/serve, no `launchctl`, no `ORGASMIC_HOME` staging,
no service mutation, no builds, no billed tests, no writes to repo source.

## Open Questions
None.

## Fix Directions
None required. The two `.context("invalid systemd \\x escape")` calls at :1410
and :1412 are now unreachable-failure paths given the guard. That is harmless
defence in depth, not a defect; deleting them would be churn, so leave them.
