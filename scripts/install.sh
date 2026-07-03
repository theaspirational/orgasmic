#!/usr/bin/env bash
# orgasmic:arch_WZFAX, dec_XSV21
# ============================================================================
# orgasmic installer
# ============================================================================
# Regular-user mode (default) installs a prebuilt runtime bundle:
#
#   $ORGASMIC_HOME
#     runtimes/<version>-<target>/
#     current -> runtimes/<version>-<target>
#     orgasmic -> current                 # compatibility content root
#     bin/orgasmic -> ../current/bin/orgasmic
#     user/ state/ sessions/ secrets/ logs/ config.yaml
#     install.json
#
# Contributor mode is explicit:
#
#   bash scripts/install.sh --from-source <checkout>
#
# Only contributor mode builds with cargo/npm or mutates a source checkout.
#
# Usage:
#   bash scripts/install.sh
#   bash scripts/install.sh --channel nightly
#   bash scripts/install.sh --version v0.1.0
#   bash scripts/install.sh --bundle /path/to/orgasmic-runtime_...tar.gz
#   bash scripts/install.sh --bundle https://.../orgasmic-runtime_...tar.gz --sha256 <hex>
#   bash scripts/install.sh --from-source /path/to/orgasmic
#
# After placing the binary, the installer wires $ORGASMIC_HOME/bin onto PATH via
# a managed env file ($ORGASMIC_HOME/env) sourced from your shell startup files,
# then verifies that `orgasmic` resolves in a fresh login shell. Pass
# --no-modify-path (or ORGASMIC_NO_MODIFY_PATH=1) to skip the shell-startup edit.
#
# Source-mode compatibility flags:
#   --branch, --repo, --dir, --no-build, --no-modify-path
# ============================================================================

set -euo pipefail

ORGASMIC_HOME="${ORGASMIC_HOME:-$HOME/.orgasmic}"
CHANNEL="${ORGASMIC_CHANNEL:-stable}"
VERSION="${ORGASMIC_VERSION:-}"
BUNDLE="${ORGASMIC_BUNDLE:-}"
BUNDLE_SHA256="${ORGASMIC_BUNDLE_SHA256:-}"
RELEASE_REPO="${ORGASMIC_RELEASE_REPO:-theaspirational/orgasmic}"

MODE="bundle"
FROM_SOURCE=""
INSTALL_DIR=""
BRANCH="${ORGASMIC_BRANCH:-main}"
REPO_URL="${ORGASMIC_REPO:-}"
REPO_SSH="git@github.com:theaspirational/orgasmic.git"
REPO_HTTPS="https://github.com/theaspirational/orgasmic.git"
DO_BUILD=true
INSTALL_WORK=""
NO_MODIFY_PATH="${ORGASMIC_NO_MODIFY_PATH:-}"

usage() {
    sed -n '/^# orgasmic installer/,/^# =====/p' "$0" | sed 's/^# \{0,1\}//'
}

select_source_mode() {
    MODE="source"
}

while [[ $# -gt 0 ]]; do
    case "$1" in
        --channel) CHANNEL="$2"; shift 2 ;;
        --version) VERSION="$2"; shift 2 ;;
        --bundle) BUNDLE="$2"; shift 2 ;;
        --sha256) BUNDLE_SHA256="$2"; shift 2 ;;
        --from-source) FROM_SOURCE="$2"; INSTALL_DIR="$2"; select_source_mode; shift 2 ;;
        --branch) BRANCH="$2"; select_source_mode; shift 2 ;;
        --repo) REPO_URL="$2"; select_source_mode; shift 2 ;;
        --dir) INSTALL_DIR="$2"; select_source_mode; shift 2 ;;
        --no-build) DO_BUILD=false; select_source_mode; shift ;;
        --no-modify-path) NO_MODIFY_PATH=1; shift ;;
        -h|--help) usage; exit 0 ;;
        *) echo "unknown option: $1" >&2; usage >&2; exit 1 ;;
    esac
done

target_key() {
    local os arch
    os="$(uname -s)"
    arch="$(uname -m)"
    case "${os}:${arch}" in
        Darwin:arm64) echo "darwin-aarch64" ;;
        Darwin:x86_64) echo "darwin-x86_64" ;;
        Linux:x86_64) echo "linux-x86_64" ;;
        Linux:aarch64|Linux:arm64) echo "linux-aarch64" ;;
        *) echo "$(printf '%s' "$os" | tr '[:upper:]' '[:lower:]')-${arch}" ;;
    esac
}

target_asset_suffix() {
    target_key | tr '-' '_'
}

require_cmd() {
    if ! command -v "$1" >/dev/null 2>&1; then
        echo "error: required command not found: $1" >&2
        exit 1
    fi
}

tar_supports_unknown_pax_warning_suppression() {
    tar --warning=no-unknown-keyword --version >/dev/null 2>&1
}

extract_runtime_tarball() {
    local bundle="$1" dest="$2"
    if tar_supports_unknown_pax_warning_suppression; then
        tar --warning=no-unknown-keyword -xzf "$bundle" -C "$dest"
    else
        tar -xzf "$bundle" -C "$dest"
    fi
}

extract_json_string() {
    local key="$1" file="$2"
    sed -nE "s/.*\"${key}\"[[:space:]]*:[[:space:]]*\"([^\"]+)\".*/\1/p" "$file" | head -1
}

extract_runtime_field() {
    local target="$1" field="$2" file="$3"
    awk -v target="\"${target}\"" -v field="\"${field}\"" '
        index($0, target) { in_target=1 }
        in_target && index($0, field) {
            line=$0
            sub(/^.*:[[:space:]]*"/, "", line)
            sub(/".*$/, "", line)
            print line
            exit
        }
        in_target && /^[[:space:]]*}[,]?[[:space:]]*$/ { in_target=0 }
    ' "$file"
}

sha256_file() {
    if command -v shasum >/dev/null 2>&1; then
        shasum -a 256 "$1" | awk '{print $1}'
    else
        sha256sum "$1" | awk '{print $1}'
    fi
}

fetch_to_file() {
    local src="$1" dest="$2"
    if [[ "$src" == http://* || "$src" == https://* ]]; then
        require_cmd curl
        curl -fsSL "$src" -o "$dest"
    elif [[ "$src" == file://* ]]; then
        cp "${src#file://}" "$dest"
    else
        cp "$src" "$dest"
    fi
}

resolve_asset_url() {
    local manifest_url="$1" asset_url="$2"
    if [[ "$asset_url" == http://* || "$asset_url" == https://* || "$asset_url" == file://* || "$asset_url" == /* ]]; then
        printf '%s\n' "$asset_url"
        return
    fi
    if [[ "$manifest_url" == http://* || "$manifest_url" == https://* ]]; then
        printf '%s/%s\n' "${manifest_url%/*}" "$asset_url"
    elif [[ "$manifest_url" == file://* ]]; then
        printf '%s/%s\n' "$(dirname "${manifest_url#file://}")" "$asset_url"
    else
        printf '%s/%s\n' "$(dirname "$manifest_url")" "$asset_url"
    fi
}

replace_symlink() {
    local target="$1" link="$2"
    mkdir -p "$(dirname "$link")"
    if [[ -e "$link" && ! -L "$link" && -d "$link" ]]; then
        echo "error: $link is a real directory; refusing to replace it with a runtime symlink" >&2
        exit 1
    fi
    local tmp="${link}.tmp.$$"
    rm -f "$tmp"
    ln -s "$target" "$tmp"
    mv -f "$tmp" "$link"
}

# Locate the freshly built binary, accounting for `--target`-qualified builds
# that land in target/<triple>/release/ instead of target/release/. Picks the
# newest candidate so plain and target-qualified builds both resolve.
resolve_source_binary() {
    local root="$1" newest="" f
    for f in "$root"/target/release/orgasmic "$root"/target/*/release/orgasmic; do
        [[ -f "$f" && -x "$f" ]] || continue
        if [[ -z "$newest" || "$f" -nt "$newest" ]]; then
            newest="$f"
        fi
    done
    [[ -n "$newest" ]] && printf '%s\n' "$newest"
}

# Wire $ORGASMIC_HOME/bin onto PATH using the binary's own managed env file.
ensure_path_on_shell() {
    local args=(path ensure)
    if [[ -n "$NO_MODIFY_PATH" ]]; then
        args+=(--no-modify-path)
    fi
    "$ORGASMIC_HOME/bin/orgasmic" "${args[@]}" || \
        echo "warning: could not wire PATH automatically; run 'orgasmic path ensure'" >&2
}

# Confirm `orgasmic` resolves in a fresh login shell; print remediation if not.
verify_on_path() {
    local login_shell="${SHELL:-/bin/sh}"
    if "$login_shell" -lc 'command -v orgasmic >/dev/null 2>&1' >/dev/null 2>&1; then
        echo "✓ orgasmic resolves on PATH in new shells"
    else
        echo ""
        echo "→ orgasmic is installed but not yet on PATH in this shell."
        echo "  Open a new terminal, or run now:"
        echo "      . \"$ORGASMIC_HOME/env\""
    fi
}

write_install_json_bundle() {
    local runtime_version="$1" runtime_target="$2" manifest_url="$3" runtime_dir="$4"
    local manifest_json="null"
    if [[ -n "$manifest_url" ]]; then
        manifest_json="\"$manifest_url\""
    fi
    cat > "$ORGASMIC_HOME/install.json" <<JSON
{
  "mode": "bundle",
  "channel": "${CHANNEL}",
  "version": "${runtime_version}",
  "target": "${runtime_target}",
  "manifest_url": ${manifest_json},
  "runtime_dir": "${runtime_dir}",
  "source_checkout": null
}
JSON
}

write_install_json_source() {
    local checkout="$1"
    cat > "$ORGASMIC_HOME/install.json" <<JSON
{
  "mode": "source",
  "channel": null,
  "version": null,
  "target": null,
  "manifest_url": null,
  "runtime_dir": null,
  "source_checkout": "${checkout}"
}
JSON
}

link_agent_skill() {
    local skill_src="$1"
    local skill_dest
    AGENT_SKILLS_DIR="${AGENT_SKILLS_DIR:-$HOME/.agents/skills}"
    skill_dest="$AGENT_SKILLS_DIR/orgasmic"
    if [[ ! -f "$skill_src/SKILL.md" ]]; then
        echo "error: shipped skill missing: $skill_src/SKILL.md" >&2
        exit 1
    fi
    mkdir -p "$AGENT_SKILLS_DIR"
    if [[ -L "$skill_dest" || ! -e "$skill_dest" ]]; then
        replace_symlink "$skill_src" "$skill_dest"
        echo "→ agent skill linked: $skill_dest -> $skill_src"
    elif [[ -f "$skill_dest/SKILL.md" ]] && head -8 "$skill_dest/SKILL.md" | grep -q '^name: orgasmic$'; then
        local skill_backup="$skill_dest.bak-$(date +%Y%m%d%H%M%S)"
        mv "$skill_dest" "$skill_backup"
        replace_symlink "$skill_src" "$skill_dest"
        echo "→ replaced stale skill copy with symlink (backup: $skill_backup)"
    else
        echo "error: $skill_dest exists and is not an orgasmic skill; leaving it untouched" >&2
        exit 1
    fi
}

link_source_content_root() {
    local checkout="$1"
    local link="$ORGASMIC_HOME/orgasmic"
    if [[ "$checkout" == "$link" ]]; then
        return
    fi
    replace_symlink "$checkout" "$link"
}

validate_runtime_payload() {
    local payload="$1"
    local missing=false
    for rel in \
        bin/orgasmic \
        runtime-manifest.json \
        docs/README.md \
        shipped/schema/tx.org \
        shipped/prompt-studio/slots.org \
        shipped/skills/orgasmic/SKILL.md
    do
        if [[ ! -f "$payload/$rel" ]]; then
            echo "error: runtime bundle missing $rel" >&2
            missing=true
        fi
    done
    if $missing; then
        exit 1
    fi
    if [[ ! -x "$payload/bin/orgasmic" ]]; then
        echo "error: runtime binary is not executable: $payload/bin/orgasmic" >&2
        exit 1
    fi
}

install_bundle_mode() {
    require_cmd tar
    mkdir -p "$ORGASMIC_HOME"/{user,state/tx,sessions,secrets,logs,bin,runtimes}
    [[ -f "$ORGASMIC_HOME/secrets/.gitignore" ]] || printf '*\n!.gitignore\n' > "$ORGASMIC_HOME/secrets/.gitignore"

    local runtime_target manifest_url bundle_url runtime_version expected_sha actual_sha
    runtime_target="$(target_key)"
    manifest_url=""
    expected_sha="$BUNDLE_SHA256"

    local work bundle_file extract_dir payload
    work="$(mktemp -d "${TMPDIR:-/tmp}/orgasmic-install.XXXXXX")"
    INSTALL_WORK="$work"
    trap 'rm -rf "${INSTALL_WORK:-}"' EXIT
    bundle_file="$work/runtime.tar.gz"

    echo "→ ORGASMIC_HOME = $ORGASMIC_HOME"
    echo "→ mode          = bundle"
    echo "→ target        = $runtime_target"

    if [[ -n "$BUNDLE" ]]; then
        echo "→ bundle        = $BUNDLE"
        fetch_to_file "$BUNDLE" "$bundle_file"
    else
        local tag
        tag="${VERSION:-$CHANNEL}"
        manifest_url="https://github.com/${RELEASE_REPO}/releases/download/${tag}/runtime-latest.json"
        echo "→ manifest      = $manifest_url"
        fetch_to_file "$manifest_url" "$work/runtime-latest.json"
        # orgasmic:dec_B4147 — prefer the per-target version so a lagging entry
        # (e.g. windows, refreshed by a separate CI dispatch) names its runtime
        # dir honestly instead of inheriting the manifest's top-level version.
        runtime_version="$(extract_runtime_field "$runtime_target" version "$work/runtime-latest.json")"
        if [[ -z "$runtime_version" ]]; then
            runtime_version="$(extract_json_string version "$work/runtime-latest.json")"
        fi
        bundle_url="$(extract_runtime_field "$runtime_target" url "$work/runtime-latest.json")"
        expected_sha="$(extract_runtime_field "$runtime_target" sha256 "$work/runtime-latest.json")"
        if [[ -z "$runtime_version" || -z "$bundle_url" || -z "$expected_sha" ]]; then
            echo "error: manifest does not contain version/url/sha256 for $runtime_target" >&2
            exit 1
        fi
        bundle_url="$(resolve_asset_url "$manifest_url" "$bundle_url")"
        echo "→ bundle        = $bundle_url"
        fetch_to_file "$bundle_url" "$bundle_file"
    fi

    if [[ -n "$expected_sha" ]]; then
        actual_sha="$(sha256_file "$bundle_file")"
        if [[ "${actual_sha}" != "${expected_sha}" ]]; then
            echo "error: checksum mismatch" >&2
            echo "  expected $expected_sha" >&2
            echo "  actual   $actual_sha" >&2
            exit 1
        fi
        echo "→ checksum ok"
    elif [[ -n "$BUNDLE" ]]; then
        echo "warning: --bundle supplied without --sha256; offline test install is unverified" >&2
    fi

    extract_dir="$work/extract"
    mkdir -p "$extract_dir"
    extract_runtime_tarball "$bundle_file" "$extract_dir"
    payload="$extract_dir"
    if [[ ! -f "$payload/bin/orgasmic" ]]; then
        local child_count child
        child_count="$(find "$extract_dir" -mindepth 1 -maxdepth 1 -type d | wc -l | tr -d ' ')"
        child="$(find "$extract_dir" -mindepth 1 -maxdepth 1 -type d -print -quit)"
        if [[ "$child_count" == "1" && -f "$child/bin/orgasmic" ]]; then
            payload="$child"
        fi
    fi
    validate_runtime_payload "$payload"

    runtime_version="${runtime_version:-$(extract_json_string version "$payload/runtime-manifest.json")}"
    runtime_target="$(extract_json_string target "$payload/runtime-manifest.json" || true)"
    runtime_target="${runtime_target:-$(target_key)}"
    if [[ -z "$runtime_version" ]]; then
        echo "error: runtime-manifest.json does not contain a version" >&2
        exit 1
    fi

    local runtime_dir runtime_tmp
    runtime_dir="$ORGASMIC_HOME/runtimes/${runtime_version}-${runtime_target}"
    runtime_tmp="$ORGASMIC_HOME/runtimes/${runtime_version}-${runtime_target}.tmp"
    if [[ ! -d "$runtime_dir" ]]; then
        rm -rf "$runtime_tmp"
        mv "$payload" "$runtime_tmp"
        mv "$runtime_tmp" "$runtime_dir"
    else
        echo "→ runtime already present: $runtime_dir"
    fi

    validate_runtime_payload "$runtime_dir"
    replace_symlink "runtimes/${runtime_version}-${runtime_target}" "$ORGASMIC_HOME/current"
    replace_symlink "current" "$ORGASMIC_HOME/orgasmic"
    replace_symlink "../current/bin/orgasmic" "$ORGASMIC_HOME/bin/orgasmic"
    link_agent_skill "$ORGASMIC_HOME/current/shipped/skills/orgasmic"
    write_install_json_bundle "$runtime_version" "$runtime_target" "$manifest_url" "$runtime_dir"

    "$ORGASMIC_HOME/bin/orgasmic" init
    ensure_path_on_shell
    "$ORGASMIC_HOME/bin/orgasmic" doctor
    if [[ "${ORGASMIC_INSTALL_SKIP_STATUS:-}" == "1" ]]; then
        echo "→ skipped status verification because ORGASMIC_INSTALL_SKIP_STATUS=1"
    else
        "$ORGASMIC_HOME/bin/orgasmic" status >/dev/null
    fi

    echo "✓ install complete"
    echo "  runtime: $runtime_dir"
    echo "  next:    orgasmic ui"
    verify_on_path
}

install_source_mode() {
    require_cmd git
    if $DO_BUILD; then
        require_cmd cargo
    fi

    INSTALL_DIR="${INSTALL_DIR:-$ORGASMIC_HOME/source}"
    echo "→ ORGASMIC_HOME = $ORGASMIC_HOME"
    echo "→ mode          = source"
    echo "→ checkout      = $INSTALL_DIR"
    echo "→ branch        = $BRANCH"
    echo "→ build         = $DO_BUILD"

    mkdir -p "$ORGASMIC_HOME"/{user,state/tx,sessions,secrets,logs,bin}
    [[ -f "$ORGASMIC_HOME/secrets/.gitignore" ]] || printf '*\n!.gitignore\n' > "$ORGASMIC_HOME/secrets/.gitignore"

    if [[ -d "$INSTALL_DIR/.git" ]]; then
        if [[ -n "$FROM_SOURCE" ]]; then
            echo "→ using contributor checkout as-is"
        else
            echo "→ existing checkout detected, updating..."
            cd "$INSTALL_DIR"
            if [[ -n "$REPO_URL" ]]; then
                git remote set-url origin "$REPO_URL"
            fi
            local stash_label stashed rc
            stash_label="orgasmic-install-$(date +%Y%m%d%H%M%S)"
            stashed=false
            if [[ -n "$(git status --porcelain)" ]]; then
                echo "→ stashing local source edits as ${stash_label}"
                git stash push --include-untracked --message "$stash_label"
                stashed=true
            fi
            set +e
            git fetch origin "$BRANCH" && git checkout "$BRANCH" && git pull --ff-only origin "$BRANCH"
            rc=$?
            set -e
            if $stashed; then
                echo "→ restoring local source edits"
                git stash pop || echo "warning: stash pop failed; recover with 'git stash list' in $INSTALL_DIR"
            fi
            if [[ $rc -ne 0 ]]; then
                echo "error: source update failed (exit $rc)" >&2
                exit $rc
            fi
            cd - >/dev/null
        fi
    else
        mkdir -p "$(dirname "$INSTALL_DIR")"
        if [[ -n "$FROM_SOURCE" ]]; then
            echo "error: --from-source path is not a git checkout: $FROM_SOURCE" >&2
            exit 1
        elif [[ -n "$REPO_URL" ]]; then
            git clone --branch "$BRANCH" "$REPO_URL" "$INSTALL_DIR"
        elif GIT_SSH_COMMAND="ssh -o BatchMode=yes -o ConnectTimeout=5" git clone --branch "$BRANCH" "$REPO_SSH" "$INSTALL_DIR" 2>/dev/null; then
            echo "→ cloned via SSH"
        else
            rm -rf "$INSTALL_DIR" 2>/dev/null || true
            git clone --branch "$BRANCH" "$REPO_HTTPS" "$INSTALL_DIR"
            echo "→ cloned via HTTPS"
        fi
    fi

    link_source_content_root "$INSTALL_DIR"
    link_agent_skill "$INSTALL_DIR/shipped/skills/orgasmic"
    write_install_json_source "$INSTALL_DIR"

    if $DO_BUILD; then
        (cd "$INSTALL_DIR" && cargo build --release)
        local source_bin
        source_bin="$(resolve_source_binary "$INSTALL_DIR")"
        if [[ -z "$source_bin" ]]; then
            echo "error: built orgasmic binary not found under $INSTALL_DIR/target (release or <triple>/release)" >&2
            exit 1
        fi
        replace_symlink "$source_bin" "$ORGASMIC_HOME/bin/orgasmic"
        "$ORGASMIC_HOME/bin/orgasmic" init
        ensure_path_on_shell
    else
        echo "→ skipped orgasmic init because --no-build was set"
    fi

    echo "✓ source install complete"
    echo "  next: orgasmic status"
    if $DO_BUILD; then
        verify_on_path
    fi
}

case "$MODE" in
    bundle) install_bundle_mode ;;
    source) install_source_mode ;;
    *) echo "internal error: unknown mode $MODE" >&2; exit 1 ;;
esac
