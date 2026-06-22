#!/usr/bin/env bash
# arch: arch_WZFAX.3
# orgasmic:arch_WZFAX.3, dec_B4147
# Local-first runtime publish. Builds the four maintainer-buildable runtime
# targets on one macOS host (darwin x2 native+cross, linux x2 via cargo-zigbuild
# with a pinned glibc floor), signs the darwin binaries with the stable codesign
# identity from ~/.tauri, smoke-tests each, then MERGES only the built targets
# into the release's runtime-latest.json and replaces only their tarballs. Windows
# is refreshed separately by a manual runtime-bundles.yml dispatch and is left
# untouched here. This replaces the old tag-driven release-runtime.yml for stable.

set -euo pipefail

TAG="stable"
VERSION=""
REPO="${ORGASMIC_RELEASE_REPO:-}"
GLIBC_FLOOR="2.17"
BUNDLE_ID="${ORGASMIC_CODESIGN_BUNDLE_ID:-com.theaspirational.orgasmic}"
SKIP_SMOKE="${ORGASMIC_PUBLISH_SKIP_SMOKE:-0}"
DRY_RUN=0
ALLOW_HEAD_MISMATCH="${ORGASMIC_PUBLISH_ALLOW_HEAD_MISMATCH:-0}"

usage() {
    cat <<'EOF'
Usage: bash scripts/publish-runtime.sh [options]

Builds + signs + smoke-tests darwin-aarch64, darwin-x86_64, linux-x86_64 and
linux-aarch64, then merges them into the release's runtime-latest.json.

Options:
  --tag <tag>          Release tag to publish to (default: stable)
  --version <v>        Version to stamp (default: workspace.package version)
  --repo <owner/name>  GitHub repo (default: gh repo view / ORGASMIC_RELEASE_REPO)
  --glibc <version>    glibc floor for linux zigbuild (default: 2.17)
  --skip-smoke         Skip all smoke tests (not recommended)
  --dry-run            Build + sign + smoke + assemble manifest, but do NOT touch
                       the release (prints the planned asset changes)
  -h, --help           Show this help

Env escape hatches:
  ORGASMIC_PUBLISH_ALLOW_HEAD_MISMATCH=1   publish even if HEAD != origin tip
  ORGASMIC_PUBLISH_SKIP_SMOKE=1            same as --skip-smoke
EOF
}

while [[ $# -gt 0 ]]; do
    case "$1" in
        --tag) TAG="$2"; shift 2 ;;
        --version) VERSION="$2"; shift 2 ;;
        --repo) REPO="$2"; shift 2 ;;
        --glibc) GLIBC_FLOOR="$2"; shift 2 ;;
        --skip-smoke) SKIP_SMOKE=1; shift ;;
        --dry-run) DRY_RUN=1; shift ;;
        -h|--help) usage; exit 0 ;;
        *) echo "unknown option: $1" >&2; usage >&2; exit 1 ;;
    esac
done

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$ROOT"

for cmd in git gh node cargo rustc shasum tar; do
    command -v "$cmd" >/dev/null 2>&1 || { echo "error: required command not found: $cmd" >&2; exit 1; }
done

if [[ "$(uname -s)" != "Darwin" ]]; then
    echo "error: publish-runtime.sh builds + signs the darwin targets and must run on macOS" >&2
    exit 1
fi

if [[ -z "$REPO" ]]; then
    REPO="$(gh repo view --json nameWithOwner -q .nameWithOwner)"
fi
if [[ -z "$VERSION" ]]; then
    VERSION="$(sed -n '/^\[workspace.package\]/,/^\[/{ s/^version = "\([^"]*\)".*/\1/p; }' Cargo.toml | head -1)"
fi
if [[ -z "$VERSION" ]]; then
    echo "error: could not resolve version from Cargo.toml [workspace.package]" >&2
    exit 1
fi

echo "→ repo    = $REPO"
echo "→ tag     = $TAG"
echo "→ version = $VERSION"
echo "→ glibc   = $GLIBC_FLOOR"
[[ "$DRY_RUN" == "1" ]] && echo "→ DRY RUN (no release changes)"

# --- 1. clean-tree + HEAD guard ----------------------------------------------
# A published bundle must correspond to a clean, pushed commit so runtime-manifest
# (commit+version) is reproducible from public history.
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
        echo "       push your commit first, or set ORGASMIC_PUBLISH_ALLOW_HEAD_MISMATCH=1 for a test tag" >&2
        exit 1
    fi
fi
echo "✓ clean tree at $HEAD_SHA"

# --- 2. build the UI once ----------------------------------------------------
echo "→ building UI once (npm --prefix ui run build)"
npm --prefix ui run build
test -f ui/dist/index.html || { echo "error: ui/dist/index.html missing after build" >&2; exit 1; }
export ORGASMIC_UI_PREBUILT=1

# --- 3. import the codesign identity into a throwaway keychain ----------------
TAURI_DIR="${HOME}/.tauri"
P12="$TAURI_DIR/orgasmic-codesign.p12"
P12_PW_FILE="$TAURI_DIR/orgasmic-codesign.p12.password"
KEYCHAIN=""
ORIG_KEYCHAINS="$(security list-keychains -d user | sed 's/"//g' | xargs || true)"

cleanup() {
    if [[ -n "$KEYCHAIN" ]]; then
        # shellcheck disable=SC2086
        [[ -n "$ORIG_KEYCHAINS" ]] && security list-keychains -d user -s $ORIG_KEYCHAINS >/dev/null 2>&1 || true
        security delete-keychain "$KEYCHAIN" >/dev/null 2>&1 || true
    fi
}
trap cleanup EXIT

if [[ -f "$P12" && -f "$P12_PW_FILE" ]]; then
    echo "→ importing codesign identity from $P12"
    KEYCHAIN="${TMPDIR:-/tmp}/orgasmic-publish.$$.keychain-db"
    KCPW="$(openssl rand -hex 16)"
    security create-keychain -p "$KCPW" "$KEYCHAIN"
    security set-keychain-settings -lut 21600 "$KEYCHAIN"
    security unlock-keychain -p "$KCPW" "$KEYCHAIN"
    security import "$P12" -k "$KEYCHAIN" -P "$(cat "$P12_PW_FILE")" -T /usr/bin/codesign -A
    security set-key-partition-list -S apple-tool:,apple:,codesign: -s -k "$KCPW" "$KEYCHAIN" >/dev/null
    # Add (do not replace) the throwaway keychain to the search list so codesign
    # can resolve the identity by hash. Restored on exit.
    # shellcheck disable=SC2086
    security list-keychains -d user -s "$KEYCHAIN" $ORIG_KEYCHAINS >/dev/null
    CODESIGN_IDENTITY="$(security find-identity -v -p codesigning "$KEYCHAIN" | awk 'NR==1{print $2}')"
    if [[ -z "$CODESIGN_IDENTITY" ]]; then
        echo "error: no codesigning identity found in imported keychain" >&2
        exit 1
    fi
    export ORGASMIC_CODESIGN_KEYCHAIN="$KEYCHAIN"
    export ORGASMIC_CODESIGN_IDENTITY="$CODESIGN_IDENTITY"
    export ORGASMIC_CODESIGN_BUNDLE_ID="$BUNDLE_ID"
    echo "✓ codesign identity = $CODESIGN_IDENTITY"
else
    echo "warning: $P12 or its password file is missing; darwin binaries stay ad-hoc signed" >&2
fi

# --- 4. build the four targets ----------------------------------------------
OUT_DIR="dist/runtime"
rm -rf "$OUT_DIR"
mkdir -p "$OUT_DIR"

# triple key smoke
TARGETS=(
    "aarch64-apple-darwin|darwin-aarch64|native"
    "x86_64-apple-darwin|darwin-x86_64|rosetta"
    "x86_64-unknown-linux-gnu|linux-x86_64|docker:linux/amd64"
    "aarch64-unknown-linux-gnu|linux-aarch64|docker:linux/arm64"
)

for entry in "${TARGETS[@]}"; do
    IFS='|' read -r triple key _smoke <<<"$entry"
    echo ""
    echo "=== building $key ($triple) ==="
    bash scripts/package-runtime.sh \
        --version "$VERSION" \
        --target "$triple" \
        --target-key "$key" \
        --glibc "$GLIBC_FLOOR" \
        --out-dir "$OUT_DIR"
done

# --- 5. smoke tests ----------------------------------------------------------
docker_ready=0
ensure_docker() {
    command -v docker >/dev/null 2>&1 || return 1
    if docker info >/dev/null 2>&1; then docker_ready=1; return 0; fi
    echo "→ docker not running; attempting to start Docker.app"
    open -a Docker >/dev/null 2>&1 || true
    for _ in $(seq 1 30); do
        if docker info >/dev/null 2>&1; then docker_ready=1; return 0; fi
        sleep 3
    done
    return 1
}

smoke_one() {
    local key="$1" smoke="$2"
    local asset; asset="$(printf '%s' "$key" | tr '-' '_')"
    local tarball; tarball="$(ls "$OUT_DIR"/orgasmic-runtime_*_"${asset}".tar.gz 2>/dev/null | head -1)"
    [[ -f "$tarball" ]] || { echo "error: missing tarball for $key" >&2; return 1; }
    local stage; stage="$(mktemp -d)"
    tar -xzf "$tarball" -C "$stage"
    local bin="$stage/bin/orgasmic"
    test -f "$bin" || { echo "error: $key bundle missing bin/orgasmic" >&2; rm -rf "$stage"; return 1; }
    case "$smoke" in
        native)
            echo "→ smoke $key: native run"
            "$bin" --version
            ;;
        rosetta)
            if arch -x86_64 /usr/bin/true >/dev/null 2>&1; then
                echo "→ smoke $key: via Rosetta"
                arch -x86_64 "$bin" --version
            else
                echo "warning: Rosetta unavailable; verifying arch only for $key" >&2
                file "$bin"
            fi
            ;;
        docker:*)
            local platform="${smoke#docker:}"
            if [[ "$docker_ready" == "1" ]]; then
                echo "→ smoke $key: docker run ($platform)"
                docker run --rm --platform "$platform" -v "$stage":/rt:ro \
                    debian:12-slim /rt/bin/orgasmic --version
            else
                echo "warning: docker unavailable; SKIPPED linux smoke for $key (binary built but not run)" >&2
            fi
            ;;
    esac
    rm -rf "$stage"
}

if [[ "$SKIP_SMOKE" == "1" ]]; then
    echo ""; echo "→ smoke tests skipped (--skip-smoke)"
else
    echo ""; echo "=== smoke tests ==="
    ensure_docker || echo "warning: docker daemon not reachable; linux legs will be skipped" >&2
    for entry in "${TARGETS[@]}"; do
        IFS='|' read -r _triple key smoke <<<"$entry"
        smoke_one "$key" "$smoke"
    done
fi

# --- 6. merge manifest + replace only built tarballs -------------------------
echo ""; echo "=== assembling merged runtime-latest.json ==="
EXISTING="$OUT_DIR/.existing-runtime-latest.json"
if gh release view "$TAG" -R "$REPO" --json assets -q '.assets[].name' 2>/dev/null | grep -qx 'runtime-latest.json'; then
    gh release download "$TAG" -R "$REPO" -p runtime-latest.json -O "$EXISTING" --clobber
    echo "→ merging into existing $TAG runtime-latest.json"
else
    echo '{}' > "$EXISTING"
    echo "→ no existing manifest on $TAG; starting fresh"
fi

REPO="$REPO" TAG="$TAG" VERSION="$VERSION" COMMIT="$HEAD_SHA" OUT_DIR="$OUT_DIR" \
EXISTING="$EXISTING" node <<'NODE'
const fs = require('node:fs');
const { REPO, TAG, VERSION, COMMIT, OUT_DIR, EXISTING } = process.env;

let manifest = {};
try { manifest = JSON.parse(fs.readFileSync(EXISTING, 'utf8')) || {}; } catch { manifest = {}; }
if (!manifest.runtimes || typeof manifest.runtimes !== 'object') manifest.runtimes = {};

// Recover the target key from the asset suffix (os '-' arch; arch may contain '_').
const keyFromName = (f) => {
  const m = f.match(/^orgasmic-runtime_.+?_([a-z0-9]+)_(.+)\.tar\.gz$/);
  return m ? `${m[1]}-${m[2]}` : null;
};

const built = [];
for (const f of fs.readdirSync(OUT_DIR).filter((f) => f.endsWith('.tar.gz')).sort()) {
  const key = keyFromName(f);
  if (!key) continue;
  const sha = fs.readFileSync(`${OUT_DIR}/${f}.sha256`, 'utf8').trim().split(/\s+/)[0];
  manifest.runtimes[key] = {
    url: `https://github.com/${REPO}/releases/download/${TAG}/${f}`,
    sha256: sha,
    version: VERSION, // per-target version: a lagging target (e.g. windows) keeps its own
  };
  built.push(key);
}

manifest.version = VERSION; // top-level fallback for older installers
manifest.channel = TAG === 'stable' ? 'stable' : (manifest.channel || TAG);
manifest.commit = COMMIT;
manifest.pub_date = new Date().toISOString();

fs.writeFileSync(`${OUT_DIR}/runtime-latest.json`, `${JSON.stringify(manifest, null, 2)}\n`);
console.log(`built targets:    ${built.join(', ')}`);
console.log(`merged manifest:  ${Object.keys(manifest.runtimes).join(', ')}`);
NODE

rm -f "$EXISTING"
echo "=== runtime-latest.json ==="
cat "$OUT_DIR/runtime-latest.json"

# names of the tarballs we are replacing (only the built targets).
# (plain loop, not mapfile — macOS ships bash 3.2)
BUILT_TARBALLS=()
while IFS= read -r t; do
    [[ -n "$t" ]] && BUILT_TARBALLS+=("$t")
done < <(cd "$OUT_DIR" && ls orgasmic-runtime_*.tar.gz)

if [[ "$DRY_RUN" == "1" ]]; then
    echo ""
    echo "→ DRY RUN: would replace on $TAG:"
    for t in "${BUILT_TARBALLS[@]}"; do echo "    $t (+ .sha256)"; done
    echo "    runtime-latest.json"
    echo "✓ dry run complete (no release changes)"
    exit 0
fi

echo ""; echo "=== publishing to $TAG ==="
if ! gh release view "$TAG" -R "$REPO" >/dev/null 2>&1; then
    flags=(--title "orgasmic $TAG" --notes "Runtime bundles $VERSION ($HEAD_SHA)" --target "$HEAD_SHA")
    if [[ "$TAG" == "stable" ]]; then flags+=(--latest); else flags+=(--prerelease); fi
    gh release create "$TAG" -R "$REPO" "${flags[@]}"
fi

# Delete ONLY the assets we are replacing (built target tarballs + sha256 + the
# manifest). Other targets (e.g. windows-x86_64) are left intact — merge, not clobber.
existing_assets="$(gh release view "$TAG" -R "$REPO" --json assets -q '.assets[].name')"
delete_asset() {
    local name="$1"
    if printf '%s\n' "$existing_assets" | grep -qx "$name"; then
        gh release delete-asset "$TAG" "$name" -R "$REPO" --yes
    fi
}
for t in "${BUILT_TARBALLS[@]}"; do
    delete_asset "$t"
    delete_asset "$t.sha256"
done
delete_asset "runtime-latest.json"

upload=("$OUT_DIR/runtime-latest.json")
for t in "${BUILT_TARBALLS[@]}"; do
    upload+=("$OUT_DIR/$t" "$OUT_DIR/$t.sha256")
done
gh release upload "$TAG" -R "$REPO" "${upload[@]}" --clobber

echo ""
echo "✓ published ${#BUILT_TARBALLS[@]} runtime targets to $TAG ($VERSION)"
echo "  $(printf '%s ' "${BUILT_TARBALLS[@]}")"
