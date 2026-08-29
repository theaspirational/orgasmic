# Cross-Review Delta Report

## Reviewer
- codex · openai · gpt-5.6-luna · effort low

## Delta

- ? **claude · anthropic · claude-haiku-4-5-20251001 · effort low:** The report presents numeric thresholds (10K–100K events, 100 ms, 500 ms, 1 s, and 1M events) as practical inflection points, but supplies no reproducible measurements and later concedes that event size, hardware, and workload dominate. These should be treated as example budgets, not decision rules.
- ? **claude · anthropic · claude-haiku-4-5-20251001 · effort low:** “Append-only provides no intermediate state corruption,” “events naturally compose,” and “deterministic conflict resolution” are conditional claims. Atomic durable append, event identity/idempotency, ordering/causality, and a defined merge policy are required; append-only storage alone does not provide them.
- ? **claude · anthropic · claude-haiku-4-5-20251001 · effort low:** The claim that Cloud Firestore recommends Realtime Database “because it uses event-based sync” is weakly supported and not needed to answer the question; it should be removed or source-checked.
- ? **claude · anthropic · claude-haiku-4-5-20251001 · effort low:** “Snapshots and compaction become mandatory” is too absolute. They are required only when measured recovery/storage/query budgets are exceeded, or when operational retention/privacy policy requires it; a bounded log or cheap projection may be sufficient.
- + A decision rule is missing for event granularity: prefer events only when operations are stable, replayable, and meaningful for synchronization/audit. If records are large opaque blobs, high-frequency UI noise, or the domain has no useful replay semantics, mutable state plus a durable journal/WAL may be simpler.
- + Compaction needs a correctness protocol, not just a performance trigger: snapshot creation and log truncation must be atomic/recoverable, preserve schema/version information, and retain events needed by offline peers or unresolved causal dependencies. Otherwise compaction can cause data loss or make later merges impossible.
- + A key failure mode omitted is event/schema evolution: old events may become unreadable after code changes. Snapshot migration, upcasters, or a retained reader version are required before relying on replay as the source of truth.
- + Privacy/security is broader than deletion: event logs may retain secrets or sensitive intermediate values in backups, exports, and replicas. Encryption/key destruction and retention boundaries can be required even when legal deletion is not the trigger.
- = **claude · anthropic · claude-haiku-4-5-20251001 · effort low:** The hybrid “append-only events + snapshot + tail log” model is a sound synthesis: snapshots optimize recovery/read paths while the append path preserves durability and recent synchronization history.
- = **claude · anthropic · claude-haiku-4-5-20251001 · effort low:** The report correctly identifies unbounded storage, replay latency, cold-start recovery, memory/GC pressure, and privacy deletion as distinct reasons to add retention controls, snapshots, or compaction. The exact remedy must preserve the merge/audit contract.

## Cross-report Contradictions

Only one other report was supplied, so no inter-participant contradiction can be established. Within the reviewed report, the specific thresholds are presented as actionable guidance while also being acknowledged as hardware- and workload-dependent; that is an unresolved tension.

## Highest-value Verification Targets

1. Benchmark replay and snapshot recovery using representative event sizes and worst-case offline tails; set thresholds from user-visible startup/query/error budgets rather than event counts.
2. Fault-inject crash points during snapshot publication and truncation; verify restart yields either the old snapshot plus log or the new snapshot plus tail, never a gap.
3. Test compaction with an offline peer and concurrent writers, including vector-clock/causal metadata, to verify no required history is discarded.
4. Exercise event schema migration across at least one old producer and current reader, and inspect whether sensitive values persist in snapshots, archives, and replicas.

## Reports Reviewed

- TASK-KK4DA.2 — Claude · Anthropic · claude-haiku-4-5-20251001 · effort low
