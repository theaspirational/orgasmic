#!/usr/bin/env bash
# orgasmic:TASK-DJ1WV
#
# Run the test suite and CLASSIFY the red instead of leaving it to folklore.
#
# Standing red that a human has to interpret from memory is worse than no red:
# it trains everyone to discount failures, which is exactly how a real
# regression walks through a green gate. This wrapper turns "which of these are
# the known ones?" into a machine answer:
#
#   1. run the suite (never piping cargo — see .orgasmic/gotchas.org)
#   2. rerun every failure IN ISOLATION
#   3. match each failure's panic text against verify/flake-registry.toml
#   4. print a verdict block that separates REAL from FLAKE, owners named
#
# Exit 0 only when every failure is a registered, signature-matched flake that
# is green in isolation. Anything else is red that means something.
#
# Usage:
#   scripts/run-tests.sh                          whole workspace
#   scripts/run-tests.sh -p orgasmic-daemon --lib scoped, same classification
#   scripts/run-tests.sh --check                  registry hygiene only
#   scripts/run-tests.sh --classify <log>         re-read an existing cargo log
#   scripts/run-tests.sh --registry <path> ...    use a different registry
#   scripts/run-tests.sh --help
#
# Exit codes: 0 clean or all-flake · 1 REAL failure present · 2 registry
# rejected · 3 wrapper misuse.

set -uo pipefail

# The drivers-suite test that spawns a real provider turn and bills real money.
# Every invocation carries the skip; the wrapper also refuses to be handed the
# name, and asserts afterwards that the test did not run.
BILLED_TEST="legacy_drivers_and_explicit_pairs_emit_equivalent_start_events"

# orgasmic:TASK-S2KM0
# The escape hatch that turns a missing-tooling sentinel FAILURE into a warning.
# Mirrors `ALLOW_MISSING_TOOLS_ENV` in crates/orgasmic-drivers/src/modes/rmux.rs.
ALLOW_MISSING_ENV="ORGASMIC_ALLOW_MISSING_TOOLS"

# Run id and home leak out of a dispatch into every child and break the suites
# we are trying to reproduce.
SCRUB=(env -u ORGASMIC_RUN_ID -u ORGASMIC_HOME)

EXIT_REAL=1
EXIT_REGISTRY=2
EXIT_MISUSE=3

die() {
    printf 'run-tests: %s\n' "$1" >&2
    exit "${2:-$EXIT_MISUSE}"
}

usage() {
    awk 'NR > 2 { if ($0 !~ /^#/) exit; sub(/^# ?/, ""); print }' "$0"
}

# ---------------------------------------------------------------------------
# arguments
# ---------------------------------------------------------------------------

REPO=$(git rev-parse --show-toplevel 2>/dev/null) ||
    die "not inside a git worktree; run this from the repo"
cd "$REPO" || die "cannot cd to $REPO"

REGISTRY="$REPO/verify/flake-registry.toml"
CLASSIFY_LOG=""
CHECK_ONLY=0
WORK=""

while [ $# -gt 0 ]; do
    case "$1" in
        --help | -h)
            usage
            exit 0
            ;;
        --check)
            CHECK_ONLY=1
            shift
            ;;
        --registry)
            [ $# -ge 2 ] || die "--registry needs a path"
            REGISTRY="$2"
            shift 2
            ;;
        --classify)
            [ $# -ge 2 ] || die "--classify needs a cargo test log"
            CLASSIFY_LOG="$2"
            shift 2
            ;;
        --work-dir)
            # Where logs and parsed failure detail land. Defaults to a temp
            # directory; the self-test pins it so it can read the artifacts.
            [ $# -ge 2 ] || die "--work-dir needs a path"
            WORK="$2"
            shift 2
            ;;
        *) break ;;
    esac
done

CARGO_ARGS=("$@")
if [ ${#CARGO_ARGS[@]} -eq 0 ]; then
    CARGO_ARGS=(--workspace)
fi

for arg in ${CARGO_ARGS[@]+"${CARGO_ARGS[@]}"}; do
    case "$arg" in
        *"$BILLED_TEST"*)
            die "refusing: \`$BILLED_TEST\` spawns a real provider turn and bills real money. This wrapper always skips it; do not name it."
            ;;
    esac
done

if [ -z "$WORK" ]; then
    WORK=$(mktemp -d "${TMPDIR:-/tmp}/orgasmic-run-tests.XXXXXX") ||
        die "cannot create a work directory"
fi
mkdir -p "$WORK/detail" || die "cannot create $WORK/detail"

# ---------------------------------------------------------------------------
# registry
# ---------------------------------------------------------------------------

REGISTRY_TSV="$WORK/registry.tsv"

# A deliberately strict TOML subset: `[[flake]]` tables of quoted scalars and
# nothing else. Strict because a registry that silently drops a malformed entry
# reclassifies a real failure as unregistered — or worse, never notices that a
# signature went missing.
parse_registry() {
    awk -v out="$REGISTRY_TSV" '
        function fail(msg) {
            printf("%s:%d: %s\n", FILENAME, FNR, msg) > "/dev/stderr"
            errs++
        }
        function flush_entry(   missing) {
            if (!open_entry) return
            missing = ""
            if (v_test == "")      missing = missing " test"
            if (v_owner == "")     missing = missing " owner"
            if (v_signature == "") missing = missing " signature"
            if (v_evidence == "")  missing = missing " evidence"
            if (v_filed == "")     missing = missing " filed"
            if (missing != "") {
                printf("%s: [[flake]] starting at line %d is missing:%s\n",
                       FILENAME, entry_line, missing) > "/dev/stderr"
                errs++
            } else {
                printf("%s\t%s\t%s\t%s\t%s\n",
                       v_test, v_owner, v_signature, v_evidence, v_filed) > out
                kept++
            }
            open_entry = 0
        }
        function unquote(val,   n) {
            if (val !~ /^".*"$/) return "\001"
            val = substr(val, 2, length(val) - 2)
            gsub(/\\"/, "\"", val)
            return val
        }
        {
            line = $0
            sub(/^[ \t]+/, "", line)
            sub(/[ \t]+$/, "", line)
            if (line == "" || line ~ /^#/) next
            if (line == "[[flake]]") {
                flush_entry()
                open_entry = 1; entry_line = FNR
                v_test = ""; v_owner = ""; v_signature = ""
                v_evidence = ""; v_filed = ""
                next
            }
            if (line ~ /^\[/) {
                fail("unknown table `" line "`; this file holds only [[flake]] entries")
                next
            }
            if (!open_entry) { fail("`" line "` appears before the first [[flake]]"); next }
            eq = index(line, "=")
            if (eq == 0) { fail("`" line "` is not a `key = \"value\"` line"); next }
            key = substr(line, 1, eq - 1); sub(/[ \t]+$/, "", key)
            raw = substr(line, eq + 1);   sub(/^[ \t]+/, "", raw)
            val = unquote(raw)
            if (val == "\001") { fail("value for `" key "` must be a double-quoted string"); next }
            if (index(val, "\t") > 0) { fail("value for `" key "` contains a tab"); next }
            if      (key == "test")      v_test = val
            else if (key == "owner")     v_owner = val
            else if (key == "signature") v_signature = val
            else if (key == "evidence")  v_evidence = val
            else if (key == "filed")     v_filed = val
            else fail("unknown key `" key "`. Known: test, owner, signature, evidence, filed")
            if (key == "owner" && val !~ /^TASK-[A-Z0-9.]+$/)
                fail("owner `" val "` is not a TASK id")
            if (key == "signature" && length(val) < 8)
                fail("signature `" val "` is too short to identify a failure mode")
        }
        END {
            flush_entry()
            if (errs > 0) exit 1
            if (kept == 0) printf("(registry holds no entries)\n") > "/dev/stderr"
        }
    ' "$REGISTRY"
}

# An entry whose owning task is closed is a graveyard headstone: it keeps a
# fixed test permanently excused. The lifecycle files are committed, so this
# check needs no daemon and works in a worktree and in CI.
check_owner_lifecycle() {
    local tasks="$REPO/.orgasmic/tasks"
    local problems=0 owner state
    if [ ! -d "$tasks" ]; then
        printf 'registry: %s not found — owner lifecycle NOT checked\n' "$tasks"
        return 0
    fi
    while IFS=$'\t' read -r _test owner _sig _ev _filed; do
        [ -n "$owner" ] || continue
        state=""
        for stage in done cancelled; do
            if grep -Eq "^[ \t]*:ID:[ \t]+${owner}[ \t]*$" "$tasks/$stage.org" 2>/dev/null; then
                state="$stage"
                break
            fi
        done
        if [ -n "$state" ]; then
            printf 'registry: %s is owned by %s, which is %s. A registry that only grows is a graveyard: delete the entry, or reopen the task.\n' \
                "$_test" "$owner" "$state"
            problems=$((problems + 1))
            continue
        fi
        if ! grep -Eqr "^[ \t]*:ID:[ \t]+${owner}[ \t]*$" "$tasks" 2>/dev/null; then
            printf 'registry: %s is owned by %s, which is not a task in %s. Nobody is fixing this flake.\n' \
                "$_test" "$owner" "$tasks"
            problems=$((problems + 1))
        fi
    done < "$REGISTRY_TSV"
    [ "$problems" -eq 0 ]
}

check_duplicates() {
    local dupes
    dupes=$(cut -f1,3 "$REGISTRY_TSV" | sort | uniq -d)
    if [ -n "$dupes" ]; then
        printf 'registry: duplicate (test, signature) pairs:\n%s\n' "$dupes"
        return 1
    fi
    return 0
}

registry_check() {
    if [ ! -f "$REGISTRY" ]; then
        printf 'registry: %s does not exist\n' "$REGISTRY" >&2
        return 1
    fi
    : > "$REGISTRY_TSV"
    parse_registry || return 1
    local ok=0
    check_duplicates || ok=1
    check_owner_lifecycle || ok=1
    return $ok
}

if ! registry_check; then
    printf 'registry: REJECTED (%s)\n' "$REGISTRY" >&2
    exit "$EXIT_REGISTRY"
fi
REGISTRY_COUNT=$(wc -l < "$REGISTRY_TSV" | tr -d ' ')
if [ "$CHECK_ONLY" -eq 1 ]; then
    printf 'registry: OK — %s entries in %s, every owner open\n' \
        "$REGISTRY_COUNT" "${REGISTRY#$REPO/}"
    exit 0
fi

# ---------------------------------------------------------------------------
# run
# ---------------------------------------------------------------------------

SUITE_LOG="$WORK/suite.log"

if [ -n "$CLASSIFY_LOG" ]; then
    [ -f "$CLASSIFY_LOG" ] || die "no such log: $CLASSIFY_LOG"
    cp "$CLASSIFY_LOG" "$SUITE_LOG" || die "cannot copy $CLASSIFY_LOG"
    SUITE_CMD="(reclassified from $CLASSIFY_LOG)"
    SUITE_EXIT="?"
else
    # `--no-fail-fast` because a classification needs the WHOLE failure list;
    # stopping at the first red binary is how a real failure hides behind a
    # known flake. Output goes to a file, never a pipe: a test that leaves a
    # descendant holding the write end makes a pipe hang forever after the
    # suite has already passed (.orgasmic/gotchas.org).
    SUITE_CMD="cargo test ${CARGO_ARGS[*]} --no-fail-fast -- --skip $BILLED_TEST"
    printf 'run-tests: %s\n' "$SUITE_CMD"
    printf 'run-tests: log %s\n' "$SUITE_LOG"
    "${SCRUB[@]}" cargo test ${CARGO_ARGS[@]+"${CARGO_ARGS[@]}"} --no-fail-fast \
        -- --skip "$BILLED_TEST" > "$SUITE_LOG" 2>&1
    SUITE_EXIT=$?
fi

# The skip is a default, not a promise. Assert it held.
BILLED_RAN=0
if grep -Eq "^test .*${BILLED_TEST} \.\.\. (ok|FAILED)" "$SUITE_LOG"; then
    BILLED_RAN=1
fi

# ---------------------------------------------------------------------------
# parse failures out of the cargo log
# ---------------------------------------------------------------------------

FAILURES_TSV="$WORK/failures.tsv"
: > "$FAILURES_TSV"

awk -v outdir="$WORK/detail" -v out="$FAILURES_TSV" '
    {
        line = $0
        if (line ~ /^[ \t]+Running /) {
            bin = line
            sub(/^.*\(/, "", bin); sub(/\).*$/, "", bin)
            if (capturing) { close(df); capturing = 0 }
            names = 0
            next
        }
        if (line ~ /^[ \t]+Doc-tests /) {
            bin = "doctests"
            if (capturing) { close(df); capturing = 0 }
            names = 0
            next
        }
        if (line ~ /^---- .* ----$/) {
            name = line
            sub(/^---- /, "", name); sub(/ [a-z]+ ----$/, "", name)
            if (capturing) close(df)
            df = outdir "/" (++idx) ".txt"
            detail[bin "\001" name] = df
            printf("") > df
            capturing = 1; names = 0
            next
        }
        if (line ~ /^failures:$/) {
            if (capturing) { close(df); capturing = 0 }
            names = 1
            next
        }
        if (line ~ /^test result:/) {
            if (capturing) { close(df); capturing = 0 }
            names = 0
            next
        }
        if (capturing) { print line > df; next }
        if (names && line ~ /^    [^ ]/) {
            name = line; sub(/^ +/, "", name); sub(/[ \t]+$/, "", name)
            key = bin "\001" name
            printf("%s\t%s\t%s\n", name, bin, (key in detail) ? detail[key] : "") >> out
        }
    }
' "$SUITE_LOG"

# `error: test failed` is an ordinary red and classifiable; a compile error is
# not — nothing ran, so nothing can be excused.
BUILD_BROKE=0
if grep -Eq '^error: could not compile|^error\[E[0-9]+\]' "$SUITE_LOG"; then
    BUILD_BROKE=1
fi

FAIL_COUNT=$(wc -l < "$FAILURES_TSV" | tr -d ' ')

# ---------------------------------------------------------------------------
# classify
# ---------------------------------------------------------------------------

# Rerun one failure alone. A flake that is only load-dependent goes green here;
# anything that still fails alone is real regardless of what the registry says.
rerun_isolated() {
    local bin="$1" name="$2" log="$3"
    case "$name" in
        *"$BILLED_TEST"*)
            printf 'refused: billed test\n' > "$log"
            return 2
            ;;
    esac
    if [ "$bin" = "doctests" ] || [ ! -x "$bin" ]; then
        printf 'no runnable test binary for %s\n' "$name" > "$log"
        return 2
    fi
    "${SCRUB[@]}" "$bin" --exact "$name" --test-threads=1 > "$log" 2>&1
    return $?
}

REAL_REPORT="$WORK/real.txt"
FLAKE_REPORT="$WORK/flake.txt"
: > "$REAL_REPORT"
: > "$FLAKE_REPORT"
REAL_COUNT=0
FLAKE_COUNT=0

# The registry is keyed by the name cargo prints. A bare function name is
# accepted too, so an entry does not go stale when a test moves module.
matching_entries() {
    awk -F'\t' -v want="$1" '
        {
            if ($1 == want) { print; next }
            n = length($1)
            if (length(want) > n && substr(want, length(want) - n - 1) == "::" $1) print
        }
    ' "$REGISTRY_TSV"
}

# The one line an operator needs to tell two failure modes apart. libtest puts
# the location on the `panicked at` line and the message on the next one, so
# both are needed — the location alone is what let one failure get filed
# against the wrong task.
first_panic() {
    local detail="$1" text=""
    if [ -n "$detail" ] && [ -f "$detail" ]; then
        text=$(awk '
            /panicked at/ {
                out = $0
                if ((getline nxt) > 0 && nxt !~ /^note: run with/) out = out " " nxt
                sub(/^[ \t]+/, "", out)
                print out
                exit
            }' "$detail")
        [ -n "$text" ] || text=$(grep -m1 -E 'assertion|^Error|^error' "$detail" |
            sed 's/^[[:space:]]*//')
        [ -n "$text" ] || text=$(grep -m1 . "$detail" | sed 's/^[[:space:]]*//')
    fi
    [ -n "$text" ] || text='(no panic text captured)'
    printf '%.220s' "$text"
}

classify_one() {
    local name="$1" bin="$2" detail="$3"
    local iso_log="$WORK/isolation-$(printf '%s' "$name" | tr -c 'A-Za-z0-9_.' '_').log"
    local entries matched_owner="" matched_sig="" matched_ev="" iso
    entries=$(matching_entries "$name")

    if [ -z "$entries" ]; then
        rerun_isolated "$bin" "$name" "$iso_log"
        iso=$?
        REAL_COUNT=$((REAL_COUNT + 1))
        {
            printf '  %s\n' "$name"
            printf '      binary   : %s\n' "$bin"
            printf '      why      : NOT IN THE REGISTRY — no entry claims this failure\n'
            printf '      isolation: %s\n' "$(iso_word "$iso")"
            printf '      panic    : %s\n' "$(first_panic "$detail")"
            printf '      next     : fix it, or register it against the open task that owns it\n'
        } >> "$REAL_REPORT"
        return
    fi

    while IFS=$'\t' read -r _t owner sig ev _filed; do
        [ -n "$sig" ] || continue
        if [ -n "$detail" ] && [ -f "$detail" ] && grep -qF -- "$sig" "$detail"; then
            matched_owner="$owner"
            matched_sig="$sig"
            matched_ev="$ev"
            break
        fi
    done <<EOF
$entries
EOF

    if [ -z "$matched_owner" ]; then
        # The mislabel detector. This test is known to flake, but it did not
        # flake THIS time — it failed a different way, and a different way is
        # nobody's known problem until somebody reads it.
        REAL_COUNT=$((REAL_COUNT + 1))
        {
            printf '  %s\n' "$name"
            printf '      binary   : %s\n' "$bin"
            printf '      why      : REGISTERED NAME, UNREGISTERED SIGNATURE — a different\n'
            printf '                 failure mode than the one this name is excused for\n'
            printf '      observed : %s\n' "$(first_panic "$detail")"
            printf '%s\n' "$entries" | while IFS=$'\t' read -r _t owner sig _ev _filed; do
                [ -n "$sig" ] || continue
                printf '      excused  : "%s" (owner %s) — did not match\n' "$sig" "$owner"
            done
            printf '      next     : read the panic. A new mode needs its own entry and its\n'
            printf '                 own owning task before it may be excused\n'
        } >> "$REAL_REPORT"
        return
    fi

    rerun_isolated "$bin" "$name" "$iso_log"
    iso=$?
    if [ "$iso" -ne 0 ]; then
        REAL_COUNT=$((REAL_COUNT + 1))
        {
            printf '  %s\n' "$name"
            printf '      binary   : %s\n' "$bin"
            printf '      why      : REGISTERED AS LOAD-DEPENDENT BUT FAILS ALONE TOO — the\n'
            printf '                 excuse ("green in isolation") no longer holds\n'
            printf '      owner    : %s\n' "$matched_owner"
            printf '      isolation: %s (log %s)\n' "$(iso_word "$iso")" "$iso_log"
            printf '      panic    : %s\n' "$(first_panic "$detail")"
        } >> "$REAL_REPORT"
        return
    fi

    FLAKE_COUNT=$((FLAKE_COUNT + 1))
    {
        printf '  %s\n' "$name"
        printf '      owner    : %s\n' "$matched_owner"
        printf '      signature: "%s" — matched\n' "$matched_sig"
        printf '      isolation: passed\n'
        printf '      evidence : %s\n' "$matched_ev"
    } >> "$FLAKE_REPORT"
}

iso_word() {
    case "$1" in
        0) printf 'passed' ;;
        2) printf 'not rerunnable' ;;
        *) printf 'FAILED (exit %s)' "$1" ;;
    esac
}

while IFS=$'\t' read -r name bin detail; do
    [ -n "$name" ] || continue
    classify_one "$name" "$bin" "$detail"
done < "$FAILURES_TSV"

# ---------------------------------------------------------------------------
# what the environment withheld
# ---------------------------------------------------------------------------

# orgasmic:TASK-S2KM0
#
# A test that did not run is not a test that passed.
#
# `assert_required_test_tooling` already refuses to let an UNacknowledged gap
# through: a missing tool fails one sentinel test per binary, which arrives
# above as an unregistered failure and is reported REAL. Nothing here weakens
# that. What it cannot catch is an ACKNOWLEDGED gap — somebody set
# ORGASMIC_ALLOW_MISSING_TOOLS, the sentinel downgraded to a warning, and the
# gated tests quietly did not run.
#
# That acknowledgement is written into the cargo log and nowhere else. The
# verdict block is the part a human reads and the only part a CI job surfaces,
# and until now it printed GREEN without ever saying what it had declined to
# run. A CI lane that skips half a suite and reports success is the exact
# failure this project's `--skip`-from-memory habit already produced once.
#
# So: lift it out and name it. Acknowledged skips do not fail the run — they
# are a deliberate operator choice, and failing them would make the lane red on
# every host without a proprietary harness CLI. They simply may not be quiet.
SKIPPED_TSV="$WORK/skipped.tsv"
: > "$SKIPPED_TSV"

# `claude (gates 8 tests), codex (gates 1 test)` out of the sentinel's warning.
# One warning per test binary, and the same tool gates a different count in
# each, so occurrences are summed rather than deduped: two binaries each gating
# one `claude` test really is two tests that did not run.
awk -v out="$SKIPPED_TSV" '
    /ORGASMIC_ALLOW_MISSING_TOOLS explicitly allows missing test tooling: / {
        body = $0
        sub(/^.*explicitly allows missing test tooling: /, "", body)
        sub(/; those gated tests did not run.*$/, "", body)
        n = split(body, parts, /\), /)
        for (i = 1; i <= n; i++) {
            item = parts[i]
            sub(/\)[ \t]*$/, "", item)
            if (match(item, / \(gates /)) {
                tool = substr(item, 1, RSTART - 1)
                count = substr(item, RSTART + RLENGTH)
                sub(/ tests?$/, "", count)
                if (count ~ /^[0-9]+$/) printf("%s\t%s\n", tool, count) >> out
            }
        }
    }
' "$SUITE_LOG"

SKIPPED_TOOLS=$(awk -F'\t' '{ total[$1] += $2 } END { for (t in total) printf("%s\t%s\n", t, total[t]) }' \
    "$SKIPPED_TSV" | sort)
SKIPPED_TESTS=$(awk -F'\t' '{ n += $2 } END { print n + 0 }' "$SKIPPED_TSV")

# `#[ignore]`d tests are the other silent non-run. The billed test is one of
# them, and it is already named above; the count catches the rest.
IGNORED_COUNT=$(awk '
    /^test result:/ { for (i = 2; i <= NF; i++) if ($i == "ignored;") n += $(i - 1) }
    END { print n + 0 }
' "$SUITE_LOG")

# ---------------------------------------------------------------------------
# verdict
# ---------------------------------------------------------------------------

RULE="======================================================================"
printf '\n%s\n' "$RULE"
printf 'VERDICT\n'
printf '%s\n' "$RULE"
printf '  suite    : %s\n' "$SUITE_CMD"
printf '  log      : %s\n' "$SUITE_LOG"
printf '  registry : %s (%s entries, every owner open)\n' "${REGISTRY#$REPO/}" "$REGISTRY_COUNT"
if [ "$BILLED_RAN" -eq 0 ]; then
    printf '  billed   : %s — NOT RUN (--skip applied to every invocation)\n' "$BILLED_TEST"
else
    printf '  billed   : %s — RAN. THIS COSTS REAL MONEY.\n' "$BILLED_TEST"
fi
printf '  failures : %s\n' "$FAIL_COUNT"
printf '  ignored  : %s test(s) carrying #[ignore]\n' "$IGNORED_COUNT"
if [ -z "$SKIPPED_TOOLS" ]; then
    printf '  environ  : complete — no tool requirement was waived\n'
else
    printf '  environ  : INCOMPLETE — %s test(s) gated out by absent tooling\n' "$SKIPPED_TESTS"
fi

if [ -n "$SKIPPED_TOOLS" ]; then
    printf '\nNOT RUN (%s) — tooling absent, and %s said that was acceptable.\n' \
        "$SKIPPED_TESTS" "$ALLOW_MISSING_ENV"
    printf 'These tests did not pass. They did not execute:\n'
    printf '%s\n' "$SKIPPED_TOOLS" | while IFS=$'\t' read -r tool count; do
        [ -n "$tool" ] || continue
        printf '  %-16s gates %s test(s)\n' "$tool" "$count"
    done
    printf '  next     : install the tooling and drop its name from %s.\n' "$ALLOW_MISSING_ENV"
    printf '             A green verdict above covers the rest of the suite, not these.\n'
fi

if [ "$BUILD_BROKE" -eq 1 ]; then
    printf '\nREAL — the workspace did not build. No classification is possible:\n'
    grep -E '^error(\[E[0-9]+\])?: ' "$SUITE_LOG" | head -5 | sed 's/^/  /'
fi

if [ "$REAL_COUNT" -gt 0 ]; then
    printf '\nREAL (%s):\n' "$REAL_COUNT"
    cat "$REAL_REPORT"
fi
if [ "$FLAKE_COUNT" -gt 0 ]; then
    printf '\nFLAKE (%s) — green in isolation, registered signature matched:\n' "$FLAKE_COUNT"
    cat "$FLAKE_REPORT"
fi

STATUS=0
if [ "$BILLED_RAN" -eq 1 ]; then
    printf '\nverdict: RED — the billed test ran; the skip did not hold.\n'
    STATUS=$EXIT_REAL
elif [ "$BUILD_BROKE" -eq 1 ]; then
    printf '\nverdict: RED — build failure.\n'
    STATUS=$EXIT_REAL
elif [ "$REAL_COUNT" -gt 0 ]; then
    printf '\nverdict: RED — %s real failure(s). This red means something.\n' "$REAL_COUNT"
    STATUS=$EXIT_REAL
elif [ "$FAIL_COUNT" -gt 0 ]; then
    printf '\nverdict: GREEN modulo %s registered flake(s). No unexplained red.\n' "$FLAKE_COUNT"
elif [ "$SUITE_EXIT" != "?" ] && [ "$SUITE_EXIT" != 0 ]; then
    printf '\nverdict: RED — cargo exited %s with no per-test failure list. Read %s.\n' \
        "$SUITE_EXIT" "$SUITE_LOG"
    STATUS=$EXIT_REAL
else
    printf '\nverdict: GREEN — no failures.\n'
fi
printf '%s\n' "$RULE"
exit "$STATUS"
