# Review brief — TASK-DN1WK: orgasmic skill as an OKF bundle

## What to review

The single implementer commit on branch `orgasmic-okf-bundle-impl`
(branched from `main` at `f044ba19`; 43 files, +3356/−76). Diff:
`git diff main...HEAD`. Almost all content under `shipped/skills/orgasmic/`
(new `index.md`, `recipes/`, `operations/`, `meta/`, rewritten `SKILL.md`,
frontmatter added to `references/`), plus a parity test in
`crates/orgasmic-cli/src/main.rs` and one DoD line in
`shipped/prompt-studio/prompt-specs/implementer.org`.
Implementer report: `/tmp/TASK-DN1WK-report.md`. Implementation brief:
`.orgasmic/tmp/dispatch/TASK-DN1WK/TASK-DN1WK-brief.md` (ledger).
The okfy vendor (v0.19) lives at `~/Documents/code/tools/okfy`; the `okfy`
CLI is installed.

## Contract (binding)

1. Bundle passes strict `okfy validate`; concepts are small, intent-shaped
   (recipes) plus verb references, every claim sourced from the corpus.
2. SKILL.md is a short door: what orgasmic is, index pointer, raw-traversal
   instructions, optional `okfy` search; skill description line names the
   major features. Interactive forum policy stays reachable.
3. Parity gate: programmatic walk of the Clap tree; every visible command
   path must appear as a backticked marker in the bundle; red-proven.
4. Eval: ten discovery queries recorded in-bundle, LLM verdicts PROPOSED
   only, bundle PROVISIONAL — zero owner verdicts. Any owner/acceptance
   self-certification is a REJECT-level violation.
5. No okfy modifications; no orgasmic behavior changes (test-only Rust).

## Review posture — this is a DOCUMENTATION TRUTH review first

1. **Concept truthfulness (top priority).** The bundle will steer fresh
   agents with real budgets. Spot-verify EVERY command line in `recipes/`
   and a sample of `operations/` against the actual CLI: run
   `cargo run -q -p orgasmic-cli -- <cmd> --help` (or the workspace build)
   and check flags, defaults, and semantics. Hunt for: invented flags,
   stale flag names (e.g. pre-rename `--question-file`/`--target-file`,
   pre-`--fast` panel minimums, curator index-only claims), wrong lifecycle
   claims (dispatch-close rules, review gates, state transitions), wrong
   forum semantics (self-curation default, `--forum` joins, review-round
   blindness, curate identity rules). Every wrong claim is at least MEDIUM.
2. **Traversal usability.** Play a fresh agent: from SKILL.md alone, resolve
   "run a cheap 10-model round then have one strong model challenge it then
   finish" strictly by following links. Note dead ends, orphan concepts,
   circular or missing links, index gaps.
3. **Parity-gate soundness.** Can it be gamed (marker in a comment, hidden
   command slip-through, alias mismatch)? Does it walk nested subcommands
   correctly? Is the failure message actionable? Confirm the red probe
   claim by re-running your own (revert after).
4. **OKF/meta honesty.** meta/ files (purpose, corpus manifest, extraction
   plan, eval.json): consistent with what actually happened (manual core-CLI
   path, not the plugin flow)? No owner verdicts recorded? `okfy eval status`
   says PROVISIONAL 0/10 owner-confirmed? Rerun validate and eval status
   yourself.
5. **Rust test + DoD line.** Test-only, no behavior change, suite green.
6. **Size discipline:** no dumped help walls; concepts stay small.

Run what you need: `okfy validate ... --strict-*`, `okfy eval status`,
`okfy query` samples, full `cargo test -p orgasmic-cli --bin orgasmic`
(DEFAULT target dir — custom CARGO_TARGET_DIR breaks
`empty_private_targets_never_run_another_worktrees_binary`), clippy
`-D warnings`. No live billed dispatches/forums; do not record owner
verdicts (that is the operator's step).

## Verdict contract

Write `/tmp/TASK-DN1WK-review.md`: verdict first (`APPROVE`/`REJECT`,
REJECT needs a concrete reproducible defect — a materially wrong command in
a recipe counts), findings ranked with file anchors and the wrong vs right
text, and answer explicitly: "Would you merge this onto main as-is?"

Terminal action:
`orgasmic dispatch finalize --task TASK-DN1WK --summary-file /tmp/TASK-DN1WK-review.md`
No `--commit`. Do not edit the branch. Exiting without finalization is a
failed run.
