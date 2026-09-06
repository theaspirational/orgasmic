# TASK-48SFW follow-up review — 4274d3e0..e15370f7

## Verdict

**APPROVE.** The four assigned items (F1, F2, F3, UTF-8 decoder) are really
fixed in the committed source, the red/green evidence is genuine and matches the
claims, and the scope is exactly one file. No HIGH findings. Three LOW/MEDIUM
findings below are follow-up material, not ship blockers.

Reviewed range: `4274d3e0443e2031a8367fcf56a007e56211f38f..e15370f746acd1d510d895a715409f13a208e536`
(worker commits `04bf2189`, `101f7e18`). Diff touches only
`crates/orgasmic-cli/src/daemon_service.rs` (+313/-47). Worktree HEAD confirmed
`e15370f7`, clean.

## Findings

### M1 — MEDIUM (correctness / legitimate-owner lockout risk)
`crates/orgasmic-cli/src/daemon_service.rs:1328` — `parse_macos_launchctl_owner_home`
now gates on the exact trimmed line `environment = {` and the exact closing `}`.
That is correct against this host's real output (verified below), but it is a
hard string coupling to `launchctl print`'s rendering. The pre-F2 code found
`ORGASMIC_HOME =>` on any line and was format-agnostic; the new parser is not.

Failure scenario: a macOS release renames or reflows that block header (or a
LaunchAgent is loaded from a plist that carries no `EnvironmentVariables`).
`homes` is then empty, `exactly_one_home` returns "missing",
`installed_macos_owner` returns `ServiceOwner::Unknown`, and
`refuse_shared_service_owner_mutation` refuses `daemon start`/`stop`/`restart`
**for the legitimate operator**. There is deliberately no force escape, so
recovery requires a code change. This is the same lockout shape F1 was opened to
remove, on a different axis.

Fix direction (not required for this merge): when no `environment` block is
found at all — as distinct from found-but-ambiguous — fall through to the disk
plist owner instead of returning `Unknown`. Ambiguity and mismatch must keep
refusing.

### L1 — LOW (correctness, missed brief requirement)
`crates/orgasmic-cli/src/daemon_service.rs:1394` — `systemd_unescape` handles an
**invalid** hex escape lossily rather than as an error. For `\xZZ` the hex branch
(`u8::from_str_radix`) fails, control falls through to the generic
backslash-escape branch, the `\` is dropped and `x` is kept, so
`ORGASMIC_HOME=/tmp/\xZZ` decodes to `/tmp/xZZ`. The fix brief asked that
"invalid decoding must remain an error/unknown, not a lossy matching owner."

Practical blast radius is small and points the safe way: the lossy value
normally *differs* from the requested home, so `ensure_service_owner_allows`
refuses rather than falsely allows, and our own writer escapes `\` → `\\` so
round-tripped units never hit this branch. Only a hand-edited unit reaches it.

Fix direction: in the `\x` branch, `bail!` when the two following bytes are not
valid hex, instead of falling through.

### L2 — LOW (security depth, ambiguity guard weaker on one path)
`crates/orgasmic-cli/src/daemon_service.rs:749` — the duplicate-owner guard in
`installed_macos_owner` is textual
(`raw.matches("<key>ORGASMIC_HOME</key>").count() > 1`), but on the new
`read_macos_plist_for_owner_scan` fallback the text it inspects is
plutil-normalized XML, where CoreFoundation has already collapsed duplicate dict
keys. So the ambiguity refusal cannot fire on that path. It is reachable only
when `read_to_string` fails, i.e. a binary plist (which structurally cannot hold
duplicate keys anyway) or a non-UTF-8 XML plist that still has duplicates —
contrived, and anyone who can write the LaunchAgent already owns the daemon.
Noted for completeness; the XML duplicate-key refusal on the normal path is
intact and covered by `duplicate_macos_home_keys_are_unknown_not_silently_chosen`.

## Open Questions

1. Should M1 degrade to the disk plist when the `environment` block is simply
   absent? That is a policy call about "no evidence" vs "conflicting evidence";
   the current code treats both as `Unknown`.
2. The systemd F1 trigger (`line == "LoadState=not-found"`) is asserted only
   against the test stub written by the same change. Nobody has confirmed it
   against real `systemctl --user show` output on this task. Should a Linux host
   capture be added the way `p1-readonly-launchctl-print.txt` was for macOS?

## Verification Notes

Read-only. No service, daemon, launchctl, systemctl or schtasks state was
touched. No test suite was rerun — the implementer's evidence was sufficient and
was checked rather than repeated.

Checked in source, not taken on claim:
- **F1** `installed_systemd_owner` (`daemon_service.rs:884-912`): on
  `LoadState=not-found` with the unit present it now reads the on-disk unit and
  parses the owner; absent unit still yields `Absent`; malformed/unreadable
  yields `Unknown`. `linux_systemd_unit_path` derives from `XDG_CONFIG_HOME`/
  `HOME` (`:866-882`), i.e. per-user, so it reads the real unit and not a staged
  home's copy — that is the property that makes the recovery correct.
  `render_linux_systemd_unit` (`:1194-1206`) writes `Environment="ORGASMIC_HOME=..."`,
  so the on-disk parse matches what we actually emit.
- **F2** verified against the real captured output in
  `p1-readonly-launchctl-print.txt`: that host prints `inherited environment = {`
  (containing unrelated secrets), `default environment = {` (PATH only), then
  `environment = {` with the single `ORGASMIC_HOME => /Users/aspirational/.orgasmic`.
  Trimmed exact-match on `environment = {` correctly selects the third block and
  skips the first two. Behaviour matches the fix's intent on the real format.
- **F3** `read_macos_plist_for_owner_scan` (`:749-774`) only reaches plutil when
  `read_to_string` fails, and bails to `Unknown` when plutil fails, so malformed
  input still refuses. I directly verified the conversion is **non-mutating** —
  the obvious HIGH risk here, since a "read" that rewrites the operator's plist
  would be the exact defect class this task exists to kill:
  `/usr/bin/plutil -convert binary1 t.plist` then
  `/usr/bin/plutil -convert xml1 -o - t.plist > /dev/null` on a throwaway temp
  file left `file`/`md5` identical (`46e5f849fc0a55ed6762383717d88f21` before and
  after). Input file untouched.
- **Decoder, single decode:** `parse_systemd_owner_home:1274-1281` now pushes
  `home.to_string()` after `take_systemd_token` already unescaped, so the value is
  decoded exactly once. Traced the literal round trip by hand:
  `/tmp/literal\x41` → `systemd_quote_arg` → `"ORGASMIC_HOME=/tmp/literal\\x41"`
  → quoted-token scan → `systemd_unescape` sees `\\` (not `\x`) → emits one `\`
  → `/tmp/literal\x41`. Matches the assertion in
  `systemd_owner_preserves_utf8_and_rejects_invalid_escapes`.
- **UTF-8:** `systemd_unescape` (`:1394-1418`) now accumulates `Vec<u8>` and
  validates once with `String::from_utf8`, so `\xc5\x81\xc3\xb3d\xc5\xba` →
  `Łódź` and a lone `\xff` errors. Checked the two `.expect()` calls for panic
  safety: `i` only ever advances by whole `char` widths or by 4 across an all-ASCII
  `\xNN`, and `0x5C` cannot appear as a UTF-8 continuation byte, so `i` and `i+1`
  are always char boundaries. No panic path found.
- **Unterminated quote** now `bail!`s (`:1300`) instead of silently accepting the
  remainder.
- **Worker prohibition intact:** `refuse_dispatch_lifecycle`
  (`daemon_lifecycle.rs:459`) is unchanged — the delta does not touch that file at
  all. The guard sits at the shared boundary: `daemon_service::start`/`stop`
  (`:225-263`) call `refuse_shared_service_owner_mutation` on all three
  persistent adapters, and `start_via_selected_adapter` is what `ensure_running`'s
  `Down` branch reaches (`daemon_lifecycle.rs:194-196`), so the original
  auto-start root cause is covered. No force escape and no installation rollback
  was added — confirmed by reading the whole diff.
- **Foreign/ambiguous still refuses:** `service_owner_guard_refuses_foreign_home_and_unknown_owner`
  is retained, and `systemd_not_found_uses_on_disk_unit_owner` itself asserts the
  foreign, ambiguous, unterminated-quote and non-UTF-8 unit cases all refuse
  before mutation.
- **Matching nondefault owner succeeds:** asserted in
  `loaded_macos_owner_ignores_inherited_environment_noise` (nondefault home with
  inherited/default noise present) and in the first half of
  `systemd_not_found_uses_on_disk_unit_owner`.

Evidence logs cross-checked (exit codes read from the `.exit` files):
- RED `p1-fix-behavior-red-daemon-service-tests.log` exit 1 — the four named
  failures are present verbatim with panic lines, and they are exactly the four
  behaviours this delta adds. A real red, not a compile error.
- GREEN `p1-fix-green-daemon-service-tests-2.log` exit 0, 0 failures;
  `p1-fix-green-lifecycle-focused-2.log` exit 0, 0 failures;
  `p1-fix-green-integration-worker-restart.log` exit 0 for
  `worker_restart_refuses_before_runtime_override_preparation`;
  `p1-fix-unterminated-systemd-tests.log` exit 0.
- `p1-fix-fmt-check.exit` 0, `p1-fix-diff-check.exit` 0.
- All runs went through `scripts/run-tests.sh` with the billed test skipped and
  the wrapper's flake registry clean. No piped invocations in this round.

Platform / coverage limits, stated rather than glossed:
- `loaded_macos_owner_ignores_inherited_environment_noise`,
  `binary_macos_plist_owner_is_allowed` and
  `launchctl_actual_environment_owner_must_be_single_nonempty` are
  `#[cfg(target_os = "macos")]`. On a Linux runner the entire F2 and F3 surface is
  compiled out and unverified.
- Every systemd assertion runs through the `ORGASMIC_TEST_SYSTEMD_OWNER_QUERY`
  stub. No real `systemctl` was invoked, so the F1 trigger string is verified
  against a hand-written fixture only (see Open Question 2). The macOS side is
  stronger — its format was checked against a real captured `launchctl print`.
- No live service was mutated, so the guard's behaviour under a genuinely loaded
  foreign LaunchAgent remains parse-level evidence.
- The wrapper reported `host: unknown (window too short to judge)` on the short
  focused runs; that is expected for 4-5 second suites and is not a masked
  failure.

## Fix Directions

1. M1: in `parse_macos_launchctl_owner_home`, distinguish "no `environment` block
   present" from "block present but ambiguous/empty", and let the first fall
   through to the disk plist owner in `installed_macos_owner`. Keeps every
   refusal that matters while removing a format-drift lockout with no escape
   hatch.
2. L1: `bail!` on a malformed `\xZZ` escape in `systemd_unescape` instead of
   falling through to the generic branch, matching the brief's "invalid decoding
   must remain an error, not a lossy matching owner".
3. Open Question 2: capture one real `systemctl --user show` output on a Linux
   host into the evidence directory, the way `p1-readonly-launchctl-print.txt`
   anchors the macOS parser, so the F1 trigger stops resting on a self-written
   stub.
4. Unrelated to this delta, noticed while reading: the doc comment on
   `test_adapter_override` (`daemon_service.rs:284-292`) still says "Only
   `detached` is accepted", but `macos`/`systemd`/`windows` arms exist under
   `cfg(test)`. Stale comment from the earlier reviewed range; no behaviour risk
   since the extra arms are test-gated.
