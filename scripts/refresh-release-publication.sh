#!/usr/bin/env bash
# orgasmic:TASK-J920C, dec_B4147
# Refresh published_at on one rolling GitHub release after all asset uploads.

set -euo pipefail

REPO="${ORGASMIC_RELEASE_REPO:-}"
TAG=""
LINE=""
CHANNEL=""

usage() {
    cat <<'EOF'
Usage: bash scripts/refresh-release-publication.sh [options]

Briefly returns one fully-uploaded rolling release to draft, republishes it,
and verifies its timestamp, assets, prerelease state, and Latest policy.

Options:
  --repo <owner/name>       GitHub repo (default: gh repo view / ORGASMIC_RELEASE_REPO)
  --tag <tag>               Existing rolling release tag
  --line <runtime|apps>     Product line
  --channel <stable|nightly>
  -h, --help
EOF
}

while [[ $# -gt 0 ]]; do
    case "$1" in
        --repo) REPO="$2"; shift 2 ;;
        --tag) TAG="$2"; shift 2 ;;
        --line) LINE="$2"; shift 2 ;;
        --channel) CHANNEL="$2"; shift 2 ;;
        -h|--help) usage; exit 0 ;;
        *) echo "unknown option: $1" >&2; usage >&2; exit 1 ;;
    esac
done

[[ -n "$TAG" ]] || { echo "error: --tag is required" >&2; exit 1; }
if [[ -z "$REPO" ]]; then
    command -v gh >/dev/null 2>&1 || {
        echo "error: --repo is required when gh is unavailable" >&2
        exit 1
    }
    REPO="$(gh repo view --json nameWithOwner -q .nameWithOwner)"
fi
command -v gh >/dev/null 2>&1 || {
    echo "error: required command not found: gh" >&2
    exit 1
}

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
# shellcheck source=scripts/release-channel-policy.sh
source "$SCRIPT_DIR/release-channel-policy.sh"
release_channel_state_policy "$LINE" "$CHANNEL"

before_state="$(
    gh release view "$TAG" -R "$REPO" \
        --json isDraft,isPrerelease,publishedAt,assets \
        --jq '[.isDraft, .isPrerelease, .publishedAt, (.assets | length)] | @tsv'
)"
IFS=$'\t' read -r before_draft before_prerelease previous_published_at before_assets \
    <<<"$before_state"
if [[ "$before_draft" != "false" || -z "$previous_published_at" || "$previous_published_at" == "null" ]]; then
    echo "error: $TAG must be published before its publication timestamp can be refreshed" >&2
    exit 1
fi
if [[ "$before_prerelease" != "$RELEASE_POLICY_PRERELEASE" ]]; then
    echo "error: $TAG prerelease state is $before_prerelease, expected $RELEASE_POLICY_PRERELEASE" >&2
    exit 1
fi

echo "→ refreshing $TAG release publication timestamp"
gh release edit "$TAG" -R "$REPO" --draft >/dev/null
if ! gh release edit "$TAG" -R "$REPO" \
    --draft=false \
    --latest="$RELEASE_POLICY_LATEST" \
    --prerelease="$RELEASE_POLICY_PRERELEASE" >/dev/null; then
    echo "warning: first attempt to republish $TAG failed; retrying once" >&2
    if ! gh release edit "$TAG" -R "$REPO" \
        --draft=false \
        --latest="$RELEASE_POLICY_LATEST" \
        --prerelease="$RELEASE_POLICY_PRERELEASE" >/dev/null; then
        echo "error: $TAG remains a draft; publish it manually with gh release edit" >&2
        exit 1
    fi
fi

after_state="$(
    gh release view "$TAG" -R "$REPO" \
        --json isDraft,isPrerelease,publishedAt,assets \
        --jq '[.isDraft, .isPrerelease, .publishedAt, (.assets | length)] | @tsv'
)"
IFS=$'\t' read -r is_draft is_prerelease published_at after_assets <<<"$after_state"
latest_tag="$(gh api "repos/$REPO/releases/latest" --jq .tag_name 2>/dev/null || true)"

if [[ "$is_draft" != "false" || -z "$published_at" || "$published_at" == "null" ]]; then
    echo "error: $TAG did not return to a published state" >&2
    exit 1
fi
if [[ "$is_prerelease" != "$RELEASE_POLICY_PRERELEASE" ]]; then
    echo "error: republished $TAG prerelease state is $is_prerelease, expected $RELEASE_POLICY_PRERELEASE" >&2
    exit 1
fi
if [[ "$after_assets" != "$before_assets" ]]; then
    echo "error: $TAG asset count changed during republish ($before_assets -> $after_assets)" >&2
    exit 1
fi
if [[ "$published_at" == "$previous_published_at" ]]; then
    echo "error: GitHub did not refresh $TAG published_at ($published_at)" >&2
    exit 1
fi
if [[ "$RELEASE_POLICY_LATEST" == "true" && "$latest_tag" != "$TAG" ]]; then
    echo "error: republished $TAG is not GitHub's latest release (latest: ${latest_tag:-none})" >&2
    exit 1
fi
if [[ "$RELEASE_POLICY_LATEST" == "false" && "$latest_tag" == "$TAG" ]]; then
    echo "error: republished $TAG unexpectedly became GitHub's latest release" >&2
    exit 1
fi

echo "✓ republished $TAG at $published_at (prerelease=$is_prerelease latest=$RELEASE_POLICY_LATEST assets=$after_assets)"
