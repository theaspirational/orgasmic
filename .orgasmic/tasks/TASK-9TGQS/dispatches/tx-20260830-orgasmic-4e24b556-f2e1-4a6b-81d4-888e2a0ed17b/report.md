# TASK-9TGQS: Forum self-curation and multi-round submission

## Changed

- Made omitted `--curator` the self-curation path for `forum ask|critique`: stages 1-2 still dispatch and promote reports, then the command persists/prints the forum manifest, report paths, and compiled in-session curation contract without launching a curator.
- Added open-forum round joins through `--forum`, mixed ask/critique manifests under `.orgasmic/tmp/forum/`, continuous subtask numbering, immutable source/artifact overrides, and explicit refusal of unknown, curated, dispatched-curator, and `--forum`+`--curator` cases.
- Added `forum curate --forum --draft --diagram --identity [--project]`: it validates the existing assembly gates, mints an honest non-dispatch curation subtask after gates pass, renders one all-round tree, submits one artifact, records evidence, closes the tasks, and marks the manifest curated.
- Preserved the legacy explicit-`--curator` single-round dispatch and the byte-identical `TASK-FBSZ2-pipeline.svg` renderer path.
- Rewrote `shipped/skills/orgasmic/SKILL.md` and `references/forum.md` for in-chat curation, optional later rounds, real session identity, and the explicit-curator non-interactive alternative.

### Contract decisions (three lines)

1. Round 1 fixes the final draft contract and verbatim first section for a mixed ask/critique forum.
2. The first round fixes `--from` and `--artifact-id`; later rounds may omit them or repeat the same values, but cannot change them.
3. A one-round self-curated forum may use the legacy diagram JSON; two or more rounds must use `rounds`, while only round report task ids (not the not-yet-minted curation id) are required in the draft.

### Two-round self-curated session

```bash
cat > /tmp/forum-question.txt <<'QUESTION'
When should append-only events be authoritative?
QUESTION

orgasmic forum ask \
  --file /tmp/forum-question.txt \
  --participant 'stdio,hermes,openai/gpt-5.6-luna,low' \
  --participant 'stdio,hermes,google/gemini-3.7-flash,low' \
  | tee /tmp/forum-round-1.json

FORUM="$(jq -r .forum /tmp/forum-round-1.json)"
cat "$(jq -r .manifest_path /tmp/forum-round-1.json)"
cat "$(jq -r .contract_path /tmp/forum-round-1.json)"
jq -r '.promoted_report_paths[]' /tmp/forum-round-1.json | while IFS= read -r report; do cat "$report"; done

# Discuss and shape the target in chat, then write it for the critique round.
cat > /tmp/forum-shaped-design.md <<'TARGET'
# Shaped design

Replace this example with the document shaped during the in-chat curation.
TARGET

orgasmic forum critique \
  --forum "$FORUM" \
  --file /tmp/forum-shaped-design.md \
  --focus 'remaining failure modes' \
  --participant 'stdio,hermes,openai/gpt-5.6-luna,low' \
  --participant 'stdio,claude,claude-fable-5,low' \
  | tee /tmp/forum-round-2.json

cat "$(jq -r .manifest_path /tmp/forum-round-2.json)"
cat "$(jq -r .contract_path /tmp/forum-round-2.json)"
jq -r '.promoted_report_paths[]' /tmp/forum-round-2.json | while IFS= read -r report; do cat "$report"; done

# After the operator says done, write these two files exactly per the latest contract.
DRAFT="/tmp/${FORUM}-curation.mdx"
DIAGRAM="/tmp/${FORUM}-diagram.json"

orgasmic forum curate \
  --forum "$FORUM" \
  --draft "$DRAFT" \
  --diagram "$DIAGRAM" \
  --identity 'session,<actual-harness>,<actual-provider/model-id>,interactive'
```

The final identity command must replace all three angle-bracket fields with the invoking session's real values; the skill explicitly forbids placeholders in the actual invocation.

## Verification Gates

- `cargo fmt --all` — green; `git diff --check` clean.
- `cargo clippy -p orgasmic-cli --all-targets -- -D warnings` — green (`/tmp/TASK-9TGQS-clippy-final.log`, PID 12269).
- `cargo test -p orgasmic-cli --bin orgasmic` — green: 284 passed, 0 failed, 1 ignored (`/tmp/TASK-9TGQS-cargo-test-bin.log`, PID 2433).
- `cargo test -p orgasmic-cli --test cli_parity` — green: 7 passed (`/tmp/TASK-9TGQS-cli-parity.log`, PID 12916).
- Focused forum regressions are included in the full bin gate: mixed manifest round-trip, refusal matrix, multi-round gate parity, task-exact diagram validation, all-round renderer structure, and untouched byte-identity fixture.

## Unmet Criteria

- None.

## Residual Risk

- Per the brief, no live billed dispatch or end-to-end artifact smoke was run. The operator-owned smoke remains the production-path check for daemon task writes, real report promotion, and artifact submission across two billed rounds.
