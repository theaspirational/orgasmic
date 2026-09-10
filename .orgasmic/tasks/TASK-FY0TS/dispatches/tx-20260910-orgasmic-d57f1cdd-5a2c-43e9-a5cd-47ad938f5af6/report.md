# Review: TASK-FY0TS fix round r2 (per-project ATTACHMENT_STORAGE lfs|local)

Reviewed `fb27c25c` against base `7af2d428` (cumulative, 8 files, +685/-51) and the
fix-round delta `868f7aac..fb27c25c` (6 files, +196/-38). Author harness: codex
gpt-5.6-sol high.

**Every finding from the r1 review (P1-P8) and gaps 1, 2 and 4 is genuinely fixed,
verified below with file:line, not taken on the implementer's word.** All six gates
run clean in this worktree. Nothing new introduced by the fix blocks ship. Four
non-blocking findings and four residual test gaps follow.

## Findings

### M1 MEDIUM (perf/ops) - `crates/orgasmic-daemon/src/ledger_sync.rs:311`
The P1 degrade warn fires **every 2 seconds, forever, per project**, and it does not
say which project.

The r2 brief asked for "lfs staging plus one `warn!`". The implementer added exactly
one `warn!` *call site*, but it sits on the unconditional per-tick path:

- `SYNC_INTERVAL = Duration::from_secs(2)` (`ledger_sync.rs:19`), loop at `:999-1004`,
  fanned out over every board entry at `:1011-1018`.
- `stage_ledger` is called unconditionally at `ledger_sync.rs:199`, after only the
  no-origin / not-on-`orgasmic` / unmerged short-circuits. There is no dirty gate.
- The degrade path now **succeeds**, so `sync_ledger_at_with_park`'s exponential
  backoff never engages - unlike the r1 behaviour, nothing throttles it.

Failure scenario: the exact one this feature exists for. An operator hand-edits
`.orgasmic/project.org` and writes `:ATTACHMENT_STORAGE: LFS` (uppercase - the drawer
convention in this repo). Their setting is silently ignored (correct, per the brief),
and the daemon then writes ~43,000 identical warn lines a day. At ~250 B/line that is
~11 MB/day against a 10 MiB roll threshold with `keep = 3`
(`logging.rs:45-48`), so the daemon's entire diagnostic history collapses to roughly
two days and is dominated by one repeated line.

Worse, the line carries no `ledger`/`project` field:

```rust
tracing::warn!(%error, "attachment storage setting is missing, unreadable, or invalid; defaulting to lfs for ledger sync");
```

With more than one project registered the operator cannot tell which `project.org` is
broken. `lib.rs:985` gets this right (`warn!(project, %error, ...)`); the hot path does
not. And because `attachment_storage` wraps the io/parse error in `.context(...)`,
`%error` renders only the top context - the underlying cause is not in the line either.

Fix direction: this is a *state*, not an event. Fold it into the existing per-ledger
`LedgerSyncStatuses` so it also shows up in operator-facing status output, and log only
on transition; or at minimum add the `ledger`/`project` field and dedupe on
(ledger, message) so a steady-state fault logs once.

### M2 MEDIUM (correctness) - `crates/orgasmic-daemon/src/lib.rs:982-987`
Boot migration now skips entirely when `project.org` cannot be read, where the base
ran it. This contradicts the degrade-to-`lfs` rule the same fix round adopted
everywhere else.

```rust
let storage = match ledger_sync::attachment_storage(root) {
    Ok(storage) => storage,
    Err(error) => { warn!(...); continue; }   // <- skips the whole migration
};
```

Base `7af2d428` gated only on the LFS probe, so a project whose `project.org` is
missing or unparseable still got its legacy home-store blobs linked into the ledger.
`migrate_projects` comes from `initial_snapshot.board` (`lib.rs:1198-1208`) - the home
board registration, which does not require the checkout's `project.org` to be readable
right now. Failure scenario: a registered project whose checkout was moved or whose
`project.org` carries the same `LFS` typo boots, migration is skipped with a warn only,
and every attachment on that project 404s while the bytes sit in
`~/.orgasmic/assets/<hash>/blobs/`. Nothing in the suite covers it.

Fix: `attachment_storage(root).unwrap_or_default()` here too, matching
`stage_ledger`. Degrading to `Lfs` is strictly better than skipping - it still runs the
LFS probe and still bails politely if git-lfs is absent.

### L1 LOW (bug) - `crates/orgasmic-daemon/src/node_services.rs:1028-1035`
`relative` is computed on the **success** path, so a `strip_prefix` failure turns a
healthy 200 GET into a 500 - the same shape as the P2 finding this round just fixed.
The value is only ever read inside the `File::open` error closure at `:1042-1051`.

Realistically unreachable today (`repo` is `owner.ledger.parent()` where
`owner.ledger` is `<root>/.orgasmic` canonicalized at `:103`, and `node_dir` is
canonicalized at `:1008` - which is the correct macOS `/var` vs `/private/var` fix, and
the route test's literal assertion on `.orgasmic/meetings/{node}/attachments/{revision}`
proves it holds). But it is free to move the two statements inside the closure and it
removes a 500 the request path does not need.

### L2 LOW (docs/usability) - `crates/orgasmic-cli/src/node.rs:251-253`
The 503 body and `artifacts.md:81-82` both tell the operator to run
`orgasmic node prop set <id> ATTACHMENT_STORAGE local --kind project --project <id>`,
but `prop set`'s own `--kind` help enumerates
`(task, decision, glossary, artifact)` - `project` is missing. An operator who checks
`--help` after the 503 sees the advertised kind isn't a documented value and may
conclude the message is wrong. `crates/orgasmic-cli/src` is in the write scope; this is
a one-word doc edit (same text repeats at `:108`, `:173`, `:284`).

### L3 LOW (correctness, deliberate?) - `crates/orgasmic-daemon/src/api.rs:18220-18223`
A `project.org` that *already* carries an invalid `ATTACHMENT_STORAGE` (e.g. pulled
from a machine where it was hand-edited) now refuses **every** project-node property
edit with the storage error, not just edits that touch that key -
`ProjectFile::from_org` validates the whole proposed file.

Not a wedge: setting a valid value, or `orgasmic node prop unset demo
ATTACHMENT_STORAGE`, makes `proposed` valid and repairs the file, so self-repair
works. But `orgasmic node prop set demo STATUS active` now fails with a message about
a key the operator did not touch. Defensible fail-closed; flagging so it is a conscious
call rather than a side effect. No test pins either direction.

### L4 LOW (hygiene) - commits `868f7aac`, `fb27c25c`
Both subjects are the literal string `TASK-FY0TS: Changed`, with body
`orgasmic dispatch finalize --status done`. The r2 brief asked for a "descriptive
imperative commit subject". Neither commit says what changed.

## Open Questions

1. M1: is the per-tick warn acceptable as-is, or should the degraded state surface
   through `LedgerSyncStatuses` (operator-visible) instead of the log? Right now a
   silently-ignored setting is only discoverable by tailing `daemon.out.log`.
2. M2: was skipping boot migration on an unreadable `project.org` intentional
   fail-closed, or an inconsistency with the P1/P2 degrade rule adopted the same round?

## Verification Notes

All commands run from the review worktree `/Users/aspirational/.orgasmic/worktrees/orgasmic/task-fy0ts-review`
with `CARGO_TARGET_DIR` unset and `--target-dir target` (worktree-local, never shared).
No daemon started, no `cargo test --workspace`, no file edited, no mutating CLI verb run.

| Gate | Result |
|---|---|
| `cargo fmt --all -- --check` | clean (rc=0) |
| `cargo clippy -p orgasmic-core --all-targets -- -D warnings` | clean (rc=0) |
| `cargo clippy -p orgasmic-daemon --all-targets -- -D warnings` | clean (rc=0) |
| `cargo test -p orgasmic-core` | 191 + 20 + 0 passed, 0 failed, 2 ignored (rc=0) |
| `cargo test -p orgasmic-daemon --lib ledger_sync` | 22 passed, 0 failed, 897 filtered (rc=0) |
| `cargo test -p orgasmic-daemon --test node_services_routes` (TEST_CMD) | 3 passed, 0 failed, 1 ignored (rc=0) |

The first attempt at the lint gates ran as a backgrounded shell job and was killed
mid-run by the session (log truncated, daemon-clippy log never created). Re-run in the
foreground; the results above are from the foreground runs. No process left behind.

### P1-P8 and gaps 1/2/4, each verified against the code, not the claim

- **P1 FIXED** - `ledger_sync.rs:309-314`: `.transpose().unwrap_or_else(|error| { warn; None }).unwrap_or_default()`.
  Sync no longer propagates. New test `invalid_or_missing_project_file_does_not_stop_sync`
  (`:1370-1402`) covers **both** directions the r1 review asked for: an invalid uppercase
  `:ATTACHMENT_STORAGE: LFS`, then the file deleted outright, each followed by a real
  `sync_once` and a `git show origin/orgasmic:<path>` assertion. Ran: passes.
- **P2 FIXED** - `get_content`: the mode read moved inside the `File::open` error closure
  (`node_services.rs:1037`), so a `project.org` problem can no longer 400 a healthy
  download. Proven end to end by the route test, which writes the invalid `LFS` value and
  then asserts `200` on the content GET before deleting the payload. `finish_upload`
  (`:858-871`) falls back to `Lfs` with a warn on a read/parse failure while keeping the
  refusal for an unknown value - matching dec_8DW4V's "refused at upload time".
- **P3 FIXED** - `ledger_sync.rs:295-300` passes the literal `".orgasmic/project.org"` to
  both `OrgFile::parse` and `ProjectFile::from_org`. Grepped every added line across
  `node_services.rs`, `ledger_sync.rs`, `schema.rs`: the only `to_string_lossy()` left is
  on the already-`strip_prefix`ed relative path. No absolute path reaches an API body.
- **P4 FIXED** - `node_services.rs:1042-1052` branches three ways on
  `record.machine.as_deref()` vs `state.machine.as_str()`. The route test now asserts the
  same-machine wording (`:920-923`, after a restart on the same `home`) **and** the
  foreign-machine wording (`:951-955`, by rewriting `:MACHINE:` to `foreign-machine` in
  `attachments.org` while the daemon is stopped). Both assert the relative path
  `.orgasmic/meetings/<node>/attachments/<rev>` verbatim, which also proves the
  canonicalization fix.
- **P5 FIXED** - `schema.rs:56-58`: `PROJECT {heading}`, one command with a
  `<lfs|local>` placeholder, trailing clause dropped. Pinned by the core unit test
  (`schema.rs:549-552`).
- **P6 FIXED** - both `owner.ledger.parent().unwrap()` sites replaced with
  `ok_or_else(|| internal(...))` (`node_services.rs:852-856`, `:1004-1006`). The
  pre-existing `owner.path.parent().unwrap()` calls (`:848`, `:966`, `:993`) are
  untouched and were not part of P6.
- **P7 FIXED** - `artifacts.md:83-87` now says `local` stops only new/changed bytes and
  that lfs-era payloads stay in the remote tree until `git rm --cached` + commit, and
  that switching does not rewrite history.
- **P8 FIXED** - `lib.rs:985` warn now reads "missing, unreadable, or invalid".
- **Gap 1 CLOSED** - by the P1 test above.
- **Gap 2 CLOSED** - the route test is now remote-backed for `lfs` too and makes the
  **positive** assertion: it polls until `git show origin/orgasmic:<payload>` succeeds,
  then asserts the payload path is present in `git ls-tree -r --name-only origin/orgasmic`
  (`node_services_routes.rs:838-853`). Not a dry-run.
- **Gap 4 CLOSED** - the re-exec'd child asserts `git lfs version` **fails** before
  relying on the 503 (`node_services_routes.rs:1043-1050`).

### Independent checks beyond the finding list

- **Exclude-pattern depth is correct.** `:(exclude,glob).orgasmic/*/*/attachments/**`
  assumes node dirs sit at exactly `.orgasmic/<collection>/<node>/`.
  `node_kernel::node_dir` (`node_kernel.rs:28-30`) is flat, and all 1019 `node.org` /
  `attachments.org` files in the live ledger at `~/.orgasmic/ledgers/orgasmic` are at that
  depth (`find ... | awk -F/ '{print NF-1}' | sort | uniq -c` -> `1019 3`). No node kind
  escapes the pattern, so no payload leaks to the remote in `local` mode.
  `attachments.org` itself is correctly *not* matched (no `/` after `attachments`), which
  the route test also proves.
- **The advertised CLI verb reaches the validated route.** `orgasmic node prop set`
  (`cli/src/node.rs:496-524`) resolves the base version via `GET /org/node?...&kind=project`
  and POSTs `set_property` to `/org/node/:id/edit` - the exact route and body the new route
  test exercises with `"kind":"project"` (`node_services_routes.rs:795-806`), and
  `org_node_path` for `NodeKind::Project` resolves to `project.root.join(".orgasmic/project.org")`
  (`api.rs:17809-17813`), the same file `ledger_sync::attachment_storage` reads. The CLI is
  a thin wrapper; the only unproven link is the CLI process itself (G3).
- **`ATTACHMENT_STORAGE` in `node_layer_schema_property_keys`** (`api.rs:19097-19104`) is
  cosmetic only - its single use (`api.rs:18010`) feeds a case-correction hint tail. It
  cannot reject or admit a value; all value refusal comes from the new `ProjectFile`
  validation. Adding it is harmless and improves the `attachment_storage=local` typo message.
- **Write-time validation cannot reject an otherwise-valid file.** `ProjectFile::from_org`
  needs only a `PROJECT` heading with `:ID:` (`schema.rs:195-217`), both of which every
  `project.org` already has. The one accidental-rejection case is L3 above.
- **`MACHINE` back-compat.** `read_attachments` uses `h.property("MACHINE")` -> `Option`
  (`core/node_services.rs:121`), the renderer omits the line when `None`
  (`core/node_services.rs:130-134`), and the core test parses a pre-`MACHINE` record to
  `None` (`:207-212`). No reader rejects the missing key. `AttachmentRecord` carries
  `#[serde(default)]`, so the added JSON field is additive for the UI.
- **MSRV.** Workspace `rust-version = "1.88"`. The new code uses inline format args
  (1.58), `let ... else` (1.65), `matches!`, `Option::then`, `Result::transpose`,
  `anyhow::Error::downcast_ref`. Nothing above 1.88.
- **`anyhow` downcast in `finish_upload`** (`node_services.rs:858-866`) is sound:
  `attachment_storage`'s last line is a bare `?` on `Result<_, SchemaError>`
  (`ledger_sync.rs:300`), so the `SchemaError` is stored directly and
  `downcast_ref::<SchemaError>()` matches. The read/parse failures are `.context()`-wrapped
  and correctly fall to the `Lfs` default instead. See G1 for why this is fragile untested.

Classification: M1/M2 are **regressions introduced by this fix round** (M1 new at
`fb27c25c`, M2 new at `868f7aac` and not addressed in r2). L1/L3 are new-at-`fb27c25c`
side effects. L2/L4 are omissions. No flaky, environment-blocked, or pre-existing
failures observed.

### Residual test gaps (none fail today; all are untested behaviour)

1. **`finish_upload`'s refusal of an unknown value is untested.** dec_8DW4V requires
   refusal "at write time **and at upload time**"; only the write path
   (`api.rs`) has a test. The upload-time 400 depends on the `anyhow` downcast at
   `node_services.rs:858-866` - adding a `.context(...)` to `ledger_sync.rs:300` in
   future would silently convert that 400 into a warn plus an `Lfs` fallback with no
   test failing.
2. **The `None` machine branch** (`node_services.rs:1049-1051`, records predating
   `MACHINE`) has no route test. Only the parse-level back-compat is covered.
3. **No test drives the CLI binary** (prior gap 3, not in the r2 fix list). The
   `--kind project --project <id>` invocation printed in the 503 and in `artifacts.md`
   is verified statically only. This is what L2 makes user-visible.
4. **`lfs_mode_without_git_lfs_names_both_fixes`** now proves git-lfs is absent inside the
   child, but still passes if the child harness runs **zero** tests - a future rename or
   filter change makes it a silent no-op. Asserting on the child's `1 passed` line would
   close it.
5. **M1 and M2 are both untested** - no test observes the warn volume, and none covers
   boot migration with an unreadable `project.org`.

## Fix Directions

None blocking. Suggested order for a follow-up task:

1. **M1** - move the degraded-storage state into `LedgerSyncStatuses` and log only on
   transition; at minimum add the `ledger`/`project` field to
   `ledger_sync.rs:311` and dedupe steady-state repeats.
2. **M2** - `ledger_sync::attachment_storage(root).unwrap_or_default()` at
   `lib.rs:982`, dropping the `continue`, so boot migration degrades the way staging and
   both request paths now do. Add the missing-`project.org` boot case to the route suite.
3. **L1** - move the `strip_prefix` / `to_string_lossy` pair
   (`node_services.rs:1028-1035`) inside the `File::open` error closure.
4. **L2** - add `project` to the `--kind` help text in `cli/src/node.rs` (4 sites).
5. **G1/G2** - two short route-test cases: upload against a `cloud` value (assert the 400
   names the key and both accepted values), and a record with `MACHINE` absent (assert the
   "predates machine tracking" wording).
6. **L4** - descriptive commit subjects on the next round.

VERDICT: approve-with-follow-ups
