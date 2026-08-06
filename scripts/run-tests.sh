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
# is green in isolation, AND the host was calm enough for that verdict to mean
# something. Anything else is either red that means something, or inconclusive
# because the host was thrashing (see HOST STATE below).
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
# rejected · 3 wrapper misuse · 4 INCONCLUSIVE (host degraded — re-run when calm).
#
# Host state (TASK-STWVB / TASK-STWVB.1 / TASK-STWVB.1.1):
#   On a live suite run: load is sampled BEFORE the suite, and syspolicyd
#   cumulative CPU-time is sampled before and after so the gate sees a delta
#   across the run — the instrument that previously produced +281 s with a
#   scan storm / +32 s without. Point-sampled %CPU is not used: it is a
#   short-window decaying average, and a post-suite sample cannot observe a
#   burst that collapses within seconds.
#
#   On --classify: host state is UNKNOWN unless the suite log carries an
#   `# orgasmic-host-state:` stamp written by the live run that produced it.
#   Reclassify-time sampling is never used — it has no relationship to the run
#   under review and must not mint a LOAD-SENSITIVE excuse.
#
#   Thresholds (TASK-STWVB.1.1):
#     SYSPOLICYD_RATE_DEGRADED — syspolicyd CPU seconds per wall second of the
#       sampled window. An absolute CPU-seconds bound is a bound on run
#       duration (ambient alone reaches 100 s in ~22 min). Load is
#       corroborating only: it is printed on the stamp but does not trip the
#       gate alone (and on Linux there is no Gatekeeper scan storm to excuse).
#
#   A missing signal is ignored, never fatal: unknown fields become `?`, never
#   a measured `0.0` or a blank. Linux has no syspolicyd. The summary word is
#   `unknown` when no signal parsed as a number — not `calm`.
#
#   Injector for the self-test live path (and only for that): set
#     ORGASMIC_HOST_STATE_SAMPLE=load=<f>,syspolicyd_cpu=<f>,wall_s=<f>
#   to force the judgment sample (load = BEFORE load, syspolicyd_cpu = delta
#   seconds, wall_s = wall seconds of the window; rate = cpu/wall_s). Omit a
#   key to leave it unknown. Ignored under --classify.

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
EXIT_INCONCLUSIVE=4

# orgasmic:TASK-STWVB.1.1
# Load is printed on the host stamp for operators but is NOT an independent
# trigger (F-C). The BEFORE-load band on this board straddles ambient
# dispatch activity (measured BEFORE values 4.01–12.95); the suite's own
# mid-run contribution was never a BEFORE baseline. Kept for stamp display.
LOAD_DEGRADED_THRESHOLD=8.0
# syspolicyd CPU seconds per wall second of the sampled window.
# Ambient (two independent 60 s idle windows, 2026-08-06): 0.0737–0.0753 s/s.
# Scoped in-run on this Mac: 0.35–0.96 s/s. Historical +32/+281 pair is the
# same quantity re-expressed over its wall. Workspace calibration and the
# chosen number are recorded in the constant comment below after measurement.
SYSPOLICYD_RATE_DEGRADED=1.50
HOST_STATE_ENV="ORGASMIC_HOST_STATE_SAMPLE"
HOST_STATE_STAMP_PREFIX="# orgasmic-host-state:"
SAMPLE_HOST_ONLY=0

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
        --sample-host)
            # Live sampler probe for the self-test: print one snapshot and exit.
            # Does not run cargo and does not consult ORGASMIC_HOST_STATE_SAMPLE.
            SAMPLE_HOST_ONLY=1
            shift
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

# --sample-host skips registry work; everything else needs a clean registry.
if [ "$SAMPLE_HOST_ONLY" -eq 0 ]; then
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
fi

# ---------------------------------------------------------------------------
# host state (TASK-STWVB / TASK-STWVB.1)
# ---------------------------------------------------------------------------

# Field extractors: empty becomes `?` so a blank can never look measured.
host_field() {
    local sample="$1" key="$2"
    printf '%s' "$sample" | awk -v key="$key" '{
        for (i = 1; i <= NF; i++) {
            if ($i ~ ("^" key "=")) {
                sub("^" key "=", "", $i)
                print $i
                exit
            }
        }
    }'
}

host_field_or_unknown() {
    local v
    v=$(host_field "$1" "$2")
    if [ -n "$v" ]; then
        printf '%s' "$v"
    else
        printf '?'
    fi
}

# macOS `ps -o time=` is [H:]MM:SS[.ss] with minutes allowed to exceed 59.
syspolicyd_time_to_seconds() {
    local t="$1"
    awk -v t="$t" 'BEGIN {
        if (t == "" || t == "?") { print "?"; exit }
        n = split(t, a, /:/)
        if (n == 1 && a[1] + 0 == a[1]) { printf "%.2f", a[1] + 0; exit }
        if (n == 2 && a[1] + 0 == a[1] && a[2] + 0 == a[2]) {
            printf "%.2f", a[1] * 60 + a[2]; exit
        }
        if (n == 3 && a[1] + 0 == a[1] && a[2] + 0 == a[2] && a[3] + 0 == a[3]) {
            printf "%.2f", a[1] * 3600 + a[2] * 60 + a[3]; exit
        }
        print "?"
    }'
}

sample_load_live() {
    local load=""
    if [ -r /proc/loadavg ]; then
        load=$(awk '{ print $1 }' /proc/loadavg 2>/dev/null) || load=""
    elif command -v sysctl >/dev/null 2>&1; then
        load=$(sysctl -n vm.loadavg 2>/dev/null | awk '{ print $2 }') || load=""
    fi
    if [ -n "$load" ] && [ "$load" != "?" ]; then
        printf '%s' "$load"
    else
        printf '?'
    fi
}

# Cumulative CPU-time string for syspolicyd (pid via pgrep -x), or `?`.
sample_syspolicyd_time_live() {
    local pid="" t=""
    if ! command -v pgrep >/dev/null 2>&1 || ! command -v ps >/dev/null 2>&1; then
        printf '?'
        return
    fi
    pid=$(pgrep -x syspolicyd 2>/dev/null | head -n1) || pid=""
    if [ -z "$pid" ]; then
        printf '?'
        return
    fi
    t=$(ps -o time= -p "$pid" 2>/dev/null | tr -d '[:space:]') || t=""
    if [ -n "$t" ]; then
        printf '%s' "$t"
    else
        printf '?'
    fi
}

# Snapshot: `load=<f|?> syspolicyd_time=<t|?>`
sample_host_snapshot_live() {
    printf 'load=%s syspolicyd_time=%s' "$(sample_load_live)" "$(sample_syspolicyd_time_live)"
}

# Judgment sample from before/after snapshots + wall seconds:
# `load=<BEFORE load> syspolicyd_cpu=<delta seconds|?> wall_s=<wall|?>`
host_judgment_from_snapshots() {
    local before="$1" after="$2" wall_s="${3:-?}"
    local load t0 t1 d0 d1 delta
    load=$(host_field_or_unknown "$before" load)
    t0=$(host_field_or_unknown "$before" syspolicyd_time)
    t1=$(host_field_or_unknown "$after" syspolicyd_time)
    d0=$(syspolicyd_time_to_seconds "$t0")
    d1=$(syspolicyd_time_to_seconds "$t1")
    if [ "$d0" != "?" ] && [ "$d1" != "?" ]; then
        delta=$(awk -v a="$d0" -v b="$d1" 'BEGIN {
            d = b - a
            if (d < 0) d = 0
            printf "%.1f", d
        }')
    else
        delta="?"
    fi
    printf 'load=%s syspolicyd_cpu=%s wall_s=%s' "$load" "$delta" "$wall_s"
}

# Parse ORGASMIC_HOST_STATE_SAMPLE=load=<f>,syspolicyd_cpu=<f>,wall_s=<f>
# (comma or space). Values are the judgment sample: BEFORE load + cumulative
# delta seconds + wall seconds of the window.
sample_host_state_injected() {
    local raw="${ORGASMIC_HOST_STATE_SAMPLE-}" load="?" cpu="?" wall="?" part key val
    raw=$(printf '%s' "$raw" | tr ',' ' ')
    for part in $raw; do
        key=${part%%=*}
        val=${part#*=}
        case "$key" in
            load) load=$val ;;
            syspolicyd_cpu) cpu=$val ;;
            wall_s) wall=$val ;;
        esac
    done
    [ -n "$load" ] || load="?"
    [ -n "$cpu" ] || cpu="?"
    [ -n "$wall" ] || wall="?"
    printf 'load=%s syspolicyd_cpu=%s wall_s=%s' "$load" "$cpu" "$wall"
}

# Rate (CPU s / wall s) from a judgment sample, or `?` when either side is
# unknown / non-numeric / wall_s <= 0.
host_syspolicyd_rate() {
    local sample="$1" cpu wall
    cpu=$(host_field_or_unknown "$sample" syspolicyd_cpu)
    wall=$(host_field_or_unknown "$sample" wall_s)
    awk -v cpu="$cpu" -v wall="$wall" '
        function num(x) { return (x != "" && x != "?" && x + 0 == x) }
        BEGIN {
            if (num(cpu) && num(wall) && wall + 0 > 0) {
                printf "%.4f", cpu / wall
            } else {
                print "?"
            }
        }'
}

# True (exit 0) when the primary syspolicyd *rate* clears its threshold.
# Load is corroborating only — never an independent trigger (F-C).
# Unknown (`?`) signals do not trip the gate — absence of evidence is not
# evidence of thrash.
host_is_degraded() {
    local sample="$1" rate
    rate=$(host_syspolicyd_rate "$sample")
    awk -v rate="$rate" -v rate_lim="$SYSPOLICYD_RATE_DEGRADED" '
        function num(x) { return (x != "" && x != "?" && x + 0 == x) }
        BEGIN {
            if (num(rate) && rate + 0 >= rate_lim + 0) exit 0
            exit 1
        }'
}

# True when any judgment field parsed as a number (measured something).
host_judgment_measured() {
    local sample="$1" load cpu wall
    load=$(host_field_or_unknown "$sample" load)
    cpu=$(host_field_or_unknown "$sample" syspolicyd_cpu)
    wall=$(host_field_or_unknown "$sample" wall_s)
    awk -v load="$load" -v cpu="$cpu" -v wall="$wall" '
        function num(x) { return (x != "" && x != "?" && x + 0 == x) }
        BEGIN {
            if (num(load) || num(cpu) || num(wall)) exit 0
            exit 1
        }'
}

write_host_stamp() {
    local log="$1" before="$2" after="$3" judgment="$4"
    printf '%s before=%s | after=%s | delta=%s\n' \
        "$HOST_STATE_STAMP_PREFIX" "$before" "$after" "$judgment" >> "$log"
}

# Read `# orgasmic-host-state:` from a suite log. Prints the delta= judgment
# sample, or empty when absent.
read_host_stamp_judgment() {
    local log="$1" line
    line=$(grep -E "^${HOST_STATE_STAMP_PREFIX}" "$log" 2>/dev/null | tail -n1) || line=""
    [ -n "$line" ] || return 1
    printf '%s' "$line" | awk '{
        for (i = 1; i <= NF; i++) {
            if ($i ~ /^delta=/) {
                sub(/^delta=/, "", $i)
                # delta=load=X syspolicyd_cpu=Y — rest of fields follow
                out = $i
                for (j = i + 1; j <= NF; j++) {
                    if ($j ~ /^(before=|after=|delta=)/) break
                    if ($j ~ /^\|/) break
                    out = out " " $j
                }
                print out
                exit
            }
        }
        exit 1
    }'
}

read_host_stamp_field() {
    local log="$1" which="$2" # before|after
    local line
    line=$(grep -E "^${HOST_STATE_STAMP_PREFIX}" "$log" 2>/dev/null | tail -n1) || line=""
    [ -n "$line" ] || return 1
    printf '%s' "$line" | awk -v which="$which" '{
        key = which "="
        for (i = 1; i <= NF; i++) {
            if (index($i, key) == 1) {
                sub("^" key, "", $i)
                out = $i
                for (j = i + 1; j <= NF; j++) {
                    if ($j ~ /^(before=|after=|delta=)/) break
                    if ($j ~ /^\|/) break
                    out = out " " $j
                }
                print out
                exit
            }
        }
        exit 1
    }'
}

if [ "$SAMPLE_HOST_ONLY" -eq 1 ]; then
    sample_host_snapshot_live
    printf '\n'
    exit 0
fi

HOST_BEFORE=""
HOST_AFTER=""
HOST_JUDGMENT=""
HOST_DEGRADED=0
HOST_UNKNOWN=0

# ---------------------------------------------------------------------------
# run
# ---------------------------------------------------------------------------

SUITE_LOG="$WORK/suite.log"

if [ -n "$CLASSIFY_LOG" ]; then
    [ -f "$CLASSIFY_LOG" ] || die "no such log: $CLASSIFY_LOG"
    cp "$CLASSIFY_LOG" "$SUITE_LOG" || die "cannot copy $CLASSIFY_LOG"
    SUITE_CMD="(reclassified from $CLASSIFY_LOG)"
    SUITE_EXIT="?"
    # F3: reclassify-time sampling is unrelated to the run that produced the
    # log. Prefer a stamp written by the live run; otherwise host is unknown
    # and LOAD-SENSITIVE is unavailable (unknown must not mint an excuse).
    if HOST_JUDGMENT=$(read_host_stamp_judgment "$SUITE_LOG"); then
        HOST_BEFORE=$(read_host_stamp_field "$SUITE_LOG" before) || HOST_BEFORE="load=? syspolicyd_time=?"
        HOST_AFTER=$(read_host_stamp_field "$SUITE_LOG" after) || HOST_AFTER="load=? syspolicyd_time=?"
        if host_is_degraded "$HOST_JUDGMENT"; then
            HOST_DEGRADED=1
        fi
    else
        HOST_UNKNOWN=1
        HOST_DEGRADED=0
        HOST_BEFORE="load=? syspolicyd_time=?"
        HOST_AFTER="load=? syspolicyd_time=?"
        HOST_JUDGMENT="load=? syspolicyd_cpu=?"
    fi
elif [ -n "${ORGASMIC_HOST_STATE_SAMPLE-}" ]; then
    # Self-test injector on the live path: force the judgment sample.
    HOST_JUDGMENT=$(sample_host_state_injected)
    HOST_BEFORE=$(printf 'load=%s syspolicyd_time=injected' "$(host_field_or_unknown "$HOST_JUDGMENT" load)")
    HOST_AFTER=$HOST_BEFORE
    SUITE_CMD="cargo test ${CARGO_ARGS[*]} --no-fail-fast -- --skip $BILLED_TEST"
    printf 'run-tests: %s\n' "$SUITE_CMD"
    printf 'run-tests: log %s\n' "$SUITE_LOG"
    printf 'run-tests: host sample injected via %s\n' "$HOST_STATE_ENV"
    "${SCRUB[@]}" cargo test ${CARGO_ARGS[@]+"${CARGO_ARGS[@]}"} --no-fail-fast \
        -- --skip "$BILLED_TEST" > "$SUITE_LOG" 2>&1
    SUITE_EXIT=$?
    write_host_stamp "$SUITE_LOG" "$HOST_BEFORE" "$HOST_AFTER" "$HOST_JUDGMENT"
    if host_is_degraded "$HOST_JUDGMENT"; then
        HOST_DEGRADED=1
    fi
else
    # `--no-fail-fast` because a classification needs the WHOLE failure list;
    # stopping at the first red binary is how a real failure hides behind a
    # known flake. Output goes to a file, never a pipe: a test that leaves a
    # descendant holding the write end makes a pipe hang forever after the
    # suite has already passed (.orgasmic/gotchas.org).
    local_wall0=$(date +%s)
    HOST_BEFORE=$(sample_host_snapshot_live)
    SUITE_CMD="cargo test ${CARGO_ARGS[*]} --no-fail-fast -- --skip $BILLED_TEST"
    printf 'run-tests: %s\n' "$SUITE_CMD"
    printf 'run-tests: log %s\n' "$SUITE_LOG"
    "${SCRUB[@]}" cargo test ${CARGO_ARGS[@]+"${CARGO_ARGS[@]}"} --no-fail-fast \
        -- --skip "$BILLED_TEST" > "$SUITE_LOG" 2>&1
    SUITE_EXIT=$?
    HOST_AFTER=$(sample_host_snapshot_live)
    local_wall1=$(date +%s)
    local_wall=$((local_wall1 - local_wall0))
    if [ "$local_wall" -lt 1 ]; then
        local_wall=1
    fi
    HOST_JUDGMENT=$(host_judgment_from_snapshots "$HOST_BEFORE" "$HOST_AFTER" "$local_wall")
    write_host_stamp "$SUITE_LOG" "$HOST_BEFORE" "$HOST_AFTER" "$HOST_JUDGMENT"
    if host_is_degraded "$HOST_JUDGMENT"; then
        HOST_DEGRADED=1
    fi
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
LOAD_REPORT="$WORK/load.txt"
: > "$REAL_REPORT"
: > "$FLAKE_REPORT"
: > "$LOAD_REPORT"
REAL_COUNT=0
FLAKE_COUNT=0
LOAD_COUNT=0

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
        # C's interlock (TASK-STWVB): an unregistered failure that is green in
        # isolation is LOAD-SENSITIVE only when THIS run's measured host state
        # was degraded. On a calm host the same shape stays REAL — the registry
        # remains the only sanctioned excuse, and a thrashing host cannot mint
        # a permanent one. Failing alone is always REAL regardless of load.
        if [ "$iso" -eq 0 ] && [ "$HOST_DEGRADED" -eq 1 ]; then
            LOAD_COUNT=$((LOAD_COUNT + 1))
            {
                printf '  %s\n' "$name"
                printf '      binary   : %s\n' "$bin"
                printf '      why      : LOAD-SENSITIVE — failed under parallelism, green alone,\n'
                printf '                 and this run measured a degraded host (not a registry excuse)\n'
                printf '      isolation: passed\n'
                printf '      panic    : %s\n' "$(first_panic "$detail")"
                printf '      next     : re-run when calm; do NOT register this — there is no owner\n'
            } >> "$LOAD_REPORT"
            return
        fi
        REAL_COUNT=$((REAL_COUNT + 1))
        {
            printf '  %s\n' "$name"
            printf '      binary   : %s\n' "$bin"
            printf '      why      : NOT IN THE REGISTRY — no entry claims this failure\n'
            printf '      isolation: %s\n' "$(iso_word "$iso")"
            printf '      panic    : %s\n' "$(first_panic "$detail")"
            if [ "$iso" -eq 0 ]; then
                printf '      note     : green alone on a calm host is still REAL until owned\n'
            fi
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
# Host-state stamp: always printed so a green on a thrashing host cannot look
# like a trusted clean run. Judgment uses syspolicyd rate (CPU s / wall s);
# load is corroborating display only. Summary word is unknown when nothing
# parsed as a number — not calm (F-E).
HOST_RATE=$(host_syspolicyd_rate "${HOST_JUDGMENT:-load=? syspolicyd_cpu=? wall_s=?}")
if [ "$HOST_UNKNOWN" -eq 1 ]; then
    printf '  host     : unknown (reclassify with no host stamp in log; LOAD-SENSITIVE unavailable)\n'
elif [ "$HOST_DEGRADED" -eq 1 ]; then
    printf '  host     : DEGRADED (threshold syspolicyd_rate>=%s; load corroborating only, display>=%s)\n' \
        "$SYSPOLICYD_RATE_DEGRADED" "$LOAD_DEGRADED_THRESHOLD"
elif host_judgment_measured "${HOST_JUDGMENT:-load=? syspolicyd_cpu=? wall_s=?}"; then
    printf '  host     : calm (threshold syspolicyd_rate>=%s; load corroborating only, display>=%s)\n' \
        "$SYSPOLICYD_RATE_DEGRADED" "$LOAD_DEGRADED_THRESHOLD"
else
    printf '  host     : unknown (no numeric host signal; LOAD-SENSITIVE unavailable)\n'
fi
printf '             before  %s\n' "$HOST_BEFORE"
printf '             after   %s\n' "${HOST_AFTER:-$HOST_BEFORE}"
printf '             delta   %s\n' "${HOST_JUDGMENT:-load=? syspolicyd_cpu=? wall_s=?}"
printf '             rate    %s\n' "$HOST_RATE"

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
if [ "$LOAD_COUNT" -gt 0 ]; then
    printf '\nLOAD-SENSITIVE (%s) — isolation-green, unregistered, host was degraded:\n' \
        "$LOAD_COUNT"
    cat "$LOAD_REPORT"
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
    # F4: an alone-red failure is a code fact whatever the host was doing.
    # The host stamp above is reported alongside this verdict, not instead of it.
    printf '\nverdict: RED — %s real failure(s). This red means something.\n' "$REAL_COUNT"
    STATUS=$EXIT_REAL
elif [ "$LOAD_COUNT" -gt 0 ] && [ "$HOST_DEGRADED" -eq 0 ]; then
    # Unreachable when the C interlock holds: LOAD-SENSITIVE requires a
    # degraded host. If this fires, the host-state gate was bypassed.
    printf '\nverdict: RED — %s load-sensitive label(s) on a calm host. Interlock broken.\n' \
        "$LOAD_COUNT"
    STATUS=$EXIT_REAL
elif [ "$SUITE_EXIT" != "?" ] && [ "$SUITE_EXIT" != 0 ]; then
    # B2: a non-zero cargo exit with no per-test failure list is a code fact
    # (crashed binary, etc.). Do not report it as "suite looked green" under
    # HOST_DEGRADED — the same one-line move F4 made for REAL_COUNT.
    printf '\nverdict: RED — cargo exited %s with no per-test failure list. Read %s.\n' \
        "$SUITE_EXIT" "$SUITE_LOG"
    STATUS=$EXIT_REAL
elif [ "$HOST_DEGRADED" -eq 1 ]; then
    # Degraded host without an alone-red REAL: green / flake / load-sensitive
    # are all untrusted. Re-run when calm.
    printf '\nverdict: INCONCLUSIVE — re-run when calm. Host was degraded'
    if [ "$LOAD_COUNT" -gt 0 ]; then
        printf ' (%s load-sensitive failure(s); not a registry excuse).\n' "$LOAD_COUNT"
    elif [ "$FAIL_COUNT" -gt 0 ]; then
        printf ' (failures were registered flakes; still not a trusted green).\n'
    else
        printf ' (suite looked green; that green is not trusted).\n'
    fi
    STATUS=$EXIT_INCONCLUSIVE
elif [ "$FAIL_COUNT" -gt 0 ]; then
    printf '\nverdict: GREEN modulo %s registered flake(s). No unexplained red.\n' "$FLAKE_COUNT"
else
    printf '\nverdict: GREEN — no failures.\n'
fi
printf '%s\n' "$RULE"
exit "$STATUS"
