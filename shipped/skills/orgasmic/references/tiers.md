# Tiers — compute, declare, then edit

Required before any manager-direct source edit.

## Authority

The tier system (trivial/ordinary/risky, triggers, floor rules) is the
resolved `default` workflow — first match wins:

1. `<project>/.orgasmic/workflows/default.org`
2. `~/.orgasmic/user/workflows/default.org`
3. `~/.orgasmic/current/shipped/workflows/default.org`

Read the resolved file; a project may override the shipped triggers. This
reference deliberately does not copy them.

## Mechanics

- Compute the tier from the task before the first edit — from the workflow's
  triggers, not from how the work feels. `orgasmic task get` prints priority
  and coupling.
- Declare: `orgasmic manager tier --task TASK-XXXXX --tier trivial`. Above the
  floor, name triggers (`--tier risky --triggers blast_radius,breadth`);
  `--reason` when the count alone would not explain it. Blocks on nothing,
  asks nobody; lands as a `manager.tier` tx.
- **No declaration, no manager-direct edit.** Undeclared = unclassified, not
  trivial → dispatch instead.
- Read-back: same verb without `--tier`; exits non-zero if nothing declared.
- Diff grew past a threshold → re-declare higher (no flag needed; both entries
  stay as audit trail). Lowering requires `--lower` plus a plain statement the
  first declaration was wrong.
- Never lower the floor because the work looked smaller from inside. Do not
  ask which tier applies — it is computed. Name tier + triggers in the report.

## What each tier means

- **Trivial** (declared): manager-direct, on a cheap fast model; CLI-only
  state writes; reconcile task/tx/handoff at natural pauses.
- **Ordinary**: dispatch an implementer with scope, acceptance criteria,
  source anchors, verification gates; review the diff before landing.
- **Risky**: grill/plan first → dispatch implementation with the constraints →
  independent review (different harness family, explicit `--effort`).

Delegation runs on committed history — see [`dispatch.md`](dispatch.md) for
the `--from` visibility trap.
