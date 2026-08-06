#!/usr/bin/env bash
# orgasmic:TASK-DJ1WV
#
# Self-test for scripts/run-tests.sh — the classifier that decides whether red
# means anything.
#
# Every case drives the real script through `--classify`, against a fixture
# cargo log and a fixture registry, with fixture "test binaries" that are two
# line shell scripts. No cargo, no network, no money: the whole file runs in
# about a second, which is what makes it usable as the injection proof for
# TASK-DJ1WV.
#
# The one case that matters most is `registered_name_with_wrong_signature`. A
# classifier that excused a failure by NAME alone would be a machine that
# repeats the mistake this task was filed about: two failure modes, one test
# name, the wrong owning task blamed.

set -uo pipefail

REPO=$(git rev-parse --show-toplevel) || exit 3
RUNNER="$REPO/scripts/run-tests.sh"
TMP=$(mktemp -d "${TMPDIR:-/tmp}/run-tests-selftest.XXXXXX") || exit 3
trap 'rm -rf "$TMP"' EXIT

PASSED=0
FAILED=0
CASE=""

start() {
    CASE="$1"
}

# Assert on the run we just made. `want_exit` is the expected exit code; the
# remaining arguments are substrings that must all appear in the output.
check() {
    local want_exit="$1" got_exit="$2" out="$3"
    shift 3
    local bad=""
    [ "$got_exit" = "$want_exit" ] || bad="exit $got_exit, wanted $want_exit"
    local needle
    for needle in "$@"; do
        grep -qF -- "$needle" "$out" || bad="$bad; output lacks \`$needle\`"
    done
    if [ -z "$bad" ]; then
        printf 'ok   %s\n' "$CASE"
        PASSED=$((PASSED + 1))
    else
        printf 'FAIL %s: %s\n' "$CASE" "${bad# }"
        printf -- '---- output ----\n'
        cat "$out"
        printf -- '----------------\n'
        FAILED=$((FAILED + 1))
    fi
}

# A stand-in test binary. libtest's contract that matters here is tiny: run the
# named test, exit 0 if it passed. `$TMP/green` passes; `$TMP/red` does not.
cat > "$TMP/green" <<'EOF'
#!/bin/sh
echo "running 1 test"
echo "test $* ... ok"
exit 0
EOF
cat > "$TMP/red" <<'EOF'
#!/bin/sh
echo "still fails when it is the only test running"
exit 101
EOF
chmod +x "$TMP/green" "$TMP/red"

# A cargo test log with one failing test. $1 binary, $2 test name, $3 panic.
write_log() {
    cat > "$TMP/suite.log" <<EOF
   Compiling orgasmic-daemon v0.1.0
    Finished \`test\` profile [unoptimized + debuginfo] target(s) in 41.20s
     Running unittests src/lib.rs ($1)

running 3 tests
test tests::unrelated_a ... ok
test tests::unrelated_b ... ok
test $2 ... FAILED

failures:

---- $2 stdout ----

thread '$2' panicked at crates/orgasmic-daemon/src/api.rs:100:5:
$3
note: run with \`RUST_BACKTRACE=1\` environment variable to display a backtrace


failures:
    $2

test result: FAILED. 2 passed; 1 failed; 0 ignored; 0 measured; 0 filtered out; finished in 4.12s

error: test failed, to rerun pass \`-p orgasmic-daemon --lib\`
EOF
}

registry() {
    printf '%s\n' "$@" > "$TMP/registry.toml"
}

# Append a live-run host stamp to the synthetic suite log. --classify reads
# this instead of sampling at reclassify time (TASK-STWVB.1 / F3).
# Judgment is rate = syspolicyd_cpu / wall_s (TASK-STWVB.1.1). The live-path
# e2e case below exercises write_host_stamp directly; these helpers only
# feed --classify fixtures.
stamp_host() {
    local before_load="$1" before_time="$2" after_load="$3" after_time="$4"
    local delta_load="$5" delta_cpu="$6" wall_s="$7"
    printf '# orgasmic-host-state: before=load=%s syspolicyd_time=%s | after=load=%s syspolicyd_time=%s | delta=load=%s syspolicyd_cpu=%s wall_s=%s\n' \
        "$before_load" "$before_time" "$after_load" "$after_time" \
        "$delta_load" "$delta_cpu" "$wall_s" >> "$TMP/suite.log"
}

stamp_host_calm() {
    # rate = 5.0/100 = 0.05
    stamp_host 0.5 10:00.00 1.2 10:05.00 0.5 5.0 100
}

stamp_host_degraded_load() {
    # High BEFORE load is display-only (F-C); rate must clear the primary gate.
    # rate = 200/100 = 2.0
    stamp_host 11.41 10:00.00 22.0 10:05.00 11.41 200.0 100
}

stamp_host_degraded_syspolicy() {
    # Calm BEFORE load; high syspolicyd rate.
    # rate = 200/100 = 2.0
    stamp_host 0.5 10:00.00 1.0 12:00.00 0.5 200.0 100
}

stamp_host_load_only_high() {
    # High BEFORE load, ambient rate — must stay calm (F-C).
    stamp_host 11.41 10:00.00 22.0 10:05.00 11.41 5.0 100
}

# Default --classify cases leave host state to the stamp (or unknown). The
# ORGASMIC_HOST_STATE_SAMPLE injector is for the live path only and is ignored
# under --classify.
run() {
    "$RUNNER" --registry "$TMP/registry.toml" --work-dir "$TMP/work" "$@" \
        > "$TMP/out.txt" 2>&1
    RUN_EXIT=$?
    rm -rf "$TMP/work"
}

# The signature the seed entries use, and a second, different failure mode for
# the same test name. This pair is the 5HBST/STWVB mislabel in miniature.
LOAD_PANIC='assertion failed: waited for the atomic claim commit'
OTHER_PANIC='resume_native_fork recover: 500'

KNOWN_FLAKE_ENTRY=(
    '[[flake]]'
    'test = "tests::recovery_inventory_waits_for_atomic_claim_commit"'
    'owner = "TASK-STWVB"'
    'signature = "waited for the atomic claim commit"'
    'evidence = "fixture entry for the self-test"'
    'filed = "2026-07-28"'
)

# ---------------------------------------------------------------------------

start "registered signature that matches, green in isolation -> FLAKE, exit 0"
registry "${KNOWN_FLAKE_ENTRY[@]}"
write_log "$TMP/green" "tests::recovery_inventory_waits_for_atomic_claim_commit" "$LOAD_PANIC"
run --classify "$TMP/suite.log"
check 0 "$RUN_EXIT" "$TMP/out.txt" \
    "FLAKE (1)" \
    "owner    : TASK-STWVB" \
    "isolation: passed" \
    "verdict: GREEN modulo 1 registered flake"

start "registered name, WRONG signature -> REAL, exit 1 (the mislabel detector)"
registry "${KNOWN_FLAKE_ENTRY[@]}"
write_log "$TMP/green" "tests::recovery_inventory_waits_for_atomic_claim_commit" "$OTHER_PANIC"
run --classify "$TMP/suite.log"
check 1 "$RUN_EXIT" "$TMP/out.txt" \
    "REAL (1)" \
    "REGISTERED NAME, UNREGISTERED SIGNATURE" \
    "$OTHER_PANIC" \
    'excused  : "waited for the atomic claim commit" (owner TASK-STWVB)' \
    "verdict: RED"

start "two entries for one name -> the matching one wins and names its own owner"
registry "${KNOWN_FLAKE_ENTRY[@]}" \
    '[[flake]]' \
    'test = "tests::recovery_inventory_waits_for_atomic_claim_commit"' \
    'owner = "TASK-QCG6J"' \
    'signature = "resume_native_fork recover: 500"' \
    'evidence = "the second failure mode of the same test name"' \
    'filed = "2026-07-28"'
write_log "$TMP/green" "tests::recovery_inventory_waits_for_atomic_claim_commit" "$OTHER_PANIC"
run --classify "$TMP/suite.log"
check 0 "$RUN_EXIT" "$TMP/out.txt" \
    "FLAKE (1)" \
    "owner    : TASK-QCG6J" \
    'signature: "resume_native_fork recover: 500" — matched'

start "unregistered failure -> REAL, exit 1, even though it is green in isolation"
registry "${KNOWN_FLAKE_ENTRY[@]}"
write_log "$TMP/green" "tests::nobody_has_ever_seen_this_one" "some brand new panic"
run --classify "$TMP/suite.log"
check 1 "$RUN_EXIT" "$TMP/out.txt" \
    "REAL (1)" \
    "NOT IN THE REGISTRY" \
    "isolation: passed" \
    "verdict: RED"

start "registered and matching, but red in isolation too -> REAL, exit 1"
registry "${KNOWN_FLAKE_ENTRY[@]}"
write_log "$TMP/red" "tests::recovery_inventory_waits_for_atomic_claim_commit" "$LOAD_PANIC"
run --classify "$TMP/suite.log"
check 1 "$RUN_EXIT" "$TMP/out.txt" \
    "REAL (1)" \
    "FAILS ALONE TOO" \
    "isolation: FAILED (exit 101)"

start "a bare function name in the registry matches the module-qualified failure"
registry '[[flake]]' \
    'test = "recovery_inventory_waits_for_atomic_claim_commit"' \
    'owner = "TASK-STWVB"' \
    'signature = "waited for the atomic claim commit"' \
    'evidence = "fixture entry for the self-test"' \
    'filed = "2026-07-28"'
write_log "$TMP/green" "api::tests::recovery_inventory_waits_for_atomic_claim_commit" "$LOAD_PANIC"
run --classify "$TMP/suite.log"
check 0 "$RUN_EXIT" "$TMP/out.txt" "FLAKE (1)"

start "clean log -> GREEN, exit 0"
registry "${KNOWN_FLAKE_ENTRY[@]}"
cat > "$TMP/suite.log" <<EOF
     Running unittests src/lib.rs ($TMP/green)

running 2 tests
test tests::unrelated_a ... ok
test tests::unrelated_b ... ok

test result: ok. 2 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 1.00s
EOF
run --classify "$TMP/suite.log"
check 0 "$RUN_EXIT" "$TMP/out.txt" "verdict: GREEN — no failures" "failures : 0"

start "a compile error is REAL — nothing ran, so nothing can be excused"
registry "${KNOWN_FLAKE_ENTRY[@]}"
cat > "$TMP/suite.log" <<'EOF'
   Compiling orgasmic-daemon v0.1.0
error[E0599]: no method named `classify` found for struct `Verdict`
error: could not compile `orgasmic-daemon` (lib test) due to 1 previous error
EOF
run --classify "$TMP/suite.log"
check 1 "$RUN_EXIT" "$TMP/out.txt" "the workspace did not build" "verdict: RED — build failure"

# -- registry hygiene -------------------------------------------------------

start "owner already done -> registry REJECTED, exit 2 (no graveyard entries)"
registry '[[flake]]' \
    'test = "tests::production_resume_native_fork_uses_pinned_claude_not_path_shim"' \
    'owner = "TASK-5HBST"' \
    'signature = "env race that TASK-5HBST already fixed"' \
    'evidence = "this entry outlived its defect"' \
    'filed = "2026-07-28"'
run --check
check 2 "$RUN_EXIT" "$TMP/out.txt" \
    "which is done" \
    "A registry that only grows is a graveyard" \
    "registry: REJECTED"

start "owner is not a task at all -> registry REJECTED, exit 2"
registry '[[flake]]' \
    'test = "tests::whatever"' \
    'owner = "TASK-ZZZZZ"' \
    'signature = "a signature long enough"' \
    'evidence = "typo in the owner id"' \
    'filed = "2026-07-28"'
run --check
check 2 "$RUN_EXIT" "$TMP/out.txt" "is not a task in" "registry: REJECTED"

start "missing key -> registry REJECTED, exit 2"
registry '[[flake]]' \
    'test = "tests::whatever"' \
    'owner = "TASK-STWVB"' \
    'evidence = "no signature, so it would excuse any failure of this name"' \
    'filed = "2026-07-28"'
run --check
check 2 "$RUN_EXIT" "$TMP/out.txt" "is missing: signature" "registry: REJECTED"

start "unknown key -> registry REJECTED, exit 2"
registry "${KNOWN_FLAKE_ENTRY[@]}" 'reason = "typo for evidence"'
run --check
check 2 "$RUN_EXIT" "$TMP/out.txt" "unknown key \`reason\`" "registry: REJECTED"

start "a healthy registry passes --check"
registry "${KNOWN_FLAKE_ENTRY[@]}"
run --check
check 0 "$RUN_EXIT" "$TMP/out.txt" "registry: OK" "every owner open"

# -- the billed test --------------------------------------------------------

BILLED="legacy_drivers_and_explicit_pairs_emit_equivalent_start_events"

start "naming the billed test is refused, exit 3"
registry "${KNOWN_FLAKE_ENTRY[@]}"
run -p orgasmic-drivers "$BILLED"
check 3 "$RUN_EXIT" "$TMP/out.txt" "bills real money"

start "a log showing the billed test ran is RED even with no failures"
registry "${KNOWN_FLAKE_ENTRY[@]}"
cat > "$TMP/suite.log" <<EOF
     Running tests/driver_modes.rs ($TMP/green)

running 1 test
test $BILLED ... ok

test result: ok. 1 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 90.0s
EOF
run --classify "$TMP/suite.log"
check 1 "$RUN_EXIT" "$TMP/out.txt" "RAN. THIS COSTS REAL MONEY." "the skip did not hold"

# -- what the environment withheld ------------------------------------------

# orgasmic:TASK-S2KM0
#
# The CI failure mode these three cases exist to make impossible: a lane runs on
# a host that cannot provide `claude`, acknowledges it with
# ORGASMIC_ALLOW_MISSING_TOOLS so the sentinel stops failing, and then reports a
# green verdict that never mentions the tests it declined to run. Every skip is
# defensible; a silent one is not.

start "acknowledged missing tooling is named in the verdict, with counts"
registry "${KNOWN_FLAKE_ENTRY[@]}"
cat > "$TMP/suite.log" <<EOF
     Running unittests src/lib.rs ($TMP/green)
warning: ORGASMIC_ALLOW_MISSING_TOOLS explicitly allows missing test tooling: claude (gates 8 tests), codex (gates 1 test); those gated tests did not run

running 1 test
test tests::unrelated_a ... ok

test result: ok. 1 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 1.00s
EOF
run --classify "$TMP/suite.log"
check 0 "$RUN_EXIT" "$TMP/out.txt" \
    "environ  : INCOMPLETE — 9 test(s) gated out by absent tooling" \
    "NOT RUN (9)" \
    "These tests did not pass. They did not execute:" \
    "claude           gates 8 test(s)" \
    "codex            gates 1 test(s)" \
    "verdict: GREEN"

# One tool, two binaries, two different gated counts. Deduping the pairs would
# report 8 and hide the ninth test; these really are nine tests that did not run.
start "the same tool gated in two binaries sums rather than dedupes"
registry "${KNOWN_FLAKE_ENTRY[@]}"
cat > "$TMP/suite.log" <<EOF
     Running unittests src/lib.rs ($TMP/green)
warning: ORGASMIC_ALLOW_MISSING_TOOLS explicitly allows missing test tooling: claude (gates 8 tests); those gated tests did not run

test result: ok. 1 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 1.00s
     Running tests/driver_modes.rs ($TMP/green)
warning: ORGASMIC_ALLOW_MISSING_TOOLS explicitly allows missing test tooling: claude (gates 1 test); those gated tests did not run

test result: ok. 1 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 1.00s
EOF
run --classify "$TMP/suite.log"
check 0 "$RUN_EXIT" "$TMP/out.txt" "NOT RUN (9)" "claude           gates 9 test(s)"

start "nothing waived -> the verdict says the environment was complete"
registry "${KNOWN_FLAKE_ENTRY[@]}"
cat > "$TMP/suite.log" <<EOF
     Running unittests src/lib.rs ($TMP/green)

running 1 test
test tests::unrelated_a ... ok

test result: ok. 1 passed; 0 failed; 2 ignored; 0 measured; 0 filtered out; finished in 1.00s
EOF
run --classify "$TMP/suite.log"
check 0 "$RUN_EXIT" "$TMP/out.txt" \
    "environ  : complete — no tool requirement was waived" \
    "ignored  : 2 test(s) carrying #[ignore]"

# -- host state and load-sensitivity (TASK-STWVB / TASK-STWVB.1) -------------

# Live sampler path (not --classify): snapshot prints ? for a missing signal,
# never a measured 0.0 or a blank load field. Assert on syspolicyd_time= (the
# field this snapshot emits) — not syspolicyd_cpu=, which cannot appear here (F-F).
start "live sampler path: snapshot uses ? for missing fields, never blank/0.0"
sample=$("$RUNNER" --sample-host)
printf '%s\n' "$sample" > "$TMP/sample.txt"
bad=""
printf '%s' "$sample" | grep -Eq '^load=' || bad="$bad; missing load="
printf '%s' "$sample" | grep -Eq 'syspolicyd_time=' || bad="$bad; missing syspolicyd_time="
printf '%s' "$sample" | grep -Eq 'load=($| )' && bad="$bad; blank load"
printf '%s' "$sample" | grep -Eq 'syspolicyd_time=0(\.0)?($| )' && bad="$bad; measured 0.0 syspolicyd_time"
# A miss must be `?`, not empty after the equals.
load_val=${sample#load=}; load_val=${load_val%% *}
time_val=${sample#*syspolicyd_time=}
[ -n "$load_val" ] || bad="$bad; empty load value"
[ -n "$time_val" ] || bad="$bad; empty syspolicyd_time value"
if [ -z "$bad" ]; then
    printf 'ok   %s\n' "$CASE"
    PASSED=$((PASSED + 1))
else
    printf 'FAIL %s: %s\n' "$CASE" "${bad#; }"
    printf -- '---- sample ----\n%s\n----------------\n' "$sample"
    FAILED=$((FAILED + 1))
fi

# --classify without a stamp: host unknown, LOAD-SENSITIVE unavailable.
start "--classify without host stamp -> host unknown, LOAD-SENSITIVE unavailable"
registry "${KNOWN_FLAKE_ENTRY[@]}"
write_log "$TMP/green" "tests::nobody_has_ever_seen_this_one" "some brand new panic"
run --classify "$TMP/suite.log"
check 1 "$RUN_EXIT" "$TMP/out.txt" \
    "REAL (1)" \
    "NOT IN THE REGISTRY" \
    "host     : unknown" \
    "LOAD-SENSITIVE unavailable" \
    "verdict: RED"

# The load-bearing interlock: on a calm host an unregistered isolation-green
# failure is still REAL. Removing that gate would let a thrashing-host excuse
# become a permanent one. The verify/TASK-STWVB injection proves this case.
start "calm host + unregistered isolation-green -> REAL, exit 1 (C interlock)"
registry "${KNOWN_FLAKE_ENTRY[@]}"
write_log "$TMP/green" "tests::nobody_has_ever_seen_this_one" "some brand new panic"
stamp_host_calm
run --classify "$TMP/suite.log"
check 1 "$RUN_EXIT" "$TMP/out.txt" \
    "REAL (1)" \
    "NOT IN THE REGISTRY" \
    "green alone on a calm host is still REAL until owned" \
    "host     : calm" \
    "verdict: RED"

start "degraded host + unregistered isolation-green -> LOAD-SENSITIVE, exit 4"
registry "${KNOWN_FLAKE_ENTRY[@]}"
write_log "$TMP/green" "tests::nobody_has_ever_seen_this_one" "some brand new panic"
stamp_host_degraded_load
run --classify "$TMP/suite.log"
check 4 "$RUN_EXIT" "$TMP/out.txt" \
    "LOAD-SENSITIVE (1)" \
    "host was degraded" \
    "host     : DEGRADED" \
    "verdict: INCONCLUSIVE — re-run when calm" \
    "load-sensitive failure"

start "degraded host via syspolicyd delta + isolation-green -> LOAD-SENSITIVE, exit 4"
registry "${KNOWN_FLAKE_ENTRY[@]}"
write_log "$TMP/green" "tests::nobody_has_ever_seen_this_one" "some brand new panic"
stamp_host_degraded_syspolicy
run --classify "$TMP/suite.log"
check 4 "$RUN_EXIT" "$TMP/out.txt" \
    "LOAD-SENSITIVE (1)" \
    "host     : DEGRADED" \
    "verdict: INCONCLUSIVE — re-run when calm"

start "degraded host + clean suite -> INCONCLUSIVE (green is not trusted)"
registry "${KNOWN_FLAKE_ENTRY[@]}"
cat > "$TMP/suite.log" <<EOF
     Running unittests src/lib.rs ($TMP/green)

running 2 tests
test tests::unrelated_a ... ok
test tests::unrelated_b ... ok

test result: ok. 2 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 1.00s
EOF
stamp_host_degraded_load
run --classify "$TMP/suite.log"
check 4 "$RUN_EXIT" "$TMP/out.txt" \
    "host     : DEGRADED" \
    "verdict: INCONCLUSIVE — re-run when calm" \
    "suite looked green; that green is not trusted"

start "degraded host + registered flake -> INCONCLUSIVE, not a trusted green"
registry "${KNOWN_FLAKE_ENTRY[@]}"
write_log "$TMP/green" "tests::recovery_inventory_waits_for_atomic_claim_commit" "$LOAD_PANIC"
stamp_host_degraded_syspolicy
run --classify "$TMP/suite.log"
check 4 "$RUN_EXIT" "$TMP/out.txt" \
    "FLAKE (1)" \
    "verdict: INCONCLUSIVE — re-run when calm" \
    "registered flakes; still not a trusted green"

# F4 overturn: alone-red keeps exit 1; host stamp is alongside, not instead.
# verify/TASK-STWVB.1 pins this case.
start "degraded host + fails alone too -> REAL exit 1, host stamp alongside"
registry "${KNOWN_FLAKE_ENTRY[@]}"
write_log "$TMP/red" "tests::nobody_has_ever_seen_this_one" "hard failure under load"
stamp_host_degraded_load
run --classify "$TMP/suite.log"
check 1 "$RUN_EXIT" "$TMP/out.txt" \
    "REAL (1)" \
    "NOT IN THE REGISTRY" \
    "isolation: FAILED (exit 101)" \
    "host     : DEGRADED" \
    "verdict: RED — 1 real failure(s). This red means something"

start "calm host + registered flake still GREEN modulo flake, exit 0"
registry "${KNOWN_FLAKE_ENTRY[@]}"
write_log "$TMP/green" "tests::recovery_inventory_waits_for_atomic_claim_commit" "$LOAD_PANIC"
stamp_host_calm
run --classify "$TMP/suite.log"
check 0 "$RUN_EXIT" "$TMP/out.txt" \
    "FLAKE (1)" \
    "host     : calm" \
    "verdict: GREEN modulo 1 registered flake"

# -- TASK-STWVB.1.1 follow-ups ----------------------------------------------

install_stub_cargo_ok() {
    mkdir -p "$TMP/bin"
    cat > "$TMP/bin/cargo" <<'EOF'
#!/bin/sh
cat <<'LOG'
     Running unittests src/lib.rs (stub)
running 1 test
test tests::x ... ok
test result: ok. 1 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.01s
LOG
exit 0
EOF
    chmod +x "$TMP/bin/cargo"
}

install_stub_cargo_abort() {
    # No per-test failure list — just a crashed binary (B2). The classifier
    # must name the non-zero cargo exit, not claim the suite looked green.
    mkdir -p "$TMP/bin"
    cat > "$TMP/bin/cargo" <<'EOF'
#!/bin/sh
cat <<'LOG'
     Running unittests src/lib.rs (stub)
error: test failed, to rerun pass `-p orgasmic-daemon --lib`

Caused by:
  process didn't exit successfully: `target/debug/deps/stub` (signal: 6, SIGABRT: process abort signal)
LOG
exit 101
EOF
    chmod +x "$TMP/bin/cargo"
}

# F-C: high BEFORE load alone must not trip the gate.
start "high BEFORE load alone stays calm (load is corroborating only)"
registry "${KNOWN_FLAKE_ENTRY[@]}"
cat > "$TMP/suite.log" <<EOF
     Running unittests src/lib.rs ($TMP/green)

running 2 tests
test tests::unrelated_a ... ok
test tests::unrelated_b ... ok

test result: ok. 2 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 1.00s
EOF
stamp_host_load_only_high
run --classify "$TMP/suite.log"
check 0 "$RUN_EXIT" "$TMP/out.txt" \
    "host     : calm" \
    "verdict: GREEN — no failures"

# F-E: measured-nothing is unknown, not calm.
start "all-unknown judgment prints host unknown, not calm"
registry "${KNOWN_FLAKE_ENTRY[@]}"
cat > "$TMP/suite.log" <<EOF
     Running unittests src/lib.rs ($TMP/green)

running 1 test
test tests::unrelated_a ... ok

test result: ok. 1 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 1.00s
EOF
stamp_host '?' '?' '?' '?' '?' '?' '?'
run --classify "$TMP/suite.log"
check 0 "$RUN_EXIT" "$TMP/out.txt" \
    "host     : unknown (no numeric host signal" \
    "verdict: GREEN — no failures"

start "unparseable judgment prints host unknown, not calm"
registry "${KNOWN_FLAKE_ENTRY[@]}"
cat > "$TMP/suite.log" <<EOF
     Running unittests src/lib.rs ($TMP/green)

running 1 test
test tests::unrelated_a ... ok

test result: ok. 1 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 1.00s
EOF
stamp_host abc abc abc abc abc xyz qqq
run --classify "$TMP/suite.log"
check 0 "$RUN_EXIT" "$TMP/out.txt" \
    "host     : unknown (no numeric host signal" \
    "verdict: GREEN — no failures"

# B2: crashed binary is never "suite looked green", even on a degraded host.
start "crashed cargo on degraded host -> RED names exit, never looked green"
registry "${KNOWN_FLAKE_ENTRY[@]}"
install_stub_cargo_abort
PATH="$TMP/bin:$PATH" \
    ORGASMIC_HOST_STATE_SAMPLE='load=11.41,syspolicyd_cpu=250.0,wall_s=100' \
    "$RUNNER" --registry "$TMP/registry.toml" --work-dir "$TMP/work" \
    > "$TMP/out.txt" 2>&1
RUN_EXIT=$?
rm -rf "$TMP/work"
bad=""
[ "$RUN_EXIT" = 1 ] || bad="exit $RUN_EXIT, wanted 1"
grep -qF 'host     : DEGRADED' "$TMP/out.txt" || bad="$bad; output lacks \`host     : DEGRADED\`"
grep -qF 'verdict: RED — cargo exited 101 with no per-test failure list' "$TMP/out.txt" \
    || bad="$bad; output lacks cargo-exit RED"
grep -qF 'suite looked green' "$TMP/out.txt" && bad="$bad; misreported suite looked green"
if [ -z "$bad" ]; then
    printf 'ok   %s\n' "$CASE"
    PASSED=$((PASSED + 1))
else
    printf 'FAIL %s: %s\n' "$CASE" "${bad#; }"
    printf -- '---- output ----\n'
    cat "$TMP/out.txt"
    printf -- '----------------\n'
    FAILED=$((FAILED + 1))
fi

# F-D: live sampler + write_host_stamp + --classify round-trip on the log
# that same run wrote. Format is not hand-copied here. Host word may be
# calm or DEGRADED depending on ambient syspolicyd burn during the second
# the stub cargo runs; what is pinned is the stamp contract.
start "live path stamps host state; --classify reads that stamp"
registry "${KNOWN_FLAKE_ENTRY[@]}"
install_stub_cargo_ok
PATH="$TMP/bin:$PATH" \
    "$RUNNER" --registry "$TMP/registry.toml" --work-dir "$TMP/work" \
    > "$TMP/live-out.txt" 2>&1
live_log="$TMP/work/suite.log"
bad=""
[ -f "$live_log" ] || bad="$bad; live run wrote no suite.log"
if [ -z "$bad" ]; then
    grep -qE "^# orgasmic-host-state:" "$live_log" \
        || bad="$bad; live run wrote no host stamp"
    grep -qE 'wall_s=[0-9]' "$live_log" \
        || bad="$bad; live stamp lacks wall_s from writer"
fi
if [ -z "$bad" ]; then
    cp "$live_log" "$TMP/suite.log"
    # Capture the delta= judgment the writer emitted.
    live_delta=$(grep -E "^# orgasmic-host-state:" "$live_log" | tail -n1 \
        | sed -n 's/.*| delta=//p')
    rm -rf "$TMP/work"
    run --classify "$TMP/suite.log"
    grep -qF "delta   $live_delta" "$TMP/out.txt" \
        || bad="$bad; reclassify delta does not match live stamp"
    grep -qF 'unknown (reclassify with no host stamp' "$TMP/out.txt" \
        && bad="$bad; reclassify ignored the live stamp"
    grep -qE 'wall_s=[0-9]' "$TMP/out.txt" \
        || bad="$bad; reclassify output lacks wall_s"
fi
if [ -z "$bad" ]; then
    printf 'ok   %s\n' "$CASE"
    PASSED=$((PASSED + 1))
else
    printf 'FAIL %s: %s\n' "$CASE" "${bad#; }"
    printf -- '---- live ----\n'
    cat "$TMP/live-out.txt" 2>/dev/null || true
    printf -- '---- classify ----\n'
    cat "$TMP/out.txt" 2>/dev/null || true
    printf -- '----------------\n'
    FAILED=$((FAILED + 1))
fi

# orgasmic:TASK-STWVB.1.1
# B1: ambient accrual over a long window crosses an absolute CPU-seconds
# bound while remaining ambient as a rate (~0.075 s/s). Judging on a rate
# keeps the default gate from returning exit 4 on a calm host.
# verify/TASK-STWVB.1.1 pins this case as the FIRST failure under injection.
start "long ambient syspolicyd accrual is calm as a rate (absolute would trip)"
registry "${KNOWN_FLAKE_ENTRY[@]}"
install_stub_cargo_ok
PATH="$TMP/bin:$PATH" ORGASMIC_HOST_STATE_SAMPLE='load=4.0,syspolicyd_cpu=105.0,wall_s=1400' \
    "$RUNNER" --registry "$TMP/registry.toml" --work-dir "$TMP/work" \
    > "$TMP/out.txt" 2>&1
RUN_EXIT=$?
rm -rf "$TMP/work"
check 0 "$RUN_EXIT" "$TMP/out.txt" \
    "host     : calm" \
    "verdict: GREEN — no failures"

# ---------------------------------------------------------------------------

printf '\n%s passed, %s failed\n' "$PASSED" "$FAILED"
[ "$FAILED" -eq 0 ] || exit 1
