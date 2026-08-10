#!/usr/bin/env bash
# Certify one clean source tree locally, reuse only an exact receipt, and post
# the successful result to the exact GitHub commit (dec_M251B).

set -euo pipefail

MODE="certify-and-publish"
REPO=""
CONTEXT="local/release-certified"

usage() {
    cat <<'EOF'
Usage: bash scripts/certify-pr.sh [--repo <owner/name>] [--no-publish | --publish-only]

The default certifies (or reuses an exact receipt) and publishes the commit
status. Use --no-publish before pushing, then --publish-only after pushing.
EOF
}

while [[ $# -gt 0 ]]; do
    case "$1" in
        --repo) REPO="$2"; shift 2 ;;
        --no-publish) MODE="no-publish"; shift ;;
        --publish-only) MODE="publish-only"; shift ;;
        -h|--help) usage; exit 0 ;;
        *) echo "unknown option: $1" >&2; usage >&2; exit 1 ;;
    esac
done

for cmd in git shasum sed; do
    command -v "$cmd" >/dev/null 2>&1 || { echo "certify-pr: missing $cmd" >&2; exit 2; }
done

ROOT="$(git rev-parse --show-toplevel 2>/dev/null)" || { echo "certify-pr: not in a worktree" >&2; exit 2; }
cd "$ROOT"
if [[ -n "$(git status --porcelain --untracked-files=all)" ]]; then
    echo "certify-pr: worktree must be clean so the receipt names the tested tree" >&2
    git status --short >&2
    exit 1
fi

git fetch --quiet origin main
HEAD_SHA="$(git rev-parse HEAD)"
ORIGIN_MAIN="$(git rev-parse origin/main)"
if [[ "$HEAD_SHA" == "$ORIGIN_MAIN" ]]; then
    BASE_SHA="$(git rev-parse HEAD^1 2>/dev/null || printf '%s' "$HEAD_SHA")"
else
    git merge-base --is-ancestor "$ORIGIN_MAIN" "$HEAD_SHA" || {
        echo "certify-pr: HEAD is not current with origin/main; update the branch first" >&2
        exit 1
    }
    BASE_SHA="$ORIGIN_MAIN"
fi

TREE_SHA="$(git rev-parse "${HEAD_SHA}^{tree}")"
CERTIFIER_SHA="$(git hash-object scripts/certify-release.sh)"
CERTIFICATION_RUST="$(sed -n 's/^CERTIFICATION_RUST="\([^"]*\)"/\1/p' scripts/certify-release.sh | head -1)"
MSRV_RUST="$(sed -n 's/^MSRV_RUST="\([^"]*\)"/\1/p' scripts/certify-release.sh | head -1)"
[[ -n "$CERTIFICATION_RUST" && -n "$MSRV_RUST" ]] || {
    echo "certify-pr: could not read certification toolchains" >&2
    exit 2
}

RECEIPT_CONTENT="$(printf 'version=1\ntree=%s\nbase=%s\ncertifier=%s\ncertification_rust=%s\nmsrv_rust=%s' \
    "$TREE_SHA" "$BASE_SHA" "$CERTIFIER_SHA" "$CERTIFICATION_RUST" "$MSRV_RUST")"
RECEIPT_KEY="$(printf '%s\n' "$RECEIPT_CONTENT" | shasum -a 256 | awk '{print $1}')"
COMMON_DIR="$(git rev-parse --git-common-dir)"
[[ "$COMMON_DIR" = /* ]] || COMMON_DIR="$ROOT/$COMMON_DIR"
RECEIPT_DIR="$COMMON_DIR/orgasmic-certifications"
RECEIPT="$RECEIPT_DIR/$RECEIPT_KEY.receipt"

receipt_matches() {
    [[ -f "$RECEIPT" && "$(cat "$RECEIPT")" == "$RECEIPT_CONTENT" ]]
}

resolve_remote() {
    command -v gh >/dev/null 2>&1 || { echo "certify-pr: missing gh" >&2; exit 2; }
    REPO="${REPO:-$(gh repo view --json nameWithOwner -q .nameWithOwner)}"
    gh api "repos/$REPO/commits/$HEAD_SHA" >/dev/null 2>&1 || {
        echo "certify-pr: GitHub does not know $HEAD_SHA; push the branch, then use --publish-only" >&2
        exit 1
    }
}

post_status() {
    local state="$1" description="$2"
    gh api --method POST "repos/$REPO/statuses/$HEAD_SHA" \
        -f state="$state" \
        -f context="$CONTEXT" \
        -f description="$description" \
        -f target_url="https://github.com/$REPO/commit/$HEAD_SHA" >/dev/null
}

if [[ "$MODE" != "no-publish" ]]; then
    resolve_remote
fi

if ! receipt_matches; then
    if [[ "$MODE" == "publish-only" ]]; then
        post_status error "local receipt missing for tree ${TREE_SHA:0:12}"
        echo "certify-pr: no exact receipt; run without --publish-only" >&2
        exit 1
    fi

    echo "→ running complete local certification for tree $TREE_SHA"
    if ! env -u ORGASMIC_RUN_ID -u ORGASMIC_HOME -u ORGASMIC_ALLOW_BILLED_TESTS \
        bash scripts/certify-release.sh; then
        [[ "$MODE" == "no-publish" ]] || post_status failure "local certification failed for tree ${TREE_SHA:0:12}"
        exit 1
    fi
    mkdir -p "$RECEIPT_DIR"
    umask 077
    tmp="$(mktemp "$RECEIPT_DIR/.receipt.XXXXXX")"
    printf '%s\n' "$RECEIPT_CONTENT" >"$tmp"
    mv "$tmp" "$RECEIPT"
    echo "✓ wrote reusable receipt $RECEIPT_KEY"
else
    echo "✓ reusing exact local receipt $RECEIPT_KEY"
fi

if [[ "$MODE" != "no-publish" ]]; then
    DESCRIPTION="tree=${TREE_SHA:0:12} base=${BASE_SHA:0:12} cert=${CERTIFIER_SHA:0:12} rust=$CERTIFICATION_RUST/$MSRV_RUST"
    post_status success "$DESCRIPTION"
    bash scripts/assert-ci-certified.sh --repo "$REPO" --sha "$HEAD_SHA"
fi

echo "local release certification: GREEN"
