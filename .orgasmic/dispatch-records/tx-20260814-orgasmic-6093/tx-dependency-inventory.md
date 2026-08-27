# tx / task-state-file dependency inventory

Research output for **TASK-AP971.4**. Blast-radius input for **dec_E01MC** (unified
per-node directory schema).

Scope: every reader and writer of two path families.

- **Family A** — `.orgasmic/tx/` monthly transaction ledger files (`tx/2026-08.org`),
  plus the daemon-home ledger at `$ORGASMIC_HOME/state/tx/` (`crates/orgasmic-core/src/home.rs:68-69`).
- **Family B** — `.orgasmic/tasks/<state>.org` per-state task files: `backlog.org`,
  `todo.org`, `in_progress.org`, `in_review.org`, `done.org`, `cancelled.org`
  (`crates/orgasmic-core/src/paths.rs:10-17`), plus the two manager singletons
  `goal.org` and `handoff.org` (`crates/orgasmic-core/src/paths.rs:18-19`).

The split under consideration: node-scoped events (comments, state changes, dispatch
events) move into per-node `journal.org` under `.orgasmic/<collection>/<ID>/`, while
project-level events stay in monthly `tx/`; per-state task files are replaced by
per-task directories.

All line numbers verified against `main` at `862d166`.

---

## Two corrections to the ticket's premises

**1. `recovery_claim.rs` has zero touchpoints in either family.** The ticket flagged it
as the file that "leans on tx heavily." It does not. All 7243 lines are about run-recovery
session state under `.orgasmic/tmp/sessions/` — `SessionFile`, `SessionEnvelope`,
`RecoveryClaim`, keyed on `origin_run_id`/`replacement_session_path`
(`crates/orgasmic-daemon/src/recovery_claim.rs:1150`, `:1170`, `:7224`). It contains no
`append_tx`, no `parse_tx_file`, no `iter_task_file_paths`, and no `.orgasmic/tx` or
`.orgasmic/tasks` path construction. Its one mention of tx is a doc comment describing a
crash window *before* an `ORIGIN=recovery` tx is written elsewhere
(`crates/orgasmic-daemon/src/recovery_claim.rs:3792`). The tx-heavy recovery logic the
ticket was pointing at actually lives in `api.rs` (`:7542-7575`, `:7595-7643`,
`:17600-19010`) and in the CLI's `scan_dispatches` fold
(`crates/orgasmic-cli/src/manager.rs:9463-9498`).

**2. `docs/agents/issue-tracker.md` does not exist.** `AGENTS.md:10` links to it, and
`git ls-files docs` returns nothing — the entire `docs/` tree is untracked. A research
agent read the file mid-sweep and found it gone on a second pass; it has no git history.
Treat that citation as unstable. `docs/` is not gitignored, so this file lands as an
untracked addition.

---

## Summary table

| Component | Touchpoints | Family | Severity for the split |
|---|---|---|---|
| `crates/orgasmic-daemon/src/api.rs` | ~50 | A + B | **Critical** |
| `crates/orgasmic-daemon/src/` (rest) | 63 | A + B | **Critical** |
| `crates/orgasmic-cli/src/` | 38 | A + B | **Critical** |
| `crates/orgasmic-core/src/` | 44 | A + B | **High** (it *defines* both families) |
| `crates/orgasmic-cli/tests/` | ~135 call sites via ~14 helpers | A + B | High (concentrated) |
| `shipped/` | 29 | A + B | High |
| `ui/` | 13 | A (12 indirect, 1 direct-path) | Medium |
| `scripts/` + `verify/` | 11 | B mostly | Medium (2 silent-failure modes) |
| `docs/` + root markdown | 6 | A + B | Low |
| `crates/orgasmic-drivers/` | **0** | — | None |
| `src-tauri/` | **0** | — | None |
| `crates/orgasmic-daemon/src/recovery_claim.rs` | **0** | — | None |

Total: roughly **254 direct touchpoints** across production and test code, plus the
UI's indirect projection consumers.

---

## `crates/orgasmic-core` — the definitions (44 touchpoints)

This crate does not orchestrate; it *defines* both path families and the tx entry
grammar. Nearly every touchpoint elsewhere in the repo resolves back to a symbol here.

### `paths.rs` — family B path policy (14 definitions)

| Line | Dir. | Family | Mechanism / impact |
|---|---|---|---|
| `paths.rs:7` | — | B | `TASKS_DIR = "tasks"`. |
| `paths.rs:10-17` | — | B | `TASK_FILE_NAMES` — the closed 6-element list of state files. Every full-corpus scan in the repo iterates this. Under per-task directories it has no successor; callers must enumerate directories instead. |
| `paths.rs:18-19` | — | B | `GOAL_FILE` / `HANDOFF_FILE`. These are project-level singletons, not per-task nodes, so they likely survive the split unchanged. |
| `paths.rs:22`, `:25` | — | B | `DEFAULT_TASK_FILE = "backlog.org"` and `DEFAULT_TASK_FILE_REL = ".orgasmic/tasks/backlog.org"`. The second is used as a *tx `:TARGET:` label* in handlers that touch no file at all (`api.rs:3846-3876`, `:4314-4590`) — a display string that would go stale silently. |
| `paths.rs:29-38` | — | B | `lifecycle_stage_file_name` — the `LifecycleStage → filename` map. This function is the definition of "a state is a file"; the split's entire premise is that it stops being true. |
| `paths.rs:40-42` | — | B | `dotorg_tasks_dir`. |
| `paths.rs:44-50` | — | B | `task_file_path` / `task_file_rel`. |
| `paths.rs:52-58` | — | B | `goal_file_path` / `goal_file_rel`. |
| `paths.rs:60-62` | — | B | `handoff_file_path`. |
| `paths.rs:64-68` | READ helper | B | **`iter_task_file_paths`** — the single most-called family-B primitive. Six call sites in production (`index.rs:2651-2734`, `api.rs:15286`, `manager.rs:8211-8237`, `identity_lint.rs:219`, `:253`, `:393`), each doing a linear scan of all six files to answer a by-id question. |
| `paths.rs:778-811` | test | B | Pins `iter_task_file_paths` at exactly 6 paths and `DEFAULT_TASK_FILE_REL` at its literal string — a red test the moment the schema moves. |

Note: everything else in `paths.rs` (lines 86-771) is the dispatch-record / artifact
promotion surface under `.orgasmic/dispatch-records/` and `.orgasmic/tmp/dispatch/`. That
is a **third, separate family** and is unaffected by this split.

### `tx.rs` — family A grammar and writer (13 touchpoints)

| Line | Dir. | Mechanism / impact |
|---|---|---|
| `tx.rs:4-12` | — | Module contract: tx files are **append-only**, drawer-only, one open handle per `TxWriter`, callers serialize externally. A per-node journal inherits all of this or forks it. |
| `tx.rs:68-83` | — | `TxEntry` — the wire struct. `tx_id`, `time`, `ty`, `actor`, `machine`, then optional `project`, `task`, `target`, `reason`, plus an ordered `extra: Vec<(String,String)>` bag. **`task` is already an optional field**, so the entry format itself can already express node scope; what is missing is a *destination resolver* keyed on it. |
| `tx.rs:110-138` | WRITE | `render` — heading is `* TX <time> <ty> <summary>`, values column-aligned at 16. Survives a relocation unchanged. |
| `tx.rs:149-155`, `:291-313`, `:314-341` | WRITE | `validate` / `validate_property_key` / `validate_property_value` — single-line values only; `END`/`PROPERTIES` reserved. Format-level, path-independent. |
| `tx.rs:174-228` | WRITE | `assert_round_trip` — composes, re-parses, and compares before any byte is written. This is the guard that keeps a ledger from being bricked by its own writer; a `journal.org` writer that skips it reintroduces TASK-HQ970. |
| `tx.rs:359-363` | WRITE | `TxWriter { path, file, needs_leading_blank }` — one handle, one file. Generalizes to N journals only by instantiating N writers, which is what the daemon's handle cache already does (`writer.rs:2225-2246`). |
| `tx.rs:366-390` | WRITE | `TxWriter::open` — `O_APPEND`, seeds `#+title: orgasmic tx <basename>` on first write. **Basename-derived title**: every per-node journal would be titled `orgasmic tx journal`, which is useless. Needs a parameter. |
| `tx.rs:401`, `:414`, `:420` | WRITE | `sync_data`, `append`, `append_many`. |
| `tx.rs:441-453` | READ | `file_ends_with_blank_line` — entry separation invariant. |
| `tx.rs:455+` | READ | **`parse_tx_file(source, display)`** — the only tx reader in the codebase. Every family-A read in every crate goes through it. Path-agnostic, so it works on a `journal.org` as-is. |

### `projects.rs` — the scaffold writer (6 touchpoints)

| Line | Dir. | Family | Mechanism / impact |
|---|---|---|---|
| `projects.rs:107-121` | WRITE | B | `SCAFFOLD_FILES` names all 8 task files explicitly. **`orgasmic project init` would produce a schema-invalid project on day one** unless this list and its shipped templates change together. |
| `projects.rs:168-169` | WRITE | A | `create_dir_all(dotorg.join("tx"))` — the tx directory is created unconditionally at init. Stays valid if project-level tx survives. |
| `projects.rs:176-200` | WRITE | A + B | The scaffold loop: reads each shipped template, renders it, and **parses it with `OrgFile::parse` before writing** (`:186-189`). A shipped template that does not parse fails init loudly — good, but it means template and code must land in one change. |
| `projects.rs:480-510`, `:592-599`, `:731-739` | test | B | Fixtures that write and then assert the existence of `todo.org`/`done.org`/`goal.org`/`handoff.org` by name. |

### Remaining core modules (11 touchpoints)

| Line | Dir. | Family | Mechanism / impact |
|---|---|---|---|
| `home.rs:68-69` | — | A | `Home::tx()` → `<home>/state/tx`. The daemon-home ledger, a **second family-A location** distinct from any project. It has no node scope at all, so it is unaffected by the node-journal split but is the reason `IndexSnapshot.tx` carries `project_id: Option<String>` (`index.rs:301-305`). |
| `home.rs:116`, `:153` | WRITE | A | Home bootstrap creates the tx dir; config template lists it. |
| `identity_lint.rs:8`, `:219`, `:253`, `:393` | READ | B | `collect_identity_occurrences`, `collect_reference_occurrences`, `org_state_rel_paths` each iterate `iter_task_file_paths`. This is the **identity/dangling-reference lint corpus**: it is what makes `parse_errors 0` and dangling-edge detection meaningful. Under per-task directories all three must walk a directory tree instead of a fixed list, or the lint silently stops seeing task nodes — a false green, not a failure. |
| `identity_lint.rs:535-649` | test | B | Fixtures keyed on `backlog.org`/`done.org` literals. |
| `id_repair.rs:285`, `:320` | WRITE | B | `repair_id_collisions` / `repair_id_collisions_with_incoming` take caller-supplied paths, so they are structurally neutral — but their only real caller is `orgasmic doctor --fix-id-collisions` (`orgasmic-cli/src/main.rs:2137-2175`), which feeds them task-file paths. |
| `id_repair.rs:395` | test | B | `.orgasmic/tasks/backlog.org` fixture literal. |
| `lib.rs:58-66`, `:86` | — | A + B | Re-exports the whole surface (`TASK_FILE_NAMES`, `iter_task_file_paths`, `task_file_path`, `parse_tx_file`, `TxWriter`, …) as the crate's public API. Any rename here is a breaking change for both the daemon and the CLI simultaneously. |
| `sandbox.rs:173`, `:177` | test | B | `"backlog.org"` as an `OrgFile::parse` display name only — cosmetic. |
| `tests/fixtures.rs:51-104` | READ | B | **Parses this repo's own live `.orgasmic/tasks/done.org` and `backlog.org`** and asserts they contain tasks. A real-corpus gate that goes red at migration time. |
| `tests/fixtures.rs:258-267` | READ | A | `read_dir(repo_root()/.orgasmic/tx)` picks a live tx file and asserts `parse_tx_file` succeeds. Same shape: real-corpus gate. |

---

## `crates/orgasmic-daemon/src/api.rs` — ~50 touchpoints (Critical)

### Family A chokepoints

| Line | Dir. | Function | Mechanism / impact |
|---|---|---|---|
| `api.rs:7889-7893` | WRITE | `tx_destination` | **The only place a project tx destination path is built**: `project.root/.orgasmic/tx/{month}.org`. Every family-A write funnels here. It branches on project-vs-home only — it has **no `task_id` parameter**, so it cannot express "this node's journal." This one function is the natural seam for the split, and also the reason the split cannot be done incrementally per-handler. |
| `api.rs:2760-2777` | WRITE | `append_tx_request` | `state.writer.append_tx(tx, None)` at `:2768`. Backs `POST /tx` and `/runs/:id/release`. |
| `api.rs:7665-7691` | WRITE | `record_api_tx` / `record_api_tx_after_project_mutation` | Second chokepoint — tx append plus index refresh, used by most handlers. |
| `api.rs:3452-3483` | READ | `project_tx_entries` | `read_dir(.orgasmic/tx)` then `parse_tx_file` on **every** `*.org` in it. Full-directory scan, no scoping. |
| `api.rs:3500-3555` | READ | `dispatch_generation_ledgers` | Folds `manager.dispatch_started` + `run.created(ORIGIN=cli_dispatch)` into per-generation status by matching `tx_id`/`DISPATCH_TX`/`RUN_ID` across the whole scan. |

### Family A writers

`post_task_comment` (`:2269-2319`, comments exist **only** as tx entries — there is no
other store), `post_tx` (`:2749`), `post_manager_action` (`:3846-3876`), manager-tier
(`:4001-4188`), `post_stage` for grill/plan (`:4314-4590`), `record_dispatch_started`
(`:6647-6721`, whose own `tx_id` becomes the `started_tx` token the CLI polls),
`record_dispatch_created` (`:6735-6815`, writes `DISPATCH_TX`/`RUN_ID` — the join key),
`record_recovery_replacement_association` (`:7595-7643`), `release_run_and_record_tx`
(`:9055-9145`, worker-side terminal tx at `:9117`), `post_task_dispatch_close_commit`
(`:16841-17046`), `write_org_node_edit_and_record` (`:15380-15450`),
`write_goal_file_and_record` (`:16407-16452`), `write_task_lifecycle_and_record`
(`:16733-16777`), `write_task_property_and_record` (`:17226-17291`), and ~12 structurally
identical recovery/reattach/finalize writers (`:17600-19010`).

### Family A readers

`get_tx` (`:2567-2650`, returns `snap.tx` rows), `post_manager_dispatch_wait`
(`:3611-3716`, calls `project_tx_entries` at `:3635` on **every poll** — a full ledger
re-read per poll tick), `recovery_association_is_durable` (`:7542-7575`, idempotency
check by scanning the whole in-memory tx list), `tx_count` on `GET /daemon/status`
(`:8004-8029`), `get_task_activity` (`:2321-2341`), `task_activity_slot` (`:4775-4792`,
feeds worker prompt hydration).

### Family B

Path resolution: `org_node_path` (`:14925-14965`, dispatches `Task` → `task.source_file`,
`Goal` → `goal_file_path`, `Handoff` → `handoff_file_path`);
`task_create_target_file_name`/`_path` (`:15918-15925`, new tasks **always** land in
`backlog.org` per dec_QQYXM); `task_file_path(root, lifecycle_stage_file_name(state))`
(`:16909-16912`, `:17141-17143`).

Writers: `post_task_create` (`:16078-16260`), `post_task_dispatch_close_commit`
(`:16841-17046`), `update_task_state` (`:17090-17225`), `update_task_properties`
(`:17293-17389`), `post_org_node_edit` (`:14983-15169`), `post_goal_set` (`:16454-16545`),
`sync_handoff_goal_id` (`:16580-16616`), `post_goal_clear` (`:16618-16676`),
`post_goal_supersede` (`:16678-16719`).

The cross-file move mechanism is `prepare_cross_file_lifecycle_move` (`:16817-16834`) plus
`append_heading_to_task_file` (`:16786-16799`): slice the heading out of `from_path` with
`OrgRewriter::remove_heading`, rewrite the stage keyword, append the subtree to `to_path`.
**Under per-task directories this mechanism disappears entirely** — a state transition
becomes a property update inside one directory, not a two-file rewrite.

Readers: `inbound_reference_owners` (`:15271-15297`) scans `project.org`, `decisions.org`,
`glossary.org`, `goal.org`, `handoff.org` and all six state files via `iter_task_file_paths`
(`:15286`) before permitting a node delete.

### The unscoped backdoor

`post_org_file` (`:13505-13557`) and `get_org_file` (`:13483-13503`) accept **any** path
that `validate_org_edit_path` (`:13559-13577`) approves — the rule is only "under
`.orgasmic/` (or `docs/adr/`) and ends in `.org`." That admits
`.orgasmic/tx/2026-08.org` and `.orgasmic/tasks/backlog.org` directly, bypassing every
specialized handler above. Under the split it would equally admit a per-node
`journal.org`, letting a client stomp an append-only ledger through a generic overwrite
endpoint. This validator needs an explicit denylist as part of the migration, not after.

### tx-derived response shapes (the UI's indirect dependency)

`GET /tx` (`:2567`), `GET /tasks/:id/activity` (`:2321`), `POST /tasks/:id/comments`
returning `{tx_id}` (`:2264-2318`), `tx_count` on `GET /daemon/status` (`:8025`),
`POST /manager/dispatch/wait` returning `{generations:[{started_tx,status,run_id}]}`
(`:3431-3442`, `:3713`), and the ubiquitous `{id, changed, tx_id}` compact mutation
response (`:15162`, `:16193`, `:17039`, `:17384`, `TaskCreateResponse` `:15949`,
`GoalMutationResponse` `:16671`, `DispatchResponse.dispatch_tx_id` `:5244`).

---

## `crates/orgasmic-daemon/src/` (excluding api.rs) — 63 touchpoints (Critical)

### `writer.rs` — 24 touchpoints, family A + B

The transactional core. Full detail:

| Line | Dir. | Mechanism / impact |
|---|---|---|
| `writer.rs:256-265` | WRITE | `TxAppend { tx_path, entry, project_id, tx_id_policy, request_id }` — `tx_path` is caller-supplied, so the struct itself is already destination-agnostic. |
| `writer.rs:274-281` | WRITE | `TxIdPolicy::{Preserve, ProjectSequence{project_id,date}}`. `ProjectSequence` is what forces the directory-wide scan below. A per-node journal has no natural project sequence. |
| `writer.rs:816-846` | WRITE | `append_tx` — single append with request-id idempotency. |
| `writer.rs:871-904` | WRITE | `transaction` — N `FileRewrite`s (family B) + exactly one `TxAppend` (family A), one command, one idempotency slot. |
| `writer.rs:910-946` | WRITE | `transaction_multi` — N rewrites + N ordered tx entries, **still one ledger** (comment at `:907-909` states it). |
| `writer.rs:952-1021` | WRITE | `transaction_mutate_file` — read-modify-write of one file + one tx, run at the head of the serialized queue. |
| `writer.rs:1023-1055` | WRITE | `transaction_mutation` — adds an externally-visible `mutation_id`. |
| `writer.rs:1406-1449` | READ | `describe_command` — shutdown-loss diagnostics read `req.tx_path`/`req.entry.ty` to name what write is at risk. |
| `writer.rs:1551-1577` | WRITE | Writer loop `Tx` arm: batches consecutive appends, publishes `TxAppended`. |
| `writer.rs:1624-1691`, `:1693-1781`, `:1782-1837` | WRITE | `Transaction` / `TransactionMulti` / `TransactionMutate` arms: idempotency by `MutationIdentity`, `Durable` vs `SyncUncertain` caching. |
| `writer.rs:2095-2106` | WRITE | `publish_multi_events` — one `TxAppended{project_id, tx_id, ty}` per tx. **No node id.** |
| `writer.rs:2108-2151` | WRITE | `process_tx_batch` — the *only* path that already tolerates several distinct `tx_path`s in one batch; fsyncs per unique path. |
| `writer.rs:2225-2246` | WRITE | `tx_handles_detached_from_paths` — invalidates a cached handle whose path was renamed out from under it. Path-generic, survives. |
| `writer.rs:2248-2258` | WRITE | `prepare_tx_entry` — derives `tx_dir` from `req.tx_path.parent()`. |
| `writer.rs:2260-2297` | WRITE | `write_tx_append` / `sync_tx_writer`. |
| **`writer.rs:2299-2342`** | WRITE | **`append_txs_inner` — `bail!("multi transaction tx entries must target one ledger")`.** The hardest single constraint in the codebase for this split. |
| `writer.rs:2344-2364` | WRITE | `next_project_tx_id` — mints `tx-{date}-{slug}-{seq:04}`; caches by `(project_id, month)`. |
| `writer.rs:2366-2419` | READ | `scan_project_tx_max_seq` — reads **every** `.org` in `tx_dir`, parses each, takes the max seq for the slug. A full-directory scan, not a tail read. |
| `writer.rs:2421-2451` | — | `project_tx_slug` / `project_tx_sequence` — the tx-id parser: exactly 4 dash fields, 8-digit date, 4-digit seq. **Returns `None` on mismatch rather than erroring**, so an id-format change degrades silently into "sequence restarts at 1." |
| `writer.rs:2673-2839` | WRITE | The two-phase commit: write `.tmp.<request_id>` sidecars + fsync → back up targets to `.bak.<request_id>` + fsync → rename all tmp→target → **then** append tx → fsync tx → on tx failure, **roll back every renamed rewrite**. Family-B writes are provisionally durable before the family-A write lands. Rollback uses `sync_tx_writer(handles, &appended[0].tx_path)` — a single sync point. |
| `writer.rs:2841-2886` | WRITE | Sidecar naming per target file — already per-path, so per-task directories only multiply rewrite targets. |

### `index.rs` — 22 touchpoints, family A + B

| Line | Dir. | Mechanism / impact |
|---|---|---|
| `index.rs:20-26` | READ | Imports `parse_tx_file`, `TxEntry`, `iter_task_file_paths`, `goal_file_path` from core. |
| `index.rs:46-47` | READ | `ProjectIndex.activity_index: BTreeMap<TaskId, Vec<ActivityEntry>>` — the materialized per-task feed. |
| `index.rs:144+` | READ | `ActivityEntry { kind, actor, body, artifacts, in_reply_to, time, tx_id }`. |
| `index.rs:301-305` | READ | `TxRecord { project_id, source_path, entry }` — `source_path` ties an entry back to its monthly file. |
| `index.rs:307-322` | READ | **`IndexSnapshot.tx: Vec<TxRecord>`** — one flat in-memory list holding every tx entry from every project *and* home. The structural heart of "tx is a small number of shared ledgers." |
| `index.rs:2031-2048` | READ | Project refresh: `load_project`, then filter `next.tx` by `project_id`. |
| `index.rs:2071-2093` | READ | `RefreshSeed::HomeTx` — a **separate refresh target** driven only by the home tx directory watch. |
| `index.rs:2171-2216` | WRITE (mem) | `publish_refresh` Project arm: `retain` + `extend` + rebuild that project's `activity_index` under the publish lock. Comment at `:2200-2203` documents the race it guards. |
| `index.rs:2270-2277` | WRITE (mem) | `publish_refresh` HomeTx arm: `rebuild_all_activity_indexes` — **every project's activity index rebuilds on any home-tx write.** |
| `index.rs:2651-2734` | READ | **`load_project`** — the center of gravity. Reads `goal_file_path`, iterates `iter_task_file_paths` parsing every heading into `TaskSummary`+`TaskBody`, then `collect_tx_dir` on `<root>/.orgasmic/tx`, then builds `activity_index`. Both families in one function. |
| `index.rs:2792-2797` | READ | `load_home_tx` — same `collect_tx_dir` on `home.tx()`, `project_id: None`. |
| `index.rs:3163-3212` | READ | **`collect_tx_dir`** — walks every `*.org` in a directory, **non-recursively**, per-file error isolation (`ParseErrorKind::HistoricalTx`). Per-node journals live in subdirectories; this function would not find them without a recursion change. |
| `index.rs:3601-3606` | WRITE (mem) | `rebuild_all_activity_indexes`. |
| `index.rs:3793-3816` | READ | `build_activity_index(project_id, records)` — filters where `record.project_id == project_id` **OR** `entry.project == project_id` (two independent, divergable scoping sources), requires `entry.task.is_some()`, then **sorts by `(time, tx_id)`**. Good news: the projection does *not* depend on monthly-file chronology, only on `time` being sortable. |
| `index.rs:3818-3839` | READ | `activity_entry_from_tx` — the grammar contract: `ty` must be exactly `"comment"`, `"task.state_transitioned"`, or `"run."`-prefixed. **Anything else is silently dropped from the feed.** |
| `index.rs:3841-3864` | READ | `activity_body` / `extra_value` — reads `extra` keys `BODY`, `FROM_STATE`, `TO_STATE`, `ARTIFACTS`, `IN_REPLY_TO`. |

### `watcher.rs` — 8 touchpoints

`watcher.rs:4-6` (module doc: watches every registered project root plus the home tx dir),
`:121-129` (**explicit recursive watch registered on `home.tx()` as its own root** — no
equivalent target exists if tx moves per-node), `:292-299` (documents that macOS FSEvents
reports `backlog.org.<uuid>.tmp → backlog.org` as parent-dir events, so extension
filtering is impossible and classification must be by directory membership), `:302-341`
(`flush` canonicalizes `home.tx()` per flush, sets `touched_home_tx`), `:342-355` (any
path under a project root marks the **whole project** for refresh — no finer granularity),
`:408-413` (`schedule_home_tx_refresh`), `:414-432` (`schedule_watcher_refresh` +
`TaskUpdated{task_id: "*"}` — the wildcard confirms the daemon never names which task
changed from an FS event), `:445-693` (tests pinning the atomic-rename contract on
`tasks/backlog.org` specifically).

### `lib.rs`, `prompt_compiler.rs`, `events.rs`, `config.rs` — 9 touchpoints

- `lib.rs:1050`, `:1065` — WRITE — A — `default_tx_path = home.tx().join("YYYY-MM.org")` stored on `ApiState` as a **literal template string**.
- `lib.rs:1452-1455` — WRITE — A — `default_home_tx_path` — month rollover is implicit: a new file is chosen by wall-clock at write time, with no rollover event.
- `lib.rs:1721-1776` — test — A — pins the dispatch-finalize durability contract (`ty: "WorkerFinalize"` appended to a monthly path; a stalled write must be recorded as at-risk, not lost).
- `prompt_compiler.rs:690-712` — READ — A — the `tx_session_telemetry` context pack builds `<project>/.orgasmic/tx` and injects the newest file into a dispatched worker's prompt. Under per-node journals there is no single "latest project ledger" to point a project-scoped slot at; this pack must become task-scoped.
- `prompt_compiler.rs:777-786` — READ — A — `latest_org_file` sorts `*.org` names and takes the last. **Depends on `YYYY-MM.org` sorting chronologically.** A `journal.org` naming scheme makes this return an arbitrary file, silently.
- `events.rs:66-69` — schema — B — `TaskUpdated { project_id, task_id }` — already node-addressable, no change needed.
- `events.rs:70-74` — schema — A — `TxAppended { project_id, tx_id, ty }` — **not** node-addressable. A subscriber wanting "which task's journal changed" must round-trip the index.
- `config.rs:329` — schema — A — validates the `tx:` config block (`commit_to_project` only). The name binds the config surface to the monthly-ledger model.

### Daemon tests

`tests/tx_ledger_guard.rs:1-40` — the TASK-HQ970 gate: a multi-line `reason` must be
refused at the API boundary on **both** destinations (project ledger and home ledger),
with the file byte-identical afterwards. `tests/writer_durability.rs:58,87,443,495,504,536,553,592`
— eight `append_tx` durability scenarios. `tests/integration.rs`, `tests/duplicate_write.rs`,
`tests/dispatch_endpoint.rs`, `tests/manager_tier_endpoint.rs`, `tests/id_mint.rs`,
`tests/identity_lint.rs`, `tests/recovery_fault_restart.rs`, `tests/body_write_guard.rs`,
`tests/node_body_roundtrip.rs`, `tests/body_format_raw.rs` all seed or assert against
these paths.

### Zero-touchpoint daemon modules (verified)

`recovery_claim.rs`, `supervisor.rs` (14149 lines; its only `WriterHandle` use is
`SessionAppend`, never `append_tx`), `artifacts.rs`, `boot_state.rs`, `content.rs`,
`manager_registration.rs`, `addressing.rs`, `run_catalog.rs`, `run_history.rs`, `auth.rs`,
`authz.rs`, `driver_resolution.rs`, `logging.rs`, `ws.rs`, `runtime.rs`, `governance.rs`,
`test_fixtures.rs`.

---

## `crates/orgasmic-cli` — 38 production touchpoints (Critical)

The CLI writes bytes to these families in exactly **one** place. Everything else is either
an RPC to the daemon or a direct read off disk that bypasses the daemon.

### `manager.rs` — ~25 touchpoints

| Line | Dir. | Family | Mechanism / impact |
|---|---|---|---|
| **`manager.rs:9463-9498`** | READ | A | **`scan_dispatches`** — reconstructs "which dispatches are open" for the whole project by folding every tx entry in file order over `:TYPE:` (`manager.dispatch_started` opens; `run.created`/`*.reported` attach at `:9538-9723`; `implementer.done`/`reviewer.done`/`architector.done`/`manager.dispatch_aborted` close). It has **no per-task scoping on read** — it discovers which nodes are involved only after parsing. Under per-node journals this query has no entry point: you cannot ask "what is open" without first knowing every node that might have a journal. Multi-task dispatches (`task_list_property` joins several ids) make even "which node's journal gets this entry" ambiguous. |
| **`manager.rs:9513-9533`** | READ | A | **`read_tx_entries`** — `read_dir(.orgasmic/tx)`, sorted, `read_to_string` + `parse_tx_file` each, concatenated. The CLI's family-A primitive; every reader below calls it. |
| **`manager.rs:8211-8237`** | READ | B | **`read_task_lifecycle`** — iterates `iter_task_file_paths`, parses each file in turn until a heading's `:ID:` matches, and errors with `dotorg_tasks_dir(...).display()` if not found in any. The CLI's family-B primitive and the single biggest family-B break point: it answers a by-id question with a linear content scan of a fixed file list. |
| `manager.rs:2340` | **WRITE** | A | The one direct client-side write: `client.post_json("/tx", &tx_request)` inside `cmd_dispatch_finalize`, used only in the stall-sweep race where the daemon's release returned no `terminal_tx_id`. It has a task-id string but no node-journal addressing. |
| `manager.rs:1056`, `:1376`, `:1424-1429` | WRITE (RPC) | A + B | `cmd_dispatch_close` → `POST .../dispatch/close` — the one endpoint that atomically appends tx **and** moves the heading between stage files. Prints `closed: {} {} tx={}`. |
| `manager.rs:8579-8628`, `:8645` | READ + WRITE | A + B | `reconcile_torn_closes` — detects a close whose tx landed but whose lifecycle leg did not, by re-scanning the **entire** tx log, then re-POSTs the missing transition. Runs at the top of `cmd_dispatch`, `cmd_dispatch_close`, and `cmd_dispatch_status`. |
| `manager.rs:8874-8890` | READ | A | `reviewer_verdict_exists` — scans the whole tx log for a `reviewer.done` with a non-empty `:VERDICT:` before letting an implementer-done merge land on the default branch. **A safety gate implemented as a full-ledger scan.** |
| `manager.rs:884`, `:930`, `:955`, `:8263`, `:8276`, `:8496-8516` | READ + WRITE (RPC) | B | Dispatch lifecycle capture/apply/rollback — captures pre-dispatch stages so a failed dispatch can restore them. |
| `manager.rs:5679`, `:5731`, `:5759` | READ | B | `build_dispatch_plan` gates on lifecycle stage and reads the active goal id. |
| `manager.rs:8942-8956` | READ | B | `read_active_goal_id` — reads `.orgasmic/tasks/goal.org` directly, looks for `:STATUS: active`. |
| `manager.rs:2855`, `:2894`, `:3835-3841`, `:5440`, `:5533`, `:7736-7737`, `:8690-8725`, `:9352-9411`, `:9506-9511`, `:9813-9834` | READ | A (+B) | `dispatch-status` (prints script-parsed `TX_ID=… TASK=… KIND=…`), managed-worktree orphan detection, `worktree-prune` refusal, cleanup-status folds, legacy close replay, `resolve_close_target`. |

### `main.rs` — 8, `node.rs` — 2, `goal.rs` — 1, `doctor.rs` — 1, `verify.rs` — 1

- `main.rs:2298-2335` — WRITE (trigger) — A + B — `orgasmic project init` → `projects::init_project`; prints every scaffolded path.
- `main.rs:2589-2610` — READ (RPC) — A + B — `orgasmic reindex`.
- `main.rs:2893-2912` — — B — `TASK_LIFECYCLE_STAGES` hardcodes the six stage names (labels, not paths — survives the split).
- `main.rs:2973-3151` — READ + WRITE (RPC) — A + B — `orgasmic tasks list/count`, `orgasmic task create/get/update`.
- `main.rs:584-589` — contract — B — the `task update --state` help text **literally documents the mechanism**: "Rewrites the heading keyword, relocates the subtree to that stage's file and records a `task.state_transitioned` tx." This text becomes false.
- `main.rs:817-881`, `:3931-3997` — READ + WRITE — A — `orgasmic tx record/list`. `--tx-path` (`:869-870`) is an explicit escape hatch for naming a raw file. `List.project` help (`:874-875`) documents a multi-ledger whole-board scan. `:3990-3993` warns `[warn] tx coverage is {coverage}` off the `x-orgasmic-project-coverage` header — a user-visible admission that tx reads are a fallible multi-file scan.
- `node.rs:279-482`, `:547-567` — READ + WRITE (RPC) — A + B — the generic node editor; a second mutation path into family B alongside `task update`. The `{id, changed, tx_id}` contract is documented six times (`:72`, `:113`, `:147`, `:179`, `:220`, `:248`).
- `goal.rs:63-68`, `:74-132` — WRITE — A + B — `GoalMutationResponse` carries a **`tx_path: String` field printed verbatim as JSON** (`:122-129`). The only CLI output that hands an operator a literal tx file path.
- `doctor.rs:129-148` — READ (existence) — A + B — `REQUIRED_SHIPPED` names `schema/tx.org` and all 8 `project-scaffold/tasks/*.org`. This checks the **install templates**, not live project data, so it breaks only in lockstep with the scaffold.
- `verify.rs:34`, `:474-487` — — A + B — `DAEMON_OWNED_PREFIX = ".orgasmic/"` excludes both families from the clean-tree check by prefix. **Robust to the split with no change** — one of the few things that is.

### CLI tests — ~135 call sites through ~14 helpers

Concentrated almost entirely in `tests/dispatch.rs` (12,017 lines): `seed_project`
(`:294-330`, writes all 8 task files with hardcoded inline headings), `sprint_source`
(`:648-662`, concatenates all six stage files into one blob), `assert_task_stage`
(`:664-671`, substring-searches that blob — **called 41 times, never checks which file**),
`tx_file_name` (`:673-676`, derives `{YYYY-MM}.org` from `Utc::now()` with a comment
warning against hardcoding a month), `tx_log` (`:688-690`, **called 52 times**),
`tx_id_for` (`:693-720`, a hand-rolled tx parser independent of `parse_tx_file`),
`:802-804` (dry-run asserts `.orgasmic/tx` does not exist), `:8300-8341` (proves a
worker's git worktree never sees live daemon `.orgasmic/tx` writes — the mechanism behind
the known review-worktree blindspot).

Also: `tests/task_property_silent_drop_cli.rs:223-245` (`Fixture::drawer` does a
**non-recursive** `read_dir(.orgasmic/tasks)` — would not discover per-task subdirectories
as written), `:489-543` (asserts `tx record`'s default `tx_path` is under the project root,
not `$ORGASMIC_HOME`), `tests/task_title_edit_cli.rs:163-198` (callers must already know
the stage filename), `tests/reindex.rs:19`, `tests/manager_register.rs:69`,
`tests/manager_tier_cli.rs:79`, `tests/id_collision_repair.rs:24,57`,
`tests/common/mod.rs:33-59` (a duplicated mirror of `doctor.rs:129-148` — drift between
the two would let doctor tests pass against a shape doctor no longer checks).

### `crates/orgasmic-drivers` — 0 touchpoints

Verified: it references only `.orgasmic/tmp/dispatch/` and worktree paths for sandbox
path-allow policy (`adapters/cursor_acp.rs:1028`, `:1042`, `adapters/cursor.rs:823`,
`transcript_finder.rs:1557`). Siblings of these families, not members.

---

## `shipped/` — 29 touchpoints (High)

### The tx entry grammar as documented (`shipped/schema/tx.org`)

Required on every entry (`:9-14`): `TX_ID` (**documented as unique only within the file**,
not globally), `TIME`, `TYPE`, `ACTOR`, `MACHINE`. Type vocabulary (`:16-218`) is
namespaced: `project.*`, `task.*`, `comment`, `run.*`, `worker.*`, `manager.*`,
`template.applied`, `question.*`, `artifact.*`. Per-type extras — e.g. `comment` requires
`PROJECT` and `TASK`, optionally `RUN_ID`/`ARTIFACTS`/`IN_REPLY_TO`/`BODY` (escaped
markdown). The dispatch-generation chain (`:89-204`) cross-links
`manager.dispatch_started` → `*.reported` → `*.done` via `RUN_ID` and `CLOSED_TX`,
resolvable today **because everything lives in the same monthly files**. No explicit
ordering rule beyond append order / `TIME`. The "Property registry" section (`:255-256`)
is unfilled and cross-references `.orgasmic/tasks/<state>.org` — the schema doc itself
assumes the old layout.

**The load-bearing change: `TX_ID` uniqueness scope must go from file-local to global**
the moment one dispatch generation's entries can be scattered across several
`journal.org` files. `writer.rs:2366-2419` already scans the whole directory to mint ids,
so the implementation is stricter than the doc — but the doc is what agents read.

### Templates that would produce a broken scaffold

- `shipped/project-scaffold/tasks/{todo,in_progress,in_review,done,cancelled}.org` — the literal per-state stubs that the split eliminates.
- `shipped/project-scaffold/tasks/backlog.org:9,41,51,90,132` — five bootstrap task headings granting `WRITE_SCOPE: .orgasmic/tx/**`. That glob goes stale or overbroad.
- `shipped/project-scaffold/tasks/goal.org:23` — an acceptance criterion reading "activity record exists under `tx/`".

### Agent-facing prose that would misdirect a worker

- `shipped/skills/orgasmic/references/recall-resume.md:57` — instructs the manager to "read that task's heading in the correct state file (`tasks/backlog.org`, `tasks/in_progress.org`, …) by searching its ID." An agent following this verbatim fails.
- `shipped/skills/orgasmic/references/recall-resume.md:51-55` — instructs scanning `.orgasmic/tx/*.org` for `manager.dispatch_started` without a matching close.
- `shipped/entry/router.org:79-86` — the canonical project map, naming all 8 task files and `tx/`.
- `shipped/prompt-studio/context-packs/sprint_tasks.org:5` — `:FILE: .orgasmic/tasks/backlog.org` — a context pack that renders from one aggregate file. **Breaks**: there is no single file to point at. (`active_goal.org:5` and `manager_handoff.org:5` point at `goal.org`/`handoff.org`, which likely remain project singletons — lower risk.)
- `shipped/prompt-studio/conventions/manager-dispatch.org:377` ("Do not paste the raw report into `done.org`"), `:489-490`; `conventions/manager-handoff.org:17-25,58-60,68,74,78`; `prompt-specs/manager.org:38,41,45`; `context-packs/tx_session_telemetry.org:10-11`; `workflows/default.org:80`; `schema/state-machine.org:9-31,43-64` (the keyword state machine that maps 1:1 to files today); `skills/orgasmic/SKILL.md:36`; `skills/orgasmic/references/init.md:49,56,94,108`.

---

## `scripts/` + `verify/` — 11 touchpoints (Medium, but two silent failures)

- **`scripts/run-tests.sh:323`** — READ — B — `grep ... "$tasks/$stage.org"` for `stage in (done cancelled)` decides whether a flake-registry owner task is closed. Under per-task directories these files vanish, `grep` fails into `2>/dev/null`, and **the flake-lifecycle gate stops catching stale exemptions without reporting anything**. A correctness regression that presents as green.
- **`scripts/run-tests-selftest.sh:202-203`** — READ — B — `for f in "$tasks"/*.org` is a shallow glob; per-task subdirectory files never match, so `open_owner()` returns nothing and the self-test's fixture generator quietly produces nothing.
- `scripts/run-tests.sh:313-316` (existence guard), `:339,341` (recursive `grep -Eqr` — degrades gracefully), `scripts/run-tests-selftest.sh:206,211` (same direct-open pattern).
- `verify/README.md:12` — "One directory per task: `verify/TASK-<id>/`" — **useful prior art**: `verify/` already implements exactly the per-task-directory pattern dec_E01MC proposes, in a separate git-committed area. Unaffected itself.
- `verify/flake-registry.toml:29-30` — prose citing `.orgasmic/tasks/`.
- `verify/TASK-HXSW0-txscope/injection.patch:12` and `expect-red:4,10-13` — a **pinned failure proof** hardcoding the literal strings `tx/YYYY-MM.org` and `/home/state/tx/`. If tx path conventions or error messages change, this proof's replay trips its own FALSE-GREEN-GUARD and needs re-authoring, independently of whether the bug it guards still exists.

---

## `ui/` — 13 touchpoints (Medium; 12 consume projection, 1 does not)

Everything here consumes daemon API responses rather than files, **with one exception**.

- **`ui/src/components/OrgView.tsx:25-36,69,74-75,116`** — direct path passthrough — B — **the single highest-severity UI finding.** `ORG_FILES` hardcodes ten literal paths including `.orgasmic/tasks/{backlog,todo,in_progress,in_review,done,cancelled,goal}.org`, and the loader plus `handleSave` call `fetchOrgFile`/`postOrgFile` against whichever is selected. **Six of ten dropdown entries 404** once per-state files are gone. This is a shipped editor tab, not a fixture.
- `ui/src/lib/api.ts:144-148` `fetchTaskActivity` → `GET /tasks/:id/activity` — A — consumes projection; UI contract unaffected, daemon-side source moves.
- `ui/src/lib/api.ts:150-159` `postTaskComment` → `POST /tasks/:id/comments` — A — same.
- `ui/src/lib/api.ts:301-322` `fetchTx`/`fetchTxWithCoverage` → `GET /tx` — A — the **global** feed. The UI call shape is unchanged, but this is where daemon-side aggregation gets materially harder: merging many per-node `journal.org` files instead of reading one directory. The existing `x-orgasmic-project-coverage` "partial" signal already anticipates incomplete aggregation.
- `ui/src/lib/api.ts:365-367` `postTx` → `POST /tx` — A.
- `ui/src/lib/api.ts:369-421` — B — **two addressing modes**: `fetchOrgFile`/`postOrgFile` are **path-addressed** (`/org/file?path=…`, migration-fragile); `fetchOrgNode`/`postOrgNodeEdit`/`postOrgNodeDelete` are **id-addressed** (`/org/node?id=…&kind=…`, migration-safe by construction). Moving `OrgView` onto the id-addressed API is the fix.
- `ui/src/components/orgdoc/NodeDocEditor.tsx:176-178,249-253` — A + B — id-addressed; migration-transparent.
- `ui/src/components/TaskDialog.tsx:44,156-167,858` — A + B — endpoint-addressed.
- `ui/src/components/ActivityView.tsx:26,240` — A — backs the global activity page.
- `ui/src/components/TasksPage.tsx:538` — B — keys off `lifecycle_stage`, not source file. Unaffected.
- `ui/src/components/TaskView.tsx:15,26,32` — B — renders `task.data?.source_file` in a key/value row. Display-only.
- `ui/src/lib/capabilities.ts:38-39` — A — a comment stating "Activity reads the daemon tx log"; stale-but-harmless.
- `ui/src/components/__tests__/TasksPage.test.tsx:21` — B — `source_file: '.orgasmic/tasks/backlog.org'` literal, unasserted.

---

## `docs/` + root markdown — 6 touchpoints (Low)

`docs/agents/issue-tracker.md:3,11` (**does not currently exist** — see corrections),
`CONTRIBUTING.md:76-77`, `:80`, `:97` ("records the resulting merge commit in the tx log" —
the singular "the tx log" needs rewording), `AGENTS.md:7` (pointer to `.orgasmic/entry.org`
→ `router.org`). No hits in `DESIGN.md`, `PRODUCT.md`, `README.md`.

---

## Highest-risk couplings

### 1. `writer.rs:2299-2342` — the one-ledger invariant

`append_txs_inner` hard-fails with `bail!("multi transaction tx entries must target one
ledger")`, and the two-phase commit built on it (`writer.rs:2673-2839`) syncs and rolls
back against `appended[0].tx_path` — a single sync point. Every atomic operation the
system performs depends on this: `post_task_dispatch_close_commit` (`api.rs:17014-17018`)
commits a close tx, a `task.state_transitioned` tx, and both task-file rewrites in one
`transaction_multi`.

**Why it is the top risk:** a dispatch close is exactly the event that becomes multi-node
under the split — one close touching several tasks (multi-task dispatch, per
`manager.rs:9463-9498`) would need to append to several `journal.org` files in a single
atomic unit, which the writer refuses by design. This is not a path change; it is a
redesign of the transaction primitive, including rollback bookkeeping across N sync
points. `process_tx_batch` (`writer.rs:2108-2151`) already tolerates multiple paths and
is the closest existing model to build from.

### 2. `manager.rs:9463-9533` + `api.rs:3452-3555` — dispatch state has no index

"Which dispatches are open?" is answered by reading **every** file in the tx directory and
folding it in order — in the CLI (`read_tx_entries` → `scan_dispatches`) and independently
in the daemon (`project_tx_entries` → `dispatch_generation_ledgers`, re-run on **every**
`dispatch/wait` poll at `api.rs:3635`). Three safety gates hang off these folds:
`reviewer_verdict_exists` blocks a merge to the default branch (`manager.rs:8874-8890`),
`reconcile_torn_closes` repairs half-committed closes (`manager.rs:8579-8628`), and
`worktree_prune` refuses to delete a worktree owned by an open dispatch
(`manager.rs:5533`).

**Why it is high risk:** the query is inherently unscoped — you cannot ask it per-node
because discovering which nodes are involved *is* the query. Per-node journals give it no
entry point. Whatever replaces it must be a real index or a project-level dispatch ledger
that stays in `tx/`, and it must land before the journals split, not after, or three
safety gates degrade to false-green simultaneously.

### 3. `paths.rs:64-68` `iter_task_file_paths` — by-id questions answered by full scans

Six production call sites treat "find task X" as "linearly parse six known files":
`index.rs:2651-2734` (project load), `api.rs:15286` (inbound-reference check before
delete), `manager.rs:8211-8237` (`read_task_lifecycle`), and three lint entry points in
`identity_lint.rs:219,253,393`.

**Why it is high risk:** the identity/reference lint is what makes `parse_errors 0` and
dangling-edge detection meaningful. If those three scans quietly stop covering task nodes
under per-task directories, the lint reports green over an unlinted corpus. Two shell
gates fail the same way — `run-tests.sh:323` and `run-tests-selftest.sh:202-203` both use
shallow globs or direct filename opens that return empty rather than erroring. That is
four independent silent-degradation paths from one primitive.

### 4. `api.rs:7889-7893` `tx_destination` — no node parameter

The single function that builds a project tx path branches only on project-versus-home. It
has no `task_id`. Every family-A write in the daemon — roughly 15 call sites plus ~12
recovery writers at `api.rs:17600-19010` — routes through it or through
`record_api_tx`/`append_tx_request`.

**Why it is high risk:** it is simultaneously the best seam and a hard cutover. Because
all writes funnel through one resolver, adding node awareness is one signature change —
but every caller must supply a node id in the same commit, and the recovery/reattach block
would need the owning task threaded through call sites that currently only carry a run id
(`api.rs:7542-7575` checks idempotency by scanning the project-wide ledger; per-node it
would not know which journal to check).

### 5. `index.rs:307-322` + `:3163-3212` — the flat snapshot and the non-recursive scan

`IndexSnapshot.tx: Vec<TxRecord>` is one flat list of every entry from every project plus
home, and `collect_tx_dir` populates it with a **non-recursive** directory walk. The whole
dual-refresh machinery (`RefreshSeed::Project` vs `RefreshSeed::HomeTx`,
`index.rs:2031-2093`, `:2171-2277`) exists solely because tx lives in a small number of
shared directories — including a fan-out where any home-tx write rebuilds **every**
project's activity index (`index.rs:2270-2277`).

**Why it is high risk:** per-node journals live in subdirectories that `collect_tx_dir`
will not descend into, so the failure mode is an empty activity feed rather than an error.
The good news, and the one genuinely favourable finding in this inventory:
`build_activity_index` sorts by `(time, tx_id)` (`index.rs:3812`), **not** by file or
append order. The projection therefore has no dependency on monthly-file chronology, and
per-task activity reads become strictly simpler — read one node's journal instead of
filtering a cross-project list. The refresh-target machinery is the part that becomes dead
weight, not the projection logic.

---

## Two smaller items worth deciding early

**The unscoped org-file endpoint.** `validate_org_edit_path` (`api.rs:13559-13577`) admits
any `.org` file under `.orgasmic/`, which today includes monthly tx files and will
tomorrow include per-node journals. `post_org_file` (`:13505`) can therefore overwrite an
append-only ledger through a generic endpoint. Add the denylist as part of the migration.

**Silent parsers.** Two functions degrade rather than fail when their assumptions break:
`project_tx_sequence` (`writer.rs:2421-2451`) returns `None` on an unrecognized tx id, so
an id-format change restarts sequences at 1; and `latest_org_file`
(`prompt_compiler.rs:777-786`) picks the lexicographically-last filename, so a naming
change hands a worker an arbitrary ledger. Both deserve an explicit error path before the
schema moves.
