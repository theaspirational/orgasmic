# Participant

Extract — codex · openai · gpt-5.6-luna · effort low

# Direct Answer

Prefer append-only event records when history, causality, auditability, replay, conflict diagnosis, or offline/Git synchronization matters more than cheap direct reads. Treat the event stream as the durable record and derive mutable projections for queries and UI.

Prefer in-place mutable state for working/materialized files whose identity is stable and whose current value is the product, provided a single writer or equivalent concurrency control owns updates. In-place state is not a substitute for an audit trail when transitions or decisions must be explainable.

Snapshots are needed when replay/rebuild is too slow or operationally risky, before destructive or difficult-to-undo operations, and when a consistent point-in-time recovery target is required. Compaction is needed when append-only history makes startup, replay, indexing, storage, or human inspection unacceptably expensive. Compaction must preserve a reconstructable checkpoint plus the retained event suffix (or an equivalent verifiable archive); otherwise it destroys audit/replay capability.

# Claims and Evidence

1. **Append-only is the right authority for causal history.** The repository's archived design describes the system as combining a structured event log with plain-text, Git-native, append-only project memory. A separate lifecycle analysis says mutable summaries cannot explain creation, accepted evidence, stage-transition reasons, waivers, or projection provenance. **Reasoning:** those questions require ordered facts and provenance, not only the latest value. **Confidence: high.** Cheapest verification: inspect current transaction/journal readers and confirm whether each lifecycle fact has an immutable event with stable identity and sequence.

2. **Mutable state is appropriate for projections and working documents.** The archived design explicitly labels decision/glossary/architecture files as daemon-written working files with stable IDs revised in place, “not append-only.” **Reasoning:** current-value reads and edits are simpler and smaller when history is not the file's primary purpose. **Confidence: high.** Cheapest verification: mutate one working record and check that its derived/indexed view remains correct while the authoritative event/audit record still exists where required.

3. **Snapshots protect high-risk boundaries; they do not replace recovery reconciliation.** The design requires graph snapshots before grilling, reconciliation, controlled daemon restart/update, and manual restore; it separately states that restart recovery is boot reconciliation, not snapshot restore. **Reasoning:** snapshots provide rollback of graph state, while reconciliation accounts for external/runtime facts that a file copy cannot recreate. **Confidence: high.** Cheapest verification: exercise restart/recovery with an interrupted run and confirm both snapshot creation and boot reconciliation behavior.

4. **Compaction is a performance/storage response, not normal mutation.** An event log should be compacted when replay latency, index rebuild time, disk growth, merge/scan cost, or operator usability crosses an explicit budget. **Reasoning:** compacting earlier weakens forensic history without operational benefit; compacting later makes recovery and reads degrade. **Confidence: medium-high** (general systems principle; the checked repository material specifies snapshots but does not define a complete compaction policy). Cheapest verification: benchmark cold rebuild and storage growth against stated thresholds using a representative event corpus.

5. **Compaction must be crash-safe and provenance-preserving.** A safe scheme writes a checkpoint/snapshot and compaction manifest atomically, retains the un-compacted suffix, validates hashes/sequence ranges, and only then retires old segments (or archives them). **Reasoning:** interruption during replacement must leave either the old log or a complete checkpoint-plus-suffix, never an ambiguous half-history. **Confidence: high.** Cheapest verification: inject interruption at each rename/delete boundary and verify replay yields the same state and event sequence.

6. **Snapshots and compaction solve different failure modes.** Snapshots bound replay and provide rollback; compaction bounds log growth and rebuild cost. Neither alone solves corruption, concurrent writers, lost external side effects, or semantic bugs. **Confidence: high.** Cheapest verification: classify each recovery scenario separately: corrupted segment, interrupted write, bad projection code, external provider mismatch, and operator-requested undo.

# Unique or Easily Missed Findings

- A snapshot is not automatically a rollback of the whole workspace: the checked design says snapshots restore the graph, while file edits remain Git's domain.
- Restart recovery must not blindly restore a snapshot; it must reconcile durable run/session/provider evidence after rebuilding projections.
- “Append-only” should apply to facts/events, not necessarily every file. Stable-ID working files can be mutable projections without weakening the event authority.
- A compaction checkpoint without an auditable boundary (source sequence/hash, schema/version, and retained suffix) is silent data loss disguised as optimization.
- Local-first tools need explicit ownership/concurrency rules: append-only reduces overwrite risk, but two writers can still produce conflicting events requiring deterministic ordering or visible conflict status.

# Uncertainties and Contradictions Within This Report

- The repository sources consulted are archived design material, and the archive itself warns that current code and graph state take precedence. They support the principles above but do not establish the current implementation's exact retention, archival, or compaction policy.
- “Snapshot” can mean a rollback copy, a replay checkpoint, or a materialized read snapshot. These have different guarantees; the report uses the repository's graph-copy meaning where citing repository evidence and uses checkpoint meaning only for compaction design.
- There is a tradeoff between retaining every event forever and privacy/secret-removal requirements. Immutable history improves auditability, but sensitive payloads may require redaction-by-reference, encrypted archival, or a documented legal retention policy rather than naive permanent retention.

# Verification Targets

- Current transaction/event schema: sequence monotonicity, stable event IDs, timestamps, actor/source, schema version, and conflict ordering.
- Projection rebuild: deleting derived indexes and rebuilding from the retained event/checkpoint set must be deterministic and byte-stable where promised.
- Snapshot atomicity: snapshot contents must be complete and restorable after process interruption; manual restore should itself create an undo snapshot if that is the operational contract.
- Compaction protocol: manifest/checkpoint hash, first/last sequence, crash recovery, concurrent writer fencing, and whether old segments remain available for audit.
- External side effects: provider/runtime handles cannot be recovered by file snapshot alone; verify reconciliation and idempotency.

# Sources Consulted

- `archive/21_05_26_init/spec.html` — mission/data-model text describing structured event log and append-only project memory; daemon-written working files revised in place; restart reconciliation; snapshot triggers and graph-only restore semantics.
- `archive/grilling-session-local-first-cockpit-report.md` — lifecycle analysis explaining why a mutable summary cannot provide causal/audit history and why an append-only lifecycle log is needed.
- `shipped/project-scaffold/project.org` — current scaffold placeholder; no task-specific operating constraints.
- `shipped/project-scaffold/gotchas.org` — current scaffold format guidance; no populated gotchas.
