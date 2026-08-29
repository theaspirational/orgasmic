# TASK-8XXEC.4 Cross-Review Delta Report

## Reviewer

hermes · google · gemini-3.7-flash · effort low

## Delta

### Attribution Errors

? **The Antithesis 15-minute reproducer is attributed to Phil Eaton; it was authored by Carl Sverre.** The reviewed report (hermes · openai · gpt-5.6-luna · effort low) states in Claim 4: "Phil Eaton published a reproducer on Antithesis that triggered the bug in 15 minutes." The Antithesis blog post "Breaking the WAL" (2026-08-12, antithesis.com/blog/2026/wal-reset-bug/) is authored by **Carl Sverre**, Senior Software Engineer at Antithesis. He describes using Claude to set up SQLite 3.51.2 in Antithesis and catching the bug in 15 minutes. Phil Eaton is the author of a **separate, independent reproducer** published on theconsensus.dev (2026-08-23, "Another look at SQLite's WAL-Reset bug"), which uses a 100-line C workload against the public SQLite API — no Antithesis instrumentation, no source modifications — exploiting the `munmap` timing window to trigger the race. The report conflates two distinct reproducers by two different authors using two different methods.

? **The platform attribution is also wrong.** The report says "a reproducer on Antithesis" when attributing to Phil Eaton. Phil Eaton's reproducer was published on theconsensus.dev and uses only the public SQLite API with `clang`/`gcc` and ThreadSanitizer — it is not an Antithesis workload. Carl Sverre's reproducer is the one that ran on the Antithesis platform. The "15 minutes" figure belongs to Carl Sverre's Antithesis run; Phil Eaton's reproducer achieved results "within seconds" on a local machine, not in 15 minutes on Antithesis.

= **The core claim that the WAL-reset bug is reproducible with a generic concurrent write/checkpoint workload is confirmed** by both sources. Carl Sverre explicitly states his workload "is a completely generic workload. It just runs writes and checkpoints concurrently — things you'd expect to actually happen in production." Phil Eaton's reproducer similarly uses a generic pattern: a checkpointer thread, a writer thread, and a reader thread, with no special knowledge of the bug mechanism beyond targeting the checkpoint code path. Both confirm the bug is reachable without exotic usage patterns.

### Material Additions

+ **Tailscale's production experience is a primary source the report does not directly cite.** The Tailscale blog post "How we tracked down a 16-year-old SQLite bug" (tailscale.com/blog/sqlite-wal-reset-bug) documents **19 separate database corruption incidents over 6 months** in their production control plane. This is the most significant real-world evidence of the bug's frequency. A former Tailscale employee shared they were checkpointing every 250ms across a fleet of servers, which the SQLite developer Richard Hipp characterized as "a fantastic way to surface a super rare bug." The report references Tailscale only indirectly through Michael Tsai's blog aggregation, missing the primary source that quantifies real-world impact.

+ **The 3.52.0 → 3.51.3 version history nuance is absent.** The Tailscale blog reveals the fix was first released as SQLite 3.52.0, which was then **withdrawn** because it introduced a false corruption warning (computed index values changed, causing `PRAGMA integrity_check` false positives — 13 databases were falsely flagged). The SQLite team then republished the fix as 3.51.3 containing only the WAL-reset fix. Desktop app developers upgrading should know 3.52.0 was pulled; the report's flat "fixed in 3.51.3" is correct but misses this operational landmine for anyone who grabbed 3.52.0.

+ **Phil Eaton's reproducer also demonstrates that ThreadSanitizer catches the bug.** The theconsensus.dev article shows that building the buggy SQLite amalgamation with `-fsanitize=thread` surfaces a data race on `pInfo->nBackfill` between `walCheckpoint` and `walRestartHdr` — this is a cheap, accessible verification method that doesn't require Antithesis or source instrumentation. The report's verification targets list only "run the Antithesis reproducer" for the WAL-reset bug reachability, missing this simpler local option.

### Claim-Level Confirmations

= **Claim 1 (network filesystem incompatibility): Confirmed.** SQLite documentation section 2.2 states: "the use of shared memory means that all readers must exist on the same machine. This is why the write-ahead log implementation will not work on a network filesystem." Disadvantage #1 restates this. The report's evidence (Sonarr, oh-my-pi issues) is supplementary; the primary documentation is dispositive.

= **Claim 2 (checkpoint starvation): Confirmed.** SQLite documentation section 6 states: "if a database has many concurrent overlapping readers and there is always at least one active reader, then no checkpoints will be able to complete and hence the WAL file will grow without bound." The report's characterization is accurate.

= **Claim 3 (SQLITE_BUSY exceptions): Confirmed.** SQLite documentation section 9 enumerates the three cases exactly as the report describes: exclusive locking mode (Chrome/Firefox), last-connection cleanup, and crash recovery. The report's enumeration is faithful to the source.

= **Claim 5 (unclean shutdown data loss): Confirmed.** SQLite documentation section 4 states: "If a database file is separated from its WAL file, then transactions that were previously committed to the database might be lost, or the database file might become corrupted. The only safe way to remove a WAL file is to open the database file... then immediately close." The report's evidence chain is sound.

= **Claim 6 (read-only media): Confirmed.** SQLite documentation section 5 and disadvantage #4 confirm the 3.22.0 relaxation and the three conditions for read-only WAL access. The report accurately represents this.

= **Claim 9 (page size lock): Confirmed.** Disadvantage #3 states: "It is not possible to change the page_size after entering WAL mode, either on an empty database or by using VACUUM or by restoring from a backup using the backup API." Directly matches the report.

= **Claim 10 (cross-ATTACH atomicity): Confirmed.** Disadvantage #2 states: "Transactions that involve changes against multiple ATTACHed databases are atomic for each individual database, but are not atomic across all databases as a set." Exact match.

### Challenged Claims

? **Claim 7 (large transactions) understates the fix.** The report says "This was partially improved in 3.11.0 (2016-02-15), but the guidance remains." The SQLite documentation shows disadvantage #8 is **struck through** (~~) with the replacement text: "Beginning with version 3.11.0 (2016-02-15), WAL mode works as efficiently with large transactions as does rollback mode." The strike-through indicates the original guidance is **withdrawn**, not "partially improved." The report's "the guidance remains" contradicts the documentation's explicit retraction. For SQLite ≥ 3.11.0 (which is virtually all current deployments), the large-transaction disadvantage appears to no longer apply per the primary source.

? **Claim 4's characterization of the WAL-reset bug as "reachable with normal workloads, not just exotic ones" needs qualification.** Both reproducers (Carl Sverre's and Phil Eaton's) were **directed searches** — the authors knew the bug existed and specifically exercised the write/checkpoint concurrency path. Carl Sverre acknowledges on HN that the agent "was aware of the bug." Phil Eaton's article is titled "Another look" — it was a follow-on, not a blind discovery. The Tailscale case is the only known organic production occurrence, and it involved aggressive 250ms checkpointing at fleet scale — arguably non-standard operation that Tailscale themselves describe as "stepping off the well-trodden operational path." The characterization "reachable with normal workloads" is true in the narrow sense (the workload is generic), but both reproducers were targeted, and the sole organic production case involved atypical checkpoint frequency.

## Cross-Report Contradictions

No contradictions between multiple reports — only one report was provided for review. All contradictions are between the report and primary sources:

1. **Antithesis reproducer attribution** — Report: "Phil Eaton published a reproducer on Antithesis." Primary sources: Carl Sverre (Antithesis) authored the 15-minute Antithesis run; Phil Eaton (theconsensus.dev) authored a separate reproducer using the public API. These are two different people, two different platforms, two different methods.

2. **Large transaction guidance status** — Report: "partially improved... the guidance remains." SQLite documentation: the original guidance is struck through and replaced with "WAL mode works as efficiently with large transactions as does rollback mode" as of 3.11.0.

## Highest-Value Verification Targets

1. **Re-verify the Antithesis reproducer authorship.** Read antithesis.com/blog/2026/wal-reset-bug/ (Carl Sverre, 2026-08-12) and theconsensus.dev/p/2026/08/23/another-look-at-sqlite-wal-reset.html (Phil Eaton, 2026-08-23). Confirm they are distinct reproducers by distinct authors. This is the report's most prominent "unique finding" and the attribution is incorrect.

2. **Re-verify the large-transaction disadvantage status.** Read sqlite.org/wal.html disadvantage #8. Confirm the original text is struck through and the 3.11.0 replacement text applies. Determine whether "the guidance remains" is accurate for current SQLite versions.

3. **Reproduce the WAL-reset bug locally with Phil Eaton's workload.** The theconsensus.dev article provides a ~100-line C program using only the public SQLite API and SQLite 3.51.2 amalgamation. This is the cheapest available verification and does not require Antithesis. ThreadSanitizer also catches the race.

4. **Read the Tailscale blog post directly.** It is the primary source for real-world WAL-reset bug frequency (19 incidents, 6 months, 250ms checkpoint cadence) and the 3.52.0 withdrawal history.

## Reports Reviewed

- TASK-8XXEC.1 — hermes · openai · gpt-5.6-luna · effort low
  Report: /Users/aspirational/.orgasmic/ledgers/orgasmic/.orgasmic/tasks/TASK-8XXEC.1/dispatches/tx-20260829-orgasmic-323c3a9a-4eed-4227-80b3-bf510f9fe2a6/report.md
