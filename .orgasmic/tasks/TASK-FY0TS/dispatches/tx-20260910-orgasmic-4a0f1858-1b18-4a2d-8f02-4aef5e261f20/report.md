# Changed

- Added `AttachmentStorage::{Lfs, Local}` to `ProjectFile`; absent and explicit `lfs` select LFS, explicit `local` selects local storage, and any other value returns `SchemaError::UnknownAttachmentStorage`.
- Reused the existing `orgasmic node prop set <project-id> ATTACHMENT_STORAGE <value> --kind project --project <project-id>` path. Project-node edits now validate the rewritten file through `ProjectFile::from_org` before commit, so unknown values are refused without a new CLI verb.
- Added optional `MACHINE` parsing/rendering to `AttachmentRecord`; new uploads record `ApiState.machine`, while old records without it still parse.
- Made ledger staging skip `.orgasmic/*/*/attachments/**` in local mode and preserve the existing LFS attribute/staging path in LFS mode.
- Made local upload finish skip the git-lfs readiness probe. Boot migration still links/copies legacy payloads for local projects without probing LFS.
- Added mode-specific missing-payload 404s and an actionable no-git-lfs 503.
- Added route coverage for invalid property writes, both 404 messages, no-git-lfs 503, local remote-tree exclusion, LFS/default upload behavior, `MACHINE`, and local boot migration. Added focused core schema/record compatibility coverage and the operator paragraph under `## Attachments`.
- Changed files: `crates/orgasmic-core/src/schema.rs`, `crates/orgasmic-core/src/node_services.rs`, `crates/orgasmic-daemon/src/api.rs`, `crates/orgasmic-daemon/src/ledger_sync.rs`, `crates/orgasmic-daemon/src/lib.rs`, `crates/orgasmic-daemon/src/node_services.rs`, `crates/orgasmic-daemon/tests/node_services_routes.rs`, `shipped/skills/orgasmic/operations/artifacts.md`.
- No CLI source change was needed because the project-node property verb already exists and resolves project nodes with `--kind project`. No generated or mechanical tracked files changed.

New/changed user-facing error strings (format placeholders are shown verbatim):

- ``{file}: heading {heading}: unknown :ATTACHMENT_STORAGE: value {value}; accepted values are lfs and local; run `orgasmic node prop set {heading} ATTACHMENT_STORAGE lfs --kind project --project {heading}` or use `local` instead of `lfs``` 
- ``attachment bytes cannot be published to this project's Git remote because git-lfs is not installed; install git-lfs, or run `orgasmic node prop set {} ATTACHMENT_STORAGE local --kind project --project {}` to set `:ATTACHMENT_STORAGE: local` on the project``
- `attachment payload is not on this machine (uploaded on machine {}); this project stores attachments locally, not in git`
- `attachment payload missing; run git lfs pull in the ledger`
- `attachment storage mode could not be read because .orgasmic/project.org is unavailable; restore that file and retry`
- `attachment storage mode could not be read because .orgasmic/project.org is invalid; fix that file and retry`

Changed internal boot warning:

- `attachment storage setting is invalid; skipping migration`

# Verification Gates

- `cargo fmt --all -- --check` — green (exit 0).
- `cargo clippy -p orgasmic-daemon --all-targets -- -D warnings` — green (exit 0).
- `cargo clippy -p orgasmic-core -- -D warnings` — green (exit 0).
- `cargo test -p orgasmic-daemon --test node_services_routes` — green: 3 passed, 0 failed, 1 ignored (the existing 2 GiB opt-in test).
- `cargo test -p orgasmic-daemon --lib ledger_sync` — green: 21 passed, 0 failed, 897 filtered out.
- `cargo test -p orgasmic-core` — green: 213 passed across unit/integration targets, 0 failed, 2 ignored.
- `git diff --check` — green.
- Extra `cargo test -p orgasmic-cli --test cli_parity` — 8 passed, 1 failed on three pre-existing unrelated shipped references (`orgasmic node show` twice and `orgasmic task list` once). The new attachment command resolved successfully and was not listed in the failure.

# Unmet Criteria

None.

# Residual Risk

- The 2 GiB streaming test remains ignored as directed and was not run.
- The unrelated pre-existing CLI parity failure remains outside this task's write scope; evidence is in `/tmp/task-fy0ts-cli-parity.log`.
