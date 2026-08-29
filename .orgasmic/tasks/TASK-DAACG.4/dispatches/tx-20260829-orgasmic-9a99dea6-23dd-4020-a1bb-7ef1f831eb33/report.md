# Reviewer

`claude · anthropic · claude-haiku-4-5-20251001 · effort low` (implementer-claude-sdk-stdio)

# Delta Summary

## = Independently Confirmed

- **Authority vs. projection distinction** (codex · openai · gpt-5.6-luna): the architecture correctly reserves append-only for transition facts and permits mutable state for derived read models and caches. Verified by: `shipped/schema/journal.org` explicitly labels ledger facts immutable and forbids generic rewrites; `crates/orgasmic-core/src/session.rs` describes folded UI state as a projection; distinct storage layers confirm the separation is structural, not aspirational.

- **Compaction requires plan/confirm atomicity** (codex · openai · gpt-5.6-luna): `crates/orgasmic-daemon/tests/run_inventory_wire.rs` does enforce confirmation token binding to planned reclaim, rejects stale tokens, and verifies byte-identical preservation of structured records. The pattern is real and tested.

- **Comments are a schema-level exception** (codex · openai · gpt-5.6-luna): `shipped/schema/journal.org` does record comment edits via optimistic-concurrency and represent deletion as tombstones. This is an explicit exception to the append-only rule, not a workaround.

## ? Weakly Supported or Needs Verification

- **"Mutable state appropriate for projections"** — architectural inference, not empirically proven. Codex correctly identifies that projections are replaceable, but the report does not confirm a case where a projection was actually deleted/rebuilt from authoritative records in production or testing. Question: does the daemon have a recovery path that rebuilds UI state from events if the projection files corrupt or disappear? (High value if verified.)

- **"Materially slow" startup ceiling is workload-dependent** — acknowledged by codex but left undefined. No threshold, profile, or guidance on when to measure. Practical question: at what event count or replay duration should an operator reach for a snapshot? (E.g., 10k events / 100ms / startup SLA.) Without a heuristic, the rule is too vague for operational decision-making.

- **Snapshot idempotence at the boundary** — codex lists this as a verification target but does not examine the actual code path. Specific risk: if snapshot creation stalls or crashes mid-write, does the system recover to old-state or detect a partial snapshot and discard it? The test suite may not exercise asymmetric failure modes (writer stalls while snapshot publishes).

- **Tail consistency after snapshot** — codex mentions keeping "a log tail after the snapshot" but does not specify the persistence contract: is the tail durably flushed before the snapshot boundary is marked? Can concurrent writes apply-on-tail while the snapshot is being consumed? The run_inventory_wire test checks byte-identity of *prior* events, not concurrent-write safety.

## + Material Additions Missing from Reviewed Report

- **Retention policy and sensitive payload handling** — codex correctly notes "size alone is a warning," but the orgasmic ledger does not document retention windows (e.g., "keep 30 days of audit history," "delete payment tokens after settlement") or a migration path when schema changes and old events become unreadable. The journal.org linting catches size, not expiry or privacy policy.

- **Bounded recovery guarantees** — no explicit statement of "after N hours of uptime, startup must take ≤ X seconds, with at most Y MB in memory." Snapshots only solve recovery latency *if* the SLA is stated first; otherwise it's optimization without a target.

- **Compaction failure modes and rollback** — codex mentions interruption/atomicity as a verification target but does not name the rollback story: if compaction begins, succeeds partially, then the daemon crashes, does the next startup see old-state or new-state? Is there a WAL or explicit checkpoint? How does an operator recover if the compaction plan was stale and the confirm token can't be re-issued?

- **Event log storage cost vs. durability trade-off** — for append-only logs, disk I/O and storage cost grow linearly. The report does not weigh: "keep all events forever" vs. "snapshot + rotate to archive" vs. "snapshot + truncate." This trade-off drives snapshot/compaction strategy in real systems.

## Cross-Report Contradictions

None identified. The codex report is internally consistent and does not contradict itself.

## Highest-Value Verification Targets

1. **Projection rebuild test** — delete UI state files or corrupt projection DB; verify that replay from event log recovers the exact same state, byte-for-byte. If this test does not exist, it's a gap; if it fails, the projection is not truly disposable.

2. **Snapshot crash injection** — interrupt compaction or snapshot publication at each write boundary (fsync, rename, file close). Verify that the next startup sees either the old valid state or the new valid state, never a mixed/partial state.

3. **Recovery SLA measurement** — run a daemon for N days, measure cold-start replay time and peak memory; compare with and without a snapshot. Establish the workload where snapshot overhead (<checkpoint latency) exceeds replay cost.

4. **Stale confirmation handling** — verify that replaying the same compaction plan with an out-of-date confirmation token is rejected, and that log growth since confirmation is not silently discarded.

5. **Tail consistency and concurrent writes** — if writers can emit events on-tail while a snapshot is being created, verify that no event is lost or double-applied during the snapshot boundary crossing.

## Reports Reviewed

- TASK-DAACG.1: `codex · openai · gpt-5.6-luna · effort low`
