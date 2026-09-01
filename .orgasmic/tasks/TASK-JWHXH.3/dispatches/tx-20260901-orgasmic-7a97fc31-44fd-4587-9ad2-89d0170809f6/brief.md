# TASK-JWHXH.3 — stop writing `.orgasmic/views/*.org`; render on demand; migrate + doctor

Read the task first: `orgasmic task get --project orgasmic TASK-JWHXH.3`, then `dec_AF61D` and
the decision it links. Line numbers below are approximate; read the current files.

## Why
`views/board.org` (3 MB), `decisions.org`, `glossary.org` are pure re-renderings of
`tasks/ decisions/ glossary/`. The daemon index skips `views/` (index.rs ~:946), no prompt
spec includes the `sprint_tasks` context pack, and its render policy is a pointer. The only
runtime reader is the UI raw org viewer (`ui/src/components/OrgView.tsx` ORG_FILES). The
files were being committed into user git history. Decision: delete the on-disk views.

## The change (deletion first)
1. `crates/orgasmic-core/src/views.rs`: keep `render_collection` as a pure renderer; add
   `pub fn render_view(project_root, name) -> Result<String>` for `board.org`,
   `decisions.org`, `glossary.org`. Delete `build_views` + `write_if_changed` and the
   `views/` directory creation. Delete the scaffold of `views/` + `board.org` in
   `crates/orgasmic-core/src/projects.rs` (~:188 skip-existing, ~:339 "board.org failed to
   parse after write", ~:399/:593 fixtures); keep `tmp/` in the scaffold .gitignore.
2. `crates/orgasmic-daemon/src/index.rs`: delete the three `build_views` call sites (~:867
   debounced rebuild, ~:972 claim views, ~:3029 board entry) and the debounce/scratch-name
   machinery that exists only for them (added by TASK-JWHXH.1.1) if nothing else uses it.
3. `crates/orgasmic-daemon/src/api.rs` `get_org_file` (~:14512): when the requested path is
   `.orgasmic/views/<name>.org`, return `render_view(...)` instead of reading disk, so the
   UI dropdown keeps working with no UI change. `post_org_file` already refuses `views`
   (~:14632) — keep. Keep `"views"` in `DAEMON_OWNED_SURFACES` (writer.rs:38).
4. CLI `orgasmic views build` (`crates/orgasmic-cli/src/main.rs` ~:498, ~:1984): delete the
   subcommand (and its cli-help fixture `cli-help/views/build.txt` if present), unless a
   test depends on it — then make it print the rendered view to stdout.
5. Migration for repos the daemon does not sync: `orgasmic project migrate`
   (`crates/orgasmic-cli/src/project_migrate.rs`): if `.orgasmic/views/` is tracked, run
   `git rm -r --cached --quiet -- .orgasmic/views`; then delete the directory; idempotent
   (second run is a no-op). `orgasmic doctor` (`crates/orgasmic-cli/src/doctor.rs`): for each
   registered project that is a git repo, warn `.orgasmic/views/ still tracked/present —
   run: orgasmic project migrate` while it is. The DAEMON never runs `git rm` outside the
   synced-ledger loop. For the synced-ledger loop (`ledger_sync.rs` ~:140-160, TASK-JWHXH.1):
   keep the untrack, and also remove the now-dead `views/` directory there (daemon owns it).
6. Prompt-studio + docs: delete the unreferenced context packs
   `shipped/prompt-studio/context-packs/{sprint_tasks,decisions,glossary}.org` (grep first:
   nothing includes them) and repoint prose in `prompt-parts/grill_domain_policy.org` ~:9-10,
   `prompt-parts/graph_authoring_policy.org` ~:9-10, `prompt-specs/manager.org` ~:45-46 to
   `orgasmic glossary list --project <id>` / `orgasmic decision list --project <id>` /
   `orgasmic task get`. Skill docs: `shipped/skills/orgasmic/references/ledger.md` ~:21,
   `references/recall-resume.md` ~:44,~:71, `operations/core-project.md` ~:46.
7. Tests: fix/delete the ones that read the files (index.rs ~:5809-5848, views.rs tests,
   projects.rs, api.rs ~:21318/~:21326 keep the refusal fixtures, daemon
   tests/integration.rs ~:312, ledger_sync.rs ~:2418/~:2443). Add: (a) plain-branch fixture
   with a tracked `.orgasmic/views/board.org`: doctor warns → migrate untracks + deletes →
   second run no-op → doctor quiet; (b) `get_org_file` for `.orgasmic/views/board.org`
   returns the rendered board with no file on disk.

OFF LIMITS (TASK-KA934.3 territory, running in parallel): `api.rs` `MEMBER_ALLOWED_ROUTES`,
`identity_middleware`, `post_task_comment*`; `writer.rs` comment functions; `authz.rs`;
`ui/src/components/TaskDialog.tsx`. Also do NOT touch `home.rs` `user/board.org` — a
different file.

## Gates (each to a log file, never pipe cargo output)
- `cargo test -p orgasmic-core`
- `cargo test -p orgasmic-daemon --lib -- views index org_file ledger_sync scaffold`
- `cargo test -p orgasmic-daemon --test integration -- scaffold` (targeted)
- `cargo test -p orgasmic-cli --bin orgasmic -- doctor migrate views` (targeted; NEVER unfiltered)
- `cargo clippy -p orgasmic-core -p orgasmic-daemon -p orgasmic-cli --all-targets -- -D warnings`
- `cargo fmt --all --check`; `cd ui && npm run typecheck` only if you touched ui/.

## Rules
- Work only in your worktree; one commit `TASK-JWHXH.3: refactor(views): <one line>`.
- NEVER `cargo test --workspace`; NEVER the whole `orgasmic-cli` crate in one command; NEVER
  set `ORGASMIC_HOME`; NEVER run `daemon start`; never touch the live ledger at
  `~/.orgasmic/ledgers/orgasmic`; never run
  `legacy_drivers_and_explicit_pairs_emit_equivalent_start_events`; `git rm --cached` only
  inside temp fixtures your tests create.
- Fix pre-existing clippy/lint diagnostics in files you touch.
- Report: what changed (`file:line`), what you deleted, each gate with its pass/fail line
  and log path, unmet criteria, residual risk. Finish with
  `orgasmic dispatch finalize --summary-file <path>` (report only, no `--commit`).
