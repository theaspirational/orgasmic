# TASK-9TGQS — forum self-curation, multi-round forums, one final artifact

## Goal

Turn a forum run into a conversation the invoking session curates:

1. **Self-curation is the default.** When `--curator` is omitted,
   `orgasmic forum ask|critique` runs stage 1 (blind panel) and stage 2
   (self-excluding cross-review) as dispatches, persists a run manifest, and
   exits WITHOUT dispatching a curator. The model that invoked the CLI (the
   operator's interactive session) is the curator.
2. **Rounds accumulate.** The first self-curated call mints the forum — the
   parent task is the forum id. Later calls pass `--forum TASK-XXXXX` to add
   another round under the same parent. Ask and critique rounds mix freely
   (e.g. ask → discuss → critique the shaped doc → another ask with a
   different panel). Each round may name a different `--participant` panel.
3. **One final artifact.** When the operator is satisfied, the session runs
   the new verb `orgasmic forum curate` with its draft + diagram JSON. It
   validates through the SAME gates as today and submits ONE artifact. The
   deterministic SVG must render ALL rounds in one tree converging on a
   single curator card.
4. **Explicit `--curator <index|spec>` keeps today's behavior exactly:**
   single round, dispatched curator, immediate artifact. Do not regress it.

## Read first

- `crates/orgasmic-cli/src/forum.rs` — entire current pipeline, assembly
  gates, `render_pipeline_svg`, `resolve_curator`, tests.
- `shipped/prompt-studio/prompt-specs/curator.org`, `critique-curator.org` —
  the curation contract (the in-session curator must be held to the same one).
- `shipped/skills/orgasmic/SKILL.md` + `references/forum.md` — the skill the
  session follows; it must be rewritten for this flow.
- Recent commits `c74eb263..97eaf308` for current contracts (`--file`,
  `--curator` index-or-spec, About-this-run footer, headline titling).

## Design decisions (binding)

### Round accumulation
- Forum id = parent task id, minted on the first self-curated call, printed
  in that call's JSON result (add `forum` field naming it explicitly).
- `--forum TASK-XXXXX` joins an existing OPEN self-curated forum: same
  project, parent still open, manifest present. Refuse joining a forum that
  was created with a dispatched curator, already curated, or unknown.
- `--forum` with an explicit `--curator` is a contradiction — refuse.
- Round subtasks keep living under the one parent (continue the existing
  `TASK-<parent>.<n>` numbering across rounds).

### Persisted manifest
- One JSON file per forum under the ledger's managed tmp tree, keyed by the
  parent id (follow the `.orgasmic/tmp/dispatch/` convention with a sibling
  `forum/` dir). It records, per round: kind, verbatim input (question, or
  target text + focus + basename), panel (mode,harness,model,effort each),
  round task ids, promoted report paths, timestamps; plus forum-level:
  project, source ref, started_at, artifact-id override if given, state
  (open|curated).
- `forum curate` reads it; a second `curate` on a curated forum is refused.
- Each self-curated call prints (stderr or JSON — your call, but stable) the
  manifest path, every promoted report path, and the compiled curation
  contract path so the session can read everything without guessing. Compile
  the curator prompt spec (ask: `curator`, critique: `critique-curator`,
  minus the dispatch-finalize Completion section — the session does not run
  `dispatch finalize`) into a file next to the manifest each round.

### `orgasmic forum curate`
- Flags: `--forum TASK-XXXXX --draft <mdx> --diagram <json> --identity
  mode,harness,model,effort` (identity = the session model curating; parse
  with `parse_participant`; it appears as the curator everywhere: About
  footer, diagram curator card, evidence). Plus `--project` guard.
- Runs the full existing gate set: model-SVG rejection, placeholder counts,
  first-section verbatim + decoy defense, required-section order, raw-task
  boundary checks (every round's task ids), run-stats placeholder last,
  headline handling. Then renders the multi-round SVG, injects, submits the
  artifact, writes parent Evidence, finishes the parent, marks the manifest
  curated.
- First verbatim section = ROUND 1's input (Question for ask, Target for
  critique), exactly as today. Later rounds' prompts are NOT separate
  verbatim sections; they appear in the diagram and in an orchestrator-
  rendered `- **Rounds:**` list inside the About-this-run footer (round
  number, kind, one-line clipped prompt, task ids).
- Bookkeeping for the curation itself: mint the curation subtask when
  `curate` runs, record the identity + draft/diagram paths on it, and close
  it honestly without inventing a dispatch (no fake worker records).

### Multi-round diagram
- Extend the diagram JSON with an optional `rounds` array:
  `{"rounds": [{"round": 1, "kind": "ask", "extracts": [...], "reviews":
  [...]}, ...], "curator_summary": "...", "headline": "..."}` — same
  per-entry shapes and caps as today. The legacy single-round top-level
  `extracts`/`reviews` shape stays accepted (dispatched-curator mode keeps
  emitting it; specs unchanged there).
- Renderer: rounds stack vertically in order — each round renders its prompt
  panel, extract cards, and review cards like today's layout — and every
  round's review row feeds the ONE curator card at the bottom.
- **Byte-identity constraint:** a single-round ask rendered through the old
  path must stay byte-identical to
  `crates/orgasmic-cli/tests/fixtures/TASK-FBSZ2-pipeline.svg`
  (`renderer_matches_stored_python_fixture` must pass untouched). Achieve
  multi-round by generalizing around that fixed output, not by changing it.
- Respect the existing SVG constraints (inline style attrs only, no
  `<style>`, no font attrs stripped by the runtime — see the existing
  renderer's conventions; artifact viewer ignores height props).

### Skill rewrite (`shipped/skills/orgasmic`)
- `/orgasmic forum` flow for self-curation: ask the operator for mode and
  panel if unspecified; run the CLI; read the manifest, compiled contract,
  and every promoted report; curate IN CHAT (discuss, synthesize, iterate
  with the operator); offer more rounds (`--forum <id>`); when the operator
  says done, write the draft MDX + diagram JSON per the compiled contract
  and run `forum curate --identity` with the session's REAL model identity
  (the skill must tell the agent to state its actual harness/model, e.g.
  `session,claude,<model-id>,interactive` — never a placeholder).
- Document the dispatched-curator path as the non-interactive alternative.

## Hard constraints

- All existing forum tests keep passing; the ask fixture stays byte-identical.
- New unit tests: manifest round-trip (write→read, mixed rounds), `--forum`
  refusal matrix (unknown forum, curated forum, dispatched-curator forum,
  `--forum` + `--curator` together), curate gate parity on a multi-round
  draft (decoy first section, missing round task ids, run-stats-last),
  multi-round diagram JSON validation (rounds cover every round's tasks
  exactly once), and a multi-round renderer structure test (per-round card
  counts, one curator card, arrows from every round's reviews).
- `cargo fmt --all`; `cargo clippy -p orgasmic-cli --all-targets -- -D
  warnings`; full `cargo test -p orgasmic-cli --bin orgasmic`.
  (Note: `empty_private_targets_never_run_another_worktrees_binary` fails
  under a custom `CARGO_TARGET_DIR`; run tests with the default target dir.)
- No live billed dispatches; the operator runs the end-to-end smoke.
- Keep shared code shared — no forked pipeline copies. Splitting forum.rs
  into modules is welcome if it keeps the diff honest.

## Deliverables

- CLI behavior above; three-line summary of any contract decision you had to
  make beyond this brief, in the report.
- Updated skill (SKILL.md + references/forum.md) and any prompt-spec edits
  needed for the compiled in-session contract.
- Report to `/tmp/TASK-9TGQS-report.md`: what changed, what you tested, and
  the exact commands a session follows for a two-round (ask then critique)
  self-curated forum.

## Completion

Write the report, then make your terminal action:
`orgasmic dispatch finalize --task TASK-9TGQS --summary-file /tmp/TASK-9TGQS-report.md --commit`
Exiting without finalization is a failed run.
