# Review: TASK-SRBGS.1 — chain-review L1–L5 (commit `79caf335`, merged `c56b0bbe`)

## Verdict

**APPROVE WITH FOLLOW-UPS.**

Nothing here is a shipped defect or a regression. L1, L2 (non-branch path) and L3
are correct and genuinely fix what they claim. L4's *documentation* is now right
but its test cannot detect the drift it was filed to prevent, and L5 changed the
source text without changing the behaviour — the migrator still prints `anomalies 0`
and always will. Both are follow-up material, not blockers.

## Findings

### F1 — MEDIUM (test) `crates/orgasmic-daemon/src/api.rs:24292`
**The L4 pin test is a tautology. It does not compare the shipped list to the route table.**

```rust
let rust = DISPATCH_PROJECT_TX_TYPES
    .iter()
    .copied()
    .filter(|ty| !event_routes_to_journal(ty))   // <- never removes anything
    .collect::<BTreeSet<_>>();
assert_eq!(shipped, rust);
```

`event_routes_to_journal` (api.rs:8782-8784) begins:

```rust
if ty.ends_with(".deleted") || DISPATCH_PROJECT_TX_TYPES.contains(&ty) {
    return false;
}
```

So for every element of `DISPATCH_PROJECT_TX_TYPES` the function returns `false`
by construction, the filter predicate is `true` for all twelve, and `rust` is
just `DISPATCH_PROJECT_TX_TYPES` re-collected. The test pins the shipped doc
against **a new hand-maintained constant** — exactly the second source of truth
the L4 finding was about — not against routing behaviour.

Two consequences:

1. **The constant is behaviourally dead.** All twelve members already returned
   `false` before this commit, by absence from the `matches!` arm at api.rs:8786-8805.
   The added short-circuit changes no routing; it exists only to give the test
   something to compare against itself.
2. **The drift the test was written to catch is still invisible.** Failure
   scenario: someone adds `fixer.reported`. It routes to project `tx/` (absence
   from the `matches!` arm), they forget both `shipped/schema/tx.org` and
   `DISPATCH_PROJECT_TX_TYPES`. `shipped == rust` still holds, the test is green,
   and the "complete routed type set" is incomplete again — the original L4 bug,
   reproduced.

The acceptance line *"L4 list matches the Rust route table with a test"* is met
in letter (a test compares the list to a Rust set) and not in substance (that set
is not the route table).

**Fix direction:** derive the Rust side from behaviour, not from a parallel list.
The exhaustive `(type, routes_to_journal)` pin table already at api.rs:~24150-24240
is the real route inventory — assert that every entry in it whose value is `false`
is either present in the shipped bullet block or in a small, explicitly-named
non-dispatch exclusion set (`ledger.sync_conflict`, `manager.action`, `*.deleted`, …).
Then a new type forces someone to make a decision instead of silently passing.
Delete the `DISPATCH_PROJECT_TX_TYPES` short-circuit at the same time; it is dead.

### F2 — LOW (correctness) `crates/orgasmic-cli/src/project_migrate.rs:89`
**`anomalies` can still only ever print `0`. The literal moved into a variable; the output did not change.**

```rust
// project_migrate.rs:204-208, inside plan()
if reassembled != file.source() {
    migration.anomalies += 1;
    bail!("byte-for-byte heading round trip failed: {}", source_path.display());
}
```

The only increment is immediately followed by `bail!`. `run_at` starts with
`let migration = plan(root)?;` (line 62), so any migration whose count would be
non-zero returns `Err` and never reaches `println!("  anomalies {}", ...)` at
line 89. The field is structurally `0` at its single read site.

L5 asked for "prints the counted value". It does — and the counted value is
provably always zero, which is what the original finding objected to. No test
asserts a non-zero anomalies line, and none can be written against the current
control flow.

**Fix direction:** pick one. Either count-and-continue (collect the anomalous
paths through the whole scan, print `anomalies {n}`, then bail listing them —
strictly more useful to an operator, who currently learns about one bad file per
run), or delete the line and say in the output that a round-trip failure aborts.

### F3 — LOW (design) `crates/orgasmic-daemon/src/writer.rs:1421`
**`mutate_file` keys its apply failure by a UUID no caller can ever hold.**

```rust
pub async fn mutate_file(&self, req: FileMutate) -> Result<()> {
    let written_path = req.path.clone();
    let apply_owner = Uuid::new_v4().to_string();   // minted here, never returned
    ...
    self.publish_paths(&apply_owner, [written_path]).await
}
```

`take_apply_failure(owner)` is only ever called with a tx id (api.rs:8592, 8637),
so an entry inserted under this UUID is unreachable. Every production journal
write goes through here — `append_journal_entry` (writer.rs:1444),
`edit_journal_comment` (1471), `tombstone_journal_comment` (1498). Effects:

- The entry is cleared only by a later **successful** `repair_projection()`
  (writer.rs:926), which clears the whole map.
- Until then, every subsequent `take_apply_failure` hits the `foreign_owner`
  branch (writer.rs:894-900) and logs `"projection failure belongs to another
  request; attempting repair"` — a warn about an owner that does not exist.

**Not a data hole.** The failing path is queued on `unapplied` (writer.rs:874-878),
and the comment routes do a full `refresh_project` right after the write
(api.rs:2418, 2458), so the projection is reconciled regardless. This is a
correctness-of-bookkeeping finding, not a live bug.

**Fix direction:** pass the request id, or make the owner `Option<&str>` and skip
the map insert when there is no claimant (queue-only). Either kills the phantom
key and the misleading warn.

### F4 — LOW (docs/UX) `crates/orgasmic-cli/src/project_migrate.rs:71`
**The branch cutover — the more destructive path — got no recovery text.**

`run_at` wraps only the non-branch arm:

```rust
if to_branch && !dry_run && ... {
    migrate_to_branch(home, root, &migration)?;   // unwrapped
} else if ...
} else if !dry_run {
    apply_with_recovery(root, &migration)?;       // wrapped
}
```

`migrate_to_branch` does `create_orphan_branch`, `git worktree add`, and finally
`std::fs::remove_dir_all(root.join(".orgasmic"))`. A failure partway leaves a
half-cutover, and re-running then dies on `refuse_dirty_tree` or on
`"ledger target already exists but is incomplete"` — with nothing printed telling
the operator how to get back. That is the same "the verb refuses forever" dead
end L2 named, on the sibling path.

Worth stating explicitly: **the L2 wrapper's text would be wrong advice here.**
`git checkout -- .orgasmic` / `clean -fd -- .orgasmic` does not undo an orphan
branch or a registered worktree. This needs its own message, not the same wrapper.

### F5 — LOW (test) `crates/orgasmic-daemon/src/api.rs:32874`
**The new L3 test proves only half the contract.**

`apply_failure_is_not_reported_by_the_next_request` drives the first request with
`state.writer.append_tx(first.tx, None)` — the raw writer call, bypassing
`refresh_after_tx` entirely. So request A never takes its own failure, and the
test asserts only that request B escapes it. It does not show the original
failure is still reported to its owner.

That contract *is* covered, but incidentally, by the pre-existing
`cached_task_retry_after_committed_503_repairs_projection` (api.rs:32735), which
asserts the 503, the tx id, and the stale projection. I ran it — see Verification.
So this is a coverage gap in the new test, not an uncovered behaviour.

## Open Questions

1. **F1 fix scope.** Making the L4 test honest means reworking it against the
   exhaustive pin table, and that table contains many `false` types that are *not*
   dispatch lifecycle (`manager.action`, `manager.correction`, `ledger.sync_conflict`,
   `task.claimed`, `*.deleted`). Exact set equality will not work; someone has to
   decide what "the complete routed type set" in the shipped doc is scoped to
   before the test can be written correctly. That is a small design call, not a
   mechanical fix.
2. **F2 preference.** Count-and-continue is more useful but changes `plan()`'s
   failure semantics (currently fail-fast on the first bad file). Is the migrator
   allowed to survey the whole tree before refusing?
3. `ledger_sync.rs:783` re-derives the expected tx path with the same
   `.join("tx").join(format!("{}.org", ...))` expression it is testing. Was a
   literal `machines/<id>/tx/<YYYY-MM>.org` assertion considered? Low value given
   the api pin test asserts the routing separately, so I did not file it.

## Verification Notes

**Ran (targeted, on this worktree at merged `c56b0bbe`):**

- `cargo test -p orgasmic-daemon --lib -- shipped_tx_types apply_failure` → 2 passed.
  Both new tests green — which is F1's point: the L4 test passes and cannot fail
  for the reason it was written.
- `cargo test -p orgasmic-core --lib identity_lint` → 11 passed, 1 ignored
  (`real_post_migration_repo_lint_probe`, a manual live-checkout probe).
  `unreadable_collection_is_an_error_not_a_clean_lint` passes.

**Read and confirmed sound — not findings:**

- **L1 fail-closed scope is safe.** Both daemon lint entry points degrade rather
  than fail: `lint_project_identity_state` (index.rs:3841) and
  `lint_cross_project_references` (index.rs:3510) push a parse error into the
  snapshot and continue. Neither turns an index refresh or a project load into a
  failure, so the MEDIUM+ regression the brief flagged does not exist.
  `cross_reference_status` (index.rs:3487) warns and yields `None` →
  `UnknownProject`, where before it yielded `Some(empty)` → `MissingId`; a status
  change for an unreadable *foreign* project, and arguably the more honest one.
  `NotFound` still maps to empty inside the unchanged `collection_node_file_paths`
  helper, so a fresh project with no `decisions/` is unaffected.
- **The chmod-000 test cleans up.** `identity_lint.rs:615-625` restores `0o700`
  *before* the `expect_err`, so a failing assert cannot leave an unreadable dir
  behind to break `cargo clean`.
- **L2's recovery commands cover the whole blast radius.** `apply()`
  (project_migrate.rs:355-376) writes exactly: `migration.nodes` dirs,
  `migration.rewrites`, deletes `migration.old_files`, and writes
  `root/.orgasmic/project.org`. Traced each back through `plan()` — nodes and
  rewrites are rooted at `dotorg` (lines ~160-242, 322), old_files are the
  `collection.sources` under `dotorg`. Nothing outside `.orgasmic`, no `views/`,
  no repo-root file. `git checkout -- .orgasmic` + `clean -fd -- .orgasmic` is
  complete for that path, and `<tree>` is `root.display()` — absolute when the
  caller passes an absolute root, which `run_at`'s callers do.
- **L3 key alignment holds at every call site.** `publish_paths` inserts under
  `res.tx_id` / `result.tx_id` / `cached.tx_id`; the api takes with the same value
  (api.rs:2962, 7483, 8489). The one multi-tx caller (api.rs:18320-18331) refreshes
  with `results.first().tx_id`, matching the key `transaction_multi` uses at
  writer.rs:1087 and 1110. No request-id-vs-tx-id mismatch.
- **A key miss is benign by design.** When `take_apply_failure` returns `None`,
  both refresh paths fall through to `repair_projection()` (api.rs:8601, 8646),
  which either makes the projection whole — so the 200 is honest — or returns the
  error as *this* caller's committed-503. The brief's "dropped with no 503 and no
  repair" scenario does not exist: nothing clears the map except an owner take or
  a repair that actually succeeded (writer.rs:926, after the queue drains clean).
- **Map growth is bounded by the same condition as before.** `apply_failures`
  gains one entry per failing write and is cleared wholesale by a successful
  repair. It grows without bound only while the index is persistently broken —
  which already grows `unapplied` without bound. Pre-existing, not introduced.
- **`ledger_sync.rs:408`** now writes `machines/<id>/tx/<YYYY-MM>.org`, matching
  the doc, and `event_routes_to_journal("ledger.sync_conflict")` is `false` (pinned
  at api.rs:24197 and again at 24299).
- **`id_collision_repair.rs:49`** is a pure type change — `.unwrap()` on the new
  `Result`. Not a weakened assertion.

**Did not check:**

- Did not re-run `cargo test -p orgasmic-cli --bin orgasmic -- project_migrate`,
  `--test id_collision_repair`, `--lib -- ledger_route ledger_sync`, clippy, or fmt.
  The manager re-ran all of those on merged main; the brief marks them established.
- Did not exercise a live daemon. The daemon on `:4848` runs the pre-fix runtime,
  so the L3 path has no production-path probe here — my L3 conclusions come from
  reading the flow plus the two unit tests I ran. Residual risk: a real concurrent
  two-request interleaving under a genuinely failing index is unproven by any test;
  both existing tests use the single-shot `fail_next_refresh`.
- Did not read `verify/*/injection.patch`, did not run the workspace or any
  unfiltered suite, did not run
  `legacy_drivers_and_explicit_pairs_emit_equivalent_start_events`.

## Fix Directions

Ordered by value:

1. **F1** — rebuild the L4 test against the exhaustive `(type, routes)` pin table
   at api.rs:~24150, with a named exclusion set for non-dispatch `false` types, and
   delete the now-dead `DISPATCH_PROJECT_TX_TYPES` short-circuit in
   `event_routes_to_journal`. Answer Open Question 1 first.
2. **F2** — make `plan()` collect anomalous paths across the whole scan and print
   the real count before bailing with the list, or drop the line.
3. **F4** — give `migrate_to_branch` its own recovery message; the checkout/clean
   text is wrong for that path.
4. **F3** — pass the request id (or `Option<&str>`) as `mutate_file`'s apply owner
   instead of a phantom UUID.
5. **F5** — extend the new test to drive request A through `refresh_after_tx` and
   assert A's own 503 and tx id, so the owner side is pinned deliberately rather
   than by a neighbouring test.

**APPROVE WITH FOLLOW-UPS.**
