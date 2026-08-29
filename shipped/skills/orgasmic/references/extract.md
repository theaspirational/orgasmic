# Multi-model extraction

Run one hard question through independent extraction, blind cross-review, and
curation into an Agent-Native MDX artifact. This is a report-only workflow: it
uses existing task, dispatch, prompt compiler, and artifact verbs and never
edits project source.

## Invocation

From the project checkout whose ledger should receive the run:

```bash
python3 <skill-dir>/scripts/multi-model-extract.py \
  --question-file /tmp/question.txt \
  --participant 'stdio,hermes,openai/gpt-5.6-luna,low' \
  --participant 'stdio,hermes,google/gemini-3.7-flash,low'
```

`--question "..."` is the short-question alternative. Participants are
`mode,harness,model,effort`; repeat `--participant` at least twice. A
`provider/model` model id supplies the vendor. Bare models are accepted for
`codex` (OpenAI) and `claude` (Anthropic). The first participant curates unless
`--curator N` selects another 1-based roster entry. Use `--from <git-ref>` when
the workers should branch from a ref other than the invoking checkout's HEAD.
Pass `--artifact-id ART-XXXXX` only when intentionally submitting a new version
of an existing artifact; otherwise the orchestrator mints a fresh id.

The script prints the parent task, extraction subtasks, cross-review subtasks,
curation subtask, and submitted artifact id as JSON. It launches every member
of a stage before one `dispatch-wait` barrier, closes every dispatch so its
report is promoted, and gives each cross-reviewer paths only to the other
participants' reports. The curator writes prose plus bounded summary fields;
the orchestrator inserts the verbatim Question section and renders the full SVG
card chain deterministically before it submits the artifact.

## Existing close limitation

`manager dispatch` currently exposes only code-oriented `implementer` and
`reviewer` kinds. A successful report-only implementer cannot be closed `done`
without claiming a source merge. The orchestrator therefore closes these
successful dispatches `aborted` solely to promote their reports, records the
promoted report as task evidence, then advances the report task through the
normal task lifecycle. Do not fabricate a merge SHA. Remove this workaround
when a report-only dispatch close exists.

On failure, keep the printed parent id and inspect its completed report tasks;
the script best-effort closes only generations it launched and does not pretend
the parent run completed.
