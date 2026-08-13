#!/usr/bin/env bash
# Unattended stable runtime release for the current production platform. The
# command is an authorization boundary: after cheap hard gates pass it does not
# pause for per-change notes approval or another ship confirmation.

set -euo pipefail

NOTES_FILE=""
VERSION=""
REPO="${ORGASMIC_RELEASE_REPO:-}"
SKIP_INSTALL=0

usage() {
    cat <<'EOF'
Usage: bash scripts/release-runtime-fast.sh --notes-file <markdown> [options]

Options:
  --notes-file <file>  Final release notes; printed, then attached without approval
  --version <version>  Explicit version (default: next patch above source and ARM stable)
  --repo <owner/name>  GitHub repository
  --skip-install       Publish but do not update/restart the local daemon
  -h, --help           Show this help
EOF
}

while [[ $# -gt 0 ]]; do
    case "$1" in
        --notes-file) NOTES_FILE="$2"; shift 2 ;;
        --version) VERSION="$2"; shift 2 ;;
        --repo) REPO="$2"; shift 2 ;;
        --skip-install) SKIP_INSTALL=1; shift ;;
        -h|--help) usage; exit 0 ;;
        *) echo "release-runtime-fast: unknown option: $1" >&2; usage >&2; exit 2 ;;
    esac
done

[[ -n "$NOTES_FILE" && -s "$NOTES_FILE" ]] || {
    echo "release-runtime-fast: --notes-file must name non-empty final notes" >&2; exit 2;
}
ROOT="$(git rev-parse --show-toplevel 2>/dev/null)" || { echo "release-runtime-fast: not in a worktree" >&2; exit 2; }
cd "$ROOT"
[[ -z "$(git status --porcelain --untracked-files=all)" ]] || {
    echo "release-runtime-fast: worktree must be clean" >&2; git status --short >&2; exit 1;
}
[[ "$(git symbolic-ref --quiet --short HEAD)" == "main" ]] || {
    echo "release-runtime-fast: stable fast release must run from main" >&2; exit 1;
}
git fetch --quiet origin main
HEAD_SHA="$(git rev-parse HEAD)"
[[ "$HEAD_SHA" == "$(git rev-parse origin/main)" ]] || {
    echo "release-runtime-fast: HEAD is not origin/main" >&2; exit 1;
}
REPO="${REPO:-$(gh repo view --json nameWithOwner -q .nameWithOwner)}"

WORK="$(mktemp -d "${TMPDIR:-/tmp}/orgasmic-fast-release.XXXXXX")"
trap 'rm -rf "$WORK"' EXIT
gh release download stable -R "$REPO" -p runtime-latest.json \
    -O "$WORK/runtime-latest.json" --clobber
read_manifest_field() {
    MANIFEST="$WORK/runtime-latest.json" FIELD="$1" node -e '
      const fs=require("node:fs");
      const m=JSON.parse(fs.readFileSync(process.env.MANIFEST,"utf8"));
      let value;
      if (process.env.FIELD === "arm-version") value=m.runtimes?.["darwin-aarch64"]?.version || m.version;
      else if (process.env.FIELD === "arm-commit") value=m.runtimes?.["darwin-aarch64"]?.commit || m.commit;
      else value=m[process.env.FIELD];
      if (value) process.stdout.write(String(value));
    '
}
STABLE_VERSION="$(read_manifest_field arm-version)"
BASE_COMMIT="$(read_manifest_field arm-commit)"
[[ -n "$STABLE_VERSION" && -n "$BASE_COMMIT" ]] || {
    echo "release-runtime-fast: stable manifest lacks ARM version or commit" >&2; exit 1;
}
git cat-file -e "$BASE_COMMIT^{commit}" 2>/dev/null || git fetch --quiet origin "$BASE_COMMIT"
[[ "$HEAD_SHA" != "$BASE_COMMIT" ]] || {
    echo "release-runtime-fast: darwin-aarch64 already points at HEAD; refusing a no-source-change version bump" >&2
    exit 1
}
SOURCE_VERSION="$(sed -n '/^\[workspace.package\]/,/^\[/{ s/^version = "\([^"]*\)".*/\1/p; }' Cargo.toml | head -1)"
[[ -n "$VERSION" ]] || VERSION="$(node scripts/runtime-candidate.mjs next-patch "$SOURCE_VERSION" "$STABLE_VERSION")"

echo "=== unattended macOS ARM stable release ==="
echo "  repo:       $REPO"
echo "  commit:     $HEAD_SHA"
echo "  previous:   $STABLE_VERSION ($BASE_COMMIT)"
echo "  version:    $VERSION"
echo "  target:     darwin-aarch64"
echo "  approvals:  command authorization (no notes or ship prompts)"
echo ""
echo "=== final release notes ==="
cat "$NOTES_FILE"
echo "=== end release notes ==="

bash scripts/certify-runtime-fast.sh --repo "$REPO" --base "$BASE_COMMIT"

CANDIDATE="dist/release-candidates/stable/$VERSION/$HEAD_SHA/darwin-aarch64"
if [[ -d "$CANDIDATE" ]]; then
    echo "→ reusing existing immutable candidate $CANDIDATE"
    node scripts/runtime-candidate.mjs verify --candidate-dir "$CANDIDATE" >/dev/null
else
    bash scripts/publish-runtime.sh --channel stable --only darwin-aarch64 \
        --version "$VERSION" --repo "$REPO" --dry-run \
        --certification-context local/release-certified
fi
bash scripts/publish-runtime.sh --candidate "$CANDIDATE"
bash scripts/sync-release-metadata.sh --repo "$REPO" --tag stable --notes-file "$NOTES_FILE"

if [[ "$SKIP_INSTALL" == "1" ]]; then
    echo "✓ stable runtime published; local installation skipped"
    exit 0
fi

INSTALLED="$HOME/.orgasmic/bin/orgasmic"
[[ -x "$INSTALLED" ]] || { echo "release-runtime-fast: installed orgasmic binary is missing" >&2; exit 1; }
INSTALL_MODE="$(INSTALL_JSON="$HOME/.orgasmic/install.json" node -e '
  const fs=require("node:fs"); const p=process.env.INSTALL_JSON;
  try { process.stdout.write(JSON.parse(fs.readFileSync(p,"utf8")).mode || ""); } catch {}
')"
if [[ "$INSTALL_MODE" == "bundle" ]]; then
    "$INSTALLED" update --channel stable
else
    echo "→ migrating local contributor/source installation to the stable bundle"
    bash scripts/install.sh --channel stable
    "$INSTALLED" restart
fi

ACTUAL_VERSION="$($INSTALLED --version | awk '{print $NF}')"
[[ "$ACTUAL_VERSION" == "$VERSION" ]] || {
    echo "release-runtime-fast: installed version is $ACTUAL_VERSION, expected $VERSION" >&2; exit 1;
}
"$INSTALLED" daemon status
"$INSTALLED" status >/dev/null
echo "✓ fast release installed and daemon verified: $VERSION"
