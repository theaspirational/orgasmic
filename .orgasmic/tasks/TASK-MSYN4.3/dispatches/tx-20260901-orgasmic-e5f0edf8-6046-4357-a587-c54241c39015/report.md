# Changed

- `crates/orgasmic-daemon/src/writer.rs:2817` now gives every `TxIdPolicy::ProjectSequence` append the existing `tx-{date}-{slug}-{uuid_v4}` shape, including node journals and legacy project tx paths.
- Deleted `ProjectTxSeqCache`, `next_project_tx_id`, `scan_project_tx_max_seq`, numeric-tail parsing, scan counters, and cache invalidation/signature plumbing from `crates/orgasmic-daemon/src/writer.rs`.
- `crates/orgasmic-daemon/src/writer.rs:3982` pins UUID minting for node journals; `:4014` concurrently mints through two independent writer handles and proves the ids differ.
- `crates/orgasmic-core/src/tx.rs:1171` adds the two-machine fold regression: same project/date/task generations use distinct UUID tx ids, and each `CLOSED_TX` closes only its referenced generation.
- `crates/orgasmic-daemon/tests/writer_durability.rs:476` retains the inode-swap/reopen coverage while accepting existing numeric ids and asserting the new UUID append lands on the replacement inode. Sequence-cache-only durability tests were deleted.
- Consumer probe (`/tmp/TASK-MSYN4.3-consumer-grep.log`): no UI tx/entry-id sort or comparison; no numeric-tail parser remains. The only production ordering hit is `crates/orgasmic-daemon/src/index.rs:4315`, which sorts by `TIME` first and uses `tx_id` only as a deterministic tie-break. The UUID parse hit is a daemon integration assertion, not an ordering consumer.
- `TxIdPolicy::Preserve` was left unchanged: it preserves a caller-supplied id and does not mint on the cross-machine path.
- Commit: `2bbd467e` (`TASK-MSYN4.3: fix(writer): mint UUID tx ids on all project paths`).

# Verification Gates

- PASS — `cargo test -p orgasmic-core --lib tx`: `25 passed; 0 failed`; new fold test `...distinct_by_uuid_tx_id ... ok`. Log: `/tmp/TASK-MSYN4.3-core-tx.log`.
- PASS — daemon lib tests selected by the replacement test names because `--list` showed the old `tx_id` / `writer::tests::prepare` filters matched nothing:
  - `cargo test -p orgasmic-daemon --lib -- writer::tests::project_sequence_policy_mints_uuid_for_node_journal --exact`: `1 passed; 0 failed`.
  - `cargo test -p orgasmic-daemon --lib -- writer::tests::two_writers_cannot_mint_the_same_node_journal_tx_id --exact`: `1 passed; 0 failed`.
  - Log: `/tmp/TASK-MSYN4.3-daemon-targeted.log`; test inventory: `/tmp/TASK-MSYN4.3-daemon-list.log`.
- PASS — `cargo test -p orgasmic-daemon --test writer_durability -- tx_append_reopens_after_path_inode_swap --exact`: `1 passed; 0 failed`. Log: `/tmp/TASK-MSYN4.3-writer-durability.log`.
- PASS — `cargo clippy -p orgasmic-core -p orgasmic-daemon --all-targets -- -D warnings`: `Finished dev profile`; log: `/tmp/TASK-MSYN4.3-clippy-final.log`.
  - The first clippy attempt correctly found stale `scan_count` references in sequence-only durability tests (`/tmp/TASK-MSYN4.3-clippy.log`); those tests were deleted or narrowed to the surviving inode-reopen behavior before the passing rerun.
- PASS — `cargo fmt --all --check`. Log: `/tmp/TASK-MSYN4.3-fmt.log`.
- PASS — `git diff --check`. Log: `/tmp/TASK-MSYN4.3-diff-check.log`.

# Unmet Criteria

None.

# Residual Risk

- UUID v4 uniqueness is probabilistic rather than mathematically impossible; it removes the deterministic cross-machine collision mode.
- The legacy enum name `ProjectSequence` remains for API compatibility/scope control even though its minting strategy is now UUID-based. Existing numeric-tail ids remain opaque, valid references.
