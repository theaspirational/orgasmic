# Brief: TASK-FY0TS fix round r2 — address the review of 868f7aac (dec_8DW4V)

Read `AGENTS.md` first. You start on branch `task-fy0ts-impl` at `868f7aac`, your own earlier commit. Do not rewrite history; add commits. Smallest working diff.

The Claude reviewer rejected the round. Its full report follows. Fix every finding P1–P8 and close the residual test gaps 1, 2 and 4. Keep the error-message rule from the first brief: each error says what happened, why, and the exact next step, names verbs and keys verbatim, and never prints an absolute path from the daemon home.

Decisions already made for you:
- P1: a missing, unreadable or invalid `project.org` never stops the sync loop. Degrade to `lfs` staging plus one `warn!`. Refusal stays at write time (`api.rs` project edit) and upload time (`finish_upload`). Add the `ledger_sync` unit test the reviewer describes (`:ATTACHMENT_STORAGE: LFS` still syncs).
- P2: read the mode only where it is needed for a message or a decision; a `project.org` problem on a GET of an existing payload is never an error. Same in `finish_upload`: on a read failure fall back to `lfs` behaviour.
- P3: display name `.orgasmic/project.org`, never the absolute path.
- P4: compare `record.machine` with `state.machine`. Same machine → say the payload is missing from this machine's ledger and name the relative path under `.orgasmic/`; different machine → current wording; `None` → say the record predates machine tracking. Test both same-machine and foreign-machine cases (a foreign id can be injected by rewriting the record in the fixture through the daemon's own write path, or by constructing the record in the test before the daemon boots).
- P5: one remediation command with a `<lfs|local>` placeholder; drop the trailing clause; say `PROJECT <id>` for the heading.
- P6: `ok_or_else(|| internal(...))`, no `unwrap` on a request path.
- P7: document that `local` stops new bytes only; bytes pushed under `lfs` stay in the remote tree until removed with `git rm --cached` in the ledger.
- P8: warn wording covers missing, unreadable and invalid.
- Gap 2: add a positive remote-backed assertion that `lfs` mode stages the blob (pointer or bytes) into `origin/orgasmic`.
- Gap 4: inside the re-exec'd child, assert `git lfs version` fails before relying on the 503.
- Open question 2 (`git rm --cached` of lfs-era blobs when switching to local): non-goal, documented under P7.

## Gates
- `cargo fmt --all -- --check`
- `cargo clippy -p orgasmic-daemon --all-targets -- -D warnings`; `cargo clippy -p orgasmic-core --all-targets -- -D warnings`
- `cargo test -p orgasmic-daemon --test node_services_routes`
- `cargo test -p orgasmic-daemon --lib ledger_sync`
- `cargo test -p orgasmic-core`

## Rules
- Never hand-edit `.orgasmic/`; never start a daemon; never `cargo test --workspace`; pass `--target-dir` explicitly; never share a target dir.
- Descriptive imperative commit subject. Finish with `orgasmic dispatch finalize --task TASK-FY0TS --summary-file <report> --commit`. Report: per finding P1–P8 and gaps 1/2/4 what changed (file:line), every changed error string verbatim, gate counts.

---
# Reviewer report (verbatim)

# Review: TASK-FY0TS (per-project ATTACHMENT_STORAGE lfs|local)

Reviewed `868f7aac` against base `7af2d428` (`git diff 7af2d428..868f7aac`), 8 files,
+520/-44. Author harness: codex gpt-5.6-sol high.

The shape of the change is right and the acceptance criteria are broadly met: the
drawer property parses with `lfs` as the absent-default, `local` adds one exclude
pathspec, `MACHINE` renders/parses and is backward compatible, and the local-mode
route test proves the payload never reaches a real bare remote's tree (not a
dry-run). Two things block: an unreadable or invalid `project.org` now stops the
entire ledger sync loop with no in-band recovery, and the flagship 404 string is
wrong in the exact case its own new test asserts.

## Findings

### P1 HIGH (bug) — `crates/orgasmic-daemon/src/ledger_sync.rs:305-310`
A bad or missing `.orgasmic/project.org` halts the whole ledger sync, permanently.

`stage_ledger` opens with:

```rust
let storage = ledger.join(".orgasmic").exists()
    .then(|| attachment_storage(ledger))
    .transpose()?;                      // <- propagates
```

`attachment_storage` (`ledger_sync.rs:293-301`) errors when `project.org` cannot be
read, cannot be parsed, or carries an unknown `:ATTACHMENT_STORAGE:` value. That
`?` runs at `ledger_sync.rs:199`, which is **before any fetch or pull** — the first
`git pull --rebase` is at `ledger_sync.rs:207`, inside the push retry loop
(`sync_once_with_park` only short-circuits earlier for "no origin" and mid-rebase).

Failure scenario, one character wide: the drawer keys in this repo are UPPERCASE by
convention, so an operator hand-editing `project.org` (a normal, hand-editable Org
file, and the file this whole feature asks them to edit) writes
`:ATTACHMENT_STORAGE: LFS` or `Local`. The match at `schema.rs:202-213` is
byte-exact lowercase, so it returns `UnknownAttachmentStorage`. From that tick on:

- nothing is staged, nothing is committed, nothing is pushed;
- nothing is **pulled** either, so a fix pushed from another machine can never
  arrive;
- `sync_ledger_at_with_park` records the failure and backs off exponentially, so
  the symptom is silence, not an error the operator sees;
- any machine that already pulled the bad value is wedged identically and must be
  repaired by hand.

Corroborated by the diff itself: the `ledger_sync` seed fixture had to gain
`.orgasmic/project.org` (`ledger_sync.rs:1164-1169`) or the pre-existing suite would
fail — i.e. `.orgasmic/` present without `project.org` is now a hard sync error where
it previously synced fine.

This is also scope drift. dec_8DW4V says "Any other value is refused by name **at
write time and at upload time**" — it does not ask sync to fail.

Fix: `.unwrap_or_default()` (i.e. `Lfs`) plus a `warn!`, instead of `?`. Refusal
stays where the decision put it: `api.rs:18220` (write) and
`node_services.rs:854` (upload).

### P2 MEDIUM (bug) — `crates/orgasmic-daemon/src/node_services.rs:985-986`
Attachment downloads now 400 on a `project.org` problem even when the payload is
present and readable.

`attachment()` computes the storage mode on the **success** path, then
`.map_err(bad)?`. Reading an existing attachment never depended on `project.org`
before. Two ways it fires: (a) any invalid value — the same typo as P1 — turns every
`GET /api/attachments/.../content` into `400 attachment storage mode could not be
read because .orgasmic/project.org is invalid; fix that file and retry`; (b) a
transient read while the sync loop rewrites the live worktree (`git reset --hard`,
`git pull --rebase --autostash`) — `node_services.rs:591` already documents that
"the ledger is a live worktree the sync loop rewrites".

`400 Bad Request` is also the wrong status: nothing is wrong with the request.

Fix: read the mode only inside the `File::open` error branch, defaulting to `Lfs`
when it cannot be read. Same for `finish_upload` (`node_services.rs:854`), where
the identical failure turns a publishable upload into a 400.

### P3 MEDIUM (security) — `crates/orgasmic-core/src/schema.rs:57` via `ledger_sync.rs:301`
The 400 body leaks the absolute ledger path to API clients.

`attachment_storage` passes `path.to_string_lossy()` — the absolute
`/Users/.../.orgasmic/project.org` — as `SchemaError::UnknownAttachmentStorage.file`,
and the `#[error(...)]` string renders `{file}` first. `map_err(bad)` puts that
Display straight in the HTTP body at `node_services.rs:854` and `:986`.

`api.rs:18150` states the opposite rule verbatim: "the Display string itself carries
the absolute file path, which the API never discloses". The write-time validation at
`api.rs:18220` gets this right (it passes the literal `"project.org"`); only the
`ledger_sync` helper leaks.

Fix: pass `".orgasmic/project.org"` as the display name in `attachment_storage`.

### P4 MEDIUM (correctness, user-facing) — `crates/orgasmic-daemon/src/node_services.rs:1011-1015`
The local-mode 404 says "not on this machine" when it *is* this machine.

`record.machine` is never compared to `state.machine`. Failure scenario: local mode,
upload from machine A, the blob is later removed on A (disk cleanup, a stray
`git clean`, an operator deleting `attachments/`), `GET .../content` on A returns

> attachment payload is not on this machine (uploaded on machine \<A\>); this project
> stores attachments locally, not in git

which is self-contradicting and tells the operator to go looking on the machine they
are already standing on. For a task whose whole point is "every error says what
happened and what to do next", this is the headline string.

The new test bakes it in rather than catching it:
`crates/orgasmic-daemon/tests/node_services_routes.rs:869-940` uploads on the test
daemon, restarts the **same** `home` (so the same machine id), deletes the blob, and
asserts exactly that sentence.

Fix: branch on `record.machine.as_deref() == Some(state.machine.as_str())` — for the
local machine, name the missing path and that the payload was lost locally; keep the
current wording for a different machine and for `None` (pre-`MACHINE` records).

### P5 LOW (docs/usability) — `crates/orgasmic-core/src/schema.rs:57`
The remediation clause contradicts itself. For `:ATTACHMENT_STORAGE: cloud` it says
run the command that sets **`lfs`**, then appends "or use `local` instead of `lfs`" —
which is meaningless to someone who typed `cloud`. Separately, `{heading}` is filled
with the project **ID** (`schema.rs:211`), so it prints "heading demo" while the
actual heading is `PROJECT demo`.

Fix: one command with an explicit `<lfs|local>` placeholder, and drop the trailing
clause.

### P6 LOW (bug) — `crates/orgasmic-daemon/src/node_services.rs:853, 985`
`owner.ledger.parent().unwrap()` replaces the previous `Option`-tolerant
`.map(...)`. `owner.ledger` is `<root>/.orgasmic` canonicalized
(`node_services.rs:103`), so `parent()` is `Some` in practice — but this is a
request path, and the old code degraded instead of panicking. Cheap:
`.ok_or_else(|| internal("ledger has no parent"))?`.

### P7 LOW (docs) — `shipped/skills/orgasmic/operations/artifacts.md:81-85`
"files under node `attachments/` directories do not [sync]" overstates `local`. The
exclude only affects `git add`; bytes already committed under `lfs` mode stay in the
remote tree, and their later edits and deletions are silently never staged.

Proven in a scratch repo with the exact pathspec from `ledger_sync.rs:317-327`:
committed `.orgasmic/meetings/n1/attachments/abc`, then in "local mode" modified
`abc` and added `def`, then ran
`git add --all -- .orgasmic ':(exclude).orgasmic/machines' ':(exclude,glob).orgasmic/*/*/attachments/**'`
→ `git diff --cached --name-status` was **empty**. `abc`'s old content stays at HEAD
forever.

If the motive for `local` is keeping bytes off a remote, the doc should say `local`
only stops *new* bytes and that already-pushed ones need `git rm` / history rewrite.

### P8 LOW (docs) — `crates/orgasmic-daemon/src/lib.rs:984`
The warn reads "attachment storage setting is **invalid**; skipping migration" but
also fires when `project.org` is merely missing or unreadable.

## Open Questions

1. Is wedging sync on a bad value a deliberate fail-closed choice? Nothing in
   dec_8DW4V or the acceptance criteria asks for it, and the criteria only mention
   the write path and the 404/503 strings. I read it as an unintended `?`.
2. Should `local` mode also `git rm --cached` blobs left over from an `lfs` era, or
   is leaving them a documented non-goal (P7)?

## Verification Notes

All commands run from the review worktree with `CARGO_TARGET_DIR` pointed at this
worktree's own `target/` (never shared). No daemon started, no
`cargo test --workspace`. Logs under `/tmp/fy0ts/`.

| Gate | Result |
|---|---|
| `cargo fmt --check` | clean (`/tmp/fy0ts/fmt.log`) |
| `cargo clippy -p orgasmic-core --all-targets -- -D warnings` | clean |
| `cargo clippy -p orgasmic-daemon --all-targets -- -D warnings` | clean |
| `cargo test -p orgasmic-core` | 191 passed, 0 failed, 2 ignored; includes the new `schema::tests::project_attachment_storage_defaults_and_rejects_unknown_values` |
| `cargo test -p orgasmic-daemon --test node_services_routes` | 3 passed, 0 failed, 1 ignored; both new tests pass |
| `cargo test -p orgasmic-daemon --lib ledger_sync` | 21 passed, 0 failed |

Production-path checks done by reading, not inference:

- **The advertised CLI command is real and hits the right file.** `orgasmic node
  prop set <id> ATTACHMENT_STORAGE local --kind project --project <id>` exists
  (`crates/orgasmic-cli/src/node.rs:232-260` for the args, `:496-520` for the
  `set_property` POST). `org_node_path` for `NodeKind::Project` resolves to
  `project.root.join(".orgasmic/project.org")` (`api.rs:17808-17812`) — the exact
  file `ledger_sync::attachment_storage` reads (`ledger_sync.rs:294`). Not the home
  board. This was my main suspicion and it is clean.
- **Write-time validation cannot reject an unrelated valid edit.**
  `ProjectFile::from_org` requires only a `PROJECT` heading with `:ID:`
  (`schema.rs:195-217`), both of which any existing `project.org` already has.
- **`MACHINE` back-compat.** `read_attachments` uses `h.property("MACHINE")` →
  `Option`, not the failing `property()` helper (`core/node_services.rs:121`);
  render omits the line when `None`. The UI's `Attachment` type
  (`ui/src/lib/nodeServices.ts:5`) is structural and does not list `machine`, so the
  extra JSON field is not a contract break.
- **Exclude pathspec correctness.** `:(exclude,glob).orgasmic/*/*/attachments/**` —
  under `:(glob)`, `*` does not cross `/`, so it matches
  `.orgasmic/<collection>/<node>/attachments/<rev>` and nothing shallower. Verified
  behaviourally in the scratch repo above.
- **Local-mode staging proof is real, not a dry-run.** The test pushes to a real
  bare remote and asserts on `git ls-tree -r --name-only origin/orgasmic`
  (`node_services_routes.rs:855-866`): `attachments.org` present, no path ending in
  the revision. Good.
- **Boot migration in local mode** is genuinely exercised — the test moves the blob
  to the legacy home store, restarts the daemon, and asserts the node blob is
  re-linked (`node_services_routes.rs:876-905`).

Classification: the P1/P2/P3/P4 defects are **regressions introduced by this
commit**, not pre-existing. No flaky or environment-blocked failures observed.

Residual test gaps (none of these fail today; all are untested behaviour):

1. No test for `.orgasmic/` present with a missing or invalid `project.org` — P1 is
   untested in both directions, and the fixture change at `ledger_sync.rs:1164-1169`
   is what hides it.
2. No **positive** remote-backed assertion that `lfs` mode still stages the blob
   into the tree. "lfs mode unchanged" rests on the pre-existing non-remote
   `exercise()` tests plus the 503 path.
3. No test drives the CLI verb end to end; only the HTTP route is exercised, so the
   `--kind project --project <id>` invocation printed in the 503 and in
   `artifacts.md` is unverified at runtime (I verified it statically, above).
4. `lfs_mode_without_git_lfs_names_both_fixes` re-execs itself with a PATH
   containing only a `git` symlink. It passes here, but it will silently become a
   no-op-shaped pass on a box where git's `--exec-path` also carries `git-lfs`.
   Consider asserting inside the child that `git lfs version` fails.

## Fix Directions

Blocking (P1): in `stage_ledger`, replace `.transpose()?` with
`.transpose().unwrap_or_else(|error| { warn!(%error, "..."); None })` — or simply
`attachment_storage(ledger).unwrap_or_default()` — so an unparseable
`project.org` degrades to `lfs` staging plus a warning instead of stopping the sync
loop. Add a `ledger_sync` unit test: seed a ledger whose `project.org` carries
`:ATTACHMENT_STORAGE: LFS`, tick, assert the tick still syncs.

Blocking (P4): compare `record.machine` against `state.machine` in `get_content` and
emit a "missing from this machine's ledger at <relative path>" message for the local
case. Update the test at `node_services_routes.rs:869-940` to cover both a
same-machine and a foreign-machine record, since it currently pins the wrong one.

Non-blocking: P2 (move the mode read into the error branch, in both `get_content`
and `finish_upload`), P3 (relative display path in `attachment_storage`), P5
(rewrite the remediation clause with a `<lfs|local>` placeholder), P6
(`ok_or_else` instead of `unwrap`), P7 (doc the pre-existing-blob caveat), P8
(widen the warn wording).

VERDICT: reject
