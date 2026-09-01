# Review: TASK-MSYN4.3 — UUID tx ids on every project path (cross-machine tx-id collision)

Implementer: codex gpt-5.6-sol, one commit `2bbd467e`, merged to main as `568cb5be`.
Read the task first: `orgasmic task get --project orgasmic TASK-MSYN4.3` (the finding and
acceptance). Then:

    git diff 568cb5be^1 568cb5be

Three files, +130/-437: `crates/orgasmic-daemon/src/writer.rs` (the substance: every
`TxIdPolicy::ProjectSequence` append now mints `tx-{date}-{slug}-{uuid_v4}`; the
`ProjectTxSeqCache` / `next_project_tx_id` / `scan_project_tx_max_seq` machinery, numeric-tail
parsing and cache invalidation are deleted), `crates/orgasmic-core/src/tx.rs` (two-machine
fold regression test), `crates/orgasmic-daemon/tests/writer_durability.rs` (sequence-only
durability tests deleted; inode-swap/reopen coverage kept).

## The finding this must close
Node journals and legacy project tx paths minted `tx-{date}-{slug}-{N}` from a per-machine
max-seq scan, so two machines writing the same project on the same day could mint the SAME
id for different generations; a `CLOSED_TX` referencing that id could then close the wrong
generation after the ledger sync folded both sides. Machine-scoped tx already used uuids.

## What the fix claims
1. One minting shape everywhere `ProjectSequence` applies; `TxIdPolicy::Preserve` untouched.
2. No production consumer parses the numeric tail or sorts by tx id, except
   `crates/orgasmic-daemon/src/index.rs:~4315`, which sorts by `TIME` first and uses `tx_id`
   only as a deterministic tie-break (implementer's grep, `/tmp/TASK-MSYN4.3-consumer-grep.log`
   may still exist — do your own grep regardless).
3. Tests: `writer::tests::project_sequence_policy_mints_uuid_for_node_journal`,
   `two_writers_cannot_mint_the_same_node_journal_tx_id`, the tx.rs fold regression
   (`…distinct_by_uuid_tx_id`), and `tx_append_reopens_after_path_inode_swap` accepting
   existing numeric ids beside a new uuid append.

## Attack these specifically
- **Consumers of the id shape.** Grep the whole tree, not just the daemon: `rg -n
  'tx-\\d|split\\(.-.\\)|rsplit|parse::<u|numeric|seq' crates/ ui/src shipped/` and read each
  hit. Anything that (a) validates an id against a numeric-tail pattern, (b) uses the tail as
  a cursor/"newer than" comparison (`/tx?after=`, `since`, "latest tx"), (c) documents the
  shape in `shipped/schema/tx.org` or `shipped/skills/orgasmic/references/*.md` (docs drift
  is a LOW; a validator that rejects uuids is a HIGH), or (d) truncates/pads ids for table
  output in the CLI or UI.
- **Ordering semantics changed silently.** With numeric tails, the `index.rs` tie-break
  preserved insertion order within one second; with uuids it is random-but-deterministic. Is
  there any consumer for which same-second order matters — two comments in one second in
  `GET /tasks/:id/activity`, a `task.state_transitioned` and a `*.done` in the same second in
  the torn-close fold (`manager.rs torn_close_candidates`, `api.rs
  recorded_close_allows_repair` — both compare parsed TIME; check what they do on EQUAL
  times), the handoff/journal renderers in the UI? Say which, if any, and whether it is a
  correctness or a cosmetic issue.
- **Mixed-version fleet.** Machine A runs the old runtime (numeric tails), machine B the new
  one (uuids), same project, same day. Can A's max-seq scan (still running on A) be confused
  by B's uuid ids in the same file (what does the deleted `scan_project_tx_max_seq` do with a
  non-numeric tail — you can read it in `568cb5be^1`)? Can A still collide with itself or
  with an older B id? This decides the release note.
- **What the deleted tests covered.** List every test removed from `writer.rs` and
  `writer_durability.rs` and say, per test, whether it covered ONLY the sequence cache (fine
  to delete) or also a durability property that no surviving test pins (inode swap, reopen
  after rename, fsync ordering, concurrent appenders). A durability property lost with the
  cache is a MEDIUM.
- **The two-writer test is trivially true with uuids.** Does any test still prove the
  cross-machine FOLD property — two entries with the same project/date/task and different ids
  survive `fold`/`parse_journal` as two generations, and `CLOSED_TX` closes exactly one? Read
  the tx.rs test; does it construct the collision the finding describes, or just two random ids?
- **Dead code and docs.** Are `ProjectSequence` (now a misnomer), any `seq` fields in
  `WriterHandle`/`ApiState`, metrics, or `daemon status` counters (`scan_count`) still
  referenced or serialised? `rg -n 'scan_count|max_seq|ProjectTxSeq' crates/ ui/src shipped/`.
- **Performance/behaviour of the deleted scan.** The old path scanned the tx file for the max
  seq (with a cache); the new path does no read. Confirm nothing else depended on that read
  (e.g. it was also the thing that created the month file or validated the header).

Already established — do not re-spend: the implementer ran 6 gates (core tx 25 passed, two
exact writer tests, writer_durability exact, clippy, fmt, diff --check) and the manager re-ran
on merged main `568cb5be`: `cargo test -p orgasmic-core --lib tx`, `cargo test -p
orgasmic-daemon --lib -- writer` (ALL writer tests, not just the two), `cargo test -p
orgasmic-daemon --test writer_durability`, clippy core+daemon, fmt — see `orgasmic task get
--project orgasmic TASK-MSYN4.3` Evidence. Targeted re-runs are fine; never the workspace.

## Rules
- READ-ONLY. No edits, no git writes, no mutating `orgasmic` verbs, nothing against the live
  ledger at `~/.orgasmic/ledgers/orgasmic` (read-only `git log`/`rg` there is fine to see real
  id shapes; the live daemon on :4848 runs the PRE-fix runtime — not a defect).
- File each finding as it appears:
  `orgasmic tx record --project orgasmic --type reviewer.finding --task TASK-MSYN4.3
  --reason "HIGH|MEDIUM|LOW <file:line> — <one sentence>"` (single line).
- Targeted tests only; NEVER the whole `orgasmic-cli` suite unfiltered; never the workspace;
  never `ORGASMIC_HOME`; never `daemon start`; do not read `verify/*/injection.patch`; never run
  `legacy_drivers_and_explicit_pairs_emit_equivalent_start_events`.
- Say what you did not check. Finish with `orgasmic dispatch finalize --summary-file <path>`
  (report only) and end with the explicit verdict sentence:
  APPROVE / APPROVE WITH FOLLOW-UPS / REJECT.
