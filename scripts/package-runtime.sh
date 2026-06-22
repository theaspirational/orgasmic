#!/usr/bin/env bash
# orgasmic:arch_WZFAX, dec_XSV21
# Package a prebuilt orgasmic runtime bundle.

set -euo pipefail

VERSION=""
TARGET_TRIPLE=""
TARGET_KEY=""
OUT_DIR="dist/runtime"
PROFILE="release"
# orgasmic:dec_B4147 — glibc floor for cargo-zigbuild Linux cross builds. Pinning
# an old floor (CentOS 7 / RHEL 7 era) lets one maintainer-host build run on old
# distros regardless of the host's own glibc.
GLIBC_FLOOR="2.17"

usage() {
    cat <<'EOF'
Usage: bash scripts/package-runtime.sh --version <version> [options]

Options:
  --target <rust-target>     Rust target triple (default: host)
  --target-key <key>         Runtime manifest key (default derived from target)
  --out-dir <dir>            Output directory (default: dist/runtime)
  --profile <profile>        Cargo profile (default: release)
  --glibc <version>          glibc floor for linux-gnu zigbuild (default: 2.17)
EOF
}

while [[ $# -gt 0 ]]; do
    case "$1" in
        --version) VERSION="$2"; shift 2 ;;
        --target) TARGET_TRIPLE="$2"; shift 2 ;;
        --target-key) TARGET_KEY="$2"; shift 2 ;;
        --out-dir) OUT_DIR="$2"; shift 2 ;;
        --profile) PROFILE="$2"; shift 2 ;;
        --glibc) GLIBC_FLOOR="$2"; shift 2 ;;
        -h|--help) usage; exit 0 ;;
        *) echo "unknown option: $1" >&2; usage >&2; exit 1 ;;
    esac
done

if [[ -z "$VERSION" ]]; then
    if git describe --tags --always >/dev/null 2>&1; then
        VERSION="$(git describe --tags --always)"
    else
        VERSION="0.0.0-dev"
    fi
fi

derive_target_key() {
    local triple="$1"
    case "$triple" in
        aarch64-apple-darwin) echo "darwin-aarch64" ;;
        x86_64-apple-darwin) echo "darwin-x86_64" ;;
        x86_64-unknown-linux-gnu|x86_64-unknown-linux-musl) echo "linux-x86_64" ;;
        aarch64-unknown-linux-gnu|aarch64-unknown-linux-musl) echo "linux-aarch64" ;;
        x86_64-pc-windows-msvc|x86_64-pc-windows-gnu) echo "windows-x86_64" ;;
        aarch64-pc-windows-msvc) echo "windows-aarch64" ;;
        *) echo "$triple" | sed 's/-unknown//g' ;;
    esac
}

if [[ -z "$TARGET_TRIPLE" ]]; then
    TARGET_TRIPLE="$(rustc -Vv | awk '/^host:/ {print $2}')"
fi
if [[ -z "$TARGET_KEY" ]]; then
    TARGET_KEY="$(derive_target_key "$TARGET_TRIPLE")"
fi

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
BUILD_DIR="$ROOT/target"
if [[ "$PROFILE" == "release" ]]; then
    CARGO_PROFILE_DIR="release"
else
    CARGO_PROFILE_DIR="$PROFILE"
fi

# orgasmic:dec_B4147 — Linux GNU targets cross-build through cargo-zigbuild with a
# pinned glibc floor; everything else uses the native cargo target compiler. The
# UI is reused (not rebuilt) per target when ORGASMIC_UI_PREBUILT=1 (handled in
# crates/orgasmic-daemon/build.rs), so a four-target publish runs npm only once.
case "$TARGET_TRIPLE" in
    *-unknown-linux-gnu)
        if ! command -v cargo-zigbuild >/dev/null 2>&1; then
            echo "error: cargo-zigbuild is required for $TARGET_TRIPLE" >&2
            echo "       install it with: cargo install cargo-zigbuild  (and: brew install zig)" >&2
            exit 1
        fi
        echo "→ building orgasmic CLI for $TARGET_TRIPLE via cargo-zigbuild (glibc $GLIBC_FLOOR)"
        cargo zigbuild --profile "$PROFILE" --package orgasmic-cli \
            --target "${TARGET_TRIPLE}.${GLIBC_FLOOR}"
        ;;
    *)
        echo "→ building orgasmic CLI for $TARGET_TRIPLE ($PROFILE)"
        cargo build --profile "$PROFILE" --package orgasmic-cli --target "$TARGET_TRIPLE"
        ;;
esac

# Windows binaries carry a .exe suffix; the bundle keeps it so the Windows
# installer finds bin/orgasmic.exe. POSIX targets stay bin/orgasmic.
EXE=""
case "$TARGET_TRIPLE" in *-pc-windows-*) EXE=".exe" ;; esac
BIN="$BUILD_DIR/$TARGET_TRIPLE/$CARGO_PROFILE_DIR/orgasmic$EXE"
if [[ ! -f "$BIN" ]]; then
    echo "error: built binary missing: $BIN" >&2
    exit 1
fi

STAGE="$(mktemp -d "${TMPDIR:-/tmp}/orgasmic-runtime.XXXXXX")"
trap 'rm -rf "$STAGE"' EXIT
mkdir -p "$STAGE/bin" "$STAGE/docs"
cp "$BIN" "$STAGE/bin/orgasmic$EXE"

# Optional: re-sign the binary with a stable code-signing identity so macOS keeps
# file-access (TCC) grants across versions instead of re-prompting on every build.
# Ad-hoc/linker signatures key TCC on the per-build cdhash; a fixed identity +
# identifier give a stable designated requirement. macOS-only, no-op unless
# ORGASMIC_CODESIGN_IDENTITY is set (so Linux/Windows legs and local builds skip).
# orgasmic:dec_B4147 — only sign apple-darwin (Mach-O) binaries. The local publish
# pipeline builds all four targets on one macOS host where codesign exists and the
# identity is set, so the Linux legs must not be handed to codesign.
case "$TARGET_TRIPLE" in *-apple-darwin) IS_DARWIN_TARGET=1 ;; *) IS_DARWIN_TARGET=0 ;; esac
if [[ "$IS_DARWIN_TARGET" == "1" && -n "${ORGASMIC_CODESIGN_IDENTITY:-}" ]] && command -v codesign >/dev/null 2>&1; then
    echo "→ codesigning bin/orgasmic as '${ORGASMIC_CODESIGN_IDENTITY}'"
    codesign --force \
        --identifier "${ORGASMIC_CODESIGN_BUNDLE_ID:-com.theaspirational.orgasmic}" \
        ${ORGASMIC_CODESIGN_KEYCHAIN:+--keychain "$ORGASMIC_CODESIGN_KEYCHAIN"} \
        --sign "$ORGASMIC_CODESIGN_IDENTITY" \
        "$STAGE/bin/orgasmic$EXE"
    codesign --verify --strict "$STAGE/bin/orgasmic$EXE"
    echo "→ designated requirement:"
    codesign -d -r- "$STAGE/bin/orgasmic$EXE" 2>&1 | sed 's/^/    /'
fi

cp -R "$ROOT/shipped" "$STAGE/shipped"
cp "$ROOT/README.md" "$STAGE/docs/README.md"
cp "$ROOT/CONTRIBUTING.md" "$STAGE/docs/CONTRIBUTING.md"

COMMIT="$(git rev-parse HEAD 2>/dev/null || true)"
BUILT_AT="$(date -u +%Y-%m-%dT%H:%M:%SZ)"
cat > "$STAGE/runtime-manifest.json" <<JSON
{
  "version": "${VERSION}",
  "target": "${TARGET_KEY}",
  "target_triple": "${TARGET_TRIPLE}",
  "commit": "${COMMIT}",
  "built_at": "${BUILT_AT}"
}
JSON

mkdir -p "$OUT_DIR"
ASSET_TARGET="$(printf '%s' "$TARGET_KEY" | tr '-' '_')"
OUT="$OUT_DIR/orgasmic-runtime_${VERSION}_${ASSET_TARGET}.tar.gz"
tar -czf "$OUT" -C "$STAGE" .

if command -v shasum >/dev/null 2>&1; then
    shasum -a 256 "$OUT" | awk '{print $1}' > "$OUT.sha256"
else
    sha256sum "$OUT" | awk '{print $1}' > "$OUT.sha256"
fi

echo "✓ runtime bundle: $OUT"
echo "✓ sha256: $(cat "$OUT.sha256")"
