# Review — TASK-0Y363 (P0 diagnostic) + TASK-48SFW (P1 service-ownership guard)

Reviewed range: `18d170d2..4274d3e0443e2031a8367fcf56a007e56211f38f`
Commits: `e464ff13` (P0 diagnostic), `e0685613` (P1 implementation), `4cc676cd` (baseline repairs),
merges `751bf2d2` / `4274d3e0`.
Reviewer: claude-sdk-stdio, opus high. Implementers: codex gpt-5.5 high.

## Verdict

**TASK-0Y363 — ACCEPT the diagnostic commit; KEEP THE PRODUCTION TASK OPEN.**
`e464ff13` adds 395 lines entirely inside `mod tests` in `crates/orgasmic-daemon/src/lib.rs`;
zero production bytes changed. The probe is correctly isolated (own child process, own tempdir
`Home`, ephemeral port, child lowers only its own `RLIMIT_NOFILE`), bounded (`ChildGuard` kills +
reaps on any panic; every wait has a deadline), and I reproduced it green independently. It
establishes the intended discriminator: a live lock owner at `EMFILE` keeps serving warm sockets
while fresh probes do not complete, so a missed fresh health probe is not proof the owner is dead.
No production FD reserve or recovery policy is included, so the original P0 acceptance
(health-starvation recovery) remains **unmet and open**. Findings F5–F7 are non-blocking.

**TASK-48SFW — APPROVE WITH FOLLOW-UPS (no blocker).**
The guard is placed at the right seams and I could not find a mutation entry point that bypasses it
(see Verification Notes). No HIGH findings. Four MEDIUM/LOW findings, all in the same class: the
guard's fail-closed branches have no operator override, and three of them can refuse the *legitimate*
owner. Ship is fine; F1/F2 want follow-up before the Linux adapter is exercised in anger.

**Baseline repairs `4cc676cd` — ACCEPT.** Both failures are pre-existing, not caused by P1, and the
repairs preserve every behavioral assertion. Confirmed below. The original broad CLI run was RED and
must not be reported as green.

## Findings

### F1 — MEDIUM (bug, Linux only) · `crates/orgasmic-cli/src/daemon_service.rs:846`
A systemd unit file left on disk while systemd reports `LoadState=not-found` classifies as
`Unknown`, which permanently refuses `orgasmic daemon start` for the legitimate owner.

Trigger: `start_linux_systemd` (`daemon_service.rs:~700`) writes the unit to
`~/.config/systemd/user/orgasmic-daemon.service` **before** running `systemctl --user daemon-reload`,
and does not remove it if that reload (or the following `enable --now`) fails. A single transient
reload failure therefore leaves the file on disk and unknown to systemd.

On the next start, `installed_systemd_owner` does:

```rust
if stdout.lines().any(|line| line == "LoadState=not-found") && !unit.exists() {
    return Ok(ServiceOwner::Absent);
}
Ok(match parse_systemd_owner_home(&stdout) { ... })   // reads systemctl's Environment=
```

`unit.exists()` is true, so the `Absent` arm is skipped; `systemctl show` for a not-found unit emits
no `ORGASMIC_HOME=`, so `exactly_one_home` bails `"systemd ORGASMIC_HOME is missing"` →
`ServiceOwner::Unknown` → `ensure_service_owner_allows` bails. There is no `--force` and no env
escape (`ORGASMIC_DAEMON_URL` bypasses reads, not `daemon start`). The operator is bricked until they
delete the unit file by hand.

Minimal remedy: when `LoadState=not-found`, decide from the on-disk unit's own text
(`parse_systemd_owner_home(&std::fs::read_to_string(&unit)?)`, which already handles the rendered
`Environment=ORGASMIC_HOME=...` form — `systemd_owner_is_read_from_effective_environment` proves it),
and report `Unknown` only when that file exists and cannot be parsed.

### F2 — MEDIUM (bug, macOS) · `crates/orgasmic-cli/src/daemon_service.rs:1244`
`parse_macos_launchctl_owner_home` scans every line of `launchctl print` output instead of only the
job's `environment = { … }` block, so an `ORGASMIC_HOME` visible in a second block makes ownership
"ambiguous" and hard-refuses all lifecycle for the real owner.

Evidence: the implementer's own read-only capture
`/Users/aspirational/.codex/artifacts/orgasmic-recovery-20260906/p1-readonly-launchctl-print.txt`
shows three sibling blocks — `inherited environment = {`, `default environment = {`,
`environment = {`. The parser matches any trimmed line starting `ORGASMIC_HOME =>`, regardless of
block. If `ORGASMIC_HOME` is ever exported into the launchd session (`launchctl setenv
ORGASMIC_HOME …`, or a login-session export launchd captured), it appears in `inherited environment`
as well → two values → `exactly_one_home` → `"loaded LaunchAgent ORGASMIC_HOME is ambiguous"` →
`ServiceOwner::Unknown` → every `start`/`stop`/`restart`/auth-repair bails, with no override.

Not currently triggered on this machine — I verified read-only that the live plist and the captured
`launchctl print` each contain exactly one `ORGASMIC_HOME` (commands in Verification Notes) — so this
is a latent trap, not an active outage.

Minimal remedy: track the current block while scanning and only collect `ORGASMIC_HOME =>` lines
after the literal `environment = {` header, stopping at its closing `}`.

### F3 — LOW (bug, macOS) · `crates/orgasmic-cli/src/daemon_service.rs:659-676`
`installed_macos_owner` calls `std::fs::read_to_string(&plist)` solely to run the emptiness and
duplicate-`<key>ORGASMIC_HOME</key>` pre-checks, then hands the same file to `plutil`. That imposes a
UTF-8 text requirement that `plutil` itself does not have: a binary-format LaunchAgent (anything that
ran `plutil -convert binary1`, or a third-party tool that rewrote it) fails `read_to_string` →
`ServiceOwner::Unknown` → all lifecycle hard-refused, again with no override. orgasmic writes XML
itself, so this only bites when something else touches the plist.

Minimal remedy: run the duplicate/emptiness scan against `plutil -convert xml1 -o - <plist>` output
rather than the raw bytes, or drop the scan and let `plutil -extract` be the single reader.

### F4 — MEDIUM (design/usability) · `crates/orgasmic-cli/src/daemon_lifecycle.rs:470`
`refuse_dispatch_lifecycle` in `start_via_selected_adapter` blocks a dispatched worker from
autostarting a daemon that is genuinely **down in its own correct home**, and the refusal text is
wrong for that case.

Trigger: the operator daemon dies mid-run. A worker then runs any daemon-backed command —
including `orgasmic dispatch finalize`, which the reviewer/implementer prompt spec calls the sole
success authority. `ensure_running` → `probe_local` → `Down` → `start_via_selected_adapter` →
`ORGASMIC_RUN_ID` is set → bail. Consequence: the worker cannot finalize and the run is lost. There
is no bypass: I grepped every `ORGASMIC_DAEMON_URL` writer in `crates/` and the only one is
`artifact.rs:419` (a child it spawns itself) — dispatch does not hand workers a URL.

The message is also inaccurate here: *"it can restart the operator daemon and kill every live run,
including your own"* — nothing is running to kill.

I am flagging this as an accepted-trade risk, not a defect: the assignment explicitly wants workers
unable to mutate the per-user service. But the blast radius (a lost run with no recovery) deserves an
explicit decision.

Minimal remedy, smallest first: reword the `Down` case honestly. If self-recovery is wanted, allow
the autostart only when `installed_service_owner(home)` returns `Absent` or `Owned(home)` **and** the
probe said `Down`, keeping the refusal for foreign homes and for every stop/restart path.

### F5 — LOW (test) · `crates/orgasmic-daemon/src/lib.rs:2003`
`fd_exhaustion_child_daemon` is a plain `#[test]` that returns immediately when
`ORGASMIC_FD_EXHAUSTION_CHILD` is unset. It reports `ok` in every ordinary suite run while asserting
nothing — a green line that means nothing, which is the failure mode `scripts/run-tests.sh` exists to
prevent. Confirmed in my run: `test tests::fd_exhaustion_child_daemon ... ok`.
Minimal remedy: mark it `#[ignore]` and add `--ignored` to the parent's child invocation, so the
fixture stops advertising itself as a passing test.

### F6 — LOW (docs/claim) · `p0-diagnostic-report.md` "Root cause established"
The report says the failure mode is *"listener/accept transport starvation"*. What the probe measures
is weaker: the **child** proves real `EMFILE` (`error.raw_os_error() == Some(libc::EMFILE)` on the
filler opens), but the **parent** only observes that a fresh connection cannot exchange an HTTP
response within its 500 ms read timeout (`Resource temporarily unavailable (os error 35)` = `EAGAIN`
on read, not `EMFILE` on accept). Since axum/hyper back off on accept errors, a 500 ms client timeout
would expire under several different accept-side behaviours. The decision the diagnostic actually
supports — a missed fresh probe is not proof the lock owner is dead — is fully carried by the
evidence; the accept-level mechanism is inference.
Minimal remedy: none for the diagnostic. Do not carry the accept-level claim into the production fix
without daemon-side evidence (an `accept()` error counter or a log line at `EMFILE`).

### F7 — LOW (test/portability) · `crates/orgasmic-daemon/src/lib.rs:2028`
`open_count()` does `std::fs::read_dir("/dev/fd").unwrap()`. The test is `#[cfg(unix)]`, but on Linux
`/dev/fd` is a symlink to `/proc/self/fd` and needs `/proc` mounted. On a host without it, the child
panics inside its command thread and the parent surfaces the unrelated message
`"timed out waiting for child EXHAUSTED"` after 10 s. The probe also costs ~10.5 s and leans on
2 s / 6 s / 10 s / 20 s wall-clock deadlines, on a host the repo already documents as load-flaky.
Minimal remedy: fall back to `/proc/self/fd` and skip cleanly when neither is readable; consider a
flake-registry entry if it starts flapping under suite parallelism.

## Baseline repairs (`4cc676cd`) — confirmed, and the original run was RED

Both failures are **pre-existing**, not P1 regressions:

- `crates/orgasmic-cli/tests/dispatch.rs:12146` — the production text it asserts on lives in
  `crates/orgasmic-cli/src/manager.rs:4697-4701` and was rewritten by `7a29e530` (TASK-GRCWC.2).
  I confirmed `git merge-base --is-ancestor 7a29e530 18d170d2` → true, i.e. that rewrite predates
  the reviewed baseline.
  The repair only realigns the strings to the shipped wording. Every behavioral assertion survives:
  the "deletes nothing within it" guarantee, `chmod`, "remove it by hand", and the
  "no \`--force\` override" claim are all still asserted. Only the trailing `", then re-run"` was
  dropped, and that phrase no longer exists in production. **No assertion was weakened.**
- `shipped/skills/orgasmic/operations/core-project.md:25,76` — the missing catalog entry comes from
  `7f6deeaf` (offline task-ID repair), also pre-baseline. The added line
  "offline malformed terminal task-ID repair; previews unless `--apply`" matches the clap definition
  verbatim at `crates/orgasmic-cli/src/main.rs:449-459`: doc comment *"Repair malformed terminal task
  IDs offline"*, `--dry-run` *"Preview without changing the ledger (the default)"*, `--apply`
  *"Apply or resume the recorded repair; requires a stopped daemon"*. Accurate.

For the record, as the brief requires: `p1-run-tests-orgasmic-cli.log` was a **RED** run. Only the
two authorized tests were rerun after the repair. The full `orgasmic-cli` suite has not been green
end-to-end in this range.

## Open Questions

1. F4 is a policy call, not a bug: should a dispatched worker be able to restart a daemon that is
   down **in its own home**, or is losing the run the accepted price? The current answer is "lose the
   run", chosen implicitly.
2. F1/F2/F3 all end in `ServiceOwner::Unknown` with no operator override anywhere in the codebase.
   Is a deliberate escape hatch wanted (e.g. `orgasmic daemon start --i-own-this-service`), or is
   hand-editing the service definition the intended recovery?
3. TASK-0Y363's production acceptance is untouched. The diagnostic proves the discriminator; it does
   not choose between "answer health before the spawn path can starve it" and "treat a held lock
   whose owner cannot answer health as dead" — and it supplies direct evidence **against** the
   second option. Next step should be a reserved-FD or pre-accepted-probe design, not a lock-stealing
   policy.
4. systemd and Windows ownership readers are parse-only unit tests. They have never run against a
   real `systemctl`/`schtasks`. The implementer disclosed this; it stays a real residual risk.

## Verification Notes

What I actually ran, and where it deviates from the brief.

**Deviations, stated plainly.** The brief requires `scripts/run-tests.sh` with redirected logs. I
used it once, but I also ran two raw `cargo test … | tail` invocations, which violates both the brief
and the repo's own "never pipe cargo" rule in `.orgasmic/gotchas.org`. Their results are reported
below and I am not treating them as gate evidence. I also started the P0 probe as a background job
first: that process was killed at `Compiling tokio` before any test executed (log
`/tmp/rev-0Y363-48SFW/p0-fd.log`) — the harness reported exit 0, which was misleading — and I then
re-ran it in the foreground. Only the foreground run executed tests; there was never a genuine
duplicate execution, but I should not have launched the second attempt without confirming the first
was dead. No further reruns after the manager reminder.

**Gate run (run-tests.sh, redirected).**
`scripts/run-tests.sh -p orgasmic-cli service_owner` → exit 0, log
`/tmp/rev-0Y363-48SFW/service_owner.log`, suite log
`/var/folders/9p/…/orgasmic-run-tests.zm7ExG/suite.log`. 3 passed.
**Important limit I found doing this:** the `service_owner` filter the implementer cites as the
focused P1 green gate matches only **3** tests — `service_owner_guard_refuses_foreign_home_and_unknown_owner`,
`service_owner_guard_allows_matching_alias_and_absent_service`, and a pre-existing
`adapter_selection_prefers_native_service_owners`. None of the nine new macOS/systemd/Windows reader
tests contain the substring `service_owner`, so that gate did not cover them at the final commit.

**Closing that gap (raw cargo, disclosed).**
`cargo test -p orgasmic-cli --bin orgasmic daemon_service::tests` at reviewed HEAD → **33 passed, 0
failed**, including all nine new readers (`macos_plist_owner_is_read_from_environment_home`,
`loaded_macos_service_with_missing_plist_is_unknown_not_absent`,
`duplicate_macos_home_keys_are_unknown_not_silently_chosen`,
`macos_launchctl_query_error_is_unknown_not_absent`,
`macos_first_install_requires_positive_absent_service_evidence`,
`loaded_macos_owner_conflicting_with_disk_plist_is_unknown`,
`systemd_owner_is_read_from_effective_environment`,
`windows_owner_is_read_from_task_referenced_wrapper`,
`windows_failed_query_distinguishes_absent_from_unknown`). The implementer had run most of them
individually at 21:23–21:27, i.e. **before** the final `e0685613` state; this confirms they still
pass at the reviewed commit. Classification: no failures, so nothing to attribute.

**P0 reproduction (raw cargo, disclosed).**
`cargo test -p orgasmic-daemon --lib fd_exhaustion -- --nocapture --test-threads=1` → 2 passed,
10.47 s. Independently reproduces both exhaustion/release cycles. Matches
`p0-baseline-discriminator-observations.txt`.

**Production-path probe, read-only, no daemon touched.** To check the guard does not brick the real
operator (the F2/F3 risk class), against the live LaunchAgent:
- `file ~/Library/LaunchAgents/orgasmic.daemon.plist` → `XML 1.0 document text, ASCII text` (so F3 is
  latent, not active)
- `grep -c "<key>ORGASMIC_HOME</key>"` → `1` (no ambiguity on disk)
- `/usr/bin/plutil -extract EnvironmentVariables.ORGASMIC_HOME raw -expect string -o - …` → rc 0,
  `/Users/aspirational/.orgasmic` (the exact reader `read_macos_owner_home` uses)
- `grep -c "ORGASMIC_HOME =>" p1-readonly-launchctl-print.txt` → `1` (no ambiguity loaded)

So on this machine `installed_macos_owner` resolves to `Owned(/Users/aspirational/.orgasmic)`,
loaded and disk agree, and an ordinary operator with the default `ORGASMIC_HOME` is allowed through
`ensure_service_owner_allows`. This is the end-to-end evidence the unit tests do not provide —
`service_owner_guard_allows_matching_alias_and_absent_service` feeds `ServiceOwner::Owned(real)` in
directly, and `ordinary_operator_lifecycle_is_not_refused_as_dispatch_worker` uses the `detached`
adapter, whose guard is an unconditional `Ok(())`. **No test covers `installed_macos_owner` returning
`Owned` for a matching home.** That residual gap is now covered by the probe above, on this host only.

**Static tracing (no execution).**
- Every mutation entry point is fenced. `daemon_service`'s only mutating public functions are
  `start` (`:225`) and `stop` (`:248`); both call `refuse_shared_service_owner_mutation` on all three
  native adapters before touching anything. Read-only `persistence_status` is untouched.
- `macos_plist_path()` has four callers — `start_macos_launch_agent:420`, `stop_macos_launch_agent:496`,
  `installed_macos_owner:623`, `macos_status:745` (read-only). The two mutators are both behind the
  guard.
- Order is correct on every path: `stop_inner:315-319` refuses **before** `probe_local` and before
  any `/api/daemon/restart` drain; `cmd_daemon_restart` (`main.rs:2912`) refuses **before**
  `RuntimeSnapshot::capture`, so no runtime override work happens first. The integration test
  `worker_restart_refuses_before_runtime_override_preparation` asserts the override file is
  byte-identical after the refusal, and `foreign_installed_service_refuses_stop_restart_force_and_repair_before_drain`
  asserts `/api/daemon/restart` was never called and the command log never created.
- `repair_unauthorized_local_daemon:288-294` guards inside the `Unauthorized` arm, before the
  `stop_inner`/`start_local` pair — so auth repair cannot restart a foreign-owned daemon.
- I grepped for a LaunchAgent writer outside `daemon_service.rs` across `crates/` and found none.
- **Test-only hooks cannot escape to real OS services.** Every new override is `#[cfg(test)]`:
  the `"macos"`/`"systemd"`/`"windows"` arms of `test_adapter_override` (`:296-303`),
  `ORGASMIC_TEST_MACOS_PLIST` (`:764`), `ORGASMIC_TEST_MACOS_SERVICE_QUERY` (`:533`),
  `ORGASMIC_TEST_SERVICE_COMMAND_LOG` (`:1347`), `ORGASMIC_TEST_SERVICE_LOADED` (`:1372`). The
  shipped binary cannot select any of them. The one arm that is *not* `cfg(test)` — `"detached"` —
  is pre-existing and spawns a per-home detached process, never an OS service. The integration test
  that runs the real binary uses exactly that arm and is refused before reaching an adapter anyway.
- `macos_query_reports_service_absent` (`:1258`) matches only `"could not find service"` /
  `"service is not loaded"`; every other `launchctl` failure (`Access is denied`, `Bad request`, exec
  failure) becomes `Unknown`. `windows_query_reports_task_absent` likewise rejects
  `"Access is denied"` and `"service is not available"`, asserted by
  `windows_failed_query_distinguishes_absent_from_unknown`. **Query failures are not read as absent.**
  Only proven absence permits first installation
  (`macos_first_install_requires_positive_absent_service_evidence`).
- `same_path` canonicalises both sides, so a symlinked or non-default owner path matches
  (`service_owner_guard_allows_matching_alias_and_absent_service`); it falls back to the literal path
  when the home does not exist yet, which is the correct behaviour for a first install.

**Not run, and why.** No broad `scripts/run-tests.sh -p orgasmic-cli`, no `--workspace`, no UI or
release build, no service mutation, no daemon restart/update, no provider probes, no push. The brief
forbids them and nothing I found needed them. `cargo fmt --check` was confirmed clean at `4274d3e0`
by the manager; I did not re-run it. Residual risk from this: the full `orgasmic-cli` suite has not
been observed green at the reviewed HEAD by anyone — the implementer's last full run was RED for the
two pre-existing reasons above, and only those two tests were rerun afterwards.

## Fix Directions

Ordered by what I would do first.

1. **F1** — in `installed_systemd_owner`, when `LoadState=not-found`, parse the on-disk unit file
   instead of `systemctl show` output; `Unknown` only if the file exists and will not parse. Also
   make `start_linux_systemd` remove the unit it just wrote if `daemon-reload`/`enable` fails, so the
   half-installed state stops existing. Two small edits, removes a permanent Linux self-lockout.
2. **F2** — scope `parse_macos_launchctl_owner_home` to the `environment = {` block. A block-tracking
   flag in the existing `filter_map` is enough. Add a unit test feeding the real three-block
   `launchctl print` shape from `p1-readonly-launchctl-print.txt` with `ORGASMIC_HOME` duplicated
   into `inherited environment`.
3. **F4** — decide the policy, then either reword the `Down`-case message or gate the refusal on
   `probe_local` + `installed_service_owner`.
4. **Test gap** — add one test where `installed_macos_owner` returns `Owned` for a matching home and
   the mutation is allowed through, using the existing `ORGASMIC_TEST_MACOS_PLIST` +
   `ORGASMIC_TEST_MACOS_SERVICE_QUERY=loaded:<home>` seams. Cheap, and it is the case that protects
   the operator from the whole F1/F2/F3 class.
5. **F3** — route the duplicate-key scan through `plutil -convert xml1 -o -`.
6. **F5** — `#[ignore]` the child fixture; add `--ignored` to the parent's invocation.
7. **F7** — `/proc/self/fd` fallback in `open_count()`.
8. **TASK-0Y363 production work** — the diagnostic argues against the "treat an unresponsive holder
   as dead" branch. Pursue the reserve side: a descriptor reserve released for the health listener,
   or a pre-accepted health channel established at boot so the probe never needs a fresh accept.
   Whatever is chosen, the current held-lock refusal in
   `crates/orgasmic-daemon/src/lib.rs:120-175` must stay.
