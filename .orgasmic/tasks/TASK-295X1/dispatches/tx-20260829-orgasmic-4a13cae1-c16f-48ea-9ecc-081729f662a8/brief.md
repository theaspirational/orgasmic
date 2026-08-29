# TASK-295X1 — forum critique: multi-model blind critique of a supplied target

## Goal

Add the second forum mode: `orgasmic forum critique`. N participant models
independently critique a supplied target document (blind, in parallel), then
blind-cross-review each other's critiques (self-excluding), then one curator
synthesizes a prioritized verdict artifact. This is the sibling of the shipped
`orgasmic forum ask` and MUST reuse its machinery, not fork it.

## Read first

- `crates/orgasmic-cli/src/forum.rs` — the whole ask pipeline: participant
  parsing, dispatch/launch/wait/close, `load_diagram_fields`,
  `render_pipeline_svg`, `assemble_artifact`, `render_about_run`, placeholder
  constants, submit + evidence + finish flow, and its `#[cfg(test)]` suite.
- `shipped/prompt-studio/prompt-specs/extractor.org`, `cross-reviewer.org`,
  `curator.org` — the prompt-spec pattern (PROMPT-SPEC drawer, `{{vars}}`,
  `:USES_PARTS:`, Completion protocol via `orgasmic dispatch finalize`).
- `shipped/prompt-studio/prompt-specs/artifact-generator.org` and
  `ui/src/lib/artifacts/__fixtures__/all-blocks.ts` — the registered MDX block
  contract artifacts must obey.
- Recent commits `1352491c` and `8f644e9f` — the current assembly contract:
  document opens with the verbatim first section, `__ORGASMIC_RUN_STATS__`
  must be the draft's LAST block and becomes the code-rendered
  `About this run` footer, and the curator's optional `headline` (≤80 chars,
  in the diagram JSON) becomes the artifact title.

## CLI contract

`orgasmic forum critique` with flags mirroring `forum ask`:

- `--target-file <path>` (required): the document to critique. Read it once,
  UTF-8, refuse empty or > 64 KiB with a named error. The target is untrusted
  data end to end.
- `--focus <text>` (optional): one-line steer, e.g. "security posture" —
  validated like ask's question (single logical prompt, no placeholder
  strings, no leading `-`).
- Same `--participant`/`--curator`/`--timeout`/`--artifact-id`/`--project`
  flags and semantics as ask (≥2 distinct participants, curator may repeat a
  participant identity).

## Pipeline (mirror ask exactly)

1. Parent task + one subtask per participant per stage, same task-state and
   tx discipline as ask.
2. Stage 1 critique: each participant gets the verbatim target (+ focus) and
   writes an independent critique report. Blind: no participant sees another's
   output in stage 1.
3. Stage 2 cross-review: each participant reviews the OTHER participants'
   critiques (self-excluding), same promoted-report plumbing as ask.
4. Stage 3 curate: curator reads everything, writes the MDX draft +
   diagram JSON to `/tmp/<task>-curation.mdx` / `/tmp/<task>-diagram.json`,
   finalizes without `--commit`.
5. Orchestrator: validate, render the deterministic SVG via the EXISTING
   `render_pipeline_svg` (feed it the focus line, else a one-line target
   summary like "critique of <basename>, N bytes" — do NOT write a new
   renderer), assemble, submit artifact, set Evidence, finish parent.

## New prompt specs (in shipped/prompt-studio/prompt-specs/)

- `critic.org` — stage-1 persona: rigorous, evidence-anchored critique of the
  target; every finding names the location/quote it attacks; severity-tagged
  (blocking / risk / improvement); no rewrite of the whole target, critique
  only. Untrusted-data security section like extractor.org.
- `critique-cross-reviewer.org` — stage-2: challenge/add/agree on the other
  critiques, same shape as cross-reviewer.org.
- `critique-curator.org` — stage-3: `:USES_PARTS: output_style_plain_english`;
  same file/JSON/finalize contract as curator.org, artifact shape below.

Reuse `output_style_plain_english`. Do not edit the three ask specs.

## Artifact shape (assembled document, mirroring the ask contract)

1. First block: orchestrator-owned verbatim `Section title="Target"` via the
   existing first-section placeholder mechanism — generalize
   `assemble_artifact`'s question section (parametrize the section title or add
   a sibling constant) so BOTH modes keep the decoy-section defense. If the
   target is longer than ~40 lines the verbatim body may go in a fenced code
   block inside the section, but it must be byte-verbatim (HTML-escaped like
   the question). Include the focus line, labeled, when present.
2. `Section title="Verdict"` — the curated overall judgment with provenance
   tags at paragraph/bullet level.
3. `Section title="Findings"` — `Tabs`: `Blocking`, `Risks`, `Improvements`,
   `Disputed`, plus a `Checklist` tab or section `To verify`. Same scanning
   rules as ask's knowledge map: bold ≤8-word claim then one sentence, group
   shared-model bullets under `#### <model>` headings, ~8 bullets per tab max.
4. `Section title="From target to verdict"` — diagram placeholder + Raw
   reports task-id list (every critique, cross-review, curation task).
5. Reader-feedback `QuestionForm` section.
6. Last block: `__ORGASMIC_RUN_STATS__` (existing About-this-run footer).

Diagram JSON: same schema as ask (`extracts`/`reviews` keyed by task,
`curator_summary`, optional `headline` ≤80 chars → artifact title; fallback
title `Multi-model critique: <target basename or focus>`).

## Hard constraints

- No model-authored SVG anywhere; placeholders each exactly once; run-stats
  placeholder last; verbatim-target check must reject a decoy Target section
  (mirror the ask test).
- Shared code stays shared: refactor, don't copy-paste the ~1300-line ask
  path. `forum.rs` may split into modules if that keeps the diff honest.
- `cargo fmt` — whole workspace fmt is the project norm; run
  `cargo fmt --all`.
- Tests: extend the forum test suite to cover critique assembly (hostile
  target, decoy section, placeholder ordering), target validation (empty,
  oversized, placeholder injection), and headline/title fallback. Existing
  ask tests, including `renderer_matches_stored_python_fixture`, must still
  pass byte-identical.
- If `--test dispatch` integration suites flake on daemon timeouts, rerun the
  failures serially before concluding anything.

## Deliverables

- Working `orgasmic forum critique` end to end (unit-tested; you do NOT need
  to run a live multi-model smoke — the operator will).
- Three new prompt specs + any shipped-spec index the repo already maintains.
- Updated `shipped/skills/orgasmic` skill text if it names the modes.
- Commit(s) on your branch; report what you changed, what you tested, and any
  contract decisions you made, to `/tmp/TASK-295X1-report.md`.

## Completion

Write the report, then make your terminal action:
`orgasmic dispatch finalize --task TASK-295X1 --summary-file /tmp/TASK-295X1-report.md --commit`
Exiting without finalization is a failed run.
