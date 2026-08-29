# Participant

Review — claude · anthropic · claude-haiku-4-5-20251001 · effort low

---

# Delta Report

## Confirmations

= **Append-only log is the source of truth for lifecycle causality.**  
Evidence: `archive/21_05_26_init/spec.html` explicitly describes Program as a "structured event log" and "append-only" for core events; the grilling-session document (section 3.5, 5.5) emphasizes that "lifecycle state becomes mutable summary, трудно audit-ить" without an append-only log. The codex report's distinction between authority (events) and projection (materialized reads) aligns with the spec's design intent.

= **Mutable state is appropriate for working/materialized files when identity is stable.**  
Evidence: `spec.html` explicitly states decision/glossary files are "working files with stable IDs that get revised in place" and "*Not* append-only." This directly supports codex · gpt-5.6-luna's claim that non-audit files can use in-place mutation for simplicity.

= **Snapshots restore graph state, not the full system; boot reconciliation is a separate concern.**  
Evidence: `spec.html` section 13.8 states: "Snapshots restore the graph, not files. File edits are git's domain" and separately notes "Restart recovery is *boot reconciliation*, not snapshot restore." This confirms the codex report's distinction.

= **Crash-safe compaction requires atomic checkpoint + suffix + validation scheme.**  
Evidence: `spec.html` mentions "snapshot safety valve" as a design concern and discusses structured event log management. The general principle of atomic writes is standard practice in durable-state systems and uncontested.

---

## Challenges

? **Claim 4: "Compaction is a performance/storage response, not normal mutation."**  
Weakness: The codex report frames compaction as purely optional/reactive. However, there is no documented compaction policy in the checked repository (`spec.html` mentions snapshots heavily but does not define a compaction threshold, retention window, or operational trigger). The report correctly notes this as medium-high confidence, but the uncertainty is unresolved: is compaction *required* for local-first tools (e.g., privacy/data-minimization compliance, unbounded log growth on long-lived daemons), or truly optional? The distinction matters for correctness and operational safety.

? **Implied assumption: "Single writer or equivalent concurrency control" for mutable state.**  
Weakness: The codex report's treatment of mutable working files assumes "a single writer owns updates." In a local-first / Git-native context, this is safe *if* the daemon is the sole writer (spec confirms: "Daemon is the sole writer"), but the report does not address the edge case of an external Git commit rewriting a working file in parallel with daemon writes. Is conflict resolution (e.g., three-way merge, daemon-reload-on-conflict) implicit in "equivalent concurrency control," or is there a gap?

? **Relationship between snapshot scope and reconciliation scope.**  
Weakness: The codex report states snapshots restore "the graph" and reconciliation handles "external/runtime facts." The spec distinguishes global-scope snapshots (`$ORGASMIC_HOME/state/snapshots/`) from project-scope snapshots (`.orgasmic/snapshots/`). The report does not clarify whether reconciliation must run at both scopes or if they compose. Is a partial snapshot (project only) sufficient for recovery, or does boot reconciliation always assume full state?

---

## Additions

+ **Hash-based integrity for compaction safety.**  
The spec mentions "semantic-hash + stale propagation" alongside snapshots. The codex report correctly identifies that "compaction checkpoint without an auditable boundary (source sequence/hash...) is silent data loss," but does not elaborate on what hashing scheme (content-addressable vs. sequence-based) would prove integrity after compaction. A complete compaction protocol would specify hash commit points and how to detect incomplete compaction on restart.

+ **Git's atomic guarantees as a design load-bearing assumption.**  
The codex report leans on "append-only" as a crash-safety primitive but does not name Git's atomic directory/file operations (`rename` under POSIX) as the durable storage layer that makes append-only safe. Local-first tools using OS filesystems depend on this; the report could strengthen its crash-safety argument by explicitly requiring append operations to rely on atomic file writes or similar kernel guarantees.

+ **Explicit ownership model for concurrency.**  
Codex correctly identifies "local-first tools need explicit ownership/concurrency rules" but does not go further: the spec's model is "daemon is sole writer, UI/CLI/agent issue reads via RPC." This centralizes mutation through a serialization point. The report could add that append-only + single-writer is the simplest correct model; multi-writer append-only requires deterministic ordering or visible conflict markers, which the spec does not address yet.

---

## Cross-Report Contradictions

None detected within the reviewed report or against the cited spec/grilling material. The codex report is internally consistent and the spec supports its main claims.

---

## Highest-Value Verification Targets

1. **Compaction policy is undefined.** Benchmark startup latency and storage growth on a realistic event corpus; define explicit thresholds (e.g., "compact when log exceeds 10MB or 100K events") and verify crash recovery under interruption.

2. **Working-file conflict scenario.** Test case: daemon writes a decision file, external `git commit` modifies it in parallel. Does the system detect conflict, defer daemon write, or silently lose one change? Current spec does not cover this.

3. **Snapshot and reconciliation interaction under failure.** Run restart recovery with an incomplete snapshot (e.g., `snapshots/<id>/` exists but is missing a key project file). Does boot reconciliation correctly reconstruct state or fail with an unclear error?

4. **Multi-scope snapshot consistency.** Take a global snapshot, then a project-scope snapshot of a different project. Can restore either independently without data loss? Does restore order matter?

---

## Reports Reviewed

- Extraction — codex · openai · gpt-5.6-luna · effort low  
  Task: TASK-KK4DA.1  
  Report path: `/Users/aspirational/.orgasmic/ledgers/orgasmic/.orgasmic/tasks/TASK-KK4DA.1/dispatches/tx-20260829-orgasmic-e8e023b2-c394-4cf2-9718-147f9aee8422/report.md`

---

## Summary

The codex · gpt-5.6-luna report provides a sound framework: append-only for authority, mutable projections for reads, snapshots + reconciliation for recovery, and crash-safe compaction. Claims are well-supported by spec and grilling-session evidence. Main gaps are absence of a defined compaction policy, unclear concurrent-write handling, and incomplete verification of snapshot/reconciliation interaction under failure.
