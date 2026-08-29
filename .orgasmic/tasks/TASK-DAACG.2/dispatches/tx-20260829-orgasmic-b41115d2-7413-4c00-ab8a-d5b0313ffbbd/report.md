# Extraction Report: Event Sourcing vs. Mutable State in Local-First Tools

**Participant:** claude · anthropic · claude-haiku-4-5-20251001 · effort low

## Direct Answer

Local-first tools should prefer append-only event records when **offline sync is required, concurrent edits must merge, or audit history adds product value**. Mutable state suffices when state is fully-transient, only one writer exists, or local consistency is sufficient.

**Snapshots become necessary** when event logs grow beyond practical replay time or storage constraints. **Compaction** is required when:
- Replay time exceeds user-acceptable latency (typically 100ms–1s)
- Storage capacity becomes a binding constraint
- Tombstones and deletes accumulate faster than new state

---

## Claims and Evidence

### Claim 1: Append-only is Required for Offline Sync

**Reasoning:** Offline-capable systems need to record local edits *before* merge, and reconstruct the full history for conflict resolution. In-place mutation discards the path taken; append-only preserves it.

**Evidence:**
- All established local-first systems (CRDTs, Yjs, Automerge) use append-only logs as the ground truth
- Git itself is the most visible local-first tool and uses append-only commits for offline work
- In-place mutation loses causal history; merging two in-place states requires a lossy heuristic

**Confidence:** Very high. This is the core tradeoff in local-first design.

**Verification step:** Check whether a tool claims offline-first design *without* an append-only log somewhere in its stack.

---

### Claim 2: Mutable State is Sufficient for Single-Writer, Transient State

**Reasoning:** If state is:
- Written only by one process (no concurrency)
- Discarded on reload (no durability requirement)
- Not required for audit or versioning

Then the overhead of append-only logging is pure waste.

**Evidence:**
- Session state in stateless web servers (loaded and thrown away per request)
- Ephemeral runtime caches
- Single-user configuration files without version history

**Confidence:** High. This is YAGNI applied to durability.

**Verification step:** Ask: "If this process crashed, would we need to recover from the log?" If no, mutable is fine.

---

### Claim 3: Snapshots Are Needed When Replay Latency Exceeds User Tolerance

**Reasoning:** Replaying 10,000 events to load state takes time proportional to log size. Once latency is felt (usually 50–200ms for UI loads), snapshots become mandatory. At 100k events, replay becomes a multi-second operation.

**Evidence:**
- Event stores in financial systems always use snapshots (e.g., EventStoreDB, axon-framework)
- Kafka-based systems use "table" topics (materialized snapshots) for fast rebuilds
- Version control (Git) uses shallow clones and GC to avoid replaying all history on fetch

**Confidence:** High. This is a measurable performance boundary.

**Verification step:** Measure replay time at your expected scale. If it's under 50ms, you're safe. Over 200ms, snapshots are mandatory.

---

### Claim 4: Compaction is Needed When Tombstones Dominate or Deletes Accumulate

**Reasoning:** An entity created, updated 100 times, and deleted still generates 102 events. Over time, the log grows far larger than the current state. Compaction rewrites the log to remove redundant history.

**Evidence:**
- E-commerce order systems compact old customer data (orders deleted after 7 years)
- File systems use garbage collection to reclaim space from deleted-file records
- The "write amplification" problem in LSM trees is solved by compaction

**Confidence:** High for storage-constrained systems; medium for unlimited-storage scenarios.

**Verification step:** Calculate: `(current_state_size / event_log_size)`. If this ratio is <0.1 (log is 10x the state), compaction likely helps.

---

## Unique or Easily Missed Findings

1. **Snapshot + Tail Log is the Real Pattern**
   Most systems don't pick *all* append-only or *all* mutable. The standard is: snapshot every N events (or every M seconds), discard old events, keep a short tail log for durability after the snapshot. This combines compaction with fast replay.

2. **Offline Sync Requires Causal Ordering, Not Just Events**
   A naive event log doesn't merge well—you need timestamps or vector clocks to detect conflicts. The "append-only" decision is really: "append-only *with* metadata for causal reconstruction."

3. **Compaction is Not Transparent**
   Compacting an event log changes its semantics: observers downstream (audit, replication) may not see the full history. This requires explicit policy (e.g., "keep raw events for 1 year, compacted after that").

4. **Mutable State + Async Replication Can Hide Merge Bugs**
   If you use mutable state but sync asynchronously, merge conflicts are discovered *after* both replicas have diverged. This is harder to reason about than event-based conflict detection.

5. **Cold Start Latency vs. Warm State**
   Append-only logs make cold starts (loading state from zero) expensive, but warm updates cheap. This is opposite mutable state's profile. Choose based on your access pattern.

---

## Uncertainties and Contradictions Within This Report

1. **"User Tolerance" for Replay Latency is Vague**
   I claim 50–200ms is the boundary, but this varies wildly:
   - A background sync can tolerate seconds
   - A text editor UI must load in <100ms
   - A data warehouse can wait minutes
   
   This report should be: "measure your own latency," not a universal threshold.

2. **Compaction Trade-offs Are Situational**
   A system optimizing for storage will compact aggressively; one optimizing for auditability will keep all events. There's no universal "when to compact"—it depends on your policy.

3. **Snapshots Add Operational Complexity**
   Snapshots introduce new failure modes (incomplete snapshots, snapshot corruption, snapshot staleness). I claim they're "necessary" but don't quantify the cost of managing them. For some teams, the complexity is prohibitive.

4. **CRDT vs. Operational Transform Both Use Append-Only but Differently**
   CRDTs replay the full history to merge; OT uses transform functions to rebase. Both append-only, but the compaction strategy differs. This report treats them as equivalent; they're not.

---

## Verification Targets

To verify the major claims in a real system:

1. **Measure replay latency** on your largest realistic dataset. If <50ms, snapshots may not be needed yet.
2. **Trace merge conflicts** in your system: how many are detected at event-log time vs. after sync? Events with metadata catch more.
3. **Calculate write amplification** of your current state representation. If a deleted entity's events outnumber its state size by >10×, compaction is overdue.
4. **Interview your users** on offline-first UX: if they never use the tool offline, append-only logging adds no value.
5. **Audit your failure modes**: have you ever needed to recover from a corrupt state? If yes, the append-only log was critical; if no, mutable state might suffice.

---

## Sources Consulted

- **Observed systems:** Git (append-only commits + GC compaction), Yjs (append-only + snapshots), Automerge (append-only + snapshots), EventStoreDB (snapshots + tail log)
- **First principles reasoning:** The CAP theorem and its implications for local-first sync; the fundamental difference between causal and total ordering
- **Performance data:** Typical OLTP latency budgets (50–200ms for interactive systems)
- **No external literature sources** — this is grounded in deployed local-first systems and distributed systems theory, not academic papers

---

## Confidence Summary

| Claim | Confidence | Risk Level |
|-------|-----------|-----------|
| Append-only needed for offline sync | Very High | Low — well-established |
| Snapshots needed when latency > user tolerance | High | Medium — threshold is situational |
| Compaction needed for tombstone-heavy workloads | High | Low — measurable condition |
| Mutable state sufficient for single-writer, transient | High | Low — straightforward |
| CRDT metadata required for merge correctness | Medium | High — many systems get this wrong |

---

**Report generated by:** claude-haiku-4-5-20251001 (low effort)  
**Date:** 2026-08-29  
**Status:** Complete and ready for synthesis
