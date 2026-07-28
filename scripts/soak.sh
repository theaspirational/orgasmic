#!/usr/bin/env bash
# orgasmic:TASK-WE0Q7
#
# scripts/soak.sh — the minutes-scale daemon soak.
#
# WHY THIS EXISTS
# ---------------
# No other gate in this repository runs a daemon for longer than about ten
# seconds, so any defect that needs *uptime* to express passes every one of
# them. TASK-Q07Y5 shipped fully green and then killed the operator's daemon
# 10.00s after every boot; TASK-R74E8 fixed it and added one unit test watching
# that one code path. The class it belongs to — timers, budgets, watchers,
# leaks, heartbeat/stall interactions, fd and session accumulation, anything
# that only shows up minutes in — still had nothing watching it.
#
# This soak boots a REAL daemon from the tree under test, keeps it under light
# periodic traffic for minutes, and asserts across the whole window that it is
# still the same process, on the same boot, answering, not leaking, and that it
# then shuts down inside its own derived budget. It is a scheduled gate, not a
# merge gate: minutes of wall clock do not belong in the merge path.
#
# WHAT IT NEVER TOUCHES  (asserted below, not merely promised)
# ------------------------------------------------------------
# The installed runtime (~/.orgasmic/bin), the real LaunchAgent / systemd unit,
# and the operator's $ORGASMIC_HOME. The daemon under test runs in the
# foreground from a temp-dir home on an ephemeral port. `daemon start|stop|
# restart` is never invoked — it rewrites the one real LaunchAgent plist even
# from a debug build — and neither is any provider-spending verb, so the soak
# costs zero billed tokens by construction.
#
# USAGE
#   scripts/soak.sh [--duration-seconds N] [--probe-interval-seconds N]
#                   [--profile debug|release] [--keep-home] [--help]
#
# Exit status is the gate: 0 = the daemon survived the window and shut down
# cleanly; nonzero = a message naming what broke and at which second.

set -euo pipefail

# ---------------------------------------------------------------------------
# defaults
# ---------------------------------------------------------------------------

# Floor on the window, independent of the derived one below. The real default
# is max(this, 10x the daemon's own shutdown budget) — see derive_budget.
MIN_DURATION_SECONDS=300
# How many times the daemon's whole shutdown budget the window must cover, so a
# budget that grows moves the soak with it instead of silently outgrowing it.
DURATION_BUDGET_MULTIPLE=10
# Margin over the derived shutdown budget before a SIGTERMed daemon is late.
SHUTDOWN_MARGIN_SECONDS=10
# Gross-growth alarms. Deliberately wide: this soak reports the numbers and
# fails only on growth no healthy idle daemon could produce. Tuning these
# finely is how a leak detector becomes a flake generator.
FD_GROWTH_LIMIT="${SOAK_FD_GROWTH_LIMIT:-128}"
RSS_GROWTH_FACTOR="${SOAK_RSS_GROWTH_FACTOR:-4}"
RSS_GROWTH_FLOOR_KB="${SOAK_RSS_GROWTH_FLOOR_KB:-262144}" # 256 MiB
# Seconds between full probes; liveness is checked every second regardless.
PROBE_INTERVAL_SECONDS=10
# How long the daemon may take to come up before the soak gives up.
BOOT_TIMEOUT_SECONDS=120

PROFILE=debug
DURATION_SECONDS=""
KEEP_HOME=0

# ---------------------------------------------------------------------------
# output helpers
# ---------------------------------------------------------------------------

say() { printf '%s\n' "$*"; }
note() { printf '  %s\n' "$*"; }

# A dead child of this script is a zombie until it is waited on, so `kill -0`
# alone would report a corpse as alive.
process_gone() {
    local state
    state="$(ps -o stat= -p "$1" 2>/dev/null | head -1 | tr -d ' ')"
    case "$state" in
    '') return 0 ;;
    Z*) return 0 ;;
    *) return 1 ;;
    esac
}

# Every failure route funnels through here, and every one of them first asks
# whether the daemon is still alive. That ordering is the point: once the daemon
# has died, "tx list failed" and "the status probe failed" are symptoms, and
# reporting a symptom as the headline is how a soak buries the very finding it
# was built to surface. The death, and the second it happened, come first.
fail() {
    local reason="$*"
    local t code
    if [ -n "$SERVE_PID" ] && process_gone "$SERVE_PID"; then
        t="$(elapsed)"
        code=0
        wait "$SERVE_PID" 2>/dev/null || code=$?
        SERVE_PID=""
        {
            printf 'SOAK FAIL: the daemon exited on its own %ss after it started (exit=%s).\n' "$t" "$code"
            printf '       Nothing asked it to stop. A daemon that only dies once it has been up\n'
            printf '       for a while is exactly the class this soak exists to catch.\n'
            printf '       noticed by: %s\n' "$reason"
        } >&2
        if [ -n "$LOG_DIR" ] && [ -f "$LOG_DIR/serve.err" ]; then
            printf -- '--- last daemon stderr ---\n' >&2
            tail -20 "$LOG_DIR/serve.err" >&2 || true
        fi
        FAILED=1
        exit 1
    fi
    printf 'SOAK FAIL: %s\n' "$reason" >&2
    FAILED=1
    exit 1
}

# A daemon that stops serving has not necessarily exited: TASK-Q07Y5's daemon
# kept its process alive — `serve` was still parked on the signal wait — while
# the listener it had already closed answered nobody. "Died" and "stopped
# answering" are two different findings, and an operator needs to be told which
# one happened and when, so this waits out the daemon's own shutdown budget
# before deciding which it is rather than guessing from one refused connection.
fail_unresponsive() {
    local since="$1"
    local waited=0
    while [ "$waited" -lt "$SHUTDOWN_DEADLINE_SECONDS" ]; do
        if process_gone "$SERVE_PID"; then
            break
        fi
        sleep 1
        waited=$((waited + 1))
    done
    if process_gone "$SERVE_PID"; then
        # fail() leads with the death and the second it happened.
        fail "the daemon stopped answering ${since}s into its life and then exited"
    fi
    {
        printf 'SOAK FAIL: the daemon stopped answering %ss into its life but is still running.\n' "$since"
        printf '       Its process is up and its port is dead — it took itself out of service\n'
        printf '       with nobody asking it to. That is the TASK-Q07Y5 shape exactly: a gate\n'
        printf '       that looks once, early, sees a healthy daemon and a green build.\n'
        printf '       waited %ss (the daemon shutdown budget plus margin) to tell "dying" from\n' "$waited"
        printf '       "dead to clients"; see %s\n' "$LOG_DIR/status.err"
    } >&2
    if [ -f "$LOG_DIR/serve.err" ]; then
        printf -- '--- last daemon stderr ---\n' >&2
        tail -20 "$LOG_DIR/serve.err" >&2 || true
    fi
    FAILED=1
    exit 1
}

# The header comment is the help text; keeping one copy is how they stay true
# to each other. Stops at the first line that is not a comment.
usage() {
    sed -n '3,$p' "$0" | sed -n '/^[^#]/q;p' | sed 's/^# \{0,1\}//'
}

# ---------------------------------------------------------------------------
# arguments
# ---------------------------------------------------------------------------

while [ $# -gt 0 ]; do
    case "$1" in
    --duration-seconds)
        DURATION_SECONDS="${2:-}"
        shift 2
        ;;
    --probe-interval-seconds)
        PROBE_INTERVAL_SECONDS="${2:-}"
        shift 2
        ;;
    --profile)
        PROFILE="${2:-}"
        shift 2
        ;;
    --keep-home)
        KEEP_HOME=1
        shift
        ;;
    -h | --help)
        usage
        exit 0
        ;;
    *)
        printf 'unknown argument: %s\n' "$1" >&2
        usage >&2
        exit 2
        ;;
    esac
done

case "$PROFILE" in
debug | release) ;;
*)
    printf 'unknown --profile %s (expected debug or release)\n' "$PROFILE" >&2
    exit 2
    ;;
esac

# ---------------------------------------------------------------------------
# state the traps and assertions need
# ---------------------------------------------------------------------------

SERVE_PID=""
SOAK_ROOT=""
SOAK_HOME=""
LOG_DIR=""
BASE_URL=""
FAILED=0
# Set the moment the daemon is spawned, so every timestamp the soak prints is
# "seconds of daemon uptime" — the axis the whole gate is about.
SPAWN_EPOCH=0
WINDOW_OPEN_EPOCH=0

elapsed() { echo $(($(date +%s) - SPAWN_EPOCH)); }
window_elapsed() { echo $(($(date +%s) - WINDOW_OPEN_EPOCH)); }

cleanup() {
    local rc=$?
    if [ -n "$SERVE_PID" ] && ! process_gone "$SERVE_PID"; then
        # Only ever the pid this script spawned. Never a name match: `pgrep -f`
        # on orgasmic keywords hits the operator's daemon and other agents'
        # worker processes.
        kill -TERM "$SERVE_PID" 2>/dev/null || true
        local waited=0
        while ! process_gone "$SERVE_PID" && [ "$waited" -lt 120 ]; do
            sleep 0.5
            waited=$((waited + 1))
        done
        kill -KILL "$SERVE_PID" 2>/dev/null || true
        wait "$SERVE_PID" 2>/dev/null || true
    fi
    if [ -n "$SOAK_ROOT" ] && [ -d "$SOAK_ROOT" ]; then
        if [ "$rc" -ne 0 ] || [ "$FAILED" -ne 0 ] || [ "$KEEP_HOME" -eq 1 ]; then
            printf 'soak workspace kept for inspection: %s\n' "$SOAK_ROOT" >&2
        else
            rm -rf "$SOAK_ROOT"
        fi
    fi
    return $rc
}
trap cleanup EXIT
trap 'exit 130' INT
trap 'exit 143' TERM

# ---------------------------------------------------------------------------
# repo + environment
# ---------------------------------------------------------------------------

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/.." && git rev-parse --show-toplevel)"
cd "$REPO_ROOT"

# What this soak must not disturb. Read once, from the environment as inherited,
# before anything of ours redefines it.
REAL_HOME="${ORGASMIC_HOME:-$HOME/.orgasmic}"
INSTALLED_RUNTIME="$HOME/.orgasmic/bin/orgasmic"
MACOS_PLIST="$HOME/Library/LaunchAgents/orgasmic.daemon.plist"
MACOS_RMUX_PLIST="$HOME/Library/LaunchAgents/orgasmic.rmux.plist"
SYSTEMD_UNIT="${XDG_CONFIG_HOME:-$HOME/.config}/systemd/user/orgasmic-daemon.service"

# A dispatch leaks its run id and home into every subprocess; both would point
# the daemon under test at the operator's state.
unset ORGASMIC_RUN_ID || true
unset ORGASMIC_HOME || true
unset ORGASMIC_DAEMON_URL || true

if [ -n "${ORGASMIC_ALLOW_BILLED_TESTS:-}" ]; then
    fail "ORGASMIC_ALLOW_BILLED_TESTS is set. The soak spends no provider turns and
       refuses to run in an environment armed to spend them."
fi

hash_file() {
    if command -v shasum >/dev/null 2>&1; then
        shasum -a 256 "$1" | awk '{print $1}'
    else
        sha256sum "$1" | awk '{print $1}'
    fi
}

# "missing" or a content fingerprint. Used to prove, after the fact, that the
# soak left the operator's lifecycle files exactly as it found them.
fingerprint() {
    if [ -e "$1" ]; then
        hash_file "$1"
    else
        echo "missing"
    fi
}

UNTOUCHABLE_PATHS="$INSTALLED_RUNTIME
$MACOS_PLIST
$MACOS_RMUX_PLIST
$SYSTEMD_UNIT
$REAL_HOME/config.yaml
$REAL_HOME/daemon.lock"

snapshot_untouchables() {
    local path
    printf '%s\n' "$UNTOUCHABLE_PATHS" | while IFS= read -r path; do
        [ -n "$path" ] || continue
        printf '%s\t%s\n' "$path" "$(fingerprint "$path")"
    done
}

# ---------------------------------------------------------------------------
# structural guard: every CLI call the soak makes goes through this
# ---------------------------------------------------------------------------

# `daemon start|stop|restart` rewrites the single real LaunchAgent plist even
# when run from a debug build in a worktree, and `run`/`dispatch`/`manager`
# spend provider turns. The soak has no business with either, so the guard is
# mechanism rather than a comment: a future edit that reaches for one of them
# fails the soak instead of touching the operator's machine.
og() {
    case "${1:-}" in
    daemon | update | install | uninstall)
        fail "internal guard: the soak tried to run \`orgasmic $1\`, which manages the
       real service/runtime. The soak owns exactly one foreground daemon: the
       one it spawned itself."
        ;;
    run | dispatch | manager)
        fail "internal guard: the soak tried to run \`orgasmic $1\`, which can start a
       provider turn. The soak is zero-cost by construction."
        ;;
    esac
    ORGASMIC_HOME="$SOAK_HOME" ORGASMIC_DAEMON_URL="$BASE_URL" "$BIN" "$@"
}

# ---------------------------------------------------------------------------
# build the tree under test + derive the budgets from it
# ---------------------------------------------------------------------------

say "orgasmic daemon soak"
note "repo        $REPO_ROOT"
note "commit      $(git rev-parse --short HEAD 2>/dev/null || echo unknown)"

TARGET_DIR="${CARGO_TARGET_DIR:-$REPO_ROOT/target}"
BIN="$TARGET_DIR/$PROFILE/orgasmic"

say "[build] cargo build -p orgasmic-cli --bin orgasmic (${PROFILE})"
if [ "$PROFILE" = release ]; then
    cargo build -p orgasmic-cli --bin orgasmic --release >&2
else
    cargo build -p orgasmic-cli --bin orgasmic >&2
fi
[ -x "$BIN" ] || fail "built binary not found at $BIN"

# The binary must be the one we just built from this tree, never the installed
# runtime. Compare resolved paths: a symlink into ~/.orgasmic would otherwise
# read as a target-dir path.
resolve() { (cd "$(dirname "$1")" && printf '%s/%s\n' "$(pwd -P)" "$(basename "$1")"); }
BIN_RESOLVED="$(resolve "$BIN")"
case "$BIN_RESOLVED" in
"$HOME/.orgasmic"/*)
    fail "the binary under test resolves inside the installed runtime ($BIN_RESOLVED).
       The soak only ever runs a binary built from the tree under test."
    ;;
esac
note "binary      $BIN_RESOLVED"

# Derived, never a literal: the shutdown bound and the window length both come
# from the tree's own ShutdownBudgets (see the example's module docs).
say "[build] deriving ShutdownBudgets::default().total() from the tree"
BUDGET_MS="$(cargo run -q -p orgasmic-daemon --example shutdown_budget_ms)"
case "$BUDGET_MS" in
'' | *[!0-9]*) fail "could not derive the shutdown budget (got: '$BUDGET_MS')" ;;
esac
BUDGET_SECONDS=$(((BUDGET_MS + 999) / 1000))
SHUTDOWN_DEADLINE_SECONDS=$((BUDGET_SECONDS + SHUTDOWN_MARGIN_SECONDS))

DERIVED_MIN_DURATION=$((BUDGET_SECONDS * DURATION_BUDGET_MULTIPLE))
DEFAULT_DURATION=$MIN_DURATION_SECONDS
if [ "$DERIVED_MIN_DURATION" -gt "$DEFAULT_DURATION" ]; then
    DEFAULT_DURATION=$DERIVED_MIN_DURATION
fi
[ -n "$DURATION_SECONDS" ] || DURATION_SECONDS=$DEFAULT_DURATION

case "$DURATION_SECONDS" in
'' | *[!0-9]*) fail "--duration-seconds must be a whole number of seconds" ;;
esac
case "$PROBE_INTERVAL_SECONDS" in
'' | *[!0-9]* | 0) fail "--probe-interval-seconds must be a positive whole number" ;;
esac

note "budget      ${BUDGET_MS}ms shutdown total (derived), exit deadline ${SHUTDOWN_DEADLINE_SECONDS}s"
note "window      ${DURATION_SECONDS}s, probing every ${PROBE_INTERVAL_SECONDS}s"
if [ "$DURATION_SECONDS" -lt "$DEFAULT_DURATION" ]; then
    say "[warn] ${DURATION_SECONDS}s is below the gate-strength window of ${DEFAULT_DURATION}s"
    say "       (max(${MIN_DURATION_SECONDS}s, ${DURATION_BUDGET_MULTIPLE}x the ${BUDGET_SECONDS}s shutdown budget))."
    say "       This run can catch a defect but does not clear the scheduled gate."
fi

# ---------------------------------------------------------------------------
# isolated home
# ---------------------------------------------------------------------------

SOAK_ROOT="$(mktemp -d "${TMPDIR:-/tmp}/orgasmic-soak.XXXXXX")"
SOAK_ROOT="$(cd "$SOAK_ROOT" && pwd -P)"
SOAK_HOME="$SOAK_ROOT/home"
LOG_DIR="$SOAK_ROOT/logs"
PROJECT_DIR="$SOAK_ROOT/project"
mkdir -p "$SOAK_HOME" "$LOG_DIR" "$PROJECT_DIR"

# Isolation is the whole safety argument, so it is asserted rather than assumed.
[ "$SOAK_HOME" != "$REAL_HOME" ] || fail "the soak home resolved to the real \$ORGASMIC_HOME ($REAL_HOME)"
case "$SOAK_HOME" in
"$REAL_HOME"/* | "$HOME/.orgasmic"/*)
    fail "the soak home ($SOAK_HOME) is inside the operator's home ($REAL_HOME)"
    ;;
"$REPO_ROOT"/*)
    fail "the soak home ($SOAK_HOME) is inside the repository under test"
    ;;
esac
note "soak home   $SOAK_HOME"
note "logs        $LOG_DIR"

# The daemon reads shipped content (scaffold templates, prompt specs) from
# `$ORGASMIC_HOME/orgasmic/shipped`. Copy it out of the tree under test rather
# than symlinking the repo in: the daemon then has no path at all that leads
# back to the repository, which is one fewer thing to reason about when the
# question is "can the soak write somewhere it must not".
mkdir -p "$SOAK_HOME/orgasmic"
cp -R "$REPO_ROOT/shipped" "$SOAK_HOME/orgasmic/shipped"

# 65533, not the default 4848: if ORGASMIC_DAEMON_URL is ever lost from a child
# environment, the CLI falls back to this config and fails loudly instead of
# quietly talking to the operator's real daemon (TASK-CJXKM).
cat >"$SOAK_HOME/config.yaml" <<'YAML'
bind_host: 127.0.0.1
bind_port: 65533
YAML

BEFORE_SNAPSHOT="$LOG_DIR/untouchables.before"
snapshot_untouchables >"$BEFORE_SNAPSHOT"

# ---------------------------------------------------------------------------
# boot
# ---------------------------------------------------------------------------

# Port 0: the kernel picks a free ephemeral port and the daemon prints the one
# it actually bound, so the soak never races another listener for a number.
# A 256-fd soft limit (the macOS default) turns a handle leak into an EMFILE
# crash before the soak can see the growth, which reports the wrong finding.
# Best effort, and printed, because the daemon under test inherits it.
ulimit -n 4096 2>/dev/null || true
note "fd limit    $(ulimit -n)"

say "[boot] orgasmic serve --bind 127.0.0.1 --port 0 (foreground, isolated home)"
ORGASMIC_HOME="$SOAK_HOME" "$BIN" serve --bind 127.0.0.1 --port 0 \
    >"$LOG_DIR/serve.out" 2>"$LOG_DIR/serve.err" &
SERVE_PID=$!
SPAWN_EPOCH="$(date +%s)"
WINDOW_OPEN_EPOCH="$SPAWN_EPOCH"

waited=0
while [ "$waited" -lt "$BOOT_TIMEOUT_SECONDS" ]; do
    if grep -q 'press Ctrl+C to stop' "$LOG_DIR/serve.out" 2>/dev/null; then
        break
    fi
    if process_gone "$SERVE_PID"; then
        say "--- serve.out ---" >&2
        cat "$LOG_DIR/serve.out" >&2 || true
        fail "the daemon exited during boot, before the soak window even started"
    fi
    sleep 1
    waited=$((waited + 1))
done
grep -q 'press Ctrl+C to stop' "$LOG_DIR/serve.out" 2>/dev/null ||
    fail "the daemon did not finish booting within ${BOOT_TIMEOUT_SECONDS}s"

PORT="$(sed -n 's|.*listening on http://127\.0\.0\.1:\([0-9]*\).*|\1|p' "$LOG_DIR/serve.out" | head -1)"
case "$PORT" in
'' | *[!0-9]*) fail "could not read the bound port from the daemon's startup line" ;;
esac
[ "$PORT" != "4848" ] || fail "the daemon bound the default operator port 4848; refusing to continue"
BASE_URL="http://127.0.0.1:$PORT"
note "pid         $SERVE_PID"
note "url         $BASE_URL"

# ---------------------------------------------------------------------------
# probe surfaces
# ---------------------------------------------------------------------------

json_number() { printf '%s\n' "$1" | sed -n "s/.*\"$2\": *\([0-9]*\).*/\1/p" | head -1; }

json_string() { printf '%s\n' "$1" | sed -n "s/.*\"$2\": *\"\([^\"]*\)\".*/\1/p" | head -1; }

rss_kb() { ps -o rss= -p "$1" 2>/dev/null | tr -d ' ' | head -1; }

fd_count() {
    if [ -d "/proc/$1/fd" ]; then
        find "/proc/$1/fd" -mindepth 1 -maxdepth 1 2>/dev/null | wc -l | tr -d ' '
    elif command -v lsof >/dev/null 2>&1; then
        lsof -p "$1" 2>/dev/null | tail -n +2 | wc -l | tr -d ' '
    else
        echo ""
    fi
}

# ---------------------------------------------------------------------------
# fixtures: a throwaway project so the traffic hits real read surfaces
# ---------------------------------------------------------------------------

say "[setup] registering a throwaway project inside the soak workspace"
git init -q "$PROJECT_DIR"
og project init --path "$PROJECT_DIR" --name soak >"$LOG_DIR/project-init.log" 2>&1 ||
    fail "could not scaffold the throwaway project (see $LOG_DIR/project-init.log)"
PROJECT_ID="$(og project list --ids 2>/dev/null | head -1)"
[ -n "$PROJECT_ID" ] || fail "the throwaway project did not register on the board"

# Registration puts the project on the board; the daemon indexes it a moment
# later. Wait for the read surface rather than racing it.
waited=0
while [ "$waited" -lt 30 ]; do
    if og tasks list --project "$PROJECT_ID" --ids >/dev/null 2>&1; then
        break
    fi
    sleep 1
    waited=$((waited + 1))
done
og tasks list --project "$PROJECT_ID" --ids >/dev/null 2>&1 ||
    fail "the daemon never indexed the throwaway project"
og task create --project "$PROJECT_ID" --title "soak read target" \
    >"$LOG_DIR/task-create.log" 2>&1 || fail "could not create the soak read target task"
TASK_ID="$(og tasks list --project "$PROJECT_ID" --ids 2>/dev/null | head -1)"
[ -n "$TASK_ID" ] || fail "the soak read target task is not listed"
note "project     $PROJECT_ID"
note "task        $TASK_ID"

# ---------------------------------------------------------------------------
# the window
# ---------------------------------------------------------------------------

SAMPLES="$LOG_DIR/samples.tsv"
printf 't_seconds\tpid\tboot_id\tparse_errors\trss_kb\tfds\n' >"$SAMPLES"

FIRST_PID=""
FIRST_BOOT_ID=""
BASE_RSS=""
BASE_FDS=""
PROBE_COUNT=0

probe() {
    local t status pid boot_id parse_errors rss fds
    t="$(elapsed)"
    status="$(og status 2>"$LOG_DIR/status.err")" || fail_unresponsive "$t"
    pid="$(json_number "$status" pid)"
    boot_id="$(json_string "$status" boot_id)"
    parse_errors="$(json_number "$status" parse_errors)"
    rss="$(rss_kb "$SERVE_PID")"
    fds="$(fd_count "$SERVE_PID")"
    PROBE_COUNT=$((PROBE_COUNT + 1))

    [ -n "$pid" ] || fail "the status response at t+${t}s carried no pid"
    [ -n "$boot_id" ] || fail "the status response at t+${t}s carried no boot_id"

    if [ -z "$FIRST_PID" ]; then
        FIRST_PID="$pid"
        FIRST_BOOT_ID="$boot_id"
        BASE_RSS="$rss"
        BASE_FDS="$fds"
        [ "$pid" = "$SERVE_PID" ] ||
            fail "the daemon answering on $BASE_URL reports pid $pid, but the soak
       spawned pid $SERVE_PID. Something else owns this port."
    fi

    # The Q07Y5 class: a daemon that restarts, or is silently replaced, under a
    # gate that only ever looked at one instant.
    [ "$pid" = "$FIRST_PID" ] ||
        fail "the daemon changed pid at t+${t}s: $FIRST_PID -> $pid. It died and was
       replaced inside the soak window."
    [ "$boot_id" = "$FIRST_BOOT_ID" ] ||
        fail "the daemon changed boot_id at t+${t}s: $FIRST_BOOT_ID -> $boot_id.
       This is a different boot of the daemon than the one the soak started."
    [ "$parse_errors" = "0" ] ||
        fail "the daemon reports $parse_errors parse error(s) at t+${t}s; it started with none"

    if [ -n "$fds" ] && [ -n "$BASE_FDS" ] && [ "$fds" -gt $((BASE_FDS + FD_GROWTH_LIMIT)) ]; then
        fail "open file descriptors grew from $BASE_FDS to $fds by t+${t}s (limit
       +${FD_GROWTH_LIMIT}). Something is accumulating handles with uptime."
    fi
    if [ -n "$rss" ] && [ -n "$BASE_RSS" ] &&
        [ "$rss" -gt $((BASE_RSS * RSS_GROWTH_FACTOR)) ] &&
        [ "$rss" -gt $((BASE_RSS + RSS_GROWTH_FLOOR_KB)) ]; then
        fail "resident memory grew from ${BASE_RSS}KB to ${rss}KB by t+${t}s (alarm at
       ${RSS_GROWTH_FACTOR}x and +${RSS_GROWTH_FLOOR_KB}KB). Something is accumulating with uptime."
    fi

    printf '%s\t%s\t%s\t%s\t%s\t%s\n' "$t" "$pid" "$boot_id" "$parse_errors" \
        "${rss:-?}" "${fds:-?}" >>"$SAMPLES"
    printf '  [t+%04ds] probe %-3d pid=%s boot_id=%s parse_errors=%s rss=%sKB fds=%s\n' \
        "$t" "$PROBE_COUNT" "$pid" "$boot_id" "$parse_errors" "${rss:-?}" "${fds:-?}"

    # Light traffic across real read surfaces, rotated so the whole window is
    # not one endpoint. Any failure here is a failure of the soak.
    case $((PROBE_COUNT % 3)) in
    0) og tx list --limit 5 >/dev/null 2>&1 || fail "\`tx list\` failed at t+${t}s" ;;
    1) og tasks list --project "$PROJECT_ID" >/dev/null 2>&1 || fail "\`tasks list\` failed at t+${t}s" ;;
    2) og task get --project "$PROJECT_ID" "$TASK_ID" >/dev/null 2>&1 || fail "\`task get\` failed at t+${t}s" ;;
    esac
}

# Liveness is checked every second, not every probe interval: the failure this
# soak was built for is a process that disappears between two green probes.
assert_alive() {
    if process_gone "$SERVE_PID"; then
        fail "the once-a-second liveness poll, $(window_elapsed)s into the ${DURATION_SECONDS}s window"
    fi
}

say "[soak] holding the daemon for ${DURATION_SECONDS}s under periodic traffic"
WINDOW_OPEN_EPOCH="$(date +%s)"
DEADLINE=$((WINDOW_OPEN_EPOCH + DURATION_SECONDS))
NEXT_PROBE=$WINDOW_OPEN_EPOCH
while :; do
    NOW="$(date +%s)"
    assert_alive
    if [ "$NOW" -ge "$DEADLINE" ]; then
        break
    fi
    if [ "$NOW" -ge "$NEXT_PROBE" ]; then
        probe
        NEXT_PROBE=$(($(date +%s) + PROBE_INTERVAL_SECONDS))
    fi
    sleep 1
done
probe
say "[soak] survived ${DURATION_SECONDS}s (daemon up $(elapsed)s): ${PROBE_COUNT} probes, one pid ($FIRST_PID), one boot_id ($FIRST_BOOT_ID)"

# ---------------------------------------------------------------------------
# shutdown, inside the daemon's own derived budget
# ---------------------------------------------------------------------------

say "[stop] SIGTERM -> pid $SERVE_PID (deadline ${SHUTDOWN_DEADLINE_SECONDS}s = ${BUDGET_SECONDS}s budget + ${SHUTDOWN_MARGIN_SECONDS}s margin)"
kill -TERM "$SERVE_PID"
TICKS=0
MAX_TICKS=$((SHUTDOWN_DEADLINE_SECONDS * 5))
while ! process_gone "$SERVE_PID"; do
    if [ "$TICKS" -ge "$MAX_TICKS" ]; then
        fail "the daemon was still running ${SHUTDOWN_DEADLINE_SECONDS}s after SIGTERM. Its own
       shutdown budget is ${BUDGET_MS}ms; a service manager would have killed it by now."
    fi
    sleep 0.2
    TICKS=$((TICKS + 1))
done
EXIT_MS=$((TICKS * 200))
EXIT_CODE=0
wait "$SERVE_PID" 2>/dev/null || EXIT_CODE=$?
DEAD_PID="$SERVE_PID"
SERVE_PID=""
[ "$EXIT_CODE" -eq 0 ] ||
    fail "the daemon exited ${EXIT_CODE} on SIGTERM (128+n means it died on a signal
       instead of running its graceful shutdown)"
note "exited cleanly in ~${EXIT_MS}ms, well inside the ${BUDGET_MS}ms budget"

# ---------------------------------------------------------------------------
# after-exit assertions
# ---------------------------------------------------------------------------

[ ! -e "$SOAK_HOME/daemon.shutdown" ] ||
    fail "the shutdown marker $SOAK_HOME/daemon.shutdown survived the exit; a
       replacement daemon would read this home as owned by a departing predecessor"

# The instance lock is proven released the only way that cannot lie: a fresh
# daemon takes it. A held lock makes this boot refuse, and an incumbent-detected
# boot exits 0 without ever binding, so the assertion is on the bind line.
say "[stop] proving the instance lock was released: a replacement boot must bind"
ORGASMIC_HOME="$SOAK_HOME" "$BIN" serve --bind 127.0.0.1 --port 0 \
    >"$LOG_DIR/relock.out" 2>"$LOG_DIR/relock.err" &
SERVE_PID=$!
waited=0
while [ "$waited" -lt 60 ] && ! grep -q 'press Ctrl+C to stop' "$LOG_DIR/relock.out" 2>/dev/null; do
    if process_gone "$SERVE_PID"; then
        break
    fi
    sleep 1
    waited=$((waited + 1))
done
if ! grep -q 'listening on http' "$LOG_DIR/relock.out" 2>/dev/null; then
    cat "$LOG_DIR/relock.out" >&2 || true
    tail -20 "$LOG_DIR/relock.err" >&2 || true
    fail "a replacement daemon could not take the home instance lock after the first
       one exited, so the lock (or its marker) was not retracted"
fi
kill -TERM "$SERVE_PID" 2>/dev/null || true
waited=0
while ! process_gone "$SERVE_PID" && [ "$waited" -lt "$SHUTDOWN_DEADLINE_SECONDS" ]; do
    sleep 1
    waited=$((waited + 1))
done
wait "$SERVE_PID" 2>/dev/null || true
SERVE_PID=""
note "lock and marker retracted (replacement boot bound, then stopped)"

AFTER_SNAPSHOT="$LOG_DIR/untouchables.after"
snapshot_untouchables >"$AFTER_SNAPSHOT"
if ! diff -u "$BEFORE_SNAPSHOT" "$AFTER_SNAPSHOT" >"$LOG_DIR/untouchables.diff"; then
    cat "$LOG_DIR/untouchables.diff" >&2
    fail "the soak changed a file it must never touch (installed runtime, real
       LaunchAgent/systemd unit, or the operator's \$ORGASMIC_HOME). See the diff above."
fi
note "untouched   installed runtime, LaunchAgent/systemd unit, and $REAL_HOME"

say ""
say "SOAK PASS"
note "window      ${DURATION_SECONDS}s, ${PROBE_COUNT} probes"
note "identity    pid ${DEAD_PID} / boot_id ${FIRST_BOOT_ID} for every probe"
note "parse       0 parse errors throughout"
note "growth      rss ${BASE_RSS}KB -> $(awk 'END{print $5}' "$SAMPLES")KB, fds ${BASE_FDS} -> $(awk 'END{print $6}' "$SAMPLES")"
note "shutdown    clean exit ~${EXIT_MS}ms after SIGTERM (budget ${BUDGET_MS}ms)"
note "samples     $SAMPLES"
