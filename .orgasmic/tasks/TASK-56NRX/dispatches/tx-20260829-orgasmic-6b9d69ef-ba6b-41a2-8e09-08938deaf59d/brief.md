# TASK-56NRX round 3 — deterministic diagram + Question section (operator feedback on ART-DSKQY)

## Operator feedback on the smoke artifact ART-DSKQY

1. The "From question to answer" diagram rendered as a white box containing one
   line of text (`Question → Extract A / Extract B → Review ?/+/= → Curate →
   Final answer`). The mock ART-MKRG1's diagram — the agreed quality bar — is a
   rich card chain: prompt card, stage pills, one card per participant with
   vendor dot + wordmark, model name, role line, 4-line excerpt, record path,
   crossing curves into cross-review cards with `? / + / =` bullets, a converge
   pill, curator card, final-answer pill.
2. A new section is required: **the user's question/prompt, verbatim, as the
   FIRST section — above "Final answer"**.

## Root cause and the required fix

The curator prompt asks the model to author the SVG. A cheap curator cannot
draw; prompt-side "shape specs" only patch the symptom. The diagram's layout is
a pure function of structured data — so **generate it in code, never in a
model**:

1. **Deterministic SVG renderer** in the orchestrator (extend
   `shipped/skills/orgasmic/scripts/multi-model-extract.py` or a sibling module
   it imports). Input: question text, ordered participant list (harness ·
   vendor · model · effort · subtask id), per-participant extract summary
   lines, per-participant review delta bullets (each tagged `?`/`+`/`=`),
   curator identity + summary, record paths. Output: the complete SVG, then
   base64 `data:image/svg+xml;base64,...` for the MDX Image block.
2. **Copy the mock's visual language exactly.** The reference SVG is inside
   ART-MKRG1's Image block — decode the base64 from
   `~/.orgasmic/ledgers/orgasmic/.orgasmic/artifacts/ART-MKRG1/artifact.mdx`
   and lift its geometry, palette (its own dark background; vendor dot colors
   anthropic `#d97757`, openai `#10a37f`, google `#6f9df2`; accent `#f08a59`),
   fonts, spacing, stage pills, and bezier crossings. Parameterize participant
   count (2..N columns, width scales), keep every text style as an inline
   `style="..."` attribute (sanitizer strips presentation attrs and `<style>`
   blocks), and size the root svg with explicit width/height so it renders at
   natural size.
3. **Shrink the curator's job to text.** Amend `curator.org`: the curator
   emits structured fields the renderer consumes — per-card excerpt lines
   (hard cap ~55 chars/line, ≤4 lines) and 3 delta bullets per review — plus
   the prose sections. The curator never writes `<svg` anywhere; the
   orchestrator injects the rendered Image block into the final MDX. Enforce
   mechanically: the orchestrator rejects/strips model-authored svg.
4. **New "Question" section** first in the artifact (before "Final answer"):
   a Section titled `Question` (or `Prompt`) holding the user's question
   verbatim as RichText. Encode this in the curator contract AND have the
   orchestrator verify the section exists and matches the input question.

## Verification

- Extend `--self-test` to render a 2- and 3-participant diagram from fixture
  data and assert structure (card count, pill labels, delta glyphs, no
  `<style>`, inline styles present, viewBox/width/height sane).
- Re-run one cheap end-to-end smoke (reuse the KK4DA question or a fresh one),
  resubmit **ART-DSKQY** as the next version (same artifact id — it's the
  operator's review target), and verify via the API that: the Question section
  is first and verbatim; the Image decodes to an SVG whose card/pill/text-node
  counts match the participant roster; raw-report ids present.
- Load sequencing and smoke hygiene rules from round 2 still bind.

## Context

Your worktree branch chain: round-1 `0a5e1c97`, round-2 `0f46d34f` (branch
`task-56nrx-impl-r3`). Round-2 report:
`.orgasmic/tmp/dispatch/brief-TASK-56NRX-r2/brief-TASK-56NRX-r2-57919a70adf34ca8a6e5349734a180a0-last.txt`.
All other round-1/round-2 constraints stand. After this round the diff goes to
cross-vendor review, so keep the diff clean and the report precise.
