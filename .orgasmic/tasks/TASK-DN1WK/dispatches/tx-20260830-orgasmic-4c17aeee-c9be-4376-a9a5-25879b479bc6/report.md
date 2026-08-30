# TASK-DN1WK implementation report

## Changed

### OKF bundle and skill door

`shipped/skills/orgasmic/` is now an OKF bundle root. `SKILL.md` is the short door: it describes orgasmic, links `index.md`, explains raw link traversal without OKFy, gives the optional `okfy query` path, and keeps the interactive forum policy reachable. Its listing description names forum ask/critique/review, self- and dispatched curation, dispatch lifecycle, and runtime install/update.

The generated bundle support files are `index.md`, `README.md`, `AGENTS.md`, `CLAUDE.md`, `log.md`, and `meta/` (purpose, corpus manifest, lexicon, extraction plan, purpose-fitness matrix, package metadata, and eval replay history). Existing detailed skill references remain under `references/` and now carry OKF Topic frontmatter.

Recipe concepts:

- `recipes/cheap-wide-forum.md`
- `recipes/adversarial-forum-review.md`
- `recipes/self-curated-forum.md`
- `recipes/judge-document.md`
- `recipes/dispatched-curator.md`
- `recipes/dispatch-task-lifecycle.md`
- `recipes/inspect-work.md`
- `recipes/install-update-runtime.md`

Verb-reference concepts:

- `operations/forum.md`
- `operations/dispatch.md`
- `operations/task-graph.md`
- `operations/artifacts.md`
- `operations/content.md`
- `operations/core-project.md`
- `operations/runs.md`
- `operations/daemon.md`

The bundle was bootstrapped from the real CLI help tree, existing skill references, prompt specs, repository `AGENTS.md`, and forum merge messages using OKFy v0.19.0 core commands (`survey`, `init`, `index`, `package`). The plugin-only `/okfy:new` and `/okfy:extract` flow was unavailable in this worker harness, so the brief's documented manual/core-CLI path was used. No change was made to OKFy.

Runtime shipment needs no new packaging abstraction: `scripts/package-runtime.sh` already copies the complete `shipped/` tree into every runtime, and `refresh_agent_skill` already links the complete runtime `shipped/skills/orgasmic` directory.

### Parity gate

`crates/orgasmic-cli/src/main.rs` now has `every_visible_cli_subcommand_is_named_in_the_shipped_okf_bundle`. It recursively walks the live Clap command tree, excludes hidden commands and generated help commands, and requires the exact backticked marker `` `orgasmic <full command path>` `` in `SKILL.md`, `operations/`, `recipes/`, or `references/`. This is programmatic rather than a curated verb list: the current 133 visible nested command paths are derived from `Cli::command()`, so adding a visible command without a concept/reference marker fails the test.

A discriminating red/green probe removed the `orgasmic member revoke` marker temporarily. The test failed with `missing ["member revoke"]`; the file was restored byte-for-byte and the final test passed.

### Implementer definition of done

`shipped/prompt-studio/prompt-specs/implementer.org` now requires updating touched OKF concepts when the CLI surface or workflows change.

## Verification Gates

| Gate | Result | Durable output |
|---|---|---|
| `okfy validate shipped/skills/orgasmic --strict-sources --strict-quality --strict-package --strict-schema --strict-injection` | PASS: `ok: true`, 0 errors, 0 warnings, 27/27 sourced concepts valid, 0 broken source paths | `/tmp/TASK-DN1WK-okfy-validate-final.json` |
| Temporary parity regression | EXPECTED RED: missing `member revoke`, 0 passed / 1 failed | `/tmp/TASK-DN1WK-parity-red.log` |
| `cargo test -p orgasmic-cli --bin orgasmic okf_bundle_tests::every_visible_cli_subcommand_is_named_in_the_shipped_okf_bundle` | PASS: 1 passed, 0 failed | `/tmp/TASK-DN1WK-parity-green-final.log` |
| `cargo test -p orgasmic-cli --bin orgasmic` (default target dir) | PASS: 300 passed, 0 failed, 1 ignored | `/tmp/TASK-DN1WK-cli-bin-final.log` |
| `cargo test -p orgasmic-cli --test cli_parity` | PASS: 7 passed, 0 failed | `/tmp/TASK-DN1WK-cli-parity-existing.log` |
| `cargo test -p orgasmic-daemon --lib prompt_compiler::tests::all_shipped_prompt_specs_compile_cleanly` | PASS: 1 passed, 0 failed | `/tmp/TASK-DN1WK-prompt-compile.log` |
| `ORGASMIC_ALLOW_MISSING_TOOLS=tmux cargo test -p orgasmic-daemon --lib required_test_tooling_is_present -- --nocapture` | PASS: daemon tooling sentinel passed | `/tmp/TASK-DN1WK-daemon-tooling-sentinel.log` |
| `cargo fmt --check` | PASS | terminal transcript |
| `git diff --check` | PASS | terminal transcript |

The single ignored CLI-bin test is the existing real installed-runtime code-sign identity probe; it is not a failure. No full workspace suite was requested or run.

### Ten standing discovery queries

Latest acceptance replay: `2026-08-30T12:38:20.411718+00:00`. All ten have a PROPOSED LLM `pass`; none has an owner verdict, so `okfy eval status` correctly reports `PROVISIONAL: 0/10 owner-confirmed (10 llm-only, 0 pending)`.

| # | Query | Top hit | Proposed verdict |
|---:|---|---|---|
| 0 | How do I run a cheap 10-model forum round? | `recipes/cheap-wide-forum` | pass |
| 1 | How do I run a forum without cross-review? | `recipes/cheap-wide-forum` | pass |
| 2 | How can one strong model challenge the existing forum answers? | `recipes/adversarial-forum-review` | pass |
| 3 | How do I judge a supplied document with several models? | `recipes/judge-document` | pass |
| 4 | How do I run and finish a self-curated multi-round forum? | `references/forum` (recipe is second) | pass |
| 5 | How do I launch a fire-and-forget forum with a dispatched curator? | `recipes/dispatched-curator` | pass |
| 6 | How do I dispatch an implementer or reviewer and complete its lifecycle? | `recipes/dispatch-task-lifecycle` | pass |
| 7 | How do I close a reviewed task with evidence? | `recipes/dispatch-task-lifecycle` | pass |
| 8 | How do I inspect tasks, runs, and artifacts? | `recipes/inspect-work` | pass |
| 9 | How do I install or update the orgasmic runtime? | `recipes/install-update-runtime` | pass |

The replay and proposed verdict evidence is stored in `shipped/skills/orgasmic/meta/eval.json`; the current status transcript is `/tmp/TASK-DN1WK-okfy-eval-status-final.log`.

## Unmet Criteria

The operator-owned ten eval-query verdicts are intentionally not self-certified. The purpose supplied in the dispatch brief was used, but only the owner can lift this bundle out of provisional.

The operator must inspect each query's hits and record a real verdict for indices 0 through 9:

```bash
okfy eval verdict shipped/skills/orgasmic latest <q-index> <pass|fail|partial> --owner --note "<reason>"
```

Then confirm the resulting state with:

```bash
okfy eval status shipped/skills/orgasmic latest
```

This is the only unmet acceptance checkpoint. It cannot be completed by the dispatched implementer under the explicit operator-owned checkpoint policy.

## Residual Risk

No live billed forum or worker dispatch was run. Behavior is covered by the CLI help corpus, strict OKF validation, programmatic parity test, focused CLI/prompt gates, and deterministic retrieval replay; an operator must still judge the ten retrieval results before the bundle is non-provisional.
