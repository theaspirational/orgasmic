# Review — TASK-MSYN4.1: org-file denylist + identity on `POST /org/file`

Reviewed `git diff 29f93ba9^1 29f93ba9` (commit `84bda242`): `crates/orgasmic-daemon/src/api.rs`,
`crates/orgasmic-daemon/src/authz.rs`. Read-only; no edits, no git writes.

## Verdict

**APPROVE WITH FOLLOW-UPS.**

The structural predicate is a real improvement and acceptance criteria 1–3 are met as
written. Two residual gaps in the same predicate (case folding, `tmp`) should be closed in
a follow-up, and one premise of the original H1 finding turns out to be false — worth
recording so nobody re-derives it.

## Findings

### MEDIUM — `crates/orgasmic-daemon/src/api.rs:14575` — case-insensitive filesystems bypass the predicate

`reject_ledger_rewrite` compares path components with `==` against the byte strings
`"machines"`, `"views"`, `"tx"`, and `"journal.org"`. `validate_org_edit_path`
(`api.rs:14553`) only requires the first component to equal `.orgasmic` exactly and the
extension to be `org`; it does not case-fold anything else. macOS APFS — the platform this
daemon actually runs on — is case-insensitive by default, verified on this host:

    $ ls /Users/aspirational/.orgasmic/ledgers/orgasmic/.orgasmic/TX
    2026-06.org  2026-07.org  2026-08.org
    $ ls /Users/aspirational/.orgasmic/ledgers/orgasmic/.orgasmic/Views
    board.org  decisions.org  glossary.org

So these all pass `validate_org_edit_path`, pass `reject_ledger_rewrite` (second component
is `"TX"`/`"Machines"`/`"Views"`, matching no arm), and then resolve on-disk to the real
daemon-owned files when the writer opens `project.root.join(rel)`:

    .orgasmic/TX/2026-09.org
    .orgasmic/Machines/08c4c046-.../claims.org
    .orgasmic/Machines/08c4c046-.../tx/2026-09.org
    .orgasmic/Views/board.org
    .orgasmic/tasks/TASK-X/Journal.org

Impact: whole-file overwrite of the append-only tx ledger, of the cross-machine claim log,
or of a derived view — exactly the class the acceptance criterion says the predicate
refuses. Not a privilege boundary crossing (see the LOW premise finding below: only Admin
can reach this route at all), which is why this is MEDIUM and not HIGH, but the guard's
whole purpose is to stop the admin UI from destroying the ledger and it does not hold on
the primary platform. Windows is likewise case-insensitive.

Fix direction: fold the two matched components before comparing, e.g.
`surface.to_ascii_lowercase()` against the three names, and the same for the `journal.org`
file-name check. One-line change per arm, and add the case variants to
`org_file_rewrite_refuses_ledger_paths`.

### MEDIUM — `crates/orgasmic-daemon/src/api.rs:14575` — `tmp` is the fourth daemon-owned surface and is not refused

The writer's own claim gate lists the daemon-owned surfaces explicitly
(`crates/orgasmic-daemon/src/writer.rs:1752`):

    if matches!(collection, "machines" | "tx" | "tmp" | "views") { return Ok(()); }

The new predicate mirrors three of those four. `tmp` is omitted, so
`.orgasmic/tmp/**/*.org` is exempt from the writer's claim gate **and** allowed by the
denylist. That directory is not scratch space — it holds live dispatch prompt bodies. On
the live ledger today:

    .orgasmic/tmp/dispatch/implementation/implementation.org
    .orgasmic/tmp/dispatch/task-AJP4A.1.1/review.org
    .orgasmic/tmp/dispatch/task-AJP4A.1.1/fix.org
    .orgasmic/tmp/dispatch/task-95SGV.2-body.org        (30+ more)

Rewriting one of these changes the instructions a dispatched agent is about to read.
Again admin-only to reach, so MEDIUM rather than HIGH.

Fix direction: add `| "tmp"` to the surface match (message: dispatch scratch state, not a
hand-editable org file), or better, derive both lists from one shared constant so
`writer.rs` and `api.rs` cannot drift again. Pin with a `.orgasmic/tmp/dispatch/x.org`
case in the existing test.

### LOW — `crates/orgasmic-daemon/src/api.rs:896-918` — H1's stated mechanism was already false

H1 says "`post_org_file` also carries no identity/Action check (pre-existing), so the
lowest role reaches it." It did not. `("POST", "/org/file")` is absent from
`MEMBER_ALLOWED_ROUTES` (`api.rs:896-918`), and `identity_middleware` (`api.rs:959-970`)
rejects any `Identity::Member` request whose matched route template is not on that list,
with 403 `"forbidden for this member role"`, before the handler runs. `GET /org/file` is
absent too.

This does not make the fix wrong — the `Action::OrgWrite` gate is correct, cheap
defense-in-depth and closes the hole if `/org/file` is ever added to the coarse allowlist.
But it means criterion 2 closed no member-reachable hole: `Identity::Admin` was the only
identity that could reach `post_org_file` before this commit, and still is. Recording it so
the residual gaps above are severity-rated consistently.

### LOW — `ui/src/lib/capabilities.ts:29` — members see an Org nav item that 403s (pre-existing, out of scope)

`NAV_CAPABILITY` has no `org` key and `MEMBER_HIDDEN_PAGES` is `{activity, prompts}`, so
`navPageVisible` returns `true` for `org` for every member. Both `GET` and `POST /org/file`
are admin-only at the middleware, so a member opening `…/org` gets a 403 on load and, if
they type anyway, a `Save failed` toast from `OrgView.tsx:108-118`. That contradicts the
comment on `MEMBER_HIDDEN_PAGES` — "so members never see nav that 403s". Pre-existing
(unchanged by this diff), not this task's scope; one-word fix is adding `'org'` to
`MEMBER_HIDDEN_PAGES`.

### LOW — `crates/orgasmic-daemon/src/authz.rs:26`, `ui/src/lib/types.ts:706` — vocabulary/doc drift

`org.write` is an addition beyond dec_KF2MR's literal action list
(`.orgasmic/decisions/dec_KF2MR/node.org:16`). `authz.rs` documents `ProjectRead` as
exactly that kind of addition in its `role_capabilities` doc comment; `OrgWrite` gets no
equivalent note. Separately, `MemberCapability` in `ui/src/lib/types.ts:706` does not list
`'org.write'`, which admin `GET /me` now returns (`api.rs:1049`) — harmless today because
`MeProject.capabilities` is typed `string[]`, but the union is now incomplete.

## What I checked and found correct

- **Predicate totality against traversal.** `validate_org_edit_path` (`api.rs:14553`)
  rejects any component that is not `Component::Normal`, so `./`, `../`, absolute paths and
  `.orgasmic/tasks/../machines/x.org` are all 400s before `reject_ledger_rewrite` is
  reached. Repeated separators (`.orgasmic//tx/x.org`) collapse inside
  `Path::components`, so they hit the `tx` arm normally. A backslash path
  (`.orgasmic\tx\x.org`) is one `Normal` component on unix and fails the
  `starts_with(".orgasmic")` check with 400. `.orgasmic` and `.orgasmic.org` both fail the
  extension / prefix checks. The only shape that slips through is case (finding 1).
- **Order of checks — no drift on the ADMIN path.** `resolve_authorized_project`
  (`api.rs:16873-16892`) is `ensure_loaded_snapshot` (`api.rs:16853-16869`) verbatim plus
  one `authz::require` line: same `select_catalog_project_id` for `req.project == None`,
  same `ensure_project_loaded` single-flight, same `ApiError::unavailable` error shape, same
  fresh snapshot. `authz::require` returns `Ok` immediately for `Identity::Admin`
  (`authz.rs:156`). Admin behaviour is unchanged.
- **The role floor is right, and it is decision-conformant.** dec_KF2MR
  (`.orgasmic/decisions/dec_KF2MR/node.org:19`) rules explicitly: "non-artifact mutation
  routes stay admin-only (editor means 'drives the artifact loop', not 'rewrites the
  graph')." `role_capabilities` (`authz.rs:74-97`) confirms `editor` holds no write action
  of any kind — its only mutation capability is `ArtifactsGenerate`. There is no structured
  verb through which an editor can mutate `.orgasmic` org files, so admin-only for
  `OrgWrite` is correct, not an inconsistency.
- **Test honesty.** `authz_org_file_write_refuses_member_before_path_validation` does not
  stop at `expect_err`: it asserts `error.status == StatusCode::FORBIDDEN`. A 400 from
  `validate_org_edit_path` running first would fail that assertion, so the deliberately
  invalid `/invalid.org` path does prove the ordering. The second assertion
  (`project_loads["proj-a"].state == ProjectLoadState::Unloaded`) independently proves
  authorization runs before project loading. Name matches behaviour.
- **No legitimate caller lost.** Repo-wide grep for `org/file`: the only write callers are
  `ui/src/lib/api.ts:498` → `ui/src/components/OrgView.tsx:111` (admin-only path) and four
  daemon integration tests using the admin bearer. There is no CLI verb for
  `POST /org/file` (`api.rs:17028` says so). Nothing writes `.orgasmic/views/`,
  `machines/`, or a journal through this route; views are built by
  `orgasmic views build` (`crates/orgasmic-cli/src/main.rs:1982`), so the new error
  message points at a real operation.
- **Targeted tests pass on the merged tree.**

      cargo test -p orgasmic-daemon --lib -- org_file_rewrite_refuses_ledger_paths \
        authz_org_file_write_refuses_member_before_path_validation \
        org_file_write_allows_admin_on_an_allowed_path
      test result: ok. 3 passed; 0 failed; 819 filtered out

## Open Questions

1. Should the refused-surface list live in one place shared by `writer.rs:1752` and
   `api.rs:14575`? They are the same concept ("daemon-owned state") and have already
   drifted once by one entry.
2. Is `.orgasmic/tmp/dispatch/**/*.org` meant to be reachable by *any* HTTP write surface,
   or should it be excluded from `validate_org_edit_path` entirely rather than denylisted?

## Verification Notes

- Ran only the three targeted tests above (passed). Did not re-run the manager's 23-test
  `org_file authz` filter, clippy, or fmt — the brief states those are already established.
- The case-insensitivity finding is proven by a filesystem probe on this host
  (`ls .orgasmic/TX`, `ls .orgasmic/Views` both list real contents; `diskutil info /` →
  APFS) plus code reading of `validate_org_edit_path` / `reject_ledger_rewrite`. I did not
  execute an end-to-end `POST /org/file` against a running daemon — that would have meant
  writing to a live ledger, which the brief forbids. The remaining risk is that some
  layer between the handler and the file open canonicalises case; I found none —
  `post_org_file` computes `project.root.join(&rel)` and hands it to the writer unchanged
  (`api.rs:14509-14523`).
- The `tmp` finding is proven by reading `writer.rs:1737-1755` and by `find` over the live
  ledger showing 30+ `.org` files under `.orgasmic/tmp/dispatch/`. Not executed either,
  for the same reason.

### Not checked

- **Symlinks.** If `.orgasmic/foo.org` is a symlink into `machines/`, `views/`, or `tx/`,
  `reject_ledger_rewrite` inspects the link path and the writer follows the link. This is
  pre-existing, requires local filesystem write to set up, and I did not test it.
- `POST /org/node/:id/edit`, `/delete`, `/regenerate`, `/submit` and the WS surface —
  untouched by this diff.
- `GET /org/file` — deliberately unchanged per the brief; I confirmed it is admin-only via
  `MEMBER_ALLOWED_ROUTES` but did not audit what it can disclose.
- `verify/*/injection.patch` — not read, per the brief.

## Fix Directions

1. Case-fold the two matched components and the `journal.org` file name in
   `reject_ledger_rewrite` (`api.rs:14575`); extend
   `org_file_rewrite_refuses_ledger_paths` with `.orgasmic/TX/2026-09.org`,
   `.orgasmic/Machines/<uuid>/claims.org`, `.orgasmic/Views/board.org`,
   `.orgasmic/tasks/TASK-X/Journal.org`.
2. Add `tmp` to the refused surfaces, ideally by lifting `["machines","tx","tmp","views"]`
   into one shared constant consumed by both `writer.rs:1752` and `api.rs:14575`.
3. Optional, separate task: add `'org'` to `MEMBER_HIDDEN_PAGES`
   (`ui/src/lib/capabilities.ts:44`) and `'org.write'` to `MemberCapability`
   (`ui/src/lib/types.ts:706`).

**APPROVE WITH FOLLOW-UPS.**
