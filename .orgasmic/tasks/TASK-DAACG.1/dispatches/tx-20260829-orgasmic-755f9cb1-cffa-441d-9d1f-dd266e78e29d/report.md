# Participant

`codex · openai · gpt-5.6-luna · effort low` (implementer-codex-chat-stdio)

# Direct Answer

Prefer append-only events when the tool needs an auditable history, crash/restart recovery, deterministic replay, synchronization across machines, or the ability to explain/undo a transition. Mutable state is simpler and preferable for derived read models, caches, indexes, and data whose history has no product value.

An event log is not a substitute for a current-state representation: replay cost, storage growth, and retention/privacy pressure eventually require snapshots or compaction. Snapshot when startup/recovery replay becomes materially slow or the log is too large to load safely; compact when old events are provably redundant, reclaimable, or vendor-owned and retention policy permits removal. Preserve enough events (or a snapshot plus a checkpoint boundary) to reproduce the current state, audit decisions, recover after partial writes, and diagnose corruption. Never compact merely because a file is large without a verified plan, atomic application, and rollback/recovery story.

# Claims and Evidence

1. **Append-only is the right authority for facts and transitions.**
   - Evidence: `shipped/schema/journal.org` defines `state_transitioned`, `property_updated`, `regenerated`, and `dispatch.linked` as append-only facts; it calls `journal.org` a ledger and forbids generic rewrites.
   - Reasoning: immutable facts preserve causality and auditability; current state can be folded from them.
   - Confidence: high. Cheapest verification: append two transitions, fold them, and confirm the original bytes remain.

2. **Append-only records are valuable for replay and recovery.**
   - Evidence: `crates/orgasmic-core/src/session.rs` documents the JSONL stream as authoritative for replay, recovery, and UI rendering; `SessionWriter` opens files in append mode.
   - Reasoning: a durable ordered stream lets a restarted process reconstruct state and lets operators inspect what actually happened instead of only seeing the last overwrite.
   - Confidence: high. Verification: restart during a run and compare reconstructed state with the event fold.

3. **Mutable state remains appropriate for projections.**
   - Evidence: the session module explicitly describes the persisted stream as authoritative while the UI consumes folded events; the journal schema distinguishes ledger authority from generic readers.
   - Reasoning: indexes, caches, and UI projections are replaceable derivatives. Making them append-only adds cost without preserving business history.
   - Confidence: medium-high (architectural inference). Verification: delete/rebuild the projection from authoritative records and compare outputs.

4. **Snapshots are needed when replay has an operational ceiling.**
   - Evidence: `crates/orgasmic-daemon/tests/run_inventory_wire.rs` exercises `/api/runs/history/compact`, checks planned reclaimable bytes, requires a dry run to change no bytes, and verifies the applied operation actually shrinks the board.
   - Reasoning: replaying every historical event increases startup latency, memory use, and exposure to old-format/corrupt records. A snapshot/checkpoint bounds recovery work while retaining the log tail needed for audit.
   - Confidence: high. Verification: measure cold-start/recovery time and peak memory as event count grows, with and without a checkpoint.

5. **Compaction must be explicit, scoped, and validated.**
   - Evidence: the same daemon test requires a confirmation token tied to the plan, rejects a stale confirmation, applies exactly the planned reclaim amount, and asserts structured-transport files remain byte-identical.
   - Reasoning: compaction can delete evidence, race with writers, or make the operator confirm a different set than the one inspected. Plan/confirm, stable identity, and atomicity reduce those failure modes.
   - Confidence: high. Verification: interrupt compaction at each write boundary and verify either the old valid state or the new valid state is recoverable.

6. **Retention and corruption are independent reasons to compact or snapshot carefully.**
   - Evidence: `shipped/schema/journal.org` has a 500 KiB low-severity lint but deliberately no rotation; `crates/orgasmic-core/src/tx.rs` says the writer accepts only formats it can read back and refuses round-trip loss.
   - Reasoning: size alone is a warning, not permission to discard history. Old schema versions, malformed records, sensitive payload retention, and partial/truncated writes require migration, quarantine, or a verified snapshot—not blind deletion.
   - Confidence: high. Verification: test truncated tail, unknown event type, schema upgrade, and retention-expiry cases separately.

# Unique or Easily Missed Findings

- Snapshots and compaction solve different problems: a snapshot bounds replay while compaction removes or rewrites redundant history. A snapshot can be added without deleting the source log; compaction needs a stronger retention and recovery contract.
- Keep an event-log tail after the snapshot. The snapshot boundary must be durable and unambiguous; otherwise a crash can cause lost or double-applied events.
- Comments are a deliberate exception in this repository: they may be edited through a structured optimistic-concurrency operation, while facts stay append-only; deletion is represented by a tombstone so reply chains remain valid (`shipped/schema/journal.org`).
- Vendor-owned/rendered payloads may be reclaimable while structured authoritative history is not; the daemon test explicitly protects the latter.

# Uncertainties and Contradictions Within This Report

- The repository has a concrete compaction path for session history, but the inspected schema says journal rotation is not yet implemented. Therefore “compact when large” is a general design rule, not a claim that every orgasmic ledger currently supports it.
- “Materially slow” is workload-dependent; no universal event-count threshold should be chosen without measurements.
- Snapshots can improve availability but create another authoritative artifact and consistency boundary. If snapshot creation is not atomic/checksummed, it can be worse than replaying the log.

# Verification Targets

- Benchmark recovery latency and peak memory against event-log size.
- Crash-inject snapshot publication and compaction; require old-or-new valid recovery, never a mixed state.
- Verify idempotence/deduplication at the snapshot boundary.
- Verify unknown-event, truncated-tail, schema-migration, and retention-policy behavior.
- Confirm audit/reply/deletion semantics survive compaction and that protected structured records are untouched.

# Sources Consulted

- `orgasmic task get TASK-DAACG.1` (task record and report-only scope).
- `shipped/schema/journal.org` (event vocabulary, mutability, ledger rules, size policy).
- `crates/orgasmic-core/src/session.rs` (append-only session writer and replay/recovery authority).
- `crates/orgasmic-core/src/tx.rs` (append-only tx format and read-back safety).
- `crates/orgasmic-daemon/tests/run_inventory_wire.rs` (dry-run, confirmation, scoped compaction, and byte-level preservation checks).
