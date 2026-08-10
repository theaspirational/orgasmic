#!/usr/bin/env bash
# The release certification gate. Keep this in lockstep with CI's
# `release-certified` aggregate: a release is ready only when this script and
# the exact-head GitHub check agree on the same source tree.

set -euo pipefail

REPO=$(git rev-parse --show-toplevel 2>/dev/null) || {
    echo "certify-release: not inside a git worktree" >&2
    exit 2
}
cd "$REPO"

CERTIFICATION_RUST="1.97.1"
MSRV_RUST="1.87.0"
CERT_CARGO=(rustup run "$CERTIFICATION_RUST" cargo)
CERT_RUSTC=(rustup run "$CERTIFICATION_RUST" rustc)
MSRV_TARGET=$(mktemp -d "${TMPDIR:-/tmp}/orgasmic-msrv.XXXXXX") || {
    echo "certify-release: could not create the isolated MSRV target directory" >&2
    exit 2
}
trap 'rm -rf "$MSRV_TARGET"' EXIT

step() {
    printf '\n==> %s\n' "$1"
}

step "Verify the pinned certification toolchain"
rustup toolchain install "$CERTIFICATION_RUST" --profile minimal \
    --component clippy,rustfmt --no-self-update
selected_toolchain=$(rustup show active-toolchain | awk '{print $1}')
case "$selected_toolchain" in
    "$CERTIFICATION_RUST" | "$CERTIFICATION_RUST"-*) ;;
    *)
        echo "certify-release: rust-toolchain.toml must select $CERTIFICATION_RUST (selected $selected_toolchain)" >&2
        exit 1
        ;;
esac
actual_rust=$("${CERT_RUSTC[@]}" --version | awk '{print $2}')
if [ "$actual_rust" != "$CERTIFICATION_RUST" ]; then
    echo "certify-release: expected rustc $CERTIFICATION_RUST, found $actual_rust" >&2
    exit 1
fi

if [ -n "${ORGASMIC_ALLOW_BILLED_TESTS:-}" ]; then
    echo "certify-release: ORGASMIC_ALLOW_BILLED_TESTS must be unset; release certification never bills a provider turn" >&2
    exit 1
fi

step "Rust formatting"
"${CERT_CARGO[@]}" fmt --all --check

step "Strict Clippy"
"${CERT_CARGO[@]}" clippy --workspace --all-targets --keep-going -- -D warnings

step "Exact-head certification guard self-test"
bash scripts/assert-ci-certified-selftest.sh

step "Test classifier self-test"
bash scripts/run-tests-selftest.sh

step "Classified workspace suite"
bash scripts/run-tests.sh

step "Workspace MSRV ($MSRV_RUST)"
rustup toolchain install "$MSRV_RUST" --profile minimal --no-self-update
CARGO_TARGET_DIR="$MSRV_TARGET" rustup run "$MSRV_RUST" cargo check \
    --workspace --all-targets --locked

step "Install locked UI dependencies"
npm ci --prefix ui

step "UI typecheck"
npm --prefix ui run typecheck

step "UI tests"
npm --prefix ui test

step "Embedded runtime UI build"
npm --prefix ui run build

step "Tauri bootstrap UI build"
npm --prefix ui run build:bootstrap

step "Tauri application check"
"${CERT_CARGO[@]}" check --manifest-path src-tauri/Cargo.toml --all-targets --locked

printf '\nrelease certification: GREEN\n'
