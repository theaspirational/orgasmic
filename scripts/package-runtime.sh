#!/usr/bin/env bash
# orgasmic:arch_WZFAX, dec_XSV21
# Package a prebuilt orgasmic runtime bundle.

set -euo pipefail

VERSION=""
TARGET_TRIPLE=""
TARGET_KEY=""
OUT_DIR="dist/runtime"
PROFILE="release"

usage() {
    cat <<'EOF'
Usage: bash scripts/package-runtime.sh --version <version> [options]

Options:
  --target <rust-target>     Rust target triple (default: host)
  --target-key <key>         Runtime manifest key (default derived from target)
  --out-dir <dir>            Output directory (default: dist/runtime)
  --profile <profile>        Cargo profile (default: release)
EOF
}

while [[ $# -gt 0 ]]; do
    case "$1" in
        --version) VERSION="$2"; shift 2 ;;
        --target) TARGET_TRIPLE="$2"; shift 2 ;;
        --target-key) TARGET_KEY="$2"; shift 2 ;;
        --out-dir) OUT_DIR="$2"; shift 2 ;;
        --profile) PROFILE="$2"; shift 2 ;;
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

echo "→ building orgasmic CLI for $TARGET_TRIPLE ($PROFILE)"
cargo build --profile "$PROFILE" --package orgasmic-cli --target "$TARGET_TRIPLE"

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
