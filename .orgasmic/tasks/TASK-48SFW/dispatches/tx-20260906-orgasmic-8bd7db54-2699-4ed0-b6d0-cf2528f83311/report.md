# TASK-48SFW final validation delta review

Delta reviewed: `e15370f7..72004345` (worker commit `1fcf5351`), 1 file,
`crates/orgasmic-cli/src/daemon_service.rs`, +35/-7 including tests.

## Verdict

**APPROVE — does not block ship.** All three claims in the brief reproduce
against source and the cited logs. One LOW input-validation gap remains in the
same `\x` branch this delta hardened; it is a follow-up, not a blocker.

## Findings

**LOW `crates/orgasmic-cli/src/daemon_service.rs:1409` (correctness /
input-validation): a sign-prefixed `\x` escape still decodes instead of
erroring.** `u8::from_str_radix(hex, 16)` accepts a leading `+`, so
`ORGASMIC_HOME=/tmp/a\x+4` silently decodes to byte `0x04` rather than
returning "invalid systemd \x escape". The four new assertions cover `\x`,
`\x4`, `\xzz`, `\x4g` but not `\x+N`.

Evidence — `rustc`-compiled probe of the exact std call, `/tmp/radix_probe.rs`:

```
"+4" => Ok(4)      "+f" => Ok(15)
"4f" => Ok(79)     " 4" => Err(InvalidDigit)
"-0" => Err(InvalidDigit)   "4 " => Err(InvalidDigit)   "0x" => Err(InvalidDigit)
```

Reachability confirmed by reading, not inferred: `parse_systemd_owner_home:1282`
-> `take_systemd_token:1308-1311` passes the raw token through
`systemd_unescape` before any prefix stripping, so `+` reaches
`bytes[i + 2..i + 4]` intact.

Impact is bounded, which is why it is LOW: it requires an already-tampered unit
file, and both the disk and loaded readings go through the same parser, so the
divergence only matters where orgasmic's lenient decode happens to equal the
current home while systemd's own `cunescape` would have rejected the unit. The
realistic worst case is a wrong `ServiceOwner::Owned` verdict on a crafted unit.

Fix direction: reject before parsing —
`if !bytes[i + 2].is_ascii_hexdigit() || !bytes[i + 3].is_ascii_hexdigit() { bail!(...) }`
— and add `\x+4` to the existing assertion block at `:2175`.

No other findings. No HIGH, no MEDIUM.

## Verification Notes

Confirmed by source reading plus the cited logs; no suite was repeated, no
service mutated, no daemon lifecycle verb run, no `ORGASMIC_HOME` staged.

**Claim 1 — malformed/incomplete `\x` escapes now error. CONFIRMED (with the
`\x+N` gap above).** The rewritten branch at `:1403-1412` bails on
`i + 3 >= bytes.len()`, so `\x` (len 2) and `\x4` (len 3) bail and the
`[i + 2..i + 4]` slice is always in bounds when it is reached. Non-UTF-8 split
and non-hex both propagate as `Err` via `context`. The old code's
`if let Ok(..)` fallthrough — which silently re-processed a malformed escape as
a plain backslash — is gone.

**Claim 2 — valid escapes / UTF-8 / literal escaped backslashes still decode
once. CONFIRMED.** Test body at `:2156-2179`: raw `Łódź` round-trips; the
multi-byte form `\xc5\x81\xc3\xb3d\xc5\xba` accumulates into `out` and decodes
once at the single `String::from_utf8` at `:1425`; and `/tmp/literal\x41`
survives a full `render_linux_systemd_unit` -> `parse_systemd_owner_home`
round trip, because the non-`x` branch at `:1414-1418` consumes `\\` as one
literal backslash and leaves `x41` as text. `\xff` still errors at the final
UTF-8 decode.

**Claim 3 — bplist fallback accepts valid binary, refuses non-UTF-8 XML.
CONFIRMED.** `read_macos_plist_for_owner_scan:749-776` now reads bytes and
requires the `bplist00` magic before invoking `plutil -convert xml1`. The new
test at `:2035-2050` writes real UTF-16LE-with-BOM XML carrying duplicate
`ORGASMIC_HOME` keys and asserts the error, so `plutil` never gets to normalize
the duplicates away before the `raw.matches("<key>ORGASMIC_HOME</key>").count() > 1`
check at `:687`.

**Fail-closed is intact.** Every new `bail!` reaches
`installed_macos_owner:673-680`, which maps `Err` to `ServiceOwner::Unknown`,
and `ensure_service_owner_allows:631-634` refuses on `Unknown` with the
per-user-service message. No disk fallback and no force override appear in the
delta — `git diff --stat` shows one file and the diff touches only the two
helpers plus tests.

**Logs re-read, not trusted on assertion.** The `.exit` files and the temp
suite logs the wrapper names both still exist:

- RED `p1-validation-red-bin-systemd` exit 1, panic at `daemon_service.rs:2170`
  in `systemd_owner_preserves_utf8_and_rejects_invalid_escapes` — the new
  assertions genuinely failed before the fix.
- GREEN `p1-validation-green-bin-systemd` exit 0; `.../orgasmic-run-tests.B65gvk/suite.log`:
  `systemd_owner_preserves_utf8_and_rejects_invalid_escapes ... ok`,
  `1 passed; 0 failed`.
- GREEN `p1-validation-green-bin-macos-plist` exit 0; `.../orgasmic-run-tests.8pIBPE/suite.log`:
  `non_utf8_xml_macos_plist_is_not_normalized_for_owner_scan ... ok`,
  `macos_plist_owner_is_read_from_environment_home ... ok`,
  `binary_macos_plist_owner_is_allowed ... ok`, `3 passed; 0 failed`.

Not a finding, but worth naming so it is not mistaken for a red run: the wrapper
verdict line prints `(0 all green, 101 libtest reported failures)` in both green
logs. That is a hardcoded exit-code legend, `scripts/run-tests.sh:1535`, not a
failure count.

**One targeted probe, on scratch files only.** To characterise the residual
binary-plist gap I converted a duplicate-key XML plist through
`plutil -convert binary1` and back in `/tmp`. `plutil` collapses the duplicates
and keeps the last (`/tmp/two`), and `plutil -extract` agrees. No repo or
service file was touched.

## Open Questions

None that block. The `\x+N` gap is stated above as a finding rather than a
question because the behaviour is proven, not uncertain.

## Fix Directions

1. **The finding.** Two `is_ascii_hexdigit()` guards at `:1409` plus one
   `\x+4` assertion. One-line-scale, same fail-closed shape as the rest.
2. **Residual gap, deliberate, no action recommended.** The duplicate-key guard
   at `:687` is XML-only: a *binary* plist with duplicate `ORGASMIC_HOME` keys
   has them collapsed by `plutil` before the count check runs (measured above).
   This is safe as-is, because launchd reads the same file through the same
   CFPropertyList collapse and lands on the same value, so the disk reading
   matches what is actually loaded. Refusing binary plists outright was not
   authorized and would break the legitimate installed-service path.
3. **Residual gap, accepted.** The `bplist00` magic check also refuses
   legitimate non-UTF-8 plists in OpenStep/ASCII format. launchd-written plists
   are XML or binary, and the outcome is a clear refusal rather than a wrong
   start, so this is the correct trade.
4. **Test gap, unchanged by this delta.** The systemd assertions are pure parser
   unit tests and run on this macOS host; there is still no Linux-host execution
   of the systemd ownership path. Pre-existing, out of scope for this delta.
