#!/usr/bin/env bash
# Risk-scoped certification for the default darwin-aarch64 stable runtime. The
# receipt is keyed by source tree, comparison base, certifier and toolchain. A
# full release-infrastructure change automatically escalates to the full gate.

set -euo pipefail

REPO=""
BASE_SHA=""
MODE="certify-and-publish"
CONTEXT="local/release-certified"
CERTIFICATION_RUST="1.97.1"

usage() {
    cat <<'EOF'
Usage: bash scripts/certify-runtime-fast.sh [--repo <owner/name>] [--base <sha>]
                                             [--no-publish | --publish-only]
EOF
}

while [[ $# -gt 0 ]]; do
    case "$1" in
        --repo) REPO="$2"; shift 2 ;;
        --base) BASE_SHA="$2"; shift 2 ;;
        --no-publish) MODE="no-publish"; shift ;;
        --publish-only) MODE="publish-only"; shift ;;
        -h|--help) usage; exit 0 ;;
        *) echo "certify-runtime-fast: unknown option: $1" >&2; usage >&2; exit 2 ;;
    esac
done

ROOT="$(git rev-parse --show-toplevel 2>/dev/null)" || { echo "certify-runtime-fast: not in a worktree" >&2; exit 2; }
cd "$ROOT"
[[ -z "$(git status --porcelain --untracked-files=all)" ]] || {
    echo "certify-runtime-fast: worktree must be clean" >&2; git status --short >&2; exit 1;
}
[[ -z "${ORGASMIC_ALLOW_BILLED_TESTS:-}" ]] || { echo "certify-runtime-fast: billed tests must be disabled" >&2; exit 1; }

HEAD_SHA="$(git rev-parse HEAD)"
TREE_SHA="$(git rev-parse 'HEAD^{tree}')"
if [[ -z "$BASE_SHA" ]]; then BASE_SHA="$(git rev-parse HEAD^ 2>/dev/null || printf '%s' "$HEAD_SHA")"; fi
BASE_SHA="$(git rev-parse "$BASE_SHA^{commit}")"
git merge-base --is-ancestor "$BASE_SHA" "$HEAD_SHA" || {
    echo "certify-runtime-fast: comparison base is not an ancestor of HEAD" >&2; exit 1;
}

CERTIFIER_SHA="$(git hash-object scripts/certify-runtime-fast.sh)"
FULL_CERTIFIER_SHA="$(git hash-object scripts/certify-release.sh)"
RECEIPT_CONTENT="$(printf 'version=1\nprofile=runtime-fast\ntree=%s\nbase=%s\ncertifier=%s\nfull_certifier=%s\nrust=%s' \
    "$TREE_SHA" "$BASE_SHA" "$CERTIFIER_SHA" "$FULL_CERTIFIER_SHA" "$CERTIFICATION_RUST")"
RECEIPT_KEY="$(printf '%s\n' "$RECEIPT_CONTENT" | shasum -a 256 | awk '{print $1}')"
COMMON_DIR="$(git rev-parse --git-common-dir)"; [[ "$COMMON_DIR" = /* ]] || COMMON_DIR="$ROOT/$COMMON_DIR"
RECEIPT_DIR="$COMMON_DIR/orgasmic-certifications"
RECEIPT="$RECEIPT_DIR/$RECEIPT_KEY.receipt"

resolve_remote() {
    command -v gh >/dev/null 2>&1 || { echo "certify-runtime-fast: gh is required" >&2; exit 2; }
    REPO="${REPO:-$(gh repo view --json nameWithOwner -q .nameWithOwner)}"
    gh api "repos/$REPO/commits/$HEAD_SHA" >/dev/null 2>&1 || {
        echo "certify-runtime-fast: push $HEAD_SHA before publishing certification" >&2; exit 1;
    }
}

post_status() {
    local state="$1" description="$2"
    gh api --method POST "repos/$REPO/statuses/$HEAD_SHA" \
        -f state="$state" -f context="$CONTEXT" -f description="$description" \
        -f target_url="https://github.com/$REPO/commit/$HEAD_SHA" >/dev/null
}

if [[ "$MODE" != "no-publish" ]]; then resolve_remote; fi
receipt_matches() { [[ -f "$RECEIPT" && "$(cat "$RECEIPT")" == "$RECEIPT_CONTENT" ]]; }

if ! receipt_matches; then
    if [[ "$MODE" == "publish-only" ]]; then
        post_status error "runtime-fast receipt missing for tree ${TREE_SHA:0:12}"
        echo "certify-runtime-fast: exact receipt is missing" >&2; exit 1
    fi

    CHANGED="$(git diff --name-only "$BASE_SHA..$HEAD_SHA")"
    if printf '%s\n' "$CHANGED" | grep -Eq '^(Cargo\.(toml|lock)|rust-toolchain\.toml|provider-host/|scripts/(assert-ci-certified|certify-|integrate-main|package-runtime|publish-runtime|runtime-candidate|release-runtime-fast|sync-release|refresh-release|release-channel)|\.github/workflows/runtime-bundles\.yml)'; then
        echo "→ release infrastructure changed; requiring the full certification gate"
        if [[ "$MODE" != "no-publish" ]] && bash scripts/assert-ci-certified.sh \
            --repo "$REPO" --sha "$HEAD_SHA" --context local/release-certified; then
            echo "✓ reusing exact-commit full certification for runtime-fast"
        else
            env -u ORGASMIC_RUN_ID -u ORGASMIC_HOME -u ORGASMIC_ALLOW_BILLED_TESTS \
                bash scripts/certify-release.sh
        fi
    else
        RUSTUP_PROXY_DIR="$(dirname "$(command -v rustup)")"; PATH="$RUSTUP_PROXY_DIR:$PATH"; export PATH
        rustup toolchain install "$CERTIFICATION_RUST" --profile minimal \
            --component clippy,rustfmt --no-self-update
        CARGO=(rustup run "$CERTIFICATION_RUST" cargo)
        echo "→ runtime-fast: formatting and strict workspace Clippy"
        "${CARGO[@]}" fmt --all --check
        "${CARGO[@]}" clippy --workspace --all-targets --keep-going -- -D warnings

        echo "→ runtime-fast: deterministic runtime unit suites"
        "${CARGO[@]}" test -p orgasmic-core
        "${CARGO[@]}" test -p orgasmic-drivers --lib
        "${CARGO[@]}" test -p orgasmic-daemon --lib
        "${CARGO[@]}" test -p orgasmic-cli --bin orgasmic

        echo "→ runtime-fast: critical persistence, update and installation contracts"
        "${CARGO[@]}" test -p orgasmic-daemon \
            --test writer_durability --test duplicate_write --test body_write_guard --test node_body_roundtrip
        "${CARGO[@]}" test -p orgasmic-cli \
            --test daemon_lifecycle --test managed_source_install --test bootstrap_smoke
        bash scripts/publish-runtime-selftest.sh
        bash scripts/assert-ci-certified-selftest.sh

        if printf '%s\n' "$CHANGED" | grep -Eq '^(ui/|crates/orgasmic-daemon/build\.rs)'; then
            echo "→ embedded UI changed; running UI gates"
            npm ci --prefix ui
            npm --prefix ui run typecheck
            npm --prefix ui test
            npm --prefix ui run build
        fi
    fi

    mkdir -p "$RECEIPT_DIR"; umask 077
    tmp="$(mktemp "$RECEIPT_DIR/.runtime-fast.XXXXXX")"
    printf '%s\n' "$RECEIPT_CONTENT" >"$tmp"; mv "$tmp" "$RECEIPT"
    echo "✓ wrote runtime-fast receipt $RECEIPT_KEY"
else
    echo "✓ reusing runtime-fast receipt $RECEIPT_KEY"
fi

if [[ "$MODE" != "no-publish" ]]; then
    post_status success "runtime-fast tree=${TREE_SHA:0:12} base=${BASE_SHA:0:12} rust=$CERTIFICATION_RUST"
    bash scripts/assert-ci-certified.sh --repo "$REPO" --sha "$HEAD_SHA" --context "$CONTEXT"
fi
echo "runtime-fast certification: GREEN"
