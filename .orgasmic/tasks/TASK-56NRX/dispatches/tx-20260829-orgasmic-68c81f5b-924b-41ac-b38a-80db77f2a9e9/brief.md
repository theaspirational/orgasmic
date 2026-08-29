# TASK-56NRX — Multi-model knowledge extraction mode

## Goal

Build the real pipeline behind the mock artifact **ART-MKRG1** (project `orgasmic`).
A user asks one hard question; N different models answer it independently; each
model blind-reviews the others' answers; one curator model merges everything into
a final orgasmic artifact. Everything runs through EXISTING orgasmic machinery —
this task is orchestration + prompt specs + artifact generation, not new
infrastructure.

## The contract: the mock is the shape spec

`ART-MKRG1` (current version, in the artifact store) is the agreed final-artifact
shape. Read it before designing anything. On disk (ledger checkout):
`~/.orgasmic/ledgers/orgasmic/.orgasmic/artifacts/ART-MKRG1/artifact.mdx`.
Its structure, which the curator prompt spec must reproduce for real runs:

1. **Header RichText** — run stats + participants roster, every participant
   identified as `harness · vendor · model · effort` (e.g. `claude-code ·
   anthropic · fable-5 · effort high`). Curator identified the same way. Never
   use anonymous labels like E1/E2.
2. **Callout** — provenance warning (multi-model synthesis; verify before acting).
3. **Section "Final answer"** — the curated answer, claims carrying
   `(vendor · model)` provenance tags.
4. **Section "From question to answer"** — the forward-chain diagram (question →
   parallel extract cards → cross-review cards with `? / + / =` delta notation →
   curate → final answer) + a "Raw reports" list of the run's subtask ids.
5. **Section "Knowledge map"** — Tabs: Shared core / Unique finds /
   Contradictions / To verify, all claims attributed by model name.
6. **Section with QuestionForm** — reader feedback questions.

## Pipeline to implement

Per run:
1. Parent task minted for the run; **one subtask per participant** (`TASK-<run>.<n>`).
   The artifact's "Raw reports" list links these ids — bare ids in artifact prose
   already linkify to the native task peek, and the peek now renders promoted
   dispatch reports (commit `f7b35c9d`: `GET /api/tasks/:id/dispatches` +
   TaskDialog "Dispatch reports" section). So: one subtask per participant per
   stage-generation is what makes raw reports readable in the UI. Preserve that
   property.
2. **Stage 1 — extract**: dispatch the SAME extraction brief to every participant
   in parallel (`orgasmic manager dispatch`, one per subtask, each with its own
   `--mode/--harness/--model/--effort`), barrier on `orgasmic manager
   dispatch-wait`, close with `dispatch-close` so records are promoted.
3. **Stage 2 — blind cross-review**: each participant gets the OTHER participants'
   stage-1 reports (not its own, no chat history) and produces a delta report:
   `?` challenged claim, `+` new addition, `=` confirmation. Same
   dispatch/wait/close mechanics.
4. **Stage 3 — curate**: one curator dispatch gets everything (question, all
   reports, all deltas) and writes the final MDX artifact, then submits it:
   `orgasmic id mint --class artifact` + `orgasmic artifact submit <id> --project
   <p> --file <mdx>`.

## Deliverables

- **Orchestrator**: a skill under `shipped/skills/` (follow the existing
  `shipped/skills/orgasmic` layout) that a manager session invokes with a
  question + participant list (each participant = mode/harness/model/effort).
  It drives stages 1–3 with the CLI. Participants are parameters, never
  hardcoded; sensible default trio is fine as documentation.
- **Prompt specs**: `extractor`, `cross-reviewer`, `curator` under
  `shipped/prompt-studio/prompt-specs/`, following the conventions of the
  existing specs there (`artifact-generator.org` is the closest relative —
  it carries the MDX block contract).
- **Curator MDX guidance** must encode the hard-won rendering rules:
  - MDX block contract: `shipped/prompt-studio/prompt-specs/artifact-generator.org`
    + fixtures `ui/src/lib/artifacts/__fixtures__/all-blocks.ts`.
  - The forward-chain diagram ships as an **Image block with a
    `data:image/svg+xml;base64,...` src** (survives the sanitizer, natural
    height). All SVG text styling as inline `style="..."` attributes — SVG
    presentation attrs and `<style>` blocks get stripped.
  - Curator writes SHORT card summaries in the diagram (fixed-size cards);
    long content lives in the reports, reached via the subtask-id links.
- **Smoke proof**: one real end-to-end run on a nontrivial question with at
  least 2 participants (pick cheap/fast models for the smoke), producing a
  submitted artifact whose report links open peeks that show the reports.
  Record the artifact id in your report.

## Constraints

- Never hand-edit the ledger; all writes through the CLI/daemon.
- Reuse before writing: dispatch, dispatch-wait, dispatch-close, prompt studio
  compilation, artifact store, task/subtask CLI all exist. If a genuinely
  missing primitive blocks the pipeline (e.g. no way to pass a per-dispatch
  file bundle), report the gap in your report rather than building a parallel
  mechanism.
- The dispatched-worker toolchain must not assume a specific harness: briefs are
  plain markdown files; reports land as promoted `report.md` records.
- Keep diffs minimal and boring; this is composition of existing verbs.

## Acceptance

1. Orchestrator skill + three prompt specs exist under `shipped/`, styled like
   their neighbors.
2. A documented invocation runs extract → cross-review → curate unattended.
3. The produced artifact matches the ART-MKRG1 shape (sections 1–6 above),
   uses `harness · vendor · model · effort` identity everywhere, and its
   raw-report links resolve to task peeks with readable reports.
4. Your report names the smoke run's parent task, subtasks, and artifact id.
