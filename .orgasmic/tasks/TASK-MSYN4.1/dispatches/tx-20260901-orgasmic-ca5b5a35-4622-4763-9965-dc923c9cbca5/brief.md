# TASK-MSYN4.1 — org-file writes must refuse the moved ledger, claims, and views structurally

Fix round for chain-review finding H1 (whole-chain review tx-1c6d2115, 2026-09-01).
The task heading above has the finding; this brief is only the delta.

## Read first (in this order)
1. `crates/orgasmic-daemon/src/api.rs:14575` `reject_ledger_rewrite` — the denylist. It
   string-prefix-matches `.orgasmic/tx` and matches `journal.org` by file name. That is all.
2. `api.rs:14551` `validate_org_edit_path` and `api.rs:14496` `post_org_file` — the handler
   takes `State` + `Json` only: no `Extension(identity)`, no Action check.
3. `api.rs:21212` `org_file_rewrite_refuses_ledger_paths` — the existing pin test. Extend it,
   do not write a parallel one.
4. `crates/orgasmic-daemon/src/writer.rs:1752` — the writer's claim gate allowlists
   `machines | tx | tmp | views`. That gate is the DAEMON's own write path and must keep
   working (the daemon appends tx, writes claims and views itself). Do NOT tighten the
   writer; tighten the API.
5. One sibling write handler that carries `Extension(identity): Extension<Identity>`
   (e.g. `api.rs:1259` / `:1279`) — mirror exactly how it authorizes, and
   `crates/orgasmic-daemon/src/authz.rs` for the `Action` enum and the role floor table.
6. `orgasmic task get --project orgasmic TASK-HQ970` — the incident that created the
   denylist (a rewritten tx file bricks the append-only ledger). Same class, reopened by the
   MSYN4 move of the authoritative ledger to `machines/<uuid>/tx/`.

## Target behaviour
- `reject_ledger_rewrite` becomes ONE structural predicate over path COMPONENTS (not string
  prefixes): refuse any path under `.orgasmic/machines/` (everything in it: `tx/`,
  `claims.org`, anything future), any path under `.orgasmic/views/`, any path under
  `.orgasmic/tx/`, and any `journal.org` anywhere under `.orgasmic/`. Each refusal names the
  surface it protects and the verb to use instead (keep the two existing messages' shape).
- `post_org_file` requires an identity and an `Action`, the same way its sibling structured
  write handlers do. Use the existing Action that governs node-body / org-node writes; add a
  new variant ONLY if no existing one has the right role floor, and then give it that same
  floor. Do not invent a new role.
- Pin with the existing test extended to at least these cases:
  `.orgasmic/machines/<uuid>/tx/2026-09.org` → refused; `.orgasmic/machines/<uuid>/claims.org`
  → refused; `.orgasmic/views/board.org` → refused; `.orgasmic/tx/2026-09.org` → refused;
  `.orgasmic/tasks/TASK-X/journal.org` → refused; `.orgasmic/gotchas.org` → still allowed.
  Plus one test that an unauthenticated / under-privileged `POST /org/file` is refused
  before any path logic runs, and one that an authorized principal still succeeds on an
  allowed path.

## Invariants
- The daemon's own writer keeps writing `machines/`, `tx/`, `views/`, `tmp/` — no change
  to `guard_node_write`'s allowlist semantics.
- `GET /org/file` is unchanged.
- No other org-file behaviour changes; every existing `org_file_*` test stays green.
- Do not touch `.orgasmic/` state anywhere (your worktree has none by design; verify graph
  state only via `orgasmic task get --project orgasmic TASK-MSYN4.1`). Never set
  `ORGASMIC_HOME`.

## Verification gates (run exactly these; targeted, never the workspace)
    cargo test -p orgasmic-daemon --lib org_file
    cargo test -p orgasmic-daemon --lib authz
    cargo clippy -p orgasmic-daemon --all-targets -- -D warnings
    cargo fmt --all --check
Redirect cargo output to a file and read it (never pipe it). If a pre-existing lint or
diagnostic sits in a file you touch, fix it too.

## Finish
Commit as:
    fix(daemon): org-file writes refuse machines/, views/, tx/ and journals structurally; post_org_file requires identity (TASK-MSYN4.1)
Report: what you changed (file:line), the exact test names run with pass counts from the log
file, which Action you chose and why, and anything you did NOT verify. Then, as your
terminal action:
    orgasmic dispatch finalize --summary-file <path> --commit
