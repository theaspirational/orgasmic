# TASK-DN1WK — the orgasmic skill becomes an OKF bundle

## Goal

`shipped/skills/orgasmic/` becomes an OKF Bundle root so any fresh agent can
discover the full orgasmic CLI surface by link traversal. SKILL.md shrinks to
the door; small purpose-shaped concept files carry the knowledge; the bundle
ships inside the runtime so every installed binary carries version-accurate
discovery.

## Read first

- `~/Documents/code/tools/okfy` at v0.19 (`50430d8`): `README.md`,
  `docs/guide/GUIDE.md` (§6 procedure, §11 quality), the OKF spec the
  validator enforces (find it under docs/ or core/), and `okfy --help` /
  subcommand help. The `okfy` CLI (v0.19.0) is installed and on PATH.
- The corpus you extract from: the orgasmic CLI help tree (run
  `orgasmic --help` and every subcommand's help from the workspace build),
  `shipped/skills/orgasmic/SKILL.md` + `references/*.md` (forum.md is
  current and good), `shipped/prompt-studio/prompt-specs/*.org`, `AGENTS.md`,
  and recent merge messages `c74eb263..f044ba19` for the forum feature set.

## Deliverables

1. **The bundle.** Preferred path: the okfy plugin flow (`/okfy:new`,
   `/okfy:extract`) if your harness can drive it. Expected path: it cannot —
   then follow GUIDE.md §6 manually: use the `okfy` core CLI for every
   deterministic step (init/scaffold, index, validate, package as the CLI
   provides) and hand-author the concepts to the OKF spec. Either way the
   result must pass `okfy validate` clean.
   - Purpose (already interviewed from the owner in-session): "let a fresh
     coding agent discover and correctly execute orgasmic workflows by
     intent" — decision-support/api-reference hybrid.
   - Concepts organized by INTENT (recipes) linking to verb references:
     recipes at minimum — run a multi-model forum (self-curated default,
     rounds, curate), cheap wide first pass (`--fast`, panel of 1),
     adversarial verify (`forum review` with one strong model), judge a
     document (`forum critique`), dispatched fire-and-forget curator,
     dispatch an implementer/reviewer task (lifecycle: dispatch → wait →
     close, states, evidence), inspect tasks/artifacts, install/update the
     runtime. Verb-reference concepts for each CLI area (forum, manager
     dispatch family, task/tasks/node, artifacts, prompt studio, daemon).
     Every concept: YAML frontmatter per spec, small, linked; every claim
     sourced from the corpus (strict source checking per the spec) — never
     invent flags or behavior; verify every command line against `--help`.
   - Ten eval test queries recorded in-bundle covering the recipes (e.g.
     "run a forum without cross-review", "one strong model challenges the
     answers", "close a reviewed task"). Verdicts may be PROPOSED by you
     but the bundle stays PROVISIONAL: never mark owner acceptance — the
     operator judges the ten queries later. State this in the report.
2. **SKILL.md = the door.** Short: what orgasmic is, where the bundle index
   lives, follow links by raw traversal, `okfy` CLI search when present.
   Keep `/orgasmic forum` interactive behavior reachable (references/ stay,
   linked from concepts where they carry the depth).
3. **Skill description line** (frontmatter) names the major features so the
   one-line listing hints at them: forum ask/critique/review, self- or
   dispatched curation, dispatch lifecycle, runtime install/update.
4. **Parity gate:** a Rust test (place near the existing shipped-content
   tests) asserting every orgasmic CLI subcommand name appears in at least
   one bundle concept or reference — new verbs fail the gate until
   documented. Keep it maintainable (walk clap command names
   programmatically if feasible; a curated list with a "update me" assertion
   is acceptable if not — say which you chose and why).
5. **DoD:** the implementer prompt spec gains one line: update touched OKF
   concepts when CLI surface or workflows change.

## Hard constraints

- `okfy validate` clean; parity test green; all existing tests keep passing
  (`cargo test -p orgasmic-cli --bin orgasmic`, DEFAULT target dir — custom
  CARGO_TARGET_DIR breaks
  `empty_private_targets_never_run_another_worktrees_binary`); prompt specs
  compile (`cargo test -p orgasmic-daemon --lib
  prompt_compiler::tests::all_shipped_prompt_specs_compile_cleanly`).
- Do not modify okfy itself. Do not run live billed dispatches or forums.
- Keep bundle files small and purposeful — no dumped help text walls; the
  spec's size discipline applies.

## Report

`/tmp/TASK-DN1WK-report.md`: bundle layout, concept inventory, how the
parity gate works, `okfy validate` output, the ten eval queries and their
PROPOSED verdicts, and exactly what the operator must still do to lift the
bundle out of provisional (the ten owner verdicts — name the command).

## Completion

Write the report, then make your terminal action:
`orgasmic dispatch finalize --task TASK-DN1WK --summary-file /tmp/TASK-DN1WK-report.md --commit`
Exiting without finalization is a failed run.
