# Brief: TASK-FY0TS — per-project ATTACHMENT_STORAGE lfs|local, actionable errors (dec_8DW4V)

Read `AGENTS.md` first. Smallest working diff; reuse what is there. Base: 7af2d428 (main).

## Read first
- `orgasmic task get TASK-FY0TS` (acceptance criteria are the contract) and `orgasmic decision get dec_8DW4V`.
- `crates/orgasmic-daemon/src/ledger_sync.rs`: `attachment_lfs_ready`, `ensure_attachment_lfs_attribute`, `stage_ledger` (its exclude list is where local mode hooks in).
- `crates/orgasmic-daemon/src/node_services.rs`: `finish_upload` (LFS probe + 503), `attachment`/`get_content` (404), `Node { path, ledger }`.
- `crates/orgasmic-core/src/schema.rs`: `ProjectFile::from_org` (add the property here); `crates/orgasmic-core/src/node_services.rs`: `AttachmentRecord`, `read_attachments`, `render_attachments`.
- The daemon's machine id is on the API state (`machine` field, set in lib.rs around the `ApiState` literal).
- How the daemon already loads `project.org` for a project: `ProjectFile::from_org` callers in `api.rs` (~line 2350 and 2420). Reuse that path; do not add a second parser.

## Error-message rule (applies to every message you touch or add)
Each error a user can hit must say (1) what happened, (2) why, (3) the exact next step, in one sentence or two. Name CLI verbs and property keys verbatim. No internal jargon (\"owner\", \"store_root\"), no paths from the daemon home unless the user must open them. Bad: `attachment payload missing`. Good: `attachment payload is not on this machine (uploaded on machine 08c4c046); this project stores attachments locally (:ATTACHMENT_STORAGE: local), not in git`.

## Tests
Extend `crates/orgasmic-daemon/tests/node_services_routes.rs` (helpers `fixture`, `exercise`, `post`, `get`). For the \"local never stages\" proof you may use a remote-backed ledger the way `ledger_sync.rs` unit tests do (`seed_remote`), or assert through `git add --dry-run` on the fixture. Keep the 2 GiB test ignored.

## Gates
- `cargo fmt --all -- --check`
- `cargo clippy -p orgasmic-daemon --all-targets -- -D warnings` and `cargo clippy -p orgasmic-core -- -D warnings`
- `cargo test -p orgasmic-daemon --test node_services_routes`
- `cargo test -p orgasmic-daemon --lib ledger_sync`
- `cargo test -p orgasmic-core`

## Rules
- Never hand-edit `.orgasmic/`; never start a daemon from this worktree; never `cargo test --workspace`.
- Pass `--target-dir` explicitly if you set one; never share a target dir with another worktree.
- Commit on your branch with a descriptive imperative subject (not \"Changed\"). Finish with `orgasmic dispatch finalize --task TASK-FY0TS --summary-file <report> --commit`. Report: what changed, deviations and why, every new/changed error string verbatim, gate counts.
