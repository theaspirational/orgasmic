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

# ---------------------------------------------------------------------------

printf '\n%s passed, %s failed\n' "$PASSED" "$FAILED"
[ "$FAILED" -eq 0 ] || exit 1
