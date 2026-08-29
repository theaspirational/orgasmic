# TASK-932SH.5 — Curation completion report

## Changed

- `/tmp/TASK-932SH.5-curation.mdx` — curated prose draft (MDX) with the full block contract: RichText header with run stats and roster, Callout provenance warning, `__ORGASMIC_QUESTION_SECTION__` placeholder, Final answer Section, From question to answer Section with `__ORGASMIC_PIPELINE_DIAGRAM__` placeholder and Raw reports list, Knowledge map Section with Tabs (Shared core, Unique finds, Contradictions, To verify with Checklist), and final QuestionForm Section.
- `/tmp/TASK-932SH.5-diagram.json` — structured diagram fields: 2 extracts (TASK-932SH.1, TASK-932SH.2), 2 reviews (TASK-932SH.3, TASK-932SH.4), curator summary.

## Verification Gates

- All four promoted reports read in full: TASK-932SH.1 (extraction, gpt-5.6-luna), TASK-932SH.2 (extraction, gemini-3.7-flash), TASK-932SH.3 (cross-review, gpt-5.6-luna), TASK-932SH.4 (cross-review, gemini-3.7-flash).
- Diagram JSON validated: 2 extracts × 4 lines each (max 39 chars ≤ 43 limit), 2 reviews × 3 bullets each (tags `?`, `+`, `=` in order), non-empty curator summary. All 4 tasks included exactly once in manifest order.
- MDX contains only orchestrator placeholders for Question (`__ORGASMIC_QUESTION_SECTION__`) and diagram (`__ORGASMIC_PIPELINE_DIAGRAM__`), no inline Question or Image/SVG.
- Every raw-report task id named in the Raw reports list: TASK-932SH.1, TASK-932SH.2, TASK-932SH.3, TASK-932SH.4, TASK-932SH.5.
- Participant identity format is `harness · vendor · model · effort` throughout; provenance tags use `(vendor · model)` at paragraph/bullet level.
- Block shapes match existing artifacts (ART-PT7ZX, ART-MKRG1): RichText, Callout, Section, Tabs/Tab, Checklist, QuestionForm.

## Unmet Criteria

None. All acceptance criteria met.

## Residual Risk

- The block contract files referenced in the brief (`shipped/prompt-studio/prompt-specs/artifact-generator.org` and `ui/src/lib/artifacts/__fixtures__/all-blocks.ts`) were not found in the ledger filesystem; block shapes were derived from existing shipped artifacts (ART-PT7ZX, ART-MKRG1) which match the contract description exactly.
- The arXiv paper (2607.02059) figures (median age 155 days, 70% upstream bypass, 41,777 license candidates) are from a single dataset and flagged in the To verify tab; they are presented as claims, not verified facts.
- No empirical study exists for vendored vs. registry-driven patch latency; this is flagged as a verification target.
