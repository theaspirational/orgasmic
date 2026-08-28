# Brief: TASK-W97C8.1 — move brief.md + compiled-prompt.md to close-time promote

Read the task node first — it carries the full design and acceptance
criteria. Summary: the daemon writes `brief.md` and `compiled-prompt.md`
into the durable `.orgasmic/tasks/TASK-X/dispatches/<tx>/` dir at dispatch
START; everything else lands at CLOSE. Move the two start-time writes to the
close-time promote so the record folder is complete-or-absent and a
failed/rolled-back dispatch leaves no orphan in the tracked tree.

You are branching from main AFTER TASK-W97C8 merged (evidence.json in the
promote path, commit 124ed1d5 + 46b015a3) — read that promote code as it is
NOW, not as older docs describe it.

Anchors:
- `crates/orgasmic-daemon/src/api.rs` (~6260, after `record_dispatch_started`):
  the start-time block creating the evidence dir and writing brief.md +
  compiled-prompt.md. Replace with writes into the gitignored tmp dispatch
  stem next to the run's `last.txt`/`stdout.log` (the CLI already places the
  manager brief at `<stem>-brief.md`; keep naming consistent with the stem
  grammar in `paths.rs` / `manager.rs:9832`).
- `crates/orgasmic-core/src/paths.rs` — `promote_validated_dispatch_attempt`
  and `DispatchAttemptArtifacts`: extend the close-time promote to copy
  brief + compiled-prompt into `dispatches/<tx>/` under the same
  validated-handle discipline, added to the unlink-only-after-every-copy-
  succeeded set. Failed-dispatch rollback stays tmp-only.
- `crates/orgasmic-cli/src/manager.rs` — close path call sites
  (`promote_dispatch_artifacts_in_place` ~8076, `promote_and_persist_...`);
  the record commit already scopes the whole dir, so no commit change
  expected.

Acceptance (from the task node):
- Nothing exists under `dispatches/<tx>/` before close; after a successful
  close the folder holds brief.md, compiled-prompt.md, report.md,
  evidence.json in ONE record commit.
- Failed/rolled-back dispatch leaves NO `dispatches/<tx>/` folder.
- Partial promote failure keeps tmp copies intact.
- Focused tests only: start writes nothing durable; close promotes all
  files; rollback leaves no orphan dir. Rerun the existing promote/close
  focused suites green (`cargo test -p orgasmic-core --lib paths::`,
  the dispatch_close/dispatch_evidence tests in orgasmic-cli,
  `--test shipped_conventions`). Pinned toolchain: `rustup run 1.97.1`
  (plain cargo is 1.94.1 on this machine).
- Daemon API surface: this one legitimately touches the daemon start path —
  keep the change to relocating the writes; no new endpoints, no request/
  response shape changes.
- Update the manager-dispatch convention if it states when brief/compiled-
  prompt land.

Report: files changed, where the tmp copies live, test names + pass counts.
