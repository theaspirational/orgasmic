# TASK-8XXEC.1 Extraction Report

## Participant

hermes · openai · gpt-5.6-luna · effort low

## Direct Answer

WAL mode is the right default for a single-writer, multi-reader desktop app on a local filesystem — it is exactly the workload WAL was designed for. The real failure modes are not about the read/write concurrency model (which works as advertised) but about **the assumptions WAL silently embeds**: that all processes share a local filesystem with working POSIX locks and mmap-able shared memory, that no read transaction stays open long enough to starve checkpoints, that the app shuts down cleanly enough for the `-wal`/`-shm` files to be reconciled, and that the SQLite version in your dependency tree is recent enough to not carry the WAL-reset corruption race. For a canonical desktop app (one process writing, several reading, all on one machine, local disk) these assumptions hold and WAL is correct. You need something else when any of them break: network/remote filesystems, multi-machine access, sustained write throughput that needs concurrent writers, very large transactions, read-only distribution media, cloud-synced directories that replicate half a database state, or an embedded SQLite old enough to carry known corruption bugs.

## Claims and Evidence

### Claim 1: WAL does not work over network filesystems (NFS, SMB/CIFS, Docker volume mounts on some platforms) — silent corruption risk.

**Reasoning/evidence:** The wal-index (the `-shm` file) is an mmap'd shared-memory region that all processes accessing the database must see. Processes on separate hosts (or on hosts where the filesystem does not provide coherent shared memory) cannot share this memory, so WAL's reader/writer concurrency breaks. The SQLite documentation states this explicitly as disadvantage #1: "All processes using a database must be on the same host computer; WAL does not work over a network filesystem." Real-world reports confirm it is not just a theoretical limitation — Sonarr issue #1886 documents WAL locking/corruption on SMB/CIFS and Docker-for-Windows host-shared paths; the oh-my-pi issue #9082 documents silent `SQLITE_CORRUPT` on NFS-mounted home directories; the SQLite mailing list confirms "WAL mode of sqlite is not supported over network file systems."

**Confidence:** High — primary documentation + multiple independent field reports.

**Cheapest verification:** Open a WAL-mode database over an NFS mount from two processes; observe either `SQLITE_BUSY` storms or corruption. Or: check the return value of `PRAGMA journal_mode=WAL` on such a mount — it may silently fall back to `delete`.

---

### Claim 2: Checkpoint starvation — a long-running read transaction prevents the WAL from being checkpointed, causing unbounded WAL growth and progressively slower reads.

**Reasoning/evidence:** The SQLite documentation (section "Avoiding Excessively Large WAL Files") states: "if a database has many concurrent overlapping readers and there is always at least one active reader, then no checkpoints will be able to complete and hence the WAL file will grow without bound." A checkpoint can only reset the WAL when no reader is using it, because resetting would overwrite pages an active reader might need. The Syncthing forum thread documents a real case: WAL grew to 15 GB (database 1.5 GB) during index exchange because continuous read transactions prevented checkpoint completion. Richard Hipp confirmed in the sqlite_users mailing list that this is expected behavior. Read performance also degrades as the WAL grows, because each reader must check the WAL for needed pages.

**Confidence:** High — primary documentation + field-confirmed.

**Cheapest verification:** Open a long read transaction, keep writing; monitor `-wal` file size. It will grow past the default ~4MB checkpoint threshold and keep growing.

---

### Claim 3: `SQLITE_BUSY` can still occur in WAL mode in several non-obvious situations — the "readers never block writers" promise has exceptions.

**Reasoning/evidence:** The SQLite documentation (section 9, "Sometimes Queries Return SQLITE_BUSY In WAL Mode") enumerates three cases: (a) another connection holds the database in exclusive locking mode (Chrome/Firefox do this — you cannot read their databases while they run); (b) during the last-connection-cleanup window, when the closing connection briefly holds an exclusive lock to clean up `-wal`/`-shm`; (c) during crash recovery — the first new connection after a crash holds an exclusive lock for recovery, so a concurrent third connection gets `SQLITE_BUSY`. Additionally, writers still serialize against each other: only one write lock exists, so concurrent write attempts produce `SQLITE_BUSY` unless `busy_timeout` is set. The Bun issue #25964 documents a Windows-specific case where file locks persist after `close()` in WAL mode.

**Confidence:** High — primary documentation.

**Cheapest verification:** Open two connections, set one to `PRAGMA locking_mode=EXCLUSIVE`, query from the other — it returns `SQLITE_BUSY`.

---

### Claim 4: The WAL-reset bug (versions 3.7.0–3.51.2) can silently corrupt the database when two connections write or checkpoint concurrently — rare but real, and a reproducer now exists without test-instrumentation hacks.

**Reasoning/evidence:** The SQLite documentation (section 11) describes the bug: a data race between a completing checkpoint, a second starting checkpoint, and a concurrent write that resets the WAL can leave the wal-index header in a state where a later checkpoint skips part of a committed transaction, corrupting the database. The bug existed from 2010-07-21 through 2026-01-09, fixed in 3.51.3 (2026-03-13); backports exist for 3.44.6 and 3.50.7. SQLite's own telemetry says the occurrence rate is "less than or equal to the expected occurrence rate of SSD malfunctions and/or cosmic-ray hits." However, as of 2026-08-24, Phil Eaton published a reproducer on Antithesis that triggered the bug in 15 minutes with a generic concurrent write/checkpoint workload (reported by Michael Tsai's blog). This means the "extremely rare" characterization is partially outdated — the bug is reachable with normal workloads given enough runtime, not just exotic usage patterns.

**Confidence:** High for the bug's existence and fix; Medium for real-world frequency (the official line says ultra-rare; the Antithesis reproducer says reachable).

**Cheapest verification:** Check `sqlite3_libversion()` against 3.51.3; if below 3.44.6 or 3.50.7 (the backport lines) and you run concurrent writers, you are exposed.

---

### Claim 5: Unclean shutdown can lose committed transactions that exist only in the WAL file — data loss, not corruption.

**Reasoning/evidence:** A SQLite forum post documents a real Android case: if an app crashes without closing its connection (and without checkpointing), transactions committed to the WAL file can be lost on reopen. The forum user showed that performing a checkpoint before exit, or closing the connection before exit, preserves all data; skipping both loses random subsets of committed transactions. The SQLite documentation corroborates: the WAL file is part of the persistent database state and should be kept with the database if copied/moved; the only safe way to remove a WAL file is to open and immediately close the database. If an OS or crash removes or orphans the `-wal` file, committed-but-uncheckpointed transactions are gone. With `synchronous=NORMAL` (the recommended WAL pairing), transactions committed since the last checkpoint are not fsync'd to disk and can be lost on power failure — this is the explicit durability tradeoff.

**Confidence:** High — primary documentation + field report.

**Cheapest verification:** Open a WAL database, insert rows, kill the process without `sqlite3_close()` or checkpoint, delete the `-wal` file, reopen — the rows are gone.

---

### Claim 6: WAL mode is incompatible with read-only media and read-only file permissions in older SQLite; even in newer versions, it requires either pre-existing `-shm`/`-wal` files or write access to the containing directory.

**Reasoning/evidence:** Disadvantage #4 in the SQLite documentation: "It is not possible to open read-only WAL databases" without meeting specific conditions. Since SQLite 3.22.0 (2018-01-22), a read-only WAL database can be opened if the `-shm` and `-wal` files already exist, or if there is write permission on the directory (so they can be created), or if the `immutable` URI query parameter is used. For a desktop app distributing a database on read-only media (CD-ROM, sealed installer), this is a real constraint — you must convert to `journal_mode=DELETE` before burning.

**Confidence:** High — primary documentation.

**Cheapest verification:** `chmod -w` the directory containing a WAL-mode database, try to open it read-only with SQLite < 3.22.0 — it fails.

---

### Claim 7: Very large transactions are a known anti-pattern in WAL mode — performance cliff and potential I/O/disk-full failure.

**Reasoning/evidence:** Disadvantage #8: "WAL works best with smaller transactions. WAL does not work well for very large transactions. For transactions larger than about 100 megabytes, traditional rollback journal modes will likely be faster. For transactions in excess of a gigabyte, WAL mode may fail with an I/O or disk-full error." This was partially improved in 3.11.0 (2016-02-15), but the guidance remains. For a desktop app doing bulk imports or large schema migrations, this matters.

**Confidence:** High — primary documentation.

**Cheapest verification:** Begin a transaction, insert >100MB of data, measure throughput vs. the same in `journal_mode=DELETE`.

---

### Claim 8: WAL mode adds two extra files (`-wal`, `-shm`) that complicate backup, copy, and application-file-format use — incomplete copies corrupt the database.

**Reasoning/evidence:** Disadvantage #6: there is "an additional quasi-persistent `-wal` file and `-shm` shared memory file." The `howtocorrupt.html` guide says: "it is important that any rollback journal or write-ahead log be copied together with the database file itself." For a desktop app whose database lives in a cloud-synced folder (iCloud Drive, Dropbox, Google Drive), the sync engine may copy the `.db` file without the `-wal`/`-shm` files (or copy them at a different instant), leaving the remote copy inconsistent or corrupt. The macOS App Sandbox can also block the temporary journal files SQLite needs (Stack Overflow report).

**Confidence:** High — primary documentation + logical inference about sync engines.

**Cheapest verification:** Put a WAL-mode database in a Dropbox folder, write to it while sync runs, check the synced copy on another machine — it may be missing the `-wal` file or have a stale version.

---

### Claim 9: Page size is locked once you enter WAL mode — `VACUUM` cannot change it, and you must exit WAL to restructure.

**Reasoning/evidence:** Disadvantage #3: "It is not possible to change the page_size after entering WAL mode, either on an empty database or by using VACUUM or by restoring from a backup using the backup API. You must be in a rollback journal mode to change the page size." This is a minor but real operational constraint — an app that wants to tune page size for a different workload must round-trip through `journal_mode=DELETE`.

**Confidence:** High — primary documentation.

**Cheapest verification:** `PRAGMA page_size=4096; PRAGMA journal_mode=WAL; PRAGMA page_size=8192;` — the page size does not change.

---

### Claim 10: Cross-database atomicity is weakened — transactions spanning multiple ATTACHed databases are atomic per-database but not as a set.

**Reasoning/evidence:** Disadvantage #2: "Transactions that involve changes against multiple ATTACHed databases are atomic for each individual database, but are not atomic across all databases as a set." For a desktop app that uses ATTACH for multi-file schemas, this is a correctness concern if cross-file atomicity is required.

**Confidence:** High — primary documentation.

**Cheapest verification:** Attach two WAL databases, write to both in one transaction, crash mid-commit — one may commit and the other may not.

---

## Unique or Easily Missed Findings

1. **The WAL-reset bug is reachable with normal workloads, not just exotic ones.** The official documentation says the bug was never reproduced organically and required test-instrumentation hacks. But as of 2026-08-24, Phil Eaton's Antithesis reproducer (reported by Michael Tsai) triggered it in 15 minutes with a generic concurrent write/checkpoint workload. The "cosmic-ray rate" framing may understate the risk for long-running multi-connection desktop apps on unpatched SQLite. If your dependency tree pins SQLite below 3.51.3 (or the backport lines 3.44.6 / 3.50.7), you should treat this as a real exposure, not a theoretical one.

2. **Checkpoint starvation is per-connection, not per-transaction.** A SQLite forum post (Van Schelven, 2024-07-04) discovered that the WAL file did not shrink when a transaction committed — it only shrank when the *connection* that held the read transaction was *closed*. This means connection pooling with held read connections can starve checkpoints even if no individual transaction is long-lived. For a desktop app that keeps a persistent read connection open (common for UI refresh loops), this is a subtle trap.

3. **`-wal` file size does not shrink after a checkpoint by default.** The SQLite documentation (section 6) and Richard Hipp's mailing list reply confirm: checkpoints overwrite the WAL from the beginning rather than truncating it, because overwriting is faster. The WAL file retains its high-water size on disk unless `journal_size_limit` is set or a `TRUNCATE` checkpoint is explicitly run. A desktop app concerned about disk footprint should set `PRAGMA journal_size_limit` or run `PRAGMA wal_checkpoint(TRUNCATE)` on shutdown.

4. **The `-shm` file is an ordinary mmap'd disk file in the database directory, not `/dev/shm` or `/tmp`.** The SQLite documentation explains this was a deliberate design choice for robustness against `chroot` and portability, but it means the file is visible, can be touched by backup/sync tools, and is subject to directory-level permissions. On macOS specifically, the App Sandbox can block creation of these journal files, breaking writes entirely (Stack Overflow report).

5. **`synchronous=NORMAL` in WAL mode sacrifices post-checkpoint transaction durability on power loss — this is by design, not a bug.** The tradeoff is explicit in the documentation: with `synchronous=NORMAL`, the WAL is not fsync'd on every commit, only on checkpoint. A power failure between commit and checkpoint loses those transactions. The database does not corrupt (WAL is more forgiving of out-of-order writes than rollback journals), but data is lost. Desktop apps that need crash durability must use `synchronous=FULL`, paying one fsync per write transaction — still cheaper than rollback journal's two fsyncs.

6. **A single bit flip in the WAL file can silently lose committed entries.** The BreakingSQLite project (danthegoodman1) demonstrates that flipping a single bit in the `-wal` file causes SQLite to truncate the WAL at the corrupted frame, discarding all subsequently committed entries — including valid ones — without error. SQLite explicitly assumes "the data it reads is exactly the same data that it previously wrote" and does not add redundancy for error detection. For a desktop app on consumer-grade storage without ECC, this is a low-probability but silent data-loss path.

7. **WAL mode slightly degrades read-heavy, write-rare workloads (~1-2%).** Disadvantage #5 notes WAL "might be very slightly slower (perhaps 1% or 2% slower) than the traditional rollback-journal approach in applications that do mostly reads and seldom write." For a desktop app that is almost entirely read-only with occasional writes, this overhead is usually irrelevant, but it means WAL is not a universal performance win.

8. **`PRAGMA journal_mode=WAL` is persistent — it is stored in the database file header.** Unlike other journal modes, WAL survives close/reopen. This means if you convert a database to WAL and ship it, every downstream connection will use WAL whether the app sets the pragma or not. This is a feature (zero-config adoption) but also a footgun: if the downstream environment is a network filesystem or read-only media, the database will be in a mode that doesn't work there.

## Uncertainties and Contradictions Within This Report

1. **WAL-reset bug frequency: official vs. reproducer evidence.** The SQLite documentation says the occurrence rate is "less than or equal to the expected occurrence rate of SSD malfunctions and/or cosmic-ray hits," based on telemetry. The Antithesis reproducer (Phil Eaton, 2026-08-24) triggered the bug in 15 minutes with a generic workload, which suggests it is more reachable than the telemetry implies. I cannot reconcile these fully — telemetry reflects observed-in-the-wild occurrences (which require the rare timing window to coincide with a real workload), while the Antithesis run is a directed search over the state space. The truth is probably: the bug is reachable with sustained concurrent write/checkpoint activity over long runtimes, but rare enough that most short-lived desktop sessions never hit it. Verification target: run the Antithesis reproducer yourself or review its report.

2. **Checkpoint starvation: transaction-lifetime vs. connection-lifetime.** The official documentation frames checkpoint starvation in terms of long-running read *transactions*. The Van Schelven forum post frames it in terms of held *connections* (the WAL did not shrink until the connection closed, not just when the transaction ended). These may be describing the same mechanism at different granularities, or there may be a subtlety where a connection holding a read snapshot (even between transactions) prevents WAL reset. I could not fully resolve this from the available sources — the documentation says "read transaction" but the field report says "connection." Verification target: reproduce with a connection that commits a read transaction but stays open, and observe whether the WAL shrinks.

3. **`synchronous=NORMAL` durability scope.** The documentation says `synchronous=NORMAL` omits the WAL fsync on commit but still fsyncs on checkpoint. The Stack Overflow discussion has slightly conflicting characterizations of exactly what is at risk (transactions since last checkpoint vs. transactions since last fsync'd WAL frame). The core claim — "you can lose committed transactions on power failure with `synchronous=NORMAL`" — is well-supported, but the exact boundary is less crisp than ideal. Verification target: test with `synchronous=NORMAL`, commit, pull power, reopen — measure which transactions survive.

4. **macOS App Sandbox + WAL interaction.** The Stack Overflow report indicates the sandbox blocks journal file creation. I did not find primary documentation from Apple or SQLite on exactly which sandbox entitlements resolve this. This is a real concern for sandboxed macOS desktop apps but I cannot fully specify the fix from the sources gathered. Verification target: test in a sandboxed macOS app with WAL mode and `com.apple.security.files.user-selected.read-write` vs. a container-internal path.

## Verification Targets

| Claim | Verification |
|-------|-------------|
| Network filesystem incompatibility | Open a WAL database over NFS/SMB from two processes; observe `SQLITE_BUSY` or corruption. Or check `PRAGMA journal_mode=WAL` return value on a network mount. |
| Checkpoint starvation | Open a long read transaction, keep writing; monitor `-wal` file size with `ls -lh db-wal`. |
| `SQLITE_BUSY` in WAL mode | Set `PRAGMA locking_mode=EXCLUSIVE` on one connection, query from another. |
| WAL-reset bug version exposure | Run `sqlite3_libversion()` / `sqlite3 --version`; compare against 3.51.3, 3.44.6, 3.50.7. |
| Unclean shutdown data loss | Insert rows, `kill -9` the process, delete `-wal`, reopen — rows missing. |
| Read-only media incompatibility | `chmod -w` the directory, open read-only — fails pre-3.22.0 or without pre-existing `-shm`. |
| Large transaction cliff | Insert >100MB in one transaction in WAL vs. DELETE mode; compare throughput. |
| Cloud sync corruption | Put database in iCloud/Dropbox, write while syncing, inspect remote copy. |
| Page size lock | `PRAGMA journal_mode=WAL; PRAGMA page_size=8192;` — no change. |
| Cross-ATTACH atomicity | Attach two WAL DBs, write to both, crash — check consistency. |
| WAL-reset bug reachability | Run Phil Eaton's Antithesis reproducer (theconsensus.dev link). |
| Checkpoint starvation per-connection | Open connection, commit read txn, keep connection open, write from another; check if WAL shrinks. |
| macOS sandbox + WAL | Test WAL writes in a sandboxed app container vs. user-selected path. |

## Sources Consulted

1. **SQLite official: Write-Ahead Logging** — https://sqlite.org/wal.html (primary; accessed 2026-08-29; last updated 2026-08-25). Covers advantages/disadvantages, concurrency, checkpointing, WAL file, read-only databases, large WAL files, shared memory, SQLITE_BUSY cases, backwards compatibility, WAL-reset bug (section 11). Full text cached at `/Users/aspirational/.hermes/cache/web/sqlite.org-05cae033ce.md`.

2. **SQLite official: How To Corrupt An SQLite Database File** — https://www.sqlite.org/howtocorrupt.html (search result excerpt; not fully extracted). Covers lock-during-close defenses (3.51.0+), WAL mode sync failure behavior, QNX mmap corruption, WAL-reset bug cross-reference, I/O error during shared-memory lock.

3. **SQLite forum: Checkpoint Starvation: connections vs. transactions** — https://sqlite.org/forum/info/7da967e0141c7a1466755f8659f7cb5e38ddbdb9aec8c78df5cb0fea22f75cf6 (Van Schelven, 2024-07-04; Richard Hipp reply). Documents that WAL did not shrink until connection closed, not just transaction ended.

4. **SQLite forum: Why data is lost in SQLite database with WAL mode on when connection is not closed properly?** — https://sqlite.org/forum/forumpost/974675b288e1fc93?raw= (field report). Documents real data loss on unclean shutdown without checkpoint/connection-close.

5. **SQLite users mailing list: WAL file growth concern** — https://groups.google.com/g/sqlite_users/c/Ztudwnl_4f4 (Bob Smith + Richard Hipp). Confirms WAL growth dynamics under concurrent read/write stress; Richard Hipp explains the non-truncating checkpoint behavior.

6. **sqlite_users mailing list: WAL mode and Network filesystems** — https://groups.google.com/g/sqlite_users/c/Gh5gnzqrJJA. Confirms WAL does not work over network filesystems.

7. **Michael Tsai blog: SQLite WAL-Reset Bug** — https://mjtsai.com/blog/2026/08/14/sqlite-wal-reset-bug/ (2026-08). Reports Phil Eaton's Antithesis reproducer that triggered the bug in 15 minutes with a generic concurrent write/checkpoint workload, challenging the "never reproduced organically" framing.

8. **Phil Eaton: Another look at SQLite's WAL-Reset bug** — https://theconsensus.dev/p/2026/08/23/another-look-at-sqlite-wal-reset.html (referenced from sqlite.org; not directly extracted). The reproducer without test-instrumentation hacks.

9. **GitHub: Sonarr #1886 — SQLite on Network Share** — https://github.com/Sonarr/Sonarr/issues/1886. Field report of WAL corruption on SMB/CIFS and Docker host-shared paths.

10. **GitHub: oh-my-pi #9082 — SQLite WAL on shared NFS silently corrupts databases** — https://github.com/can1357/oh-my-pi/issues/9082. Documents silent `SQLITE_CORRUPT` on NFS; references similar reports from OpenAI Codex and OpenCode projects.

11. **GitHub: BreakingSQLite** — https://github.com/danthegoodman1/BreakingSQLite. Demonstrates silent committed-write loss from a single bit flip in the WAL file.

12. **GitHub: Bun #25964 — SQLite database file locked on Windows after close() with WAL mode** — https://github.com/oven-sh/bun/issues/25964. Windows-specific file lock persistence after close in WAL mode.

13. **Stack Overflow: How safe is SQLite WAL on power failures?** — https://stackoverflow.com/questions/3584530/how-safe-is-sqlite-wal-on-power-failures. Clarifies the `synchronous=NORMAL` durability tradeoff: no corruption, but possible transaction loss on power failure.

14. **Stack Overflow: SQLite not working on macOS using SwiftUI with the App Sandbox** — https://stackoverflow.com/questions/78908421/sqlite-not-working-on-macos-using-swiftui-with-the-app-sandbox. Reports App Sandbox blocking journal file creation.

15. **Coddy: WAL Mode and Concurrency** — https://coddy.tech/docs/sqlite/wal-mode-and-concurrency (Kevin Spektor). Practical overview of WAL pragmas, checkpoint modes, and recommended production setup.

16. **SQLite source: wal-lock.md** — https://github.com/sqlite/sqlite/blob/master/doc/wal-lock.md. Documents the WAL lock hierarchy (WRITER, CHECKPOINTER, read-mark slots) and the blocking-lock configurations that determine when `SQLITE_BUSY` is returned vs. blocked.

17. **Litestream: WAL Truncate Threshold Configuration** — https://litestream.io/guides/wal-truncate-threshold/. Operational guidance for monitoring and managing WAL growth in long-read apps.

18. **Syncthing forum** — https://forum.syncthing.net/t/syncthing-on-sqlite-help-test/23981?page=8. Real-world WAL growth to 15 GB under concurrent read/write load; developer analysis of checkpoint starvation mechanics.

19. **SQLite forum: Multiple Writers** — https://sqlite.org/forum/info/b4e8b29ae409cd198652c6b7e70b53b70f269e67e1d2573d627feeba37bbf85. Confirms SQLite is fundamentally single-writer; file-level locks restrict to one writer per database file. References `BEGIN CONCURRENT` branch and LMDB as alternatives.
