# TASK-JWHXH.3 — stop writing `.orgasmic/views/*.org`; render on demand; migrate + doctor

Commit: `9af6548d` on `task-jwhxh.3-impl` (one commit, worktree only).
Implements dec_AF61D + dec_XH2XY (task notes' rewritten scope).

## Changed

**Renderer (core)**
- `crates/orgasmic-core/src/views.rs:20-33` — new `pub fn render_view(project_root, file)` (board.org / decisions.org / glossary.org); pure, writes nothing. Deleted `build_views`, `write_if_changed`, `TMP_COUNTER` (atomic-write scratch machinery). `render_collection` kept as private pure renderer.
- `crates/orgasmic-core/src/lib.rs:96` — re-export `build_views` → `render_view`.
- `crates/orgasmic-core/src/projects.rs:476-479` — init test now asserts scaffold `.gitignore` has `tmp/` and NOT `views/`, and no `.orgasmic/views` dir is created (scaffold never wrote views files; only the ignore line existed).
- `shipped/project-scaffold/.gitignore` — `tmp/\nviews/\n` → `tmp/\n`.

**Daemon**
- `crates/orgasmic-daemon/src/index.rs` — deleted all three `build_views` call sites (debounced rebuild, claims.org reload hook, boot board-entry load) and the debounce machinery that existed only for them (`schedule_view_rebuild`, `view_dirty_roots`, `view_drain_scheduled`, `VIEW_REBUILD_DEBOUNCE`). `tmp|views` watcher skip at index.rs:913 and the `views` collector skip at :3679 kept (harmless guards).
- `crates/orgasmic-daemon/src/api.rs:14512-14570` — `get_org_file` renders `.orgasmic/views/<name>.org` on demand via `render_view` (`rendered_org_view` matcher); unknown names fall through to the disk read (404). `post_org_file` refusal and `DAEMON_OWNED_SURFACES` `"views"` (writer.rs:38) untouched — off-limits adjacent files untouched.
- `crates/orgasmic-daemon/src/ledger_sync.rs:136-158` — synced-ledger loop: kept the `git rm -r -q --cached --ignore-unmatch -- .orgasmic/views`, added `remove_dir_all(.orgasmic/views)` (NotFound tolerated), removed the now-dead `views/`-in-.gitignore ensure. tmp-sidecar untrack untouched. This remains the daemon's ONLY `git rm` (verified: no other `"rm"` in orgasmic-daemon).

**CLI**
- `crates/orgasmic-cli/src/main.rs` — deleted `Cmd::Views`, `ViewsCmd`, the match arm, `cmd_views_build`. Probe: `orgasmic views build` → `unrecognized subcommand 'views'`.
- `crates/orgasmic-cli/src/project_migrate.rs:56-190` — `ViewsMigration` (plan/apply): `git ls-files -- .orgasmic/views` detection, `git rm -r -q --cached --ignore-unmatch`, dir deletion; idempotent. Folded into `run_at` before the early returns and before `migrate_to_branch` (so the orphan-branch copy never carries views). `refuse_dirty_tree` now excludes `.orgasmic/views` paths so a re-run before the operator commits the staged deletion is a no-op, not a refusal. Summary prints the views outcome.
- `crates/orgasmic-cli/src/doctor.rs:251-294` — `push_tracked_views_findings`: for each registered git-repo project, warns `<root>: .orgasmic/views/* tracked in git|still present — run: orgasmic project migrate` while tracked or present. Wired into `diagnose`.

**Docs**
- Deleted `shipped/prompt-studio/context-packs/{sprint_tasks,decisions,glossary}.org` (grepped: nothing includes them).
- `prompt-parts/grill_domain_policy.org:9-10`, `prompt-parts/graph_authoring_policy.org:9-10`, `prompt-specs/manager.org:44-46` repointed to `orgasmic glossary list --project <id>` / `orgasmic decision list --project <id>` / `orgasmic task get` (verbs verified to exist in main.rs).
- Skill docs: `references/ledger.md` (views line removed), `references/recall-resume.md:44,71` (views reads → CLI), `operations/core-project.md` (views aliases/sources/example removed).

**Tests**
- views.rs: ingest-order-independence test now uses `render_view` + asserts nothing lands on disk; new unknown-name test; deleted the atomic-write concurrency test (machinery gone).
- index.rs: two view-rebuild tests replaced by `refresh_and_node_writes_never_materialize_views` (boot rebuild, refresh, incremental node write → no `.orgasmic/views` ever, task still indexed).
- api.rs: `org_file_get_renders_views_on_demand_without_disk_files` (board/decisions/glossary over the real handler, no disk file) + unknown-view-name → 404. Existing refusal fixtures kept.
- ledger_sync.rs: `existing_ledger_views_are_untracked_deleted_and_idempotent` (was ignored+kept-file).
- project_migrate.rs: `plain_branch_views_doctor_warns_migrate_untracks_then_doctor_quiet` and `ledger_without_remote_views_doctor_warns_migrate_untracks_then_doctor_quiet` (doctor warns → migrate untracks+deletes → second run no-op with no intermediate commit → doctor quiet; ledger fixture has no remote so the sync loop stays Idle).

## Verification Gates

All logs in `/var/folders/9p/823z6j817xj9ts2xpvnx1q_40000gn/T/opencode/jwhxh3-logs/`:

| Gate | Result | Log |
|---|---|---|
| `cargo test -p orgasmic-core` | ok — 180+19 passed, 0 failed | gate-core.log |
| `cargo test -p orgasmic-daemon --lib -- views index org_file ledger_sync scaffold` | ok — 111 passed, 0 failed | gate-daemon-lib.log |
| `cargo test -p orgasmic-daemon --test integration -- scaffold` | ok — 2 passed, 0 failed | gate-daemon-integration.log |
| `cargo test -p orgasmic-cli --bin orgasmic -- doctor migrate views` | ok — 34 passed, 0 failed | gate-cli.log |
| `cargo clippy -p orgasmic-core -p orgasmic-daemon -p orgasmic-cli --all-targets -- -D warnings` | rc=0, 0 warnings | gate-clippy.log |
| `cargo fmt --all --check` | rc=0 | gate-fmt.log |

`ui/` untouched → no `npm run typecheck` (per brief). Production-path probes: CLI binary rejects `views build`; `project migrate --help` intact; `get_org_file` exercised through the real axum handler.

## Unmet Criteria

None — all four acceptance boxes covered:
- plain-branch fixture doctor→migrate→no-op→quiet: test green.
- ledger-without-remote fixture same: test green.
- daemon `git rm --cached` confined to the synced-ledger loop: structural (grep) + code review.
- clippy/fmt/targeted tests: green.

## Residual Risk

- `shipped/skills/orgasmic/meta/corpus-manifest.json` still lists `cli-help/views.txt` / `cli-help/views/build.txt` hashes (and hashes for the edited prompt-spec files are now stale). Nothing in this repo validates it; regeneration presumably happens at skill-corpus rebuild — flagging so the next corpus refresh drops them.
- Existing user checkouts with a `views/` line in `.orgasmic/.gitignore` keep that line (harmless; migrate/doctor only look at tracked/present state, not the ignore file).
- Synced ledgers with a remote peer still running a pre-dec_XH2XY daemon: the old peer may re-commit views files; the new sync loop untracks + deletes them on each tick until the peer is updated.
