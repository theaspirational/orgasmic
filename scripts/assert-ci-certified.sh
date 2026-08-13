#!/usr/bin/env bash
# Legacy filename, local authority: fail closed unless the exact current commit
# carries the successful status required by branch protection (dec_M251B).

set -euo pipefail

REPO=""
HEAD_SHA=""
CONTEXT="local/release-certified"

usage() {
    cat <<'EOF'
Usage: bash scripts/assert-ci-certified.sh [--repo <owner/name>] [--sha <commit>]
                                             [--context <status-context>]

Requires a successful status in the selected context on the exact current
commit. Defaults to local/release-certified, the current repository and HEAD.
There is no publication bypass.
EOF
}

while [[ $# -gt 0 ]]; do
    case "$1" in
        --repo) REPO="$2"; shift 2 ;;
        --sha) HEAD_SHA="$2"; shift 2 ;;
        --context) CONTEXT="$2"; shift 2 ;;
        -h|--help) usage; exit 0 ;;
        *) echo "unknown option: $1" >&2; usage >&2; exit 1 ;;
    esac
done

for cmd in git gh node; do
    command -v "$cmd" >/dev/null 2>&1 || {
        echo "error: required command not found: $cmd" >&2
        exit 1
    }
done

CURRENT_HEAD="$(git rev-parse HEAD | tr '[:upper:]' '[:lower:]')"
HEAD_SHA="${HEAD_SHA:-$CURRENT_HEAD}"
if [[ ! "$HEAD_SHA" =~ ^[0-9a-fA-F]{40}$ ]]; then
    echo "error: expected a full 40-character commit SHA, got: '$HEAD_SHA'" >&2
    exit 1
fi
HEAD_SHA="$(printf '%s' "$HEAD_SHA" | tr '[:upper:]' '[:lower:]')"
if [[ "$HEAD_SHA" != "$CURRENT_HEAD" ]]; then
    echo "error: stable publish blocked: requested SHA $HEAD_SHA is not current HEAD $CURRENT_HEAD" >&2
    exit 1
fi

REPO="${REPO:-$(gh repo view --json nameWithOwner -q .nameWithOwner)}"
[[ -n "$REPO" ]] || { echo "error: could not resolve GitHub repository" >&2; exit 1; }

echo "→ checking exact-commit local certification for $HEAD_SHA"
if ! STATUS_JSON="$(gh api "repos/$REPO/commits/$HEAD_SHA/status" 2>&1)"; then
    echo "error: stable publish blocked: could not query commit statuses" >&2
    echo "$STATUS_JSON" >&2
    exit 1
fi

ROW="$(STATUS_JSON="$STATUS_JSON" HEAD_SHA="$HEAD_SHA" CONTEXT="$CONTEXT" node <<'NODE'
const payload = JSON.parse(process.env.STATUS_JSON);
if (String(payload.sha || '').toLowerCase() !== process.env.HEAD_SHA) process.exit(3);
const wanted = process.env.CONTEXT.toLowerCase();
const status = (payload.statuses || []).find(
  candidate => String(candidate.context || '').toLowerCase() === wanted,
);
if (!status) process.stdout.write('missing\t-\t-\t-');
else process.stdout.write([
  status.state || '-',
  status.creator?.login || '-',
  status.description || '-',
  status.target_url || '-',
].join('\t'));
NODE
)" || {
    echo "error: stable publish blocked: GitHub returned invalid status metadata" >&2
    exit 1
}

IFS=$'\t' read -r STATE ACTOR DESCRIPTION TARGET_URL <<<"$ROW"
if [[ "$STATE" != "success" ]]; then
    echo "error: stable publish blocked: $CONTEXT is $STATE for exact HEAD $HEAD_SHA" >&2
    echo "       actor=$ACTOR description=$DESCRIPTION target=$TARGET_URL" >&2
    echo "       run: bash scripts/certify-pr.sh" >&2
    exit 1
fi

echo "✓ exact HEAD is locally certified by $ACTOR: $DESCRIPTION"
