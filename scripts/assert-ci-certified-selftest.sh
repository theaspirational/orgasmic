#!/usr/bin/env bash

set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
ASSERT_SCRIPT="$ROOT/scripts/assert-ci-certified.sh"
TEST_SHA="0123456789abcdef0123456789abcdef01234567"
TMP="$(mktemp -d)"
trap 'rm -rf "$TMP"' EXIT

mkdir -p "$TMP/bin"
cat >"$TMP/bin/git" <<'FAKE_GIT'
#!/usr/bin/env bash
set -euo pipefail
if [[ "$1 $2" == "rev-parse HEAD" ]]; then
    printf '0123456789abcdef0123456789abcdef01234567\n'
else
    echo "unexpected fake git invocation: $*" >&2
    exit 2
fi
FAKE_GIT
cat >"$TMP/bin/gh" <<'FAKE_GH'
#!/usr/bin/env bash
set -euo pipefail

TEST_SHA="0123456789abcdef0123456789abcdef01234567"
OTHER_SHA="fedcba9876543210fedcba9876543210fedcba98"
SCENARIO="${FAKE_GH_SCENARIO:?}"

if [[ "$1 $2" == "run list" ]]; then
    case "$SCENARIO" in
        success)
            printf '[{"databaseId":101,"headSha":"%s","status":"completed","conclusion":"success","url":"https://example.test/runs/101"}]\n' "$TEST_SHA"
            ;;
        wrong-sha)
            printf '[{"databaseId":102,"headSha":"%s","status":"completed","conclusion":"success","url":"https://example.test/runs/102"}]\n' "$OTHER_SHA"
            ;;
        no-run)
            printf '[]\n'
            ;;
        running)
            printf '[{"databaseId":103,"headSha":"%s","status":"in_progress","conclusion":"","url":"https://example.test/runs/103"}]\n' "$TEST_SHA"
            ;;
        failed-run|missing-job|failed-job|invalid-jobs|view-error)
            conclusion=success
            [[ "$SCENARIO" == "failed-run" ]] && conclusion=failure
            printf '[{"databaseId":104,"headSha":"%s","status":"completed","conclusion":"%s","url":"https://example.test/runs/104"}]\n' "$TEST_SHA" "$conclusion"
            ;;
        invalid-runs)
            printf 'not json\n'
            ;;
        query-error)
            echo "simulated GitHub outage" >&2
            exit 1
            ;;
        *) exit 2 ;;
    esac
elif [[ "$1 $2" == "run view" ]]; then
    case "$SCENARIO" in
        success)
            printf '{"jobs":[{"name":"release-certified","status":"completed","conclusion":"success"}]}\n'
            ;;
        running)
            printf '{"jobs":[{"name":"release-certified","status":"queued","conclusion":""}]}\n'
            ;;
        failed-run|failed-job)
            printf '{"jobs":[{"name":"release-certified","status":"completed","conclusion":"failure"}]}\n'
            ;;
        missing-job)
            printf '{"jobs":[{"name":"cargo fmt --all --check","status":"completed","conclusion":"success"}]}\n'
            ;;
        invalid-jobs)
            printf 'not json\n'
            ;;
        view-error)
            echo "simulated job API outage" >&2
            exit 1
            ;;
        *) exit 2 ;;
    esac
else
    echo "unexpected fake gh invocation: $*" >&2
    exit 2
fi
FAKE_GH
chmod +x "$TMP/bin/git" "$TMP/bin/gh"

pass=0
fail=0

expect_success() {
    local scenario="$1"
    local log="$TMP/$scenario.log"
    if FAKE_GH_SCENARIO="$scenario" PATH="$TMP/bin:$PATH" \
        bash "$ASSERT_SCRIPT" --repo theaspirational/orgasmic --sha "$TEST_SHA" >"$log" 2>&1; then
        if grep -q "exact HEAD is release-certified" "$log"; then
            pass=$((pass + 1))
            return
        fi
    fi
    echo "FAIL: expected success for $scenario" >&2
    sed -n '1,120p' "$log" >&2
    fail=$((fail + 1))
}

expect_failure() {
    local scenario="$1"
    local expected="$2"
    local log="$TMP/$scenario.log"
    if FAKE_GH_SCENARIO="$scenario" PATH="$TMP/bin:$PATH" \
        bash "$ASSERT_SCRIPT" --repo theaspirational/orgasmic --sha "$TEST_SHA" >"$log" 2>&1; then
        echo "FAIL: expected failure for $scenario" >&2
        sed -n '1,120p' "$log" >&2
        fail=$((fail + 1))
        return
    fi
    if ! grep -q "$expected" "$log"; then
        echo "FAIL: $scenario did not report '$expected'" >&2
        sed -n '1,120p' "$log" >&2
        fail=$((fail + 1))
        return
    fi
    pass=$((pass + 1))
}

expect_sha_mismatch() {
    local log="$TMP/sha-mismatch.log"
    if FAKE_GH_SCENARIO=success PATH="$TMP/bin:$PATH" \
        bash "$ASSERT_SCRIPT" --repo theaspirational/orgasmic \
        --sha fedcba9876543210fedcba9876543210fedcba98 >"$log" 2>&1; then
        echo "FAIL: expected a non-HEAD SHA to be rejected" >&2
        sed -n '1,120p' "$log" >&2
        fail=$((fail + 1))
        return
    fi
    if ! grep -q "is not current HEAD" "$log"; then
        echo "FAIL: non-HEAD SHA did not report the mismatch" >&2
        sed -n '1,120p' "$log" >&2
        fail=$((fail + 1))
        return
    fi
    pass=$((pass + 1))
}

expect_success success
expect_failure wrong-sha "no ci.yml push run exists for exact HEAD"
expect_failure no-run "no ci.yml push run exists for exact HEAD"
expect_failure running "release-certified=queued/-"
expect_failure failed-run "run=completed/failure"
expect_failure missing-job "release-certified=missing/missing"
expect_failure failed-job "release-certified=completed/failure"
expect_failure query-error "could not query ci.yml push runs"
expect_failure invalid-runs "GitHub returned invalid run metadata"
expect_failure invalid-jobs "release-certified=invalid/invalid"
expect_failure view-error "release-certified=inspection-failed/inspection-failed"
expect_sha_mismatch

echo "$pass assertion self-tests passed"
if [[ "$fail" -ne 0 ]]; then
    echo "$fail assertion self-tests failed" >&2
    exit 1
fi
