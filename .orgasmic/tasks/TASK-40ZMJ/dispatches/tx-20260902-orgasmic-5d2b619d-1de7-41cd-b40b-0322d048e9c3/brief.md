# TASK-40ZMJ — the quota half: make a provider lockout visible before a worker dies

Read the task's Evidence section first.

## State

The health half SHIPPED today (commit 85fde20d, merged as 0a4f348e):
`orgasmic manager drivers --health [--json]` runs the same adapter preflight
the dispatch path uses and prints
`<harness> auth=<ok|missing|unknown (<why>)> quota=unknown (no probe)`.

The QUOTA half was skipped with a stated reason: no terminal-reason
classification and no 429/quota signal existed anywhere in the drivers, only
codex's passive `account.rate-limits.updated` event. It was blocked on
"letter item 1", TASK-XQCNA.

## THE BLOCKER IS NOW CLEARED

TASK-XQCNA shipped today (merge e796cb72): terminal runs now carry a
CLASSIFIED `ExitReason` and `dispatch-status` prints an exit reason plus an
evidence path. That is the classification the quota work was waiting for.
Build on it rather than inventing a parallel mechanism.

## What to build

1. A quota-lockout MEMORY: when a run terminates with a quota/rate-limit
   reason, record that the provider is locked and until when, where the next
   dispatch can see it.
2. A refusal on the dispatch path that names it, in the shape the task body
   asks for: `provider_quota: locked until <when>`.
3. `--force-preflight` to override that refusal deliberately.
4. `drivers --health` should report the remembered lockout instead of the
   current flat `quota=unknown (no probe)` when one is known.

## Honesty requirement

Do NOT invent a quota signal a provider does not send. Where the only
available input is codex's passive `account.rate-limits.updated` event, say so
and key on that. Where a harness gives nothing, `quota=unknown (no probe)`
must REMAIN the honest answer — the whole point of the health work was to stop
claiming knowledge the process never had.

## Guardrails

- Never set `ORGASMIC_ALLOW_BILLED_TESTS`; do not run anything that spends
  money. Test with a synthesised signal, not a real lockout.
- Use a PRIVATE cargo target dir passed as a FLAG, never exported.

## Acceptance

- A run classified as quota-terminated records a lockout, and the next
  dispatch to that provider is refused by name with the expiry.
- `--force-preflight` overrides it and the override is recorded on the tx.
- A provider with no quota signal still reports `unknown (no probe)`.
- clippy `-D warnings` and `cargo fmt --all --check` clean.
