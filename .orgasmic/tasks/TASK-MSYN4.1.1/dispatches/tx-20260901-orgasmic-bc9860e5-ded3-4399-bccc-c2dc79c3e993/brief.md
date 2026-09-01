# TASK-MSYN4.1.1 — case-fold the org-file denylist, refuse tmp/, share the surface list

Fix round for the review of TASK-MSYN4.1 (merged `29f93ba9`). The heading above carries the
findings with file:line. This brief is only the delta.

## Read first
1. `crates/orgasmic-daemon/src/api.rs` — `reject_ledger_rewrite` (~14575) as merged in
   `29f93ba9`, and `validate_org_edit_path` just above it.
2. `crates/orgasmic-daemon/src/writer.rs:1752` — the writer claim gate's exemption
   `matches!(collection, "machines" | "tx" | "tmp" | "views")`. Same concept, already
   drifted by one entry. Both sites must read ONE constant after this round.
3. The existing test `org_file_rewrite_refuses_ledger_paths` (api.rs tests) — extend it.
4. `ui/src/lib/capabilities.ts` (`MEMBER_HIDDEN_PAGES`), `ui/src/lib/types.ts`
   (`MemberCapability`), and the `ProjectRead` doc note in
   `crates/orgasmic-daemon/src/authz.rs` — three one-liners.

## Target
- One `pub(crate) const DAEMON_OWNED_SURFACES: [&str; 4] = ["machines", "tx", "tmp", "views"]`
  (name yours; place it where both `writer.rs` and `api.rs` can import it without a new
  module) consumed by both sites.
- `reject_ledger_rewrite`: compare the second component and the `journal.org` file name
  after `to_ascii_lowercase()` (or `eq_ignore_ascii_case`). Keep the per-surface messages;
  `tmp` gets its own ("dispatch scratch state, not a hand-editable org file").
- Tests: add `.orgasmic/TX/2026-09.org`, `.orgasmic/Machines/<uuid>/claims.org`,
  `.orgasmic/Views/board.org`, `.orgasmic/tasks/TASK-X/Journal.org`,
  `.orgasmic/tmp/dispatch/x.org` → refused. Keep the allowed cases. Add one test that the
  writer gate and the API predicate agree on every entry of the shared constant.
- UI/doc: `'org'` in `MEMBER_HIDDEN_PAGES`; `'org.write'` in `MemberCapability`; doc note on
  `Action::OrgWrite` mirroring the `ProjectRead` one.

## Invariants
- The writer must still be able to write all four surfaces itself — you are sharing the
  LIST, not changing the writer's behaviour.
- No change to `GET /org/file`, no change to which roles hold which Action.
- Never touch `.orgasmic/` state; never set `ORGASMIC_HOME`; verify task state only via
  `orgasmic task get --project orgasmic TASK-MSYN4.1.1`.

## Gates (exactly these; redirect cargo output to a file, never pipe)
    cargo test -p orgasmic-daemon --lib -- org_file authz
    cargo clippy -p orgasmic-daemon --all-targets -- -D warnings
    cargo fmt --all --check
    cd ui && npm run typecheck

## Finish
Commit:
    fix(daemon): org-file denylist case-folds and refuses tmp/; daemon-owned surfaces shared with the writer gate (TASK-MSYN4.1.1)
Report file:line changes, exact test names + counts from the logs, and anything not verified.
Terminal action: `orgasmic dispatch finalize --summary-file <path> --commit`
