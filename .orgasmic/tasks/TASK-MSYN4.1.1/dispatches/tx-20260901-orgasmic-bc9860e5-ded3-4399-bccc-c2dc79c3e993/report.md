# fix(daemon): org-file denylist case-folds and refuses tmp/; daemon-owned surfaces shared with the writer gate (TASK-MSYN4.1.1)

## Changed
- `crates/orgasmic-daemon/src/writer.rs:38,1754-1758` defines the single `DAEMON_OWNED_SURFACES` constant and uses ASCII-insensitive matching in the writer claim exemption.
- `crates/orgasmic-daemon/src/api.rs:14576-14614` consumes the shared constant, ASCII-folds daemon-owned surface names and `journal.org`, preserves per-surface errors, and rejects `tmp/` dispatch scratch state.
- `crates/orgasmic-daemon/src/api.rs:21221-21284` extends `api::tests::org_file_rewrite_refuses_ledger_paths` with the five required refused paths while retaining allowed cases, and adds `api::tests::org_file_rewrite_agrees_with_writer_daemon_owned_surfaces` for all four shared entries.
- `crates/orgasmic-daemon/src/authz.rs:72` documents `OrgWrite`; `ui/src/lib/capabilities.ts:43` hides Org from members; `ui/src/lib/types.ts:716` adds `org.write`.

## Verification Gates
- `cargo test -p orgasmic-daemon --lib -- org_file authz` — PASS: 24 passed, 0 failed, 0 ignored, 799 filtered; includes `api::tests::org_file_rewrite_refuses_ledger_paths` and `api::tests::org_file_rewrite_agrees_with_writer_daemon_owned_surfaces`. Log: `/tmp/task-msyn4.1.1-gates/final/cargo-test.log`.
- `cargo clippy -p orgasmic-daemon --all-targets -- -D warnings` — PASS. Log: `/tmp/task-msyn4.1.1-gates/final/cargo-clippy.log`.
- `cargo fmt --all --check` — PASS. Log: `/tmp/task-msyn4.1.1-gates/final/cargo-fmt-check.log`.
- `cd ui && npm run typecheck` — PASS after `npm ci` restored ignored `ui/node_modules`; the initial attempt was environment-blocked by `tsc: command not found`. Final log: `/tmp/task-msyn4.1.1-gates/final/ui-typecheck.log`.

## Unmet Criteria
- None.

## Residual Risk
- No live HTTP/APFS probe was run; the exact case variants are covered directly at the production deny predicate, before filesystem resolution.
