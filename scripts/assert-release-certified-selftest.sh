#!/usr/bin/env bash

set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
ASSERT_SCRIPT="$ROOT/scripts/assert-release-certified.sh"
TEST_SHA="0123456789abcdef0123456789abcdef01234567"
TMP="$(mktemp -d)"
trap 'rm -rf "$TMP"' EXIT

mkdir -p "$TMP/bin"
cat >"$TMP/bin/git" <<'FAKE_GIT'
#!/usr/bin/env bash
set -euo pipefail
[[ "$1 $2" == "rev-parse HEAD" ]] || { echo "unexpected git: $*" >&2; exit 2; }
printf '0123456789abcdef0123456789abcdef01234567\n'
FAKE_GIT
cat >"$TMP/bin/gh" <<'FAKE_GH'
#!/usr/bin/env bash
set -euo pipefail
case "$1 $2" in
    "repo view") printf 'example/repo\n' ;;
    "api repos/example/repo/commits/0123456789abcdef0123456789abcdef01234567/status")
        case "${TEST_CASE:-success}" in
            success) state=success; context=local/release-certified; sha=0123456789abcdef0123456789abcdef01234567 ;;
            failure) state=failure; context=local/release-certified; sha=0123456789abcdef0123456789abcdef01234567 ;;
            pending) state=pending; context=local/release-certified; sha=0123456789abcdef0123456789abcdef01234567 ;;
            missing) state=success; context=some/other-check; sha=0123456789abcdef0123456789abcdef01234567 ;;
            wrong-sha) state=success; context=local/release-certified; sha=ffffffffffffffffffffffffffffffffffffffff ;;
            api-error) echo "status api unavailable" >&2; exit 1 ;;
            invalid) printf '{not json\n'; exit 0 ;;
            *) echo "unknown TEST_CASE" >&2; exit 2 ;;
        esac
        printf '{"sha":"%s","statuses":[{"state":"%s","context":"%s","description":"tree=abc base=def","target_url":"https://example.invalid/commit","creator":{"login":"maintainer"}}]}\n' "$sha" "$state" "$context"
        ;;
    *) echo "unexpected gh: $*" >&2; exit 2 ;;
esac
FAKE_GH
chmod +x "$TMP/bin/git" "$TMP/bin/gh"

run_case() {
    local name="$1" expected="$2"
    local log="$TMP/$name.log"
    set +e
    PATH="$TMP/bin:$PATH" TEST_CASE="$name" bash "$ASSERT_SCRIPT" --repo example/repo --sha "$TEST_SHA" >"$log" 2>&1
    local status=$?
    set -e
    if [[ "$status" -ne "$expected" ]]; then
        echo "FAIL $name: expected $expected, got $status" >&2
        cat "$log" >&2
        exit 1
    fi
    echo "ok: $name"
}

run_case success 0
run_case failure 1
run_case pending 1
run_case missing 1
run_case wrong-sha 1
run_case api-error 1
run_case invalid 1

set +e
PATH="$TMP/bin:$PATH" bash "$ASSERT_SCRIPT" --repo example/repo --sha ffffffffffffffffffffffffffffffffffffffff >"$TMP/mismatch.log" 2>&1
status=$?
set -e
[[ "$status" -eq 1 ]] || { echo "FAIL mismatched HEAD: got $status" >&2; exit 1; }
echo "ok: mismatched HEAD"

echo "assert-release-certified self-test: GREEN"
