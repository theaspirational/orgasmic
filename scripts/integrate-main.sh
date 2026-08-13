#!/usr/bin/env bash
# Turn one clean, certified commit into a merged protected-branch PR, then
# converge the local checkout and remove the integrated branch.

set -euo pipefail

REMOTE="origin"
BASE="main"
REPO=""
TITLE=""
BODY_FILE=""
DRY_RUN=0
START_DIR="$PWD"

usage() {
    cat <<'EOF'
Usage: orgasmic integrate [--remote <name>] [--base <branch>] [--repo <owner/name>]
                          [--title <text>] [--body-file <path>] [--dry-run]

Integrates the current clean HEAD through the repository's protected branch:
fetch base, push a temporary head when necessary, create/reuse a draft PR,
reuse or run exact-tree certification, mark the PR ready, merge it, fast-forward
the local base, and delete the integrated head branch.

The command never force-pushes and refuses a HEAD that is behind or diverged
from the remote base. A failed certification leaves the draft PR and branch in
place so the same command can safely resume.
EOF
}

while [[ $# -gt 0 ]]; do
    case "$1" in
        --remote) REMOTE="$2"; shift 2 ;;
        --base) BASE="$2"; shift 2 ;;
        --repo) REPO="$2"; shift 2 ;;
        --title) TITLE="$2"; shift 2 ;;
        --body-file) BODY_FILE="$2"; shift 2 ;;
        --dry-run) DRY_RUN=1; shift ;;
        -h|--help) usage; exit 0 ;;
        *) echo "integrate: unknown option: $1" >&2; usage >&2; exit 2 ;;
    esac
done

if [[ -n "$BODY_FILE" && "$BODY_FILE" != /* ]]; then
    BODY_FILE="$START_DIR/$BODY_FILE"
fi

for cmd in git gh bash; do
    command -v "$cmd" >/dev/null 2>&1 || {
        echo "integrate: missing required command: $cmd" >&2
        exit 2
    }
done

ROOT="$(git rev-parse --show-toplevel 2>/dev/null)" || {
    echo "integrate: not in a git worktree" >&2
    exit 2
}
cd "$ROOT"

if [[ -n "$(git status --porcelain --untracked-files=all)" ]]; then
    echo "integrate: worktree must be clean; commit the exact tree first" >&2
    git status --short >&2
    exit 1
fi

CERTIFIER="$ROOT/scripts/certify-pr.sh"
if [[ ! -f "$CERTIFIER" ]]; then
    echo "integrate: missing exact-tree certifier $CERTIFIER" >&2
    exit 2
fi
if [[ -n "$BODY_FILE" && ! -f "$BODY_FILE" ]]; then
    echo "integrate: --body-file does not exist: $BODY_FILE" >&2
    exit 2
fi

run() {
    if [[ "$DRY_RUN" -eq 1 ]]; then
        printf 'DRY-RUN:'
        printf ' %q' "$@"
        printf '\n'
    else
        "$@"
    fi
}

CURRENT_BRANCH="$(git symbolic-ref --quiet --short HEAD)" || {
    echo "integrate: detached HEAD is unsupported; create a branch first" >&2
    exit 1
}
HEAD_SHA="$(git rev-parse HEAD)"
SHORT_SHA="${HEAD_SHA:0:12}"

converge_local() {
    if [[ "$DRY_RUN" -eq 1 ]]; then
        run git fetch --prune "$REMOTE" "$BASE"
        if [[ "$CURRENT_BRANCH" != "$BASE" ]]; then
            run git switch "$BASE"
        fi
        run git merge --ff-only "$REMOTE/$BASE"
        if [[ "$CURRENT_BRANCH" != "$BASE" ]]; then
            run git branch -d "$CURRENT_BRANCH"
        fi
        return
    fi

    if [[ "$CURRENT_BRANCH" != "$BASE" ]]; then
        git switch "$BASE"
    fi
    git merge --ff-only "$REMOTE/$BASE"
    if [[ "$CURRENT_BRANCH" != "$BASE" ]]; then
        git branch -d "$CURRENT_BRANCH"
    fi
    git worktree prune
}

if [[ "$DRY_RUN" -eq 0 ]]; then
    git fetch --quiet --prune "$REMOTE" "$BASE"
elif ! git rev-parse --verify --quiet "refs/remotes/$REMOTE/$BASE" >/dev/null; then
    echo "integrate: dry-run needs an existing $REMOTE/$BASE tracking ref" >&2
    exit 1
fi

REMOTE_BASE="$(git rev-parse "refs/remotes/$REMOTE/$BASE")"
if git merge-base --is-ancestor "$HEAD_SHA" "$REMOTE_BASE"; then
    echo "integrate: $SHORT_SHA is already integrated into $REMOTE/$BASE; converging locally"
    converge_local
    exit 0
fi
git merge-base --is-ancestor "$REMOTE_BASE" "$HEAD_SHA" || {
    echo "integrate: HEAD is behind or diverged from $REMOTE/$BASE; rebase or merge first" >&2
    exit 1
}

if [[ "$CURRENT_BRANCH" == "$BASE" ]]; then
    HEAD_BRANCH="codex/integrate-$SHORT_SHA"
else
    HEAD_BRANCH="$CURRENT_BRANCH"
fi
TITLE="${TITLE:-Integrate $SHORT_SHA}"
REPO="${REPO:-$(gh repo view --json nameWithOwner -q .nameWithOwner)}"

echo "integrate: head=$SHORT_SHA base=$REMOTE/$BASE branch=$HEAD_BRANCH repo=$REPO"
run git push "$REMOTE" "HEAD:refs/heads/$HEAD_BRANCH"

PR_NUMBER=""
PR_IS_DRAFT=""
if [[ "$DRY_RUN" -eq 0 ]]; then
    PR_NUMBER="$(gh pr list --repo "$REPO" --head "$HEAD_BRANCH" --base "$BASE" \
        --state open --json number --jq '.[0].number // empty')"
fi
if [[ -z "$PR_NUMBER" ]]; then
    create_args=(pr create --repo "$REPO" --base "$BASE" --head "$HEAD_BRANCH" --draft --title "$TITLE")
    if [[ -n "$BODY_FILE" ]]; then
        create_args+=(--body-file "$BODY_FILE")
    else
        create_args+=(--body "Automated protected-branch integration for $SHORT_SHA.")
    fi
    if [[ "$DRY_RUN" -eq 1 ]]; then
        run gh "${create_args[@]}"
        PR_NUMBER="<new-pr>"
        PR_IS_DRAFT="true"
    else
        gh "${create_args[@]}" >/dev/null
        PR_NUMBER="$(gh pr list --repo "$REPO" --head "$HEAD_BRANCH" --base "$BASE" \
            --state open --json number --jq '.[0].number // empty')"
        [[ -n "$PR_NUMBER" ]] || {
            echo "integrate: PR creation succeeded but the PR could not be resolved" >&2
            exit 1
        }
        PR_IS_DRAFT="true"
    fi
else
    PR_IS_DRAFT="$(gh pr view "$PR_NUMBER" --repo "$REPO" --json isDraft -q .isDraft)"
    echo "integrate: resuming PR #$PR_NUMBER"
fi

certify_args=(bash "$CERTIFIER" --repo "$REPO" --remote "$REMOTE" --base "$BASE")
run "${certify_args[@]}"

if [[ "$DRY_RUN" -eq 1 ]]; then
    run gh pr view "$PR_NUMBER" --repo "$REPO" --json headRefOid -q .headRefOid
    run git fetch --prune "$REMOTE" "$BASE"
else
    PR_HEAD_SHA="$(gh pr view "$PR_NUMBER" --repo "$REPO" --json headRefOid -q .headRefOid)"
    if [[ "$PR_HEAD_SHA" != "$HEAD_SHA" ]]; then
        echo "integrate: PR #$PR_NUMBER moved from certified $HEAD_SHA to $PR_HEAD_SHA; refusing merge" >&2
        exit 1
    fi
    git fetch --quiet --prune "$REMOTE" "$BASE"
    POST_CERT_BASE="$(git rev-parse "refs/remotes/$REMOTE/$BASE")"
    if [[ "$POST_CERT_BASE" != "$REMOTE_BASE" ]]; then
        echo "integrate: $REMOTE/$BASE moved during certification; update HEAD and rerun" >&2
        exit 1
    fi
fi

if [[ "$PR_IS_DRAFT" == "true" ]]; then
    run gh pr ready "$PR_NUMBER" --repo "$REPO"
fi
run gh pr merge "$PR_NUMBER" --repo "$REPO" --merge --delete-branch \
    --match-head-commit "$HEAD_SHA"

if [[ "$DRY_RUN" -eq 1 ]]; then
    converge_local
    exit 0
fi

git fetch --quiet --prune "$REMOTE" "$BASE"
MERGED_BASE="$(git rev-parse "refs/remotes/$REMOTE/$BASE")"
git merge-base --is-ancestor "$HEAD_SHA" "$MERGED_BASE" || {
    echo "integrate: PR #$PR_NUMBER is not in $REMOTE/$BASE yet; rerun after the merge completes" >&2
    exit 1
}
converge_local

# `gh pr merge --merge` creates a new commit SHA even when its tree is exactly
# the certified PR tree. Rebind the existing tree/base/toolchain receipt to that
# merge SHA immediately, so the stable publisher does not discover a missing
# exact-commit status after spending minutes building artifacts. The
# --publish-only path fails closed if the merge tree or base does not match the
# receipt; it never reruns or weakens certification here.
bash "$ROOT/scripts/certify-pr.sh" --repo "$REPO" --remote "$REMOTE" --base "$BASE" --publish-only

echo "integrate: merged PR #$PR_NUMBER; $BASE is current and the integrated branch is gone"
