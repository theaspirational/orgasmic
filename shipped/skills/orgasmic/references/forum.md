# Multi-model forum

When the operator invokes `/orgasmic forum` without naming a mode, ask which
mode they want:

- `ask` — multi-model knowledge extraction;
- `critique` — multi-model critique of a supplied document;
- other modes are expected later.

For an available chosen mode, run its documented command below.

## Ask

Run one hard question through independent extraction, blind cross-review, and
curation into an Agent-Native MDX artifact. This is a report-only workflow: it
uses the native Rust CLI and never edits project source.

## Invocation

From the project checkout whose ledger should receive the run:

```bash
orgasmic forum ask \
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

The mode prints the parent task, extraction subtasks, cross-review subtasks,
curation subtask, and submitted artifact id as JSON. It launches every member
of a stage before one `dispatch-wait` barrier, closes every dispatch so its
report is promoted, and gives each cross-reviewer paths only to the other
participants' reports. The curator writes prose plus bounded summary fields;
the orchestrator inserts the verbatim Question section and renders the full SVG
card chain deterministically before it submits the artifact.

## Critique

Run a supplied UTF-8 document through independent critique, blind cross-review,
and curation into a prioritized verdict artifact:

```bash
orgasmic forum critique \
  --target-file /tmp/design.md \
  --focus 'security posture' \
  --participant 'stdio,hermes,openai/gpt-5.6-luna,low' \
  --participant 'stdio,hermes,google/gemini-3.7-flash,low'
```

`--target-file` is required, non-empty, and limited to 64 KiB. `--focus` is an
optional one-line steer. Participant, curator, source-ref, timeout, artifact,
and project flags have the same semantics as `ask`. The mode prints critique
subtasks instead of extraction subtasks; cross-review remains self-excluding.
The orchestrator owns the verbatim Target section and deterministic diagram.

Successful workers close through `manager dispatch-close --status done
--report-only`. The close promotes their reports, records `REPORT_ONLY=true`,
and requires no merge SHA.

On failure, keep the printed parent id and inspect its completed report tasks;
the verb best-effort closes only generations it launched and does not pretend
the parent run completed.
