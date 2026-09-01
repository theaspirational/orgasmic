# Chain review — dec_E01MC / TASK-TWCP9 (17 tasks, `543782d7^1..31344134`)

First independent whole-chain review. 15 findings filed as `reviewer.finding` tx
against their owning chain task as they were found.

## Verdict

**APPROVE WITH FOLLOW-UPS.**

The substrate itself is sound. The node kernel is small, pure, and correct; the
tx-split routing table is pinned by a real test; the migrator verifies a
byte-for-byte heading round trip and refuses on every anomaly class before it
writes; `assert_body_round_trip` makes a column-0 `* ` in a node body
unwritable; the dispatch close stays one tx append plus N node rewrites with the
renames ordered *before* the append, so a crash leaves "nodes flipped, no close
tx", which the fold reads as still-open and a re-close resolves as a no-op. That
is the right failure direction and I could not break it.

What I would not ship unchanged is the **distribution half** — MSYN4/CLM6W. Six
HIGH findings sit there and in what the split did to the repair paths. None of
them can lose a single-machine ledger today (this machine is the only writer,
and the six HIGHs are individually repairable), which is why this is not a
REJECT. But every one of them is a second-machine-day-one defect, and one
(#1) is exploitable by any authenticated principal right now.

Fix #1, #2 and #4 before a second machine ever joins. #3 is live today.

## Findings

### HIGH

**H1 — `reject_ledger_rewrite` no longer covers the tx ledger (MSYN4)**
`crates/orgasmic-daemon/src/api.rs:14575`
MSYN4 moved the authoritative tx ledger to `machines/<uuid>/tx/`, but the
denylist still only matches `.orgasmic/tx*` and `**/journal.org`. Replaying the
exact predicate against Rust's component-wise `Path::starts_with`:

    .orgasmic/machines/<uuid>/tx/2026-09.org   blocked=false
    .orgasmic/machines/<uuid>/claims.org       blocked=false
    .orgasmic/views/board.org                  blocked=false
    .orgasmic/tx/2026-09.org                   blocked=true
    .orgasmic/tasks/TASK-X/journal.org         blocked=true

`validate_org_edit_path` allows any `.org` under `.orgasmic/`, and
`guard_node_write` (writer.rs:1752) explicitly allowlists `machines | tx | tmp |
views`, so nothing downstream stops it either. `POST /org/file` therefore
whole-file overwrites the append-only dispatch ledger, forges or erases the
cross-machine claim log, and writes `views/` — which AP971.8 decision 3 says is
"never a write target". `post_org_file` also carries no `Extension(identity)`
and no `Action` check (pre-existing), so the lowest role reaches it. This is
exactly the class TASK-HQ970 built this denylist to close, reopened by the move.

**H2 — the sync loop races the writer and has already committed torn state (MSYN4)**
`crates/orgasmic-daemon/src/ledger_sync.rs:56`
`sync_once` takes no writer lease and `git add --all -- .orgasmic` sweeps the
whole tree. It has already caught the writer mid-transaction on the live ledger:

    cd544977  A .orgasmic/tasks/TASK-JHWNP.1/node.org.bak.53cd3fda-…   (d13cac05 deletes it)
    8f937138  A .orgasmic/artifacts/ART-MKRG1/node.org.bak.fd0f75a5-… (1c2e9fe3 deletes it)

Those are `transaction_backup_path` sidecars. The same window spans
`transaction_multi_locked_inner`'s rename loop (writer.rs:3478), so a dispatch
close can be committed and pushed with some `node.org` files rewritten and the
close tx not yet appended — another machine then pulls a half-applied close.
Needs a writer lease, or at minimum a pathspec excluding `*.tmp.*` / `*.bak.*`.

**H3 — the tx split disarmed the torn-close "operator moved it on purpose" guard (EPG6H, RGRD5)**
`crates/orgasmic-cli/src/manager.rs:9793`, `crates/orgasmic-daemon/src/api.rs:18497`
Both repair paths clear/deny on a later `task.state_transitioned`, and both read
only `tx/` and `machines/*/tx/` (`read_tx_entries` manager.rs:9822,
`project_tx_entries` api.rs:3773). EPG6H routes every `task.state_transitioned`
to the node journal. Measured on the live ledger:

    journals carrying task.state_transitioned : 50
    machines/*/tx carrying it                 : 0

So the arm is provably dead. `in_review>in_progress` is a legal transition in
the shipped descriptor, so: a dispatch closes a task `in_progress -> in_review`;
the operator moves it back to `in_progress` for rework; the next manager command
sees a close tx that is the last lifecycle event with the task sitting at
`LIFECYCLE_FROM`, calls it torn, and drags it to `in_review` again. The daemon
side is worse — `repair_allowed` also skips the `Done` evidence gate
(api.rs:18398). `manager.dispatch_started` still clears the candidate, which is
the only thing narrowing this.

**H4 — `views/` is not gitignored on the live ledger (JWHXH)**
`crates/orgasmic-daemon/src/ledger_sync.rs:56`
AP971.8 decision 3 was implemented for *new* projects only
(`shipped/project-scaffold/.gitignore` gained `views/`). The existing ledger's
`.orgasmic/.gitignore` still holds just `tmp/`, and `git ls-files` confirms
`views/{board,glossary,decisions}.org` are all tracked and re-committed by the
2s sync tick. Every node write is the two-file diff the decision existed to
avoid, and two machines regenerating a throwaway file guarantees the rebase
conflict of H5/H6. `shipped/entry/router.org:84` now tells agents these files
are gitignored; they are not.

**H5 — a rebase conflict wedges the sync loop silently and forever (MSYN4)**
`crates/orgasmic-daemon/src/ledger_sync.rs:96`
`pull --rebase` failure aborts and `bail!`s. The next tick repeats it
identically, local commits pile up unpushed, and the only surface is
`tracing::warn!` — no status field, no CLI line, no rejected-event location.
MSYN4's acceptance "rejected events land in an inspectable location with a
reason" is unmet for the whole-sync failure case, which is the one that matters.

**H6 — the claim gate only refuses *claimed* nodes (CLM6W)**
`crates/orgasmic-daemon/src/writer.rs:1766`
`guard_node_write` returns `Ok` for any node with no claim row. Claims are taken
and released per dispatch — the live log shows 62 `task.claimed` / 70
`task.claim_released` — so every node is unclaimed between dispatches, and a
comment or a task update from two daemons both land. That contradicts
ledger_sync.rs:52's stated premise ("a foreign node dir can only appear modified
here if something wrote outside its pen, which the claim gate refuses"), and the
consequence is H5.

### MEDIUM

**M1 — `views/` go stale after every incremental write (8AV8B, JWHXH)**
`crates/orgasmic-daemon/src/index.rs:2970`
`build_views` is reachable only from `load_project` (boot / project add /
reindex) and from the `machines/*/claims.org` branch of `apply_written_path`
(index.rs:920). `reload_node_dir` never calls it, and the watcher now routes
every project path through `apply_written_path` instead of
`schedule_watcher_refresh`. AP971.8 decision 2 ("generated after each affected
refresh, debounced") is unmet. On this repo the views look fresh only because
dispatch claim churn rewrites `claims.org` constantly; I diffed all 789 board
entries against the node dirs and found 0 drift *right now*. In a project
without dispatches they never regenerate after boot — and the prompt-studio
context-packs were repointed at exactly these files, so a worker's compiled
prompt would carry a stale glossary and decisions list as fact.

**M2 — TP593's `real_data` acceptance is vacuous (TP593)**
`crates/orgasmic-core/src/node_kernel.rs:429`
`every_migrated_node_parses` returns `Ok` immediately unless
`ORGASMIC_MIGRATED_DIR` is set; grep over `*.rs *.sh *.toml *.yml *.md *.org`
finds it set nowhere, so `assert!(n > 800)` has never run in any gate. The doc
comment also points at `scripts/ap971-migrate-proto.py`, which is not in the
tree (the migrator is Rust now).

**M3 — seven live-corpus tests are permanent silent passes (LBRX7)**
`crates/orgasmic-core/tests/fixtures.rs:37`
`live_ledger_present()` returns false whenever `.orgasmic/project.org` is
absent, which post-cutover is every fresh clone and every CI checkout
(`/.orgasmic/` is gitignored on `main`). Reproduced here:

    cargo test -p orgasmic-core --test fixtures        → 19 passed in 0.00s
    …-- --nocapture | grep -c "skipping: no live"      → 7

`parses_real_done_tasks`, `live_state_files_parse_without_retired_property_warnings`,
`parses_real_decisions`, `parses_real_glossary`, `parses_real_project` and two
more assert nothing in the gate that certifies releases.

**M4 — anyone who can comment can destroy anyone's comment (KA934)**
`crates/orgasmic-daemon/src/api.rs:2386` / `:2421`
Both handlers gate only on `Action::TasksComment`, which `authz.rs:77` grants to
the lowest role (`viewer`), and neither checks authorship. A viewer can rewrite
or tombstone a `reviewer.finding` or `review.verdict`. The UI hiding Edit/Delete
on automated rows (TaskDialog.tsx) is cosmetic. Worse for an append-only system:
`tombstone_comment` (node_kernel.rs:291) rewrites `TYPE` and drops the body
without recording actor or time, and `edit_comment_body` stamps only
`:EDITED_AT:`, never `:EDITED_BY:` — the journal cannot say who destroyed what.

**M5 — cross-machine tx id collisions (MSYN4)**
`crates/orgasmic-daemon/src/writer.rs:2959`
`tx-{date}-{slug}-{seq:04}` has no machine component and the sequence is
per-project, so two daemons minting concurrently produce identical `TX_ID`s for
different events. `EVENT_ID` keeps them from being deduped away, but the
dispatch fold identifies generations *by TX_ID* — `close_dispatch` matches
`CLOSED_TX` against `started.tx_id` (tx.rs:220), `attach_initial_run` matches
`DISPATCH_TX`, `recorded_close_allows_repair` matches `CLOSED_TX`. A collision
mis-attributes a close to the wrong dispatch. Separately,
`next_project_tx_id` serves from an in-memory `project_max` invalidated only
when a tx file's inode changes (`tx_handles_detached_from_paths`), so a pull
bringing higher remote sequences without touching this machine's own month file
remints ids that already exist.

### LOW

- **L1 (SRBGS)** `identity_lint.rs:218,225,245` — `unwrap_or_default()` swallows
  real IO errors (NotFound is already `Ok(vec![])` inside the helper), so an
  unreadable collection makes the id-collision and dangling-reference lints
  report clean.
- **L2 (SRBGS)** `project_migrate.rs:345` — `apply()` is not atomic and has no
  recovery path. A partial failure leaves node dirs behind; `plan()` then bails
  "migration target already exists" and `refuse_dirty_tree()` bails on the dirty
  tree, so the verb refuses forever and the operator must know to
  `git checkout`/`git clean` by hand. No test covers partial failure.
- **L3 (8AV8B)** `api.rs:8570` — `take_apply_failure()` is a single daemon-wide
  slot, so one request's projection failure surfaces as a committed-503 on the
  next unrelated request, and that early return skips `repair_projection`.
- **L4 (GCXB7)** `shipped/schema/tx.org:101` — the "complete routed type set"
  added for AP971.11 item 3 omits `fixer.done` and
  `implementer.commit_pending`, both of which the Rust pin test routes to `tx/`,
  and nothing tests the shipped list against `event_routes_to_journal`.
- **L5 (SRBGS)** `project_migrate.rs:86` — `println!("  anomalies 0")` is an
  unconditional literal. Not a lie (every anomaly class `bail!`s earlier) but
  the line carries no information.

## The ten claims: settled vs open

| # | claim | verdict |
|---|---|---|
| 1 | writer atomicity / RGRD5 repair | **half settled.** The crash story is sound (renames before append; a torn close reads as open and re-closes idempotently). RGRD5's repair is **not** sound — H3. |
| 2 | tx split | **settled for routing** (the pin test is real and passes), **open for readers** — H3 is a reader that still expects the old aggregate surface. |
| 3 | migrator | **settled** for idempotence, `mkdir` collision, done/cancelled coverage and the round trip (all verified in `plan`); **open** for partial failure — L2. I did not run the migrator against a scratch pre-cutover worktree. |
| 4 | claims / fold | **open** — H6. The type-set guard *is* total over the listed types (test passes). The 409 is reachable from every writer entry point (`guard_node_paths` is called by all seven mutating methods). |
| 5 | sync loop | **open** — H2, H5, M5. AS0FS's shape is confirmed and is broader than filed: it covers unclaimed *node dirs*, not just singletons. |
| 6 | incremental refresh | **`c31639c9` is not the only one** — M1 and L3. |
| 7 | descriptor guard bypasses | **settled.** Exactly one bypass exists (`repair_allowed`, api.rs:18375), reachable only via `repair_closed_tx` from `reconcile_torn_closes`. It is legitimate in shape; its *precondition* is broken — H3. |
| 8 | deleted readers | **settled.** The legacy path helpers (`TASK_FILE_NAMES`, `DEFAULT_TASK_FILE_REL`, `lifecycle_stage_file_name`) survive but have no non-test callers. Only L1 silently reports empty. |
| 9 | UI | **settled.** No `dangerouslySetInnerHTML` on the new surfaces; React escapes comment bodies; `threadActivity` is cycle-safe (`seen` guard); everything writes through the daemon API. The one real issue is server-side — M4. |
| 10 | test honesty | **open** — M2, M3. The ported tests I read are honest. |

### Tests I read (claim 10)

Honest ports, assertions preserved, helpers adapted rather than weakened:
`duplicate_write.rs` (`read_collection`, `count_glossary_headings`,
`count_task_id`, `node_delete_rejects_inbound_references_and_tasks`,
`org_node_delete_requires_occ_records_tx_and_survives_reindex`),
`node_property_silent_drop_cli.rs` (`drawer`, `graph_file`,
`seed_drawer_lines`), `identity_lint.rs`, `watcher.rs` (5 fixture tests —
these now exercise the *new* incremental path, which is a strengthening),
`project_migrate.rs::migration_is_verbatim_dry_run_and_idempotent` and
`::branch_cutover_is_orphan_dry_run_idempotent_and_worker_discoverable`,
`ledger_sync.rs::a_node_written_under_a_released_claim_still_reaches_the_remote`.
Dishonest: the seven in `fixtures.rs` (M3) and `node_kernel.rs::real_data` (M2).

## Verification notes

Read-only throughout. No file edits, no git writes, no mutating `orgasmic`
verbs. The only writes were the 15 `reviewer.finding` tx the brief mandates.

Commands run:

- `cargo test -p orgasmic-core --test fixtures` → 19 passed, 0.00s;
  `-- --nocapture | grep -c "skipping"` → 7. (M3, reproduced.)
- `cargo test -p orgasmic-core node_kernel` → 4 passed (3 real + the M2 no-op).
- `cargo test -p orgasmic-core node_type|views|claims` → 1 test each.
- `cargo test -p orgasmic-daemon --lib ap971_every_known_event_type_has_a_pinned_ledger_route`
  → passed.
- Live ledger, read-only: `git ls-files .orgasmic/views/` (H4);
  `git log --all --name-only -- .orgasmic | grep -E '\.(tmp|bak)\.'` (H2);
  `grep -rl "TYPE:    task.state_transitioned"` across journals vs
  `machines/*/tx` (H3); claim-type counts (H6); a python diff of all 789
  `views/board.org` entries against `tasks/*/node.org` states (M1, 0 drift now).
- H1's predicate replayed in a throwaway `rustc` program in `/tmp` (deleted
  after) to confirm `Path::starts_with` component semantics rather than infer
  them.
- Live daemon as a read oracle: `orgasmic decision get dec_E01MC`,
  `orgasmic task get TASK-AP971.{2,3,5,6,7,8,9,10,11}`, `orgasmic status`.

## What I did NOT check

- **The migrator against a scratch pre-cutover tree.** The brief offered
  `git worktree add <tmp> 543782d7^1` + migrate the copy. I read `plan`/`apply`
  line by line instead and verified the round-trip and anomaly checks are real
  code that `bail!`s. The "873 nodes, 0 anomalies" claim on *this* repository is
  therefore unreproduced by me; L2 is the gap I would test first.
- **Two real daemons against a real remote.** H2/H5/H6/M5 are read from the code
  plus live-ledger forensics, not from a running two-machine harness. H2 is
  nonetheless confirmed by artefacts already committed.
- **`crates/orgasmic-daemon/src/api.rs` in full** (41k lines). I read the
  routing, tx preparation, refresh, transition-guard, comment, regenerate and
  org-file surfaces, and grepped the rest for the specific claims.
- **`verify/*/injection.patch`** — deliberately not read (TASK-52NJS).
- **`crates/orgasmic-cli` test suites** — not run (brief prohibits the
  whole-crate command; the chain's own gates already cover them).
- **ARRV8's artifact port and AN992's CLI verbs** beyond reading the migrator's
  artifact arm, `node.rs`, and the regenerate close-out. I traced
  `close_out_node_regenerate_round` (api.rs:20119), which rewrites `journal.org`
  *and* appends to it in one `transaction` — safe only because the renames
  precede `append_txs_inner` and `tx_handles_detached_from_paths` reopens the
  stale handle. Correct, but fragile enough to be worth a comment.

## Fix directions

1. **H1** — make the denylist structural, not prefix-matched: refuse any path
   under `.orgasmic/machines/`, any `.orgasmic/views/`, plus the existing
   `tx/` and `journal.org` rules. One predicate, and pin it with the four cases
   above.
2. **H2** — give `sync_once` the writer lease that already exists
   (`with_detached_session_lease` is the shape), or fall back to
   `git add --all -- .orgasmic ':(exclude)*.tmp.*' ':(exclude)*.bak.*'` and
   accept the torn-close window as a known ceiling.
3. **H3** — teach `read_tx_entries` and `project_tx_entries` to fold node
   journals for `task.state_transitioned`, or drop the ledger-derived
   "already-transitioned" test entirely and rely on the `from_state ==
   LIFECYCLE_FROM` check plus an explicit operator-intent marker. Do not leave a
   guard arm that reads a surface the event no longer lands on.
4. **H4** — append `views/` to the live `.orgasmic/.gitignore` and
   `git rm --cached` the three files. One commit; unblocks H5/H6's worst case.
5. **H5** — surface the last sync outcome in `orgasmic status` and stop
   retrying an identical failing rebase silently.
6. **H6** — either claim on first write and hold until sync (making the pen
   real), or drop the "two machines never write the same node" claim from the
   design and give the sync loop a conflict path. Half of each is what is
   shipped.
7. **M1** — call `build_views` (debounced) at the tail of `reload_node_dir`, not
   only from `load_project`.
8. **M2/M3** — either wire the corpus into the gate (a committed fixture tree,
   or CI checking out the `orgasmic` branch beside `shipped/`) or delete the
   tests. A test that cannot fail is worse than no test, because it is counted.
9. **M4** — add an authorship check to comment edit/delete, and record the
   actor and time on both the edit and the tombstone.
10. **M5** — put the machine id in the tx id, or scope the sequence per machine.

Would I ship this as the storage substrate every future ledger sits on? Yes,
with the follow-ups above — the kernel and the split are the right shapes and
the single-machine path is solid. The distribution layer is a prototype wearing
a production interface, and H1 through H6 are the price of that.

**APPROVE WITH FOLLOW-UPS.**
