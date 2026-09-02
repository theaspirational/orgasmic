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

# orgasmic:TASK-STWVB.1.1.1.1
# R-5: the companion `--classify`-agreement cases used to sit behind
# `if [ -f "$live_log" ]`, so a live run that wrote no log deleted the case
# instead of failing it, and nothing noticed. Two tripwires now: the helper
# below FAILS the started case when the log is missing, and the run asserts
# the total case count so a case cannot go missing any other way either.
#
# orgasmic:TASK-STWVB.1.1.1.1.1
# F-6: DERIVED, not declared. A constant ~1000 lines from the cases it counts
# has to be edited to add one, and its failure message ("a case was added or
# vanished") reads as an invitation to bump the number. Counting the `start`
# calls in this file instead means the expectation moves with the cases and
# only a case that STOPS RUNNING can trip it.
EXPECTED_CASES=$(grep -c '^start "' "${BASH_SOURCE[0]}")
live_log=""

start() {
    CASE="$1"
}

fail_case() {
    printf 'FAIL %s: %s\n' "$CASE" "$1"
    FAILED=$((FAILED + 1))
}

# Hand the log the previous live run wrote to the next --classify case. Fails
# the started case (never skips it) when the live run produced no log.
take_live_log() {
    if [ ! -f "$live_log" ]; then
        rm -rf "$TMP/work"
        fail_case "the live run wrote no suite.log at $live_log"
        return 1
    fi
    cp "$live_log" "$TMP/suite.log" || {
        rm -rf "$TMP/work"
        fail_case "cannot copy $live_log"
        return 1
    }
    rm -rf "$TMP/work"
    return 0
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
# A fixture verify/ so the real repo's artifact state cannot colour these cases.
mkdir -p "$TMP/verify"

run() {
    "$RUNNER" --registry "$TMP/registry.toml" --verify-dir "$TMP/verify" \
        --work-dir "$TMP/work" "$@" > "$TMP/out.txt" 2>&1
    RUN_EXIT=$?
    rm -rf "$TMP/work"
}

# The signature the seed entries use, and a second, different failure mode for
# the same test name. This pair is the 5HBST/STWVB mislabel in miniature.
LOAD_PANIC='assertion failed: waited for the atomic claim commit'
OTHER_PANIC='resume_native_fork recover: 500'

# orgasmic:TASK-STWVB.1.1.1.1
# The healthy-registry fixtures need an owner `check_owner_lifecycle` accepts:
# a task that exists and is neither done nor cancelled. A hardcoded id rots —
# this fixture named TASK-STWVB until that task closed, at which point every
# case using it failed `registry: REJECTED` / exit 2 and the whole gate went
# red for a reason that has nothing to do with the classifier under test.
# Resolve an open one at startup instead, so the self-test measures the
# classifier and not the task board.
open_owner() {
    # Mirror run-tests.sh `orgasmic_tasks_dir`: the 2026-08-27 ledger cutover
    # moved the committed task nodes to ~/.orgasmic/ledgers/<project>.
    local tasks="$REPO/.orgasmic/tasks" f id
    if [ ! -d "$tasks" ] && [ -d "$HOME/.orgasmic/ledgers/$(basename "$REPO")/.orgasmic/tasks" ]; then
        tasks="$HOME/.orgasmic/ledgers/$(basename "$REPO")/.orgasmic/tasks"
    fi
    for f in "$tasks"/*/node.org; do
        [ -f "$f" ] || continue
        grep -Eq '^\*+[ \t]+(DONE|CANCELLED)[ \t]+' "$f" && continue
        while read -r id; do
            [ -n "$id" ] || continue
            printf '%s\n' "$id"
            return 0
        done <<EOF
$(awk 'match($0, /^[ \t]*:ID:[ \t]+TASK-[A-Z0-9.]+[ \t]*$/) {
        id = $2
        print id
    }' "$f")
EOF
    done
    return 1
}

FIXTURE_OWNER=$(open_owner) || {
    printf 'FAIL setup: no open task in %s (or the ledger checkout) to own the fixture registry entries\n' \
        "$REPO/.orgasmic/tasks"
    exit 1
}

KNOWN_FLAKE_ENTRY=(
    '[[flake]]'
    'test = "tests::recovery_inventory_waits_for_atomic_claim_commit"'
    "owner = \"$FIXTURE_OWNER\""
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
    "owner    : $FIXTURE_OWNER" \
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
    "excused  : \"waited for the atomic claim commit\" (owner $FIXTURE_OWNER)" \
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
    "owner = \"$FIXTURE_OWNER\"" \
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
    "owner = \"$FIXTURE_OWNER\"" \
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
check 0 "$RUN_EXIT" "$TMP/out.txt" "registry: OK" "every owner open" "artifacts: 0/0 replayable"

# orgasmic:TASK-8Q92K
start "a verify artifact whose patch no longer applies fails --check, exit 2"
registry "${KNOWN_FLAKE_ENTRY[@]}"
mkdir -p "$TMP/verify/TASK-STALE"
printf 'diff --git a/nope.txt b/nope.txt\n--- a/nope.txt\n+++ b/nope.txt\n@@ -1 +1 @@\n-never\n+there\n' \
    > "$TMP/verify/TASK-STALE/injection.patch"
run --check
rm -rf "$TMP/verify/TASK-STALE"
check 2 "$RUN_EXIT" "$TMP/out.txt" "TASK-STALE STALE (error:" "artifacts: 0/1 replayable" "artifacts: REJECTED"

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
# Reject both a bare decimal phantom zero and the realistic cumulative-ps
# shape `0:00.00` (F2 re-expressed; M-5).
printf '%s' "$sample" | grep -Eq 'syspolicyd_time=0(\.0)?($| )' && bad="$bad; measured 0.0 syspolicyd_time"
printf '%s' "$sample" | grep -Eq 'syspolicyd_time=0:00(\.0+)?($| )' && bad="$bad; measured 0:00 syspolicyd_time"
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

# TASK-4BBA6: a deliberately quiet cargo command that cannot finish before the
# watchdog gets two injected high samples.  The trap makes the test assert the
# same cancellation path a real cargo process takes, without compiling code or
# sampling the real machine.
install_stub_cargo_watchdog_target() {
    mkdir -p "$TMP/bin"
    cat > "$TMP/bin/cargo" <<'EOF'
#!/bin/sh
trap 'exit 143' TERM INT
while :; do
    sleep 1
done
EOF
    cat > "$TMP/bin/pgrep" <<'EOF'
#!/bin/sh
if [ "$1" = "-x" ] && [ "$2" = "syspolicyd" ]; then
    echo 4242
    exit 0
fi
# The watchdog's tree walk asks `pgrep -P`; this synthetic scanner has no
# descendants, and returning 1 means no pids rather than a probe failure.
exit 1
EOF
    cat > "$TMP/bin/ps" <<'EOF'
#!/bin/sh
case " $* " in
    *" %cpu= "*) echo '450.0' ;;
    *) echo '0:00.00' ;;
esac
EOF
    chmod +x "$TMP/bin/cargo" "$TMP/bin/pgrep" "$TMP/bin/ps"
}

# The first cargo invocation is the ordinary suite and finishes green.  The
# second is run-tests.sh's debug_assertions=off leg; only then does pgrep expose
# the synthetic scanner, proving that the special extra cargo command goes
# through the same watchdog rather than escaping it.
install_stub_cargo_no_debug_watchdog_target() {
    mkdir -p "$TMP/bin"
    cat > "$TMP/bin/cargo" <<EOF
#!/bin/sh
count_file="$TMP/watchdog-cargo-count"
count=0
[ -f "\$count_file" ] && count=\$(cat "\$count_file")
count=\$((count + 1))
printf '%s' "\$count" > "\$count_file"
if [ "\$count" -eq 1 ]; then
    cat <<'LOG'
     Running unittests src/lib.rs (stub)
running 1 test
test tests::x ... ok
test result: ok. 1 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.01s
LOG
    exit 0
fi
trap 'exit 143' TERM INT
while :; do
    sleep 1
done
EOF
    cat > "$TMP/bin/pgrep" <<EOF
#!/bin/sh
count_file="$TMP/watchdog-cargo-count"
count=0
[ -f "\$count_file" ] && count=\$(cat "\$count_file")
if [ "\$1" = "-x" ] && [ "\$2" = "syspolicyd" ] && [ "\$count" -ge 2 ]; then
    echo 4242
    exit 0
fi
exit 1
EOF
    cat > "$TMP/bin/ps" <<'EOF'
#!/bin/sh
case " $* " in
    *" %cpu= "*) echo '450.0' ;;
    *) echo '0:00.00' ;;
esac
EOF
    chmod +x "$TMP/bin/cargo" "$TMP/bin/pgrep" "$TMP/bin/ps"
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

# Live-path flake: cargo exits 101 WITH a per-test failure list whose
# signature matches the registry, and the binary path is the green fixture
# so isolation passes. The thirteen --classify flake cases cannot see M-1
# (SUITE_EXIT="?" short-circuits the crashed-binary arm).
# orgasmic:TASK-STWVB.1.1.1.1.1
# The exit code is a parameter (default 101) so F-1's pair can be driven by
# the SAME stub with only the code changed: 101 -> GREEN modulo 1 exit 0,
# 137 -> RED exit 1. Same output, same flake, opposite verdict, and nothing
# but the exit code differs between them.
install_stub_cargo_registered_flake() {
    local code="${1:-101}"
    mkdir -p "$TMP/bin"
    cat > "$TMP/bin/cargo" <<EOF
#!/bin/sh
cat <<LOG
     Running unittests src/lib.rs ($TMP/green)

running 1 test
test tests::recovery_inventory_waits_for_atomic_claim_commit ... FAILED

failures:

---- tests::recovery_inventory_waits_for_atomic_claim_commit stdout ----
thread 'tests::recovery_inventory_waits_for_atomic_claim_commit' panicked at src/lib.rs:1:1:
assertion failed: waited for the atomic claim commit

failures:
    tests::recovery_inventory_waits_for_atomic_claim_commit

test result: FAILED. 0 passed; 1 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.01s
LOG
exit $code
EOF
    chmod +x "$TMP/bin/cargo"
}

# Live-path load-sensitive shape: unregistered isolation-green failure.
install_stub_cargo_unregistered_iso_green() {
    mkdir -p "$TMP/bin"
    cat > "$TMP/bin/cargo" <<EOF
#!/bin/sh
cat <<LOG
     Running unittests src/lib.rs ($TMP/green)

running 1 test
test tests::nobody_has_ever_seen_this_one ... FAILED

failures:

---- tests::nobody_has_ever_seen_this_one stdout ----
thread 'tests::nobody_has_ever_seen_this_one' panicked at src/lib.rs:1:1:
some brand new panic

failures:
    tests::nobody_has_ever_seen_this_one

test result: FAILED. 0 passed; 1 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.01s
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
    "host     : unknown (window unknown)" \
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
    "host     : unknown (window unknown)" \
    "verdict: GREEN — no failures"

# B2 + TASK-STWVB.1.1.1.1: a crashed binary is never "suite looked green", even
# on a degraded host. After R-1 the crash is caught from the LOG and named in
# its own CRASHED section, so this case now pins the crash arm rather than the
# cargo-exit arm — same exit, strictly more said about why.
start "crashed cargo on degraded host -> RED names the crash, never looked green"
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
grep -qF 'CRASHED (1)' "$TMP/out.txt" || bad="$bad; output lacks the CRASHED section"
grep -qF '(signal: 6, SIGABRT: process abort signal)' "$TMP/out.txt" \
    || bad="$bad; output does not name the signal"
grep -qF 'verdict: RED — 1 crashed test target(s)' "$TMP/out.txt" \
    || bad="$bad; output lacks the crashed-target RED"
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

# TASK-4BBA6: the guard lives inside the runner, not around a particular
# invocation.  A sustained instantaneous scanner burst terminates cargo and
# reports the run as host-state INCONCLUSIVE before the ordinary classifier can
# turn a truncated log into a false product red.
start "watchdog stops sustained syspolicyd burst and returns INCONCLUSIVE"
registry "${KNOWN_FLAKE_ENTRY[@]}"
install_stub_cargo_watchdog_target
PATH="$TMP/bin:$PATH" \
    ORGASMIC_HOST_STATE_SAMPLE='load=0.5,syspolicyd_cpu=5.0,wall_s=100' \
    ORGASMIC_RUN_TESTS_WATCHDOG_TEST_FAST=1 \
    "$RUNNER" --registry "$TMP/registry.toml" --work-dir "$TMP/work" \
    > "$TMP/out.txt" 2>&1
RUN_EXIT=$?
rm -rf "$TMP/work"
check 4 "$RUN_EXIT" "$TMP/out.txt" \
    "watchdog : TRIPPED — syspolicyd percent=450.0 threshold=400 samples=2 interval_s=0; cargo was truncated before a test verdict" \
    "verdict: INCONCLUSIVE — host safety watchdog stopped the suite"

start "watchdog covers the debug_assertions=off cargo leg too"
registry "${KNOWN_FLAKE_ENTRY[@]}"
install_stub_cargo_no_debug_watchdog_target
PATH="$TMP/bin:$PATH" \
    ORGASMIC_RUN_TESTS_WATCHDOG_TEST_FAST=1 \
    "$RUNNER" --registry "$TMP/registry.toml" --work-dir "$TMP/work" \
    > "$TMP/out.txt" 2>&1
RUN_EXIT=$?
rm -rf "$TMP/work"
check 4 "$RUN_EXIT" "$TMP/out.txt" \
    "suite    : cargo test -p orgasmic-cli --bin orgasmic the_pause_rendezvous_hooks_park_only_in_debug_builds (debug_assertions=off)" \
    "watchdog : TRIPPED — syspolicyd percent=450.0 threshold=400 samples=2 interval_s=0; cargo was truncated before a test verdict"

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

# -- TASK-STWVB.1.1.1 -------------------------------------------------------
# orgasmic:TASK-STWVB.1.1.1
#
# M-1: the crashed-binary arm must not fire when FAIL_COUNT > 0. Round 3
# dropped that guard; verify/TASK-STWVB.1.1.1 pins this live-path case as the
# FIRST failure under injection. --classify cases cannot see it.

start "live path: registered flake on calm host -> GREEN modulo flake, exit 0"
registry "${KNOWN_FLAKE_ENTRY[@]}"
install_stub_cargo_registered_flake
PATH="$TMP/bin:$PATH" ORGASMIC_HOST_STATE_SAMPLE='load=0.5,syspolicyd_cpu=5.0,wall_s=100' \
    "$RUNNER" --registry "$TMP/registry.toml" --work-dir "$TMP/work" \
    > "$TMP/out.txt" 2>&1
RUN_EXIT=$?
live_log="$TMP/work/suite.log"
check 0 "$RUN_EXIT" "$TMP/out.txt" \
    "FLAKE (1)" \
    "host     : calm" \
    "verdict: GREEN modulo 1 registered flake"
# Same log through --classify must agree (standing invariant M-1 broke).
start "live path flake log via --classify agrees: GREEN modulo flake, exit 0"
if take_live_log; then
    run --classify "$TMP/suite.log"
    check 0 "$RUN_EXIT" "$TMP/out.txt" \
        "FLAKE (1)" \
        "verdict: GREEN modulo 1 registered flake"
fi

start "live path: registered flake on degraded host -> INCONCLUSIVE, exit 4"
registry "${KNOWN_FLAKE_ENTRY[@]}"
install_stub_cargo_registered_flake
PATH="$TMP/bin:$PATH" ORGASMIC_HOST_STATE_SAMPLE='load=0.5,syspolicyd_cpu=200.0,wall_s=100' \
    "$RUNNER" --registry "$TMP/registry.toml" --work-dir "$TMP/work" \
    > "$TMP/out.txt" 2>&1
RUN_EXIT=$?
live_log="$TMP/work/suite.log"
check 4 "$RUN_EXIT" "$TMP/out.txt" \
    "FLAKE (1)" \
    "host     : DEGRADED" \
    "verdict: INCONCLUSIVE — re-run when calm" \
    "registered flakes; still not a trusted green"
start "live path degraded-flake log via --classify agrees: INCONCLUSIVE, exit 4"
if take_live_log; then
    run --classify "$TMP/suite.log"
    check 4 "$RUN_EXIT" "$TMP/out.txt" \
        "FLAKE (1)" \
        "verdict: INCONCLUSIVE — re-run when calm"
fi

start "live path: degraded host + unregistered iso-green -> INCONCLUSIVE, exit 4"
registry "${KNOWN_FLAKE_ENTRY[@]}"
install_stub_cargo_unregistered_iso_green
PATH="$TMP/bin:$PATH" ORGASMIC_HOST_STATE_SAMPLE='load=0.5,syspolicyd_cpu=200.0,wall_s=100' \
    "$RUNNER" --registry "$TMP/registry.toml" --work-dir "$TMP/work" \
    > "$TMP/out.txt" 2>&1
RUN_EXIT=$?
rm -rf "$TMP/work"
check 4 "$RUN_EXIT" "$TMP/out.txt" \
    "LOAD-SENSITIVE (1)" \
    "host     : DEGRADED" \
    "verdict: INCONCLUSIVE — re-run when calm" \
    "load-sensitive failure"

# M-2: host word from the RATE, not any-field. load alone must not mint calm.
start "live path: cpu unknown keeps host unknown (no syspolicyd signal)"
registry "${KNOWN_FLAKE_ENTRY[@]}"
install_stub_cargo_ok
PATH="$TMP/bin:$PATH" ORGASMIC_HOST_STATE_SAMPLE='load=4.0,syspolicyd_cpu=?,wall_s=300' \
    "$RUNNER" --registry "$TMP/registry.toml" --work-dir "$TMP/work" \
    > "$TMP/out.txt" 2>&1
RUN_EXIT=$?
rm -rf "$TMP/work"
check 0 "$RUN_EXIT" "$TMP/out.txt" \
    "host     : unknown (no syspolicyd signal)" \
    "verdict: GREEN — no failures"

start "live path: stamp with no wall_s is unknown (pre-b92199a compat)"
registry "${KNOWN_FLAKE_ENTRY[@]}"
install_stub_cargo_ok
PATH="$TMP/bin:$PATH" ORGASMIC_HOST_STATE_SAMPLE='load=4.0,syspolicyd_cpu=250.0' \
    "$RUNNER" --registry "$TMP/registry.toml" --work-dir "$TMP/work" \
    > "$TMP/out.txt" 2>&1
RUN_EXIT=$?
rm -rf "$TMP/work"
check 0 "$RUN_EXIT" "$TMP/out.txt" \
    "host     : unknown (window unknown)" \
    "verdict: GREEN — no failures"

start "live path: wall_s=0 is unknown (window unknown), not calm"
registry "${KNOWN_FLAKE_ENTRY[@]}"
install_stub_cargo_ok
PATH="$TMP/bin:$PATH" ORGASMIC_HOST_STATE_SAMPLE='load=4.0,syspolicyd_cpu=250.0,wall_s=0' \
    "$RUNNER" --registry "$TMP/registry.toml" --work-dir "$TMP/work" \
    > "$TMP/out.txt" 2>&1
RUN_EXIT=$?
rm -rf "$TMP/work"
check 0 "$RUN_EXIT" "$TMP/out.txt" \
    "host     : unknown (window unknown)" \
    "verdict: GREEN — no failures"

# M-3: short windows neither calm nor DEGRADED.
start "live path: wall_s=1 rate above threshold is unknown (window too short)"
registry "${KNOWN_FLAKE_ENTRY[@]}"
install_stub_cargo_ok
PATH="$TMP/bin:$PATH" ORGASMIC_HOST_STATE_SAMPLE='load=4.0,syspolicyd_cpu=2.0,wall_s=1' \
    "$RUNNER" --registry "$TMP/registry.toml" --work-dir "$TMP/work" \
    > "$TMP/out.txt" 2>&1
RUN_EXIT=$?
rm -rf "$TMP/work"
check 0 "$RUN_EXIT" "$TMP/out.txt" \
    "host     : unknown (window too short to judge)" \
    "verdict: GREEN — no failures"

start "live path: wall_s=3 rate at threshold is unknown (window too short)"
registry "${KNOWN_FLAKE_ENTRY[@]}"
install_stub_cargo_ok
PATH="$TMP/bin:$PATH" ORGASMIC_HOST_STATE_SAMPLE='load=4.0,syspolicyd_cpu=4.5,wall_s=3' \
    "$RUNNER" --registry "$TMP/registry.toml" --work-dir "$TMP/work" \
    > "$TMP/out.txt" 2>&1
RUN_EXIT=$?
rm -rf "$TMP/work"
check 0 "$RUN_EXIT" "$TMP/out.txt" \
    "host     : unknown (window too short to judge)" \
    "verdict: GREEN — no failures"

# M-4: LOAD_DEGRADED_THRESHOLD is enforced by annotating elevated load.
start "elevated BEFORE load is annotated on the host line"
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
    "load=11.41 elevated (>=8.0), corroborating only" \
    "verdict: GREEN — no failures"

# -- TASK-STWVB.1.1.1.1 -----------------------------------------------------
# orgasmic:TASK-STWVB.1.1.1.1
#
# R-1: a crashed target contributes ZERO listed failures, so every detector
# keyed off cargo's exit code is switched off by the FAIL_COUNT guard the
# moment any other failure classifies. On a board with seven registered flakes
# that is the ordinary case: crash + flake returned `GREEN modulo 1 registered
# flake` at exit 0 over a log saying SIGABRT.

# The shape real cargo emitted in the reviewer's reproduction: one target fails
# with a listed failure whose signature is registered and which is green in
# isolation (so it classifies FLAKE, FAIL_COUNT=1), one target aborts and
# reports nothing at all. Backticks are escaped so the writer does not run
# them; the inner heredoc is quoted so the stub does not either.
install_stub_cargo_crash_plus_flake() {
    mkdir -p "$TMP/bin"
    cat > "$TMP/bin/cargo" <<EOF
#!/bin/sh
cat <<'LOG'
     Running unittests src/lib.rs ($TMP/green)

running 1 test
test tests::recovery_inventory_waits_for_atomic_claim_commit ... FAILED

failures:

---- tests::recovery_inventory_waits_for_atomic_claim_commit stdout ----
thread 'tests::recovery_inventory_waits_for_atomic_claim_commit' panicked at src/lib.rs:1:1:
assertion failed: waited for the atomic claim commit

failures:
    tests::recovery_inventory_waits_for_atomic_claim_commit

test result: FAILED. 0 passed; 1 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.01s

error: test failed, to rerun pass \`-p orgasmic-daemon --lib\`
     Running unittests src/main.rs (target/debug/deps/crashdemo-0cce3376237212be)

running 1 test
error: test failed, to rerun pass \`--bin crashdemo\`

Caused by:
  process didn't exit successfully: \`target/debug/deps/crashdemo-0cce3376237212be\` (signal: 6, SIGABRT: process abort signal)

error: 2 targets failed:
    \`-p orgasmic-daemon --lib\`
    \`--bin crashdemo\`
LOG
exit 101
EOF
    chmod +x "$TMP/bin/cargo"
}

# A non-zero cargo exit that names no failing target and lists no failure —
# cargo refused the invocation. CRASH_COUNT is 0 here by construction, so this
# is what keeps the B2 arm (and, under --classify, the suite-exit stamp)
# honest rather than dead.
install_stub_cargo_exit_without_targets() {
    mkdir -p "$TMP/bin"
    cat > "$TMP/bin/cargo" <<'EOF'
#!/bin/sh
cat <<'LOG'
error: no test target named `nope` in default-run packages
LOG
exit 101
EOF
    chmod +x "$TMP/bin/cargo"
}

start "live path: crashed target + registered flake -> RED exit 1, crash named"
registry "${KNOWN_FLAKE_ENTRY[@]}"
install_stub_cargo_crash_plus_flake
PATH="$TMP/bin:$PATH" ORGASMIC_HOST_STATE_SAMPLE='load=0.5,syspolicyd_cpu=5.0,wall_s=100' \
    "$RUNNER" --registry "$TMP/registry.toml" --work-dir "$TMP/work" \
    > "$TMP/out.txt" 2>&1
RUN_EXIT=$?
live_log="$TMP/work/suite.log"
check 1 "$RUN_EXIT" "$TMP/out.txt" \
    "crashed  : 1 target(s) died without reporting a failure list" \
    "CRASHED (1)" \
    "--bin crashdemo" \
    "(signal: 6, SIGABRT: process abort signal)" \
    "FLAKE (1)" \
    "host     : calm" \
    "verdict: RED — 1 crashed test target(s)"
# The crash must survive the flake, not the other way round.
start "crash + flake never reads as GREEN modulo flake"
if grep -qF 'verdict: GREEN modulo' "$TMP/out.txt"; then
    fail_case "a crashed target read as GREEN modulo a registered flake"
else
    printf 'ok   %s\n' "$CASE"
    PASSED=$((PASSED + 1))
fi

# Same log, both modes, same verdict — the acceptance criterion R-2 found unmet,
# checked on the crashed-binary population specifically.
start "crash+flake log via --classify agrees: RED exit 1, crash named"
if take_live_log; then
    run --classify "$TMP/suite.log"
    check 1 "$RUN_EXIT" "$TMP/out.txt" \
        "CRASHED (1)" \
        "FLAKE (1)" \
        "verdict: RED — 1 crashed test target(s)"
fi

# R-2: the suite exit is stamped into the log, so --classify judges the same
# number the live run saw. Before the stamp this log reclassified as
# `GREEN — no failures`, exit 0, against a live verdict of RED, exit 1.
start "live path: non-zero exit naming no target -> RED, cargo exit named"
registry "${KNOWN_FLAKE_ENTRY[@]}"
install_stub_cargo_exit_without_targets
PATH="$TMP/bin:$PATH" ORGASMIC_HOST_STATE_SAMPLE='load=0.5,syspolicyd_cpu=5.0,wall_s=100' \
    "$RUNNER" --registry "$TMP/registry.toml" --work-dir "$TMP/work" \
    > "$TMP/out.txt" 2>&1
RUN_EXIT=$?
live_log="$TMP/work/suite.log"
check 1 "$RUN_EXIT" "$TMP/out.txt" \
    "crashed  : none" \
    "verdict: RED — cargo exited 101 with no per-test failure list"

start "that log via --classify agrees: RED exit 1 (suite exit is stamped)"
if take_live_log; then
    grep -qE '^# orgasmic-suite-exit: 101$' "$TMP/suite.log" \
        || fail_case "live run wrote no suite-exit stamp"
    run --classify "$TMP/suite.log"
    check 1 "$RUN_EXIT" "$TMP/out.txt" \
        "verdict: RED — cargo exited 101 with no per-test failure list"
fi

# A pre-stamp log has no suite exit to read, and that fallback is documented.
# The crash detector is derived from the log text, so a legacy log carrying a
# crashed target is still RED.
start "--classify on an UNSTAMPED crashed-binary log is still RED, exit 1"
registry "${KNOWN_FLAKE_ENTRY[@]}"
cat > "$TMP/suite.log" <<'EOF'
     Running unittests src/lib.rs (target/debug/deps/stub-1234)
error: test failed, to rerun pass `-p orgasmic-daemon --lib`

Caused by:
  process didn't exit successfully: `target/debug/deps/stub-1234` (signal: 11, SIGSEGV: invalid memory reference)
EOF
run --classify "$TMP/suite.log"
check 1 "$RUN_EXIT" "$TMP/out.txt" \
    "CRASHED (1)" \
    "(signal: 11, SIGSEGV: invalid memory reference)" \
    "verdict: RED — 1 crashed test target(s)"

# -- TASK-STWVB.1.1.1.1.1 ---------------------------------------------------
# orgasmic:TASK-STWVB.1.1.1.1.1
#
# F-1: `FAIL_COUNT -eq 0` guarded the B2 arm as a PROXY for "the exit is 101".
# A cargo killed by the OS or by ^C exits 137/130 with the suite truncated,
# and one registered flake lifting FAIL_COUNT off zero swallowed it whole:
# `GREEN modulo 1 registered flake(s)`, exit 0, over a log stamped
# `# orgasmic-suite-exit: 137`. The pair below is the same stub with only the
# exit code changed, so the two verdicts cannot be explained by anything else.

start "live path: same flake stub at exit 101 -> GREEN modulo flake, exit 0"
registry "${KNOWN_FLAKE_ENTRY[@]}"
install_stub_cargo_registered_flake 101
PATH="$TMP/bin:$PATH" ORGASMIC_HOST_STATE_SAMPLE='load=0.5,syspolicyd_cpu=5.0,wall_s=100' \
    "$RUNNER" --registry "$TMP/registry.toml" --work-dir "$TMP/work" \
    > "$TMP/out.txt" 2>&1
RUN_EXIT=$?
rm -rf "$TMP/work"
check 0 "$RUN_EXIT" "$TMP/out.txt" \
    "cargo    : exited 101 (0 all green, 101 libtest reported failures)" \
    "crashed  : none" \
    "FLAKE (1)" \
    "verdict: GREEN modulo 1 registered flake"

start "live path: same flake stub at exit 137 -> RED exit 1, the exit is named"
registry "${KNOWN_FLAKE_ENTRY[@]}"
install_stub_cargo_registered_flake 137
PATH="$TMP/bin:$PATH" ORGASMIC_HOST_STATE_SAMPLE='load=0.5,syspolicyd_cpu=5.0,wall_s=100' \
    "$RUNNER" --registry "$TMP/registry.toml" --work-dir "$TMP/work" \
    > "$TMP/out.txt" 2>&1
RUN_EXIT=$?
live_log="$TMP/work/suite.log"
check 1 "$RUN_EXIT" "$TMP/out.txt" \
    "cargo    : exited 137 — ANOMALOUS" \
    "crashed  : none" \
    "FLAKE (1)" \
    "host     : calm" \
    "verdict: RED — cargo exited 137"

# The classified flake must not be able to excuse the truncation.
start "a killed cargo never reads as GREEN modulo flake"
if grep -qF 'verdict: GREEN modulo' "$TMP/out.txt"; then
    fail_case "a cargo killed at exit 137 read as GREEN modulo a registered flake"
else
    printf 'ok   %s\n' "$CASE"
    PASSED=$((PASSED + 1))
fi

start "killed-cargo log via --classify agrees: RED exit 1 (exit 137 is stamped)"
if take_live_log; then
    grep -qE '^# orgasmic-suite-exit: 137$' "$TMP/suite.log" \
        || fail_case "live run wrote no suite-exit stamp for 137"
    run --classify "$TMP/suite.log"
    check 1 "$RUN_EXIT" "$TMP/out.txt" \
        "FLAKE (1)" \
        "verdict: RED — cargo exited 137"
fi

# The one population whose verdict this arm CHANGES rather than fixes: a
# degraded host used to make a killed cargo INCONCLUSIVE (exit 4). The arm
# sits above HOST_DEGRADED, beside the crash arm, for the reason settled
# there — a truncated run has no trusted green in it to be inconclusive
# about, and exit 4 and exit 1 both fail CI, so only the diagnosis differs.
# Runs AFTER the --classify companion above: it reuses $TMP/work.
start "killed cargo outranks a degraded host: RED exit 1, not INCONCLUSIVE"
registry "${KNOWN_FLAKE_ENTRY[@]}"
install_stub_cargo_registered_flake 137
PATH="$TMP/bin:$PATH" ORGASMIC_HOST_STATE_SAMPLE='load=9.0,syspolicyd_cpu=50.0,wall_s=10' \
    "$RUNNER" --registry "$TMP/registry.toml" --work-dir "$TMP/work" \
    > "$TMP/out.txt" 2>&1
RUN_EXIT=$?
rm -rf "$TMP/work"
check 1 "$RUN_EXIT" "$TMP/out.txt" \
    "host     : DEGRADED" \
    "verdict: RED — cargo exited 137"

# F-4: the `(exit status: …)` cause shape had ZERO fixture coverage across all
# 50 prior cases, and it is the shape the crash detector reads on the real
# suite. Both forms, on a target that DID report its failures.
#
# The cause line names the binary by ABSOLUTE path while the `Running` line
# names it relative — measured on cargo 1.94.1 — so these fixtures also pin
# that the accounted/crashed join uses the `Running` path.

start "ordinary flake red WITH an (exit status: 101) cause -> GREEN modulo 1, exit 0"
registry "${KNOWN_FLAKE_ENTRY[@]}"
write_log "$TMP/green" "tests::recovery_inventory_waits_for_atomic_claim_commit" "$LOAD_PANIC"
cat >> "$TMP/suite.log" <<EOF

Caused by:
  process didn't exit successfully: \`/abs/target/debug/deps/stub-1234\` (exit status: 101)
EOF
run --classify "$TMP/suite.log"
check 0 "$RUN_EXIT" "$TMP/out.txt" \
    "crashed  : none" \
    "FLAKE (1)" \
    "verdict: GREEN modulo 1 registered flake"

# F-2: the exclusion was `(exit status: 101)` ONLY, and it DECIDED the crash
# rather than explaining one — so this same log with the status changed to 1
# returned `RED — 1 crashed test target(s)` and printed "this target reported
# NO failures" about a target whose failures are listed three lines above.
start "same log with an (exit status: 1) cause is still GREEN modulo 1 — it reported"
registry "${KNOWN_FLAKE_ENTRY[@]}"
write_log "$TMP/green" "tests::recovery_inventory_waits_for_atomic_claim_commit" "$LOAD_PANIC"
cat >> "$TMP/suite.log" <<EOF

Caused by:
  process didn't exit successfully: \`/abs/target/debug/deps/stub-1234\` (exit status: 1)
EOF
run --classify "$TMP/suite.log"
check 0 "$RUN_EXIT" "$TMP/out.txt" \
    "crashed  : none" \
    "FLAKE (1)" \
    "verdict: GREEN modulo 1 registered flake"

# F-3: NAMED and UNACCOUNTED can describe DIFFERENT targets. `-p … --lib`
# reported its failures and carries a non-101 cause; `--bin crashdemo`
# genuinely vanished and carries no cause at all. Unioned by cardinality this
# printed `--lib` — the target that DID report — and `--bin crashdemo`
# appeared nowhere.
start "reporting target + a different vanished target -> only the vanished one is named"
registry "${KNOWN_FLAKE_ENTRY[@]}"
write_log "$TMP/green" "tests::recovery_inventory_waits_for_atomic_claim_commit" "$LOAD_PANIC"
cat >> "$TMP/suite.log" <<EOF

Caused by:
  process didn't exit successfully: \`/abs/target/debug/deps/stub-1234\` (exit status: 1)
     Running unittests src/main.rs (target/debug/deps/crashdemo-0cce3376237212be)

running 1 test
error: test failed, to rerun pass \`--bin crashdemo\`

error: 2 targets failed:
    \`-p orgasmic-daemon --lib\`
    \`--bin crashdemo\`
EOF
run --classify "$TMP/suite.log"
check 1 "$RUN_EXIT" "$TMP/out.txt" \
    "crashed  : 1 target(s) died without reporting a failure list" \
    "CRASHED (1)" \
    "--bin crashdemo" \
    "cargo named this target as failed and printed no cause" \
    "failing  : 2 target(s) per cargo; 1 produced a failure list" \
    "verdict: RED — 1 crashed test target(s)"

start "the reporting target is not named as a crash"
if grep -qF '  -p orgasmic-daemon --lib' "$TMP/out.txt"; then
    fail_case "a target that reported its failure list was printed as crashed"
else
    printf 'ok   %s\n' "$CASE"
    PASSED=$((PASSED + 1))
fi

# orgasmic:TASK-STWVB.1.1.1.1.1.1
# F-6: the rerun line is the one parser dependency whose breakage fails GREEN.
# Reword it (as a cargo upgrade could) while cargo's independent `error: N
# target(s) failed:` summary still counts one: before the cross-check this
# read `GREEN modulo 1 registered flake`, exit 0, over a log this script could
# no longer parse. The control below is the same log with the rerun line
# intact, so the summary line alone is proven not to redden a run.
start "F-6: rerun line reworded, cargo summary counts 1 -> RED parser drift, exit 1"
registry "${KNOWN_FLAKE_ENTRY[@]}"
write_log "$TMP/green" "tests::recovery_inventory_waits_for_atomic_claim_commit" "$LOAD_PANIC"
sed 's/^error: test failed, to rerun pass `/error: test failed; rerun with `/' \
    "$TMP/suite.log" > "$TMP/suite.log.reworded" && mv "$TMP/suite.log.reworded" "$TMP/suite.log"
printf 'error: 1 target failed:\n    `-p orgasmic-daemon --lib`\n' >> "$TMP/suite.log"
run --classify "$TMP/suite.log"
check 1 "$RUN_EXIT" "$TMP/out.txt" \
    "targets  : cargo counts 1 failing target(s), this script parsed 0 rerun line(s) — PARSER DRIFT" \
    "verdict: RED — cargo counts 1 failing target(s) but this script parsed 0 rerun line(s)."

start "F-6 control: rerun line intact, cargo summary counts 1 -> agree, GREEN modulo 1"
registry "${KNOWN_FLAKE_ENTRY[@]}"
write_log "$TMP/green" "tests::recovery_inventory_waits_for_atomic_claim_commit" "$LOAD_PANIC"
printf 'error: 1 target failed:\n    `-p orgasmic-daemon --lib`\n' >> "$TMP/suite.log"
run --classify "$TMP/suite.log"
check 0 "$RUN_EXIT" "$TMP/out.txt" \
    "crashed  : none" \
    "verdict: GREEN modulo 1 registered flake"
if grep -qF 'PARSER DRIFT' "$TMP/out.txt"; then
    fail_case "agreeing counts were reported as parser drift"
fi

# F-7: a cargo-shaped rerun line INSIDE a test's captured panic block used to be
# read as a real failing target. The phantom then claimed the reporting binary,
# so the target that DID report its failures was printed as crashed and
# `--bin phantom` appeared nowhere. Both awks now honour the capture region.
PHANTOM_PANIC="$LOAD_PANIC"$'\n''error: test failed, to rerun pass `--bin phantom`'
start "F-7: cargo-shaped rerun line inside a captured panic block mints no target"
registry "${KNOWN_FLAKE_ENTRY[@]}"
write_log "$TMP/green" "tests::recovery_inventory_waits_for_atomic_claim_commit" "$PHANTOM_PANIC"
printf 'error: 1 target failed:\n    `-p orgasmic-daemon --lib`\n' >> "$TMP/suite.log"
run --classify "$TMP/suite.log"
check 0 "$RUN_EXIT" "$TMP/out.txt" \
    "crashed  : none" \
    "FLAKE (1)" \
    "verdict: GREEN modulo 1 registered flake"
start "F-7: the phantom target is named nowhere and nothing reads as crashed"
if grep -qF -- '--bin phantom' "$TMP/out.txt" || grep -qF 'CRASHED (' "$TMP/out.txt"; then
    fail_case "a rerun line inside a captured panic block minted a target"
    printf -- '---- output ----\n'; cat "$TMP/out.txt"; printf -- '----------------\n'
else
    printf 'ok   %s\n' "$CASE"
    PASSED=$((PASSED + 1))
fi

# ---------------------------------------------------------------------------

printf '\n%s passed, %s failed\n' "$PASSED" "$FAILED"
# R-5: a case that stops running must not be able to look like a pass.
# F-6: the expectation is the number of `start` calls in this file, so adding
# a case needs no bookkeeping and this can only fire on a case that started
# and never reported.
TOTAL_CASES=$((PASSED + FAILED))
if [ "$TOTAL_CASES" -ne "$EXPECTED_CASES" ]; then
    printf 'FAIL case count: %s case(s) reported, but this file opens %s of them — a case started and never reported\n' \
        "$TOTAL_CASES" "$EXPECTED_CASES"
    exit 1
fi
[ "$FAILED" -eq 0 ] || exit 1
