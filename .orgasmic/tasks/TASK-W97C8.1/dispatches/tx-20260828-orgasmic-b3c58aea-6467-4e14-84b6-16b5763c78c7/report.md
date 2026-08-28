# Review: TASK-W97C8.1 — brief.md + compiled-prompt.md moved to close-time promote

Branch `task-w97c8.1-impl` @ d57d2824 vs main @ 46b015a3.

## Verdict

**FINDINGS** — 1 HIGH, 2 MEDIUM, 2 LOW.

The core relocation is right and the acceptance criteria the assignment names
are met and tested: start writes nothing durable, close lands the full record
in the one path-scoped commit, rollback leaves no `dispatches/<tx>/`, partial
promote retains every tmp copy. Handle discipline (`O_NOFOLLOW` openat) is
preserved for the two new sidecars.

What blocks ship is F-1: the promote path now makes the tmp brief and the tmp
compiled prompt *mandatory inputs to worktree removal*. A dispatch whose tmp
sidecars are absent — which includes every dispatch already open when this
binary is installed — can no longer be closed at all.

---

## Findings

### F-1 HIGH (bug / regression) — a missing tmp brief or compiled prompt blocks the entire close, not just the two new files

`crates/orgasmic-cli/src/manager.rs:7686-7700`
`crates/orgasmic-core/src/paths.rs:250-284`, `:824-850`

When `started_tx` is `Some`, `remove_worktree_required_with_hook` now routes
through `validate_dispatch_record_targets`, which hard-errors if the brief path
is absent from the tx record, or if either sidecar file is missing on disk:

```rust
// manager.rs:7686
let artifacts = match started_tx {
    Some(_) => orgasmic_core::validate_dispatch_record_targets(...),
    None    => orgasmic_core::validate_dispatch_cleanup_targets(...),
}
.map_err(|err| anyhow::anyhow!(err))?;   // <-- bails BEFORE `git worktree remove`
```

Measured (probe against the built `orgasmic-core`, source in
`/tmp/w97probe/src/main.rs`):

```
A missing compiled-prompt -> Some("No such file or directory (os error 2)")
B missing brief           -> Some("No such file or directory (os error 2)")
C brief_path None         -> Some("brief_path required for dispatch promote")
```

Because the `?` fires before `git worktree remove`, `cleanup_dispatch`
(`manager.rs:8009-8013`) classifies it `worktree_failed` → status
`WorktreeFailed`, no report promoted, no evidence, no branch delete, worktree
left on disk. On main the identical close succeeds and promotes
report + stdout + evidence.

Concrete triggers, in order of likelihood:

1. **Upgrade with a dispatch in flight.** A dispatch started by the pre-change
   daemon has its compiled prompt at `dispatches/<tx>/compiled-prompt.md` and
   *nothing* at `<stem>-compiled-prompt.md`. Closing it with the new CLI hits
   case A and cannot complete. This is not hypothetical for a self-hosted tool
   that is reinstalled from source mid-session.
2. `git clean -fdx` / any tmp sweep of `.orgasmic/tmp/dispatch/` between start
   and close (the stem dir is gitignored, so `clean -x` targets it).
3. A `manager.dispatch_started` tx without `BRIEF_PATH`/`CODEX_BRIEF_PATH`
   (`manager.rs:10636` parses it as `Option`) — case C.

The old code could not be broken this way: the brief was never read at close
and the compiled prompt was already durable.

**Fix direction:** the two sidecars are *evidence*, not preconditions. Treat a
missing one the way `stdout.log` already treats emptiness — promote what
exists, and record the gap loudly (`report: brief.md missing from tmp` in
`CLEANUP_ERROR`, or a stub file naming the absence). Keep the hard error only
for a path that exists but fails the safety checks. Concretely: make
`validate_dispatch_record_targets` return `brief_file: None` on `ENOENT`
instead of `Err`, and have `promote_validated_dispatch_attempt` record the
absence rather than `ok_or_else`-ing at `paths.rs:390-398`.

---

### F-2 MEDIUM (correctness) — `compiled-prompt.md` is stem-scoped, so the promoted bundle need not be the one the worker saw

`crates/orgasmic-core/src/paths.rs:288-299`

```rust
pub fn dispatch_compiled_prompt_path(last_path: &Path) -> Result<PathBuf, String> {
    let parent = last_path.parent()...;
    let stem = parent.file_name()...;          // <-- the DIRECTORY name
    Ok(parent.join(format!("{stem}-compiled-prompt.md")))
}
```

`last.txt`/`stdout.log` are `<stem>-<attempt_id>-…`
(`manager.rs:10380-10392`); the compiled prompt is `<stem>-…` with no attempt
component. The stem dir is explicitly shared: it is derived from the `--brief`
file's *basename* (`dispatch_artifact_stem`, `manager.rs:10187-10212`), and
`DispatchArtifactReservation::reserve` (`manager.rs:10232`) retries only on
last/stdout collision — the close test itself keeps a live sibling
`task-dispatch-attempt2-last.txt` in the same dir
(`tests/dispatch.rs:4808`).

So two dispatches whose brief files share a basename share one
`<stem>-compiled-prompt.md`. The second start's `std::fs::write`
(`api.rs:6261`) silently overwrites the first's bundle, and the first close
commits a `compiled-prompt.md` that is not what that worker was given. The
second close then fails outright via F-1, because the first close unlinked the
shared file. Brief item 6 ("no divergence between what the worker saw and what
the record keeps") is therefore not guaranteed by construction — it holds only
because brief basenames happen to be unique by convention today.

**Fix direction:** make it attempt-scoped, mirroring the existing
`dispatch_sibling_artifact_paths_from_last` (`manager.rs:10405-10417`) —
derive the name from the `last.txt` filename by replacing the `-last.txt`
suffix with `-compiled-prompt.md`. Same call-site signature, one function body.

---

### F-3 MEDIUM (correctness / data loss) — sidecar validation enforces no name grammar, so a wrong `BRIEF_PATH` consumes and deletes another attempt's artifact

`crates/orgasmic-core/src/paths.rs:824-850`

`validate_dispatch_sidecar_file` checks `..`, symlink, regular-file, and
parent-is-stem-dir — but unlike its sibling `validate_dispatch_artifact_file`
(`paths.rs:785-821`) it never calls `parse_dispatch_artifact_name` and carries
no equivalent of that function's explicit
`"brief path cannot be deleted as dispatch artifact"` guard. Any regular file
directly in the stem dir is accepted as "the brief".

Measured (probe case D, `BRIEF_PATH` pointed at a sibling attempt's
`task-dispatch-9999-last.txt`):

```
D sibling last.txt accepted as brief; outcome=PromoteOutcome { report_path: Some(...), error: None }
  sibling_still_in_tmp=false
  promoted_brief=Some("SIBLING ATTEMPT REPORT")
```

The sibling attempt's retained report was written into the record as
`brief.md` and then unlinked from tmp — permanent loss of a retained artifact
the stem-dir design exists to preserve (`prune_dispatch_stem_removes_only_selected_attempt_artifacts`).
Reaching this needs a mis-recorded or tampered `BRIEF_PATH` property, which is
why this is MEDIUM and not HIGH, but the guard costs one comparison and its
absence is asymmetric with the last/stdout path right beside it.

Note the positive: handle discipline itself is sound. Both sidecars are opened
through `open_artifact_in_stem_dir` (`paths.rs:760-783`) with
`O_RDONLY | O_NOFOLLOW | O_CLOEXEC` against the retained stem-dir fd, and
unlinked through the same fd, so a worker cannot redirect the copy with a
symlink after validation.

---

### F-4 LOW (hygiene) — rollback now orphans the tmp compiled prompt forever

Failed-dispatch rollback passes `brief_path: None` (`manager.rs:7869`) and the
daemon's own rollback calls `prune_validated_dispatch_attempt` on artifacts
built by `validate_dispatch_cleanup_targets` (`api.rs:7024`). Both leave
`brief_file`/`compiled_prompt_file` as `None`, and
`unlink_validated_attempt_artifacts` (`paths.rs:474-483`) skips what is `None`.
So every rolled-back dispatch leaves `<stem>-compiled-prompt.md` behind in tmp.

The acceptance criterion still holds — the file is gitignored and no
`dispatches/<tx>/` appears (verified: `dispatch_timeout_requests_daemon_cleanup`
passes with its new assertion). This is accumulation, not a correctness break,
and it is partly masked by F-2: the next start overwrites it. Worth naming
because the assignment's design line says rollback "prunes tmp only", and it
now prunes tmp *incompletely*.

---

### F-5 LOW (test) — the new behavior's failure edges are untested

- No test covers a close with a missing brief or missing compiled prompt —
  i.e. the F-1 regression is invisible to the suite. The retrofitted
  `paths.rs` tests all write both sidecars via the new
  `write_dispatch_record_sidecars` helper (`paths.rs:965-972`).
- No test covers any `validate_dispatch_sidecar_file` rejection branch
  (symlink, `..`, outside the stem dir). Its last/stdout sibling has three
  (`validate_dispatch_cleanup_rejects_symlink_artifacts`,
  `…rejects_external_suffix_lookalike`, `…rejects_brief_and_mismatched_pair`).
- `tests/dispatch.rs:4869-4890` proves the four files are reachable at `HEAD`,
  not that they arrived in ONE commit. `git log --oneline -- <record_dir>`
  yielding a single line would prove the stated property; `cat-file -e` per
  file does not. (The assertion is not vacuous — `run_git`
  (`tests/dispatch.rs:281-295`) asserts exit success — it just proves a weaker
  claim than the docstring.)
- No test for the F-2 overwrite (two starts in one stem dir).

---

## Open Questions

1. **Upgrade story for in-flight dispatches.** Is there an accepted answer for
   dispatches open across a runtime reinstall, or is "close them before you
   upgrade" the operator contract? F-1's severity turns entirely on this. If
   the contract exists and is written down somewhere I did not read, F-1 drops
   to MEDIUM.
2. **Is a unique brief basename a guarantee or a habit?** Nothing in
   `build_dispatch_plan` enforces it — `--brief` takes any path
   (`manager.rs:5970`). If it is meant to be a guarantee, it belongs in
   `dispatch_artifact_stem` as a check, not in the convention prose.

---

## Verification Notes

Everything below was run in this worktree on the pinned toolchain
(`rustup run 1.97.1`). No files in the repo were modified.

| Check | Result |
|---|---|
| `cargo test -p orgasmic-core --lib paths::` | 14 passed, 0 failed |
| `cargo test -p orgasmic-cli --bins manager::tests` | 85 passed, 0 failed |
| `cargo test -p orgasmic-cli --test dispatch dispatch_close_promotes_complete_record_only_at_close` | passed |
| `cargo test -p orgasmic-cli --test dispatch dispatch_timeout_requests_daemon_cleanup` | passed |
| `cargo test -p orgasmic-cli --test shipped_conventions` | 5 passed, 0 failed (gate 5/5) |

Read for this review: the full `46b015a3..d57d2824` diff; `paths.rs:200-500`,
`:700-860`, `:960-1340`; `manager.rs:620-670`, `900-960`, `5960-6100`,
`7599-7800`, `7930-8130`, `8179-8210`, `8436-8520`, `10187-10420`,
`10589-10640`, `10835-10875`; `api.rs:6180-6300`, `6578`, `7024`;
`shipped/prompt-studio/conventions/manager-dispatch.org` diff.

Production-path probe (F-1, F-3): a standalone crate at `/tmp/w97probe`
depending on this worktree's `orgasmic-core` by path, calling the real
`validate_dispatch_record_targets` and `promote_validated_dispatch_attempt`
against a real stem dir on disk. Output quoted inline above. A unit test could
not prove F-1's close-blocking consequence without editing the repo, so the
probe proves the validation error and `manager.rs:7686-7700` is quoted for the
propagation to `WorktreeFailed`.

Checked against the brief's eight items:

1. **Complete-or-absent** — start-time durable write is gone; the only write is
   `std::fs::write` to the gitignored tmp stem (`api.rs:6260-6266`). Verified
   by the new `!record_dir.exists()` assertion right after the dispatch
   returns (`tests/dispatch.rs:4786-4791`). Error paths after
   `record_dispatch_started` create nothing under `dispatches/` — the only
   remaining `create_dir_all` on the record dir is inside
   `promote_validated_dispatch_attempt` (`paths.rs:386`). *Caveat:* a promote
   that fails on brief.md or compiled-prompt.md returns `report_path: None`,
   so `promote_and_persist_dispatch_record` (`manager.rs:8453`) skips the
   commit, but the `create_dir_all` already ran — an untracked, gitignored-free
   residue dir can exist. Pre-existing class (main had the same shape for
   report.md), not widened.
2. **Rollback** — no durable dir; verified by the new assertion in
   `dispatch_timeout_requests_daemon_cleanup` (passes; it would have failed on
   main, where start wrote `dispatches/<tx>/brief.md`). Tmp prune is
   incomplete — see F-4.
3. **Handle discipline** — sound; `O_NOFOLLOW` openat, unlink via the retained
   stem-dir fd, unlink strictly after every copy succeeded
   (`paths.rs:434-441`), and `promote_keeps_tmp_when_evidence_copy_fails` now
   asserts both new tmp sidecars survive a partial failure
   (`paths.rs:1292-1300`). Name grammar is the gap — F-3.
4. **Where the brief comes from** — `manager.dispatch_started`'s `BRIEF_PATH`
   (`manager.rs:10636`), which records the *relocated* brief:
   `DispatchArtifactReservation::reserve` rewrites `plan.brief_path` into the
   stem dir (`manager.rs:940-947`) before `materialize_dispatch_brief` writes
   it, so "manager passed a brief outside the stem dir" cannot reach the
   promote — the operator's original `--brief` path is never recorded. Good.
   "Tmp brief deleted before close" and "no BRIEF_PATH recorded" are **not**
   handled — that is F-1, and it fails loud but in the wrong place (it takes
   the worktree removal down with it).
5. **Evidence interplay** — `build_dispatch_evidence` (`manager.rs:8179`)
   never refuses; it always yields JSON. The refusal lives in the core floor
   (`refusing semantically empty dispatch evidence`, observed in probe case D).
   That refusal returns `report_path: Some` + `error`, so
   `manager.rs:8453` **still commits** a record holding
   brief/compiled-prompt/report but no `evidence.json` and no `stdout.log`,
   with tmp retained. So the honest promise is "absent before close,
   possibly-partial after", not "complete-or-absent". This is inherited from
   TASK-W97C8 and is not widened by this change (on main those two files were
   in the dir from start anyway) — flagged for coherence, not as a regression.
6. **Compiled prompt unchanged at close** — yes, no recompilation: the close
   copies the byte-identical file the daemon wrote from the same `bundle`
   variable it handed `spawn_worker_run` (`api.rs:6198`, `6260`). But see F-2:
   identity is not *guaranteed*, because the filename is not attempt-scoped.
7. **Tests** — absent-before-close is asserted on the real production close
   path (full daemon + CLI dispatch/close in `tests/dispatch.rs`), not on
   hand-assigned state; rollback orphan and partial-failure retention are both
   covered. Conventions gate 5/5. Gaps in F-5.
8. **No behavior widened** — confirmed. `validate_dispatch_record_targets`
   delegates to the same `cleanup`/`promote` validators for the worktree and
   last/stdout pair; the `worktree.filter(|w| w.exists())` rewrite at
   `manager.rs:8111-8117` is behaviour-identical to the `match` it replaced; no
   API shape change; report/stdout/evidence promotion unchanged. The daemon
   diff is confined to relocating the write. One narrow behaviour change worth
   noting: the daemon dropped its `create_dir_all` and now writes straight
   into the stem dir (`api.rs:6261`) — safe today because the CLI reserves the
   pair there first, but a non-CLI `/dispatch` caller supplying a `last_path`
   in a nonexistent dir now gets an error where it previously got a directory.

---

## Fix Directions

Ranked; F-1 is the only one I would gate the merge on.

1. **F-1** — `paths.rs:250-284` + `:390-398`: return `None` for a sidecar whose
   file is missing (`ErrorKind::NotFound`), and have the promote record the
   absence instead of `ok_or_else`. Missing evidence should degrade the record,
   never the close. Keep `Err` for exists-but-unsafe.
2. **F-2** — `paths.rs:288-299`: derive the compiled-prompt name from the
   `last.txt` *filename*, not the stem dir name, so it carries the attempt id.
   `dispatch_sibling_artifact_paths_from_last` (`manager.rs:10405`) is the
   pattern to copy. Both the daemon writer and the close reader call the same
   helper, so it is a one-body change.
3. **F-3** — `paths.rs:824`: after computing `file_name`, require it to equal
   `{stem}-brief.md` or `{stem}-compiled-prompt.md`. Two string compares; makes
   the sidecar validator as strict as the artifact validator beside it.
4. **F-5** — three focused tests, all in `paths.rs`: promote with the brief
   deleted, promote with the compiled prompt deleted, and a sidecar-rejection
   test (symlink into the worktree). Plus swap the four `cat-file -e` calls in
   `tests/dispatch.rs:4869` for one `git log --oneline -- <record_dir>` length
   assertion, which is what "one record commit" actually means.
5. **F-4** — either thread the sidecars into the rollback artifacts so the
   prune is total, or have `prune_dispatch_stem_after_worktree` drop the two
   well-known sidecar names best-effort. Lowest value; F-2's fix changes the
   shape of this anyway.
