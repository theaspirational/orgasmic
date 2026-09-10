# Brief: TASK-V2859 — attachment blobs into the ledger under Git LFS (dec_X2046)

Read `AGENTS.md` first. Smallest working diff; reuse what is there.

## Why
`attachments.org` sits next to `node.org` in the ledger branch (backed up by push). The bytes do not: `finish_upload` in `crates/orgasmic-daemon/src/node_services.rs` hard-links them into `store_root(project)/blobs/<sha256>` = `~/.orgasmic/assets/<hash>/blobs/`, outside any git repo. A machine loss keeps the record and loses the payload.

## What to change (all in orgasmic-daemon)
1. `finish_upload`: publish to `<node dir>/attachments/<sha256>` where `<node dir>` = `owner.path.parent()` (the dir holding `node.org` and `attachments.org`; `commit_extra` already writes `attachments.org` there). Keep the hard-link-then-AlreadyExists idiom; fall back to copy on `CrossDevice` (EXDEV), since the ledger may sit on another volume than the home assets store. Upload staging (`upload_dir`, `payload`) stays where it is.
2. `attachment()` (the helper `get_content`/HEAD use): resolve the path from the node dir. Change the 404 text to something true, e.g. `"attachment payload missing"`.
3. Quota: the `read_dir(base.join("blobs"))` scan in `start_upload` must count the new location. Simplest true answer: walk `<ledger>/.orgasmic/*/*/attachments/*` sizes for the project. Keep the existing `ponytail:` comment style.
4. `.gitattributes`: in `ledger_sync.rs`, where the ledger checkout is prepared (before the first `git add --all -- .orgasmic`), write `.orgasmic/.gitattributes` idempotently (only when missing or the line is absent) containing:
   `*/*/attachments/** filter=lfs diff=lfs merge=lfs -text`
   Do not require the `git-lfs` binary at test time: writing the attribute file is enough for the tests; only note in the doc that the remote needs LFS.
5. Boot migration (lib.rs boot, or the ledger prepare step, whichever is smaller): for each project, read every `attachments.org` under `.orgasmic/*/*/`, and for each record whose `<node dir>/attachments/<revision>` is missing but `store_root/blobs/<revision>` exists, hard-link/copy it in. Leave old files. Idempotent, logs a count, never fails boot on a single bad file.
6. Doc: one short paragraph in `shipped/skills/orgasmic/operations/artifacts.md` (or the doc that already describes attachments, if any): bytes live in the ledger under LFS at `<node>/attachments/<sha256>`; the ledger remote must have LFS enabled; legacy home blobs migrate at boot.

## Tests (crates/orgasmic-daemon/tests/node_services_routes.rs)
Use the existing `fixture()` / `exercise()` helpers. Prove: blob file exists under the node folder after finish; GET content serves it; a second finish of the same bytes is fine; `.orgasmic/.gitattributes` holds the LFS rule after the daemon has prepared the ledger; a legacy blob placed in `store_root/blobs` with a matching `attachments.org` record is served after boot (migration).

## Gates
- `cargo fmt --all -- --check`
- `cargo clippy -p orgasmic-daemon --all-targets -- -D warnings`
- `cargo test -p orgasmic-daemon --test node_services_routes`
- `cargo test -p orgasmic-daemon --lib ledger_sync` (or the ledger_sync unit tests by name)

## Rules
- Never hand-edit `.orgasmic/`; never start a daemon (`orgasmic daemon start`) from this worktree.
- Pass `--target-dir` explicitly if you set one; never share a target dir with another worktree.
- Commit on your branch. Finish with `orgasmic dispatch finalize --task TASK-V2859 --summary-file <report> --commit`. The report: what changed, deviations from this brief and why, gate output counts.
