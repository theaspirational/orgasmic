#!/usr/bin/env bash
# Fail closed unless the exact commit being published has a successful push run
# of ci.yml and that run contains the successful release-certified aggregate job.

set -euo pipefail

REPO=""
HEAD_SHA=""
WORKFLOW="ci.yml"
CERTIFICATION_JOB="release-certified"

usage() {
    cat <<'EOF'
Usage: bash scripts/assert-ci-certified.sh [--repo <owner/name>] [--sha <commit>]

Requires a completed, successful push run of .github/workflows/ci.yml for the
exact commit, including a completed, successful release-certified job. Defaults
to the current repository and HEAD. This gate has no publication bypass.
EOF
}

while [[ $# -gt 0 ]]; do
    case "$1" in
        --repo) REPO="$2"; shift 2 ;;
        --sha) HEAD_SHA="$2"; shift 2 ;;
        -h|--help) usage; exit 0 ;;
        *) echo "unknown option: $1" >&2; usage >&2; exit 1 ;;
    esac
done

for cmd in git gh node; do
    command -v "$cmd" >/dev/null 2>&1 || {
        echo "error: required command not found: $cmd" >&2
        exit 1
    }
done

CURRENT_HEAD="$(git rev-parse HEAD)"
if [[ -z "$HEAD_SHA" ]]; then
    HEAD_SHA="$CURRENT_HEAD"
fi
if [[ ! "$HEAD_SHA" =~ ^[0-9a-fA-F]{40}$ ]]; then
    echo "error: expected a full 40-character commit SHA, got: '$HEAD_SHA'" >&2
    exit 1
fi
HEAD_SHA="$(printf '%s' "$HEAD_SHA" | tr '[:upper:]' '[:lower:]')"
CURRENT_HEAD="$(printf '%s' "$CURRENT_HEAD" | tr '[:upper:]' '[:lower:]')"
if [[ "$HEAD_SHA" != "$CURRENT_HEAD" ]]; then
    echo "error: stable publish blocked: requested SHA $HEAD_SHA is not current HEAD $CURRENT_HEAD" >&2
    exit 1
fi

if [[ -z "$REPO" ]]; then
    REPO="$(gh repo view --json nameWithOwner -q .nameWithOwner)"
fi
if [[ -z "$REPO" ]]; then
    echo "error: could not resolve GitHub repository" >&2
    exit 1
fi

echo "→ checking exact-commit CI certification for $HEAD_SHA"

if ! RUNS_JSON="$(gh run list \
    --repo "$REPO" \
    --workflow "$WORKFLOW" \
    --commit "$HEAD_SHA" \
    --event push \
    --limit 100 \
    --json databaseId,headSha,status,conclusion,url 2>&1)"; then
    echo "error: stable publish blocked: could not query $WORKFLOW push runs" >&2
    echo "$RUNS_JSON" >&2
    exit 1
fi

# `--commit` is a server-side filter, but still compare the returned head SHA so
# an API/CLI regression cannot certify a neighboring commit.
RUN_ROWS="$(RUNS_JSON="$RUNS_JSON" HEAD_SHA="$HEAD_SHA" node 2>/dev/null <<'NODE'
const runs = JSON.parse(process.env.RUNS_JSON);
const wanted = process.env.HEAD_SHA.toLowerCase();
for (const run of runs) {
  if (String(run.headSha || '').toLowerCase() !== wanted) continue;
  const values = [run.databaseId, run.status, run.conclusion || '-', run.url || '-'];
  process.stdout.write(`${values.join('\t')}\n`);
}
NODE
)" || {
    echo "error: stable publish blocked: GitHub returned invalid run metadata" >&2
    exit 1
}

if [[ -z "$RUN_ROWS" ]]; then
    echo "error: stable publish blocked: no $WORKFLOW push run exists for exact HEAD $HEAD_SHA" >&2
    echo "       push this commit and wait for CI before publishing" >&2
    exit 1
fi

CERTIFIED_URL=""
DIAGNOSTICS=""
while IFS=$'\t' read -r RUN_ID RUN_STATUS RUN_CONCLUSION RUN_URL; do
    [[ -n "$RUN_ID" ]] || continue

    if JOBS_JSON="$(gh run view "$RUN_ID" --repo "$REPO" --json jobs 2>&1)"; then
        if JOB_STATE="$(JOBS_JSON="$JOBS_JSON" CERTIFICATION_JOB="$CERTIFICATION_JOB" node 2>/dev/null <<'NODE'
const payload = JSON.parse(process.env.JOBS_JSON);
const wanted = process.env.CERTIFICATION_JOB;
const job = (payload.jobs || []).find((candidate) => candidate.name === wanted);
if (!job) {
  process.stdout.write('missing');
} else {
  process.stdout.write(`${job.status || '-'}\t${job.conclusion || '-'}`);
}
NODE
)"; then
            if [[ "$JOB_STATE" == "missing" ]]; then
                JOB_STATUS="missing"
                JOB_CONCLUSION="missing"
            else
                IFS=$'\t' read -r JOB_STATUS JOB_CONCLUSION <<<"$JOB_STATE"
            fi
        else
            JOB_STATUS="invalid"
            JOB_CONCLUSION="invalid"
        fi
    else
        JOB_STATUS="inspection-failed"
        JOB_CONCLUSION="inspection-failed"
    fi

    if [[ "$RUN_STATUS" == "completed" && "$RUN_CONCLUSION" == "success" && \
          "$JOB_STATUS" == "completed" && "$JOB_CONCLUSION" == "success" ]]; then
        CERTIFIED_URL="$RUN_URL"
        break
    fi

    DIAGNOSTICS+="  run $RUN_ID: run=${RUN_STATUS}/${RUN_CONCLUSION}, ${CERTIFICATION_JOB}=${JOB_STATUS}/${JOB_CONCLUSION}, ${RUN_URL}"$'\n'
done <<<"$RUN_ROWS"

if [[ -z "$CERTIFIED_URL" ]]; then
    echo "error: stable publish blocked: exact HEAD $HEAD_SHA is not release-certified" >&2
    printf '%s' "$DIAGNOSTICS" >&2
    echo "       wait for a completed successful push run whose $CERTIFICATION_JOB job succeeds" >&2
    exit 1
fi

echo "✓ exact HEAD is release-certified: $CERTIFIED_URL"
