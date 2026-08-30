# TASK-E1D35 implementation report

## What changed

- Added `orgasmic forum review --forum TASK-XXXXX --participant <spec> [--round N | --all-rounds] [--focus <one-line>]`.
- Review intake reuses the existing open self-curated forum refusal gate, defaults to every non-review round, rejects missing rounds and explicit review-of-review selection, and validates the optional focus like critique.
- Reviewers are a freely chosen 1+ panel. Each reviewer receives only selected promoted stage-1 reports; matching `harness + normalized vendor/model` reports are removed per reviewer, and intake refuses a reviewer whose scope becomes empty.
- Extended forum manifests with `kind: review`, reviewed round numbers in the review input, and a serde-defaulted `review_tasks` list. Review rounds hold one task/report per reviewer, no stage-1 tasks, and no `cross_review_tasks`; validation requires referenced rounds to exist, precede the review, and not be review rounds.
- Curate raw-task coverage, curation contracts, diagram JSON, run metadata, and task ordinals now include review-round tasks exactly once. Review diagram rounds require `reviews`, forbid `extracts`, and retain the existing three-tag `?`/`+`/`=` gate.
- Multi-round SVG renders a distinct tinted review row, links each reviewed answer round to it, and links every review card to the one curator card. The existing dispatched ask renderer and stored ask SVG fixture remain unchanged.
- Added `forum-reviewer.org`. A separate prompt spec is necessary because one detached review can span ask and critique rounds, has an optional review-specific focus, and must never assume a paired stage-1 answer by the reviewer; its policy/output language is intentionally the existing cross-reviewer delta contract.
- Updated the shipped forum skill reference with when to use `review` versus document `critique`, selection/self-exclusion rules, and a single-strong-reviewer example.

Changed source files:
- `crates/orgasmic-cli/src/forum.rs`
- `shipped/prompt-studio/prompt-specs/forum-reviewer.org`
- `shipped/skills/orgasmic/references/forum.md`

## Contract decisions

- `--all-rounds` is the default when neither selector is present and skips earlier review rounds; naming a review round with `--round` is refused explicitly.
- Self-exclusion ignores mode and effort and compares harness plus normalized provider/model identity, matching the task's harness+model rule.
- `review_tasks` is a dedicated serde-defaulted manifest field rather than overloading `first_stage_tasks`; old manifests without it still deserialize and validate.
- Review rounds are stage-2-only: `first_stage_tasks` and `cross_review_tasks` must be empty, and each panel member maps positionally to one `review_tasks` entry and one promoted path.
- No live billed dispatch was run.

## Verification

- Baseline before edits: `cargo test -p orgasmic-cli --bin orgasmic forum::tests` — 24 passed. Log: `/tmp/TASK-E1D35-baseline-1788090251.log`.
- `cargo fmt --all` — passed; `git diff --check` — passed.
- `cargo clippy -p orgasmic-cli --all-targets -- -D warnings` — passed. Log: `/tmp/TASK-E1D35-final-clippy-1788091082.log`, PID 92511, exit 0.
- `cargo test -p orgasmic-cli --bin orgasmic` with the default target dir (`CARGO_TARGET_DIR` unset) — 299 passed, 0 failed, 1 ignored. Log: `/tmp/TASK-E1D35-final-cli-tests-1788091098.log`, PID 93174, exit 0.
- `cargo test -p orgasmic-daemon --lib prompt_compiler::tests::all_shipped_prompt_specs_compile_cleanly` — 1 passed. Log: `/tmp/TASK-E1D35-final-prompt-test-1788091132.log`, PID 3575, exit 0.
- Focused forum tests — 30 passed, including manifest round-trip/old defaults, refusal selection, self-exclusion and empty scope, diagram accept/reject, ask+fast+review renderer structure, raw curate coverage, and the byte-identical ask SVG fixture.
- Production CLI probes: `cargo run -q -p orgasmic-cli --bin orgasmic -- forum review --help` exposed the intended surface; the built binary refused `TASK-ZZZZZ` with `Error: unknown forum TASK-ZZZZZ` before any task creation or dispatch. Captures: `/tmp/TASK-E1D35-review-help.txt`, `/tmp/TASK-E1D35-unknown-forum.err`.

## Exact fast ask -> strong review -> curate sequence

```bash
cat > /tmp/forum-question.txt <<'QUESTION'
What should this forum determine?
QUESTION

ASK_JSON=$(orgasmic forum ask \
  --file /tmp/forum-question.txt \
  --fast \
  --participant 'stdio,hermes,openai/gpt-5.6-luna,low' \
  --participant 'stdio,hermes,google/gemini-3.7-flash,low' \
  --participant 'stdio,claude,claude-haiku-4-5-20251001,low')
FORUM=$(python3 -c 'import json,sys; print(json.load(sys.stdin)["forum"])' <<<"$ASK_JSON")

REVIEW_JSON=$(orgasmic forum review \
  --forum "$FORUM" \
  --participant 'stdio,claude,claude-fable-5,high')
CONTRACT=$(python3 -c 'import json,sys; print(json.load(sys.stdin)["contract_path"])' <<<"$REVIEW_JSON")
cat "$CONTRACT"

DRAFT="/tmp/${FORUM}-curation.mdx"
DIAGRAM="/tmp/${FORUM}-diagram.json"
${EDITOR:-vi} "$DRAFT" "$DIAGRAM"
: "${CURATOR_IDENTITY:?set CURATOR_IDENTITY to this session's real mode,harness,model,effort}"
orgasmic forum curate \
  --forum "$FORUM" \
  --draft "$DRAFT" \
  --diagram "$DIAGRAM" \
  --identity "$CURATOR_IDENTITY"
```

The editor step must produce the two files exactly as the printed compiled contract requires; the review-round diagram entry uses `reviews` only.
