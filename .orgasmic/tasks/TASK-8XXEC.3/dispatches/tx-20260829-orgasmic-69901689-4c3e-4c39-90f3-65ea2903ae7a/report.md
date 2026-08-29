# Cross-Review Delta Report: SQLite WAL Mode Failure Modes

## Reviewer

hermes · openai · gpt-5.6-luna · effort low

## Delta

### ? Challenged claims

**?1. Claim 6 / Uncertainty 1 — "the documentation still recommends rollback journal modes for transactions larger than a few dozen megabytes"**

The report (hermes · google · gemini-3.7-flash · effort low) states the SQLite documentation "still recommends rollback journal modes for 'transactions larger than a few dozen megabytes.'" This is incorrect. On the current sqlite.org/wal.html §1, that recommendation is explicitly struck through (superseded). The current non-struck-through text reads: "Beginning with version 3.11.0 (2016-02-15), WAL mode works as efficiently with large transactions as does rollback mode." There is no current recommendation to prefer rollback modes for large transactions on modern SQLite. The report's Uncertainty #1 partially hedges this ("I could not independently verify the exact current threshold"), but the claim text itself still asserts the docs "still recommend" the old guidance — a contradiction with the primary source. The report does correctly note that §6 separately warns large transactions cause large WAL files (a space/growth concern, not a performance recommendation), but it conflates the two.

**?2. Claim 3 — "The SQLite documentation lists two specific scenarios in §9" for SQLITE_BUSY on readers**

The report enumerates two SQLITE_BUSY scenarios from wal.html §9: (a) last-connection cleanup and (b) crash recovery. In fact §9 lists **three**: the first scenario — another connection holding the database in exclusive locking mode (e.g., Chrome and Firefox do this) — is omitted. While less common in a single-writer multi-reader desktop app that controls its own connections, it is relevant when the app might open its database in EXCLUSIVE locking mode, or when another process (browser, file inspector) holds an exclusive lock. The report's claim of "two specific scenarios" understates the documented list.

### + Material additions missing from the reviewed report

**+1. The WAL-reset bug (wal.html §11) — a corruption-class failure mode for multi-connection WAL databases**

The reviewed report does not mention the WAL-reset bug, which was discovered 2026-03-03 and fixed in SQLite 3.51.3 (2026-03-13). It affects databases in WAL mode with two or more connections (separate threads or processes) attempting to write or checkpoint at the same instant. The data race can cause the second checkpoint to skip a committed transaction, resulting in **database file corruption**. This is directly relevant to a single-writer multi-reader desktop app: such apps typically have multiple connections (one writer, several readers) and run automatic checkpoints, which is exactly the trigger condition. Backports exist for 3.44.6 and 3.50.7. The SQLite developers rate the wild occurrence as ≤ SSD malfunction / cosmic-ray rates, but the consequence is corruption, so the fix matters. Any desktop app shipping an unpatched SQLite (< 3.51.3) with multiple WAL connections carries this residual risk.

**+2. WAL mode persistence and cross-connection visibility (wal.html §3.3)**

The report does not mention that WAL mode is persistent — `PRAGMA journal_mode=WAL` survives close/reopen, unlike other journal modes. Additionally, "The WAL journal mode will be set on all connections to the same database file if it is set on any one connection." This has operational consequences for desktop apps: a one-time setup command converts the database permanently, and any connection setting WAL affects all connections to that file. This is a relevant operational characteristic for deployment and migration.

**+3. Multi-database (ATTACH) transactions are not atomic as a set (wal.html §1, disadvantage #2)**

The report does not mention that transactions involving multiple ATTACHed databases are atomic per-database but not atomic across all databases as a set in WAL mode. For desktop apps that use ATTACH for multi-file databases (e.g., separating config from data), this is a real transactional integrity failure mode.

**+4. Read-only WAL database opening requirements (wal.html §1, disadvantage #4 / §5)**

The report does not cover the read-only opening constraints. Prior to SQLite 3.22.0 (2018-01-22), reading a WAL-mode database required write access (for the -shm file). Since 3.22.0, read-only opening is possible if the -shm and -wal files already exist, or there is write permission on the directory, or the `immutable` URI parameter is used. For a desktop app distributing a database on read-only media (CD, locked folder), this matters — and the docs recommend converting to `journal_mode=DELETE` before burning to read-only media.

### = Independently confirmed

**=1. Checkpoint starvation from long-running readers (Claim 2, Finding C)**

Confirmed against wal.html §2.2 and §6. §2.2 states: "a long-running read transaction can prevent a checkpointer from making progress" and the checkpoint "must stop when it reaches a page in the WAL that is past the end mark of any current reader." §6 adds: "if a database has many concurrent overlapping readers and there is always at least one active reader, then no checkpoints will be able to complete and hence the WAL file will grow without bound." The report's characterization is accurate.

**=2. Readers can get SQLITE_BUSY with no application write in progress (Claim 3, Finding B)**

Confirmed against wal.html §9 (three scenarios) and the Hynek Schlawack TIL post (hynek.me/til/sqlite-read-only-wal-locked). The TIL post's reproducer shows 7/6400 "database is locked" errors for bursty short-lived read-only connections in WAL mode with no busy timeout, and 0 errors with a 1s busy timeout or in DELETE mode. The post is dated 26 July 2026. The report's description of this finding is accurate.

**=3. WAL does not work over network filesystems (Claim 4)**

Confirmed against wal.html §1: "All processes using a database must be on the same host computer; WAL does not work over a network filesystem." The report's claim and the supporting secondary sources (GoToSocial docs, SQLite forum) are consistent with the primary source.

**=4. Page size cannot be changed in WAL mode (Claim 5)**

Confirmed against wal.html §1: "It is not possible to change the page_size after entering WAL mode, either on an empty database or by using VACUUM or by restoring from a backup using the backup API. You must be in a rollback journal mode to change the page size."

**=5. Read-only last connection leaves -wal/-shm files (Claim 7, Finding A)**

Confirmed against walformat.html §1.4: "if the last client to disconnect was a read-only client, then the final cleanup operation does not occur and the shm and wal files may still exist on disk even when the database is not in use."

**=6. EXCLUSIVE locking mode eliminates -shm but locks to single process (Claim 8, Uncertainty 3)**

Confirmed against wal.html §8: "If EXCLUSIVE locking mode is set prior to the first WAL-mode database access... SQLite never attempts to call any of the shared-memory methods and hence no shared-memory wal-index is ever created." And: "the database connection remains in EXCLUSIVE mode as long as the journal mode is WAL; attempts to change the locking mode using 'PRAGMA locking_mode=NORMAL;' are no-ops." The report's Uncertainty #3 about boundary conditions is valid — §8 does note that "As long as exactly one connection is using a shared-memory wal-index, the locking mode can be changed freely between NORMAL and EXCLUSIVE."

**=7. The -shm file is never fsync'd (Uncertainty 4)**

Confirmed against walformat.html §1.3: "the shm file is never fsync()-ed to disk" because it "does not need to be preserved across a crash" and is rebuilt from the WAL on recovery. The report's assessment that recovery is robust but crash-edge-cases are untested is a fair characterization.

**=8. Checkpoint modes: PASSIVE / FULL / RESTART / TRUNCATE (Finding D)**

Confirmed. wal.html §3.2 describes "three subtypes: PASSIVE, FULL, and RESTART" but §6 and the C API (`sqlite3_wal_checkpoint_v2`) define four, including TRUNCATE. The report's enumeration of four modes is correct. The Syncthing discussion of TRUNCATE + super-lock is a legitimate real-world example.

**=9. POSIX file locks are process-scoped (Finding E)**

Confirmed. This is a well-known POSIX behavior: file locks (`fcntl`/`flock`) are per-process, not per-connection (fd). SQLite's WAL lock implementation uses POSIX byte-range locks, so a single process with multiple connections shares one lock set. This is correctly noted by the report.

## Cross-report Contradictions

No cross-report contradictions are possible in this dispatch — only one other participant's report was provided for review (TASK-8XXEC.2, hermes · google · gemini-3.7-flash · effort low). All challenges above are against that single report.

The most significant internal contradiction within the reviewed report is **?1**: Claim 6 asserts the docs "still recommend" rollback modes for large transactions, while Uncertainty #1 hedges the same point as unverifiable. The primary source resolves this — the old recommendation is struck through and superseded by the 3.11.0 guidance.

## Highest-value Verification Targets

| Priority | Target | Method |
|----------|--------|--------|
| 1 | WAL-reset bug (§11) applies to the app's SQLite version | Check the app's bundled SQLite version against 3.51.3; if < 3.51.3 and the app uses 2+ WAL connections, document the corruption risk and recommend upgrade/backport |
| 2 | Checkpoint starvation in practice | Open a WAL db, start a `BEGIN; SELECT …;` read txn and leave it open, write continuously from another connection, observe `-wal` file growth past the 1000-page auto-checkpoint threshold |
| 3 | Bursty read-only SQLITE_BUSY reproducer | Run the Hynek TIL reproducer script with `--workers 64 --rounds 100`; confirm ~1-10 failures in WAL/no-timeout, 0 with busy timeout or DELETE mode |
| 4 | Read-only last connection leaves files | Open WAL db read-only as the last connection, close, check for orphaned `-wal`/`-shm` |
| 5 | Large transaction WAL growth (not performance) | Benchmark 500MB single-transaction insert in WAL mode; measure WAL file size and commit latency. This tests the §6 space concern, not the superseded performance recommendation |

## Reports Reviewed

- **TASK-8XXEC.2** — hermes · google · gemini-3.7-flash · effort low
  Report: `/Users/aspirational/.orgasmic/ledgers/orgasmic/.orgasmic/tasks/TASK-8XXEC.2/dispatches/tx-20260829-orgasmic-f7a20486-fadb-47b0-9d0f-5471f9cd555b/report.md`
