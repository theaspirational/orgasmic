# Review TASK-JWHXH.3 — on-disk views deleted; render on demand; migrate + doctor

Scope reviewed: `git diff ad642ca7^1 ad642ca7` (19 files, +404/-306), plus `dec_AF61D`,
`dec_XH2XY`, the task record, and the surrounding source in `views.rs`, `index.rs`,
`api.rs`, `ledger_sync.rs`, `project_migrate.rs`, `doctor.rs`, `projects.rs`.

## Verdict

APPROVE WITH FOLLOW-UPS. One MEDIUM (doctor blind to non-git projects) and six LOWs.
No HIGH: no data-loss path, no unmet git-fixture acceptance criterion, no regression
proven in the shipped paths.

## Findings

**MEDIUM — `crates/orgasmic-cli/src/doctor.rs:254` (correctness / decision conformance).**
`push_tracked_views_findings` does `if !is_git_work_tree(&root) { continue; }` before it
ever looks at `dir_present`. A registered project that is not a git work tree therefore
keeps a stale `.orgasmic/views/board.org` on disk forever with no warning — the daemon no
longer refreshes it, so anything reading the file (Emacs, an agent with old habits) gets a
silently frozen board. The `"still present"` branch at `:270` was written for exactly this
case but is unreachable outside git repos. `dec_XH2XY` says doctor warns "while it is still
there", not "while it is still tracked". `ViewsMigration::plan`/`apply` already handle a
non-git tree correctly (`dir_present` is computed unconditionally), so only the doctor gate
is wrong.
Fix direction: drop the early `continue` and run the `git ls-files` probe only when
`is_git_work_tree(&root)`, keeping the `dir_present` branch for everyone.

**LOW — `crates/orgasmic-daemon/src/api.rs:14571` (perf).** `get_org_file` calls
`render_view` inline on the async executor thread; there is no `spawn_blocking`, and
`render_collection` is fully synchronous (one `read_to_string` + one `OrgFile::parse` per
node, plus an `OrgRewriter` round trip per claimed task). Measured against the real ledger
`~/.orgasmic/ledgers/orgasmic` (814 task nodes / 3.18 MB, 120 decisions, 60 terms) with a
release build of `orgasmic-core` (throwaway crate in `/tmp/jwhxh3-bench`):

    board.org      41ms cold, 24ms warm  -> 3,136,072 bytes
    decisions.org   4ms cold,  3ms warm  ->   288,461 bytes
    glossary.org    2ms cold,  1.5ms warm ->   29,031 bytes

24 ms of blocking work on a tokio worker per request is not the daemon stall the brief
worried about, and the UI viewer fetches only on tab/refresh, not in a poll loop. It is
linear in task count, so this is a "wrap in `spawn_blocking` when a project passes a few
thousand tasks" follow-up, not a ship blocker.

**LOW — `crates/orgasmic-daemon/src/api.rs:14705` (docs, user-facing string).**
`reject_ledger_rewrite` still tells the caller to "regenerate it through the view refresh
operation". This change deleted every such operation (`orgasmic views build`, the index
rebuild sites, the debounce). A user who hits Save in the UI's raw org viewer on
`.orgasmic/views/board.org` — which is `ORG_FILES[0]`, the default selection in
`ui/src/components/OrgView.tsx:26` — gets a 400 pointing at nothing. The refusal itself is
correct and unchanged.

**LOW — `crates/orgasmic-core/src/views.rs:55,61` (robustness).** `render_collection`
`bail!`s on the first node with != 1 heading or an invalid task state. Previously that
failure surfaced as a parse-error entry while the last successfully written
`.orgasmic/views/board.org` stayed readable; now one corrupt task node returns 500 from
`GET /org/file` for the entire board view. The index-derived UI (board, task lists) is
unaffected — it keeps its own parse-error path — so blast radius is the raw org viewer only.

**LOW — `crates/orgasmic-cli/src/project_migrate.rs:155,170` (dead code / misleading output).**
`views_applied == true` implies `!is_clean()`, i.e. `tracked` non-empty or `dir_present`, so
one of the two earlier arms always fires and the `else if views_applied` arm at `:170`
("views untracked and directory removed") can never print. Separately, the `:155` line
prints `views.tracked.len()` from the pre-apply plan on a non-dry run, i.e. it reports state
the same run just destroyed as though it were current.

**LOW — `shipped/schema/tx.org:294` and `shipped/skills/orgasmic/meta/corpus-manifest.json:135-136`
(docs).** `tx.org` still documents "derived aggregate read views under `.orgasmic/views/`".
The corpus manifest still carries `cli-help/views.txt` and `cli-help/views/build.txt` for
the deleted verb; `operations/core-project.md` dropped those `sources:` entries, so the two
are now inconsistent (the manifest is a dated `okfy` snapshot, so this is refresh-on-next-
update, not a build break — nothing in-tree reads it).

**LOW — `crates/orgasmic-core/src/projects.rs:188` (scope).** The task description still
carries "init_project must not skip the ignore rule when .gitignore already exists
(projects.rs:188)". It is not implemented: the scaffold loop is still
`if dest.exists() { continue; }`, so a project with a pre-existing `.orgasmic/.gitignore`
never gains `tmp/`. `dec_XH2XY` removes the `views/` half of that requirement entirely
(the scaffold `.gitignore` no longer ships `views/`), so what remains is the pre-existing
`tmp/` gap, out of this decision's scope. Flagging so the dropped acceptance line is a
manager decision rather than a silent omission.

## What I attacked and found clean

- **Data safety of the deletes.** `std::fs::remove_dir_all` on a symlinked `views` removes
  only the link, not the target — probed on this exact toolchain (rustc 1.94.1, Homebrew):
  `is_dir() == true`, `remove_dir_all -> Ok(())`, target file survives, symlink gone. No
  path outside the derived dir is reachable: both call sites join a literal
  `.orgasmic/views` onto a root that comes from `find_project_root()` (CLI) or the
  registered ledger path (daemon).
- **Sync-loop delete is inside the synced-ledger loop.** `ledger_sync.rs:153` sits after the
  `origin`-remote guard (`:104`, returns `Idle`) and the `HEAD == orgasmic` guard (`:124`,
  returns `Idle`), inside `sync_once_with_park`. `grep '"--cached"' crates/orgasmic-daemon/src/`
  returns only `ledger_sync.rs:147` and `:164` — acceptance criterion "daemon never runs
  `git rm --cached` outside the synced-ledger loop" holds.
- **No renderer left to race the delete.** Nothing writes `.orgasmic/views/` anymore:
  `build_views`/`write_if_changed` are gone, `index.rs:902` and `:3651` still classify
  `views` as ignored, `watcher.rs:351` still drops events under it, and `render_view` is
  pure (asserted by `views.rs` tests and by `refresh_and_node_writes_never_materialize_views`).
- **`refuse_dirty_tree` exclusion.** Probed on git 2.52.0 in a throwaway repo: a
  negative-only pathspec is accepted (rc=0) and other dirty paths still report —
  `git status --porcelain=v1 -uall -- ':(exclude).orgasmic/views'` printed ` M other.txt`
  and `?? new.txt` while suppressing only ` M .orgasmic/views/board.org`. `migrate_to_branch`
  cannot proceed over unrelated dirt.
- **Ordering with `--to-branch`.** `views.apply` runs before `migrate_to_branch`, so
  `copy_tree(root/.orgasmic -> stage)` at `:507` no longer carries views onto the orphan
  branch, and the final `remove_dir_all(root/.orgasmic)` at `:549` supersedes the staged
  `rm --cached`. The printed recovery hint (`git checkout -- .orgasmic`) will not restore
  views files, which is the intended end state.
- **Behavioural equivalence.** `render_collection` is byte-for-byte unchanged by this diff
  (only its callers moved), including the stable `sort_by_key(stage)` and the header
  constants in `VIEWS`, so the rendered bytes match the old `build_views` output for the
  same tree.
- **Idle-ledger regression.** The deleted `claims.org` hook (old `index.rs:~972`) only ever
  called `build_views`; `reload_tx_file` still runs for `machines/<id>/claims.org` in the
  same arm (`index.rs:917-926`), so claim reloading is intact.
- **Peer on an old runtime.** Each tick untracks and now also deletes. The commit-per-tick
  churn (and the conflict/park loop when a peer re-adds) already existed before this change,
  because the old code also staged `git rm --cached` on every tick; the added
  `remove_dir_all` does not change the commit shape. Same magnitude as `dec_EWY0K`'s
  existing conflict path, no new defect.
- **Docs honesty.** `orgasmic glossary list --project <id>` and
  `orgasmic decision list --project <id>` both exist with those exact flags
  (`crates/orgasmic-cli/src/main.rs:707` `GlossaryCmd::List`, `:784` `DecisionCmd::List`).
- **Deleted context packs are unreferenced.** No prompt spec or part names `sprint_tasks`,
  `decisions` or `glossary` as a `:CONTEXT_PACKS:` value anywhere in
  `shipped/prompt-studio/`, and a missing pack degrades to a `PromptDiagnostic`
  (`prompt_compiler.rs:266`), not a hard failure.
- **Nothing else moved.** All 19 files map onto the brief's bullets; the only surprises are
  the two stale doc strings filed above.

## Open Questions

1. Is the `tmp/` half of "init_project must not skip the ignore rule" deliberately dropped
   with `dec_XH2XY`, or should it become its own follow-up task?
2. Does a non-git registered project need the doctor warning, or is `project migrate` alone
   the intended path there? (I read `dec_XH2XY` as requiring the warning.)

## Verification Notes

- Read: full diff `ad642ca7^1..ad642ca7`; `dec_AF61D` and `dec_XH2XY` node.org; task record
  via `orgasmic task get --project orgasmic TASK-JWHXH.3`; source around every call site.
- Ran (read-only, outside the repo): a release-mode `orgasmic-core` bench crate at
  `/tmp/jwhxh3-bench` against the live ledger for the render timings above; a symlink
  `remove_dir_all` probe; a throwaway git repo at `/tmp/jwhxh3-git.GExQ` for the
  `:(exclude)` pathspec behaviour. No repo file was edited, no mutating `orgasmic` verb
  beyond the seven `tx record --type reviewer.finding` entries the brief requires.
- Did NOT re-run the gate suites (core / daemon lib / integration scaffold / cli 34 /
  clippy / fmt) — the implementer and the manager both ran them on merged main `ad642ca7`
  with logs in the task Evidence, and the brief says not to re-spend that.
- Did NOT check: the `:(exclude)` pathspec on git older than 2.52.0; a case-insensitive-FS
  `.orgasmic/Views/` variant (git's pathspec is case-sensitive while `is_dir()` is not, so
  migrate would delete the dir without untracking it — I judged this not worth a finding
  since nothing creates that spelling); the UI end-to-end (no browser run); the live daemon
  on :4848 (old runtime, off limits per the brief).

## Fix Directions

1. `doctor.rs:254` — hoist the git probe: keep `dir_present` reporting for every registered
   project, run `git ls-files` only inside a work tree. One-line change, and the existing
   `"still present"` string already covers it.
2. `api.rs:14705` — reword to name the CLI (`orgasmic task get` / `decision list` /
   `glossary list`) or simply "derived views are rendered on demand and cannot be written".
3. `project_migrate.rs:155-171` — collapse the three arms into one that reports what
   `apply` actually did (pass the applied counts out of `apply`, or print from the
   post-apply state).
4. `views.rs:55` — optional: skip and count unparsable nodes instead of `bail!`, so one bad
   node degrades the view rather than 500-ing it.
5. `tx.org:294` — drop the `.orgasmic/views/` clause; refresh the corpus manifest on the
   next `okfy update`.

APPROVE WITH FOLLOW-UPS.
