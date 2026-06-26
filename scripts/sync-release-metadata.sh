#!/usr/bin/env bash
# orgasmic:dec_B4147
# Sync GitHub release title/notes/state for the runtime and app release lines.

set -euo pipefail

REPO="${ORGASMIC_RELEASE_REPO:-}"
TAG=""
LINE=""
CHANNEL=""
VERSION=""
COMMIT=""
DRY_RUN=0
RUNTIME_MANIFEST=""
APP_MANIFEST=""
ANDROID_MANIFEST=""
NOTES_FILE=""
TMP_DIR=""

usage() {
    cat <<'EOF'
Usage: bash scripts/sync-release-metadata.sh [options]

Computes and applies the canonical GitHub release title, notes, target commit,
latest flag, and prerelease flag for orgasmic release channels.

Options:
  --repo <owner/name>       GitHub repo (default: gh repo view / ORGASMIC_RELEASE_REPO)
  --tag <tag>               Release tag (default: derived from --line + --channel)
  --line <runtime|apps>     Product line
  --channel <stable|nightly>
  --version <v>             Release version. If omitted, inferred from release manifests.
  --commit <sha>            Release target. If omitted, inferred from release target/tag.
  --notes-file <path>       Use prepared release notes instead of generated one-line notes.
  --dry-run                 Print computed metadata without editing GitHub.

Backfill/testing inputs:
  --runtime-manifest <path> Runtime runtime-latest.json for version inference
  --app-manifest <path>     App latest.json for version inference
  --android-manifest <path> App android-latest.json fallback for version inference

Canonical titles:
  stable       -> Orgasmic Runtime <version>
  nightly      -> Orgasmic Runtime Nightly
  apps-stable  -> Orgasmic Apps <version>
  apps-nightly -> Orgasmic Apps Nightly
EOF
}

cleanup() {
    if [[ -n "$TMP_DIR" ]]; then
        rm -rf "$TMP_DIR"
    fi
    return 0
}
trap cleanup EXIT

while [[ $# -gt 0 ]]; do
    case "$1" in
        --repo) REPO="$2"; shift 2 ;;
        --tag) TAG="$2"; shift 2 ;;
        --line) LINE="$2"; shift 2 ;;
        --channel) CHANNEL="$2"; shift 2 ;;
        --version) VERSION="$2"; shift 2 ;;
        --commit) COMMIT="$2"; shift 2 ;;
        --notes-file) NOTES_FILE="$2"; shift 2 ;;
        --dry-run) DRY_RUN=1; shift ;;
        --runtime-manifest) RUNTIME_MANIFEST="$2"; shift 2 ;;
        --app-manifest) APP_MANIFEST="$2"; shift 2 ;;
        --android-manifest) ANDROID_MANIFEST="$2"; shift 2 ;;
        -h|--help) usage; exit 0 ;;
        *) echo "unknown option: $1" >&2; usage >&2; exit 1 ;;
    esac
done

tmp_dir() {
    if [[ -z "$TMP_DIR" ]]; then
        TMP_DIR="$(mktemp -d)"
    fi
    printf '%s\n' "$TMP_DIR"
}

infer_line_channel_from_tag() {
    case "$TAG" in
        stable)
            [[ -z "$LINE" ]] && LINE="runtime"
            [[ -z "$CHANNEL" ]] && CHANNEL="stable"
            ;;
        nightly)
            [[ -z "$LINE" ]] && LINE="runtime"
            [[ -z "$CHANNEL" ]] && CHANNEL="nightly"
            ;;
        apps|apps-stable)
            [[ -z "$LINE" ]] && LINE="apps"
            [[ -z "$CHANNEL" ]] && CHANNEL="stable"
            ;;
        apps-nightly)
            [[ -z "$LINE" ]] && LINE="apps"
            [[ -z "$CHANNEL" ]] && CHANNEL="nightly"
            ;;
    esac
    return 0
}

derive_tag() {
    case "$LINE:$CHANNEL" in
        runtime:stable) printf 'stable\n' ;;
        runtime:nightly) printf 'nightly\n' ;;
        apps:stable) printf 'apps-stable\n' ;;
        apps:nightly) printf 'apps-nightly\n' ;;
        *) return 1 ;;
    esac
}

json_version() {
    local path="$1"
    local version
    command -v node >/dev/null 2>&1 || {
        echo "error: node is required to parse manifest JSON" >&2
        return 1
    }
    if ! version="$(node -e '
const fs = require("node:fs");
const file = process.argv[1];
const data = JSON.parse(fs.readFileSync(file, "utf8"));
const version = data && data.version;
if (typeof version === "string") process.stdout.write(version);
' "$path")"; then
        return 1
    fi
    [[ -n "$version" ]] || return 1
    printf '%s\n' "$version"
}

download_asset() {
    local asset="$1"
    local output="$2"
    command -v gh >/dev/null 2>&1 || {
        echo "error: gh is required to download $asset from $TAG" >&2
        return 1
    }
    gh release download "$TAG" -R "$REPO" -p "$asset" -O "$output" --clobber >/dev/null 2>&1
}

infer_runtime_version() {
    local manifest="$RUNTIME_MANIFEST"
    if [[ -z "$manifest" ]]; then
        manifest="$(tmp_dir)/runtime-latest.json"
        download_asset runtime-latest.json "$manifest" || {
            echo "error: could not infer version; runtime-latest.json is unavailable on $TAG" >&2
            return 1
        }
    fi
    json_version "$manifest" || {
        echo "error: could not read version from $manifest" >&2
        return 1
    }
}

infer_apps_version() {
    local version=""
    local manifest="$APP_MANIFEST"
    if [[ -n "$manifest" && -f "$manifest" ]]; then
        version="$(json_version "$manifest" || true)"
    fi
    if [[ -z "$version" && -n "$ANDROID_MANIFEST" && -f "$ANDROID_MANIFEST" ]]; then
        version="$(json_version "$ANDROID_MANIFEST" || true)"
    fi
    if [[ -z "$version" && -z "$APP_MANIFEST" && -z "$ANDROID_MANIFEST" ]]; then
        manifest="$(tmp_dir)/latest.json"
        if download_asset latest.json "$manifest"; then
            version="$(json_version "$manifest" || true)"
        fi
        if [[ -z "$version" ]]; then
            manifest="$(tmp_dir)/android-latest.json"
            if download_asset android-latest.json "$manifest"; then
                version="$(json_version "$manifest" || true)"
            fi
        fi
    fi
    [[ -n "$version" ]] || {
        echo "error: could not infer version; latest.json/android-latest.json are unavailable on $TAG" >&2
        return 1
    }
    printf '%s\n' "$version"
}

infer_version() {
    case "$LINE" in
        runtime) infer_runtime_version ;;
        apps) infer_apps_version ;;
        *) return 1 ;;
    esac
}

infer_commit() {
    local target=""
    local tag_sha=""
    command -v gh >/dev/null 2>&1 || {
        echo "error: gh is required to infer --commit" >&2
        return 1
    }
    target="$(gh release view "$TAG" -R "$REPO" --json targetCommitish -q .targetCommitish 2>/dev/null || true)"
    if command -v git >/dev/null 2>&1; then
        tag_sha="$(git ls-remote "https://github.com/${REPO}.git" "refs/tags/${TAG}" 2>/dev/null | awk 'NR == 1 { print $1 }')"
    fi
    # Prefer the release's recorded targetCommitish (set at publish time to the
    # publish commit) over the git tag ref. A rolling tag like apps-nightly can
    # lag behind the published commit (publish-apps.sh moves the release target
    # via gh but not the tag), so trusting the tag first would reset the release
    # target backward on a notes-only sync. Matches the documented priority:
    # "inferred from release target/tag".
    if [[ -n "$target" && "$target" != "null" ]]; then
        printf '%s\n' "$target"
    elif [[ -n "$tag_sha" ]]; then
        printf '%s\n' "$tag_sha"
    else
        echo "error: could not infer commit from release target or tag $TAG" >&2
        return 1
    fi
}

infer_line_channel_from_tag
if [[ -z "$TAG" ]]; then
    TAG="$(derive_tag)" || {
        echo "error: --tag is required unless --line and --channel derive one" >&2
        exit 1
    }
fi
infer_line_channel_from_tag

case "$LINE" in
    runtime|apps) ;;
    *) echo "error: --line must be one of: runtime, apps" >&2; exit 1 ;;
esac
case "$CHANNEL" in
    stable|nightly) ;;
    *) echo "error: --channel must be one of: stable, nightly" >&2; exit 1 ;;
esac

if [[ -z "$REPO" ]]; then
    command -v gh >/dev/null 2>&1 || {
        echo "error: --repo is required when gh is unavailable" >&2
        exit 1
    }
    REPO="$(gh repo view --json nameWithOwner -q .nameWithOwner)"
fi

if [[ -z "$VERSION" ]]; then
    VERSION="$(infer_version)"
fi
if [[ -z "$COMMIT" ]]; then
    COMMIT="$(infer_commit)"
fi
if [[ -n "$NOTES_FILE" && ! -f "$NOTES_FILE" ]]; then
    echo "error: --notes-file does not exist: $NOTES_FILE" >&2
    exit 1
fi

case "$LINE:$CHANNEL" in
    runtime:stable)
        TITLE="Orgasmic Runtime $VERSION"
        NOTES="Runtime bundles $VERSION from $COMMIT."
        LATEST="true"
        PRERELEASE="false"
        ;;
    runtime:nightly)
        TITLE="Orgasmic Runtime Nightly"
        NOTES="Runtime bundles $VERSION from $COMMIT."
        LATEST="false"
        PRERELEASE="true"
        ;;
    apps:stable)
        TITLE="Orgasmic Apps $VERSION"
        NOTES="App builds $VERSION from $COMMIT."
        LATEST="false"
        PRERELEASE="false"
        ;;
    apps:nightly)
        TITLE="Orgasmic Apps Nightly"
        NOTES="App builds $VERSION from $COMMIT."
        LATEST="false"
        PRERELEASE="true"
        ;;
    *) echo "error: unsupported release metadata tuple: $LINE/$CHANNEL" >&2; exit 1 ;;
esac

if [[ "$DRY_RUN" == "1" ]]; then
    echo "DRY RUN: would sync release metadata"
    echo "repo=$REPO"
    echo "tag=$TAG"
    echo "line=$LINE"
    echo "channel=$CHANNEL"
    echo "version=$VERSION"
    echo "target=$COMMIT"
    echo "title=$TITLE"
    if [[ -n "$NOTES_FILE" ]]; then
        echo "notes_file=$NOTES_FILE"
    else
        echo "notes=$NOTES"
    fi
    echo "latest=$LATEST"
    echo "prerelease=$PRERELEASE"
    exit 0
fi

command -v gh >/dev/null 2>&1 || {
    echo "error: required command not found: gh" >&2
    exit 1
}

flags=(--target "$COMMIT" --title "$TITLE" --latest="$LATEST" --prerelease="$PRERELEASE")
if [[ -n "$NOTES_FILE" ]]; then
    flags+=(--notes-file "$NOTES_FILE")
else
    flags+=(--notes "$NOTES")
fi
if gh release view "$TAG" -R "$REPO" >/dev/null 2>&1; then
    gh release edit "$TAG" -R "$REPO" "${flags[@]}" >/dev/null
else
    gh release create "$TAG" -R "$REPO" "${flags[@]}"
fi

echo "synced release metadata for $TAG: $TITLE"
