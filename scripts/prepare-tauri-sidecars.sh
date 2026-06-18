#!/usr/bin/env bash
set -euo pipefail

profile="release"
target_triple=""

while [[ $# -gt 0 ]]; do
  case "$1" in
    --profile)
      profile="${2:-}"
      shift 2
      ;;
    --target)
      target_triple="${2:-}"
      shift 2
      ;;
    --debug)
      profile="debug"
      shift
      ;;
    --release)
      profile="release"
      shift
      ;;
    *)
      echo "unknown argument: $1" >&2
      exit 2
      ;;
  esac
done

if [[ "$profile" != "release" && "$profile" != "debug" ]]; then
  echo "--profile must be 'release' or 'debug'" >&2
  exit 2
fi

repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$repo_root"

if [[ -z "$target_triple" ]]; then
  target_triple="$(rustc -Vv | awk '/^host:/ { print $2 }')"
fi

if [[ -z "$target_triple" ]]; then
  echo "could not determine Rust target triple" >&2
  exit 1
fi

if [[ "$profile" == "release" ]]; then
  cargo build --release -p orgasmic-cli --target "$target_triple"
  binary="target/${target_triple}/release/orgasmic"
else
  cargo build -p orgasmic-cli --target "$target_triple"
  binary="target/${target_triple}/debug/orgasmic"
fi

if [[ ! -x "$binary" ]]; then
  echo "expected executable sidecar at $binary" >&2
  exit 1
fi

case "$target_triple" in
  *windows*) ext=".exe" ;;
  *) ext="" ;;
esac

mkdir -p src-tauri/binaries
install -m 0755 "$binary" "src-tauri/binaries/orgasmic-${target_triple}${ext}"
echo "prepared src-tauri/binaries/orgasmic-${target_triple}${ext}"
