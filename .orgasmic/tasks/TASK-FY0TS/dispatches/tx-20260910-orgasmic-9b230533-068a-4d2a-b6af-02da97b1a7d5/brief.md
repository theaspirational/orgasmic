# Review brief: TASK-FY0TS implementer generation tx-20260910-orgasmic-4a0f1858-1b18-4a2d-8f02-4aef5e261f20

Review worker commit `868f7aac` (your worktree HEAD) against base `7af2d428`: `git diff 7af2d428..868f7aac`. Author harness: codex (gpt-5.6-sol, high).

## What it claims
Implements dec_8DW4V: per-project `:ATTACHMENT_STORAGE: lfs|local` on the PROJECT heading of `.orgasmic/project.org`. `local` keeps attachment bytes out of git (ledger staging excludes `*/*/attachments/**`, upload finish skips the LFS probe), records carry a `MACHINE` id, and 404/503 messages tell the user what to do. Read `orgasmic task get TASK-FY0TS` and `orgasmic decision get dec_8DW4V`.

## Look hard at
1. `ledger_sync.rs`: how the mode is read on every sync tick (cost, failure handling when project.org is missing/invalid — does a bad value halt sync, and is that the right call?). The exclude path: can a blob published in `lfs` mode before a switch to `local` still be staged, or vice versa, and is either outcome surprising?
2. `schema.rs`: parsing and the `SchemaError` variant; does the write-time validation in `api.rs` (project node prop set) actually reject `ATTACHMENT_STORAGE=foo`, and can it reject an unrelated valid edit by accident?
3. `node_services.rs`: the mode read in `finish_upload` and `get_content`; the 503/404 strings — are they true in every branch they fire from? Any message that still uses jargon or an unhelpful path?
4. `AttachmentRecord.MACHINE`: rendering/parsing compatibility with records written before this change; any place that reads records and now panics/rejects on the missing key.
5. Tests: do they prove local mode never stages a blob on a remote-backed ledger (not just a dry-run)? Anything asserted only via string equality on messages that could go stale silently?
6. MSRV, clippy, fmt, commit hygiene.

## Gates you may run (own target dir; never start a daemon; never `cargo test --workspace`)
- `cargo test -p orgasmic-daemon --test node_services_routes --target-dir target`
- `cargo test -p orgasmic-daemon --lib ledger_sync --target-dir target`
- `cargo test -p orgasmic-core --target-dir target`

## Output
Finish with `orgasmic dispatch finalize --task TASK-FY0TS --summary-file <report>`. Report: findings (bugs first, each with file:line and a concrete failure), then a one-word verdict line `VERDICT: approve | approve-with-follow-ups | reject`.
