# TASK-DN1WK review — orgasmic skill as an OKF bundle

## Verdict

**APPROVE.** Would I merge this onto main as-is? **Yes.** No wrong command,
flag, or lifecycle claim survived spot-verification; every binding contract
item holds; all gates re-ran green independently. The findings below are a
MEDIUM link-form nit and two LOW maintainability notes — none blocks ship.

## Findings

**MEDIUM docs `shipped/skills/orgasmic/recipes/*.md` (9 links): recipe→operations
links use root-absolute paths that break literal raw traversal.**
Every recipe's step 1 links its operations page as `/operations/forum.md`
(also `/operations/dispatch.md`, `/operations/task-graph.md`,
`/operations/runs.md`, `/operations/artifacts.md`, `/operations/core-project.md`
— 9 occurrences, e.g. `recipes/cheap-wide-forum.md:22`,
`recipes/dispatch-task-lifecycle.md:28`, `recipes/inspect-work.md:26`).
Resolved literally from `recipes/`, `/operations/forum.md` is a dead
filesystem path; the same lines link references correctly as
`../references/dispatch.md`. SKILL.md promises "Raw Markdown traversal is the
complete fallback", and strict link resolvers (IDE preview, link checkers, a
literal-minded agent) dead-end on the absolute form. `okfy validate --strict-*`
accepts it (bundle-root form), and an LLM agent will usually guess the target,
so this is a usability nick, not a broken acceptance criterion — the
acceptance traversal (SKILL.md → index.md → recipes/cheap-wide-forum.md) uses
only relative links and works.
Wrong: `[forum operations](/operations/forum.md)`.
Right: `[forum operations](../operations/forum.md)`.

**LOW docs `shipped/skills/orgasmic/meta/corpus.md:3`: corpus snapshot roots at
a throwaway tmp path with `git_sha: null`, and no regeneration recipe is
recorded.** `corpus: /private/tmp/TASK-DN1WK-corpus.kHb6rP` vanishes on tmp
cleanup and `git_sha: null` drops the version anchor. I proved the CI gate
survives: renamed the corpus dir away, `okfy validate --strict-sources ...`
still returned `ok: true, 0 errors` (manifest hashes carry it), restored the
dir. But future `okfy update`/`okfy diff` maintenance needs a rebuilt corpus,
and nothing in `meta/` records how the `cli-help/*.txt` tree was generated.
Fix direction: record the help-dump command loop in `meta/corpus.md` or the
extraction plan, and set `git_sha` on the next snapshot.

**LOW docs `recipes/cheap-wide-forum.md:26` / `recipes/adversarial-forum-review.md:31`:
the two challenge-flow recipes end without linking the finish step.**
cheap-wide says "curate in the current chat" and adversarial says "fold them
into the later curation", but neither links `recipes/self-curated-forum.md`
(which owns `orgasmic forum curate`). The brief's end-to-end intent ("cheap
10-model round → strong-model challenge → finish") forces a bounce back
through `index.md`. Reachable, so not a dead end — one added link per recipe
closes it.

## Open Questions

- None blocking. Operator still owes the ten eval verdicts
  (`okfy eval verdict shipped/skills/orgasmic latest <i> <pass|fail|partial>
  --owner --note "..."`) — correctly left undone per the operator-owned
  checkpoint policy; I recorded none.

## Verification Notes

All commands run in the review worktree at `4094f9d5`, default target dir.

- **Command truthfulness (top priority): clean.** Built `orgasmic-cli` and
  diffed every recipe claim against live `--help`: `forum ask/critique/review/
  curate`, `manager dispatch/dispatch-wait/dispatch-close/drivers`,
  `dispatch finalize`, `tasks list --stage`, `task get`, `run list/show`,
  `run history inspect`, `artifact blocks --full`, `artifact comments`,
  `update`, `ui --print-url`, `doctor`, `status`, `scripts/install.sh
  --channel`. Flags, defaults (45m timeouts, `--fast` panel-of-one,
  worktree-remove/branch-delete defaults, BACKLOG/TODO dispatch gate,
  64 KiB critique limit) all match. Runtime-enforced claims verified in
  source: `--forum` + `--curator` bails (`forum.rs:2452`), dispatched-curator
  forums refuse joins (`forum.rs:2459`), later-round `--from`/`--artifact-id`
  must match the original (`forum.rs:3084-3091` — recipe's "only on first
  round" is slightly stricter than the code, which is safe advice), reviewer
  blindness to own stage-1 reports and review outputs (`forum.rs:3857`, test
  `forum.rs:4550`).
- **Reverse parity (docs→CLI), my own probe:** extracted every backticked
  `orgasmic ...` marker in the bundle and ran each against the binary; the
  only non-resolving strings are placeholders (`<command-path>`,
  `node body set|append` prose). No invented commands.
- **Parity gate red probe (mine, independent of the implementer's):** mutated
  the single `` `orgasmic run history compact` `` marker in
  `operations/runs.md` → `okf_bundle_tests` failed listing the missing path;
  restored byte-for-byte (same shasum `2125192…`), `git status` clean. Test
  logic reviewed: filters hidden + `help` at every recursion level; the
  ``` `orgasmic {path}` ``` closing-backtick format means a longer path can't
  satisfy a shorter one. One-directional (CLI→docs) by design; I covered the
  other direction above.
- **Gates re-run:** strict `okfy validate` → `ok: true`, 0 errors/warnings,
  27/27 sourced. `okfy eval status … latest` → `owner_confirmed: 0`,
  `provisional: 10`, `provisional: true`; `meta/eval.json` has every
  `owner_verdict: null` — zero self-certification. Full
  `cargo test -q -p orgasmic-cli --bin orgasmic` → 300 passed / 0 failed /
  1 ignored (pre-existing codesign probe) — log
  `/tmp/dn1wk-review-cli-tests.log`. `cargo clippy -p orgasmic-cli
  --all-targets -- -D warnings` → clean.
  `all_shipped_prompt_specs_compile_cleanly` → pass (implementer.org DoD line
  compiles).
- **Production-path probe for the rewritten SKILL.md:** daemon integration
  test `skill_routes_include_shipped_and_user_markdown_skills` (boots a real
  daemon over a home symlinked to this repo's `shipped/`) → pass; the new
  frontmatter (`type`/`title`/`sources` extras) parses because
  `MarkdownSkillFrontmatter` has no `deny_unknown_fields`
  (`content.rs:23-28`), and `/orgasmic` trigger survives.
- **Dropped SKILL.md content audited, not regressed:** the
  `ORGASMIC_MANAGER_WAKE_V1` handling lives in `shipped/entry/router.org:12`
  (test-guarded via `orgasmic-cli/tests/entry.rs:29`); "bare `/orgasmic` runs
  recall" lives in `references/recall-resume.md:95`. Both reachable from the
  index.
- **Retrieval probe:** `okfy query shipped/skills/orgasmic "run a cheap
  10-model round"` → top hit `recipes/cheap-wide-forum` (score 15.0).
- **Size discipline:** largest operation is 152 lines (alias/source lists),
  recipes 37–50 lines; no help-dump walls.
- **Not run:** live billed dispatches/forums (per brief); full workspace
  suite (not assigned). The parity list covers the CLI in this worktree; the
  live daemon runs the older installed runtime, so no live-daemon probes were
  used as evidence (known worktree blindspot).

## Fix Directions

1. Change the 9 `/operations/*.md` recipe links to `../operations/*.md`
   (one `sed` across `recipes/`, then re-run `okfy validate` + the parity
   test). Fine as a follow-up; no re-review needed.
2. Add a `meta/corpus.md` note with the corpus regeneration commands and a
   real `git_sha` at the next `okfy update`.
3. Add a "finish" link from `cheap-wide-forum.md` and
   `adversarial-forum-review.md` to `self-curated-forum.md`.
4. Operator: record the ten eval verdicts to lift PROVISIONAL.
