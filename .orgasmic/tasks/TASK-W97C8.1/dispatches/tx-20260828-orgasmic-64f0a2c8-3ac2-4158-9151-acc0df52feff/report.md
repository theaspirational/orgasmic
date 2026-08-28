# Review: TASK-W97C8.1 round 2 — close-time promote of brief + compiled prompt

Branch `task-w97c8.1-impl-r2` @ 0884b10c vs main @ 46b015a3.
Round-1 review (d57d2824): 1 HIGH, 2 MEDIUM, 2 LOW.

## Verdict

**APPROVE-WITH-FOLLOW-UPS** — all five round-1 findings verified fixed; four
LOW residuals named below, none ship-blocking.

Every acceptance criterion in the assignment is met and is now proved on the
production close path, not just in unit tests: the tracked record dir does not
exist before close, holds brief.md + compiled-prompt.md + report.md +
evidence.json + stdout.log after close, arrives in exactly one commit, and a
rolled-back dispatch leaves neither a `dispatches/<tx>/` folder nor a tmp
compiled prompt.

---

## Round-1 findings: fix verification

### F-1 HIGH — missing tmp sidecar blocked the whole close → **FIXED**

`paths.rs:262-296` now returns `None` for an absent brief (`brief_path` is
`None`, or `symlink_metadata` is `ENOENT`) instead of `Err`, and
`validate_dispatch_compiled_prompt` (`paths.rs:353-376`) returns `Ok(())` when
`validate_dispatch_sidecar_file` reports `ENOENT`.
`promote_validated_dispatch_attempt` (`paths.rs:448-470`) pushes
`"<name> missing from tmp"` onto an `errors` vector and keeps promoting.

Round-1 probe cases re-checked against the new core:

| case | round 1 | round 2 |
|---|---|---|
| A missing compiled prompt | blocked close | close completes; `compiled-prompt.md missing from tmp` in `CLEANUP_ERROR` |
| B missing brief | blocked close | close completes; `brief.md missing from tmp` in `CLEANUP_ERROR` |
| C `brief_path` = `None` | `Err("brief_path required for dispatch promote")` | error string is gone from the crate (`grep` over `paths.rs`: only `last_path required …` remains at `:222`, `:243`, `:267`); `None` takes the same branch as B |

A and B are now covered by real tests through `cleanup_dispatch`, not a probe —
`manager.rs:13313-13366` (`dispatch_close_completes_when_brief_sidecar_is_missing`,
`…_compiled_prompt_sidecar_is_missing`). Both pass and assert the three things
the finding demanded: worktree removed, `report.md`/`evidence.json`/`stdout.log`
promoted, gap named in the cleanup error. The remaining sidecar is still
promoted, so one gap does not cost the other.

Exists-but-unsafe still hard-errors: `validate_dispatch_sidecar_file`
(`paths.rs:895-937`) returns `Err` for a symlink, a `..` component, a wrong
name, or a parent that is not the stem dir — only `ENOENT` is soft.

Non-blocking is real end to end, not just at the core boundary: a `partial`
status only triggers `eprintln!("warning: …")` (`manager.rs:1385`) and a
`dispatch-status --cleanup-failed` row (`manager.rs:2931`, `10796`);
`repersist_dispatch_record_best_effort` (`manager.rs:8664`) returns early
because the record is already in history. Nothing bails.

**Upgrade scenario, traced end to end** (dispatch started by the pre-W97C8.1
daemon, closed by this CLI): the record dir already holds the start-written
`brief.md` and `compiled-prompt.md`, and tmp holds the brief (the CLI has
always written it) but no `-compiled-prompt.md`. Close promotes the tmp brief
over the start-written one — `copy_validated_artifact_to` finishes with
`std::fs::rename` (`paths.rs:578`), which replaces an existing regular file —
promotes report/evidence/stdout, and `commit_promoted_dispatch_record` commits
the whole directory including the start-written compiled prompt. The record
ends up complete and in one commit. The only cost is a spurious `partial`
status (see R-2). The close is no longer blocked, which was the finding.

### F-2 MEDIUM — compiled prompt was stem-scoped → **FIXED**

`dispatch_compiled_prompt_path` (`paths.rs:299-312`) now derives the name from
the `last.txt` *filename* by suffix-replace (`-last.txt` → `-compiled-prompt.md`),
exactly the `dispatch_sibling_artifact_paths_from_last` pattern, so the bundle
inherits `last.txt`'s attempt id. One helper, one grammar: the daemon writer
(`api.rs:6260`) and the close reader (`paths.rs:365`) both call it — the
divergence risk is closed by construction, not by convention.

Proved: `prune_dispatch_stem_removes_only_selected_attempt_artifacts`
(`paths.rs:1120-1162`) writes two attempts' bundles and asserts attempt B's
survives attempt A's prune; `attempt_scoped_paths_isolate_consecutive_dispatch_bundles`
(`manager.rs:12405-12433`) asserts the two paths differ and neither content
leaks. Both pass.

### F-3 MEDIUM — no name grammar on the brief sidecar → **FIXED**

`dispatch_brief_name` (`paths.rs:316-332`) derives the stem from the brief
filename and rejects a mismatch. It mirrors `dispatch_artifact_stem`
(`manager.rs:10187-10212`) exactly — strip `-brief.md`, else `file_stem()` —
so it accepts every name the CLI itself can mint and nothing else. Probe D
(`BRIEF_PATH` pointed at a sibling attempt's `…-last.txt`) now yields
`"brief … does not match dispatch stem …"`, asserted by the new
`validate_dispatch_record_rejects_wrong_or_symlinked_brief_sidecar`
(`paths.rs:1557-1605`), which also asserts symlink rejection AND that the
symlink victim's content is untouched. Passes.

Handle discipline unregressed: `open_artifact_in_stem_dir` (`paths.rs:823-853`)
still opens `O_RDONLY | O_NOFOLLOW | O_CLOEXEC` via `openat` on the retained
stem-dir fd; the only change is the return type (`String` → `std::io::Result`),
which is what lets the caller distinguish `ENOENT` from unsafe. Both new
sidecars go through it, and both unlink through the same fd
(`paths.rs:535-543`).

### F-4 LOW — rollback orphaned the tmp compiled prompt → **FIXED**

`validate_dispatch_cleanup_targets` (`paths.rs:227-230`) now attaches the
compiled prompt, so the `started_tx: None` arm at `manager.rs:7783-7789`
prunes it through `prune_validated_dispatch_attempt`. The daemon's own
rollback shares that function, and the integration test that exercises it —
`dispatch_timeout_requests_daemon_cleanup` (`tests/dispatch.rs:4545-4560`) —
asserts no `-compiled-prompt.md` survives in the stem dir. Passes.
Sibling attempts are untouched (F-2's test above).

### F-5 LOW — failure edges untested → **FIXED**

- Missing-brief and missing-compiled-prompt closes: two new tests, above.
- Sidecar rejection branches: new test, above.
- One-commit property: `tests/dispatch.rs:4886-4901` now asserts
  `git log --oneline -- <record_dir>` yields exactly one line, on the real
  daemon-backed close. This is the assertion the round-1 finding asked for,
  and it is on the production path.
- F-2's two-starts-in-one-stem case is moot now that the path is attempt-scoped;
  the distinctness test replaces it.

---

## Residual findings (all LOW, all follow-ups)

### R-1 LOW (correctness) — the brief is still stem-scoped, so F-2's defect class survives for `brief.md`

`manager.rs:10388` — `dispatch_artifact_paths_for_attempt` returns
`dir.join(file_name)` for the brief with no attempt component, while `last.txt`
and `stdout.log` get `{stem}-{attempt_id}-…` on the next two lines.
`DispatchArtifactReservation::reserve` (`manager.rs:10243-10254`) collides only
on the last/stdout pair, so every attempt in a stem dir shares one brief file,
and `materialize_dispatch_brief` (`manager.rs:10419-10425`) overwrites it on
each dispatch.

Consequences, now that close consumes and unlinks the brief (it did not before
this branch — the old test asserted `"brief should be retained after close"`):
two attempts sharing a stem dir mean the first close promotes whichever brief
content was written last and unlinks it, and the second close records
`brief.md missing from tmp`.

**Why LOW, not MEDIUM:** `build_dispatch_plan` refuses a second same-kind
dispatch overlapping an open one (`manager.rs:5944`), and brief basenames are
task-scoped by convention, so two *live* attempts in one stem dir are not
reachable through the CLI today. The blast radius is also small — the compiled
prompt embeds the brief verbatim (`api.rs:21534` asserts the bundle contains
`Dispatch Brief`), so `compiled-prompt.md` still carries the content.

**Fix direction:** name the brief `{stem}-{attempt_id}-brief.md` and derive it
from `last.txt` the way `dispatch_compiled_prompt_path` now does; that also
lets `dispatch_brief_name` drop its `file_stem()` fallback.

### R-2 LOW (usability) — an upgrade-era close is permanently flagged as a cleanup failure with nothing to fix

A dispatch started by the pre-W97C8.1 daemon has no tmp compiled prompt, so its
close under this CLI records `CLEANUP_STATUS=partial` and
`CLEANUP_ERROR=compiled-prompt.md missing from tmp` — even though the record dir
already holds the start-written `compiled-prompt.md` and the committed record is
complete. `cleanup_status_reports_failure` (`manager.rs:10827`) treats anything
but `ok`/`cleanup_already_run` as a failure, so the entry sits in
`orgasmic manager dispatch-status --cleanup-failed` forever, pointing at a file
that is present. Self-limiting (only dispatches open across the upgrade), but an
operator will chase it.

**Fix direction:** before reporting a sidecar as missing, check whether
`dest_dir.join(name)` already exists; if it does, drop the error. Two lines, and
it also covers a re-run of a partially-promoted close. Alternatively note the
case in the convention (see R-4).

### R-3 LOW (robustness) — a sidecar copy failure now aborts the whole record promotion

`paths.rs:448-470`: a failed `copy_validated_artifact_to` for `brief.md` or
`compiled-prompt.md` returns `report_path: None`, which (a) skips the
`report.md`, `evidence.json` and `stdout.log` copies entirely and (b) makes
`promote_and_persist_dispatch_record` (`manager.rs:8453`) skip the commit — while
`create_dir_all` at `paths.rs:443` has already run. On main, `report.md` was
copied first, so this class had one trigger; it now has three, and it lands in
the "empty record dir" residue the code documents at `manager.rs:8705-8725`.

No data loss: tmp is not unlinked, so re-running the close recovers. Reachable
only on real I/O failure (ENOSPC, or `dest_dir/brief.md` existing as a
directory).

**Fix direction:** copy `report.md` first, then treat a sidecar *copy* failure
the same way a sidecar *absence* is treated — `errors.push(...)` and continue —
so one bad sidecar never costs the report.

### R-4 LOW (hygiene) — the branch adds 5 `cargo fmt --check` violations

`rustup run 1.97.1 cargo fmt --all -- --check` exits 1 with 13 hunks. Five are
inside this branch's own additions:

- `crates/orgasmic-core/src/paths.rs:1563`, `:1573`, `:1591` (the new rejection test)
- `crates/orgasmic-cli/src/manager.rs:13357` (`…_compiled_prompt_sidecar_is_missing`)
- `crates/orgasmic-cli/tests/dispatch.rs:4549` (the new rollback assertion)

The other eight are pre-existing (`src/main.rs:1863`, `manager.rs:5992`, `:7548`,
`core/src/lib.rs:37` — the `id` pub-use block, not the `paths` one this branch
edited — `core/src/projects.rs:212`, `daemon/src/api.rs:31241`,
`prompt_compiler.rs:1454`, `:1464`), so `cargo fmt --all --check` is already red
on main and this is not the branch's regression alone. `cargo fmt` fixes all
five; every hunk is pure line-joining.

Also noted, not a finding: `validate_dispatch_record_targets` calls
`validate_dispatch_compiled_prompt` twice when `worktree_path` is `Some` — once
inside `validate_dispatch_cleanup_targets` (`paths.rs:229`) and again at
`paths.rs:294`. The second open replaces the first handle, which is dropped, so
it is correct but is one wasted `openat` per close.

---

## Cross-checks from the brief

| Check | Result |
|---|---|
| Partial-failure retention (all tmp copies kept on any failed copy) | Holds. `promote_keeps_tmp_when_evidence_copy_fails` (`paths.rs:1340-1410`) now asserts the brief and compiled prompt survive too; `unlink_validated_attempt_artifacts` is still called only on the all-succeeded arm (`paths.rs:498`). Passes. |
| No daemon API shape change | Confirmed. `api.rs` diff is 12 lines inside `post_task_dispatch`; no request/response type touched. |
| Evidence.json promotion (TASK-W97C8) unaffected | Confirmed. `write_promoted_bytes_to` is unchanged; the only delta is that its failure message is now joined with any sidecar-gap messages (`paths.rs:481-487`). |
| `shipped_conventions` 5/5 | Passes. |
| Convention updated | `shipped/prompt-studio/conventions/manager-dispatch.org:250,314-320` — names the compiled prompt in the tmp record, states the tracked dir "does not exist before close", and lists what close promotes. It does NOT document the `partial` status a missing sidecar produces; worth a sentence given R-2. |

---

## Open Questions

1. Is `partial` the intended status for a missing sidecar, or should a gap that
   costs only evidence-of-evidence stay `ok` with a warning? R-2's noise comes
   entirely from that choice, and `cleanup_status_reports_failure` has no
   severity axis today.
2. Should brief basename uniqueness across concurrent dispatches be a guarantee
   (a check in `dispatch_artifact_stem`) rather than a habit? Round-1 open
   question 2, still open; R-1 is its remaining consequence.

---

## Verification Notes

All commands run in this worktree on the pinned toolchain (`rustup run 1.97.1`).
No repo file was modified; logs under `/tmp/w97c8.1-review/`.

| Check | Result |
|---|---|
| `cargo test -p orgasmic-core --lib paths::` | 15 passed, 0 failed |
| `cargo test -p orgasmic-cli --bin orgasmic dispatch_close` | 13 passed, 0 failed (includes both new missing-sidecar tests) |
| `cargo test -p orgasmic-cli --test dispatch -- dispatch_close_promotes_complete_record_only_at_close dispatch_timeout_requests_daemon_cleanup` | 2 passed, 0 failed |
| `cargo test -p orgasmic-cli --test shipped_conventions` | 5 passed, 0 failed (gate 5/5) |
| `cargo clippy -p orgasmic-cli -p orgasmic-core -p orgasmic-daemon --all-targets -- -D warnings` | clean, exit 0 |
| `cargo fmt --all -- --check` | exit 1, 13 hunks — 5 branch-introduced, 8 pre-existing (R-4) |

No probe crate was needed this round: round 1's probe cases A/B/C/D are now
covered by tests that run the real `cleanup_dispatch` and real
`validate_dispatch_record_targets`, which is strictly stronger evidence than
the round-1 probe. Case C was verified by source instead — the
`"brief_path required for dispatch promote"` error no longer exists in the
crate, and `brief_path: None` takes the same `None => None` arm at
`paths.rs:271` as a missing file.

Read for this review: the full `46b015a3..0884b10c` diff; `paths.rs:200-620`,
`:750-940`, `:1040-1610`; `manager.rs:900-960`, `2900-2955`, `1370-1395`,
`5925-5960`, `7599-7800`, `7930-8130`, `8436-8500`, `8650-8745`,
`10187-10300`, `10375-10425`, `10795-10875`, `12405-12435`, `13040-13370`;
`api.rs:6230-6275`; `tests/dispatch.rs` diff;
`shipped/prompt-studio/conventions/manager-dispatch.org` diff; round-1 review
at `.orgasmic/tmp/dispatch/task-w97c8.1/review-round-1.md`.

Failure classification: no test failed. The `cargo fmt` failure is a
pre-existing red gate (8 hunks on main, in files this branch never touches)
that the branch adds 5 hunks to — mixed pre-existing + regression, split above.

---

## Fix Directions (priority order, all post-merge)

1. **R-4** — run `cargo fmt` on the branch (5 hunks). Mechanical.
2. **R-2** — skip the `missing from tmp` error when `dest_dir/<name>` already
   exists, so upgrade-era closes and re-runs report `ok`.
3. **R-3** — move the `report.md` copy above the two sidecar copies, and make a
   sidecar copy failure `errors.push` + continue instead of returning
   `report_path: None`.
4. **R-1** — attempt-scope the brief filename via the same `-last.txt`
   suffix-replace the compiled prompt now uses; drop the `file_stem()` fallback
   in `dispatch_brief_name` once it lands.
