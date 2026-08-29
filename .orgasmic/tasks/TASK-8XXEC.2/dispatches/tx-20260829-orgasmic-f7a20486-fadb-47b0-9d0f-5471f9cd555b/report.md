# Extraction Report: SQLite WAL Mode Failure Modes for Single-Writer Multi-Reader Desktop Apps

## Participant

hermes · google · gemini-3.7-flash · effort low

## Direct Answer

SQLite WAL mode is a strong default for a single-writer, multi-reader desktop app — it is the configuration SQLite's own documentation recommends for exactly that pattern. But "relying on it" without understanding its failure modes leads to four classes of real production problems:

1. **Checkpoint starvation and unbounded WAL growth** — a long-running read transaction (or a steady stream of overlapping short readers) can prevent the checkpoint from completing, so the `-wal` file grows without limit, degrading read performance and consuming disk space.
2. **Surprising SQLITE_BUSY on readers** — readers can get `SQLITE_BUSY` during connection open/close, recovery, and checkpoint windows even when no application write is in progress. This is especially sharp for short-lived, bursty read-only connections with no busy timeout.
3. **Filesystem and environment incompatibilities** — WAL requires shared memory (`-shm`) and POSIX byte-range locks; network filesystems (NFS, SMB/CIFS, some FUSE), constrained sandboxes, and some container volume mounts break it silently or noisily.
4. **Operational edge cases around lifecycle, large transactions, and page-size changes** — the last-connection cleanup path, crash recovery, very large write transactions, and the inability to change page size while in WAL mode all produce specific failure modes.

You need something else (a client-server database, a different journal mode, or an explicit checkpoint/lock strategy) when: you need true multi-writer concurrency; your database lives on a network filesystem; you need to change the page size without a full dump/reload; your workload involves very large transactions; or you cannot tolerate any reader seeing `SQLITE_BUSY` under bursty connection patterns and cannot set a busy timeout.

## Claims and Evidence

### Claim 1: WAL allows one writer and many concurrent readers, but writes are still serialized.

**Confidence:** Very high (checked against primary source).

SQLite's official WAL documentation states: "Writers merely append new content to the end of the WAL file. Because writers do nothing that would interfere with the actions of readers, writers and readers can run at the same time. However, since there is only one WAL file, there can only be one writer at a time." (sqlite.org/wal.html, §2.2 Concurrency)

The Xojo forum post corroborates the practical consequence: a second thread that tries to write while the first is still writing is blocked for the duration of the busy timeout, and during that window the read threads can also be blocked depending on the threading model. (forum.xojo.com/t/sqlite-with-wal-multiuser-mode-behavior-that-disappoints-me/29211)

**Verification:** Open a WAL-mode database with two connections; start a write transaction on connection A; attempt a write on connection B — it will block or return `SQLITE_BUSY` after the busy timeout expires. The cheapest probe is a 10-line Python script with `sqlite3.connect` and a `busy_timeout` of 0.

### Claim 2: Long-running read transactions can starve checkpoints, causing unbounded WAL growth.

**Confidence:** Very high (checked against primary source and multiple real-world reports).

The SQLite documentation explicitly describes this as "Checkpoint starvation": "A checkpoint is only able to run to completion, and reset the WAL file, if there are no other database connections using the WAL file. If another connection has a read transaction open, then the checkpoint cannot reset the WAL file because doing so might delete content out from under the reader." (sqlite.org/wal.html, §6)

Richard Hipp confirms in a forum thread that the diagnostic query to find the blocking reader is: `SELECT sql FROM sqlite_stmt WHERE busy;` (sqlite.org/forum/info/7da967e0141c7a1466755f8659e7cb5e38ddbdb9aec8c78df5cb0fea22f75cf6)

Syncthing's issue #10559 is a real-world production report of checkpoint starvation with WAL mode, where `wal_checkpoint(TRUNCATE)` blocks until all readers are finished. (github.com/syncthing/syncthing/issues/10559)

**Verification:** Open a WAL-mode database, start a `BEGIN; SELECT ...;` read transaction and leave it open, then write continuously from another connection. Observe the `-wal` file size growing past the 1000-page (~4MB) auto-checkpoint threshold without truncation.

### Claim 3: Readers can get SQLITE_BUSY even when no write is happening — specifically during connection open/close and recovery.

**Confidence:** High (checked against primary source and independently reproduced).

The SQLite documentation lists two specific scenarios in §9: (a) when the last connection is closing and acquires an exclusive lock for cleanup, a new connection trying to open can get `SQLITE_BUSY`; (b) after a crash, the first connection to open runs recovery under an exclusive lock, and a third connection trying to query during recovery gets `SQLITE_BUSY`. (sqlite.org/wal.html, §9)

Hynek Schlawack's TIL post independently reproduced a third scenario: short-lived, bursty read-only connections (open, SELECT, close) in WAL mode can get `SQLITE_BUSY` even with no application writes at all, because opening/closing a WAL database briefly requires exclusive locks on the `-shm` file. With 64 workers doing 100 rounds each, the "WAL, no busy timeout" scenario produced 7/6400 locked errors, while "WAL, 1s busy timeout" produced 0 and "DELETE, no busy timeout" also produced 0. (hynek.me/til/sqlite-read-only-wal-locked)

**Verification:** Run the reproducer script from the Hynek TIL post (provided in full in the article) with `--workers 64 --rounds 100`.

### Claim 4: WAL mode does not work over network filesystems (NFS, SMB/CIFS, some FUSE).

**Confidence:** Very high (checked against primary source, multiple corroborating sources).

The SQLite documentation states directly: "All processes using a database must be on the same host computer; WAL does not work over a network filesystem. This is because WAL requires all processes to share a small amount of memory and processes on separate host machines obviously cannot share memory with each other." (sqlite.org/wal.html, §1 Overview)

The GoToSocial documentation reinforces: "We do not support running GoToSocial with SQLite on a networked filesystem and we will not be able to help you if you damage your database this way." (docs.gotosocial.org/en/latest/advanced/sqlite-networked-storage)

A SQLite forum thread on WAL over network filesystems adds: "Network filesystems have a long history of suboptimal lock support... we invariably recommend against using sqlite on networked filesystems." (sqlite.org/forum/forumpost/9cb84b9347719852)

**Verification:** Place a WAL-mode database on an NFS mount, open it from two processes — expect `SQLITE_PROTOCOL` ("locking protocol") or `SQLITE_IOERR` errors.

### Claim 5: It is impossible to change the database page size while in WAL mode.

**Confidence:** Very high (checked against primary source).

The SQLite documentation states: "It is not possible to change the page_size after entering WAL mode, either on an empty database or by using VACUUM or by restoring from a backup using the backup API. You must be in a rollback journal mode to change the page size." (sqlite.org/wal.html, §1)

**Verification:** Create a WAL-mode database with the default page size, attempt `PRAGMA page_size = 8192; VACUUM;` — the page size will not change. Switch to `DELETE` journal mode first, change the page size, then switch back to WAL.

### Claim 6: WAL mode performs poorly or fails for very large transactions.

**Confidence:** High (checked against primary source; the 100MB/1GB thresholds were stated in older docs but revised in 3.11.0+).

The SQLite documentation previously warned that WAL does not work well for transactions larger than ~100MB and may fail with I/O or disk-full errors for transactions exceeding 1GB. Beginning with version 3.11.0 (2016-02-15), this was significantly improved, but the documentation still recommends rollback journal modes for "transactions larger than a few dozen megabytes." (sqlite.org/wal.html, §1 and §6)

**Verification:** Benchmark a bulk insert of 500MB of data in a single transaction in WAL mode vs. DELETE mode; measure WAL file growth and commit latency.

### Claim 7: The last connection to close performs a final checkpoint and deletes the -wal and -shm files — but only if it closes cleanly and is not read-only.

**Confidence:** Very high (checked against primary source).

The SQLite WAL format documentation states: "If the last client using the database shuts down cleanly by calling sqlite3_close(), then a checkpoint is run automatically in order to transfer all information from the wal file over into the main database, and both the shm file and the wal file are unlinked. However, if the last client did not call sqlite3_close() before it shut down, or if the last client to disconnect was a read-only client, then the final cleanup operation does not occur and the shm and wal files may still exist on disk even when the database is not in use." (sqlite.org/walformat.html, §1.4)

A real-world report confirms this: after terminating a process with a read-only connection as the last one, `wal.db-shm` and `wal.db-wal` were left on disk. (github.com/WiseLibs/better-sqlite3/issues/376)

**Verification:** Open a WAL-mode database with a read-only connection as the last connection, close it, and check whether `-wal` and `-shm` files persist (they will).

### Claim 8: WAL's -shm file requires shared memory support; EXCLUSIVE locking mode is the escape hatch but eliminates multi-process access.

**Confidence:** Very high (checked against primary source).

The SQLite documentation describes the WAL-without-shared-memory mode: "WAL databases can be created, read, and written even if shared memory is unavailable as long as the locking_mode is set to EXCLUSIVE before the first attempted access." In this mode, "SQLite never attempts to call any of the shared-memory methods and hence no shared-memory wal-index is ever created" — but "the database connection remains in EXCLUSIVE mode as long as the journal mode is WAL." (sqlite.org/wal.html, §8)

This means that if you need WAL but cannot use shared memory (e.g., a constrained embedded environment), you must use EXCLUSIVE locking, which reduces you to a single process — defeating the multi-reader benefit.

**Verification:** Set `PRAGMA locking_mode=EXCLUSIVE` before opening a WAL database; confirm no `-shm` file is created. Attempt to open a second connection from another process — it will fail.

## Unique or Easily Missed Findings

### A. Read-only connections are the ones that leave -wal/-shm files on disk

Most discussions of WAL cleanup focus on crashes. But the SQLite documentation explicitly states that if the last connection to disconnect is a **read-only** client, the final checkpoint and file cleanup does not occur — even on a clean `sqlite3_close()`. This is a subtle but documented behavior that can cause orphaned `-wal`/`-shm` files in desktop apps that have a mix of read-only and read-write connections and where the read-only connection happens to close last.

### B. "Reading implies writing" in WAL mode — the -shm file gets modified by read-only access

Hynek Schlawack's TIL post demonstrates that read-only connections in WAL mode still write to the `-shm` file (the modification times of `-wal` and `-shm` update even when the main `.db` file hasn't been touched in days). This is because connections coordinate through the `-shm` file, and opening/closing a WAL database can briefly require exclusive locks. This means "read-only" is a misnomer for the filesystem side of things in WAL mode, and it can cause `SQLITE_BUSY` on bursty short-lived read-only connections with no busy timeout — a scenario where most developers expect zero contention.

### C. The checkpoint cannot reset the WAL past the oldest reader's end mark — even if the data is already in the main db

The checkpoint must stop at the point in the WAL corresponding to the oldest active reader's snapshot. This means even a single long-running analytical query (e.g., a full-table scan for a dashboard) can pin the entire WAL, preventing truncation while writes continue appending. The practical consequence is that a desktop app with a background "live query" or "watch" feature can cause unbounded WAL growth if writes are also continuous.

### D. Checkpoint modes matter: PASSIVE (default) vs. FULL vs. RESTART vs. TRUNCATE

The default automatic checkpoint is PASSIVE — it does as much as possible without blocking but cannot guarantee WAL reset. `TRUNCATE` checkpoints are stronger but block until all readers are finished and block new writers, which can cause its own contention. A desktop app that needs bounded WAL size may need to periodically grab an exclusive lock, block all readers, and run `PRAGMA wal_checkpoint(TRUNCATE)` — the Syncthing developers concluded they needed exactly this approach (every 8 hours with a super-lock). (forum.syncthing.net/t/syncthing-on-sqlite-help-test/23981?page=8)

### E. POSIX file locks are process-scoped, not connection-scoped

A single process with multiple connections to the same WAL database shares lock state at the process level. This means that within a single process, you cannot use multiple connections to achieve additional concurrency beyond what WAL already provides — the process holds one set of locks. This is usually fine for a desktop app, but it means the "multi-reader" benefit is really about multi-process or multi-thread with separate connections, and the lock granularity is per-process, not per-connection.

### F. SQLITE_BUSY vs SQLITE_LOCKED distinction

In WAL mode, `SQLITE_BUSY` (database-level contention) is the common error, but `SQLITE_LOCKED` (table-level contention) has different semantics. The distinction matters for error handling: `SQLITE_BUSY` is retried via `busy_timeout`, while `SQLITE_LOCKED` is not. In WAL mode, table-level locks are largely absent, so `SQLITE_LOCKED` is rare — but it can still appear in specific schema-change scenarios.

## Uncertainties and Contradictions Within This Report

### 1. Large transaction thresholds: stale vs. current guidance

Older SQLite documentation warned that WAL "does not work well for transactions larger than about 100 megabytes" and "may fail with I/O or disk-full error" for transactions exceeding 1GB. The current documentation notes this was improved in version 3.11.0 (2016-02-15), with the strikethrough text indicating the old limitation is largely resolved. However, the docs still recommend rollback journal modes for transactions "larger than a few dozen megabytes." I could not independently verify the exact current threshold on a modern SQLite build — this would require benchmarking.

### 2. Reader blocking during writer contention — framework-dependent

The Xojo forum post reports that during a writer's busy timeout, read threads are also blocked. The SQLite forum response suggests this may be a framework/threading-model issue rather than a WAL-level issue: "Journal mode and threading mode are orthogonal concepts." The extent to which readers are blocked during writer contention depends on whether the application's threading model holds a global mutex during the busy wait. I did not verify this independently — it may be specific to the Xojo framework's cooperative threading model rather than SQLite itself.

### 3. EXCLUSIVE locking mode "stuck" behavior

The documentation states that if EXCLUSIVE locking mode is set before the first WAL access, the connection is "stuck" in EXCLUSIVE mode and the only way out is to first change out of WAL journal mode. But it also says "As long as exactly one connection is using a shared-memory wal-index, the locking mode can be changed freely between NORMAL and EXCLUSIVE." I did not test the exact boundary conditions for when this transition is and isn't possible in a multi-connection desktop app scenario.

### 4. The -shm file is never fsync'd

The WAL format documentation notes: "the shm file is never fsync()-ed to disk" because it is "only used to coordinate access between concurrent clients" and is rebuilt from the WAL on recovery. This means a crash could leave a stale/corrupt `-shm` file, but SQLite handles this by rebuilding it on next open. I did not verify whether there are edge cases where a corrupt `-shm` combined with a corrupt `-wal` could lead to data loss — the documentation implies recovery is robust, but I did not test crash scenarios.

## Verification Targets

| Claim | Cheapest useful verification |
|-------|------------------------------|
| Writer serialization | 2 connections, `busy_timeout=0`, concurrent writes → `SQLITE_BUSY` |
| Checkpoint starvation | Long-running read txn + continuous writes → observe `-wal` file growth |
| Reader SQLITE_BUSY on bursty opens | Run the Hynek TIL reproducer script (64 workers × 100 rounds) |
| Network filesystem incompatibility | Place WAL db on NFS mount → `SQLITE_PROTOCOL` or `SQLITE_IOERR` |
| Page size change impossible | `PRAGMA page_size=8192; VACUUM;` in WAL mode → no effect |
| Read-only last connection leaves files | Open read-only, close, check for orphaned `-wal`/`-shm` |
| EXCLUSIVE mode eliminates `-shm` | Set `locking_mode=EXCLUSIVE` before first WAL access → no `-shm` created |
| Large transaction degradation | Benchmark 500MB single-transaction insert in WAL vs. DELETE mode |

## Sources Consulted

1. **SQLite Official WAL Documentation** — https://www.sqlite.org/wal.html (primary source; §1 Overview, §2.2 Concurrency, §2.3 Performance, §5 Read-Only Databases, §6 Avoiding Excessively Large WAL Files, §8 WAL Without Shared-Memory, §9 SQLITE_BUSY in WAL Mode, §10 Backwards Compatibility)
2. **SQLite WAL File Format Documentation** — https://sqlite.org/walformat.html (primary source; §1.4 File Lifecycles, §1.5 Variations, lock semantics)
3. **SQLite wal-lock.md (GitHub)** — https://github.com/sqlite/sqlite/blob/master/doc/wal-lock.md (primary source; blocking lock semantics, recovery, reader/writer/checkpointer lock behavior)
4. **SQLite Temporary Files Documentation** — https://sqlite.org/tempfiles.html (primary source; `-wal` and `-shm` file lifecycle)
5. **Hynek Schlawack, "SQLite WAL Mode Can Lock Short-Lived Readers"** — https://hynek.me/til/sqlite-read-only-wal-locked/ (independent reproduction of reader SQLITE_BUSY with full reproducer script; July 2026)
6. **Syncthing Issue #10559: Checkpoint starvation** — https://github.com/syncthing/syncthing/issues/10559 (real-world production report)
7. **Syncthing forum discussion on SQLite checkpoint strategy** — https://forum.syncthing.net/t/syncthing-on-sqlite-help-test/23981?page=8 (developer discussion of TRUNCATE checkpoint + super-lock approach)
8. **GoToSocial documentation: SQLite on networked storage** — https://docs.gotosocial.org/en/latest/advanced/sqlite-networked-storage/ (real-world guidance against WAL on network filesystems)
9. **SQLite Forum: WAL on network filesystems** — https://sqlite.org/forum/forumpost/9cb84b9347719852 (community + maintainer discussion)
10. **SQLite Forum: Checkpoint Starvation: connections vs. transactions** — https://sqlite.org/forum/info/7da967e0141c7a1466755f8659e7cb5e38ddbdb9aec8c78df5cb0fea22f75cf6 (Richard Hipp's diagnostic query)
11. **SQLite Forum: WAL journal and threading mode** — https://sqlite.org/forum/info/461653af585fb599 (orthogonality of journal mode and threading mode)
12. **SQLite Forum: Exclusive write-only lock?** — https://sqlite.org/forum/forumpost/fa3db692ec (WAL lock semantics clarification)
13. **WiseLibs/better-sqlite3 Issue #376: WAL mode cleanup** — https://github.com/WiseLibs/better-sqlite3/issues/376 (real-world report of read-only last connection leaving `-wal`/`-shm` files)
14. **SQLite Forum: Using WAL mode with multiple processes** — https://sqlite.org/forum/forumpost/e6c238e854 (WAL mode persistence and multi-process setup)
15. **Oldmoe's blog: Concurrent Write Transactions in SQLite** — https://oldmoe.blog/2024/07/08/the-write-stuff-concurrent-write-transactions-in-sqlite (analysis of WAL write serialization and fsync impact)
16. **SQLite Forum: WAL file growth concern** — https://groups.google.com/g/sqlite_users/c/Ztudwnl_4f4 (Richard Hipp and Bob Smith discussion of checkpoint starvation mechanics)
