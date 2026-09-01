# Review: the dec_E01MC / TASK-TWCP9 chain — first independent whole-chain review

You are reviewing ONE chain of 17 tasks as a single body of work. Every task
heading injected above is part of it. The chain replaced orgasmic's storage
substrate: per-node directories, a node kernel, a tx split, a migrator, a
distributed ledger on its own git branch, cross-machine claims and a sync
loop. It shipped as runtime 0.0.23 and the live daemon on this machine runs it.
No independent reviewer has read it; the only review so far was the manager's
own inline pass on 2026-08-27. You are the first.

## What to review

Range on `main`: `543782d7^1..31344134` — one merge of 60 commits
(`543782d7`, the chain) plus two cutover commits (`e6e8bc7f`, `31344134`).

Read the CODE diff only:

    git diff 543782d7^1 31344134 --stat -- . ':!.orgasmic'
    git diff 543782d7^1 31344134 -- . ':!.orgasmic'

That is ~12k lines across `crates/orgasmic-core`, `crates/orgasmic-daemon`,
`crates/orgasmic-cli`, `ui/src`, `shipped/`. The ~170k deleted lines under
`.orgasmic/` are the tracked ledger leaving the tree (TWCP9) — do not read
them line by line. The evidence for that half is the migrator's round trip
(873 nodes, 0 anomalies, byte-for-byte); you may attack that claim by running
the migrator against a scratch copy of the pre-cutover tree
(`git worktree add <tmp> 543782d7^1`, then migrate the copy — never the live
ledger).

Integration commits, in landing order (each is a `--first-parent` step inside
the merge; `git log --first-parent 543782d7^1..543782d7^2` lists them):

| task | integration | what |
|---|---|---|
| TP593 | `f56668a6` | node kernel in core + writer ops |
| 7A0H4 | `442f3ae1` | node type descriptors, loader, descriptor-driven mint |
| GCXB7 | `7c5f2d27` | shared dispatch fold + type-set guard |
| SRBGS | `c9c63d00` | migrator; indexer over node dirs; old aggregate readers deleted |
| JWHXH | `894ad037` | derived read views `views/{board,glossary,decisions}.org` |
| EPG6H | `e69ca882` | tx split: node-scoped events → `tasks/<ID>/journal.org` |
| ARRV8 | `3ff87fc5` | artifacts onto the node kernel |
| AN992 | `67830220` | CLI verbs over node dirs + shipped content |
| IXPD4 | `857c2f42` | daemon index tests ported to node-dir fixtures |
| TFXR2 | `62dc3e8a` | daemon api/watcher + cli manager tests ported |
| RGRD5 | `40c8a71b` | torn-close repair bypasses the descriptor guard when the ledger already recorded the transition |
| LBRX7 | `1d816983` | ledger branch extraction — `.orgasmic` leaves the tracked tree |
| 8AV8B | `b630d323` | incremental refresh: per-node reload, writer apply-own-write |
| MSYN4 | `3c3ff1b9` | machine identity + ledger sync loop over the git remote |
| 8DMQS | `0304dcb7` | regenerate generalization over descriptors |
| CLM6W | `9d124bfb` | cross-machine task claims + multi-machine fold |
| KA934 | `6cb3de44` | UI: per-node comments, regenerate, activity rail |

Cross-merge repairs inside the range: `6c97e599` (four defects in the
8AV8B/MSYN4 integration), `dd74ea50`, `3891ca4c`, `f70d1e5e`. Fixes from the
2026-08-27 inline review, also inside the range: `31206393` (claim refusal is
a 409 naming the holding machine), `c31639c9` (one project's write no longer
clears another project's stale projection), `c18b3a11` (sync stages what this
machine wrote, not what it still holds), `8b092d27` (fmt + clippy -D clean),
`366c4b5b` (cutover tells the operator about the uncommitted deletion).

Related but OUTSIDE the range, context only: `9acfba79` (three pre-existing
workspace reds closed for the 0.0.23 certification), `9413059a`
(owner-lifecycle checks follow the cutover), TASK-AS0FS (P2, filed from the
inline review: singleton ledger files — `project.org`, `goal.org`,
`handoff.org`, `gotchas.org`, `views/` — have no owner across machines and the
sync loop has no conflict path).

## The spec you are checking the code against

The design is in the AP971 ticket Resolutions and `dec_E01MC`:

    orgasmic decision get --project orgasmic dec_E01MC
    orgasmic task get --project orgasmic TASK-AP971.5      # tx split table
    orgasmic task get --project orgasmic TASK-AP971.{2,3,6,7,8,9,10,11}

Contract points the code must honour (from those Resolutions):

- node dir = id; `mkdir` is the collision check; one shared id sequence.
- tx split: an event goes to a node journal iff it is about exactly one
  dir-backed node; singletons stay files; creation = journal entry #1;
  deletion = tx tombstone; dispatch lifecycle stays in monthly `tx/`.
- dispatch close = ONE tx append + N node rewrites; torn-close repair is
  legacy-only.
- `views/` are daemon-written and gitignored; never a write target.
- per-node reload; the writer applies its own write to the index without a
  full rescan.
- regenerate runs over descriptor specs; task regenerate never re-dispatches.
- migrate covers done/cancelled too; there is NO v1 compatibility layer; the
  version stamp is a label only.
- two machines never write the same node (claims); the dispatch fold has a
  type-set guard.

## Where you are

- Your worktree has NO `.orgasmic/` directory. That is by design (LBRX7).
- The live ledger is `/Users/aspirational/.orgasmic/ledgers/orgasmic/.orgasmic/`
  — a git worktree of this repo on branch `orgasmic`. READ ONLY for you.
- The live daemon on `:4848` runs 0.0.23, built from this chain. Use it as an
  executable oracle through read verbs only (`orgasmic task get --project
  orgasmic …`, `orgasmic tasks list --project orgasmic`, `orgasmic status`).
  Never a mutating verb against it.

## What the verdict must answer

Would you ship this chain as the storage substrate every future ledger sits
on? **APPROVE / APPROVE WITH FOLLOW-UPS / REJECT**, findings ranked by
severity with `file:line`, each tagged with the chain task it belongs to.

Attack these claims specifically — they are the load-bearing ones:

1. **Writer atomicity (TP593, 8AV8B, RGRD5).** A crash between the single tx
   append and the N node rewrites of a dispatch close: what is on disk, and
   does the next boot repair it or read a lie? Is RGRD5's repair sound when the
   ledger recorded a transition the node never received?
2. **The tx split (EPG6H).** Find an event that lands in the wrong surface, in
   both, or in neither. Find a reader that still expects the old aggregate
   `tx/` for a node-scoped event.
3. **The migrator (SRBGS).** Idempotence on re-run; behaviour on partial
   failure; `mkdir` collision; done/cancelled coverage. "0 anomalies" was
   measured on this repository only.
4. **Cross-machine claims and fold (CLM6W, GCXB7).** Can two machines write the
   same node through ANY path? Is the type-set guard total over the event
   types? Is the 409 from `31206393` reachable on every write path or only the
   one that was fixed?
5. **Sync loop (MSYN4).** What does a failed `pull --rebase` leave behind; can
   the loop push a partial state; does `c18b3a11`'s "what this machine wrote"
   enumeration miss a file class; what happens when two machines both hold
   unsynced writes to the same singleton (AS0FS territory — confirm the shape).
6. **Incremental refresh (8AV8B).** `c31639c9` fixed one cross-project
   projection clear. Is it the only one?
7. **Descriptor guard bypasses (7A0H4, 8DMQS, RGRD5).** Every path that skips
   the transition guard — enumerate them, decide which are legitimate.
8. **Deleted readers (SRBGS).** Any surviving caller that now silently reads
   nothing and reports an empty result as fact.
9. **UI (KA934).** Per-node comments and regenerate: untrusted content into
   the DOM; the activity rail's data source; anything that writes outside the
   CLI/daemon path.
10. **Test honesty (IXPD4, TFXR2, `3891ca4c`).** Ported tests must still assert
    the property their name promises, not merely compile against the new
    layout. Sample a dozen and say which you read.

Already established — spend no effort here: fmt and clippy -D warnings are
clean (`8b092d27`); the workspace suite was certified for the 0.0.23 stable
publish (`9acfba79`); the five 2026-08-27 fixes above are applied.

## Rules

- Strictly READ-ONLY: no file edits, no git writes, no mutating `orgasmic`
  verbs, nothing against the live ledger.
- **Do NOT bulk-read `verify/*/injection.patch` files.** A provider content
  filter has killed two reviews mid-run on exactly that pattern (TASK-52NJS).
  Read the tests instead. If you cannot check something, say so — an honest
  "not checked" beats a dead run.
- **File every finding the moment you have it** as a `reviewer.finding` tx
  bound to the chain task it belongs to — one line, no newlines:

      orgasmic tx record --project orgasmic --type reviewer.finding \
        --task TASK-XXXXX \
        --reason "HIGH|MEDIUM|LOW <crate/path.rs:line> — <what breaks, one sentence>"

  Do not hold findings for the report; a run can die and the report with it.
- Tests: targeted only — `cargo test -p <crate> <test_name>`. NEVER
  `cargo test --workspace`; NEVER the whole `orgasmic-cli` crate in one
  command (see `.orgasmic/gotchas.org` in the ledger: "Never run cargo test
  --workspace" — this laptop reboots). Never run
  `legacy_drivers_and_explicit_pairs_emit_equivalent_start_events`. Never set
  `ORGASMIC_ALLOW_BILLED_TESTS` or `ORGASMIC_HOME`.
- Roughly half of the reviews on this project return REJECT. Softened
  findings help nobody; name what breaks.
- Tell me what you did NOT check, and which of the ten claims you consider
  settled vs. open.
- Finish with `orgasmic dispatch finalize --summary-file <path>` (report
  only, no `--commit`). End the report with the explicit verdict sentence.
