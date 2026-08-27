# AGENTS.md

Work normally — no mandatory orchestration overhead.

Task/decision tracking for this repo lives in **orgasmic**, opt-in via the
`/orgasmic` skill. Load it only when the user invokes `/orgasmic` (or clearly
asks for an orgasmic feature: tracking tasks, recording decisions, dispatching
workers). Never hand-edit `.orgasmic/` ledger state — it goes through the
`orgasmic` CLI, and the live ledger lives at `~/.orgasmic/ledgers/orgasmic`,
not in this tree.
