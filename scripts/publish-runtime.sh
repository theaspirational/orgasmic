#!/usr/bin/env bash
# Local runtime candidate builder and publisher. The default target is the
# maintainer's current production platform (darwin-aarch64). A preview creates
# one immutable candidate; publication promotes those exact bytes without
# compiling, signing, or packaging them again.

set -euo pipefail

TAG=""
VERSION=""
CHANNEL="stable"
REPO="${ORGASMIC_RELEASE_REPO:-}"
GLIBC_FLOOR="2.17"
BUNDLE_ID="${ORGASMIC_CODESIGN_BUNDLE_ID:-com.theaspirational.orgasmic}"
SKIP_SMOKE="${ORGASMIC_PUBLISH_SKIP_SMOKE:-0}"
ALLOW_HEAD_MISMATCH="${ORGASMIC_PUBLISH_ALLOW_HEAD_MISMATCH:-0}"
DRY_RUN=0
ONLY="darwin-aarch64"
CANDIDATE=""
CANDIDATE_ROOT="dist/release-candidates"
CERTIFICATION_CONTEXT="local/release-certified"

usage() {
    cat <<'EOF'
Usage: bash scripts/publish-runtime.sh [options]

By default builds only darwin-aarch64. A dry-run persists an immutable release
candidate. Publish that exact candidate with --candidate; it is never rebuilt.

Options:
  --channel <stable|nightly>  Release channel (default: stable)
  --tag <tag>                 Release tag override
  --version <version>         Runtime version override
  --repo <owner/name>         GitHub repository
  --only <target[,target...]> Build selected targets (default: darwin-aarch64)
  --all-targets               Build all four local targets
  --glibc <version>           Linux glibc floor (default: 2.17)
  --skip-smoke                Skip target smoke tests
  --dry-run                   Build and persist a candidate without publishing
  --candidate <directory>     Publish an existing candidate without rebuilding
  --candidate-root <dir>      Candidate storage root
  --certification-context <c> Required stable commit status context
  -h, --help                  Show this help

Targets: darwin-aarch64, darwin-x86_64, linux-x86_64, linux-aarch64
EOF
}

while [[ $# -gt 0 ]]; do
    case "$1" in
        --channel) CHANNEL="$2"; shift 2 ;;
        --tag) TAG="$2"; shift 2 ;;
        --version) VERSION="$2"; shift 2 ;;
        --repo) REPO="$2"; shift 2 ;;
        --only) ONLY="$2"; shift 2 ;;
        --all-targets) ONLY="darwin-aarch64,darwin-x86_64,linux-x86_64,linux-aarch64"; shift ;;
        --glibc) GLIBC_FLOOR="$2"; shift 2 ;;
        --skip-smoke) SKIP_SMOKE=1; shift ;;
        --dry-run) DRY_RUN=1; shift ;;
        --candidate) CANDIDATE="$2"; shift 2 ;;
        --candidate-root) CANDIDATE_ROOT="$2"; shift 2 ;;
        --certification-context) CERTIFICATION_CONTEXT="$2"; shift 2 ;;
        -h|--help) usage; exit 0 ;;
        *) echo "unknown option: $1" >&2; usage >&2; exit 1 ;;
    esac
done

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$ROOT"
[[ -x "$HOME/.cargo/bin/cargo" ]] && PATH="$HOME/.cargo/bin:$PATH"

for cmd in git gh node shasum tar; do
    command -v "$cmd" >/dev/null 2>&1 || { echo "error: required command not found: $cmd" >&2; exit 1; }
done
[[ "$(uname -s)" == "Darwin" ]] || { echo "error: runtime candidates must be built or published from macOS" >&2; exit 1; }
case "$CHANNEL" in stable|nightly) ;; *) echo "error: invalid channel: $CHANNEL" >&2; exit 1 ;; esac
[[ -n "$TAG" ]] || { [[ "$CHANNEL" == "nightly" ]] && TAG="nightly" || TAG="stable"; }
[[ -n "$REPO" ]] || REPO="$(gh repo view --json nameWithOwner -q .nameWithOwner)"

json_field() {
    JSON_INPUT="$1" FIELD="$2" node -e '
      const value=JSON.parse(process.env.JSON_INPUT)[process.env.FIELD];
      if (value === undefined || value === null) process.exit(2);
      process.stdout.write(String(value));
    '
}

fetch_release_manifest() {
    local output="$1"
    if gh release view "$TAG" -R "$REPO" --json assets -q '.assets[].name' 2>/dev/null | grep -qx 'runtime-latest.json'; then
        gh release download "$TAG" -R "$REPO" -p runtime-latest.json -O "$output" --clobber
    else
        printf '{}\n' >"$output"
    fi
}

require_clean_pushed_head() {
    [[ -z "$(git status --porcelain)" ]] || {
        echo "error: working tree is dirty; commit or stash before building or publishing" >&2
        git status --short >&2
        exit 1
    }
    HEAD_SHA="$(git rev-parse HEAD)"
    TREE_SHA="$(git rev-parse 'HEAD^{tree}')"
    DEFAULT_BRANCH="$(git symbolic-ref --quiet --short HEAD || echo main)"
    if [[ "$ALLOW_HEAD_MISMATCH" != "1" ]]; then
        git fetch --quiet origin "$DEFAULT_BRANCH" || true
        REMOTE_SHA="$(git rev-parse "origin/$DEFAULT_BRANCH" 2>/dev/null || true)"
        [[ -n "$REMOTE_SHA" && "$HEAD_SHA" == "$REMOTE_SHA" ]] || {
            echo "error: HEAD $HEAD_SHA does not match origin/$DEFAULT_BRANCH ${REMOTE_SHA:-unknown}" >&2
            exit 1
        }
    fi
    echo "✓ clean, pushed tree $TREE_SHA at $HEAD_SHA"
}

assert_stable_certification() {
    if [[ "$CHANNEL" == "stable" ]]; then
        bash scripts/assert-ci-certified.sh \
            --repo "$REPO" \
            --sha "$HEAD_SHA" \
            --context "$CERTIFICATION_CONTEXT"
    fi
}

candidate_artifacts() {
    CANDIDATE_JSON="$1" node <<'NODE'
const candidate = JSON.parse(process.env.CANDIDATE_JSON);
for (const artifact of candidate.artifacts) {
  process.stdout.write(`${artifact.target}\t${artifact.filename}\t${artifact.sha256}\n`);
}
NODE
}

verify_candidate_signatures() {
    local candidate_dir="$1" metadata="$2" expected_requirement target filename digest stage actual incumbent
    expected_requirement="$(json_field "$metadata" codesignRequirement)"
    while IFS=$'\t' read -r target filename digest; do
        case "$target" in darwin-*) ;; *) continue ;; esac
        stage="$(mktemp -d "${TMPDIR:-/tmp}/orgasmic-candidate-signature.XXXXXX")"
        tar -xzf "$candidate_dir/$filename" -C "$stage"
        /usr/bin/codesign --verify --strict "$stage/bin/orgasmic"
        actual="$(/usr/bin/codesign -d -r- "$stage/bin/orgasmic" 2>&1 | sed -n 's/^designated => //p' | head -1)"
        rm -rf "$stage"
        [[ -n "$actual" && "$actual" == "$expected_requirement" ]] || {
            echo "error: candidate code identity changed for $target" >&2
            echo "  expected: $expected_requirement" >&2
            echo "  actual:   ${actual:-missing}" >&2
            exit 1
        }
        if [[ -x "$HOME/.orgasmic/bin/orgasmic" ]]; then
            incumbent="$(/usr/bin/codesign -d -r- "$HOME/.orgasmic/bin/orgasmic" 2>&1 | sed -n 's/^designated => //p' | head -1)"
            [[ -z "$incumbent" || "$actual" == "$incumbent" ]] || {
                echo "error: candidate code identity differs from the installed runtime" >&2
                echo "       publishing it could trigger new macOS file-access prompts" >&2
                echo "  installed: $incumbent" >&2
                echo "  candidate: $actual" >&2
                exit 1
            }
        fi
    done < <(candidate_artifacts "$metadata")
}

publish_candidate() {
    local candidate_dir="$1" metadata="$2" remote_manifest remote_hash expected_hash
    local target filename digest downloaded actual

    [[ "$(json_field "$metadata" commit)" == "$HEAD_SHA" ]] || { echo "error: candidate commit is not HEAD" >&2; exit 1; }
    [[ "$(json_field "$metadata" tree)" == "$TREE_SHA" ]] || { echo "error: candidate tree is not the current tree" >&2; exit 1; }
    [[ "$(json_field "$metadata" repo)" == "$REPO" ]] || { echo "error: candidate repository mismatch" >&2; exit 1; }
    [[ "$(json_field "$metadata" tag)" == "$TAG" ]] || { echo "error: candidate tag mismatch" >&2; exit 1; }
    [[ "$(json_field "$metadata" channel)" == "$CHANNEL" ]] || { echo "error: candidate channel mismatch" >&2; exit 1; }
    [[ "$(json_field "$metadata" certificationContext)" == "$CERTIFICATION_CONTEXT" ]] || {
        echo "error: candidate certification context mismatch" >&2; exit 1;
    }
    VERSION="$(json_field "$metadata" version)"
    verify_candidate_signatures "$candidate_dir" "$metadata"

    remote_manifest="$(mktemp "${TMPDIR:-/tmp}/orgasmic-remote-manifest.XXXXXX")"
    fetch_release_manifest "$remote_manifest"
    remote_hash="$(node scripts/runtime-candidate.mjs hash --file "$remote_manifest")"
    expected_hash="$(json_field "$metadata" existingManifestSha256)"
    rm -f "$remote_manifest"
    [[ "$remote_hash" == "$expected_hash" ]] || {
        echo "error: $TAG manifest changed after candidate creation; rebuild the candidate" >&2
        echo "  candidate base: $expected_hash" >&2
        echo "  current remote: $remote_hash" >&2
        exit 1
    }

    # This may create the release, but the install authority remains the manifest,
    # which is deliberately uploaded last.
    bash scripts/sync-release-metadata.sh \
        --repo "$REPO" --tag "$TAG" --line runtime --channel "$CHANNEL" \
        --version "$VERSION" --commit "$HEAD_SHA"
    git push -f origin "$HEAD_SHA:refs/tags/$TAG" >/dev/null 2>&1 \
        || echo "warning: could not move $TAG tag to $HEAD_SHA" >&2

    echo "=== uploading immutable runtime assets ==="
    while IFS=$'\t' read -r target filename digest; do
        gh release upload "$TAG" -R "$REPO" \
            "$candidate_dir/$filename" "$candidate_dir/$filename.sha256" --clobber
        downloaded="$(mktemp "${TMPDIR:-/tmp}/orgasmic-published-asset.XXXXXX")"
        gh release download "$TAG" -R "$REPO" -p "$filename" -O "$downloaded" --clobber
        actual="$(node scripts/runtime-candidate.mjs hash --file "$downloaded")"
        rm -f "$downloaded"
        [[ "$actual" == "$digest" ]] || { echo "error: remote checksum mismatch for $filename" >&2; exit 1; }
        echo "✓ uploaded and verified $target ($digest)"
    done < <(candidate_artifacts "$metadata")

    echo "=== switching $TAG manifest ==="
    gh release upload "$TAG" -R "$REPO" "$candidate_dir/runtime-latest.json" --clobber
    remote_manifest="$(mktemp "${TMPDIR:-/tmp}/orgasmic-published-manifest.XXXXXX")"
    gh release download "$TAG" -R "$REPO" -p runtime-latest.json -O "$remote_manifest" --clobber
    remote_hash="$(node scripts/runtime-candidate.mjs hash --file "$remote_manifest")"
    rm -f "$remote_manifest"
    [[ "$remote_hash" == "$(json_field "$metadata" proposedManifestSha256)" ]] || {
        echo "error: published manifest checksum mismatch" >&2; exit 1;
    }
    bash scripts/refresh-release-publication.sh \
        --repo "$REPO" --tag "$TAG" --line runtime --channel "$CHANNEL"
    echo "✓ published candidate $candidate_dir ($VERSION)"
}

require_clean_pushed_head

if [[ -n "$CANDIDATE" ]]; then
    [[ "$DRY_RUN" == "0" ]] || { echo "error: --candidate and --dry-run are mutually exclusive" >&2; exit 1; }
    CANDIDATE="$(cd "$CANDIDATE" && pwd)"
    CANDIDATE_JSON="$(node scripts/runtime-candidate.mjs verify --candidate-dir "$CANDIDATE")"
    CHANNEL="$(json_field "$CANDIDATE_JSON" channel)"
    TAG="$(json_field "$CANDIDATE_JSON" tag)"
    REPO="$(json_field "$CANDIDATE_JSON" repo)"
    CERTIFICATION_CONTEXT="$(json_field "$CANDIDATE_JSON" certificationContext)"
    assert_stable_certification
    publish_candidate "$CANDIDATE" "$CANDIDATE_JSON"
    exit 0
fi

assert_stable_certification
for cmd in cargo rustc rustup npm; do
    command -v "$cmd" >/dev/null 2>&1 || { echo "error: required build command not found: $cmd" >&2; exit 1; }
done
[[ -n "$VERSION" ]] || VERSION="$(sed -n '/^\[workspace.package\]/,/^\[/{ s/^version = "\([^"]*\)".*/\1/p; }' Cargo.toml | head -1)"
[[ -n "$VERSION" ]] || { echo "error: could not resolve runtime version" >&2; exit 1; }
if [[ "$CHANNEL" == "nightly" && "$VERSION" != *-nightly.* ]]; then
    # shellcheck disable=SC2016 # JavaScript template literals are intentional.
    VERSION="$(BASE="$VERSION" node -e 'const b=process.env.BASE,d=new Date(),D=`${d.getUTCFullYear()}${String(d.getUTCMonth()+1).padStart(2,"0")}${String(d.getUTCDate()).padStart(2,"0")}`;process.stdout.write(`${b}-nightly.${D}.${Math.floor(d.getTime()/1000)}`)')"
fi

ALL_TARGETS=(
    "aarch64-apple-darwin|darwin-aarch64|native"
    "x86_64-apple-darwin|darwin-x86_64|rosetta"
    "x86_64-unknown-linux-gnu|linux-x86_64|docker:linux/amd64"
    "aarch64-unknown-linux-gnu|linux-aarch64|docker:linux/arm64"
)
TARGETS=()
remaining_targets="$ONLY"
while [[ -n "$remaining_targets" ]]; do
    requested="${remaining_targets%%,*}"
    if [[ "$remaining_targets" == *,* ]]; then remaining_targets="${remaining_targets#*,}"; else remaining_targets=""; fi
    matched=0
    for entry in "${ALL_TARGETS[@]}"; do
        IFS='|' read -r _triple key _smoke <<<"$entry"
        if [[ "$requested" == "$key" ]]; then TARGETS+=("$entry"); matched=1; break; fi
    done
    [[ "$matched" == "1" ]] || { echo "error: unsupported target '$requested'" >&2; exit 1; }
done
[[ "${#TARGETS[@]}" -gt 0 ]] || { echo "error: no targets selected" >&2; exit 1; }

installed_targets="$(rustup target list --installed 2>/dev/null || true)"
needs_darwin=0; needs_linux=0; selection_slug=""
for entry in "${TARGETS[@]}"; do
    IFS='|' read -r triple key _smoke <<<"$entry"
    selection_slug="${selection_slug:+$selection_slug+}$key"
    [[ "$triple" == "$(rustc -Vv | awk '/^host:/ {print $2}')" ]] \
        || printf '%s\n' "$installed_targets" | grep -qx "$triple" \
        || { echo "error: Rust target '$triple' is not installed" >&2; exit 1; }
    case "$key" in darwin-*) needs_darwin=1 ;; linux-*) needs_linux=1 ;; esac
done
if [[ "$needs_linux" == "1" ]]; then
    cargo zigbuild --help >/dev/null 2>&1 \
        || { echo "error: cargo-zigbuild is required for Linux targets" >&2; exit 1; }
fi
if [[ "$needs_darwin" == "1" ]]; then
    for cmd in security openssl codesign; do
        command -v "$cmd" >/dev/null 2>&1 || { echo "error: required signing command not found: $cmd" >&2; exit 1; }
    done
fi

CANDIDATE_DIR="$CANDIDATE_ROOT/$CHANNEL/$VERSION/$HEAD_SHA/$selection_slug"
if [[ -e "$CANDIDATE_DIR" ]]; then
    echo "error: candidate already exists: $CANDIDATE_DIR" >&2
    echo "       publish it with --candidate, or remove it before rebuilding" >&2
    exit 1
fi

mkdir -p "$ROOT/dist"
WORK_DIR="$(mktemp -d "$ROOT/dist/.runtime-build.XXXXXX")"
EXISTING_MANIFEST="$WORK_DIR/existing-runtime-latest.json"
fetch_release_manifest "$EXISTING_MANIFEST"
node scripts/runtime-candidate.mjs validate-version \
    --manifest "$EXISTING_MANIFEST" --channel "$CHANNEL" \
    --version "$VERSION" --targets "$ONLY"

KEYCHAIN=""
RESTORE_VERSION_FILES=0
ORIG_KEYCHAINS=""
cleanup() {
    rm -rf "${WORK_DIR:-}"
    if [[ -n "$KEYCHAIN" ]]; then
        # shellcheck disable=SC2086
        [[ -n "$ORIG_KEYCHAINS" ]] && security list-keychains -d user -s $ORIG_KEYCHAINS >/dev/null 2>&1 || true
        security delete-keychain "$KEYCHAIN" >/dev/null 2>&1 || true
    fi
    if [[ "$RESTORE_VERSION_FILES" == "1" ]]; then
        git checkout -- Cargo.toml Cargo.lock 2>/dev/null || true
    fi
}
trap cleanup EXIT

CODESIGN_REQUIREMENT="not-applicable"
if [[ "$needs_darwin" == "1" ]]; then
    P12="$HOME/.tauri/orgasmic-codesign.p12"
    P12_PW_FILE="$HOME/.tauri/orgasmic-codesign.p12.password"
    if [[ ! -f "$P12" || ! -f "$P12_PW_FILE" ]]; then
        [[ "$CHANNEL" != "stable" ]] || { echo "error: stable Darwin candidate requires the persistent signing identity" >&2; exit 1; }
        echo "warning: signing identity unavailable; Darwin nightly remains ad-hoc signed" >&2
    else
        ORIG_KEYCHAINS="$(security list-keychains -d user | sed 's/"//g' | xargs || true)"
        KEYCHAIN="${TMPDIR:-/tmp}/orgasmic-publish.$$.keychain-db"
        KCPW="$(openssl rand -hex 16)"
        security create-keychain -p "$KCPW" "$KEYCHAIN"
        security set-keychain-settings -lut 21600 "$KEYCHAIN"
        security unlock-keychain -p "$KCPW" "$KEYCHAIN"
        security import "$P12" -k "$KEYCHAIN" -P "$(cat "$P12_PW_FILE")" -T /usr/bin/codesign -A
        security set-key-partition-list -S apple-tool:,apple:,codesign: -s -k "$KCPW" "$KEYCHAIN" >/dev/null
        # shellcheck disable=SC2086
        security list-keychains -d user -s "$KEYCHAIN" $ORIG_KEYCHAINS >/dev/null
        CODESIGN_IDENTITY="$(security find-identity -v -p codesigning "$KEYCHAIN" | awk 'NR==1{print $2}')"
        [[ -n "$CODESIGN_IDENTITY" ]] || { echo "error: imported keychain has no signing identity" >&2; exit 1; }
        export ORGASMIC_CODESIGN_KEYCHAIN="$KEYCHAIN"
        export ORGASMIC_CODESIGN_IDENTITY="$CODESIGN_IDENTITY"
        export ORGASMIC_CODESIGN_BUNDLE_ID="$BUNDLE_ID"
    fi
fi

SOURCE_VERSION="$(sed -n '/^\[workspace.package\]/,/^\[/{ s/^version = "\([^"]*\)".*/\1/p; }' Cargo.toml | head -1)"
if [[ "$VERSION" != "$SOURCE_VERSION" ]]; then
    RESTORE_VERSION_FILES=1
    node scripts/runtime-candidate.mjs stamp-workspace-version \
        --cargo-toml Cargo.toml --cargo-lock Cargo.lock --version "$VERSION"
    echo "→ temporarily stamped runtime version $VERSION (source files restored on exit)"
fi

echo "→ building embedded UI once"
npm --prefix ui run build
test -f ui/dist/index.html || { echo "error: ui/dist/index.html missing after build" >&2; exit 1; }
export ORGASMIC_UI_PREBUILT=1

OUT_DIR="$WORK_DIR/artifacts"
mkdir -p "$OUT_DIR"
for entry in "${TARGETS[@]}"; do
    IFS='|' read -r triple key _smoke <<<"$entry"
    echo "=== building $key ($triple) ==="
    bash scripts/package-runtime.sh --version "$VERSION" --target "$triple" \
        --target-key "$key" --glibc "$GLIBC_FLOOR" --out-dir "$OUT_DIR"
done

docker_ready=0
docker_info_bounded() {
    docker info >/dev/null 2>&1 & local probe=$!
    ( sleep "${ORGASMIC_PUBLISH_DOCKER_TIMEOUT:-10}"; kill "$probe" 2>/dev/null ) & local killer=$!
    if wait "$probe" 2>/dev/null; then kill "$killer" 2>/dev/null; wait "$killer" 2>/dev/null; return 0; fi
    wait "$killer" 2>/dev/null; return 1
}
smoke_one() {
    local key="$1" smoke="$2" asset tarball stage bin platform
    asset="$(printf '%s' "$key" | tr '-' '_')"; tarball="$OUT_DIR/orgasmic-runtime_${asset}.tar.gz"
    stage="$(mktemp -d "${TMPDIR:-/tmp}/orgasmic-runtime-smoke.XXXXXX")"
    tar -xzf "$tarball" -C "$stage"; bin="$stage/bin/orgasmic"
    case "$smoke" in
        native) "$bin" --version ;;
        rosetta) if arch -x86_64 /usr/bin/true >/dev/null 2>&1; then arch -x86_64 "$bin" --version; else file "$bin"; fi ;;
        docker:*)
            platform="${smoke#docker:}"
            [[ "$docker_ready" == "1" ]] || { echo "error: Docker is required to smoke selected Linux target $key" >&2; rm -rf "$stage"; return 1; }
            docker run --rm --platform "$platform" -v "$stage":/rt:ro debian:12-slim /rt/bin/orgasmic --version
            ;;
    esac
    rm -rf "$stage"
}
if [[ "$SKIP_SMOKE" != "1" ]]; then
    if [[ "$needs_linux" == "1" ]]; then
        command -v docker >/dev/null 2>&1 && docker_info_bounded && docker_ready=1
        [[ "$docker_ready" == "1" ]] || { echo "error: Docker is unavailable for selected Linux smoke tests" >&2; exit 1; }
    fi
    for entry in "${TARGETS[@]}"; do
        IFS='|' read -r _triple key smoke <<<"$entry"; smoke_one "$key" "$smoke"
    done
fi

for entry in "${TARGETS[@]}"; do
    IFS='|' read -r _triple key _smoke <<<"$entry"
    case "$key" in
        darwin-*)
            asset="$(printf '%s' "$key" | tr '-' '_')"
            stage="$(mktemp -d "${TMPDIR:-/tmp}/orgasmic-runtime-identity.XXXXXX")"
            tar -xzf "$OUT_DIR/orgasmic-runtime_${asset}.tar.gz" -C "$stage"
            /usr/bin/codesign --verify --strict "$stage/bin/orgasmic"
            requirement="$(/usr/bin/codesign -d -r- "$stage/bin/orgasmic" 2>&1 | sed -n 's/^designated => //p' | head -1)"
            rm -rf "$stage"
            [[ -n "$requirement" ]] || { echo "error: signed runtime has no designated requirement" >&2; exit 1; }
            if [[ "$CODESIGN_REQUIREMENT" == "not-applicable" ]]; then CODESIGN_REQUIREMENT="$requirement"; fi
            [[ "$requirement" == "$CODESIGN_REQUIREMENT" ]] || { echo "error: Darwin targets have different code identities" >&2; exit 1; }
            ;;
    esac
done

mkdir -p "$(dirname "$CANDIDATE_DIR")"
CANDIDATE_JSON="$(node scripts/runtime-candidate.mjs create \
    --source-dir "$OUT_DIR" --candidate-dir "$CANDIDATE_DIR" \
    --existing-manifest "$EXISTING_MANIFEST" --repo "$REPO" --tag "$TAG" \
    --channel "$CHANNEL" --version "$VERSION" --commit "$HEAD_SHA" --tree "$TREE_SHA" \
    --toolchain "$(rustc --version)" --certification-context "$CERTIFICATION_CONTEXT" \
    --codesign-requirement "$CODESIGN_REQUIREMENT")"
echo "✓ immutable candidate: $CANDIDATE_DIR"

if [[ "$DRY_RUN" == "1" ]]; then
    echo "✓ preview complete; publish without rebuilding:"
    echo "  bash scripts/publish-runtime.sh --candidate '$CANDIDATE_DIR'"
    exit 0
fi
publish_candidate "$CANDIDATE_DIR" "$CANDIDATE_JSON"
