# TASK-FBSZ2 — native `orgasmic extract` + truthful report-only dispatch close

## Why

The multi-model extraction orchestrator
(`shipped/skills/orgasmic/scripts/multi-model-extract.py`, landed via
TASK-56NRX, merged at a13b6896/fca620d4) is the only piece of the product that
is not the self-contained Rust binary: it requires `python3` on the operator's
machine. It also carries a documented lie-shaped workaround: successful
report-only workers are closed `--status aborted` solely to promote their
reports, because `dispatch-close --status done` demands a merge sha
(`shipped/skills/orgasmic/references/extract.md`, "Existing close limitation";
also flagged as review Fix Direction 4 in
`.orgasmic/tmp/dispatch/brief-TASK-56NRX-review/…-last.txt` in the ledger
checkout at `~/.orgasmic/ledgers/orgasmic/`).

## Deliverables

1. **`orgasmic extract` subcommand** (orgasmic-cli), a 1:1 behavioral port of
   the Python orchestrator. The script IS the spec: same flags
   (`--question`/`--question-file`, repeatable `--participant
   mode,harness,model,effort`, `--curator N`, `--from`, `--artifact-id`), same
   staged flow (parallel extract → blind cross-review with self-exclusion →
   curate), same up-front validations (roster against `manager drivers`
   catalog, question placeholder/leading-dash rejection), same deterministic
   ART-MKRG1-style SVG renderer (43-char caps, 2..N scaling, inline styles,
   vendor colors), same MDX assembly (verbatim Question section first,
   placeholder substitution, model-SVG rejection, boundary-aware raw-task
   check), same JSON result shape (parent, subtasks, artifact id).
2. **Report-only close primitive.** Add a truthful way to close a successful
   report-only dispatch — your design call (e.g. a `--report-only` close mode
   recording a distinct tx type, or a report-only dispatch kind), consistent
   with the tx-type conventions in the daemon. It must promote the record like
   today's close paths and must not require or fabricate a merge sha. The
   native verb uses it; delete the aborted-close workaround and its
   documentation notes in `references/extract.md` and the prompt specs.
3. **Retire the script.** Delete `multi-model-extract.py`; reduce
   `shipped/skills/orgasmic/references/extract.md` to documenting the native
   verb. Update `SKILL.md` routing accordingly.
4. **Port the tests.** The script's `--self-test` assertions (renderer
   structure/scaling, cap clipping, `.1`/`.11` boundary fixture, question
   rejection, model-SVG rejection, hostile-question escaping round-trip)
   become Rust unit tests. Add one fixture comparing the Rust renderer's SVG
   against a stored known-good SVG from the Python renderer to prove parity
   before the script is deleted.
5. **Smoke.** One cheap two-participant end-to-end run through the native
   verb producing a submitted artifact; report its id, parent task, and that
   every promoted report is readable via `GET /api/tasks/:id/dispatches`.

## Constraints

- Reuse the existing daemon/CLI machinery from inside the CLI crate rather
  than shelling out to `orgasmic` where an internal call exists; where the
  script shelled out to daemon HTTP verbs, use the same API surface the CLI
  already uses.
- Never hand-edit the ledger. Load-sequence heavy work: no cargo build/test
  concurrent with a live smoke; wait for 1m load < 4 before dispatching smoke
  workers.
- The three prompt specs (`extractor`, `cross-reviewer`, `curator`) keep their
  contracts except for the close-workaround language they carry; the curator's
  `USES_PARTS: output_style_plain_english` stays.
- Close every generation you launch; finalize blocked rather than claiming
  completion if the smoke cannot complete.

## Acceptance

- `python3` is no longer needed anywhere: the script is gone, `orgasmic
  extract --help` documents the verb, and the smoke ran through the binary.
- No dispatch in the smoke run was closed `aborted` while actually successful.
- Renderer parity fixture passes; all ported self-test assertions pass under
  `cargo test`.
- Report names the smoke's parent task, subtasks, and artifact id.
