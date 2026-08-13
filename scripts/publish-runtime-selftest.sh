#!/usr/bin/env bash
# Hermetic candidate/manifest tests. No Cargo build, signing, network or GitHub
# mutation is performed.

set -euo pipefail
ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$ROOT"
WORK="$(mktemp -d "${TMPDIR:-/tmp}/orgasmic-runtime-candidate-test.XXXXXX")"
trap 'rm -rf "$WORK"' EXIT

mkdir -p "$WORK/source"
printf 'signed-arm-runtime\n' >"$WORK/source/orgasmic-runtime_darwin_aarch64.tar.gz"
ARM_SHA="$(shasum -a 256 "$WORK/source/orgasmic-runtime_darwin_aarch64.tar.gz" | awk '{print $1}')"
printf '%s  %s\n' "$ARM_SHA" orgasmic-runtime_darwin_aarch64.tar.gz \
    >"$WORK/source/orgasmic-runtime_darwin_aarch64.tar.gz.sha256"
cat >"$WORK/existing.json" <<'JSON'
{
  "version": "0.0.8",
  "channel": "stable",
  "runtimes": {
    "darwin-aarch64": {"url": "old-arm", "sha256": "old-arm-sha", "version": "0.0.8"},
    "linux-x86_64": {"url": "old-linux", "sha256": "old-linux-sha", "version": "0.0.7"}
  }
}
JSON

node scripts/runtime-candidate.mjs create \
    --source-dir "$WORK/source" --candidate-dir "$WORK/candidate" \
    --existing-manifest "$WORK/existing.json" --repo owner/repo --tag stable \
    --channel stable --version 0.0.9 \
    --commit 1111111111111111111111111111111111111111 \
    --tree 2222222222222222222222222222222222222222 \
    --toolchain rustc-1 --certification-context local/runtime-fast-certified \
    --codesign-requirement 'identifier "com.example" and anchor apple generic' >/dev/null
node scripts/runtime-candidate.mjs verify --candidate-dir "$WORK/candidate" >/dev/null

CANDIDATE_DIR="$WORK/candidate" ARM_SHA="$ARM_SHA" node <<'NODE'
const fs = require('node:fs');
const path = require('node:path');
const dir = process.env.CANDIDATE_DIR;
const manifest = JSON.parse(fs.readFileSync(path.join(dir, 'runtime-latest.json')));
const candidate = JSON.parse(fs.readFileSync(path.join(dir, 'candidate.json')));
if (manifest.runtimes['linux-x86_64'].url !== 'old-linux') throw new Error('unselected target was changed');
if (manifest.runtimes['linux-x86_64'].version !== '0.0.7') throw new Error('unselected version was changed');
const arm = manifest.runtimes['darwin-aarch64'];
if (arm.version !== '0.0.9' || arm.sha256 !== process.env.ARM_SHA) throw new Error('selected target was not updated');
if (arm.commit !== '1111111111111111111111111111111111111111') throw new Error('selected commit was not recorded');
if (!arm.url.includes(`orgasmic-runtime_0.0.9_darwin_aarch64_${process.env.ARM_SHA.slice(0, 12)}.tar.gz`)) {
  throw new Error('asset name is not immutable/content-addressed');
}
if (candidate.artifacts.length !== 1 || candidate.artifacts[0].target !== 'darwin-aarch64') {
  throw new Error('candidate artifact inventory is wrong');
}
NODE

NEXT="$(node scripts/runtime-candidate.mjs next-patch 0.0.18 0.0.7 0.1.2)"
[[ "$NEXT" == "0.1.3" ]] || { echo "selftest: next patch was $NEXT" >&2; exit 1; }

cat >"$WORK/Cargo.toml" <<'TOML'
[workspace]
members = []
[workspace.package]
version = "0.0.8"
edition = "2021"
[profile.dev]
debug = 1
TOML
for package in orgasmic-core orgasmic-daemon orgasmic-cli orgasmic-drivers; do
    printf '[[package]]\nname = "%s"\nversion = "0.0.8"\n\n' "$package" >>"$WORK/Cargo.lock"
done
node scripts/runtime-candidate.mjs stamp-workspace-version \
    --cargo-toml "$WORK/Cargo.toml" --cargo-lock "$WORK/Cargo.lock" --version 0.0.9
[[ "$(rg -c 'version = "0.0.9"' "$WORK/Cargo.toml")" == "1" ]]
[[ "$(rg -c 'version = "0.0.9"' "$WORK/Cargo.lock")" == "4" ]]

node scripts/runtime-candidate.mjs validate-version --manifest "$WORK/existing.json" \
    --channel stable --version 0.0.9 --targets darwin-aarch64
if node scripts/runtime-candidate.mjs validate-version --manifest "$WORK/existing.json" \
    --channel stable --version 0.0.8 --targets darwin-aarch64 >/dev/null 2>&1; then
    echo "selftest: same stable version passed preflight" >&2
    exit 1
fi

mkdir -p "$WORK/old-version-source"
cp "$WORK/source/"* "$WORK/old-version-source/"
if node scripts/runtime-candidate.mjs create \
    --source-dir "$WORK/old-version-source" --candidate-dir "$WORK/old-version-candidate" \
    --existing-manifest "$WORK/existing.json" --repo owner/repo --tag stable \
    --channel stable --version 0.0.8 \
    --commit 1111111111111111111111111111111111111111 \
    --tree 2222222222222222222222222222222222222222 \
    --toolchain rustc-1 --certification-context local/runtime-fast-certified \
    --codesign-requirement requirement >/dev/null 2>&1; then
    echo "selftest: same stable target version was accepted" >&2
    exit 1
fi

ARTIFACT="$(find "$WORK/candidate" -name '*.tar.gz' -print -quit)"
printf 'tampered\n' >>"$ARTIFACT"
if node scripts/runtime-candidate.mjs verify --candidate-dir "$WORK/candidate" >/dev/null 2>&1; then
    echo "selftest: tampered candidate was accepted" >&2
    exit 1
fi

bash -n scripts/publish-runtime.sh
bash -n scripts/certify-runtime-fast.sh
bash -n scripts/release-runtime-fast.sh 2>/dev/null || [[ ! -e scripts/release-runtime-fast.sh ]]
echo "publish-runtime selftest: PASS"
