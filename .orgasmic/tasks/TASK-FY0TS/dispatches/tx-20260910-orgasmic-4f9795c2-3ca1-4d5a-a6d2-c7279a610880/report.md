# Changed

- P1 — `crates/orgasmic-daemon/src/ledger_sync.rs:304-315`: `stage_ledger` now warns once and defaults to LFS when `.orgasmic/project.org` is missing, unreadable, malformed, or has an unknown storage value, so fetch/stage/push continue. `crates/orgasmic-daemon/src/ledger_sync.rs:1370-1408` proves both `:ATTACHMENT_STORAGE: LFS` and a missing project file still push new task files to `origin/orgasmic`.
- P2 — `crates/orgasmic-daemon/src/node_services.rs:971-1056`: attachment lookup no longer reads the storage mode. `get_content` reads it only after payload open fails and defaults to LFS on any read/parse/schema failure. `finish_upload` defaults missing, unreadable, or malformed project files to LFS while still refusing the named `UnknownAttachmentStorage` value at upload time (`:858-873`). The route test rewrites the setting to invalid uppercase `LFS` and proves an existing payload still returns 200 (`crates/orgasmic-daemon/tests/node_services_routes.rs:835-848`).
- P3 — `crates/orgasmic-daemon/src/ledger_sync.rs:293-302`: schema parsing always uses display name `.orgasmic/project.org`; daemon-home absolute paths remain only in logs/context, never HTTP bodies.
- P4 — `crates/orgasmic-daemon/src/node_services.rs:1039-1052`: local 404s compare `record.machine` with `state.machine`, distinguish same-machine, foreign-machine, and pre-machine-tracking records, and name only the safe relative payload path. Route coverage for same and foreign machines is at `crates/orgasmic-daemon/tests/node_services_routes.rs:931-970`.
- P5 — `crates/orgasmic-core/src/schema.rs:56-63`: unknown storage now names `PROJECT <id>` and gives one CLI remediation with the `<lfs|local>` placeholder. Assertions are at `:546-553`.
- P6 — `crates/orgasmic-daemon/src/node_services.rs:852-857,1002-1012`: both `owner.ledger.parent()` request paths use `ok_or_else`; the relative-path construction also avoids panics and canonicalizes the existing node directory so macOS `/var` and `/private/var` aliases cannot leak or miscompare.
- P7 — `shipped/skills/orgasmic/operations/artifacts.md:80-89`: documents that local mode excludes only new/changed bytes, previously pushed LFS payloads remain until `git rm --cached` plus commit, and existing history is not rewritten.
- P8 — `crates/orgasmic-daemon/src/lib.rs:982-988`: boot migration warning now covers missing, unreadable, and invalid settings.
- Gap 1 — the new ledger-sync test exercises invalid and missing `project.org` against a real bare remote.
- Gap 2 — `crates/orgasmic-daemon/tests/node_services_routes.rs:814-833` positively proves the LFS payload path reaches `origin/orgasmic`; the existing local assertion still proves its payload does not.
- Gap 4 — `crates/orgasmic-daemon/tests/node_services_routes.rs:1007-1017` asserts `git lfs version` fails inside the re-executed child before relying on the 503.

Changed user-facing error strings, verbatim:

- `{file}: PROJECT {heading}: unknown :ATTACHMENT_STORAGE: value {value}; accepted values are lfs and local; run `orgasmic node prop set {heading} ATTACHMENT_STORAGE <lfs|local> --kind project --project {heading}``
- `ledger has no parent; repair the project registration and retry`
- `node has no directory; repair the node record and retry`
- `node directory is unavailable; restore it and retry`
- `attachment payload location is outside the ledger; repair the project registration and retry`
- `attachment payload is missing from this machine's ledger at {relative}; this project stores attachments locally, not in git; restore that file from backup or upload it again`
- `attachment payload is not on this machine (uploaded on machine {machine}); this project stores attachments locally, not in git; copy the payload from that machine to {relative}`
- `attachment payload is not on this machine; its record predates machine tracking, and this project stores attachments locally, not in git; locate the original upload machine and copy the payload to {relative}`

Changed operator diagnostics, verbatim:

- `attachment storage setting is missing, unreadable, or invalid; defaulting to lfs for ledger sync`
- `attachment storage setting could not be read; defaulting to lfs for upload`
- `attachment storage setting is missing, unreadable, or invalid; skipping migration`

The unchanged 503 was reverified verbatim from the implementation: `attachment bytes cannot be published to this project's Git remote because git-lfs is not installed; install git-lfs, or run `orgasmic node prop set <project-id> ATTACHMENT_STORAGE local --kind project --project <project-id>` to set `:ATTACHMENT_STORAGE: local` on the project`.

# Verification Gates

- `cargo fmt --all -- --check`: green; 0 formatting differences (`/tmp/fy0ts-r2/gate-fmt.log`).
- `cargo clippy --target-dir "$PWD/target/task-fy0ts-r2-gates" -p orgasmic-daemon --all-targets -- -D warnings`: green (`/tmp/fy0ts-r2/gate-clippy-daemon.log`).
- `cargo clippy --target-dir "$PWD/target/task-fy0ts-r2-gates" -p orgasmic-core --all-targets -- -D warnings`: green (`/tmp/fy0ts-r2/gate-clippy-core.log`).
- TEST_CMD, `cargo test --target-dir "$PWD/target/task-fy0ts-r2-gates" -p orgasmic-daemon --test node_services_routes`: green; 3 passed, 0 failed, 1 ignored (`/tmp/fy0ts-r2/gate-routes.log`).
- `cargo test --target-dir "$PWD/target/task-fy0ts-r2-gates" -p orgasmic-daemon --lib ledger_sync`: green; 22 passed, 0 failed, 897 filtered (`/tmp/fy0ts-r2/gate-ledger-sync.log`).
- `cargo test --target-dir "$PWD/target/task-fy0ts-r2-gates" -p orgasmic-core`: green; 213 passed across test binaries, 0 failed, 2 ignored (`/tmp/fy0ts-r2/gate-core.log`).
- `git diff --check`: green.

# Unmet Criteria

None.

# Residual Risk

- Reviewer gap 3 remains intentionally outside this fix brief: the printed `orgasmic node prop set ... --kind project --project ...` command is statically verified but has no new CLI end-to-end test.
- Switching to `local` does not remove payloads already committed under LFS and does not rewrite Git history; this is the decided non-goal now documented in the operator guide.
