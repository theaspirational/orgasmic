# TASK-FAS8R — rename `orgasmic extract` → `orgasmic forum` with mode subcommands

## Why

Operator feedback: `orgasmic extract` is misleading — a user cannot tell what
is extracted, from where, or why. The feature is really an umbrella for
multi-model deliberation modes; the first is knowledge extraction, a critic
mode is next (TASK-295X1), more later. The chosen umbrella name is **forum**.

## Deliverables

1. **CLI**: rename the `extract` verb (landed at b8723a5c + 2c8c81f3,
   `crates/orgasmic-cli/src/extract.rs`) to a `forum` command group with mode
   subcommands: `orgasmic forum ask` carries the current behavior and flags
   unchanged (`--question`/`--question-file`, `--participant`, `--curator`,
   `--from`, `--artifact-id`, `--project`). Structure the clap types so a
   future `forum critique` mode slots in beside `ask` without reshuffling.
   `orgasmic forum --help` must explain the umbrella in one sentence and list
   modes. NO back-compat alias for `extract`: the verb has never shipped in an
   installed runtime, there are zero users to break — delete the old name
   completely (help text, error strings, module/test names, docs).
2. **Skill**: update `shipped/skills/orgasmic/SKILL.md` routing from
   `/orgasmic extract` to `/orgasmic forum`; rename
   `references/extract.md` → `references/forum.md`. The reference must say:
   when the operator invokes the skill without naming a mode, the agent asks
   which mode they want — currently `ask` (multi-model knowledge extraction),
   with `critique` (multi-model critic) listed as coming and other modes
   expected later — then runs the chosen mode's documented command.
3. **Terminology sweep**: rename internal identifiers/docs where they say
   "extract" meaning THIS pipeline (file name `extract.rs` → `forum.rs` or
   similar, test names, progress strings). Do NOT touch the `extractor`
   prompt-spec family — "extractor/cross-reviewer/curator" name stage roles
   inside the ask mode and remain accurate; leave the prompt specs' content
   alone.
4. **Tests**: existing extract unit tests, the Python SVG parity fixture, and
   the report-only close tests must all pass after the rename; update names,
   not behavior. `cargo clippy -p orgasmic-cli --all-targets -- -D warnings`,
   `cargo fmt --check`, `git diff --check`.
5. **Proof**: `target/debug/orgasmic forum --help` and `orgasmic forum ask
   --help` output pasted or path-logged in your report; grep proof that no
   user-facing `orgasmic extract` string remains.

## Constraints

- Pure rename/reshape: no behavior changes to the pipeline. Smallest diff that
  achieves the rename cleanly.
- No live smoke required; the manager will smoke `forum ask` from the merged
  binary during the runtime reinstall that follows this task.
