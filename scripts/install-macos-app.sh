#!/usr/bin/env bash
set -euo pipefail

repo="${ORGASMIC_REPO:-theaspirational/orgasmic}"
version="${ORGASMIC_VERSION:-latest}"
install_dir="${ORGASMIC_APP_DIR:-}"
dmg_source="${ORGASMIC_DMG:-}"
open_after_install=1

usage() {
  cat <<'USAGE'
Install the Apple Silicon orgasmic macOS tester app.

Usage:
  install-macos-app.sh [--version <tag>|--dmg <path-or-url>] [--install-dir <dir>] [--no-open]

Environment:
  ORGASMIC_REPO      GitHub repo, default: theaspirational/orgasmic
  ORGASMIC_VERSION   Release tag, default: latest
  ORGASMIC_DMG       Local path or URL to an orgasmic *_aarch64.dmg
  ORGASMIC_APP_DIR   Install directory, default: ~/Applications
USAGE
}

while [[ $# -gt 0 ]]; do
  case "$1" in
    --version)
      version="${2:-}"
      shift 2
      ;;
    --dmg)
      dmg_source="${2:-}"
      shift 2
      ;;
    --install-dir)
      install_dir="${2:-}"
      shift 2
      ;;
    --no-open)
      open_after_install=0
      shift
      ;;
    -h|--help)
      usage
      exit 0
      ;;
    *)
      echo "unknown argument: $1" >&2
      usage >&2
      exit 2
      ;;
  esac
done

if [[ "$(uname -s)" != "Darwin" ]]; then
  echo "orgasmic macOS app installer must run on macOS" >&2
  exit 1
fi

if [[ "$(uname -m)" != "arm64" ]]; then
  echo "prebuilt orgasmic tester app is Apple Silicon only for now" >&2
  echo "Use the contributor source setup on Intel Macs." >&2
  exit 1
fi

for tool in curl hdiutil ditto codesign file; do
  if ! command -v "$tool" >/dev/null 2>&1; then
    echo "required tool not found: $tool" >&2
    exit 1
  fi
done

if [[ -z "$install_dir" ]]; then
  install_dir="$HOME/Applications"
fi

mkdir -p "$install_dir"
install_dir="$(cd "$install_dir" && pwd)"

tmp_dir="$(mktemp -d)"
mount_dir="$tmp_dir/mount"
dmg_path="$tmp_dir/orgasmic.dmg"
app_path="$install_dir/orgasmic.app"
attached=0

cleanup() {
  if [[ "$attached" -eq 1 ]]; then
    hdiutil detach "$mount_dir" -quiet >/dev/null 2>&1 || true
  fi
  rm -rf "$tmp_dir"
}
trap cleanup EXIT

resolve_release_dmg_url() {
  local endpoint
  if [[ "$version" == "latest" ]]; then
    endpoint="https://api.github.com/repos/${repo}/releases/latest"
  else
    endpoint="https://api.github.com/repos/${repo}/releases/tags/${version}"
  fi

  curl -fsSL -H "Accept: application/vnd.github+json" "$endpoint" |
    awk -F'"' '/browser_download_url/ && /_aarch64\.dmg/ { print $4; exit }'
}

download_dmg() {
  if [[ -n "$dmg_source" ]]; then
    case "$dmg_source" in
      http://*|https://*)
        echo "Downloading $dmg_source"
        curl -fL --progress-bar "$dmg_source" -o "$dmg_path"
        ;;
      *)
        if [[ ! -f "$dmg_source" ]]; then
          echo "DMG not found: $dmg_source" >&2
          exit 1
        fi
        cp "$dmg_source" "$dmg_path"
        ;;
    esac
    return
  fi

  local url
  url="$(resolve_release_dmg_url)"
  if [[ -z "$url" ]]; then
    echo "could not find an Apple Silicon orgasmic DMG on ${repo} release ${version}" >&2
    exit 1
  fi

  echo "Downloading $url"
  curl -fL --progress-bar "$url" -o "$dmg_path"
}

download_dmg

mkdir -p "$mount_dir"
hdiutil attach "$dmg_path" -mountpoint "$mount_dir" -nobrowse -readonly -quiet
attached=1

mounted_app="$(find "$mount_dir" -maxdepth 2 -type d -name 'orgasmic.app' -print -quit)"
if [[ -z "$mounted_app" ]]; then
  echo "orgasmic.app not found inside DMG" >&2
  exit 1
fi

staged_app="$tmp_dir/orgasmic.app"
ditto "$mounted_app" "$staged_app"

if ! file "$staged_app/Contents/MacOS/orgasmic-desktop" | grep -q "arm64"; then
  echo "downloaded orgasmic app is not an arm64 build" >&2
  exit 1
fi

codesign --verify --deep --strict "$staged_app"

rm -rf "$app_path"
ditto "$staged_app" "$app_path"

# The terminal installer intentionally avoids browser-applied quarantine for the
# free/dev tester build. If a local DMG already carried quarantine metadata,
# remove it from the installed app so the result matches the curl install path.
xattr -dr com.apple.quarantine "$app_path" >/dev/null 2>&1 || true

echo "Installed orgasmic to $app_path"

if [[ "$open_after_install" -eq 1 ]]; then
  open "$app_path"
fi
