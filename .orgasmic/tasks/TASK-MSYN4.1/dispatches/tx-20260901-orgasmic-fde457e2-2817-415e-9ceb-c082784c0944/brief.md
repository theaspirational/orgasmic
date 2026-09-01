# Review: TASK-MSYN4.1 — org-file denylist + identity on `POST /org/file`

Fix round for chain-review finding H1 (whole-chain review tx-1c6d2115). Implementer:
codex gpt-5.6-sol, one commit `84bda242`, merged to main as `29f93ba9`.

## What to review

    git diff 29f93ba9^1 29f93ba9

Two files: `crates/orgasmic-daemon/src/api.rs` (+/-) and
`crates/orgasmic-daemon/src/authz.rs` (+5). ~110 lines net.

## The finding this must close (H1, verbatim mechanism)

`reject_ledger_rewrite` string-prefix-matched `.orgasmic/tx` and matched `journal.org`
by file name. MSYN4 moved the authoritative ledger to `.orgasmic/machines/<uuid>/tx/`,
and `.orgasmic/machines/<uuid>/claims.org` and `.orgasmic/views/` were writable too.
`post_org_file` had no identity and no Action check, so the lowest role could
whole-file overwrite the append-only dispatch ledger, forge the cross-machine claim log,
or write derived views.

## What the fix claims

1. `reject_ledger_rewrite` is now one component-wise predicate: any path whose first
   component is `.orgasmic` and second is `machines` | `views` | `tx` is refused, and any
   `.orgasmic/**/journal.org` is refused; `.orgasmic/tx-notes.org` (prefix collision) and
   `.orgasmic/gotchas.org` stay allowed.
2. `post_org_file` now takes `Extension(identity)` and calls
   `resolve_authorized_project(.., Action::OrgWrite)` BEFORE path validation and before
   project loading (a test asserts the project stays `Unloaded` on a 403).
3. New `Action::OrgWrite` ("org.write") is granted to NO member role — admin-only. The
   implementer's argument: whole-file org writes had no member-level home before (they
   were simply unchecked), and the closest sibling floor is admin.
4. `GET /org/file` and the writer's own claim gate (`writer.rs:1752` allowlist for
   `machines | tx | tmp | views`) are unchanged — the daemon must keep writing those
   paths itself.

## Attack these specifically

- **Predicate totality.** What does `validate_org_edit_path` normalise before the
  predicate sees the path? Can `./.orgasmic/machines/...`, `.orgasmic//tx/...`,
  `.orgasmic/tasks/../machines/...`, a `CurDir`/`ParentDir` component, a Windows
  separator, or a symlinked path reach `reject_ledger_rewrite` with a first component
  that is not `Normal(".orgasmic")`? If normalisation is upstream, say where; if a shape
  slips through, that is a HIGH.
- **Order of checks.** Authorization now runs before the org parse and before
  `ensure_loaded_snapshot`. Did replacing `ensure_loaded_snapshot` with
  `resolve_authorized_project` change behaviour for the ADMIN path (project resolution
  when `req.project` is `None`, lazy-load semantics, error shape)? Compare the two
  helpers.
- **The role floor.** `OrgWrite` to nobody means a member with role `editor` can no
  longer save from the UI's `OrgView` (`ui/src/components/OrgView.tsx:111` →
  `postOrgFile`). Is that the right floor, or should `editor` hold `OrgWrite`? Judge
  against what `editor` can already do through node-body / task-update routes: if an
  editor can already mutate the same files through structured verbs, admin-only here is
  inconsistency, not safety; if editors cannot, admin-only is correct. State which.
- **Test honesty.** `authz_org_file_write_refuses_member_before_path_validation` sends an
  INVALID path (`/invalid.org`) and expects 403 — does that prove ordering, or would a
  400 have been swallowed by `expect_err`? Read the assertion, not the name.
- **Anything the predicate now refuses that a legitimate caller relied on.** Grep the
  UI and CLI for org-file writes to `.orgasmic/views/`, `machines/`, or a journal.

Already established — do not re-spend: on the merged tree the manager ran
`cargo test -p orgasmic-daemon --lib -- org_file authz` → 23 passed / 0 failed;
`cargo clippy -p orgasmic-daemon --all-targets -- -D warnings` and
`cargo fmt --all --check` results are in the task's Evidence section by the time you
read this (verify via `orgasmic task get --project orgasmic TASK-MSYN4.1`).

## Rules

- READ-ONLY. No edits, no git writes, no mutating `orgasmic` verbs, nothing against the
  live ledger at `~/.orgasmic/ledgers/orgasmic`.
- File each finding as it appears:
  `orgasmic tx record --project orgasmic --type reviewer.finding --task TASK-MSYN4.1
  --reason "HIGH|MEDIUM|LOW <file:line> — <one sentence>"` (single line).
- Targeted tests only (`cargo test -p orgasmic-daemon --lib <name>`); never the
  workspace; never `ORGASMIC_HOME`; do not read `verify/*/injection.patch`.
- Say what you did not check. Finish with `orgasmic dispatch finalize --summary-file
  <path>` (report only) and end with the explicit verdict sentence:
  APPROVE / APPROVE WITH FOLLOW-UPS / REJECT.
