# Agent selection — harness, model, effort

Kind, mode, harness + the harness's own model/effort decide who runs a
dispatch. `orgasmic manager drivers` lists installed pairs. Model and effort
are harness vocabulary: per-dispatch flags, passed through unvalidated, never
stored anywhere in orgasmic.

## The one allowed question

At the **first dispatch of the session** (not session start), ask the operator
once, in one turn:

1. Which harness family to implement with.
2. What effort level review and risky stages carry.

Carry both answers for the rest of the session; stop asking. Session-scoped,
not stored. No operator present → dispatch anyway with the selection rationale
in `--reason` (it lands on the dispatch tx). Never select by silent default;
an unasked dispatch with a stated reason is fine.

## Rules

- **Reviewer independence** (runtime policy): the reviewer runs on a different
  harness family than the run under review. Same-family review requires a
  stated reason on the dispatch. Author's harness unknown → ask which family
  wrote it; defaulting to your own family risks same-family review.
- **Review and risky stages carry an explicit `--effort`.** Unset effort means
  the harness chose silently and the value is unrecoverable afterwards (run
  records carry no model/effort field). Unset is allowed as a stated choice,
  never as an oversight.
- Ordinary/trivial work may run at harness default effort.
