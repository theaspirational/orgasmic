#!/usr/bin/env bash
# arch: arch_WZFAX.4
# orgasmic:arch_WZFAX.4, dec_B4147
# Local-first apps publish. Builds the maintainer-buildable app targets on one
# macOS host — the darwin-arm macOS app (.dmg + Tauri updater .app.tar.gz/.sig)
# and the android-aarch64 APK — signs them with the keys in ~/.tauri, then MERGES
# only the built targets onto the `apps` release (replacing just their assets,
# --latest=false so it never steals the runtime `stable` release's badge). This is
# the local mirror of release-macos.yml / release-android.yml; those CI workflows
# stay as the dispatch fallback. App version is the source-of-truth in
# src-tauri/tauri.conf.json (decoupled from the CLI / workspace Cargo.toml).
# See dec_B4147 (+ its amendment) and arch_WZFAX.4.

set -euo pipefail

TAG="apps"
TARGET="all"
REPO="${ORGASMIC_RELEASE_REPO:-}"
DRY_RUN=0
ALLOW_HEAD_MISMATCH="${ORGASMIC_PUBLISH_ALLOW_HEAD_MISMATCH:-0}"

# Toolchain locations (Homebrew-managed on this host). Overridable via env.
ANDROID_SDK_DEFAULT="/opt/homebrew/share/android-commandlinetools"
ANDROID_NDK_VERSION="${ANDROID_NDK_VERSION:-29.0.14206865}"

usage() {
    cat <<'EOF'
Usage: bash scripts/publish-apps.sh [options]

Builds + signs the macOS app and/or the Android APK locally and merges them onto
the `apps` release. App version comes from src-tauri/tauri.conf.json.

Options:
  --target <mac|android|all>  Which app(s) to build/publish (default: all)
  --tag <tag>                 Release tag to publish to (default: apps)
  --repo <owner/name>         GitHub repo (default: gh repo view / ORGASMIC_RELEASE_REPO)
  --dry-run                   Build + sign + stage, but do NOT touch the release
  -h, --help                  Show this help

Signing material (must exist in ~/.tauri):
  macOS updater : orgasmic-updater.key (+ .password)   -> TAURI_SIGNING_PRIVATE_KEY
  Android APK   : org-shell-android-upload.jks (+ .password), alias org-shell

Env escape hatches:
  ORGASMIC_PUBLISH_ALLOW_HEAD_MISMATCH=1   publish even if HEAD != origin tip
  ANDROID_SDK_ROOT / ANDROID_HOME          override the Android SDK location
  ANDROID_NDK_VERSION                       NDK to use (default: 29.0.14206865)
EOF
}

while [[ $# -gt 0 ]]; do
    case "$1" in
        --target) TARGET="$2"; shift 2 ;;
        --tag) TAG="$2"; shift 2 ;;
        --repo) REPO="$2"; shift 2 ;;
        --dry-run) DRY_RUN=1; shift ;;
        -h|--help) usage; exit 0 ;;
        *) echo "unknown option: $1" >&2; usage >&2; exit 1 ;;
    esac
done

case "$TARGET" in
    mac|android|all) ;;
    *) echo "error: --target must be one of: mac, android, all" >&2; exit 1 ;;
esac
BUILD_MAC=0; BUILD_ANDROID=0
[[ "$TARGET" == "mac" || "$TARGET" == "all" ]] && BUILD_MAC=1
[[ "$TARGET" == "android" || "$TARGET" == "all" ]] && BUILD_ANDROID=1

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$ROOT"

# Prefer the rustup-managed toolchain. `tauri android build` and the darwin app
# build need the per-target std libs that `rustup target add` installs; a
# Homebrew/system cargo only carries the host target. The android npm scripts
# already prepend ~/.cargo/bin, but the mac path benefits too.
if [[ -x "$HOME/.cargo/bin/cargo" ]]; then
    PATH="$HOME/.cargo/bin:$PATH"
fi

for cmd in git gh node npm cargo rustc shasum; do
    command -v "$cmd" >/dev/null 2>&1 || { echo "error: required command not found: $cmd" >&2; exit 1; }
done

if [[ "$(uname -s)" != "Darwin" ]]; then
    echo "error: publish-apps.sh builds + signs the macOS app and must run on macOS" >&2
    exit 1
fi

test -d ui/node_modules || { echo "error: ui/node_modules missing; run: npm --prefix ui install" >&2; exit 1; }

installed_targets="$(rustup target list --installed 2>/dev/null || true)"
need_target() {
    printf '%s\n' "$installed_targets" | grep -qx "$1" || {
        echo "error: rust target '$1' is not installed ($(rustc --version 2>/dev/null))" >&2
        echo "       run: rustup target add $1" >&2
        exit 1
    }
}
[[ "$BUILD_MAC" == "1" ]] && need_target aarch64-apple-darwin
[[ "$BUILD_ANDROID" == "1" ]] && need_target aarch64-linux-android

if [[ -z "$REPO" ]]; then
    REPO="$(gh repo view --json nameWithOwner -q .nameWithOwner)"
fi

# App version is the source-of-truth in tauri.conf.json (NOT the workspace
# Cargo.toml). To release a new app version: bump tauri.conf.json, commit, push,
# then run this script — keeps the published artifacts reproducible from history.
VERSION="$(node -e 'process.stdout.write(JSON.parse(require("fs").readFileSync("src-tauri/tauri.conf.json","utf8")).version||"")')"
if [[ ! "$VERSION" =~ ^[0-9]+\.[0-9]+\.[0-9]+([-.+].*)?$ ]]; then
    echo "error: invalid app version in src-tauri/tauri.conf.json: '$VERSION'" >&2
    exit 1
fi

echo "→ repo    = $REPO"
echo "→ tag     = $TAG"
echo "→ version = $VERSION"
echo "→ targets = $([[ $BUILD_MAC == 1 ]] && printf 'mac ')$([[ $BUILD_ANDROID == 1 ]] && printf 'android')"
[[ "$DRY_RUN" == "1" ]] && echo "→ DRY RUN (no release changes)"

# --- clean-tree + HEAD guard -------------------------------------------------
# A published app must correspond to a clean, pushed commit so the version +
# commit recorded in the manifests is reproducible from public history.
if [[ -n "$(git status --porcelain)" ]]; then
    echo "error: working tree is dirty; commit or stash before publishing" >&2
    git status --short >&2
    exit 1
fi
HEAD_SHA="$(git rev-parse HEAD)"
DEFAULT_BRANCH="$(git symbolic-ref --quiet --short HEAD || echo main)"
if [[ "$ALLOW_HEAD_MISMATCH" != "1" ]]; then
    git fetch --quiet origin "$DEFAULT_BRANCH" || true
    REMOTE_SHA="$(git rev-parse "origin/${DEFAULT_BRANCH}" 2>/dev/null || echo "")"
    if [[ -z "$REMOTE_SHA" || "$HEAD_SHA" != "$REMOTE_SHA" ]]; then
        echo "error: HEAD ($HEAD_SHA) does not match origin/${DEFAULT_BRANCH} (${REMOTE_SHA:-unknown})" >&2
        echo "       push your commit first, or set ORGASMIC_PUBLISH_ALLOW_HEAD_MISMATCH=1 for testing" >&2
        exit 1
    fi
fi
echo "✓ clean tree at $HEAD_SHA"

# The android build stamps versionCode into a tracked file; restore the tree on
# exit so a published run leaves no local drift behind.
ANDROID_CONF="src-tauri/tauri.android.conf.json"
RESTORE_ANDROID_TREE=0
cleanup() {
    if [[ "$RESTORE_ANDROID_TREE" == "1" ]]; then
        git checkout -- "$ANDROID_CONF" 2>/dev/null || true
        git checkout -- src-tauri/gen/android 2>/dev/null || true
    fi
}
trap cleanup EXIT

OUT_DIR="dist/apps"
rm -rf "$OUT_DIR"
mkdir -p "$OUT_DIR"

# track which asset patterns each built target owns, for merge-not-clobber publish
MAC_BUILT=0
ANDROID_BUILT=0
ANDROID_APK_NAME=""

# --- macOS app ---------------------------------------------------------------
if [[ "$BUILD_MAC" == "1" ]]; then
    echo ""; echo "=== building macOS app (aarch64-apple-darwin) ==="
    UPDATER_KEY="$HOME/.tauri/orgasmic-updater.key"
    UPDATER_PW="$HOME/.tauri/orgasmic-updater.key.password"
    if [[ ! -f "$UPDATER_KEY" || ! -f "$UPDATER_PW" ]]; then
        echo "error: macOS updater signing key missing: $UPDATER_KEY (+ .password)" >&2
        exit 1
    fi
    # Tauri minisign updater signature (matches the CI secret). No Apple
    # notarization here — parity with release-macos.yml.
    export TAURI_SIGNING_PRIVATE_KEY="$(cat "$UPDATER_KEY")"
    export TAURI_SIGNING_PRIVATE_KEY_PASSWORD="$(cat "$UPDATER_PW")"

    # CI=1 makes tauri-bundler invoke create-dmg with --skip-jenkins, which skips
    # the Finder/AppleScript "prettify" pass in bundle_dmg.sh. That pass needs a
    # logged-in Aqua session and fails in a non-GUI/headless shell; GitHub's macOS
    # runner sets CI=true, so this just matches the CI build's DMG behavior.
    CI=true npm --prefix ui run tauri:bundle:mac

    bundle="src-tauri/target/aarch64-apple-darwin/release/bundle"
    dmg_path="$(find "$bundle/dmg" -name 'orgasmic_*_aarch64.dmg' -print -quit 2>/dev/null || true)"
    update_path="$bundle/macos/orgasmic.app.tar.gz"
    sig_path="${update_path}.sig"
    if [[ -z "$dmg_path" || ! -f "$update_path" || ! -f "$sig_path" ]]; then
        echo "error: expected macOS DMG + updater artifacts were not produced" >&2
        exit 1
    fi

    cp "$dmg_path" "$OUT_DIR/orgasmic_${VERSION}_aarch64.dmg"
    cp "$update_path" "$OUT_DIR/orgasmic.app.tar.gz"
    cp "$sig_path" "$OUT_DIR/orgasmic.app.tar.gz.sig"

    SIG_PATH="$OUT_DIR/orgasmic.app.tar.gz.sig" \
    MANIFEST_PATH="$OUT_DIR/latest.json" \
    UPDATE_URL="https://github.com/${REPO}/releases/download/${TAG}/orgasmic.app.tar.gz" \
    APPS_VERSION="$VERSION" COMMIT="$HEAD_SHA" node <<'NODE'
const fs = require('node:fs');
const manifest = {
  version: process.env.APPS_VERSION,
  notes: `macOS app ${process.env.APPS_VERSION} from ${process.env.COMMIT}`,
  pub_date: new Date().toISOString(),
  platforms: {
    'darwin-aarch64': {
      signature: fs.readFileSync(process.env.SIG_PATH, 'utf8').trim(),
      url: process.env.UPDATE_URL,
    },
  },
};
fs.writeFileSync(process.env.MANIFEST_PATH, `${JSON.stringify(manifest, null, 2)}\n`);
NODE
    MAC_BUILT=1
    echo "✓ staged macOS app ${VERSION}"
fi

# --- Android APK -------------------------------------------------------------
if [[ "$BUILD_ANDROID" == "1" ]]; then
    echo ""; echo "=== building Android APK (aarch64) ==="

    # SDK / NDK / JDK — self-set (do NOT rely on the user's shell env; a stale
    # ANDROID_HOME pointing at a nonexistent dir is exactly what we route around).
    SDK="${ANDROID_SDK_ROOT:-${ANDROID_HOME:-}}"
    if [[ -z "$SDK" || ! -d "$SDK/platform-tools" ]]; then
        SDK="$ANDROID_SDK_DEFAULT"
    fi
    if [[ ! -d "$SDK/platform-tools" ]]; then
        echo "error: Android SDK not found at '$SDK' (set ANDROID_SDK_ROOT)" >&2
        exit 1
    fi
    NDK_DIR="$SDK/ndk/$ANDROID_NDK_VERSION"
    if [[ ! -d "$NDK_DIR" ]]; then
        NDK_DIR="$(find "$SDK/ndk" -maxdepth 1 -mindepth 1 -type d 2>/dev/null | sort -V | tail -1 || true)"
    fi
    if [[ -z "$NDK_DIR" || ! -x "$NDK_DIR/ndk-build" ]]; then
        echo "error: Android NDK not found under $SDK/ndk (wanted $ANDROID_NDK_VERSION)" >&2
        exit 1
    fi
    JDK17="$(/usr/libexec/java_home -v 17 2>/dev/null || true)"
    if [[ -z "$JDK17" ]]; then
        echo "error: JDK 17 not found (/usr/libexec/java_home -v 17). Tauri's Gradle needs it." >&2
        exit 1
    fi
    export ANDROID_HOME="$SDK" ANDROID_SDK_ROOT="$SDK"
    export NDK_HOME="$NDK_DIR" ANDROID_NDK_HOME="$NDK_DIR"
    export JAVA_HOME="$JDK17"
    echo "→ SDK=$SDK"
    echo "→ NDK=$NDK_DIR"
    echo "→ JDK=$JDK17"

    JKS="$HOME/.tauri/org-shell-android-upload.jks"
    JKS_PW_FILE="$HOME/.tauri/org-shell-android-upload.password"
    if [[ ! -f "$JKS" || ! -f "$JKS_PW_FILE" ]]; then
        echo "error: Android upload keystore missing: $JKS (+ .password)" >&2
        exit 1
    fi
    mkdir -p src-tauri/gen/android
    {
        echo "keyAlias=org-shell"
        echo "password=$(cat "$JKS_PW_FILE")"
        echo "storeFile=$JKS"
    } > src-tauri/gen/android/keystore.properties   # gitignored

    # Monotonic versionCode from semver, stamped into the tracked android config.
    # Snapshot+restore (via the EXIT trap) keeps the working tree clean.
    RESTORE_ANDROID_TREE=1
    VERSION_CODE="$(VERSION="$VERSION" node <<'NODE'
const fs = require('node:fs');
const p = 'src-tauri/tauri.android.conf.json';
const m = /^(\d+)\.(\d+)\.(\d+)/.exec(process.env.VERSION);
const code = Number(m[1]) * 10000 + Number(m[2]) * 100 + Number(m[3]);
const cfg = JSON.parse(fs.readFileSync(p, 'utf8'));
cfg.bundle.android.versionCode = code;
fs.writeFileSync(p, `${JSON.stringify(cfg, null, 2)}\n`);
process.stdout.write(String(code));
NODE
)"
    echo "→ versionCode = $VERSION_CODE"

    npm --prefix ui run tauri:android:build

    apk_path="$(find src-tauri/gen/android/app/build/outputs/apk -path '*/release/*' -name '*.apk' -print -quit 2>/dev/null || true)"
    if [[ -z "$apk_path" ]]; then
        echo "error: expected Android release APK was not produced" >&2
        exit 1
    fi
    ANDROID_APK_NAME="orgasmic_${VERSION}_${VERSION_CODE}_android_aarch64.apk"
    cp "$apk_path" "$OUT_DIR/$ANDROID_APK_NAME"
    apk_sha="$(shasum -a 256 "$OUT_DIR/$ANDROID_APK_NAME" | awk '{print $1}')"

    APK_URL="https://github.com/${REPO}/releases/download/${TAG}/${ANDROID_APK_NAME}" \
    APK_SHA256="$apk_sha" MANIFEST_PATH="$OUT_DIR/android-latest.json" \
    APPS_VERSION="$VERSION" VERSION_CODE="$VERSION_CODE" TAG="$TAG" COMMIT="$HEAD_SHA" node <<'NODE'
const fs = require('node:fs');
const manifest = {
  channel: process.env.TAG,
  packageName: 'com.theaspirational.orgasmic',
  version: process.env.APPS_VERSION,
  versionCode: Number(process.env.VERSION_CODE),
  notes: `Android APK ${process.env.APPS_VERSION} from ${process.env.COMMIT}`,
  pubDate: new Date().toISOString(),
  apkUrl: process.env.APK_URL,
  apkSha256: process.env.APK_SHA256,
};
fs.writeFileSync(process.env.MANIFEST_PATH, `${JSON.stringify(manifest, null, 2)}\n`);
NODE
    ANDROID_BUILT=1
    echo "✓ staged Android APK ${VERSION} (code ${VERSION_CODE})"
fi

# --- publish (merge-not-clobber onto the apps release) -----------------------
echo ""; echo "=== staged assets ==="
ls -1 "$OUT_DIR"

# the asset names this run owns and will replace on the release
REPLACE=()
[[ "$MAC_BUILT" == "1" ]] && REPLACE+=("orgasmic_${VERSION}_aarch64.dmg" "orgasmic.app.tar.gz" "orgasmic.app.tar.gz.sig" "latest.json")
[[ "$ANDROID_BUILT" == "1" ]] && REPLACE+=("$ANDROID_APK_NAME" "android-latest.json")

if [[ "$DRY_RUN" == "1" ]]; then
    echo ""
    echo "→ DRY RUN: would publish to $TAG (target $HEAD_SHA, --latest=false) and replace:"
    for a in "${REPLACE[@]}"; do echo "    $a"; done
    # also delete any stale dmg/apk at OTHER versions for the built target(s)
    echo "✓ dry run complete (no release changes)"
    exit 0
fi

echo ""; echo "=== publishing to $TAG ==="
title="orgasmic apps ${VERSION}"
notes="orgasmic app builds ${VERSION} from ${HEAD_SHA}."
# --latest=false: apps own the dedicated `apps` release; the runtime `stable`
# release keeps the "latest" badge.
if gh release view "$TAG" -R "$REPO" >/dev/null 2>&1; then
    gh release edit "$TAG" -R "$REPO" --target "$HEAD_SHA" --title "$title" --notes "$notes" --latest=false >/dev/null
else
    gh release create "$TAG" -R "$REPO" --target "$HEAD_SHA" --title "$title" --notes "$notes" --latest=false
fi

# Delete the assets this run replaces — including stale dmg/apk at older versions
# for the built target(s) (their filenames carry the version). Assets for a target
# we did NOT build (e.g. android when --target mac) are left intact.
existing_assets="$(gh release view "$TAG" -R "$REPO" --json assets -q '.assets[].name')"
del() {
    printf '%s\n' "$existing_assets" | grep -qx "$1" && gh release delete-asset "$TAG" "$1" -R "$REPO" --yes || true
}
if [[ "$MAC_BUILT" == "1" ]]; then
    while IFS= read -r a; do
        case "$a" in orgasmic_*_aarch64.dmg|orgasmic.app.tar.gz|orgasmic.app.tar.gz.sig|latest.json) del "$a" ;; esac
    done <<<"$existing_assets"
fi
if [[ "$ANDROID_BUILT" == "1" ]]; then
    while IFS= read -r a; do
        case "$a" in orgasmic_*_android_*.apk|android-latest.json) del "$a" ;; esac
    done <<<"$existing_assets"
fi

gh release upload "$TAG" -R "$REPO" "$OUT_DIR"/* --clobber

echo ""
echo "✓ published apps to $TAG ($VERSION):"
[[ "$MAC_BUILT" == "1" ]] && echo "    macOS:   orgasmic_${VERSION}_aarch64.dmg, orgasmic.app.tar.gz(.sig), latest.json"
[[ "$ANDROID_BUILT" == "1" ]] && echo "    Android: $ANDROID_APK_NAME, android-latest.json"
